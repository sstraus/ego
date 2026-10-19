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
                let spec = catalogue.authorize_user(&named[..end]).ok()?;
                spec.command_template()?;
                Some(AdmittedSkillOperation::invoke(
                    spec.descriptor(),
                    named[end..].trim_start(),
                ))
            })
        }),
        HeadlessInput::Resumed(_) | HeadlessInput::Answered { .. } => None,
    };
    if (command.is_some() || mcp_command.is_some()) && !images.is_empty() {
        return Err(RunTermination::UsageError {
            message: "custom commands do not accept image attachments".to_owned(),
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
    let tools = tools.with_named_agents(
        crate::named_agents::discover(cwd, hook_trust)
            .map_err(|error| RunTermination::UsageError { message: error.0 })?,
    );
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
    .with_delegation_host(std::env::var_os("TUIC_SESSION").is_some())
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
    Ok((termination, state, result))
}
