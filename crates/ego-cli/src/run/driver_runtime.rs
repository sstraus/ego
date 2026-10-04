//! Drive an admitted session through its selected headless or interactive posture.

use super::*;

enum StagedPasteSource {
    Path(String),
    Inline(Image),
}

struct StagedPasteImage {
    label: String,
    source: StagedPasteSource,
}

async fn paste_image_limits(
    source: &Arc<model::CliGenerationModelSource>,
    confinement: &RunConfinement,
    evidence: &SessionEvidenceStore,
    run_deadline: Option<std::time::Instant>,
) -> Result<ego_core::generator::capabilities::ImageLimits, String> {
    let paste_cancel = CancelToken::new();
    let (ports, _, _) = effect_scope(confinement, evidence, &paste_cancel, run_deadline)
        .await
        .map_err(|error| format!("image paste could not check the model: {error:?}"))?;
    let http = Arc::new(ProviderHttpAdapter::new(ports)) as Arc<dyn ego_ai::http::Http>;
    let profile = source
        .bind(http, paste_cancel, run_deadline)
        .resolve(GenerationModelRequest::new(GenerationRole::Main))
        .await
        .map_err(|error| format!("image paste could not resolve the model: {error}"))?;
    attachment::supported_limits(
        &profile.capabilities,
        profile.identity.0.as_str(),
        &profile.request_model,
    )
    .cloned()
    .map_err(|error| error.to_string())
}

fn paste_chips(images: &[StagedPasteImage]) -> Vec<String> {
    images.iter().map(|image| image.label.clone()).collect()
}

pub(super) struct DriverExecution<'host, 'store, S: EventSink> {
    pub(super) args: Args,
    pub(super) cancel: CancelToken,
    pub(super) host: RunHost<'host>,
    pub(super) scoped: &'store ScopedInvocation,
    pub(super) materialized: MaterializedRun<'store>,
    pub(super) generation: GenerationReady,
    pub(super) activated: ActivatedRun<'store, S>,
    pub(super) policy_application: Option<Arc<PolicyApplicationOwner>>,
}

pub(super) async fn drive_execution<S: EventSink>(
    runtime: DriverExecution<'_, '_, S>,
) -> Result<Ended, RunTermination> {
    let DriverExecution {
        args,
        cancel,
        host,
        scoped,
        materialized,
        generation,
        activated,
        policy_application,
    } = runtime;
    let RunHost {
        driver,
        elicitor,
        status,
        diagnostics,
        text_output,
        evidence,
    } = host;
    let ScopedInvocation {
        mcp_scope: _mcp_scope,
        mcp_runtime,
        mcp_registry: _,
        artifact_store,
        jsonl_store,
        session_id: _,
        clock,
    } = scoped;
    let MaterializedRun {
        opened: _,
        system,
        mut registry,
        skills,
        compiled,
        text,
        slug: _,
        generation: _,
        skill_trust,
        skill_profile_layers,
        run_deadline,
        root,
        cwd,
        confinement,
        effective,
        skill_resource_roots,
        currency,
        child_root,
        child_executor,
        context: _,
        mode: _,
        policy_selection,
        policy_source,
        invocation,
        run_id,
        resume_plan: _,
        recovered_capture: _,
    } = materialized;
    let GenerationReady {
        source,
        selection,
        main_capabilities: _,
        judge_available,
    } = generation;
    let ActivatedRun {
        mut active,
        opening,
        sink,
        admitted_tools,
        mcp_errors,
        resume,
    } = activated;
    let mcp_runtime = Arc::clone(mcp_runtime);
    let clock = *clock;
    let sink = &sink;

    let fs = OsFs;
    let gate = SharedBudgetGate::new(budget::gate(
        &effective.ceilings,
        effective.clamped.money,
        currency,
    ));
    let interactive_metadata_ceilings = effective.ceilings.clone();

    let mut config = TurnConfig {
        system,
        identity: Some(AgentIdentity {
            version: crate::build_info::VERSION.to_owned(),
            sandbox: confinement.level(),
            offline: confinement.offline(),
            host: None,
        }),
        token_ceiling: effective.ceilings.tokens,
        retry: args.retry,
        fallbacks: args.fallbacks.clone(),
    };

    let turns = Turns {
        schema: compiled.as_ref(),
        retries: args.schema_retries,
        max_turns: args.max_turns,
        deadline: run_deadline,
    };

    // One turn-local coordinator over the session's single shared journal.
    //
    // Rebuilt per turn rather than held across the session, and that is
    // `CancelToken`'s doing rather than a preference: the token is latching
    // (`ego-core/src/turn/mod.rs:89`, "the first cause wins") and lives inside
    // `Session`, so a `Ctrl+C` on turn one would leave every later turn to
    // abort before its first check. A fresh loop takes a fresh token. It costs
    // nothing — every field below is a borrow or a clone of something already
    // assembled; generation authority itself is deliberately resolved
    // again inside each attempt.
    // A macro and not a closure: the loop borrows from this function's locals,
    // and a closure cannot name the lifetimes of a `TurnLoop<'_, '_>` it returns.
    macro_rules! build {
        ($active:expr, $cancel:expr, $ports:expr, $observation:expr, $probe_evidence:expr, $hooks:expr, $journal:expr, $skill_owner:expr) => {{
            let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(&$ports)))
                as Arc<dyn ego_ai::http::Http>;
            let runtime = GenerationRuntime {
                source: source.bind(http, $cancel.clone(), run_deadline),
                policy_source: Arc::clone(&policy_source),
                gate: gate.clone(),
                config: config.clone(),
                admission: Arc::clone(&evidence),
                invocation,
            };
            let effect_ports = ToolEffectPorts::new(
                &fs,
                $ports.process(),
                $active.evidence(),
                $ports.resolver_handle(),
                $ports.one_hop_handle(),
                $ports.duplex_handle(),
            )
            .with_cancel($cancel.clone());
            let tools = match admitted_tools.as_ref() {
                Some(source) => Tools::from_registry_with_source(
                    &registry,
                    Arc::clone(source),
                    effect_ports,
                    CanonPath::new(&cwd),
                )
                .map_err(|error| RunTermination::Failed {
                    message: format!("the admitted MCP tool catalogue is invalid: {error}"),
                })?,
                None => Tools::from_registry(&registry, effect_ports, CanonPath::new(&cwd)),
            };
            let tools = match McpRuntimeBinding::resume_sealer(mcp_runtime.as_ref()) {
                Some(resume) => tools.with_resume(resume),
                None => tools,
            };
            let tools = match skill_resource_roots.as_ref() {
                Some(roots) => tools.with_skill_resource_roots(roots.clone()),
                None => tools,
            };
            let loop_ = TurnLoop::from_shared_journal(
                $active.id(),
                ($journal).clone(),
                $active.run_evidence(),
                $cancel.clone(),
                runtime,
                tools,
            )
            .map_err(|error| RunTermination::Failed {
                message: error.to_string(),
            })?
            .with_hook_binding(
                ($hooks).state.clone(),
                Arc::clone(&($hooks).binding),
                ($hooks).mcp.as_ref().map(Arc::clone),
            )
            .map_err(|error| RunTermination::Failed {
                message: error.to_string(),
            })?;
            let skill_owner: Arc<dyn SkillInvocationPort + '_> = ($skill_owner).clone();
            let loop_ = loop_.with_skill_invocation_port(skill_owner);
            let loop_ = match $active.replayed() {
                Some(replayed) => loop_.with_ancestor_conversation(replayed.conversation().clone()),
                None => loop_,
            };
            let loop_ = match elicitor.as_ref() {
                Some(elicitor) => loop_.with_elicitor(Arc::clone(elicitor)),
                None => loop_,
            };
            let loop_ = if judge_available {
                loop_.with_judge(JudgeContext {
                    confinement: confinement.clone(),
                    exec_caps: $observation.capabilities(),
                })
            } else {
                loop_
            };
            super::composition::with_permission_analysis(loop_, skill_trust)
                .with_execution_observation($observation, $probe_evidence)
        }};
    }

    macro_rules! open_session_hooks {
        ($active:expr, $state:expr, $hook_ports:ident, $policy:expr) => {{
            let hook_effects = ToolEffectPorts::new(
                &fs,
                $hook_ports.process(),
                $active.evidence(),
                $hook_ports.resolver_handle(),
                $hook_ports.one_hop_handle(),
                $hook_ports.duplex_handle(),
            );
            let leased_journal =
                LeasedSharedJournal::new($active.writer(), &clock, sink, Producer::Main, $state);
            let journal = leased_journal.shared();
            let hooks = compose_session_hooks(SessionHookInput {
                session: $active.id(),
                journal: journal.clone(),
                run_evidence: $active.run_evidence(),
                response_evidence: $active.evidence(),
                admission: Arc::clone(&evidence),
                invocation,
                gate: gate.clone(),
                layers: skill_profile_layers.as_deref(),
                trust: skill_trust,
                source: Arc::clone(&source),
                ports: Arc::clone(&$hook_ports),
                policy: $policy,
                effects: hook_effects,
                mcp_runtime: Some(Arc::clone(&mcp_runtime) as Arc<dyn McpRuntimeBinding>),
                cancel: cancel.clone(),
                deadline: run_deadline,
                instructions_loaded: config.system.is_some(),
            })
            .await?;
            (leased_journal, journal, hooks)
        }};
    }

    macro_rules! install_model_switch_owner {
        ($hooks:expr, $owner:ident, $status:expr) => {{
            $owner = ($status).as_ref().and_then(|status| {
                status.model_config_store.as_ref().map(|config_store| {
                    RunModelSwitchOwner::new(
                        Arc::clone(&selection),
                        Arc::clone(config_store),
                        ($hooks).runtime.clone(),
                        ($hooks).turn,
                        None,
                    )
                })
            });
        }};
    }

    match driver {
        Driver::Once => {
            let input = match resume {
                Some(RuntimeResume::Model(turn)) => HeadlessInput::Resumed(turn),
                Some(RuntimeResume::Foreign {
                    question,
                    answer,
                    policy,
                }) => HeadlessInput::Answered {
                    question,
                    answer,
                    policy,
                },
                None => HeadlessInput::Prompt(&RunPrompt::from(text.clone())),
            };
            let (ports, observation, probe_evidence) =
                effect_scope(&confinement, active.evidence(), &cancel, run_deadline).await?;
            let pause = PauseToken::default();
            let driven = drive_headless_turn(
                HeadlessSession {
                    active: &mut active,
                    opening,
                    clock: &clock,
                    sink,
                    cancel: &cancel,
                    pause: &pause,
                },
                HeadlessGeneration {
                    source: Arc::clone(&source),
                    admission: Arc::clone(&evidence),
                    invocation,
                    config: config.clone(),
                    gate: gate.clone(),
                    hook_layers: skill_profile_layers.as_deref(),
                    hook_trust: skill_trust,
                    policy_source: Arc::clone(&policy_source),
                    judge_available,
                },
                HeadlessChild {
                    parent_run: run_id,
                    root: child_root.clone(),
                    store: Arc::clone(jsonl_store),
                    executor: Arc::clone(&child_executor),
                    inherited_effect_tools: None,
                },
                HeadlessEffects {
                    mcp_runtime: Some(mcp_runtime.clone()),
                    registry: &registry,
                    admitted_tools: admitted_tools.clone(),
                    cwd: &cwd,
                    confinement: &confinement,
                    run_deadline,
                    ports,
                    observation,
                    probe_evidence,
                    elicitor,
                },
                HeadlessTurn {
                    turns: &turns,
                    attachment_specs: &args.attach,
                    input,
                    diagnostics,
                },
            )
            .await;
            let (termination, state, result) = match driven {
                Ok(driven) => driven,
                Err(termination) => (
                    termination,
                    active.state().clone(),
                    Value::String(String::new()),
                ),
            };
            active.replace_state(state.clone());
            let mut ended = Ended {
                termination,
                session: active.id(),
                state,
                result,
                mcp_errors: mcp_errors.clone(),
            };
            if let Some(artifact_store) = artifact_store.as_ref() {
                finalize_ci_bundle(
                    jsonl_store.as_ref(),
                    artifact_store,
                    run_id,
                    &mut ended.termination,
                    diagnostics,
                );
            }
            Ok(ended)
        }
        Driver::Interactive { seat, between } => {
            let interactive_metadata =
                SessionMetadata::new(Arc::clone(jsonl_store), Arc::new(clock));
            type CompactionRuntime = (
                Option<SystemPrompt>,
                Vec<RegisteredTool>,
                Option<crate::skill::PreparedSkillRescan>,
            );

            enum EpochStart {
                Initial,
                New,
                Fork {
                    diagnostic: Option<String>,
                    projected: Option<Box<PublicEvent>>,
                },
                Compact {
                    diagnostic: Option<String>,
                    request: RequestId,
                    parent: Option<EventId>,
                },
            }

            enum EpochTransition<'store> {
                Exit,
                New {
                    target: ActiveSession<'store>,
                },
                Fork {
                    target: ActiveSession<'store>,
                    publication: ego_core::session::TargetPublication,
                    diagnostic: Option<String>,
                    projected: Option<PublicEvent>,
                },
                Compact {
                    target: ActiveSession<'store>,
                    publication: ego_core::session::TargetPublication,
                    diagnostic: Option<String>,
                    runtime: Option<CompactionRuntime>,
                    request: RequestId,
                    parent: Option<EventId>,
                },
            }

            let mut control = seat.take_control();
            let mut state = opening;
            let mut last = Ended {
                termination: RunTermination::Done,
                session: active.id(),
                state: state.clone(),
                result: Value::String(String::new()),
                mcp_errors: mcp_errors.clone(),
            };

            // `ego "explain this failing test"` seeds the first turn; `ego`
            // alone starts at the prompt. Both are one session — the argument
            // is a first thing to say, not a different mode.
            let mut seeded = (!text.trim().is_empty()).then(|| text.clone());
            let mut resumed = resume;
            let mut pending_attachments = args.attach.clone();
            let mut staged_images = Vec::<StagedPasteImage>::new();
            let mut epoch_start = EpochStart::Initial;

            // Borrow-scoped session authorities live in one lexical epoch.
            // Only owned transition data crosses the boundary where the
            // replaceable ActiveSession is mutated or replaced.
            'sessions: loop {
                let (transition, source_state, session_end) = {
                    let (session_hook_ports, _, _) =
                        effect_scope(&confinement, active.evidence(), &cancel, run_deadline)
                            .await?;
                    let recovered_epoch_policy = match resumed.as_ref() {
                        Some(RuntimeResume::Foreign { policy, .. }) => Some(Arc::clone(policy)),
                        Some(RuntimeResume::Model(_)) | None => None,
                    };
                    let session_policy = match recovered_epoch_policy.as_ref() {
                        Some(policy) => Arc::clone(policy),
                        None => policy_source
                            .current()
                            .map_err(|message| RunTermination::Failed { message })?,
                    };
                    let (leased_session_journal, session_journal, session_hooks) = open_session_hooks!(
                        active,
                        active.state().clone(),
                        session_hook_ports,
                        session_policy
                    );
                    let mut child_bindings = RunExecutionBindings::new(
                        Arc::new(CliChildSinkFactory),
                        elicitor.as_ref().map(Arc::clone),
                    )
                    .with_budget_gate(gate.clone());
                    if let Some(admitted) = admitted_tools.as_ref() {
                        child_bindings = child_bindings.with_mcp(
                            Arc::clone(admitted),
                            mcp_errors.clone(),
                            Arc::clone(&mcp_runtime) as Arc<dyn McpRuntimeBinding>,
                        );
                    }
                    let child_service = compose_child_service(ChildComposition {
                        parent_run: RunId::new(active.id(), invocation.uuid()),
                        parent_session: active.id(),
                        root: child_root.clone(),
                        store: Arc::clone(jsonl_store),
                        executor: Arc::clone(&child_executor),
                        gate: gate.clone(),
                        bindings: child_bindings,
                        blobs: active.evidence().clone(),
                    });
                    let skill_owner = Arc::new(CliSkillInvocationOwner::new(
                        child_service,
                        session_hooks.runtime.clone(),
                        cancel.clone(),
                        skill_trust,
                    ));
                    let model_switch_owner;
                    install_model_switch_owner!(session_hooks, model_switch_owner, status);
                    if let EpochStart::Compact {
                        request, parent, ..
                    } = &epoch_start
                        && let Err(error) = dispatch_request_hook(
                            &session_hooks.runtime,
                            *request,
                            session_hooks.turn,
                            *parent,
                            HookLifecycle::PostCompact,
                        )
                        .await
                    {
                        diagnostics.note(&format!(
                            "the compaction post hook failed: {}",
                            error.exit_reason()
                        ));
                    }
                    state = session_hooks.state.state();
                    last.session = active.id();
                    last.state = state.clone();
                    last.result = Value::String(String::new());
                    let activation_failed = match &epoch_start {
                        EpochStart::Initial => false,
                        EpochStart::New => {
                            sink.publish(
                                &PublicEvent::SessionStarted {
                                    v: PUBLIC_EVENT_VERSION,
                                    session: active.id(),
                                    capabilities: capabilities(
                                        &registry,
                                        config.system.is_some(),
                                        compiled.is_some(),
                                    ),
                                    mcp_errors: mcp_errors.clone(),
                                    hook_errors: Vec::new(),
                                    instructions: config
                                        .system
                                        .as_ref()
                                        .map(|prompt| prompt.sources.clone())
                                        .unwrap_or_default(),
                                },
                                &state,
                            );
                            false
                        }
                        EpochStart::Fork {
                            diagnostic,
                            projected,
                        } => {
                            if let Some(diagnostic) = diagnostic {
                                diagnostics.note(&format!(
                                    "fork target {} is authoritative with a durability warning: {diagnostic}",
                                    active.id()
                                ));
                            }
                            sink.publish(
                                &PublicEvent::SessionStarted {
                                    v: PUBLIC_EVENT_VERSION,
                                    session: active.id(),
                                    capabilities: capabilities(
                                        &registry,
                                        config.system.is_some(),
                                        compiled.is_some(),
                                    ),
                                    mcp_errors: mcp_errors.clone(),
                                    hook_errors: Vec::new(),
                                    instructions: config
                                        .system
                                        .as_ref()
                                        .map(|prompt| prompt.sources.clone())
                                        .unwrap_or_default(),
                                },
                                &state,
                            );
                            if let Some(event) = projected {
                                sink.publish(event.as_ref(), &state);
                            }
                            !seat.activate_session(active.id())
                        }
                        EpochStart::Compact { diagnostic, .. } => {
                            if let Some(diagnostic) = diagnostic {
                                diagnostics.note(&format!(
                                    "compaction target is authoritative with a durability warning: {diagnostic}"
                                ));
                            }
                            sink.publish(
                                &PublicEvent::SessionStarted {
                                    v: PUBLIC_EVENT_VERSION,
                                    session: active.id(),
                                    capabilities: capabilities(
                                        &registry,
                                        config.system.is_some(),
                                        compiled.is_some(),
                                    ),
                                    mcp_errors: mcp_errors.clone(),
                                    hook_errors: Vec::new(),
                                    instructions: config
                                        .system
                                        .as_ref()
                                        .map(|prompt| prompt.sources.clone())
                                        .unwrap_or_default(),
                                },
                                &state,
                            );
                            !seat.activate_session(active.id())
                        }
                    };
                    if !activation_failed
                        && !matches!(&epoch_start, EpochStart::Initial)
                        && let Some(status) = status.as_ref()
                        && status.human
                    {
                        match selection.snapshot() {
                            Ok(selected) => status.seat.refresh_status(&state, selected),
                            Err(error) => diagnostics.note(&match &epoch_start {
                                EpochStart::New => format!(
                                    "the new session status selection could not be refreshed: {error}"
                                ),
                                EpochStart::Fork { .. } => format!(
                                    "the forked session status selection could not be refreshed: {error}"
                                ),
                                EpochStart::Compact { .. } => format!(
                                    "the compacted session status selection could not be refreshed: {error}"
                                ),
                                EpochStart::Initial => unreachable!(
                                    "initial epoch status is not refreshed through transition setup"
                                ),
                            }),
                        }
                    }
                    if activation_failed {
                        last.termination = RunTermination::Failed {
                            message: format!(
                                "session {} was published but the terminal could not activate it",
                                active.id()
                            ),
                        };
                    }
                    let transition = if activation_failed {
                        EpochTransition::Exit
                    } else {
                        'epoch: loop {
                            let resumed_turn = resumed.take();
                            let queued = !state.unacknowledged_prompts().is_empty();
                            let asked = if resumed_turn.is_some() || queued {
                                None
                            } else {
                                let queue = control.status();
                                let pending_work = queue.pending_steers > 0
                                    || queue.queued > 0
                                    || queue.interrupt_pending
                                    || !state.unacknowledged_prompts().is_empty()
                                    || state.pending_question().is_some()
                                    || !state.live_attempts.is_empty();
                                seat.set_compaction_availability(compaction_availability(
                                    pending_work,
                                ));
                                // The terminal owner keeps its next read active while this
                                // coordinator drives the turn; output returns through that
                                // same owner's bounded painter channel.
                                let line = match seeded.take() {
                                    Some(first) => repl::Line::Submitted(first),
                                    None => {
                                        seat.idle();
                                        if let Some(status) = status.as_ref()
                                            && status.human
                                        {
                                            status.seat.activate();
                                        }
                                        seat.read()
                                    }
                                };

                                match line {
                                    repl::Line::HostCommand(
                                        repl::HostCommand::RemovePasteImage,
                                    ) => {
                                        if staged_images.pop().is_some() {
                                            seat.set_image_chips(paste_chips(&staged_images));
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::ClipboardPaste) => {
                                        if pending_work {
                                            seat.set_notice("image paste is unavailable while source work is pending".to_owned());
                                            continue;
                                        }
                                        let kind = match seat.pasteboard_kind() {
                                            Ok(kind) => kind,
                                            Err(error) => {
                                                seat.set_notice(format!(
                                                    "pasteboard could not be inspected: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        match kind {
                                            PasteboardKind::Image => {
                                                let limits = match paste_image_limits(
                                                    &source,
                                                    &confinement,
                                                    active.evidence(),
                                                    run_deadline,
                                                )
                                                .await
                                                {
                                                    Ok(limits) => limits,
                                                    Err(error) => {
                                                        seat.set_notice(error);
                                                        continue;
                                                    }
                                                };
                                                let cap = limits.max_bytes_per_image().map_or(
                                                    ego_core::generator::types::MAX_IMAGE_BYTES,
                                                    |limit| limit.get(),
                                                );
                                                let bytes = match seat.pasteboard_image(cap) {
                                                    Ok(bytes) => bytes,
                                                    Err(error) => {
                                                        seat.set_notice(error);
                                                        continue;
                                                    }
                                                };
                                                let image = match Image::new(
                                                    bytes,
                                                    ego_core::generator::types::ImageMediaType::Png,
                                                ) {
                                                    Ok(image) => image,
                                                    Err(error) => {
                                                        seat.set_notice(format!(
                                                            "pasteboard image is invalid: {error}"
                                                        ));
                                                        continue;
                                                    }
                                                };
                                                staged_images.push(StagedPasteImage {
                                                    label: "clipboard".to_owned(),
                                                    source: StagedPasteSource::Inline(image),
                                                });
                                                seat.set_image_chips(paste_chips(&staged_images));
                                            }
                                            PasteboardKind::Text => match seat.pasteboard_text() {
                                                Ok(text) => seat.insert_paste_text(text),
                                                Err(error) => seat.set_notice(error),
                                            },
                                            PasteboardKind::Empty => seat.set_notice(
                                                "pasteboard has no image or text".to_owned(),
                                            ),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::PastePath(path)) => {
                                        if pending_work {
                                            seat.set_notice("image paste is unavailable while source work is pending".to_owned());
                                            continue;
                                        }
                                        let limits = match paste_image_limits(
                                            &source,
                                            &confinement,
                                            active.evidence(),
                                            run_deadline,
                                        )
                                        .await
                                        {
                                            Ok(limits) => limits,
                                            Err(error) => {
                                                seat.set_notice(error);
                                                continue;
                                            }
                                        };
                                        let size = match std::fs::metadata(&path) {
                                            Ok(metadata) => metadata.len(),
                                            Err(error) => {
                                                seat.set_notice(format!(
                                                    "image paste could not inspect {path}: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let cap = limits.max_bytes_per_image().map_or(
                                            ego_core::generator::types::MAX_IMAGE_BYTES,
                                            |limit| limit.get(),
                                        );
                                        if size > cap {
                                            seat.set_notice(format!("image paste is {size} bytes, above the {cap}-byte limit"));
                                            continue;
                                        }
                                        let name = std::path::Path::new(&path)
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| path.clone());
                                        staged_images.push(StagedPasteImage {
                                            label: name,
                                            source: StagedPasteSource::Path(path),
                                        });
                                        seat.set_image_chips(paste_chips(&staged_images));
                                        continue;
                                    }
                                    repl::Line::Submitted(text) => {
                                        Some(InteractiveTurnInput::Prompt(text))
                                    }
                                    repl::Line::Blank => continue,
                                    // Ctrl+C at an idle prompt drops the line and nothing
                                    // else. Someone who pressed it to stop a turn must not
                                    // lose the session by pressing it once more.
                                    repl::Line::Interrupted => continue,
                                    repl::Line::Ended => break 'epoch EpochTransition::Exit,
                                    repl::Line::Refused(why) => {
                                        diagnostics.note(why);
                                        continue;
                                    }
                                    repl::Line::HostCommand(
                                        repl::HostCommand::SessionMetadata(operation),
                                    ) => {
                                        if pending_work {
                                            diagnostics.note(
                                                "session metadata is unavailable while source work is pending",
                                            );
                                            continue;
                                        }
                                        if let Err(error) = crate::metadata::execute_interactive(
                                            &interactive_metadata,
                                            &leased_session_journal,
                                            &session_journal,
                                            &operation,
                                            &interactive_metadata_ceilings,
                                            &mut |message| diagnostics.note(message),
                                        ) {
                                            diagnostics.note(&format!(
                                                "session metadata operation failed: {error}"
                                            ));
                                        }
                                        state = match session_journal.inspect(Clone::clone) {
                                            Ok(state) => state,
                                            Err(error) => {
                                                last.termination = hook_journal_termination(error);
                                                break 'epoch EpochTransition::Exit;
                                            }
                                        };
                                        last.state = state.clone();
                                        if let Some(status) = status.as_ref()
                                            && status.human
                                        {
                                            match selection.snapshot() {
                                                Ok(selected) => {
                                                    status.seat.refresh_status(&state, selected)
                                                }
                                                Err(error) => diagnostics.note(&format!(
                                                    "the metadata status selection could not be refreshed: {error}"
                                                )),
                                            }
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Skill(
                                        operation,
                                    )) => {
                                        if pending_work {
                                            diagnostics.note(
                                                "a skill cannot be invoked while source work is pending",
                                            );
                                            continue;
                                        }
                                        Some(InteractiveTurnInput::Skill(operation))
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Skills(
                                        repl::actions::SkillSurfaceAction::List,
                                    )) => {
                                        match skills.as_ref() {
                                            Some(catalogue) => match catalogue.capture() {
                                                Ok(capture) => {
                                                    diagnostics.note(&capture.render_list())
                                                }
                                                Err(error) => diagnostics.note(&format!(
                                                    "the skill catalogue could not be captured: {error}"
                                                )),
                                            },
                                            None => diagnostics
                                                .note("the skill catalogue is unavailable"),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Skills(
                                        repl::actions::SkillSurfaceAction::Diagnose,
                                    )) => {
                                        match skills.as_ref() {
                                            Some(catalogue) => match catalogue.capture() {
                                                Ok(capture) => {
                                                    diagnostics.note(&capture.render_diagnostics())
                                                }
                                                Err(error) => diagnostics.note(&format!(
                                                    "the skill catalogue could not be captured: {error}"
                                                )),
                                            },
                                            None => diagnostics
                                                .note("the skill catalogue is unavailable"),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Skills(
                                        repl::actions::SkillSurfaceAction::Rescan,
                                    )) => {
                                        if pending_work {
                                            diagnostics.note(
                                                "skills cannot be rescanned while source work is pending",
                                            );
                                            continue;
                                        }
                                        let Some(catalogue) = skills.as_ref() else {
                                            diagnostics.note("the skill catalogue is unavailable");
                                            continue;
                                        };
                                        seat.busy();
                                        let prepared = match catalogue.prepare_rescan() {
                                            Ok(prepared) => prepared,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "the skill catalogue could not be rescanned: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let published = prepared.published();
                                        let prospective = prepared.capture();
                                        debug_assert_eq!(
                                            prospective.retained_last_good(),
                                            !published
                                        );
                                        let next_registry = match tools(
                                            args.tools.as_ref(),
                                            Some(prospective.core()),
                                        ) {
                                            Ok(registry) => registry,
                                            Err(error) => {
                                                last.termination = RunTermination::Failed {
                                                    message: format!(
                                                        "the rescanned skill catalogue could not rebuild model tools: {error}"
                                                    ),
                                                };
                                                break 'epoch EpochTransition::Exit;
                                            }
                                        };
                                        let capture = match catalogue.commit_rescan(prepared) {
                                            Ok(capture) => capture,
                                            Err(error) => {
                                                last.termination = RunTermination::Failed {
                                                    message: format!(
                                                        "the prepared skill catalogue could not be published: {error}"
                                                    ),
                                                };
                                                break 'epoch EpochTransition::Exit;
                                            }
                                        };
                                        registry = next_registry;
                                        if published {
                                            diagnostics.note(&format!(
                                                "skills: published generation {}",
                                                capture.snapshot_digest()
                                            ));
                                        } else {
                                            diagnostics.note(&format!(
                                                "skills: retained last-good generation {}",
                                                capture.snapshot_digest()
                                            ));
                                            diagnostics.note(&capture.render_diagnostics());
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Hooks) => {
                                        match session_hooks.state.catalogue_snapshot() {
                                            Ok(catalogue) => match hooks::render(&catalogue, false) {
                                                Ok(rendered) if rendered.is_empty() => diagnostics
                                                    .note(&crate::output::empty_listing(
                                                        hooks::EMPTY_HOOKS,
                                                        "ego config edit",
                                                    )),
                                                Ok(rendered) => diagnostics.note(&rendered),
                                                Err(error) => diagnostics.note(&format!(
                                                    "the hook catalogue could not be rendered: {error}"
                                                )),
                                            },
                                            Err(error) => diagnostics.note(&format!(
                                                "the hook catalogue could not be captured: {error}"
                                            )),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::ModelSwitch(
                                        request,
                                    )) => {
                                        let Some(owner) = model_switch_owner.clone() else {
                                            diagnostics
                                    .note("the active session has no model-switch lifecycle owner");
                                            continue;
                                        };
                                        seat.busy();
                                        let receipt = format!(
                                            "Model: {}{}",
                                            request.target().0,
                                            if request.is_persistent() {
                                                ""
                                            } else {
                                                " (this session only)"
                                            }
                                        );
                                        let applied = owner.apply(request).await;
                                        let publication = match &applied {
                                            Ok(publication) => publication.as_ref(),
                                            Err(error) => error.publication(),
                                        };
                                        if let Some(
                                            UserConfigPublication::PublishedDurabilityUncertain {
                                                diagnostic,
                                            },
                                        ) = publication
                                        {
                                            diagnostics.note(&format!(
                                    "model defaults are authoritative with a durability warning: {diagnostic}"
                                ));
                                        }
                                        match applied {
                                            Ok(_) => diagnostics.note(&receipt),
                                            Err(error) => diagnostics
                                                .note(&format!("model switch failed: {error}")),
                                        }
                                        if let Some(status) = status.as_ref()
                                            && status.human
                                        {
                                            match selection.snapshot() {
                                    Ok(selected) => status.seat.refresh_status(&state, selected),
                                    Err(error) => diagnostics.note(&format!(
                                        "the model-switch status selection could not be refreshed: {error}"
                                    )),
                                }
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Policy(request)) => {
                                        let Some(owner) = policy_application.as_ref() else {
                                            let notice =
                                                "interactive policy controls are unavailable"
                                                    .to_owned();
                                            if let Some(status) = status.as_ref()
                                                && status.human
                                            {
                                                status.seat.set_notice(notice);
                                            } else {
                                                diagnostics.note(&notice);
                                            }
                                            continue;
                                        };
                                        if let PolicyControlRequest::SetMode(mode) = &request {
                                            let mode = *mode;
                                            let notice = match owner
                                                .apply(PolicyControlRequest::SetMode(mode))
                                            {
                                                Ok(PolicyControlOutcome::Mode { staging }) => {
                                                    if staging.changed {
                                                        format!(
                                                            "mode `{mode}` staged for the next generation"
                                                        )
                                                    } else {
                                                        format!("mode `{mode}` is already selected")
                                                    }
                                                }
                                                Ok(_) => "policy control failed: the outcome did not match the mode request".to_owned(),
                                                Err(error) => {
                                                    format!("policy control failed: {error}")
                                                }
                                            };
                                            if let Some(status) = status.as_ref()
                                                && status.human
                                            {
                                                status.seat.set_notice(notice);
                                            } else {
                                                diagnostics.note(&notice);
                                            }
                                            continue;
                                        }
                                        let Some(output) = text_output else {
                                            let notice =
                                                "interactive permission output is unavailable"
                                                    .to_owned();
                                            if let Some(status) = status.as_ref()
                                                && status.human
                                            {
                                                status.seat.set_notice(notice);
                                            } else {
                                                diagnostics.note(&notice);
                                            }
                                            continue;
                                        };
                                        let handled = handle_policy_request(
                                            request,
                                            |request| owner.apply(request),
                                            output,
                                            |_preview| {
                                                status
                                                    .as_ref()
                                                    .filter(|status| status.human)
                                                    .map_or(
                                                        repl::Confirmation::Declined,
                                                        |status| status.seat.confirm_remember(),
                                                    )
                                            },
                                        );
                                        match handled {
                                            Ok(Some(
                                                repl::Line::Ended
                                                | repl::Line::ExternalBreak(_),
                                            )) => break 'epoch EpochTransition::Exit,
                                            Ok(Some(_)) => diagnostics.note(
                                                "permission confirmation returned a non-terminal line",
                                            ),
                                            Ok(None) => {}
                                            Err(error) => {
                                                let notice =
                                                    format!("policy control failed: {error}");
                                                if let Some(status) = status.as_ref()
                                                    && status.human
                                                {
                                                    status.seat.set_notice(notice);
                                                } else {
                                                    diagnostics.note(&notice);
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Help,
                                    )) => {
                                        diagnostics.note(&seat.action_help());
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Expand,
                                    )) => {
                                        diagnostics.expand_last_tool();
                                        continue;
                                    }
                                    // The same snapshot `ego doctor` prints,
                                    // built by the same probes. Two renderings
                                    // of one value is the whole contract; two
                                    // implementations would be two diagnoses.
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Doctor,
                                    )) => {
                                        diagnostics.note(&crate::doctor::observed_with_width(
                                            root.clone(),
                                            seat.width(),
                                        ));
                                        continue;
                                    }
                                    // To the transcript rather than to a file
                                    // this action would have had to invent a
                                    // name for. `ego export ID --file` is where
                                    // a file is asked for by name; the two
                                    // disclosure modes and the three formats
                                    // are the same ones, carried by the slash
                                    // grammar — `/export local json`.
                                    repl::Line::HostCommand(repl::HostCommand::Export(
                                        selection,
                                    )) => {
                                        // The same workspace and store the
                                        // session was opened against, so
                                        // `ego --directory X` exports X and not
                                        // whatever the process happens to sit in.
                                        let args = crate::cli::ExportArgs {
                                            source: active.id(),
                                            sanitize: selection.sanitize,
                                            format: selection.format,
                                            file: None,
                                            session_dir: args.session_dir.clone(),
                                            directory: args.directory.clone(),
                                        };
                                        match exported(&args) {
                                            Ok(rendered) => diagnostics.note(&rendered),
                                            Err(termination) => diagnostics.note(&format!(
                                                "the session could not be exported: {}",
                                                termination.exit_reason()
                                            )),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Sessions,
                                    )) => {
                                        match crate::catalogue::listing(
                                            jsonl_store.as_ref(),
                                            &root,
                                            false,
                                        ) {
                                            Ok(metas) => diagnostics.note(
                                                &crate::catalogue::table(&metas, false).join("\n"),
                                            ),
                                            Err(error) => diagnostics.note(&format!(
                                                "the session catalogue could not be read: {error}"
                                            )),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Usage,
                                    )) => {
                                        let metadata = match crate::catalogue::listing(
                                            jsonl_store.as_ref(),
                                            &root,
                                            false,
                                        ) {
                                            Ok(metadata) => metadata,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                            "the session catalogue could not be read: {error}"
                                        ));
                                                continue;
                                            }
                                        };
                                        let Some(meta) = metadata
                                            .into_iter()
                                            .find(|meta| meta.id == active.id())
                                        else {
                                            diagnostics.note(
                                    "the active session is absent from its durable catalogue",
                                );
                                            continue;
                                        };
                                        match UsageReportService::new(jsonl_store.as_ref(), &clock)
                                            .report(std::slice::from_ref(&meta), UsagePeriod::All)
                                            .await
                                        {
                                            Ok(report) => {
                                                // `/usage` has no flags, so it
                                                // gets the compact default.
                                                let rendered = crate::usage::render_text(
                                                    &report,
                                                    false,
                                                    Some(state.context),
                                                );
                                                diagnostics.note(rendered.trim_end());
                                            }
                                            Err(error) => diagnostics.note(&format!(
                                                "the usage report could not be built: {error}"
                                            )),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::New,
                                    )) => {
                                        if pending_work {
                                            diagnostics.note(
                                    "a new session cannot be selected while source work is pending",
                                );
                                            continue;
                                        }
                                        let target = SessionId::new();
                                        let target_opened = match session::open(
                                            jsonl_store.as_ref(),
                                            &target,
                                            &clock,
                                        ) {
                                            Ok(opened) => opened,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "the new session could not be opened: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let target_owner = match ActiveSession::durable(
                                            target_opened,
                                            SessionState::default(),
                                            EVIDENCE_CAP,
                                        ) {
                                            Ok(owner) => owner,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                        "the new session evidence could not be opened: {error}"
                                    ));
                                                continue;
                                            }
                                        };
                                        if !seat.activate_session(target) {
                                            diagnostics.note(
                                                "the terminal could not activate the new session",
                                            );
                                            continue;
                                        }
                                        break 'epoch EpochTransition::New {
                                            target: target_owner,
                                        };
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Fork,
                                    )) => {
                                        if pending_work {
                                            diagnostics.note(
                                    "the session cannot be forked while source work is pending",
                                );
                                            continue;
                                        }
                                        let prepared = match ego_core::session::PreparedFork::new(
                                            active.id(),
                                            &state,
                                            session::now(&clock),
                                        ) {
                                            Ok(prepared) => prepared,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "the session cannot be forked: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let replayed = match active.replay_seed(prepared.seed()) {
                                            Ok(replayed) => replayed,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                        "the fork source context could not be replayed: {error}"
                                    ));
                                                continue;
                                            }
                                        };
                                        let target = SessionId::new();
                                        let target_opened = match session::open(
                                            jsonl_store.as_ref(),
                                            &target,
                                            &clock,
                                        ) {
                                            Ok(opened) => opened,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "the fork target could not be opened: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let target_state =
                                            SessionState::replay(prepared.seed().events().cloned());
                                        let mut target_owner = match ActiveSession::durable(
                                            target_opened,
                                            target_state,
                                            EVIDENCE_CAP,
                                        ) {
                                            Ok(owner) => owner,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                        "the fork target evidence could not be opened: {error}"
                                    ));
                                                continue;
                                            }
                                        };
                                        target_owner.replace_replayed(replayed);
                                        let publication =
                                            target_owner.publish_target(prepared.seed());
                                        let diagnostic =
                                            publication.diagnostic().map(str::to_string);
                                        if matches!(
                                            &publication,
                                            ego_core::session::TargetPublication::NotPublished { .. }
                                        ) {
                                            diagnostics.note(&format!(
                                                "the fork target was not published: {}",
                                                diagnostic
                                                    .as_deref()
                                                    .unwrap_or("publication was refused")
                                            ));
                                            continue;
                                        }
                                        break 'epoch EpochTransition::Fork {
                                            target: target_owner,
                                            publication,
                                            diagnostic,
                                            projected: project_public(prepared.seed().event()),
                                        };
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Compact,
                                    )) => {
                                        if compaction_availability(pending_work)
                                            != repl::actions::Availability::Enabled
                                        {
                                            continue;
                                        }
                                        if !seat.confirm_compaction(active.id()) {
                                            continue;
                                        }
                                        seat.busy();

                                        let target_runtime = Arc::new(Mutex::new(None));
                                        let prepared_runtime = Arc::clone(&target_runtime);
                                        let source_id = active.id();
                                        let request_id = RequestId::from_uuid(uuid::Uuid::now_v7());
                                        let pre_compact = match dispatch_request_hook(
                                            &session_hooks.runtime,
                                            request_id,
                                            session_hooks.turn,
                                            None,
                                            HookLifecycle::PreCompact,
                                        )
                                        .await
                                        {
                                            Ok(result) => result,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "compaction was blocked before preparation: {}",
                                                    error.exit_reason()
                                                ));
                                                continue;
                                            }
                                        };
                                        let compaction_cancel = CancelToken::new();
                                        let future: CompactionFuture<
                                            '_,
                                            Result<PreparedCompaction, CompactionError>,
                                        > = Box::pin(async {
                                            let target_system = instructions(
                                                &effective,
                                                &args,
                                                &root,
                                                &cwd,
                                                diagnostics,
                                            )
                                            .map_err(|error| {
                                                CompactionError::Internal(error.to_string())
                                            })?;
                                            let prepared_skills = skills
                                                .as_ref()
                                                .map(|catalogue| catalogue.prepare_rescan())
                                                .transpose()
                                                .map_err(|error| {
                                                    CompactionError::Internal(error.to_string())
                                                })?;
                                            let target_skills = prepared_skills
                                                .as_ref()
                                                .map(crate::skill::PreparedSkillRescan::capture);
                                            let target_registry = tools(
                                                args.tools.as_ref(),
                                                target_skills
                                                    .as_ref()
                                                    .map(crate::skill::SkillCatalogueCapture::core),
                                            )
                                            .map_err(|error| {
                                                CompactionError::Internal(error.to_string())
                                            })?;
                                            let target_tools: Vec<ToolDescriptor> = target_registry
                                                .iter()
                                                .map(RegisteredTool::descriptor)
                                                .collect();
                                            let (ports, observation, probe_evidence) =
                                                effect_scope(
                                                    &confinement,
                                                    active.evidence(),
                                                    &compaction_cancel,
                                                    run_deadline,
                                                )
                                                .await
                                                .map_err(compaction_termination)?;
                                            let http = Arc::new(ProviderHttpAdapter::new(
                                                Arc::clone(&ports),
                                            ))
                                                as Arc<dyn ego_ai::http::Http>;
                                            let runtime = GenerationRuntime {
                                                source: source.bind(
                                                    http,
                                                    compaction_cancel.clone(),
                                                    run_deadline,
                                                ),
                                                policy_source: Arc::clone(&policy_source),
                                                gate: gate.clone(),
                                                config: config.clone(),
                                                admission: Arc::clone(&evidence),
                                                invocation,
                                            };
                                            let effect_ports = ToolEffectPorts::new(
                                                &fs,
                                                ports.process(),
                                                active.evidence(),
                                                ports.resolver_handle(),
                                                ports.one_hop_handle(),
                                                ports.duplex_handle(),
                                            )
                                            .with_cancel(compaction_cancel.clone());
                                            let compaction_tools = match admitted_tools.as_ref() {
                                                Some(source) => Tools::from_registry_with_source(
                                                    &target_registry,
                                                    Arc::clone(source),
                                                    effect_ports,
                                                    CanonPath::new(&cwd),
                                                )
                                                .map_err(|error| {
                                                    CompactionError::Internal(error.to_string())
                                                })?,
                                                None => Tools::from_registry(
                                                    &target_registry,
                                                    effect_ports,
                                                    CanonPath::new(&cwd),
                                                ),
                                            };
                                            let compaction_tools =
                                                match McpRuntimeBinding::resume_sealer(
                                                    mcp_runtime.as_ref(),
                                                ) {
                                                    Some(resume) => {
                                                        compaction_tools.with_resume(resume)
                                                    }
                                                    None => compaction_tools,
                                                };
                                            let compaction_tools =
                                                match skill_resource_roots.as_ref() {
                                                    Some(roots) => compaction_tools
                                                        .with_skill_resource_roots(roots.clone()),
                                                    None => compaction_tools,
                                                };
                                            let mut compaction = TurnLoop::from_shared_journal(
                                                active.id(),
                                                session_journal.clone(),
                                                active.run_evidence(),
                                                compaction_cancel.clone(),
                                                runtime,
                                                compaction_tools,
                                            )
                                            .map_err(|error| {
                                                CompactionError::Internal(error.to_string())
                                            })?
                                            .with_hook_binding(
                                                session_hooks.state.clone(),
                                                Arc::clone(&session_hooks.binding),
                                                session_hooks.mcp.as_ref().map(Arc::clone),
                                            )
                                            .map_err(|error| {
                                                CompactionError::Internal(error.to_string())
                                            })?;
                                            if let Some(replayed) = active.replayed() {
                                                compaction = compaction.with_ancestor_conversation(
                                                    replayed.conversation().clone(),
                                                );
                                            }
                                            if let Some(elicitor) = elicitor.as_ref() {
                                                compaction =
                                                    compaction.with_elicitor(Arc::clone(elicitor));
                                            }
                                            if judge_available {
                                                compaction = compaction.with_judge(JudgeContext {
                                                    confinement: confinement.clone(),
                                                    exec_caps: observation.capabilities(),
                                                });
                                            }
                                            let mut compaction =
                                                super::composition::with_permission_analysis(
                                                    compaction,
                                                    skill_trust,
                                                )
                                                .with_execution_observation(
                                                    observation,
                                                    probe_evidence,
                                                )
                                                .with_interactive_control(&mut control);
                                            let prepared = compaction
                                                .prepare_compaction(
                                                    target_system.clone(),
                                                    target_tools,
                                                )
                                                .await;
                                            drop(compaction);
                                            state = session_hooks.state.state();
                                            seat.refresh_queue(control.status());
                                            if !state.unacknowledged_prompts().is_empty()
                                                || state.pending_question().is_some()
                                                || !state.live_attempts.is_empty()
                                            {
                                                return Err(CompactionError::Internal(
                                        "compaction stopped before target creation because new source work became pending"
                                            .to_owned(),
                                    ));
                                            }
                                            let prepared = prepared?;
                                            *prepared_runtime
                                                .lock()
                                                .expect("interactive compaction runtime") = Some((
                                                target_system,
                                                target_registry,
                                                prepared_skills,
                                            ));
                                            Ok(prepared)
                                        });
                                        let mut preparation = DeferredCompactionPreparation {
                                            source: source_id,
                                            future: Some(future),
                                        };
                                        let request = CompactionTransitionRequest {
                                            request_id,
                                            source: source_id,
                                        };
                                        let receipt = CompactionTransitionService::new(clock)
                                            .compact(
                                                request,
                                                &mut preparation,
                                                jsonl_store.as_ref(),
                                            )
                                            .await;
                                        drop(preparation);
                                        let receipt = match receipt {
                                            Ok(receipt) => receipt,
                                            Err(error) => {
                                                diagnostics
                                                    .note(&format!("compaction failed: {error}"));
                                                if let Some(status) = status.as_ref()
                                                    && status.human
                                                    && let Ok(selected) = selection.snapshot()
                                                {
                                                    status.seat.refresh_status(&state, selected);
                                                }
                                                continue;
                                            }
                                        };
                                        let target = receipt.target;
                                        let target_opened = match session::open(
                                            jsonl_store.as_ref(),
                                            &target,
                                            &clock,
                                        ) {
                                            Ok(opened) => Opened::Durable(Box::new(opened)),
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "compaction target could not be opened: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let target_state = target_opened.state();
                                        let target_owner = match ActiveSession::durable(
                                            target_opened,
                                            target_state,
                                            EVIDENCE_CAP,
                                        ) {
                                            Ok(owner) => owner,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                        "compaction target evidence could not be opened: {error}"
                                    ));
                                                continue;
                                            }
                                        };
                                        let diagnostic =
                                            receipt.publication.diagnostic().map(str::to_string);
                                        if matches!(
                                            &receipt.publication,
                                            ego_core::session::TargetPublication::NotPublished { .. }
                                        ) {
                                            diagnostics.note("compaction target was not published");
                                            continue;
                                        }
                                        let runtime = target_runtime
                                            .lock()
                                            .expect("interactive compaction runtime")
                                            .take();
                                        break 'epoch EpochTransition::Compact {
                                            target: target_owner,
                                            publication: receipt.publication,
                                            diagnostic,
                                            runtime,
                                            request: request_id,
                                            parent: pre_compact.closure_event(),
                                        };
                                    }
                                    repl::Line::HostCommand(_) => continue,
                                    repl::Line::ExternalBreak(_) => {
                                        break 'epoch EpochTransition::Exit;
                                    }
                                }
                            };

                            // Per turn, for the reason `build` documents.
                            seat.busy();
                            if let Some(status) = status.as_ref()
                                && status.human
                            {
                                status.seat.activate();
                            }
                            let turn_started = std::time::Instant::now();
                            let turn_cancel = CancelToken::new();
                            let (ports, observation, probe_evidence) = effect_scope(
                                &confinement,
                                active.evidence(),
                                &turn_cancel,
                                run_deadline,
                            )
                            .await?;
                            let inline = staged_images
                                .iter()
                                .filter_map(|staged| match &staged.source {
                                    StagedPasteSource::Inline(image) => Some(image.clone()),
                                    StagedPasteSource::Path(_) => None,
                                })
                                .collect::<Vec<_>>();
                            let specs = pending_attachments
                                .iter()
                                .cloned()
                                .chain(staged_images.iter().filter_map(
                                    |staged| match &staged.source {
                                        StagedPasteSource::Path(path) => Some(path.clone()),
                                        StagedPasteSource::Inline(_) => None,
                                    },
                                ))
                                .collect::<Vec<_>>();
                            let images = if matches!(
                                asked.as_ref(),
                                Some(InteractiveTurnInput::Prompt(_))
                            ) && (!inline.is_empty() || !specs.is_empty())
                            {
                                let images = match acquire_attachments(
                                    Attachments {
                                        inline: &inline,
                                        specs: &specs,
                                    },
                                    &source,
                                    &ports,
                                    &turn_cancel,
                                    run_deadline,
                                    &cwd,
                                    active.evidence().blob_store(),
                                )
                                .await
                                {
                                    Ok(images) => images,
                                    // An admission refusal of this turn's
                                    // attachments is not a bad command line:
                                    // the staged images stay so the user can
                                    // remove one (Ctrl+Backspace) and resubmit.
                                    Err(RunTermination::UsageError { message }) => {
                                        diagnostics.note(&format!(
                                                "{message}; the turn was not sent, remove an attachment and submit it again"
                                            ));
                                        continue;
                                    }
                                    Err(other) => return Err(other),
                                };
                                pending_attachments.clear();
                                staged_images.clear();
                                seat.set_image_chips(Vec::new());
                                images
                            } else {
                                Vec::new()
                            };
                            let loop_ = build!(
                                active,
                                &turn_cancel,
                                ports,
                                observation,
                                probe_evidence,
                                session_hooks,
                                session_journal,
                                skill_owner
                            );
                            let loop_ = match resumed_turn.as_ref() {
                                Some(RuntimeResume::Foreign { policy, .. }) => loop_
                                    .with_hook_policy(Arc::clone(policy))
                                    .map_err(|message| RunTermination::Failed { message })?,
                                Some(RuntimeResume::Model(_)) | None => loop_,
                            };
                            let mut loop_ = loop_.with_interactive_control(&mut control);
                            let driven = match resumed_turn {
                                Some(RuntimeResume::Model(turn)) => {
                                    let (termination, result) = turns
                                        .drive_resumed(&mut loop_, turn, &turn_cancel, diagnostics)
                                        .await;
                                    Some((termination, result))
                                }
                                Some(RuntimeResume::Foreign {
                                    question, answer, ..
                                }) => {
                                    let (termination, result) = turns
                                        .drive_answered(
                                            &mut loop_,
                                            question,
                                            answer,
                                            &turn_cancel,
                                            diagnostics,
                                        )
                                        .await;
                                    Some((termination, result))
                                }
                                None => match asked {
                                    Some(InteractiveTurnInput::Prompt(asked)) => {
                                        let (termination, result) = turns
                                            .drive_with_images(
                                                &mut loop_,
                                                &asked,
                                                images,
                                                &turn_cancel,
                                                diagnostics,
                                            )
                                            .await;
                                        Some((termination, result))
                                    }
                                    Some(InteractiveTurnInput::Skill(operation)) => {
                                        let (termination, result) = turns
                                            .drive_admitted_skill(
                                                &mut loop_,
                                                operation,
                                                &turn_cancel,
                                                diagnostics,
                                            )
                                            .await;
                                        Some((termination, result))
                                    }
                                    None => match loop_.run_queued_turn().await {
                                        Ok(Some(outcome)) => {
                                            let termination = loop_
                                                .state()
                                                .pending_question()
                                                .map(|pending| RunTermination::QuestionRequired {
                                                    question: Box::new(
                                                        QuestionReport::from_request(
                                                            pending.item_id(),
                                                            pending.target(),
                                                            pending.request(),
                                                        ),
                                                    ),
                                                })
                                                .or_else(|| {
                                                    RunTermination::after_turn(
                                                        outcome.exit_reason,
                                                        outcome.denied,
                                                    )
                                                })
                                                .unwrap_or(RunTermination::Done);
                                            Some((
                                                termination,
                                                Value::String(last_assistant_message(
                                                    &loop_.state(),
                                                )),
                                            ))
                                        }
                                        Ok(None) => None,
                                        Err(abandoned) => Some((
                                            RunTermination::Failed {
                                                message: format!(
                                                    "the session log could not be written: {}",
                                                    abandoned.cause
                                                ),
                                            },
                                            Value::String(last_assistant_message(&loop_.state())),
                                        )),
                                    },
                                },
                            };
                            drop(loop_);
                            state = session_hooks.state.state();
                            if let Some(status) = status.as_ref()
                                && status.human
                            {
                                match policy_selection.snapshot() {
                                    Ok(policy) => status.seat.set_status_mode(policy.mode),
                                    Err(error) => diagnostics.note(&format!(
                                        "the active policy mode could not be refreshed: {error}"
                                    )),
                                }
                                match selection.snapshot() {
                        Ok(selected) => status.seat.refresh_status(&state, selected),
                        Err(error) => diagnostics.note(&format!(
                            "the native status selection could not be refreshed: {error}"
                        )),
                    }
                            }
                            seat.refresh_queue(control.status());
                            let Some((termination, result)) = driven else {
                                continue;
                            };
                            between();
                            if matches!(termination, RunTermination::Signalled(Signal::Int))
                                && !cancel.is_cancelled()
                            {
                                diagnostics.note(&format!(
                                    "{} after {:.1}s",
                                    repl::INTERRUPTED_RECEIPT,
                                    turn_started
                                        .elapsed()
                                        .saturating_sub(seat.modal_wait())
                                        .as_secs_f64()
                                ));
                            }

                            let queue_abandoned =
                                matches!(&termination, RunTermination::Failed { .. });
                            last = Ended {
                                termination,
                                session: active.id(),
                                state: state.clone(),
                                result,
                                mcp_errors: mcp_errors.clone(),
                            };

                            // The person's own cancellations end a turn; the process's end
                            // the process. A broken stdout or a SIGTERM is not a turn to
                            // return to a prompt from.
                            if cancel.is_cancelled() {
                                break 'epoch EpochTransition::Exit;
                            }
                            if queue_abandoned {
                                break 'epoch EpochTransition::Exit;
                            }
                        }
                    };

                    drop(model_switch_owner);
                    let session_end = match skill_owner.drain().await {
                        Ok(()) => {
                            let runtime = match recovered_epoch_policy {
                                Some(policy) => session_hooks.bind_policy(policy),
                                None => session_hooks.bind_current(policy_source.as_ref()),
                            };
                            match runtime {
                                Ok(runtime) => {
                                    dispatch_session_hook(
                                        &runtime,
                                        active.id(),
                                        session_hooks.turn,
                                        HookLifecycle::SessionEnd,
                                    )
                                    .await
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(RunTermination::Failed {
                            message: error.to_string(),
                        }),
                    };
                    drop(skill_owner);
                    drop(session_hooks);
                    drop(session_journal);
                    let source_state = leased_session_journal
                        .try_into_state()
                        .map_err(hook_journal_termination)?;
                    drop(session_hook_ports);
                    (transition, source_state, session_end)
                };

                match transition {
                    EpochTransition::Exit => {
                        active.replace_state(source_state.clone());
                        last.state = source_state;
                        if let Err(termination) = session_end {
                            last.termination = termination;
                        }
                        break 'sessions Ok(last);
                    }
                    EpochTransition::New { target } => {
                        active.replace_state(source_state);
                        if let Err(error) = session_end {
                            diagnostics.note(&format!(
                                "the previous session-end hook failed: {}",
                                error.exit_reason()
                            ));
                        }
                        active = target;
                        epoch_start = EpochStart::New;
                    }
                    EpochTransition::Fork {
                        target,
                        publication,
                        diagnostic,
                        projected,
                    } => {
                        active.replace_state(source_state);
                        if !active.apply_publication(target, &publication) {
                            return Err(RunTermination::Failed {
                                message: format!(
                                    "the published fork target could not be activated: {}",
                                    diagnostic.as_deref().unwrap_or("publication was refused")
                                ),
                            });
                        }
                        if let Err(error) = session_end {
                            diagnostics.note(&format!(
                                "the fork source session-end hook failed: {}",
                                error.exit_reason()
                            ));
                        }
                        epoch_start = EpochStart::Fork {
                            diagnostic,
                            projected: projected.map(Box::new),
                        };
                    }
                    EpochTransition::Compact {
                        target,
                        publication,
                        diagnostic,
                        runtime,
                        request,
                        parent,
                    } => {
                        active.replace_state(source_state);
                        if !active.apply_publication(target, &publication) {
                            return Err(RunTermination::Failed {
                                message: "the published compaction target could not be activated"
                                    .to_owned(),
                            });
                        }
                        if let Err(error) = session_end {
                            diagnostics.note(&format!(
                                "the compacted source session-end hook failed: {}",
                                error.exit_reason()
                            ));
                        }
                        if let Some((target_system, target_registry, prepared_skills)) = runtime {
                            if let Some(prepared) = prepared_skills {
                                let published = prepared.published();
                                let Some(catalogue) = skills.as_ref() else {
                                    return Err(RunTermination::Failed {
                                        message: "the prepared skill catalogue lost its publication owner"
                                            .to_owned(),
                                    });
                                };
                                let capture = catalogue.commit_rescan(prepared).map_err(|error| {
                                    RunTermination::Failed {
                                        message: format!(
                                            "the prepared skill catalogue could not be published after compaction activation: {error}"
                                        ),
                                    }
                                })?;
                                if published {
                                    diagnostics.note(&format!(
                                        "skills: published generation {}",
                                        capture.snapshot_digest()
                                    ));
                                } else {
                                    diagnostics.note(&format!(
                                        "skills: retained last-good generation {}",
                                        capture.snapshot_digest()
                                    ));
                                    diagnostics.note(&capture.render_diagnostics());
                                }
                            }
                            registry = target_registry;
                            config.system = target_system;
                        }
                        epoch_start = EpochStart::Compact {
                            diagnostic,
                            request,
                            parent,
                        };
                    }
                }
            }
        }
    }
}
