//! Drive one non-interactive turn from input to durable outcome.

use super::*;

pub(super) struct HeadlessSession<'run, 'store> {
    pub(super) active: &'run mut ActiveSession<'store>,
    pub(super) opening: SessionState,
    pub(super) clock: &'run SystemClock,
    pub(super) sink: &'run dyn EventSink,
    pub(super) cancel: &'run CancelToken,
    pub(super) control: Option<ego_core::turn::InteractiveControl>,
    pub(super) pause: &'run PauseToken,
}

pub(super) struct HeadlessGeneration<'a> {
    pub(super) source: Arc<model::CliGenerationModelSource>,
    pub(super) admission: Arc<EvidenceAdmissionService>,
    pub(super) invocation: RunInvocationId,
    pub(super) config: TurnConfig,
    pub(super) provider_recovery: bool,
    pub(super) goal: Option<String>,
    pub(super) goal_turns: u32,
    pub(super) goal_check_command: Option<Vec<String>>,
    pub(super) goal_ceilings: ego_core::ceiling::Ceilings,
    pub(super) child_defaults: ego_core::turn::ChildDefaults,
    pub(super) gate: SharedBudgetGate,
    pub(super) hook_layers: Option<&'a [Sourced<ConfigLayer>]>,
    pub(super) hooks_off: bool,
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
    /// The host can activate a compaction successor and continue in it.
    /// Context percent at which it compacts; `None` keeps explicit recovery.
    pub(super) automatic_compaction: Option<ego_core::compact::ContextPercent>,
}

/// A published successor of the driven source session.
pub(super) struct HeadlessCompaction {
    pub(super) request: RequestId,
    pub(super) receipt: CompactionReceipt,
    /// Source turns driven before the compaction, for the run turn limit.
    pub(super) turns_taken: u32,
}

/// What one headless drive produced; a compaction names the session to continue.
pub(super) type HeadlessDriven = (
    RunTermination,
    SessionState,
    Value,
    Option<HeadlessCompaction>,
);

pub(super) async fn drive_headless_turn(
    session: HeadlessSession<'_, '_>,
    generation: HeadlessGeneration<'_>,
    child: HeadlessChild,
    effects: HeadlessEffects<'_>,
    turn: HeadlessTurn<'_>,
) -> Result<HeadlessDriven, RunTermination> {
    let HeadlessSession {
        active,
        opening,
        clock,
        sink,
        cancel,
        pause,
        mut control,
    } = session;
    let HeadlessGeneration {
        source,
        admission,
        invocation,
        config,
        provider_recovery,
        goal,
        goal_turns,
        goal_check_command,
        goal_ceilings,
        child_defaults,
        gate,
        hook_layers,
        hooks_off,
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
        automatic_compaction,
    } = turn;
    let opening_turns = opening.turns.len();
    let recovered_policy = match &input {
        HeadlessInput::Answered { policy, .. } => Some(Arc::clone(policy)),
        HeadlessInput::Prompt(_) | HeadlessInput::Resumed(_) | HeadlessInput::Checkpoint { .. } => {
            None
        }
    };
    let images = acquire_attachments(
        Attachments {
            inline: match &input {
                HeadlessInput::Prompt(prompt) => prompt.images(),
                HeadlessInput::Resumed(_)
                | HeadlessInput::Answered { .. }
                | HeadlessInput::Checkpoint { .. } => &[],
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
    let audio = super::port_adapters::acquire_audio(
        match &input {
            HeadlessInput::Prompt(prompt) => prompt.audio(),
            _ => &[],
        },
        &source,
        &ports,
        cancel,
        run_deadline,
        active.evidence().blob_store(),
    )
    .await?;
    let execution_capabilities = observation.capabilities();
    let http = source.provider_http(Arc::clone(&ports));
    let journal = SharedJournal::new(Journal::new(
        active.id(),
        active.store(),
        clock,
        sink,
        Producer::Main,
        opening,
    ));
    let instructions_loaded = config.system.is_some();
    // Fresh instructions and tools of this run seed a compaction successor.
    let target_system = config.system.clone();
    let runtime = GenerationRuntime {
        source: source.bind(http, cancel.clone(), run_deadline),
        policy_source: Arc::clone(&policy_source),
        gate: gate.clone(),
        config,
        admission: Arc::clone(&admission),
        invocation,
        retry_pause: Arc::new(TokioRetryPause),
    };
    let fs = &active.checkpoint_fs;
    let effect_ports = ToolEffectPorts::new(
        fs,
        ports.process(),
        active.evidence(),
        ports.resolver_handle(),
        ports.one_hop_handle(),
        ports.duplex_handle(),
    )
    .with_secret_values(ego_tools::secrets::from_env())
    .with_cancel(cancel.clone());
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
        hooks_off,
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
    let token_family = child_executor.token_family.unwrap_or(active.id());
    let mut family_executor = (*child_executor).clone();
    family_executor.token_family = Some(token_family);
    let child_executor = Arc::new(family_executor);
    let family_store = Arc::clone(&child_store);
    let session_store = Arc::clone(&child_store);
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
    let skill_owner = Arc::new(
        CliSkillInvocationOwner::new(
            child_service,
            hooks.runtime.clone(),
            cancel.clone(),
            hook_trust,
        )
        .with_token_family(family_store, token_family)
        .with_task_blobs(active.evidence().clone())
        .with_task_policy(
            policy_source
                .capture()
                .map_err(|message| RunTermination::Failed { message })?
                .snapshot()
                .digest(),
        ),
    );
    // Headless CLI and ACP prompts are explicit command input adapters. Ordinary
    // interactive/queued prompt data never passes through this selection.
    let actions = repl::actions::ActionRegistry::final_surface();
    if let Some(runtime) = mcp_runtime.as_ref() {
        actions.bind_mcp_view(Arc::clone(runtime));
    }
    let mcp_command = match &input {
        HeadlessInput::Prompt(prompt)
            if prompt.text().starts_with("/mcp__")
                || prompt.text().starts_with("/mcp_resource__") =>
        {
            match actions.classify(actions.submit(prompt.text(), repl::actions::InputOrigin::Typed))
            {
                repl::actions::Dispatch::McpContext(command) => Some(command),
                _ => {
                    return Err(RunTermination::UsageError {
                        message: "MCP context command is unavailable or its arguments are invalid"
                            .to_owned(),
                    });
                }
            }
        }
        _ => None,
    };
    let command = match &input {
        HeadlessInput::Prompt(prompt) => prompt.text().strip_prefix('/').and_then(|named| {
            let end = named.find(char::is_whitespace).unwrap_or(named.len());
            registry.iter().find_map(|tool| {
                let RegisteredTool::Skill(catalogue) = tool else {
                    return None;
                };
                catalogue.user_command(&named[..end], named[end..].trim_start())
            })
        }),
        HeadlessInput::Resumed(_)
        | HeadlessInput::Answered { .. }
        | HeadlessInput::Checkpoint { .. } => None,
    };
    if (command.is_some() || mcp_command.is_some()) && (!images.is_empty() || !audio.is_empty()) {
        return Err(RunTermination::UsageError {
            message: "custom commands do not accept image or audio attachments".to_owned(),
        });
    }
    let tools = match admitted_tools {
        Some(source) => {
            Tools::from_registry_with_source(registry, source, effect_ports, CanonPath::new(cwd))
                .map_err(|error| RunTermination::Failed {
                    message: format!("the admitted MCP tool catalogue is invalid: {error}"),
                })?
        }
        None => Tools::from_registry(registry, effect_ports, CanonPath::new(cwd)),
    };
    let tools = tools
        .with_named_agents(
            crate::named_agents::discover(cwd, hook_trust)
                .map_err(|error| RunTermination::UsageError { message: error.0 })?,
        )
        .with_child_defaults(child_defaults);
    let tools = match mcp_runtime.as_ref().and_then(|mcp| mcp.resume_sealer()) {
        Some(resume) => tools.with_resume(resume),
        None => tools,
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
    .with_goal(goal, goal_turns)
    .with_goal_policy(goal_ceilings, goal_check_command)
    .with_provider_recovery(provider_recovery.then_some(ego_core::turn::PROVIDER_RECOVERY_HORIZON))
    .with_skill_invocation_port(skill_owner.clone())
    .with_background_task_port(skill_owner.clone())
    .with_task_guidance()
    .with_pause_token(pause.clone());
    let loop_ = match automatic_compaction {
        Some(threshold) => loop_
            .with_automatic_compaction()
            .with_compaction_threshold(threshold),
        None => loop_,
    };
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
        Some(replayed) => loop_.with_replayed_context(replayed),
        None => loop_,
    };
    let loop_ = match elicitor {
        Some(elicitor) => loop_.with_elicitor(elicitor),
        None => loop_,
    };
    let loop_ = super::composition::with_permission_analysis(loop_, hook_trust);
    let loop_ = if judge_available {
        loop_.with_judge(JudgeContext {
            confinement: confinement.clone(),
            exec_caps: execution_capabilities,
        })
    } else {
        loop_
    }
    .with_execution_observation(observation, probe_evidence);
    let mut loop_ = match control.as_mut() {
        Some(control) => loop_.with_interactive_control(control),
        None => loop_,
    };
    let context_descriptors = mcp_runtime
        .as_ref()
        .and_then(|runtime| runtime.context_commands().ok())
        .map(|capture| {
            capture
                .tools()
                .iter()
                .filter_map(|tool| match tool {
                    RegisteredTool::Effect(tool) => Some(tool.descriptor()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    sink.context_commands(context_descriptors, &loop_.state());
    let from_checkpoint = match &input {
        HeadlessInput::Checkpoint { request } => {
            dispatch_post_compact(&hooks, *request, diagnostics).await;
            true
        }
        _ => false,
    };
    let (termination, result) = match input {
        HeadlessInput::Prompt(_) if mcp_command.is_some() => {
            turns
                .drive_mcp_context(
                    &mut loop_,
                    mcp_command.expect("selected context command"),
                    cancel,
                    diagnostics,
                )
                .await
        }
        HeadlessInput::Prompt(prompt) => match command {
            Some(operation) => {
                turns
                    .drive_admitted_skill(&mut loop_, operation, cancel, diagnostics)
                    .await
            }
            None => {
                turns
                    .drive_with_media(
                        &mut loop_,
                        prompt.text(),
                        images,
                        audio,
                        cancel,
                        diagnostics,
                    )
                    .await
            }
        },
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
        HeadlessInput::Checkpoint { .. } => {
            turns
                .drive_checkpoint(&mut loop_, cancel, diagnostics)
                .await
        }
    };
    // A successor that overflows before any measured reply would compact the
    // same checkpoint again; it keeps the typed overflow instead.
    let compaction = if automatic_compaction.is_some()
        && matches!(termination, RunTermination::ContextOverflow { .. })
        && (!from_checkpoint || loop_.state().context.used_tokens.is_some())
    {
        let turns_taken = u32::try_from(loop_.state().turns.len().saturating_sub(opening_turns))
            .unwrap_or(u32::MAX);
        let target_tools = registry.iter().map(RegisteredTool::descriptor).collect();
        compact_source(
            &mut loop_,
            &hooks,
            (target_system, target_tools),
            (active.id(), session_store.as_ref(), clock),
            diagnostics,
        )
        .await
        .map(|(request, receipt)| HeadlessCompaction {
            request,
            receipt,
            turns_taken,
        })
    } else {
        None
    };
    drop(loop_);
    skill_owner
        .drain_tasks()
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
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
    Ok((termination, state, result, compaction))
}

/// Run the interactive clean-session compaction on the driven source.
///
/// A failure is reported and returns `None`: the caller keeps the source's
/// typed overflow, so the run still exits non-zero.
async fn compact_source<'session: 'turn, 'turn>(
    loop_: &mut TurnLoop<'session, 'turn>,
    hooks: &SessionHookComposition<'_>,
    target: (Option<SystemPrompt>, Vec<ToolDescriptor>),
    source: (SessionId, &JsonlStore, &SystemClock),
    diagnostics: &dyn Diagnostics,
) -> Option<(RequestId, CompactionReceipt)> {
    let (target_system, target_tools) = target;
    let (source, store, clock) = source;
    let failed = |message: String| {
        diagnostics.note(&format!(
            "compaction failed: {message}. The session is unchanged; retry with 'ego compact {source}'."
        ));
        None
    };
    diagnostics.note(
        "Compacting the session now: saving a summary. The task will continue automatically.",
    );
    let request = RequestId::from_uuid(uuid::Uuid::now_v7());
    if let Err(error) = dispatch_request_hook(
        &hooks.runtime,
        request,
        hooks.turn,
        None,
        HookLifecycle::PreCompact,
    )
    .await
    {
        return failed(format!(
            "blocked before preparation: {}",
            error.exit_reason()
        ));
    }
    let future: CompactionFuture<'_, Result<PreparedCompaction, CompactionError>> =
        Box::pin(loop_.prepare_compaction(target_system, target_tools));
    let mut preparation = DeferredCompactionPreparation {
        source,
        future: Some(future),
    };
    let receipt = CompactionTransitionService::new(*clock)
        .compact(
            CompactionTransitionRequest {
                request_id: request,
                source,
            },
            &mut preparation,
            store,
        )
        .await;
    drop(preparation);
    match receipt {
        Ok(receipt) => Some((request, receipt)),
        Err(error) => failed(error.to_string()),
    }
}

/// The successor's first hook boundary, as in an interactive compaction epoch.
async fn dispatch_post_compact(
    hooks: &SessionHookComposition<'_>,
    request: RequestId,
    diagnostics: &dyn Diagnostics,
) {
    // Compaction starts a clean event stream: the source PreCompact closure is
    // not a parent in this session, and the request identifies the lifecycle.
    if let Err(error) = dispatch_request_hook(
        &hooks.runtime,
        request,
        hooks.turn,
        None,
        HookLifecycle::PostCompact,
    )
    .await
    {
        diagnostics.note(&format!(
            "the compaction post hook failed: {}",
            error.exit_reason()
        ));
    }
}
