//! Drive an admitted session through its selected headless or interactive posture.

use super::*;

async fn admit_saved_session_policy(
    active: &mut ActiveSession<'_>,
    observation: &ExecObservation,
    root: &Path,
    journal_ports: (&SystemClock, &dyn EventSink),
    diagnostics: &dyn Diagnostics,
    human: Option<&dyn HumanApprover>,
) -> Result<(), RunTermination> {
    // The saved authority is the observation the last admitted turn journaled.
    let Some(previous) = active.state().execution_observation.clone() else {
        return Ok(());
    };
    if observation.same_measurement(&previous) {
        return Ok(());
    }
    let (clock, sink) = journal_ports;
    let mut journal = Journal::new(
        active.id(),
        active.store(),
        clock,
        sink,
        Producer::Main,
        active.state().clone(),
    );
    ego_core::run::admit_execution_policy(
        &previous,
        observation,
        CanonPath::new(root),
        human,
        &mut journal,
        |message| {
            diagnostics.note(message);
            Ok(())
        },
    )
    .await
    .map_err(|error| RunTermination::PermissionRequired {
        action: error.to_string(),
        by: DeniedBy::Nobody,
    })?;
    let state = journal.state().clone();
    drop(journal);
    active.replace_state(state);
    Ok(())
}

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
    let (ports, _, _) = effect_scope(confinement, evidence, &paste_cancel, run_deadline, None)
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

/// A successor session opened inside this process needs its own run context
/// before its first generation can be admitted.
fn admit_successor_context(
    admission: &EvidenceAdmissionService,
    active: &ActiveSession<'_>,
    run_context: &Option<RunContextEvidence>,
) -> Result<(), RunTermination> {
    let context = run_context
        .as_ref()
        .expect("the admitted run context outlives its first session")
        .for_session(active.id());
    admission
        .admit_run_context(active.run_evidence(), context)
        .map(|_| ())
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })
}

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
    Resume {
        meta: ego_core::store::session_store::SessionMeta,
    },
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
        mcp_registry,
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
        context: run_context,
        mode: _,
        policy_selection,
        policy_source,
        mut perimeter,
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
    let mut session_mcp = effective
        .mcp
        .entries()
        .iter()
        .filter_map(|(name, entry)| match entry {
            ego_core::config::ResolvedMcpEntry::Configured {
                server, provenance, ..
            } => Some((name.clone(), server.clone(), provenance.clone())),
            ego_core::config::ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .collect();
    let mut disabled_mcp = effective
        .mcp
        .entries()
        .iter()
        .filter(|(name, entry)| {
            matches!(
                entry,
                ego_core::config::ResolvedMcpEntry::Configured { enabled: false, .. }
            ) && mcp_registry.status().server(name).is_none()
        })
        .map(|(name, _)| name.clone())
        .collect();
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
        cache: effective.cache,
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

    let exec_environment = effective
        .exec
        .policy()
        .expect("admitted config has valid environment names");
    let mut refresh_prompt = false;

    let turns = Turns {
        schema: compiled.as_ref(),
        retries: args.schema_retries,
        max_turns: args.max_turns,
        deadline: run_deadline,
    };

    macro_rules! open_session_hooks {
        ($active:expr, $state:expr, $hook_ports:ident, $policy:expr) => {{
            let hook_effects = ToolEffectPorts::new(
                &fs,
                $hook_ports.process(),
                $active.evidence(),
                $hook_ports.resolver_handle(),
                $hook_ports.one_hop_handle(),
                $hook_ports.duplex_handle(),
            )
            .with_secret_values(ego_tools::secrets::from_env());
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
                    answer: answer
                        .expect("only the terminal plans an answerless foreign presentation"),
                    policy,
                },
                None => HeadlessInput::Prompt(&RunPrompt::from(text.clone())),
            };
            // One run is one session: the judge's grants die with it.
            let residual = judge_available.then(ResidualWriteDesk::new);
            let (ports, observation, probe_evidence) = effect_scope(
                &confinement,
                active.evidence(),
                &cancel,
                run_deadline,
                residual.clone(),
            )
            .await?;
            admit_saved_session_policy(
                &mut active,
                &observation,
                &root,
                (&clock, sink),
                diagnostics,
                None,
            )
            .await?;
            let opening = active.state().clone();
            let pause = PauseToken::default();
            let driven = drive_headless_turn(
                HeadlessSession {
                    active: &mut active,
                    opening,
                    clock: &clock,
                    sink,
                    cancel: &cancel,
                    pause: &pause,
                    control: None,
                },
                HeadlessGeneration {
                    source: Arc::clone(&source),
                    admission: Arc::clone(&evidence),
                    invocation,
                    config: config.clone(),
                    provider_recovery: args.provider_recovery,
                    goal: args.goal.clone(),
                    goal_turns: args.goal_turns,
                    goal_check_command: args.goal_check_command.clone(),
                    goal_ceilings: effective.ceilings.clone(),
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
                selected_session: None,
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
            replay_interactive_session(&args, seat.as_ref(), &active, text_output, diagnostics);
            let interactive_metadata =
                SessionMetadata::new(Arc::clone(jsonl_store), Arc::new(clock));
            let mut control = seat.take_control();
            let mut state = opening;
            let mut last = Ended {
                termination: RunTermination::Done,
                session: active.id(),
                state: state.clone(),
                result: Value::String(String::new()),
                selected_session: None,
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
            if !args.no_context_tool && args.tools.is_none() {
                registry.push(RegisteredTool::Context);
            }
            let mut checkpoint_pending = false;
            let mut last_generation_request = None;
            let mut automatic_compaction_failed = false;

            // Borrow-scoped session authorities live in one lexical epoch.
            // Only owned transition data crosses the boundary where the
            // replaceable ActiveSession is mutated or replaced.
            'sessions: loop {
                // A new, forked, compacted or resumed epoch is another
                // session, so the judge's grants do not cross into it.
                let residual = judge_available.then(ResidualWriteDesk::new);
                let (transition, source_state, session_end) = {
                    let (session_hook_ports, observation, probe_evidence) =
                        effect_scope(&confinement, active.evidence(), &cancel, run_deadline, None)
                            .await?;
                    let human = seat
                        .has_human()
                        .then(|| SeatApprover { seat: seat.clone() });
                    admit_saved_session_policy(
                        &mut active,
                        &observation,
                        &root,
                        (&clock, sink),
                        diagnostics,
                        human.as_ref().map(|human| human as &dyn HumanApprover),
                    )
                    .await?;
                    perimeter =
                        perimeter.with_execution_observation(&observation, probe_evidence.as_ref());
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
                    dispatch_post_compact_hook(&epoch_start, &session_hooks, diagnostics).await;
                    state = session_hooks.state.state();
                    last.session = active.id();
                    last.state = state.clone();
                    last.result = Value::String(String::new());
                    let activation_failed = activate_epoch(
                        &epoch_start,
                        active.id(),
                        seat.as_ref(),
                        sink,
                        &state,
                        EpochCapabilities {
                            registry: &registry,
                            system: config.system.as_ref(),
                            has_schema: compiled.is_some(),
                            mcp_errors: &mcp_errors,
                        },
                        diagnostics,
                    );
                    refresh_epoch_status(
                        activation_failed,
                        &epoch_start,
                        status.as_ref(),
                        &selection,
                        &state,
                        diagnostics,
                    );
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
                            } else if checkpoint_pending {
                                checkpoint_pending = false;
                                Some(InteractiveTurnInput::Checkpoint)
                            } else {
                                let queue = control.status();
                                let pending_work = queue.pending_steers > 0
                                    || queue.queued > 0
                                    || queue.interrupt_pending
                                    || !state.unacknowledged_prompts().is_empty()
                                    || state.pending_question().is_some();
                                // Reopened generation attempts are interrupted by the
                                // compaction journal; they are not live terminal work.
                                seat.set_compaction_availability(compaction_availability(
                                    pending_work,
                                ));
                                // The terminal owner keeps its next read active while this
                                // coordinator drives the turn; output returns through that
                                // same owner's bounded painter channel.
                                let model_compaction =
                                    state.context_request.is_some() && !args.no_context_tool;
                                let automatic_compaction = !automatic_compaction_failed
                                    && !pending_work
                                    && (model_compaction
                                        || ego_core::turn::compaction_due(state.context));
                                let line = if automatic_compaction {
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Compact,
                                    ))
                                } else {
                                    match seeded.take() {
                                        Some(first) => repl::Line::Submitted(first),
                                        None => {
                                            read_interactive_command(seat.as_ref(), status.as_ref())
                                        }
                                    }
                                };

                                if let Some(residual) = &residual {
                                    perimeter = perimeter.with_residual_grants(&residual.grants());
                                }

                                match dispatch_interactive_command(
                                    InteractiveCommand {
                                        args: &args,
                                        active: &active,
                                        seat: &seat,
                                        source: &source,
                                        confinement: &confinement,
                                        run_deadline,
                                        staged_images: &mut staged_images,
                                        last: &mut last,
                                        state: &mut state,
                                        diagnostics,
                                        interactive_metadata: &interactive_metadata,
                                        leased_session_journal: &leased_session_journal,
                                        session_journal: &session_journal,
                                        interactive_metadata_ceilings:
                                            &interactive_metadata_ceilings,
                                        status: &status,
                                        selection: &selection,
                                        skills: &skills,
                                        effective: &effective,
                                        jsonl_store,
                                        exec_environment: exec_environment.clone(),
                                        registry: &mut registry,
                                        session_hooks: &session_hooks,
                                        model_switch_owner: model_switch_owner.clone(),
                                        policy_application: &policy_application,
                                        text_output,
                                        session_hook_ports: &session_hook_ports,
                                        skill_profile_layers: &skill_profile_layers,
                                        perimeter: &perimeter,
                                        mcp_registry,
                                        session_mcp: &mut session_mcp,
                                        disabled_mcp: &mut disabled_mcp,
                                        root: &root,
                                        cwd: &cwd,
                                        clock: &clock,
                                        config: &mut config,
                                        refresh_prompt: &mut refresh_prompt,
                                        automatic_compaction_failed:
                                            &mut automatic_compaction_failed,
                                        policy_source: &policy_source,
                                        gate: &gate,
                                        evidence: &evidence,
                                        invocation,
                                        admitted_tools: &admitted_tools,
                                        mcp_runtime: &(mcp_runtime.clone()
                                            as Arc<dyn McpRuntimeBinding>),
                                        skill_resource_roots: &skill_resource_roots,
                                        last_generation_request: &last_generation_request,
                                        elicitor: &elicitor,
                                        judge_available,
                                        skill_trust,
                                        control: &mut control,
                                        checkpoint_pending: &mut checkpoint_pending,
                                    },
                                    line,
                                    (pending_work, model_compaction, automatic_compaction),
                                )
                                .await?
                                {
                                    CommandOutcome::Continue => continue,
                                    CommandOutcome::Input(input) => Some(input),
                                    CommandOutcome::Transition(transition) => {
                                        break 'epoch *transition;
                                    }
                                }
                            };

                            let (exit, next_perimeter) = drive_interactive_turn_phase(
                                InteractiveTurnPhase {
                                    seat: seat.as_ref(),
                                    status: status.as_ref(),
                                    confinement: &confinement,
                                    active: &active,
                                    run_deadline,
                                    residual: residual.clone(),
                                    perimeter,
                                    session_hook_ports: &session_hook_ports,
                                    pending_attachments: &mut pending_attachments,
                                    staged_images: &mut staged_images,
                                    source: &source,
                                    cwd: &cwd,
                                    diagnostics,
                                    state: &mut state,
                                    args: &args,
                                    effective: &effective,
                                    policy_source: &policy_source,
                                    gate: &gate,
                                    config: &config,
                                    evidence: &evidence,
                                    invocation,
                                    registry: &registry,
                                    admitted_tools: &admitted_tools,
                                    mcp_runtime: mcp_runtime.as_ref(),
                                    skill_resource_roots: &skill_resource_roots,
                                    elicitor: &elicitor,
                                    judge_available,
                                    skill_trust,
                                    session_hooks: &session_hooks,
                                    session_journal: &session_journal,
                                    skill_owner: skill_owner.clone(),
                                    refresh_prompt: &mut refresh_prompt,
                                    control: &mut control,
                                    turns: &turns,
                                    last_generation_request: &mut last_generation_request,
                                    policy_selection: &policy_selection,
                                    selection: &selection,
                                    between,
                                    cancel: &cancel,
                                    last: &mut last,
                                    mcp_errors: &mcp_errors,
                                },
                                resumed_turn,
                                asked,
                            )
                            .await?;
                            perimeter = next_perimeter;
                            if exit {
                                break 'epoch EpochTransition::Exit;
                            }
                        }
                    };

                    drop(model_switch_owner);
                    let session_end = end_session_epoch(
                        &skill_owner,
                        &session_hooks,
                        recovered_epoch_policy,
                        policy_source.as_ref(),
                        active.id(),
                    )
                    .await;
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
                        finish_interactive_session(
                            &mut active,
                            &mut last,
                            source_state,
                            session_end,
                            None,
                        );
                        break 'sessions Ok(last);
                    }
                    EpochTransition::Resume { meta } => {
                        finish_interactive_session(
                            &mut active,
                            &mut last,
                            source_state,
                            session_end,
                            Some((meta, seat.as_ref(), control)),
                        );
                        break 'sessions Ok(last);
                    }
                    EpochTransition::New { target } => {
                        epoch_start = activate_new_session(
                            &mut active,
                            source_state,
                            session_end,
                            target,
                            (&evidence, &run_context, diagnostics),
                        )?;
                    }
                    EpochTransition::Fork {
                        target,
                        publication,
                        diagnostic,
                        projected,
                    } => {
                        epoch_start = activate_fork_transition(
                            &mut active,
                            source_state,
                            session_end,
                            (target, publication, diagnostic, projected),
                            (&evidence, &run_context, diagnostics),
                        )?;
                    }
                    EpochTransition::Compact {
                        target,
                        publication,
                        diagnostic,
                        runtime,
                        request,
                        parent,
                    } => {
                        epoch_start = activate_compaction_transition(
                            &mut active,
                            source_state,
                            session_end,
                            (target, publication, diagnostic, runtime, request, parent),
                            CompactionActivation {
                                evidence: &evidence,
                                run_context: &run_context,
                                diagnostics,
                                skills: skills.as_deref(),
                                registry: &mut registry,
                                config: &mut config,
                            },
                        )?;
                    }
                }
            }
        }
    }
}

async fn stage_clipboard_image(
    seat: &dyn InteractiveSeat,
    pending_work: bool,
    source: &Arc<model::CliGenerationModelSource>,
    effects: (
        &RunConfinement,
        &SessionEvidenceStore,
        Option<std::time::Instant>,
    ),
    staged_images: &mut Vec<StagedPasteImage>,
) {
    let (confinement, evidence, run_deadline) = effects;
    if pending_work {
        seat.set_notice("image paste is unavailable while source work is pending".to_owned());
        return;
    }
    let kind = match seat.pasteboard_kind() {
        Ok(kind) => kind,
        Err(error) => {
            seat.set_notice(format!("pasteboard could not be inspected: {error}"));
            return;
        }
    };
    match kind {
        PasteboardKind::Image => {
            let limits = match paste_image_limits(source, confinement, evidence, run_deadline).await
            {
                Ok(limits) => limits,
                Err(error) => {
                    seat.set_notice(error);
                    return;
                }
            };
            let cap = limits
                .max_bytes_per_image()
                .map_or(ego_core::generator::types::MAX_IMAGE_BYTES, |limit| {
                    limit.get()
                });
            let bytes = match seat.pasteboard_image(cap) {
                Ok(bytes) => bytes,
                Err(error) => {
                    seat.set_notice(error);
                    return;
                }
            };
            let image = match Image::new(bytes, ego_core::generator::types::ImageMediaType::Png) {
                Ok(image) => image,
                Err(error) => {
                    seat.set_notice(format!("pasteboard image is invalid: {error}"));
                    return;
                }
            };
            staged_images.push(StagedPasteImage {
                label: "clipboard".to_owned(),
                source: StagedPasteSource::Inline(image),
            });
            seat.set_image_chips(paste_chips(staged_images));
        }
        PasteboardKind::Text => match seat.pasteboard_text() {
            Ok(text) => seat.insert_paste_text(text),
            Err(error) => seat.set_notice(error),
        },
        PasteboardKind::Empty => seat.set_notice("pasteboard has no image or text".to_owned()),
    }
}

async fn stage_image_path(
    seat: &dyn InteractiveSeat,
    pending_work: bool,
    path: String,
    source: &Arc<model::CliGenerationModelSource>,
    effects: (
        &RunConfinement,
        &SessionEvidenceStore,
        Option<std::time::Instant>,
    ),
    staged_images: &mut Vec<StagedPasteImage>,
) {
    let (confinement, evidence, run_deadline) = effects;
    if pending_work {
        seat.set_notice("image paste is unavailable while source work is pending".to_owned());
        return;
    }
    let limits = match paste_image_limits(source, confinement, evidence, run_deadline).await {
        Ok(limits) => limits,
        Err(error) => {
            seat.set_notice(error);
            return;
        }
    };
    let size = match std::fs::metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            seat.set_notice(format!("image paste could not inspect {path}: {error}"));
            return;
        }
    };
    let cap = limits
        .max_bytes_per_image()
        .map_or(ego_core::generator::types::MAX_IMAGE_BYTES, |limit| {
            limit.get()
        });
    if size > cap {
        seat.set_notice(format!(
            "image paste is {size} bytes, above the {cap}-byte limit"
        ));
        return;
    }
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    staged_images.push(StagedPasteImage {
        label: name,
        source: StagedPasteSource::Path(path),
    });
    seat.set_image_chips(paste_chips(staged_images));
}

async fn run_interactive_setup(
    prompt: String,
    root: &Path,
    status: Option<&InteractiveStatus>,
    diagnostics: &dyn Diagnostics,
) {
    let setup_args = crate::cli::SetupArgs {
        items: None,
        judge: None,
        prompt: (!prompt.is_empty()).then_some(prompt),
        profile: None,
        directory: Some(root.to_path_buf()),
    };
    let confirm = |question: String| {
        status
            .as_ref()
            .filter(|status| status.human)
            .is_some_and(|status| {
                matches!(
                    status.seat.confirm_setup(question),
                    repl::Confirmation::Confirmed
                )
            })
    };
    let select = |findings: &[ego_core::setup::Finding]| {
        use ego_core::question::{
            QuestionAnswer, QuestionMode, QuestionOption, QuestionOutcome, QuestionRequest,
        };
        let status = status.as_ref().filter(|status| status.human)?;
        let options = findings
            .iter()
            .map(|finding| {
                QuestionOption::new(
                    finding.id.to_string(),
                    ego_core::permit::terminal_safe(&format!(
                        "{}: {} — {}",
                        finding.id,
                        finding.requested.display(),
                        finding.evidence
                    )),
                )
            })
            .collect::<Vec<_>>();
        if options.is_empty() {
            return Some(Vec::new());
        }
        let request = QuestionRequest::new(
            "Select setup proposal items; only selected items enter the preview. Final confirmation still required.",
            QuestionMode::Multiple,
            options,
            false,
        );
        match status.seat.select_setup(request) {
            QuestionOutcome::Answered(QuestionAnswer::Multiple(ids)) => {
                ids.into_iter().map(|id| id.parse::<usize>().ok()).collect()
            }
            _ => None,
        }
    };
    let mut setup_cancel = status
        .as_ref()
        .map(|status| status.seat.setup_cancellation());
    let terminal_cancel = async {
        match setup_cancel.as_mut() {
            Some(cancel) => cancel.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    let setup_result = tokio::select! {
        biased;
        _ = terminal_cancel => Err(crate::cli::UsageError("Setup declined/cancelled; no configuration writes".into())),
        _ = super::turns::stop_signal(None) => Err(crate::cli::UsageError("Setup declined/cancelled; no configuration writes".into())),
        result = crate::setup::observed_selection_host(
            root.to_path_buf(),
            &setup_args,
            Some(&confirm),
            Some(&select),
        ) => result,
    };
    drop(setup_cancel);
    match setup_result {
        Ok(report) => diagnostics.note(&report),
        Err(error) => diagnostics.note(&error.to_string()),
    }
}

async fn run_interactive_config(
    arguments: &str,
    effective: &Effective,
    skill_profile_layers: Option<&[Sourced<ConfigLayer>]>,
    perimeter: &crate::perimeter::Perimeter,
    diagnostics: &dyn Diagnostics,
) {
    if arguments == "perimeter" {
        diagnostics.note(&perimeter.text());
    } else {
        match crate::config::repl_set_args(arguments) {
            Ok(set) => match crate::config::repl_set(&set) {
                Ok(receipt) => {
                    diagnostics.note(&receipt);
                    diagnostics.note("Saved to user base configuration; applies to future sessions. Current session authority is unchanged.");
                    if let Some(warning) = crate::config::saved_override(
                        &set,
                        effective,
                        skill_profile_layers.unwrap_or_default(),
                    ) {
                        diagnostics.note(&warning);
                    }
                    diagnostics.note(&crate::config::repl_text(
                        effective,
                        skill_profile_layers.unwrap_or_default(),
                        perimeter,
                    ));
                }
                Err(error) => diagnostics.note(&format!("config set failed: {error:?}")),
            },
            Err(error) => diagnostics.note(&error.to_string()),
        }
    }
}

async fn select_session_transition<'store>(
    command: repl::HostCommand,
    seat: &dyn InteractiveSeat,
    pending_work: bool,
    store: &'store Arc<JsonlStore>,
    root: &Path,
    session_id: SessionId,
    journal_ports: (&SystemClock, &dyn Diagnostics),
) -> Option<EpochTransition<'store>> {
    let (clock, diagnostics) = journal_ports;
    if !seat.has_human() {
        match crate::catalogue::listing(store.as_ref(), root, true) {
            Ok(metas) => diagnostics.note(&crate::catalogue::table(&metas, false).join("\n")),
            Err(error) => {
                diagnostics.note(&format!("the session catalogue could not be read: {error}"))
            }
        }
        return None;
    }
    if pending_work {
        diagnostics.note("a session cannot be selected while source work is pending");
        return None;
    }
    let mut metas: Vec<_> = match crate::catalogue::saved_sessions(store.as_ref()) {
        Ok(metas) => metas
            .into_iter()
            .filter(|meta| meta.id != session_id)
            .collect(),
        Err(error) => {
            diagnostics.note(&format!("the session catalogue could not be read: {error}"));
            return None;
        }
    };
    if let repl::HostCommand::History(query) = &command {
        let history = store.history();
        let query = query.clone();
        let hits = match tokio::task::spawn_blocking(move || history.matches(&query)).await {
            Ok(Ok(hits)) => hits,
            result => {
                diagnostics.note(&format!("history could not be searched: {result:?}"));
                return None;
            }
        };
        metas.retain(|meta| hits.iter().any(|hit| hit.id == meta.id));
    }
    let target = match &command {
        repl::HostCommand::Resume(Some(id)) => match id.parse() {
            Ok(id) => Some(id),
            Err(_) => {
                diagnostics.note("invalid session ID");
                return None;
            }
        },
        repl::HostCommand::Resume(None) => metas
            .iter()
            .filter(|meta| meta.workspace_root == root)
            .max_by_key(|meta| meta.updated)
            .map(|meta| meta.id),
        _ => seat.pick_session(metas.clone()),
    };
    let target = target?;
    let meta = metas.into_iter().find(|meta| meta.id == target)?;
    if !meta.workspace_root.is_dir() {
        diagnostics.note("the selected session root is unavailable");
        return None;
    }
    let opened = match session::open(store.as_ref(), &target, clock) {
        Ok(opened) => opened,
        Err(error) => {
            diagnostics.note(&format!(
                "the selected session could not be opened: {error}"
            ));
            return None;
        }
    };
    let target_state = opened.state.clone();
    // Interrupted work requires the full `ego resume` recovery path.
    if target_state.turns.values().any(|turn| !turn.completed)
        || !target_state.unacknowledged_prompts().is_empty()
        || target_state.pending_question().is_some()
        || !target_state.live_attempts.is_empty()
    {
        diagnostics.note(&format!(
            "session {target} has unfinished work; use ego resume {target}"
        ));
        return None;
    }
    if meta.workspace_root != root {
        drop(opened);
        return Some(EpochTransition::Resume { meta });
    }
    let target_owner = match ActiveSession::durable(opened, target_state, EVIDENCE_CAP) {
        Ok(owner) => owner,
        Err(error) => {
            diagnostics.note(&format!(
                "the selected session could not be replayed: {error}"
            ));
            return None;
        }
    };
    if !seat.activate_session(target) {
        diagnostics.note("the terminal could not activate the selected session");
        return None;
    }
    diagnostics.note(&format!("resumed session {target}"));
    Some(EpochTransition::New {
        target: target_owner,
    })
}

async fn new_session_transition<'store>(
    seat: &dyn InteractiveSeat,
    pending_work: bool,
    store: &'store Arc<JsonlStore>,
    clock: &SystemClock,
    diagnostics: &dyn Diagnostics,
) -> Option<EpochTransition<'store>> {
    if pending_work {
        diagnostics.note("a new session cannot be selected while source work is pending");
        return None;
    }
    let target = SessionId::new();
    let target_opened = match session::open(store.as_ref(), &target, clock) {
        Ok(opened) => opened,
        Err(error) => {
            diagnostics.note(&format!("the new session could not be opened: {error}"));
            return None;
        }
    };
    let target_owner =
        match ActiveSession::durable(target_opened, SessionState::default(), EVIDENCE_CAP) {
            Ok(owner) => owner,
            Err(error) => {
                diagnostics.note(&format!(
                    "the new session evidence could not be opened: {error}"
                ));
                return None;
            }
        };
    if !seat.activate_session(target) {
        diagnostics.note("the terminal could not activate the new session");
        return None;
    }
    Some(EpochTransition::New {
        target: target_owner,
    })
}

async fn fork_session_transition<'store>(
    command: repl::HostCommand,
    pending_work: bool,
    store: &'store Arc<JsonlStore>,
    session_id: SessionId,
    state: &SessionState,
    clock: &SystemClock,
    diagnostics: &dyn Diagnostics,
) -> Option<EpochTransition<'store>> {
    if pending_work {
        diagnostics.note("the session cannot switch forks while source work is pending");
        return None;
    }
    let lifecycle = ego_core::session_lifecycle::SessionLifecycle::new(
        Arc::clone(store),
        Arc::new(super::session_commands::NoLiveRuns),
        Arc::new(SystemClock),
    );
    let fork_at_message = matches!(command, repl::HostCommand::Fork(_));
    let prepared = match command {
        repl::HostCommand::Fork(item) => lifecycle.prepare_fork_at(session_id, item),
        _ => ego_core::session::PreparedFork::new(session_id, state, session::now(clock))
            .map_err(ego_core::session_lifecycle::LifecycleError::from),
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            diagnostics.note(&format!("the session cannot be forked: {error}"));
            return None;
        }
    };
    let target = SessionId::new();
    let publication = match lifecycle.publish_prepared_fork(target, &prepared) {
        Ok(publication) => publication,
        Err(error) => {
            diagnostics.note(&format!("the fork target was not published: {error}"));
            return None;
        }
    };
    let target_opened = match session::open(store.as_ref(), &target, clock) {
        Ok(opened) => opened,
        Err(error) => {
            diagnostics.note(&format!(
                "the published fork target {target} could not be opened: {error}"
            ));
            return None;
        }
    };
    // Only a tip fork preserves the measured source context.
    // An earlier message has a different provider baseline.
    let mut target_state = SessionState::replay(prepared.seed().events().cloned());
    if !fork_at_message {
        target_state.context = state.context;
    }
    let target_owner = match ActiveSession::durable(target_opened, target_state, EVIDENCE_CAP) {
        Ok(owner) => owner,
        Err(error) => {
            diagnostics.note(&format!(
                "the published fork target {target} could not be replayed: {error}"
            ));
            return None;
        }
    };
    let diagnostic = publication.diagnostic().map(str::to_string);
    Some(EpochTransition::Fork {
        target: target_owner,
        publication,
        diagnostic,
        projected: project_public(prepared.seed().event()),
    })
}

async fn drive_interactive_input<'session: 'turn, 'turn>(
    turns: &Turns<'_>,
    loop_: &mut TurnLoop<'session, 'turn>,
    resumed_turn: Option<RuntimeResume>,
    asked: Option<InteractiveTurnInput>,
    images: Vec<ImageRef>,
    turn_cancel: &CancelToken,
    diagnostics: &dyn Diagnostics,
) -> Option<(RunTermination, Value)> {
    match resumed_turn {
        Some(RuntimeResume::Model(turn)) => {
            let (termination, result) = turns
                .drive_resumed(loop_, turn, turn_cancel, diagnostics)
                .await;
            Some((termination, result))
        }
        Some(RuntimeResume::Foreign {
            question, answer, ..
        }) => {
            let driven = match answer {
                Some(answer) => {
                    turns
                        .drive_answered(loop_, question, answer, turn_cancel, diagnostics)
                        .await
                }
                None => {
                    turns
                        .drive_presented(loop_, question, turn_cancel, diagnostics)
                        .await
                }
            };
            Some(driven)
        }
        None => match asked {
            Some(InteractiveTurnInput::Checkpoint) => Some(
                turns
                    .drive_checkpoint(loop_, turn_cancel, diagnostics)
                    .await,
            ),
            Some(InteractiveTurnInput::Prompt(asked)) => {
                let (termination, result) = turns
                    .drive_with_images(loop_, &asked, images, turn_cancel, diagnostics)
                    .await;
                Some((termination, result))
            }
            Some(InteractiveTurnInput::McpContext(command)) => Some(
                turns
                    .drive_mcp_context(loop_, command, turn_cancel, diagnostics)
                    .await,
            ),
            Some(InteractiveTurnInput::Skill(operation)) => {
                let (termination, result) = turns
                    .drive_admitted_skill(loop_, operation, turn_cancel, diagnostics)
                    .await;
                Some((termination, result))
            }
            None => match loop_.run_queued_turn().await {
                Ok(Some(outcome)) => {
                    let termination = loop_
                        .state()
                        .pending_question()
                        .map(|pending| RunTermination::QuestionRequired {
                            question: Box::new(QuestionReport::from_request(
                                pending.item_id(),
                                pending.target(),
                                pending.request(),
                            )),
                        })
                        .or_else(|| RunTermination::after_outcome(&outcome))
                        .unwrap_or(RunTermination::Done);
                    Some((
                        termination,
                        Value::String(last_assistant_message(&loop_.state())),
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
    }
}

struct EpochCapabilities<'a> {
    registry: &'a [RegisteredTool],
    system: Option<&'a SystemPrompt>,
    has_schema: bool,
    mcp_errors: &'a [String],
}

fn activate_epoch(
    epoch_start: &EpochStart,
    session_id: SessionId,
    seat: &dyn InteractiveSeat,
    sink: &dyn EventSink,
    state: &SessionState,
    capabilities_input: EpochCapabilities<'_>,
    diagnostics: &dyn Diagnostics,
) -> bool {
    let EpochCapabilities {
        registry,
        system,
        has_schema,
        mcp_errors,
    } = capabilities_input;
    match epoch_start {
        EpochStart::Initial => false,
        EpochStart::New => {
            sink.publish(
                &PublicEvent::SessionStarted {
                    v: PUBLIC_EVENT_VERSION,
                    session: session_id,
                    capabilities: capabilities(registry, system.is_some(), has_schema),
                    mcp_errors: mcp_errors.to_vec(),
                    hook_errors: Vec::new(),
                    instructions: system
                        .map(|prompt| prompt.sources.clone())
                        .unwrap_or_default(),
                },
                state,
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
                    session_id
                ));
            }
            sink.publish(
                &PublicEvent::SessionStarted {
                    v: PUBLIC_EVENT_VERSION,
                    session: session_id,
                    capabilities: capabilities(registry, system.is_some(), has_schema),
                    mcp_errors: mcp_errors.to_vec(),
                    hook_errors: Vec::new(),
                    instructions: system
                        .map(|prompt| prompt.sources.clone())
                        .unwrap_or_default(),
                },
                state,
            );
            if let Some(event) = projected {
                sink.publish(event.as_ref(), state);
            }
            !seat.activate_session(session_id)
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
                    session: session_id,
                    capabilities: capabilities(registry, system.is_some(), has_schema),
                    mcp_errors: mcp_errors.to_vec(),
                    hook_errors: Vec::new(),
                    instructions: system
                        .map(|prompt| prompt.sources.clone())
                        .unwrap_or_default(),
                },
                state,
            );
            !seat.activate_session(session_id)
        }
    }
}

fn handle_interactive_policy(
    request: PolicyControlRequest,
    policy_application: Option<&PolicyApplicationOwner>,
    status: Option<&InteractiveStatus>,
    text_output: Option<&dyn TextOutput>,
    diagnostics: &dyn Diagnostics,
) -> bool {
    let Some(owner) = policy_application else {
        let notice = "interactive policy controls are unavailable".to_owned();
        if let Some(status) = status
            && status.human
        {
            status.seat.set_notice(notice);
        } else {
            diagnostics.note(&notice);
        }
        return false;
    };
    if let PolicyControlRequest::SetMode(mode) = &request {
        let mode = *mode;
        let notice = match owner.apply(PolicyControlRequest::SetMode(mode)) {
            Ok(PolicyControlOutcome::Mode { staging }) => {
                if staging.changed {
                    format!("mode `{mode}` staged for the next generation")
                } else {
                    format!("mode `{mode}` is already selected")
                }
            }
            Ok(_) => "policy control failed: the outcome did not match the mode request".to_owned(),
            Err(error) => {
                format!("policy control failed: {error}")
            }
        };
        if let Some(status) = status
            && status.human
        {
            status.seat.set_notice(notice);
        } else {
            diagnostics.note(&notice);
        }
        return false;
    }
    let Some(output) = text_output else {
        let notice = "interactive permission output is unavailable".to_owned();
        if let Some(status) = status
            && status.human
        {
            status.seat.set_notice(notice);
        } else {
            diagnostics.note(&notice);
        }
        return false;
    };
    let handled = handle_policy_request(
        request,
        |request| owner.apply(request),
        output,
        |_preview| {
            status
                .as_ref()
                .filter(|status| status.human)
                .map_or(repl::Confirmation::Declined, |status| {
                    status.seat.confirm_remember()
                })
        },
    );
    match handled {
        Ok(Some(repl::Line::Ended | repl::Line::ExternalBreak(_))) => return true,
        Ok(Some(_)) => diagnostics.note("permission confirmation returned a non-terminal line"),
        Ok(None) => {}
        Err(error) => {
            let notice = format!("policy control failed: {error}");
            if let Some(status) = status
                && status.human
            {
                status.seat.set_notice(notice);
            } else {
                diagnostics.note(&notice);
            }
        }
    }

    false
}

fn render_skill_catalogue(
    catalogue: Option<&crate::skill::SkillCatalogue>,
    diagnose: bool,
    diagnostics: &dyn Diagnostics,
) {
    let Some(catalogue) = catalogue else {
        diagnostics.note("the skill catalogue is unavailable");
        return;
    };
    match catalogue.capture() {
        Ok(capture) => diagnostics.note(&if diagnose {
            capture.render_diagnostics()
        } else {
            capture.render_list()
        }),
        Err(error) => diagnostics.note(&format!(
            "the skill catalogue could not be captured: {error}"
        )),
    }
}

fn render_session_hooks(hook_state: &SessionHookState<'_>, diagnostics: &dyn Diagnostics) {
    match hook_state.catalogue_snapshot() {
        Ok(catalogue) => match hooks::render(&catalogue, false) {
            Ok(rendered) if rendered.is_empty() => diagnostics.note(&crate::output::empty_listing(
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
}

async fn switch_session_model(
    request: ModelSwitchRequest,
    model_switch_owner: Option<RunModelSwitchOwner<'_>>,
    seat: &dyn InteractiveSeat,
    status: Option<&InteractiveStatus>,
    selection: &model::GenerationSelection,
    state: &SessionState,
    diagnostics: &dyn Diagnostics,
) {
    let Some(owner) = model_switch_owner else {
        diagnostics.note("the active session has no model-switch lifecycle owner");
        return;
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
    if let Some(UserConfigPublication::PublishedDurabilityUncertain { diagnostic }) = publication {
        diagnostics.note(&format!(
            "model defaults are authoritative with a durability warning: {diagnostic}"
        ));
    }
    match applied {
        Ok(_) => diagnostics.note(&receipt),
        Err(error) => diagnostics.note(&format!("model switch failed: {error}")),
    }
    if let Some(status) = status
        && status.human
    {
        match selection.snapshot() {
            Ok(selected) => status.seat.refresh_status(state, selected),
            Err(error) => diagnostics.note(&format!(
                "the model-switch status selection could not be refreshed: {error}"
            )),
        }
    }
}

async fn report_session_usage(
    store: &JsonlStore,
    root: &Path,
    session_id: SessionId,
    clock: &SystemClock,
    context: ego_core::state::ContextWindow,
    diagnostics: &dyn Diagnostics,
) {
    let metadata = match crate::catalogue::listing(store, root, false) {
        Ok(metadata) => metadata,
        Err(error) => {
            diagnostics.note(&format!("the session catalogue could not be read: {error}"));
            return;
        }
    };
    let Some(meta) = metadata.into_iter().find(|meta| meta.id == session_id) else {
        diagnostics.note("the active session is absent from its durable catalogue");
        return;
    };
    match UsageReportService::new(store, clock)
        .report(std::slice::from_ref(&meta), UsagePeriod::All)
        .await
    {
        Ok(report) => {
            // `/usage` has no flags, so it
            // gets the compact default.
            let rendered = crate::usage::render_text(&report, false, Some(context));
            diagnostics.note(rendered.trim_end());
        }
        Err(error) => diagnostics.note(&format!("the usage report could not be built: {error}")),
    }
}

struct InteractiveTurnBuild<'a, 'session> {
    args: &'a Args,
    effective: &'a Effective,
    source: &'a Arc<model::CliGenerationModelSource>,
    policy_source: &'a Arc<dyn GenerationPolicySource>,
    gate: &'a SharedBudgetGate,
    config: &'a TurnConfig,
    evidence: &'a Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    run_deadline: Option<std::time::Instant>,
    registry: &'a [RegisteredTool],
    admitted_tools: &'a Option<Arc<dyn AdmittedToolSource>>,
    cwd: &'a Path,
    mcp_runtime: &'a dyn McpRuntimeBinding,
    skill_resource_roots: &'a Option<ego_core::skill::SkillResourceRoots>,
    elicitor: &'a Option<Arc<dyn Elicitor>>,
    judge_available: bool,
    confinement: &'a RunConfinement,
    skill_trust: Trust,
    active: &'session ActiveSession<'session>,
    hooks: &'a SessionHookComposition<'session>,
    journal: &'a SharedJournal<'session>,
    skill_owner: Arc<dyn SkillInvocationPort + 'session>,
}

fn build_interactive_turn<'session: 'turn, 'turn>(
    input: InteractiveTurnBuild<'turn, 'session>,
    effects: (
        &'turn Arc<RunEffectPorts>,
        &'turn CancelToken,
        ExecObservation,
        Option<ego_core::store::evidence::EvidenceRef>,
    ),
) -> Result<TurnLoop<'session, 'turn>, RunTermination> {
    let InteractiveTurnBuild {
        args,
        effective,
        source,
        policy_source,
        gate,
        config,
        evidence,
        invocation,
        run_deadline,
        registry,
        admitted_tools,
        cwd,
        mcp_runtime,
        skill_resource_roots,
        elicitor,
        judge_available,
        confinement,
        skill_trust,
        active,
        hooks,
        journal,
        skill_owner,
    } = input;
    let (ports, cancel, observation, probe_evidence) = effects;
    let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(ports))) as Arc<dyn ego_ai::http::Http>;
    let runtime = GenerationRuntime {
        source: source.bind(http, cancel.clone(), run_deadline),
        policy_source: Arc::clone(policy_source),
        gate: gate.clone(),
        config: config.clone(),
        admission: Arc::clone(evidence),
        invocation,
        retry_pause: Arc::new(TokioRetryPause),
    };
    let effect_ports = ToolEffectPorts::new(
        &OsFs,
        ports.process(),
        active.evidence(),
        ports.resolver_handle(),
        ports.one_hop_handle(),
        ports.duplex_handle(),
    )
    .with_secret_values(ego_tools::secrets::from_env())
    .with_cancel(cancel.clone());
    let tools = match admitted_tools.as_ref() {
        Some(source) => Tools::from_registry_with_source(
            registry,
            Arc::clone(source),
            effect_ports,
            CanonPath::new(cwd),
        )
        .map_err(|error| RunTermination::Failed {
            message: format!("the admitted MCP tool catalogue is invalid: {error}"),
        })?,
        None => Tools::from_registry(registry, effect_ports, CanonPath::new(cwd)),
    };
    let tools = match McpRuntimeBinding::resume_sealer(mcp_runtime) {
        Some(resume) => tools.with_resume(resume),
        None => tools,
    };
    let tools = match skill_resource_roots.as_ref() {
        Some(roots) => tools.with_skill_resource_roots(roots.clone()),
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
    .with_context_policy(args.context_clear, args.context_interval)
    .with_goal(args.goal.clone(), args.goal_turns)
    .with_goal_policy(effective.ceilings.clone(), args.goal_check_command.clone())
    .with_hook_binding(
        hooks.state.clone(),
        Arc::clone(&hooks.binding),
        hooks.mcp.as_ref().map(Arc::clone),
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;

    let loop_ = loop_
        .with_provider_recovery(
            args.provider_recovery
                .then_some(ego_core::turn::PROVIDER_RECOVERY_HORIZON),
        )
        .with_skill_invocation_port(skill_owner);
    let loop_ = match active.replayed() {
        Some(replayed) => loop_.with_replayed_context(replayed),
        None => loop_,
    };
    let loop_ = match elicitor.as_ref() {
        Some(elicitor) => loop_.with_elicitor(Arc::clone(elicitor)),
        None => loop_,
    };
    let loop_ = if judge_available {
        loop_.with_judge(JudgeContext {
            confinement: confinement.clone(),
            exec_caps: observation.capabilities(),
        })
    } else {
        loop_
    };
    Ok(
        super::composition::with_permission_analysis(loop_, skill_trust)
            .with_execution_observation(observation, probe_evidence),
    )
}

struct InteractiveCompaction<'a, 'session> {
    args: &'a Args,
    effective: &'a Effective,
    root: &'a Path,
    cwd: &'a Path,
    diagnostics: &'a dyn Diagnostics,
    skills: &'a Option<Arc<crate::skill::SkillCatalogue>>,
    jsonl_store: &'a Arc<JsonlStore>,
    confinement: &'a RunConfinement,
    active: &'session ActiveSession<'session>,
    cancel: &'a CancelToken,
    run_deadline: Option<std::time::Instant>,
    source: &'a Arc<model::CliGenerationModelSource>,
    policy_source: &'a Arc<dyn GenerationPolicySource>,
    gate: &'a SharedBudgetGate,
    config: &'a TurnConfig,
    evidence: &'a Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    admitted_tools: &'a Option<Arc<dyn AdmittedToolSource>>,
    mcp_runtime: &'a dyn McpRuntimeBinding,
    skill_resource_roots: &'a Option<ego_core::skill::SkillResourceRoots>,
    session_journal: &'a SharedJournal<'session>,
    session_hooks: &'a SessionHookComposition<'session>,
    last_generation_request: Option<GenRequest>,
    elicitor: &'a Option<Arc<dyn Elicitor>>,
    judge_available: bool,
    skill_trust: Trust,
    control: &'a mut InteractiveControl,
    state: &'a mut SessionState,
    seat: &'a dyn InteractiveSeat,
    model_compaction: bool,
    prepared_runtime: &'a Mutex<Option<CompactionRuntime>>,
}

async fn prepare_interactive_compaction<'session: 'turn, 'turn>(
    input: InteractiveCompaction<'turn, 'session>,
) -> Result<PreparedCompaction, CompactionError> {
    let InteractiveCompaction {
        args,
        effective,
        root,
        cwd,
        diagnostics,
        skills,
        jsonl_store,
        confinement,
        active,
        cancel: compaction_cancel,
        run_deadline,
        source,
        policy_source,
        gate,
        config,
        evidence,
        invocation,
        admitted_tools,
        mcp_runtime,
        skill_resource_roots,
        session_journal,
        session_hooks,
        last_generation_request,
        elicitor,
        judge_available,
        skill_trust,
        control,
        state,
        seat,
        model_compaction,
        prepared_runtime,
    } = input;
    let fs = OsFs;

    let target_system = instructions(effective, args, root, cwd, diagnostics)
        .map_err(|error| CompactionError::Internal(error.to_string()))?;
    let prepared_skills = skills
        .as_ref()
        .map(|catalogue| catalogue.prepare_rescan())
        .transpose()
        .map_err(|error| CompactionError::Internal(error.to_string()))?;
    let target_skills = prepared_skills
        .as_ref()
        .map(crate::skill::PreparedSkillRescan::capture);
    let mut target_registry = super::composition::tools_with_mode(
        args.tools.as_ref(),
        target_skills
            .as_ref()
            .map(crate::skill::SkillCatalogueCapture::core),
        effective
            .active_profile
            .as_ref()
            .map(|profile| profile.name.as_str()),
        Some(Arc::new(jsonl_store.history())),
        effective
            .exec
            .policy()
            .map_err(|error| CompactionError::Internal(error.to_string()))?,
        super::composition::orchestration_mode(effective),
    )
    .map_err(|error| CompactionError::Internal(error.to_string()))?;
    if !args.no_context_tool && args.tools.is_none() {
        target_registry.push(RegisteredTool::Context);
    }
    let target_tools: Vec<ToolDescriptor> = target_registry
        .iter()
        .map(RegisteredTool::descriptor)
        .collect();
    let (ports, observation, probe_evidence) = effect_scope(
        confinement,
        active.evidence(),
        compaction_cancel,
        run_deadline,
        None,
    )
    .await
    .map_err(compaction_termination)?;
    let http =
        Arc::new(ProviderHttpAdapter::new(Arc::clone(&ports))) as Arc<dyn ego_ai::http::Http>;
    let runtime = GenerationRuntime {
        source: source.bind(http, compaction_cancel.clone(), run_deadline),
        policy_source: Arc::clone(policy_source),
        gate: gate.clone(),
        config: config.clone(),
        admission: Arc::clone(evidence),
        invocation,
        retry_pause: Arc::new(TokioRetryPause),
    };
    let effect_ports = ToolEffectPorts::new(
        &fs,
        ports.process(),
        active.evidence(),
        ports.resolver_handle(),
        ports.one_hop_handle(),
        ports.duplex_handle(),
    )
    .with_secret_values(ego_tools::secrets::from_env())
    .with_cancel(compaction_cancel.clone());
    let compaction_tools = match admitted_tools.as_ref() {
        Some(source) => Tools::from_registry_with_source(
            &target_registry,
            Arc::clone(source),
            effect_ports,
            CanonPath::new(cwd),
        )
        .map_err(|error| CompactionError::Internal(error.to_string()))?,
        None => Tools::from_registry(&target_registry, effect_ports, CanonPath::new(cwd)),
    };
    let compaction_tools = match McpRuntimeBinding::resume_sealer(mcp_runtime) {
        Some(resume) => compaction_tools.with_resume(resume),
        None => compaction_tools,
    };
    let compaction_tools = match skill_resource_roots.as_ref() {
        Some(roots) => compaction_tools.with_skill_resource_roots(roots.clone()),
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
    .map_err(|error| CompactionError::Internal(error.to_string()))?
    .with_context_policy(args.context_clear, args.context_interval)
    .with_last_generation_request(last_generation_request.clone())
    .with_hook_binding(
        session_hooks.state.clone(),
        Arc::clone(&session_hooks.binding),
        session_hooks.mcp.as_ref().map(Arc::clone),
    )
    .map_err(|error| CompactionError::Internal(error.to_string()))?;
    if let Some(replayed) = active.replayed() {
        compaction = compaction.with_replayed_context(replayed);
    }
    if let Some(elicitor) = elicitor.as_ref() {
        compaction = compaction.with_elicitor(Arc::clone(elicitor));
    }
    if judge_available {
        compaction = compaction.with_judge(JudgeContext {
            confinement: confinement.clone(),
            exec_caps: observation.capabilities(),
        });
    }
    let mut compaction = super::composition::with_permission_analysis(compaction, skill_trust)
        .with_execution_observation(observation, probe_evidence)
        .with_interactive_control(control);
    let prepared = compaction
        .prepare_compaction(target_system.clone(), target_tools)
        .await;
    drop(compaction);
    *state = session_hooks.state.state();
    seat.refresh_queue(control.status());
    if !state.unacknowledged_prompts().is_empty()
        || state.pending_question().is_some()
        || !state.live_attempts.is_empty()
        || (model_compaction && state.detached_execs.values().any(Option::is_none))
    {
        return Err(CompactionError::Internal(
            "compaction stopped before target creation because new source work became pending"
                .to_owned(),
        ));
    }
    let prepared = prepared?;
    *prepared_runtime
        .lock()
        .expect("interactive compaction runtime") =
        Some((target_system, target_registry, prepared_skills));
    Ok(prepared)
}

fn publish_compaction_runtime(
    runtime: Option<CompactionRuntime>,
    skills: Option<&crate::skill::SkillCatalogue>,
    registry: &mut Vec<RegisteredTool>,
    config: &mut TurnConfig,
    diagnostics: &dyn Diagnostics,
) -> Result<(), RunTermination> {
    if let Some((target_system, target_registry, prepared_skills)) = runtime {
        if let Some(prepared) = prepared_skills {
            let published = prepared.published();
            let Some(catalogue) = skills else {
                return Err(RunTermination::Failed {
                    message: "the prepared skill catalogue lost its publication owner".to_owned(),
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
        *registry = target_registry;
        config.system = target_system;
    }
    Ok(())
}

fn activate_fork_transition<'store>(
    active: &mut ActiveSession<'store>,
    source_state: SessionState,
    session_end: Result<(), RunTermination>,
    target: (
        ActiveSession<'store>,
        ego_core::session::TargetPublication,
        Option<String>,
        Option<PublicEvent>,
    ),
    context: (
        &EvidenceAdmissionService,
        &Option<RunContextEvidence>,
        &dyn Diagnostics,
    ),
) -> Result<EpochStart, RunTermination> {
    let (target, publication, diagnostic, projected) = target;
    let (evidence, run_context, diagnostics) = context;
    let parent = active.id();
    active.replace_state(source_state);
    if !active.apply_publication(target, &publication) {
        return Err(RunTermination::Failed {
            message: format!(
                "the published fork target could not be activated: {}",
                diagnostic.as_deref().unwrap_or("publication was refused")
            ),
        });
    }
    admit_successor_context(evidence, active, run_context)?;
    if let Err(error) = session_end {
        diagnostics.note(&format!(
            "the fork source session-end hook failed: {}",
            error.exit_reason()
        ));
    }
    diagnostics.note(&format!(
        "forked session {parent} into {}; this is now session {}. \
                             To return to the source: ego resume {parent} ({})",
        active.id(),
        active.id(),
        crate::usage::context_line(active.state().context)
    ));
    Ok(EpochStart::Fork {
        diagnostic,
        projected: projected.map(Box::new),
    })
}

fn rescan_session_skills(
    pending_work: bool,
    skills: Option<&crate::skill::SkillCatalogue>,
    seat: &dyn InteractiveSeat,
    inputs: (
        &Args,
        &Effective,
        &Arc<JsonlStore>,
        ego_core::permit::EnvPolicy,
    ),
    diagnostics: &dyn Diagnostics,
) -> Result<Option<Vec<RegisteredTool>>, RunTermination> {
    let (args, effective, jsonl_store, exec_environment) = inputs;

    if pending_work {
        diagnostics.note("skills cannot be rescanned while source work is pending");
        return Ok(None);
    }
    let Some(catalogue) = skills else {
        diagnostics.note("the skill catalogue is unavailable");
        return Ok(None);
    };
    seat.busy();
    let prepared = match catalogue.prepare_rescan() {
        Ok(prepared) => prepared,
        Err(error) => {
            diagnostics.note(&format!(
                "the skill catalogue could not be rescanned: {error}"
            ));
            return Ok(None);
        }
    };
    let published = prepared.published();
    let prospective = prepared.capture();
    debug_assert_eq!(prospective.retained_last_good(), !published);
    let next_registry = match super::composition::tools_with_mode(
        args.tools.as_ref(),
        Some(prospective.core()),
        effective
            .active_profile
            .as_ref()
            .map(|profile| profile.name.as_str()),
        Some(Arc::new(jsonl_store.history())),
        exec_environment,
        super::composition::orchestration_mode(effective),
    ) {
        Ok(registry) => registry,
        Err(error) => {
            return Err(RunTermination::Failed {
                message: format!(
                    "the rescanned skill catalogue could not rebuild model tools: {error}"
                ),
            });
        }
    };
    let capture = match catalogue.commit_rescan(prepared) {
        Ok(capture) => capture,
        Err(error) => {
            return Err(RunTermination::Failed {
                message: format!("the prepared skill catalogue could not be published: {error}"),
            });
        }
    };
    let mut registry = next_registry;
    if !args.no_context_tool && args.tools.is_none() {
        registry.push(RegisteredTool::Context);
    }
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
    Ok(Some(registry))
}

enum CommandOutcome<'store> {
    Continue,
    Input(InteractiveTurnInput),
    Transition(Box<EpochTransition<'store>>),
}

struct InteractiveCommand<'a, 'session, 'store> {
    args: &'a Args,
    active: &'session ActiveSession<'store>,
    seat: &'a Arc<dyn InteractiveSeat>,
    source: &'a Arc<model::CliGenerationModelSource>,
    confinement: &'a RunConfinement,
    run_deadline: Option<std::time::Instant>,
    staged_images: &'a mut Vec<StagedPasteImage>,
    last: &'a mut Ended,
    state: &'a mut SessionState,
    diagnostics: &'a dyn Diagnostics,
    interactive_metadata: &'a SessionMetadata<JsonlStore>,
    leased_session_journal:
        &'a LeasedSharedJournal<'session, ego_tools::store::SessionWriter<'store>>,
    session_journal: &'a SharedJournal<'session>,
    interactive_metadata_ceilings: &'a Ceilings,
    status: &'a Option<InteractiveStatus>,
    selection: &'a Arc<model::GenerationSelection>,
    skills: &'a Option<Arc<crate::skill::SkillCatalogue>>,
    effective: &'a Effective,
    jsonl_store: &'store Arc<JsonlStore>,
    exec_environment: ego_core::permit::EnvPolicy,
    registry: &'a mut Vec<RegisteredTool>,
    session_hooks: &'a SessionHookComposition<'session>,
    model_switch_owner: Option<RunModelSwitchOwner<'session>>,
    policy_application: &'a Option<Arc<PolicyApplicationOwner>>,
    text_output: Option<&'a dyn TextOutput>,
    session_hook_ports: &'a Arc<RunEffectPorts>,
    skill_profile_layers: &'a Option<Vec<Sourced<ConfigLayer>>>,
    perimeter: &'a crate::perimeter::Perimeter,
    mcp_registry: &'a Arc<McpRegistryController>,
    session_mcp: &'a mut Vec<(McpServerName, McpServerConfig, McpConfigProvenance)>,
    disabled_mcp: &'a mut BTreeSet<McpServerName>,
    root: &'a Path,
    cwd: &'a Path,
    clock: &'a SystemClock,
    config: &'a mut TurnConfig,
    refresh_prompt: &'a mut bool,
    automatic_compaction_failed: &'a mut bool,
    policy_source: &'a Arc<dyn GenerationPolicySource>,
    gate: &'a SharedBudgetGate,
    evidence: &'a Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    admitted_tools: &'a Option<Arc<dyn AdmittedToolSource>>,
    mcp_runtime: &'a Arc<dyn McpRuntimeBinding>,
    skill_resource_roots: &'a Option<ego_core::skill::SkillResourceRoots>,
    last_generation_request: &'a Option<GenRequest>,
    elicitor: &'a Option<Arc<dyn Elicitor>>,
    judge_available: bool,
    skill_trust: Trust,
    control: &'a mut InteractiveControl,
    checkpoint_pending: &'a mut bool,
}

async fn dispatch_interactive_command<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
    posture: (bool, bool, bool),
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        args,
        active,
        seat,
        source,
        confinement,
        run_deadline,
        staged_images,
        last,
        state,
        diagnostics,
        interactive_metadata,
        leased_session_journal,
        session_journal,
        interactive_metadata_ceilings,
        status,
        selection,
        skills,
        effective,
        jsonl_store,
        exec_environment,
        registry,
        session_hooks,
        model_switch_owner,
        policy_application,
        text_output,
        session_hook_ports,
        skill_profile_layers,
        perimeter,
        mcp_registry,
        session_mcp,
        disabled_mcp,
        root,
        cwd,
        clock,
        config,
        refresh_prompt,
        automatic_compaction_failed,
        policy_source,
        gate,
        evidence,
        invocation,
        admitted_tools,
        mcp_runtime,
        skill_resource_roots,
        last_generation_request,
        elicitor,
        judge_available,
        skill_trust,
        control,
        checkpoint_pending,
    } = input;
    let (pending_work, _, _) = posture;
    match line {
        repl::Line::HostCommand(repl::HostCommand::RemovePasteImage) => {
            if staged_images.pop().is_some() {
                seat.set_image_chips(paste_chips(staged_images));
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::ClipboardPaste) => {
            stage_clipboard_image(
                seat.as_ref(),
                pending_work,
                source,
                (confinement, active.evidence(), run_deadline),
                staged_images,
            )
            .await;
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::PastePath(path)) => {
            stage_image_path(
                seat.as_ref(),
                pending_work,
                path,
                source,
                (confinement, active.evidence(), run_deadline),
                staged_images,
            )
            .await;
            Ok(CommandOutcome::Continue)
        }
        repl::Line::Submitted(text) => {
            Ok(CommandOutcome::Input(InteractiveTurnInput::Prompt(text)))
        }
        repl::Line::Blank => Ok(CommandOutcome::Continue),
        // Ctrl+C at an idle prompt drops the line and nothing
        // else. Someone who pressed it to stop a turn must not
        // lose the session by pressing it once more.
        repl::Line::Interrupted => Ok(CommandOutcome::Continue),
        repl::Line::Ended => {
            // The last turn's provider/model refusal was already shown.
            // Preserve cancellation, permission, and other outcomes.
            if matches!(
                &last.termination,
                RunTermination::Provider { .. }
                    | RunTermination::ProviderUncertain { .. }
                    | RunTermination::UsageError { .. }
            ) {
                last.termination = RunTermination::Done;
            }
            Ok(CommandOutcome::Transition(Box::new(EpochTransition::Exit)))
        }
        repl::Line::Refused(why) => {
            diagnostics.note(why);
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::SessionMetadata(operation)) => {
            update_session_metadata(
                SessionMetadataCommand {
                    metadata: interactive_metadata,
                    leased: leased_session_journal,
                    journal: session_journal,
                    ceilings: interactive_metadata_ceilings,
                    state,
                    last,
                    status,
                    selection,
                    diagnostics,
                },
                operation,
                pending_work,
            )
        }
        repl::Line::HostCommand(repl::HostCommand::McpContext(command)) => {
            if pending_work {
                diagnostics
                    .note("MCP context commands are unavailable while source work is pending");
                return Ok(CommandOutcome::Continue);
            }
            Ok(CommandOutcome::Input(InteractiveTurnInput::McpContext(
                command,
            )))
        }
        repl::Line::HostCommand(repl::HostCommand::Skill(operation)) => {
            if pending_work {
                diagnostics.note("a skill cannot be invoked while source work is pending");
                return Ok(CommandOutcome::Continue);
            }
            Ok(CommandOutcome::Input(InteractiveTurnInput::Skill(
                operation,
            )))
        }
        repl::Line::HostCommand(repl::HostCommand::Skills(
            repl::actions::SkillSurfaceAction::List,
        )) => {
            render_skill_catalogue(skills.as_deref(), false, diagnostics);
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Skills(
            repl::actions::SkillSurfaceAction::Diagnose,
        )) => {
            render_skill_catalogue(skills.as_deref(), true, diagnostics);
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Skills(
            repl::actions::SkillSurfaceAction::Rescan,
        )) => {
            match rescan_session_skills(
                pending_work,
                skills.as_deref(),
                seat.as_ref(),
                (args, effective, jsonl_store, exec_environment.clone()),
                diagnostics,
            ) {
                Ok(Some(next_registry)) => *registry = next_registry,
                Ok(None) => {}
                Err(termination) => {
                    last.termination = termination;
                    return Ok(CommandOutcome::Transition(Box::new(EpochTransition::Exit)));
                }
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Hooks) => {
            render_session_hooks(&session_hooks.state, diagnostics);
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::ModelSwitch(request)) => {
            switch_session_model(
                request,
                model_switch_owner.clone(),
                seat.as_ref(),
                status.as_ref(),
                selection,
                state,
                diagnostics,
            )
            .await;
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Policy(request)) => {
            if handle_interactive_policy(
                request,
                policy_application.as_deref(),
                status.as_ref(),
                text_output,
                diagnostics,
            ) {
                return Ok(CommandOutcome::Transition(Box::new(EpochTransition::Exit)));
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Tasks(arguments)) => {
            diagnostics.note(&background_tasks(
                &arguments,
                state,
                session_hook_ports.process(),
            ));
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Help)) => {
            diagnostics.note(&seat.action_help());
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Expand)) => {
            diagnostics.expand_last_tool();
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Config)) => {
            diagnostics.note(&crate::config::repl_text(
                effective,
                skill_profile_layers.as_deref().unwrap_or_default(),
                perimeter,
            ));
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Mcp(arguments)) => {
            {
                let controller = mcp_registry;
                let result =
                    crate::mcp::session_command(&arguments, controller, session_mcp, disabled_mcp)
                        .await;
                match result {
                    Ok(receipt) => diagnostics.note(&receipt),
                    Err(error) => diagnostics.note(&format!("MCP control failed: {error:?}")),
                }
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Config(arguments)) => {
            run_interactive_config(
                &arguments,
                effective,
                skill_profile_layers.as_deref(),
                perimeter,
                diagnostics,
            )
            .await;
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Setup(prompt)) => {
            run_interactive_setup(prompt, root, status.as_ref(), diagnostics).await;
            Ok(CommandOutcome::Continue)
        }
        // The same snapshot `ego doctor` prints,
        // built by the same probes. Two renderings
        // of one value is the whole contract; two
        // implementations would be two diagnoses.
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Doctor)) => {
            diagnostics.note(&crate::doctor::observed_session(
                root.to_path_buf(),
                seat.width(),
                perimeter,
            ));
            Ok(CommandOutcome::Continue)
        }
        // To the transcript rather than to a file
        // this action would have had to invent a
        // name for. `ego export ID --file` is where
        // a file is asked for by name; the two
        // disclosure modes and the three formats
        // are the same ones, carried by the slash
        // grammar — `/export local json`.
        repl::Line::HostCommand(repl::HostCommand::Export(selection)) => {
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
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(
            command @ (repl::HostCommand::Action(repl::actions::ActionId::Sessions)
            | repl::HostCommand::History(_)
            | repl::HostCommand::Resume(_)),
        ) => {
            if let Some(transition) = select_session_transition(
                command,
                seat.as_ref(),
                pending_work,
                jsonl_store,
                root,
                active.id(),
                (clock, diagnostics),
            )
            .await
            {
                return Ok(CommandOutcome::Transition(Box::new(transition)));
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Usage)) => {
            report_session_usage(
                jsonl_store.as_ref(),
                root,
                active.id(),
                clock,
                state.context,
                diagnostics,
            )
            .await;
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Refresh)) => {
            if pending_work {
                diagnostics.note("instructions cannot be refreshed while source work is pending");
                return Ok(CommandOutcome::Continue);
            }
            match instructions(effective, args, root, cwd, diagnostics) {
                Ok(system) => {
                    config.system = system;
                    *refresh_prompt = true;
                    diagnostics.note(
                        "instructions refreshed; the next generation opens a new prompt epoch",
                    );
                }
                Err(error) => {
                    diagnostics.note(&format!("instructions were not refreshed: {error}"))
                }
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::New)) => {
            if let Some(transition) =
                new_session_transition(seat.as_ref(), pending_work, jsonl_store, clock, diagnostics)
                    .await
            {
                return Ok(CommandOutcome::Transition(Box::new(transition)));
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(
            command @ (repl::HostCommand::Action(repl::actions::ActionId::Fork)
            | repl::HostCommand::Fork(_)),
        ) => {
            if let Some(transition) = fork_session_transition(
                command,
                pending_work,
                jsonl_store,
                active.id(),
                state,
                clock,
                diagnostics,
            )
            .await
            {
                return Ok(CommandOutcome::Transition(Box::new(transition)));
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Compact)) => {
            compact_session_command(
                InteractiveCommand {
                    args,
                    active,
                    seat,
                    source,
                    confinement,
                    run_deadline,
                    staged_images,
                    last,
                    state,
                    diagnostics,
                    interactive_metadata,
                    leased_session_journal,
                    session_journal,
                    interactive_metadata_ceilings,
                    status,
                    selection,
                    skills,
                    effective,
                    jsonl_store,
                    exec_environment,
                    registry,
                    session_hooks,
                    model_switch_owner,
                    policy_application,
                    text_output,
                    session_hook_ports,
                    skill_profile_layers,
                    perimeter,
                    mcp_registry,
                    session_mcp,
                    disabled_mcp,
                    root,
                    cwd,
                    clock,
                    config,
                    refresh_prompt,
                    automatic_compaction_failed,
                    policy_source,
                    gate,
                    evidence,
                    invocation,
                    admitted_tools,
                    mcp_runtime,
                    skill_resource_roots,
                    last_generation_request,
                    elicitor,
                    judge_available,
                    skill_trust,
                    control,
                    checkpoint_pending,
                },
                posture,
            )
            .await
        }
        repl::Line::HostCommand(_) => Ok(CommandOutcome::Continue),
        repl::Line::ExternalBreak(_) => {
            Ok(CommandOutcome::Transition(Box::new(EpochTransition::Exit)))
        }
    }
}

struct SessionMetadataCommand<'a, 'session, 'store> {
    metadata: &'a SessionMetadata<JsonlStore>,
    leased: &'a LeasedSharedJournal<'session, ego_tools::store::SessionWriter<'store>>,
    journal: &'a SharedJournal<'session>,
    ceilings: &'a Ceilings,
    state: &'a mut SessionState,
    last: &'a mut Ended,
    status: &'a Option<InteractiveStatus>,
    selection: &'a model::GenerationSelection,
    diagnostics: &'a dyn Diagnostics,
}

fn update_session_metadata<'store>(
    input: SessionMetadataCommand<'_, '_, '_>,
    operation: crate::cli::SessionMetadataOperation,
    pending_work: bool,
) -> Result<CommandOutcome<'store>, RunTermination> {
    let SessionMetadataCommand {
        metadata: interactive_metadata,
        leased: leased_session_journal,
        journal: session_journal,
        ceilings: interactive_metadata_ceilings,
        state,
        last,
        status,
        selection,
        diagnostics,
    } = input;

    if pending_work {
        diagnostics.note("session metadata is unavailable while source work is pending");
        return Ok(CommandOutcome::Continue);
    }
    if let Err(error) = crate::metadata::execute_interactive(
        interactive_metadata,
        leased_session_journal,
        session_journal,
        &operation,
        interactive_metadata_ceilings,
        &mut |message| diagnostics.note(message),
    ) {
        diagnostics.note(&format!("session metadata operation failed: {error}"));
    }
    *state = match session_journal.inspect(Clone::clone) {
        Ok(state) => state,
        Err(error) => {
            last.termination = hook_journal_termination(error);
            return Ok(CommandOutcome::Transition(Box::new(EpochTransition::Exit)));
        }
    };
    last.state = state.clone();
    if let Some(status) = status.as_ref()
        && status.human
    {
        match selection.snapshot() {
            Ok(selected) => status.seat.refresh_status(state, selected),
            Err(error) => diagnostics.note(&format!(
                "the metadata status selection could not be refreshed: {error}"
            )),
        }
    }
    Ok(CommandOutcome::Continue)
}

async fn compact_session_command<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    posture: (bool, bool, bool),
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        args,
        active,
        seat,
        source,
        confinement,
        run_deadline,
        state,
        diagnostics,
        session_journal,
        status,
        selection,
        skills,
        effective,
        jsonl_store,
        session_hooks,
        root,
        cwd,
        clock,
        config,
        automatic_compaction_failed,
        policy_source,
        gate,
        evidence,
        invocation,
        admitted_tools,
        mcp_runtime,
        skill_resource_roots,
        last_generation_request,
        elicitor,
        judge_available,
        skill_trust,
        control,
        checkpoint_pending,
        ..
    } = input;
    let (pending_work, model_compaction, automatic_compaction) = posture;

    if compaction_availability(pending_work) != repl::actions::Availability::Enabled {
        return Ok(CommandOutcome::Continue);
    }
    if !automatic_compaction && !seat.confirm_compaction(active.id()) {
        return Ok(CommandOutcome::Continue);
    }
    // One automatic attempt per source epoch; failure remains visible.
    *automatic_compaction_failed = automatic_compaction;
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
            return Ok(CommandOutcome::Continue);
        }
    };
    let compaction_cancel = CancelToken::new();
    let future: CompactionFuture<'_, Result<PreparedCompaction, CompactionError>> =
        Box::pin(prepare_interactive_compaction(InteractiveCompaction {
            args,
            effective,
            root,
            cwd,
            diagnostics,
            skills,
            jsonl_store,
            confinement,
            active,
            cancel: &compaction_cancel,
            run_deadline,
            source,
            policy_source,
            gate,
            config,
            evidence,
            invocation,
            admitted_tools,
            mcp_runtime: mcp_runtime.as_ref(),
            skill_resource_roots,
            session_journal,
            session_hooks,
            last_generation_request: last_generation_request.clone(),
            elicitor,
            judge_available,
            skill_trust,
            control,
            state,
            seat: seat.as_ref(),
            model_compaction,
            prepared_runtime: prepared_runtime.as_ref(),
        }));
    let mut preparation = DeferredCompactionPreparation {
        source: source_id,
        future: Some(future),
    };
    let request = CompactionTransitionRequest {
        request_id,
        source: source_id,
    };
    let receipt = CompactionTransitionService::new(*clock)
        .compact(request, &mut preparation, jsonl_store.as_ref())
        .await;
    drop(preparation);

    let receipt = match receipt {
        Ok(receipt) => receipt,
        Err(error) => {
            diagnostics.note(&format!("compaction failed: {error}"));
            if model_compaction {
                session_journal
                    .append(TurnId::new(), None, None, EventKind::ContextRequestConsumed)
                    .map_err(|error| RunTermination::Failed {
                        message: error.to_string(),
                    })?;
                *state = session_hooks.state.state();
            }
            if let Some(status) = status.as_ref()
                && status.human
                && let Ok(selected) = selection.snapshot()
            {
                status.seat.refresh_status(state, selected);
            }
            return Ok(CommandOutcome::Continue);
        }
    };
    if model_compaction {
        session_journal
            .append(TurnId::new(), None, None, EventKind::ContextRequestConsumed)
            .map_err(|error| RunTermination::Failed {
                message: error.to_string(),
            })?;
        *state = session_hooks.state.state();
    }
    let target = receipt.target;
    let target_opened = match session::open(jsonl_store.as_ref(), &target, clock) {
        Ok(opened) => Opened::Durable(Box::new(opened)),
        Err(error) => {
            diagnostics.note(&format!("compaction target could not be opened: {error}"));
            return Ok(CommandOutcome::Continue);
        }
    };
    let target_state = target_opened.state();
    let target_owner = match ActiveSession::durable(target_opened, target_state, EVIDENCE_CAP) {
        Ok(owner) => owner,
        Err(error) => {
            diagnostics.note(&format!(
                "compaction target evidence could not be opened: {error}"
            ));
            return Ok(CommandOutcome::Continue);
        }
    };
    let diagnostic = receipt.publication.diagnostic().map(str::to_string);
    if matches!(
        &receipt.publication,
        ego_core::session::TargetPublication::NotPublished { .. }
    ) {
        diagnostics.note("compaction target was not published");
        return Ok(CommandOutcome::Continue);
    }
    let runtime = target_runtime
        .lock()
        .expect("interactive compaction runtime")
        .take();
    *checkpoint_pending = automatic_compaction;
    *automatic_compaction_failed = false;
    Ok(CommandOutcome::Transition(Box::new(
        EpochTransition::Compact {
            target: target_owner,
            publication: receipt.publication,
            diagnostic,
            runtime,
            request: request_id,
            parent: pre_compact.closure_event(),
        },
    )))
}

async fn end_session_epoch(
    skill_owner: &CliSkillInvocationOwner<'_>,
    session_hooks: &SessionHookComposition<'_>,
    recovered_epoch_policy: Option<Arc<PolicyEngine>>,
    policy_source: &dyn GenerationPolicySource,
    session_id: SessionId,
) -> Result<(), RunTermination> {
    match skill_owner.drain().await {
        Ok(()) => {
            let runtime = match recovered_epoch_policy {
                Some(policy) => session_hooks.bind_policy(policy),
                None => session_hooks.bind_current(policy_source),
            };
            match runtime {
                Ok(runtime) => dispatch_session_hook(
                    &runtime,
                    session_id,
                    session_hooks.turn,
                    HookLifecycle::SessionEnd,
                )
                .await
                .map(|_| ()),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(RunTermination::Failed {
            message: error.to_string(),
        }),
    }
}

struct InteractiveImages<'a> {
    asked: Option<&'a InteractiveTurnInput>,
    pending_attachments: &'a mut Vec<String>,
    staged_images: &'a mut Vec<StagedPasteImage>,
    source: &'a Arc<model::CliGenerationModelSource>,
    ports: &'a Arc<RunEffectPorts>,
    cancel: &'a CancelToken,
    run_deadline: Option<std::time::Instant>,
    cwd: &'a Path,
    blobs: &'a dyn BlobStore,
    seat: &'a dyn InteractiveSeat,
    diagnostics: &'a dyn Diagnostics,
}

async fn acquire_interactive_images(
    input: InteractiveImages<'_>,
) -> Result<Option<Vec<ImageRef>>, RunTermination> {
    let InteractiveImages {
        asked,
        pending_attachments,
        staged_images,
        source,
        ports,
        cancel: turn_cancel,
        run_deadline,
        cwd,
        blobs,
        seat,
        diagnostics,
    } = input;
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
        .chain(
            staged_images
                .iter()
                .filter_map(|staged| match &staged.source {
                    StagedPasteSource::Path(path) => Some(path.clone()),
                    StagedPasteSource::Inline(_) => None,
                }),
        )
        .collect::<Vec<_>>();
    let images = if matches!(asked, Some(InteractiveTurnInput::Prompt(_)))
        && (!inline.is_empty() || !specs.is_empty())
    {
        let images = match acquire_attachments(
            Attachments {
                inline: &inline,
                specs: &specs,
            },
            source,
            ports,
            turn_cancel,
            run_deadline,
            cwd,
            blobs,
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
                return Ok(None);
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

    Ok(Some(images))
}

fn refresh_turn_status(
    status: Option<&InteractiveStatus>,
    policy_selection: &GenerationPolicySelection,
    selection: &model::GenerationSelection,
    state: &SessionState,
    diagnostics: &dyn Diagnostics,
) {
    if let Some(status) = status
        && status.human
    {
        match policy_selection.snapshot() {
            Ok(policy) => status.seat.set_status_mode(policy.mode),
            Err(error) => diagnostics.note(&format!(
                "the active policy mode could not be refreshed: {error}"
            )),
        }
        match selection.snapshot() {
            Ok(selected) => status.seat.refresh_status(state, selected),
            Err(error) => diagnostics.note(&format!(
                "the native status selection could not be refreshed: {error}"
            )),
        }
    }
}

fn replay_interactive_session(
    args: &Args,
    seat: &dyn InteractiveSeat,
    active: &ActiveSession<'_>,
    text_output: Option<&dyn TextOutput>,
    diagnostics: &dyn Diagnostics,
) {
    if seat.has_human()
        && matches!(&args.session, crate::cli::SessionChoice::Resume(_))
        && let Some(output) = text_output
    {
        let environment = crate::render::caps::Environment::read();
        let capabilities = crate::render::Capabilities::of(&crate::render::caps::Terminal {
            flag_plain: args.plain,
            ..crate::render::caps::Terminal::from_process(args.no_color, &environment)
        });
        if let Err(error) =
            crate::render::replay::replay(active.writer(), active.id(), output, capabilities)
        {
            diagnostics.note(&format!("the transcript could not be replayed: {error}"));
        }
    }
}

fn refresh_epoch_status(
    activation_failed: bool,
    epoch_start: &EpochStart,
    status: Option<&InteractiveStatus>,
    selection: &model::GenerationSelection,
    state: &SessionState,
    diagnostics: &dyn Diagnostics,
) {
    if !activation_failed
        && !matches!(epoch_start, EpochStart::Initial)
        && let Some(status) = status
        && status.human
    {
        match selection.snapshot() {
            Ok(selected) => status.seat.refresh_status(state, selected),
            Err(error) => diagnostics.note(&match epoch_start {
                EpochStart::New => {
                    format!("the new session status selection could not be refreshed: {error}")
                }
                EpochStart::Fork { .. } => {
                    format!("the forked session status selection could not be refreshed: {error}")
                }
                EpochStart::Compact { .. } => format!(
                    "the compacted session status selection could not be refreshed: {error}"
                ),
                EpochStart::Initial => {
                    unreachable!("initial epoch status is not refreshed through transition setup")
                }
            }),
        }
    }
}

async fn dispatch_post_compact_hook(
    epoch_start: &EpochStart,
    session_hooks: &SessionHookComposition<'_>,
    diagnostics: &dyn Diagnostics,
) {
    if let EpochStart::Compact {
        request, parent, ..
    } = epoch_start
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
}

fn report_settled_execs(
    state: &SessionState,
    settled_before: &std::collections::HashSet<ego_core::event::id::DetachedExecId>,
    diagnostics: &dyn Diagnostics,
) {
    for (id, snapshot) in &state.detached_execs {
        if let Some(snapshot) = snapshot
            && !settled_before.contains(id)
        {
            diagnostics.note(&snapshot.model_text());
        }
    }
}

fn report_interactive_turn_end(
    termination: &RunTermination,
    cancel: &CancelToken,
    seat: &dyn InteractiveSeat,
    turn_started: std::time::Instant,
    diagnostics: &dyn Diagnostics,
) {
    if matches!(termination, RunTermination::Signalled(Signal::Int)) && !cancel.is_cancelled() {
        diagnostics.note(&format!(
            "{} after {:.1}s",
            repl::INTERRUPTED_RECEIPT,
            turn_started
                .elapsed()
                .saturating_sub(seat.modal_wait())
                .as_secs_f64()
        ));
    }

    if let RunTermination::Failed { message } = termination {
        diagnostics.note(&format!("{message}; fix the error and retry, or use /quit"));
    }
}

struct CompactionActivation<'a> {
    evidence: &'a EvidenceAdmissionService,
    run_context: &'a Option<RunContextEvidence>,
    diagnostics: &'a dyn Diagnostics,
    skills: Option<&'a crate::skill::SkillCatalogue>,
    registry: &'a mut Vec<RegisteredTool>,
    config: &'a mut TurnConfig,
}
fn activate_compaction_transition<'store>(
    active: &mut ActiveSession<'store>,
    source_state: SessionState,
    session_end: Result<(), RunTermination>,
    target: (
        ActiveSession<'store>,
        ego_core::session::TargetPublication,
        Option<String>,
        Option<CompactionRuntime>,
        RequestId,
        Option<EventId>,
    ),
    input: CompactionActivation<'_>,
) -> Result<EpochStart, RunTermination> {
    let (target, publication, diagnostic, runtime, request, parent) = target;
    let CompactionActivation {
        evidence,
        run_context,
        diagnostics,
        skills,
        registry,
        config,
    } = input;
    let parent_id = active.id();
    let parent_context = source_state.context;
    active.replace_state(source_state);
    if !active.apply_publication(target, &publication) {
        return Err(RunTermination::Failed {
            message: "the published compaction target could not be activated".to_owned(),
        });
    }
    admit_successor_context(evidence, active, run_context)?;
    if let Err(error) = session_end {
        diagnostics.note(&format!(
            "the compacted source session-end hook failed: {}",
            error.exit_reason()
        ));
    }
    publish_compaction_runtime(runtime, skills, registry, config, diagnostics)?;
    diagnostics.note(&format!(
        "compacted session {parent_id} into {}; this is now session {}. \
                             Before: {}; after: known once the next reply is generated. \
                             To return to the source: ego resume {parent_id}",
        active.id(),
        active.id(),
        crate::usage::context_line(parent_context)
    ));
    Ok(EpochStart::Compact {
        diagnostic,
        request,
        parent,
    })
}

struct InteractiveTurnPhase<'a, 'session, 'store> {
    seat: &'a dyn InteractiveSeat,
    status: Option<&'a InteractiveStatus>,
    confinement: &'a RunConfinement,
    active: &'session ActiveSession<'store>,
    run_deadline: Option<std::time::Instant>,
    residual: Option<Arc<ResidualWriteDesk>>,
    perimeter: crate::perimeter::Perimeter,
    session_hook_ports: &'a Arc<RunEffectPorts>,
    pending_attachments: &'a mut Vec<String>,
    staged_images: &'a mut Vec<StagedPasteImage>,
    source: &'a Arc<model::CliGenerationModelSource>,
    cwd: &'a Path,
    diagnostics: &'a dyn Diagnostics,
    state: &'a mut SessionState,
    args: &'a Args,
    effective: &'a Effective,
    policy_source: &'a Arc<dyn GenerationPolicySource>,
    gate: &'a SharedBudgetGate,
    config: &'a TurnConfig,
    evidence: &'a Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    registry: &'a [RegisteredTool],
    admitted_tools: &'a Option<Arc<dyn AdmittedToolSource>>,
    mcp_runtime: &'a dyn McpRuntimeBinding,
    skill_resource_roots: &'a Option<ego_core::skill::SkillResourceRoots>,
    elicitor: &'a Option<Arc<dyn Elicitor>>,
    judge_available: bool,
    skill_trust: Trust,
    session_hooks: &'a SessionHookComposition<'session>,
    session_journal: &'a SharedJournal<'session>,
    skill_owner: Arc<dyn SkillInvocationPort + 'session>,
    refresh_prompt: &'a mut bool,
    control: &'a mut InteractiveControl,
    turns: &'a Turns<'a>,
    last_generation_request: &'a mut Option<GenRequest>,
    policy_selection: &'a GenerationPolicySelection,
    selection: &'a model::GenerationSelection,
    between: &'a (dyn Fn() + Send + Sync),
    cancel: &'a CancelToken,
    last: &'a mut Ended,
    mcp_errors: &'a [String],
}

async fn drive_interactive_turn_phase<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveTurnPhase<'a, 'session, 'store>,
    resumed_turn: Option<RuntimeResume>,
    asked: Option<InteractiveTurnInput>,
) -> Result<(bool, crate::perimeter::Perimeter), RunTermination> {
    let InteractiveTurnPhase {
        seat,
        status,
        confinement,
        active,
        run_deadline,
        residual,
        mut perimeter,
        session_hook_ports,
        pending_attachments,
        staged_images,
        source,
        cwd,
        diagnostics,
        state,
        args,
        effective,
        policy_source,
        gate,
        config,
        evidence,
        invocation,
        registry,
        admitted_tools,
        mcp_runtime,
        skill_resource_roots,
        elicitor,
        judge_available,
        skill_trust,
        session_hooks,
        session_journal,
        skill_owner,
        refresh_prompt,
        control,
        turns,
        last_generation_request,
        policy_selection,
        selection,
        between,
        cancel,
        last,
        mcp_errors,
    } = input;
    // Per turn, for the reason `build` documents.
    seat.busy();
    if let Some(status) = status
        && status.human
    {
        status.seat.activate();
    }
    let turn_started = std::time::Instant::now();
    let turn_cancel = CancelToken::new();
    let (ports, observation, probe_evidence) = effect_scope(
        confinement,
        active.evidence(),
        &turn_cancel,
        run_deadline,
        residual.clone(),
    )
    .await?;
    perimeter = perimeter.with_execution_observation(&observation, probe_evidence.as_ref());
    let ports = Arc::new(ports.with_session_processes(session_hook_ports));
    let Some(images) = acquire_interactive_images(InteractiveImages {
        asked: asked.as_ref(),
        pending_attachments,
        staged_images,
        source,
        ports: &ports,
        cancel: &turn_cancel,
        run_deadline,
        cwd,
        blobs: active.evidence().blob_store(),
        seat,
        diagnostics,
    })
    .await?
    else {
        return Ok((false, perimeter));
    };
    let settled_before = state
        .detached_execs
        .iter()
        .filter_map(|(id, snapshot)| snapshot.is_some().then_some(*id))
        .collect::<std::collections::HashSet<_>>();
    let loop_ = build_interactive_turn(
        InteractiveTurnBuild {
            args,
            effective,
            source,
            policy_source,
            gate,
            config,
            evidence,
            invocation,
            run_deadline,
            registry,
            admitted_tools,
            cwd,
            mcp_runtime,
            skill_resource_roots,
            elicitor,
            judge_available,
            confinement,
            skill_trust,
            active,
            hooks: session_hooks,
            journal: session_journal,
            skill_owner,
        },
        (&ports, &turn_cancel, observation, probe_evidence),
    )?;
    let loop_ =
        match resumed_turn.as_ref() {
            Some(RuntimeResume::Foreign { policy, .. }) => loop_
                .with_hook_policy(Arc::clone(policy))
                .map_err(|message| RunTermination::Failed { message })?,
            Some(RuntimeResume::Model(_)) | None => loop_,
        };
    let previous_epoch = state
        .prompt_snapshot
        .as_ref()
        .map(|snapshot| snapshot.epoch);
    let loop_ = if *refresh_prompt {
        loop_.with_prompt_refresh()
    } else {
        loop_
    };
    let mut loop_ = loop_
        .with_automatic_compaction()
        .with_interactive_control(control);
    let driven = drive_interactive_input(
        turns,
        &mut loop_,
        resumed_turn,
        asked,
        images,
        &turn_cancel,
        diagnostics,
    )
    .await;
    *last_generation_request = loop_.last_generation_request();
    drop(loop_);
    *state = session_hooks.state.state();
    report_settled_execs(state, &settled_before, diagnostics);
    if state
        .prompt_snapshot
        .as_ref()
        .map(|snapshot| snapshot.epoch)
        != previous_epoch
    {
        *refresh_prompt = false;
    }
    refresh_turn_status(status, policy_selection, selection, state, diagnostics);
    seat.refresh_queue(control.status());
    let Some((termination, result)) = driven else {
        return Ok((false, perimeter));
    };
    between();
    report_interactive_turn_end(&termination, cancel, seat, turn_started, diagnostics);
    *last = Ended {
        termination,
        session: active.id(),
        state: state.clone(),
        result,
        selected_session: None,
        mcp_errors: mcp_errors.to_vec(),
    };

    // The person's own cancellations end a turn; the process's end
    // the process. A broken stdout or a SIGTERM is not a turn to
    // return to a prompt from.
    if cancel.is_cancelled() {
        return Ok((true, perimeter));
    }
    Ok((false, perimeter))
}

fn finish_interactive_session(
    active: &mut ActiveSession<'_>,
    last: &mut Ended,
    source_state: SessionState,
    session_end: Result<(), RunTermination>,
    resume: Option<(
        ego_core::store::session_store::SessionMeta,
        &dyn InteractiveSeat,
        InteractiveControl,
    )>,
) {
    active.replace_state(source_state.clone());
    last.state = source_state;
    if let Err(termination) = session_end {
        last.termination = termination;
    } else if let Some((meta, seat, control)) = resume {
        last.selected_session = Some(meta);
        seat.restore_control(control);
    }
}

fn read_interactive_command(
    seat: &dyn InteractiveSeat,
    status: Option<&InteractiveStatus>,
) -> repl::Line {
    seat.idle();
    if let Some(status) = status
        && status.human
    {
        status.seat.activate();
    }
    seat.read()
}

fn activate_new_session<'store>(
    active: &mut ActiveSession<'store>,
    source_state: SessionState,
    session_end: Result<(), RunTermination>,
    target: ActiveSession<'store>,
    context: (
        &EvidenceAdmissionService,
        &Option<RunContextEvidence>,
        &dyn Diagnostics,
    ),
) -> Result<EpochStart, RunTermination> {
    let (evidence, run_context, diagnostics) = context;
    active.replace_state(source_state);
    if let Err(error) = session_end {
        diagnostics.note(&format!(
            "the previous session-end hook failed: {}",
            error.exit_reason()
        ));
    }
    *active = target;
    admit_successor_context(evidence, active, run_context)?;
    Ok(EpochStart::New)
}
