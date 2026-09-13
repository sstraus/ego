//! Drive one non-interactive turn from input to durable outcome.

use super::*;

pub(super) struct HeadlessSession<'run, 'store> {
    pub(super) active: &'run mut ActiveSession<'store>,
    pub(super) opening: SessionState,
    pub(super) clock: &'run SystemClock,
    pub(super) sink: &'run dyn EventSink,
    pub(super) cancel: &'run CancelToken,
    pub(super) pause: &'run PauseToken,
}

pub(super) struct HeadlessGeneration<'a> {
    pub(super) source: Arc<model::CliGenerationModelSource>,
    pub(super) admission: Arc<EvidenceAdmissionService>,
    pub(super) invocation: RunInvocationId,
    pub(super) config: TurnConfig,
    pub(super) gate: SharedBudgetGate,
    pub(super) hook_layers: Option<&'a [Sourced<ConfigLayer>]>,
    pub(super) hook_trust: Trust,
    pub(super) policy_source: Arc<dyn GenerationPolicySource>,
    pub(super) judge_available: bool,
}

pub(super) struct HeadlessChild {
    pub(super) parent_run: RunId,
    pub(super) root: RunRoot,
    pub(super) store: Arc<JsonlStore>,
    pub(super) executor: Arc<CliRunExecutor>,
    pub(super) inherited_effect_tools: Option<Arc<BTreeSet<String>>>,
}

pub(super) struct HeadlessEffects<'a> {
    pub(super) mcp_runtime: Option<Arc<dyn McpRuntimeBinding>>,
    pub(super) registry: &'a [RegisteredTool],
    pub(super) admitted_tools: Option<Arc<dyn AdmittedToolSource>>,
    pub(super) cwd: &'a Path,
    pub(super) confinement: &'a RunConfinement,
    pub(super) run_deadline: Option<Instant>,
    pub(super) ports: Arc<RunEffectPorts>,
    pub(super) observation: ExecObservation,
    pub(super) probe_evidence: Option<ego_core::store::evidence::EvidenceRef>,
    pub(super) elicitor: Option<Arc<dyn Elicitor>>,
}

pub(super) struct HeadlessTurn<'a> {
    pub(super) turns: &'a Turns<'a>,
    pub(super) attachment_specs: &'a [String],
    pub(super) input: HeadlessInput<'a>,
    pub(super) diagnostics: &'a dyn Diagnostics,
}

pub(super) async fn drive_headless_turn(
    session: HeadlessSession<'_, '_>,
    generation: HeadlessGeneration<'_>,
    child: HeadlessChild,
    effects: HeadlessEffects<'_>,
    turn: HeadlessTurn<'_>,
) -> Result<(RunTermination, SessionState, Value), RunTermination> {
    let HeadlessSession {
        active,
        opening,
        clock,
        sink,
        cancel,
        pause,
    } = session;
    let HeadlessGeneration {
        source,
        admission,
        invocation,
        config,
        gate,
        hook_layers,
        hook_trust,
        policy_source,
        judge_available,
    } = generation;
    let HeadlessChild {
        parent_run,
        root: child_root,
        store: child_store,
        executor: child_executor,
        inherited_effect_tools,
    } = child;
    let HeadlessEffects {
        mcp_runtime,
        registry,
        admitted_tools,
        cwd,
        confinement,
        run_deadline,
        ports,
        observation,
        probe_evidence,
        elicitor,
    } = effects;
    let HeadlessTurn {
        turns,
        attachment_specs,
        input,
        diagnostics,
    } = turn;
    let recovered_policy = match &input {
        HeadlessInput::Answered { policy, .. } => Some(Arc::clone(policy)),
        HeadlessInput::Prompt(_) | HeadlessInput::Resumed(_) => None,
    };
    let images = acquire_attachments(
        Attachments {
            inline: match &input {
                HeadlessInput::Prompt(prompt) => prompt.images(),
                HeadlessInput::Resumed(_) | HeadlessInput::Answered { .. } => &[],
            },
            specs: attachment_specs,
        },
        &source,
        &ports,
        cancel,
        run_deadline,
        cwd,
        active.evidence().blob_store(),
    )
    .await?;
    let execution_capabilities = observation.capabilities();
    let http =
        Arc::new(ProviderHttpAdapter::new(Arc::clone(&ports))) as Arc<dyn ego_ai::http::Http>;
    let journal = SharedJournal::new(Journal::new(
        active.id(),
        active.store(),
        clock,
        sink,
        Producer::Main,
        opening,
    ));
    let instructions_loaded = config.system.is_some();
    let runtime = GenerationRuntime {
        source: source.bind(http, cancel.clone(), run_deadline),
        policy_source: Arc::clone(&policy_source),
        gate: gate.clone(),
        config,
        admission: Arc::clone(&admission),
        invocation,
    };
    let fs = OsFs;
    let effect_ports = ToolEffectPorts::new(
        &fs,
        ports.process(),
        active.evidence(),
        ports.resolver_handle(),
        ports.one_hop_handle(),
        ports.duplex_handle(),
    );
    let hook_policy = match recovered_policy.as_ref() {
        Some(policy) => Arc::clone(policy),
        None => policy_source
            .current()
            .map_err(|message| RunTermination::Failed { message })?,
    };
    let hooks = compose_session_hooks(SessionHookInput {
        session: active.id(),
        journal: journal.clone(),
        run_evidence: active.run_evidence(),
        response_evidence: active.evidence(),
        admission,
        invocation,
        gate: gate.clone(),
        layers: hook_layers,
        trust: hook_trust,
        source: Arc::clone(&source),
        ports: Arc::clone(&ports),
        policy: hook_policy,
        effects: effect_ports.clone(),
        mcp_runtime: mcp_runtime.as_ref().map(Arc::clone),
        cancel: cancel.clone(),
        deadline: run_deadline,
        instructions_loaded,
    })
    .await?;
    let mut child_bindings = RunExecutionBindings::new(
        Arc::new(CliChildSinkFactory),
        elicitor.as_ref().map(Arc::clone),
    )
    .with_budget_gate(gate.clone());
    if let (Some(admitted), Some(mcp)) = (admitted_tools.as_ref(), mcp_runtime.as_ref()) {
        child_bindings = child_bindings.with_mcp(Arc::clone(admitted), Vec::new(), Arc::clone(mcp));
    }
    let child_service = compose_child_service(ChildComposition {
        parent_run,
        parent_session: active.id(),
        root: child_root,
        store: child_store,
        executor: child_executor,
        gate: gate.clone(),
        bindings: child_bindings,
        blobs: active.evidence().clone(),
    });
    let skill_owner = Arc::new(CliSkillInvocationOwner::new(
        child_service,
        hooks.runtime.clone(),
        cancel.clone(),
    ));
    let tools = match admitted_tools {
        Some(source) => {
            Tools::from_registry_with_source(registry, source, effect_ports, CanonPath::new(cwd))
                .map_err(|error| RunTermination::Failed {
                    message: format!("the admitted MCP tool catalogue is invalid: {error}"),
                })?
        }
        None => Tools::from_registry(registry, effect_ports, CanonPath::new(cwd)),
    };
    let loop_ = TurnLoop::from_shared_journal(
        active.id(),
        journal.clone(),
        active.run_evidence(),
        cancel.clone(),
        runtime,
        tools,
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?
    .with_hook_binding(
        hooks.state.clone(),
        Arc::clone(&hooks.binding),
        hooks.mcp.as_ref().map(Arc::clone),
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?
    .with_skill_invocation_port(skill_owner.clone())
    .with_pause_token(pause.clone());
    let loop_ = match recovered_policy.as_ref() {
        Some(policy) => loop_
            .with_hook_policy(Arc::clone(policy))
            .map_err(|message| RunTermination::Failed { message })?,
        None => loop_,
    };
    let loop_ = match inherited_effect_tools {
        Some(tools) => loop_.with_effect_tool_ceiling(tools),
        None => loop_,
    };
    let loop_ = match active.replayed() {
        Some(replayed) => loop_.with_ancestor_conversation(replayed.conversation().clone()),
        None => loop_,
    };
    let loop_ = match elicitor {
        Some(elicitor) => loop_.with_elicitor(elicitor),
        None => loop_,
    };
    let loop_ = super::composition::with_permission_analysis(loop_, hook_trust);
    let mut loop_ = if judge_available {
        loop_.with_judge(JudgeContext {
            confinement: confinement.clone(),
            exec_caps: execution_capabilities,
        })
    } else {
        loop_
    }
    .with_execution_observation(observation, probe_evidence);
    let (termination, result) = match input {
        HeadlessInput::Prompt(prompt) => {
            turns
                .drive_with_images(&mut loop_, prompt.text(), images, cancel, diagnostics)
                .await
        }
        HeadlessInput::Resumed(turn) => {
            turns
                .drive_resumed(&mut loop_, turn, cancel, diagnostics)
                .await
        }
        HeadlessInput::Answered {
            question, answer, ..
        } => {
            turns
                .drive_answered(&mut loop_, question, answer, cancel, diagnostics)
                .await
        }
    };
    drop(loop_);
    skill_owner
        .drain()
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    drop(skill_owner);
    let session_end_runtime = match recovered_policy {
        Some(policy) => hooks.bind_policy(policy),
        None => hooks.bind_current(policy_source.as_ref()),
    };
    let session_end = match session_end_runtime {
        Ok(runtime) => {
            dispatch_session_hook(&runtime, active.id(), hooks.turn, HookLifecycle::SessionEnd)
                .await
        }
        Err(error) => Err(error),
    };
    drop(hooks);
    let state = journal.try_into_state().map_err(hook_journal_termination)?;
    session_end?;
    Ok((termination, state, result))
}
