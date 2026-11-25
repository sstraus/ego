//! Drive an admitted session through its selected headless or interactive posture.

use super::*;

#[allow(clippy::too_many_arguments)]
async fn admit_saved_session_policy(
    active: &mut ActiveSession<'_>,
    observation: &ExecObservation,
    effective: &Effective,
    root: &Path,
    journal_ports: (&SystemClock, &dyn EventSink),
    diagnostics: &dyn Diagnostics,
    human: Option<&dyn HumanApprover>,
    accept_delta: bool,
    root_continuation: Option<(Option<TurnId>, &RootTransitionSlot, &CancelToken)>,
) -> Result<(), RunTermination> {
    let refused = |message: String| RunTermination::PermissionRequired {
        action: message,
        by: DeniedBy::Nobody,
    };
    let root_approval = if let Some((continuation, slot, cancel)) = root_continuation {
        let approval = take_root_transition(slot, continuation).map_err(refused)?;
        if cancel.is_cancelled() {
            return Err(cancelled_termination(cancel));
        }
        approval.map(|approval| (continuation, approval))
    } else {
        None
    };
    // The saved authority is the observation the last admitted turn journaled.
    let Some(previous) = active.state().execution_observation.clone() else {
        return if root_approval.is_some() {
            Err(refused(
                "root continuation has no previous execution measurement".into(),
            ))
        } else {
            Ok(())
        };
    };
    let (clock, sink) = journal_ports;
    let mut journal = Journal::new(
        active.id(),
        active.store(),
        clock,
        sink,
        Producer::Main,
        active.state().clone(),
    );
    let approved_roots = active
        .state()
        .session_roots
        .iter()
        .chain(
            active
                .state()
                .permanent_root_consents
                .iter()
                .filter(|consent| {
                    effective.roots.iter().any(|root| {
                        root.path == consent.root.path().as_path()
                            && root.access == consent.root.access()
                            && root.consent.as_ref() == Some(&consent.receipt)
                    })
                })
                .map(|consent| &consent.root),
        )
        .cloned()
        .collect::<Vec<_>>();
    let measured_root_approval = if let Some((continuation, approval)) = root_approval {
        let approved_root = approval.root();
        let grant = ego_core::permit::SandboxGrant::new(
            approved_root.path().clone(),
            match approved_root.access() {
                ego_core::permit::RootAccess::Read => ego_core::permit::SandboxAccess::Read,
                ego_core::permit::RootAccess::ReadWrite => ego_core::permit::SandboxAccess::Write,
            },
        );
        if ego_core::config::Ruleset::is_credential_path(approved_root.path())
            || effective.permissions.denies_sandbox(&grant, root)
        {
            return Err(refused("approved root is now protected or denied".into()));
        }
        let item = approval.item();
        let action = active
            .state()
            .items
            .get(&item)
            .filter(|state| {
                state.kind == ego_core::state::ItemKind::ToolCall
                    && !active.state().has_turn_completed(state.turn)
            })
            .map(|state| (state.turn, item));
        approval
            .consume(
                active.id(),
                continuation,
                action,
                &previous,
                observation,
                &approved_roots,
            )
            .map_err(refused)?;
        true
    } else {
        false
    };
    if observation.same_measurement(&previous) {
        return Ok(());
    }
    ego_core::run::admit_execution_policy(
        &previous,
        observation,
        &approved_roots,
        CanonPath::new(root),
        human,
        accept_delta || measured_root_approval,
        &mut journal,
        |message| {
            diagnostics.note(message);
            Ok(())
        },
    )
    .await
    .map_err(|error| RunTermination::PermissionRequired {
        action: error.to_string(),
        by: if matches!(
            error,
            ego_core::run::RunError::ExecutionPolicyDenied { decided: true }
        ) {
            DeniedBy::Decided(ego_core::permit::Authority::Party)
        } else {
            DeniedBy::Nobody
        },
    })?;
    let state = journal.state().clone();
    drop(journal);
    active.replace_state(state);
    Ok(())
}

fn record_session_selections(
    active: &mut ActiveSession<'_>,
    effective: &Effective,
    tools: Option<Vec<String>>,
    selection: &model::GenerationSelection,
    confinement: &RunConfinement,
    clock: &SystemClock,
    sink: &dyn EventSink,
) -> Result<(), RunTermination> {
    let selected = selection
        .snapshot()
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    let connection = selection.connection().or_else(|| {
        let route = model_route(&selected.identity.0);
        effective
            .providers
            .instances
            .iter()
            .find(|(id, _)| id.as_str() == route)
            .and_then(|(_, instance)| instance.value.credential_ref.as_ref())
            .map(|id| id.as_str())
    });
    let selections = RunSelections::new(
        Some(selected.identity),
        selected.explicit_thinking,
        Some(effective.mode.unwrap_or(Mode::Default)),
        None,
        None,
    )
    .with_connection(connection.map(str::to_owned))
    .with_tools(tools)
    .with_sandbox(confinement.level());
    if active.state().session_selections.as_ref() == Some(&selections) {
        return Ok(());
    }
    let mut journal = Journal::new(
        active.id(),
        active.store(),
        clock,
        sink,
        Producer::Main,
        active.state().clone(),
    );
    journal
        .append(
            TurnId::new(),
            None,
            None,
            EventKind::SessionSelectionsSelected { selections },
        )
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    let state = journal.state().clone();
    drop(journal);
    active.replace_state(state);
    Ok(())
}

async fn paste_image_limits(
    source: &Arc<model::CliGenerationModelSource>,
    confinement: &RunConfinement,
    evidence: &SessionEvidenceStore,
    run_deadline: Option<std::time::Instant>,
) -> Result<ego_core::generator::capabilities::ImageLimits, String> {
    let paste_cancel = CancelToken::new();
    let (ports, _, _) = effect_scope(
        confinement,
        evidence,
        &paste_cancel,
        run_deadline,
        None,
        None,
    )
    .await
    .map_err(|error| format!("image paste could not check the model: {error:?}"))?;
    let http = source.provider_http(ports);
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

/// A successor session opened inside this process needs its own run context
/// before its first generation can be admitted. Publish its PID binding only
/// here, after the active owner has been replaced.
fn admit_successor_context(
    admission: &EvidenceAdmissionService,
    active: &ActiveSession<'_>,
    run_context: &Option<RunContextEvidence>,
) -> Result<(), RunTermination> {
    active
        .writer()
        .publish_pid_index()
        .map_err(|error| RunTermination::Failed {
            message: format!("the active session PID index could not be published: {error}"),
        })?;
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
    New {
        replay: bool,
    },
    Fork {
        diagnostic: Option<String>,
        projected: Option<Box<PublicEvent>>,
    },
    Compact {
        diagnostic: Option<String>,
        request: RequestId,
    },
}

/// A terminal change of saved root authority. Both re-enter the same session.
enum RootReadmission {
    Revoke(CanonPath),
    Grant {
        resume: Option<ego_core::event::id::TurnId>,
    },
}

enum EpochTransition<'store> {
    RevokeRoot {
        path: CanonPath,
    },
    /// A saved root grant becomes effective through full same-session admission.
    GrantRoot {
        resume: Option<ego_core::event::id::TurnId>,
    },
    Resume {
        meta: ego_core::store::session_store::SessionMeta,
    },
    SwitchProvider(Box<ProviderSwitch>),
    Exit,
    New {
        target: ActiveSession<'store>,
        replay: bool,
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
    },
}

pub(super) struct DriverExecution<'host, 'store, S: EventSink> {
    pub(super) provider_switch: Option<(&'host ProviderSwitch, &'host mut bool)>,
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
        mut provider_switch,
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
        mcp_task_port,
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
        no_hooks,
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
        config_notes,
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
    let child_executor = Arc::new(
        child_executor
            .with_pinned_child_authority(&effective, &confinement, selection.connection())
            .with_live_child_selection(Arc::clone(&selection)),
    );
    let ActivatedRun {
        mut active,
        opening,
        sink,
        admitted_tools,
        mcp_errors,
        resume,
    } = activated;
    let mcp_runtime = Arc::clone(mcp_runtime);
    let (mut session_mcp, mut disabled_mcp) =
        session_mcp_configuration(&args, &effective, &cwd, mcp_registry);
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
            config_notes,
        }),
        token_ceiling: effective.ceilings.tokens,
        retry: args.retry,
        fallbacks: args.fallbacks.clone(),
    };

    let exec_environment = effective
        .exec_policy()
        .expect("admitted config has valid environment names");
    let mut refresh_prompt = false;

    let turns = Turns {
        parent_cancel: cancel.clone(),
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
                hooks_off: no_hooks,
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

    match driver {
        Driver::Once => {
            let mut records =
                (args.stdin == crate::cli::Stdin::Jsonl).then(super::headless_jsonl::Input::start);
            let mut last = finish_headless_execution(
                &mut active,
                Ok((RunTermination::Done, opening, Value::String(String::new()))),
                jsonl_store,
                None,
                run_id,
                &mcp_errors,
                diagnostics,
            );
            let mut resumed = resume;
            let mut accept_authority_delta = args.accept_authority_delta;
            loop {
                let prompt = if let Some(records) = records.as_mut() {
                    match records.next(&cancel, run_deadline).await {
                        Ok(Some(prompt)) => prompt,
                        Ok(None) => return Ok(last),
                        Err(termination) => {
                            last.termination = termination;
                            return Ok(last);
                        }
                    }
                } else {
                    RunPrompt::from(text.clone())
                };
                let mut input = headless_input(resumed.take(), &prompt);
                // A CI bundle covers one session, so that profile keeps explicit recovery.
                let compaction_host = artifact_store.is_none();
                let mut turns_taken: u32 = 0;
                let driven = loop {
                    // One session is one judge: its grants die with it.
                    let residual = Some(ResidualWriteDesk::with_limits(effective.residual_limits));
                    let (ports, observation, probe_evidence) = effect_scope(
                        &confinement,
                        active.evidence(),
                        &cancel,
                        run_deadline,
                        residual.clone(),
                        None,
                    )
                    .await?;
                    admit_saved_session_policy(
                        &mut active,
                        &observation,
                        &effective,
                        &root,
                        (&clock, sink),
                        diagnostics,
                        None,
                        std::mem::take(&mut accept_authority_delta),
                        None,
                    )
                    .await?;
                    record_session_selections(
                        &mut active,
                        &effective,
                        super::composition::selected_tools(&args, &effective).cloned(),
                        &selection,
                        &confinement,
                        &clock,
                        sink,
                    )?;
                    let opening = active.state().clone();
                    // Session roots cannot cross an in-process session transition.
                    // Opt-in: `compaction.headless` (Boss, 2026-11-02, story 853-a59c).
                    let automatic_compaction = (compaction_host
                        && effective.compaction.headless
                        && opening.session_roots.is_empty())
                    .then_some(effective.compaction.threshold);
                    let segment_turns = Turns {
                        parent_cancel: turns.parent_cancel.clone(),
                        schema: turns.schema,
                        retries: turns.retries,
                        max_turns: turns.max_turns.map(|max| max.saturating_sub(turns_taken)),
                        deadline: turns.deadline,
                    };
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
                            model_ceiling: None,
                            model_catalogue: None,
                            admission: Arc::clone(&evidence),
                            invocation,
                            config: config.clone(),
                            provider_recovery: args.provider_recovery,
                            goal: args.goal.clone(),
                            goal_turns: args.goal_turns,
                            goal_check_command: args.goal_check_command.clone(),
                            goal_ceilings: effective.ceilings.clone(),
                            child_defaults: effective.task.clone(),
                            gate: gate.clone(),
                            hook_layers: skill_profile_layers.as_deref(),
                            skill_review: effective.skill_safety_review
                                && !args.no_skill_safety_check,
                            hooks_off: no_hooks,
                            hook_trust: skill_trust,
                            policy_source: Arc::clone(&policy_source),
                            judge_available,
                        },
                        HeadlessChild {
                            worker: false,
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
                            elicitor: elicitor.clone(),
                            root_grants: None,
                        },
                        HeadlessTurn {
                            turns: &segment_turns,
                            attachment_specs: &args.attach,
                            input,
                            diagnostics,
                            automatic_compaction,
                        },
                    )
                    .await;
                    let (termination, state, result, compaction) = match driven {
                        Ok(driven) => driven,
                        Err(termination) => break Err(termination),
                    };
                    let Some(compaction) = compaction else {
                        break Ok((termination, state, result));
                    };
                    active.replace_state(state);
                    activate_headless_successor(
                        &mut active,
                        &compaction.receipt,
                        jsonl_store.as_ref(),
                        &clock,
                        (evidence.as_ref(), &run_context),
                    )?;
                    let started = active.state().clone();
                    sink.publish(
                        &PublicEvent::SessionCompacted {
                            source: compaction.receipt.source,
                            source_seq: compaction.receipt.source_seq,
                            target: active.id(),
                        },
                        &started,
                    );
                    sink.publish(
                        &PublicEvent::SessionStarted {
                            v: PUBLIC_EVENT_VERSION,
                            session: active.id(),
                            capabilities: capabilities(
                                &registry,
                                config.system.is_some(),
                                compiled.is_some(),
                                no_hooks,
                            ),
                            mcp_errors: mcp_errors.to_vec(),
                            hook_errors: Vec::new(),
                            instructions: config
                                .system
                                .as_ref()
                                .map(|prompt| prompt.sources.clone())
                                .unwrap_or_default(),
                        },
                        &started,
                    );
                    turns_taken = turns_taken.saturating_add(compaction.turns_taken);
                    input = HeadlessInput::Checkpoint {
                        request: compaction.request,
                    };
                };
                last = finish_headless_execution(
                    &mut active,
                    driven,
                    jsonl_store,
                    artifact_store.as_ref(),
                    run_id,
                    &mcp_errors,
                    diagnostics,
                );
                if records.is_some()
                    && args.output == Output::Jsonl
                    && let Some(output) = text_output
                {
                    super::headless_jsonl::publish(&last, output)?;
                }
                if records.is_none()
                    || !matches!(last.termination, RunTermination::Done)
                    || cancel.is_cancelled()
                {
                    return Ok(last);
                }
            }
        }
        Driver::Interactive {
            seat,
            between,
            mut resume_after_root,
            root_transition,
            seed_opening_prompt,
        } => {
            if matches!(&args.session, SessionChoice::Resume(_))
                || provider_switch
                    .as_ref()
                    .is_some_and(|(change, _)| change.fork.is_some())
            {
                replay_interactive_session(&args, seat.as_ref(), &active, text_output, diagnostics);
            }
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
            let mut resumed = resume_after_root.map(RuntimeResume::Model).or(resume);
            let mut pending_attachments = if seed_opening_prompt {
                args.attach.clone()
            } else {
                Vec::new()
            };
            let mut epoch_start = EpochStart::Initial;
            // The owner's option covers the resume it was given for, not a later `/resume`.
            let mut accept_authority_delta = args.accept_authority_delta;
            register_context_tool(&args, &effective, &mut registry);
            let mut checkpoint_pending = false;
            let mut last_generation_request = None;
            let mut automatic_compaction_failed = false;
            // The prompt-cache deadline of the last idle compaction attempt.
            let mut idle_compaction_attempt = None;

            // Borrow-scoped session authorities live in one lexical epoch.
            // Only owned transition data crosses the boundary where the
            // replaceable ActiveSession is mutated or replaced.
            'sessions: loop {
                // Judge grants never cross epochs. The executing turn restores
                // owner grants only from this session's durable receipts.
                let residual = Some(ResidualWriteDesk::with_limits(effective.residual_limits));
                let sandbox_grants = SeatSandboxGrants::for_seat(
                    Arc::clone(&seat),
                    crate::config::ego_home().map(|home| {
                        (
                            Arc::new(crate::acp_sandbox_grants::FileSandboxGrants::new(home))
                                as Arc<dyn ego_acp::sandbox::SandboxGrantStore>,
                            confinement.workspace().as_path().to_path_buf(),
                        )
                    }),
                    effective.permissions.clone(),
                    effective.deny_prompts,
                );
                let (transition, source_state, session_end) = {
                    let (session_hook_ports, observation, probe_evidence) = effect_scope(
                        &confinement,
                        active.evidence(),
                        &cancel,
                        run_deadline,
                        None,
                        None,
                    )
                    .await?;
                    let wake_seat = Arc::clone(&seat);
                    let background_waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                        wake_seat.wake_background_exec();
                    });
                    session_hook_ports.set_background_waker(Arc::clone(&background_waker));
                    // A live MCP task wakes the idle seat at its poll time;
                    // dropping the timer with this epoch stops it.
                    let _mcp_task_wake = mcp_task_port.as_ref().map(|port| {
                        super::mcp_task_wake::McpTaskWake::start(
                            Arc::clone(port),
                            Arc::clone(&background_waker),
                        )
                    });
                    let human = seat_human(&seat, effective.deny_prompts);
                    admit_saved_session_policy(
                        &mut active,
                        &observation,
                        &effective,
                        &root,
                        (&clock, sink),
                        diagnostics,
                        human.as_deref(),
                        std::mem::take(&mut accept_authority_delta),
                        Some((resume_after_root.take(), &root_transition, &cancel)),
                    )
                    .await?;
                    record_session_selections(
                        &mut active,
                        &effective,
                        super::composition::selected_tools(&args, &effective).cloned(),
                        &selection,
                        &confinement,
                        &clock,
                        sink,
                    )?;
                    perimeter =
                        perimeter.with_execution_observation(&observation, probe_evidence.as_ref());
                    let (recovered_epoch_policy, session_policy) =
                        recover_epoch_policy(resumed.as_ref(), policy_source.as_ref())?;
                    collect_epoch_status_providers(
                        status.as_ref(),
                        &effective,
                        skill_trust,
                        (&cwd, exec_environment.clone()),
                        &session_policy,
                        (&session_hook_ports, active.evidence(), &cancel),
                        diagnostics,
                    )
                    .await;
                    let (leased_session_journal, session_journal, session_hooks) = open_session_hooks!(
                        active,
                        active.state().clone(),
                        session_hook_ports,
                        session_policy
                    );
                    if let Some((change, activated)) = provider_switch.take() {
                        if !seat.activate_session(active.id()) {
                            return Err(RunTermination::UsageError {
                                message: "the terminal could not activate the selected provider"
                                    .into(),
                            });
                        }
                        *activated = true;
                        complete_provider_switch(
                            change,
                            &session_hooks,
                            status.as_ref(),
                            diagnostics,
                        )
                        .await;
                    }
                    let child_bindings = epoch_child_bindings(
                        &elicitor,
                        &gate,
                        admitted_tools.as_ref(),
                        &mcp_errors,
                        &mcp_runtime,
                    );
                    let mut family_executor = (*child_executor)
                        .clone()
                        .with_live_child_selection(Arc::clone(&selection));
                    family_executor.token_family = Some(active.id());
                    let child_service = compose_child_service(ChildComposition {
                        parent_run: RunId::new(active.id(), invocation.uuid()),
                        parent_session: active.id(),
                        root: child_root.clone(),
                        store: Arc::clone(jsonl_store),
                        executor: Arc::new(family_executor),
                        gate: gate.clone(),
                        bindings: child_bindings,
                        blobs: active.evidence().clone(),
                    });
                    let skill_owner = Arc::new(
                        CliSkillInvocationOwner::new(
                            child_service,
                            session_hooks.runtime.clone(),
                            cancel.clone(),
                            skill_trust,
                        )
                        .with_token_family(Arc::clone(jsonl_store), active.id())
                        .with_task_blobs(active.evidence().clone())
                        .with_task_waker(background_waker)
                        .with_task_surface(
                            status
                                .as_ref()
                                .and_then(|status| status.seat.background_task_surface()),
                        )
                        .with_task_policy(
                            policy_source
                                .capture()
                                .map_err(|message| RunTermination::Failed { message })?
                                .snapshot()
                                .digest(),
                        ),
                    );
                    let model_switch_owner =
                        epoch_model_switch_owner(&session_hooks, status.as_ref(), &selection);
                    dispatch_post_compact_hook(&epoch_start, &session_hooks, diagnostics).await;
                    state = session_hooks.state.state();
                    last.session = active.id();
                    last.state = state.clone();
                    last.result = Value::String(String::new());
                    if matches!(epoch_start, EpochStart::New { replay: true }) {
                        replay_interactive_session(
                            &args,
                            seat.as_ref(),
                            &active,
                            text_output,
                            diagnostics,
                        );
                    }
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
                            hooks_off: no_hooks,
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
                    // A root saved in this epoch waits for a quiet turn boundary.
                    let mut root_grant_pending = false;
                    let mut root_resume_turn = None;
                    let transition = if activation_failed {
                        EpochTransition::Exit
                    } else {
                        'epoch: loop {
                            let resumed_turn = resumed.take();
                            let queued =
                                has_admitted_interactive_input(resumed_turn.as_ref(), &state);
                            let asked = if queued {
                                None
                            } else if let Some(input) =
                                take_pending_checkpoint(&mut checkpoint_pending)
                            {
                                Some(input)
                            } else {
                                let pending_work = interactive_work_pending(&control, &state);
                                // The snapshot reads model facts from disk; skip it
                                // on every idle read while idle compaction is off.
                                let idle = effective
                                    .compaction
                                    .idle
                                    .then(|| selection.snapshot().ok())
                                    .flatten()
                                    .map(|selected| IdleCompactionWatch {
                                        config: &effective.compaction,
                                        state: &state,
                                        model: selected.identity.0,
                                        attempted: &mut idle_compaction_attempt,
                                        clock: &clock,
                                    });
                                let (line, model_compaction, automatic_compaction) =
                                    next_interactive_command(
                                        seat.as_ref(),
                                        status.as_ref(),
                                        &state,
                                        &args,
                                        &mut seeded,
                                        pending_work,
                                        (automatic_compaction_failed, &last.termination),
                                        idle,
                                        effective.compaction.threshold,
                                    );

                                if let Some(residual) = &residual {
                                    perimeter = perimeter.with_residual_grants(&residual.grants());
                                }

                                match dispatch_interactive_command(
                                    InteractiveCommand {
                                        args: &args,
                                        active: &active,
                                        pending_attachments: &mut pending_attachments,
                                        seat: &seat,
                                        source: &source,
                                        confinement: &confinement,
                                        run_deadline,
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
                                        mcp_task_port: mcp_task_port.as_ref(),
                                        session_evidence: active.evidence(),
                                        task_owner: &skill_owner,
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

                            // Fresh work can retry a transient failure, but cannot
                            // remove the structural session-root blocker. A successor
                            // or root revocation re-arms recovery at its transition.
                            if state.session_roots.is_empty()
                                && (asked.as_ref().is_some_and(|input| {
                                    !matches!(input, InteractiveTurnInput::Checkpoint)
                                }) || !state.unacknowledged_prompts().is_empty())
                            {
                                automatic_compaction_failed = false;
                            }
                            let checkpoint_turn =
                                matches!(asked, Some(InteractiveTurnInput::Checkpoint));
                            let (exit, next_perimeter, readmission) = drive_interactive_turn_phase(
                                InteractiveTurnPhase {
                                    root_transition: &root_transition,
                                    seat: seat.as_ref(),
                                    status: status.as_ref(),
                                    confinement: &confinement,
                                    active: &active,
                                    run_deadline,
                                    residual: residual.clone(),
                                    sandbox_grants: sandbox_grants.clone(),
                                    perimeter,
                                    session_hook_ports: &session_hook_ports,
                                    pending_attachments: &mut pending_attachments,
                                    source: &source,
                                    cwd: &cwd,
                                    diagnostics,
                                    text_output,
                                    state: &mut state,
                                    args: &args,
                                    effective: &effective,
                                    policy_source: &policy_source,
                                    gate: &gate,
                                    config: &config,
                                    evidence: &evidence,
                                    invocation,
                                    registry: &registry,
                                    skills: skills.as_deref(),
                                    mcp_registry,
                                    session_mcp: &session_mcp,
                                    disabled_mcp: &disabled_mcp,
                                    admitted_tools: &admitted_tools,
                                    mcp_runtime: mcp_runtime.as_ref(),
                                    skill_resource_roots: &skill_resource_roots,
                                    elicitor: &elicitor,
                                    judge_available,
                                    skill_trust,
                                    session_hooks: &session_hooks,
                                    session_journal: &session_journal,
                                    interactive_metadata: &interactive_metadata,
                                    leased_session_journal: &leased_session_journal,
                                    skill_owner: skill_owner.clone(),
                                    mcp_task_port: mcp_task_port.clone(),
                                    refresh_prompt: &mut refresh_prompt,
                                    control: &mut control,
                                    turns: &turns,
                                    last_generation_request: &mut last_generation_request,
                                    policy_selection: &policy_selection,
                                    selection: &selection,
                                    model_switch_owner: model_switch_owner.as_ref(),
                                    policy_application: policy_application.as_deref(),
                                    between,
                                    cancel: &cancel,
                                    last: &mut last,
                                    mcp_errors: &mcp_errors,
                                    jsonl_store,
                                    root: &root,
                                    clock: &clock,
                                    skill_profile_layers: skill_profile_layers.as_deref(),
                                },
                                resumed_turn,
                                asked,
                            )
                            .await?;
                            perimeter = next_perimeter;
                            // As headless: a successor compacts again only when it is
                            // cut. One that finishes above the threshold waits for input.
                            if checkpoint_turn
                                && !matches!(
                                    last.termination,
                                    RunTermination::ContextOverflow { .. }
                                )
                            {
                                automatic_compaction_failed = true;
                            }
                            let granted = match readmission {
                                Some(RootReadmission::Grant { resume }) => {
                                    root_resume_turn = resume.or(root_resume_turn);
                                    true
                                }
                                Some(RootReadmission::Revoke(path)) => {
                                    break 'epoch EpochTransition::RevokeRoot { path };
                                }
                                None => false,
                            };
                            if exit {
                                break 'epoch EpochTransition::Exit;
                            }
                            root_grant_pending |= granted;
                            if root_grant_pending {
                                // Re-admission ends this epoch's effect owners, so
                                // it never stops background work the owner started.
                                if state.detached_execs.values().any(Option::is_none)
                                    || skill_owner
                                        .has_unsettled_tasks()
                                        .map_err(|message| RunTermination::Failed { message })?
                                {
                                    if granted {
                                        diagnostics.note(
                                            "Root saved; ego re-admits this session when background work ends",
                                        );
                                    }
                                } else {
                                    break 'epoch EpochTransition::GrantRoot {
                                        resume: root_resume_turn,
                                    };
                                }
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
                    // MCP tasks belong to the session that started them. A root
                    // change keeps the session; every other transition ends it.
                    if let Some(port) = &mcp_task_port
                        && !matches!(
                            transition,
                            EpochTransition::RevokeRoot { .. } | EpochTransition::GrantRoot { .. }
                        )
                    {
                        let fs = OsFs;
                        let effects = ToolEffectPorts::new(
                            &fs,
                            session_hook_ports.process(),
                            active.evidence(),
                            session_hook_ports.resolver_handle(),
                            session_hook_ports.one_hop_handle(),
                            session_hook_ports.duplex_handle(),
                        );
                        port.cancel_all(effects).await;
                    }
                    drop(session_hook_ports);
                    (transition, source_state, session_end)
                };

                match transition {
                    EpochTransition::RevokeRoot { path } => {
                        // The lexical epoch has dropped every old effect owner and
                        // settled its child trees. Re-enter admission through the
                        // existing outer same-session loop; never mutate a live guard.
                        session_end?;
                        active.replace_state(source_state);
                        let meta = root_revocation::revoke(
                            &mut active,
                            path,
                            &args,
                            &clock,
                            sink,
                            jsonl_store,
                        )?;
                        last.state = active.state().clone();
                        last.termination = RunTermination::Done;
                        last.selected_session = Some(meta.into());
                        seat.restore_control(control);
                        diagnostics
                            .note("Root revoked; children stopped; rebuilding session authority");
                        break 'sessions Ok(last);
                    }
                    EpochTransition::GrantRoot { resume } => {
                        // The same boundary as revocation: this epoch's effect
                        // owners are gone, and the outer loop admits the session
                        // again from its journal and config. No live guard changes.
                        session_end?;
                        active.replace_state(source_state);
                        let meta = jsonl_store
                            .readable_meta(&active.id())
                            .map_err(|error| RunTermination::Failed {
                                message: error.to_string(),
                            })?
                            .ok_or_else(|| RunTermination::Failed {
                                message: "granted session metadata is unavailable".into(),
                            })?;
                        last.state = active.state().clone();
                        last.selected_session = Some(SessionReadmission::Resume {
                            meta: Box::new(meta),
                            root_resume: resume,
                        });
                        seat.restore_control(control);
                        diagnostics.note("Root granted; rebuilding session authority");
                        break 'sessions Ok(last);
                    }
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
                    EpochTransition::SwitchProvider(change) => {
                        if session_end.is_ok() {
                            last.selected_session =
                                Some(SessionReadmission::SwitchProvider(change));
                            seat.restore_control(control);
                        }
                        finish_interactive_session(
                            &mut active,
                            &mut last,
                            source_state,
                            session_end,
                            None,
                        );
                        break 'sessions Ok(last);
                    }
                    transition => {
                        // The failure flag and the last ending belong to the
                        // source. A successor compacts again once it measured
                        // usage (1076-c308); /clear, /new and /fork start quiet.
                        automatic_compaction_failed = false;
                        last.termination = RunTermination::Done;
                        epoch_start = activate_session_transition(
                            transition,
                            &mut active,
                            source_state,
                            session_end,
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

fn activate_session_transition<'store>(
    transition: EpochTransition<'store>,
    active: &mut ActiveSession<'store>,
    source_state: SessionState,
    session_end: Result<(), RunTermination>,
    activation: CompactionActivation<'_>,
) -> Result<EpochStart, RunTermination> {
    let CompactionActivation {
        evidence,
        run_context,
        diagnostics,
        skills,
        registry,
        config,
    } = activation;
    // The process policy and OS guard were built for the source session.
    // A successor must never reuse its session-only filesystem authority.
    if !source_state.session_roots.is_empty() {
        return Err(RunTermination::Failed {
            message: "session roots cannot cross an in-process session transition; preserve a factual handoff and start a fresh independently admitted ego session, not a same-ID resume".into(),
        });
    }
    match transition {
        EpochTransition::New { target, replay } => activate_new_session(
            active,
            source_state,
            session_end,
            target,
            (evidence, run_context, diagnostics),
        )
        .map(|()| EpochStart::New { replay }),
        EpochTransition::Fork {
            target,
            publication,
            diagnostic,
            projected,
        } => activate_fork_transition(
            active,
            source_state,
            session_end,
            (target, publication, diagnostic, projected),
            (evidence, run_context, diagnostics),
        ),
        EpochTransition::Compact {
            target,
            publication,
            diagnostic,
            runtime,
            request,
        } => activate_compaction_transition(
            active,
            source_state,
            session_end,
            (target, publication, diagnostic, runtime, request),
            CompactionActivation {
                evidence,
                run_context,
                diagnostics,
                skills,
                registry,
                config,
            },
        ),
        EpochTransition::Exit
        | EpochTransition::Resume { .. }
        | EpochTransition::SwitchProvider(_)
        | EpochTransition::RevokeRoot { .. }
        | EpochTransition::GrantRoot { .. } => {
            unreachable!("terminal transitions retain the driver control owner")
        }
    }
}

fn headless_input(resume: Option<RuntimeResume>, prompt: &RunPrompt) -> HeadlessInput<'_> {
    match resume {
        Some(RuntimeResume::Model(turn)) => HeadlessInput::Resumed(turn),
        Some(RuntimeResume::Foreign {
            question,
            answer,
            policy,
        }) => HeadlessInput::Answered {
            question,
            answer: answer.expect("only the terminal plans an answerless foreign presentation"),
            policy,
        },
        None => HeadlessInput::Prompt(prompt),
    }
}

fn finish_headless_execution(
    active: &mut ActiveSession<'_>,
    driven: Result<(RunTermination, SessionState, Value), RunTermination>,
    jsonl_store: &JsonlStore,
    artifact_store: Option<&JsonlStore>,
    run_id: RunId,
    mcp_errors: &[String],
    diagnostics: &dyn Diagnostics,
) -> Ended {
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
        mcp_errors: mcp_errors.to_vec(),
    };
    if let Some(artifact_store) = artifact_store {
        finalize_ci_bundle(
            jsonl_store,
            artifact_store,
            run_id,
            &mut ended.termination,
            diagnostics,
        );
    }
    ended
}

fn register_context_tool(args: &Args, effective: &Effective, registry: &mut Vec<RegisteredTool>) {
    if !args.no_context_tool && super::composition::selected_tools(args, effective).is_none() {
        registry.push(RegisteredTool::Context);
    }
}

fn epoch_child_bindings(
    elicitor: &Option<Arc<dyn Elicitor>>,
    gate: &SharedBudgetGate,
    admitted_tools: Option<&Arc<dyn AdmittedToolSource>>,
    mcp_errors: &[String],
    mcp_runtime: &Arc<ego_mcp::McpRegistryRuntime>,
) -> RunExecutionBindings {
    let mut child_bindings = RunExecutionBindings::new(
        Arc::new(CliChildSinkFactory),
        elicitor.as_ref().map(Arc::clone),
    )
    .with_budget_gate(gate.clone());
    if let Some(admitted) = admitted_tools {
        child_bindings = child_bindings.with_mcp(
            Arc::clone(admitted),
            mcp_errors.to_vec(),
            Arc::clone(mcp_runtime) as Arc<dyn McpRuntimeBinding>,
        );
    }
    child_bindings
}

fn has_admitted_interactive_input(resumed: Option<&RuntimeResume>, state: &SessionState) -> bool {
    // Durable work precedes checkpoint recovery and a fresh terminal read.
    resumed.is_some() || !state.unacknowledged_prompts().is_empty()
}

fn take_pending_checkpoint(pending: &mut bool) -> Option<InteractiveTurnInput> {
    if !*pending {
        return None;
    }
    *pending = false;
    Some(InteractiveTurnInput::Checkpoint)
}

fn epoch_model_switch_owner<'session>(
    hooks: &SessionHookComposition<'session>,
    status: Option<&InteractiveStatus>,
    selection: &Arc<model::GenerationSelection>,
) -> Option<RunModelSwitchOwner<'session>> {
    status.and_then(|status| {
        status.model_config_store.as_ref().map(|config_store| {
            RunModelSwitchOwner::new(
                Arc::clone(selection),
                Arc::clone(config_store),
                hooks.runtime.clone(),
                hooks.turn,
                None,
            )
        })
    })
}

fn session_mcp_configuration(
    args: &Args,
    effective: &Effective,
    cwd: &Path,
    mcp_registry: &McpRegistryController,
) -> (
    Vec<(McpServerName, McpServerConfig, McpConfigProvenance)>,
    BTreeSet<McpServerName>,
) {
    // `--mcp-only` sessions list and toggle only what the invocation selected.
    let mut session_mcp: Vec<_> = effective
        .mcp
        .entries()
        .iter()
        .filter(|_| !args.mcp_only)
        .filter_map(|(name, entry)| match entry {
            ego_core::config::ResolvedMcpEntry::Configured {
                server, provenance, ..
            } => Some((name.clone(), server.clone(), provenance.clone())),
            ego_core::config::ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .collect();
    // `--mcp @FILE` servers are not configuration, yet the session attached them: `/mcp`
    // lists them like every other attached server. Startup already validated the selection.
    for entry in crate::config::selected_mcp(&args.mcp, effective, cwd).unwrap_or_default() {
        if !session_mcp.iter().any(|(name, _, _)| name == &entry.0) {
            session_mcp.push(entry);
        }
    }
    let disabled_mcp = effective
        .mcp
        .entries()
        .iter()
        .filter(|(name, entry)| {
            matches!(
                entry,
                ego_core::config::ResolvedMcpEntry::Configured { enabled: false, .. }
            ) && mcp_registry.status().server(name).is_none()
                && session_mcp.iter().any(|(listed, _, _)| listed == *name)
        })
        .map(|(name, _)| name.clone())
        .collect();
    (session_mcp, disabled_mcp)
}

fn interactive_work_pending(control: &InteractiveControl, state: &SessionState) -> bool {
    let queue = control.status();
    queue.pending_steers > 0
        || queue.queued > 0
        || queue.interrupt_pending
        || !state.unacknowledged_prompts().is_empty()
        || state.pending_question().is_some()
}

/// Why a compaction runs without the operator's `/compact` confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutomaticCompaction {
    /// Context pressure or a model request; the task continues afterwards.
    Pressure,
    /// The idle session's prompt cache is about to expire; nothing continues.
    CacheExpiry,
}

/// Pending work, model-requested compaction, and the automatic trigger.
type CommandPosture = (bool, bool, Option<AutomaticCompaction>);

/// The facts an idle read needs to start compaction before the cache expires.
struct IdleCompactionWatch<'a> {
    config: &'a ego_core::compact::CompactionConfig,
    state: &'a SessionState,
    model: String,
    attempted: &'a mut Option<ego_core::event::time::Timestamp>,
    clock: &'a dyn ego_core::event::time::Clock,
}

impl IdleCompactionWatch<'_> {
    fn decide(&self) -> ego_core::compact::IdleCompaction {
        self.config
            .idle_compaction(self.state, &self.model, *self.attempted, self.clock.now())
    }
}

#[allow(clippy::too_many_arguments)]
fn next_interactive_command(
    seat: &dyn InteractiveSeat,
    status: Option<&InteractiveStatus>,
    state: &SessionState,
    args: &Args,
    seeded: &mut Option<String>,
    pending_work: bool,
    recovery: (bool, &RunTermination),
    idle: Option<IdleCompactionWatch<'_>>,
    threshold: ego_core::compact::ContextPercent,
) -> (repl::Line, bool, Option<AutomaticCompaction>) {
    let (automatic_compaction_failed, termination) = recovery;
    // Reopened attempts are journaled interruptions, not live terminal work.
    seat.set_compaction_availability(compaction_availability(pending_work));
    // Automatic recovery must not end the epoch owning an unconsumed result.
    // Explicit session transitions keep their existing operator admission.
    let compaction_pending =
        compaction_availability(pending_work || state.detached_execs.values().any(Option::is_none))
            != repl::actions::Availability::Enabled;
    let model_compaction = state.context_request.is_some() && !args.no_context_tool;
    let overflowed = matches!(termination, RunTermination::ContextOverflow { .. });
    // As in a headless run: a successor that overflows before it measured any
    // usage would compact the same checkpoint again, so it keeps the overflow.
    let recoverable = !automatic_compaction_failed
        && (state.context.used_tokens.is_some()
            || !matches!(
                state.lineage,
                Some(ego_core::session::SessionLineage::Compaction { .. })
            ));
    let pressure = recoverable
        && !compaction_pending
        && (model_compaction
            || ego_core::turn::compaction_due(state.context, threshold)
            || overflowed);
    let compact =
        || repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Compact));
    // Keep reads on the terminal owner and return output through its painter.
    if pressure {
        return (
            compact(),
            model_compaction,
            Some(AutomaticCompaction::Pressure),
        );
    }
    // Recovery of a turn cut at the context limit failed, or its successor was
    // cut before it measured usage: the task cannot continue without input.
    // Say so to the host instead of waiting idle (1076-c308).
    if !compaction_pending && overflowed {
        seat.hold_error("context_overflow");
    }
    if let Some(first) = seeded.take() {
        return (repl::Line::Submitted(first), model_compaction, None);
    }
    // A failed compaction stays idle until fresh operator work re-arms it.
    let idle = idle.filter(|_| !automatic_compaction_failed && !compaction_pending);
    match read_interactive_command(seat, status, idle) {
        Some(line) => (line, model_compaction, None),
        None => (
            compact(),
            model_compaction,
            Some(AutomaticCompaction::CacheExpiry),
        ),
    }
}

fn recover_epoch_policy(
    resumed: Option<&RuntimeResume>,
    policy_source: &dyn GenerationPolicySource,
) -> Result<(Option<Arc<PolicyEngine>>, Arc<PolicyEngine>), RunTermination> {
    let recovered = match resumed {
        Some(RuntimeResume::Foreign { policy, .. }) => Some(Arc::clone(policy)),
        Some(RuntimeResume::Model(_)) | None => None,
    };
    let policy = match recovered.as_ref() {
        Some(policy) => Arc::clone(policy),
        None => policy_source
            .current()
            .map_err(|message| RunTermination::Failed { message })?,
    };
    Ok((recovered, policy))
}

async fn collect_epoch_status_providers(
    status: Option<&InteractiveStatus>,
    effective: &Effective,
    skill_trust: Trust,
    environment: (&Path, ego_core::permit::EnvPolicy),
    policy: &PolicyEngine,
    effects: (&RunEffectPorts, &SessionEvidenceStore, &CancelToken),
    diagnostics: &dyn Diagnostics,
) {
    let (cwd, exec_environment) = environment;
    let (session_hook_ports, evidence, cancel) = effects;
    let fs = OsFs;
    if let Some(status) = status.filter(|status| status.human && status.native)
        && skill_trust == Trust::Trusted
        && let Some(providers) = effective
            .ui
            .statusline
            .as_ref()
            .and_then(|line| line.providers.as_ref())
    {
        let effects = ToolEffectPorts::new(
            &fs,
            session_hook_ports.process(),
            evidence,
            session_hook_ports.resolver_handle(),
            session_hook_ports.one_hop_handle(),
            session_hook_ports.duplex_handle(),
        )
        .with_cancel(cancel.clone());
        let (values, errors) = crate::status_provider::collect(
            providers,
            cwd,
            exec_environment,
            policy,
            effects,
            session_hook_ports.process(),
        )
        .await;
        status.seat.set_status_providers(values);
        for error in errors {
            diagnostics.note(&error);
        }
    }
}

async fn stage_clipboard_image(
    seat: &dyn InteractiveSeat,
    source: &Arc<model::CliGenerationModelSource>,
    effects: (
        &RunConfinement,
        &SessionEvidenceStore,
        Option<std::time::Instant>,
    ),
    draft: &repl::ImageDraft,
) -> Result<(), String> {
    let (confinement, evidence, deadline) = effects;
    match seat
        .pasteboard_kind()
        .map_err(|error| format!("pasteboard could not be inspected: {error}"))?
    {
        PasteboardKind::Image => {
            let limits = paste_image_limits(source, confinement, evidence, deadline).await?;
            let cap = limits
                .max_bytes_per_image()
                .map_or(ego_core::generator::types::MAX_IMAGE_BYTES, |n| n.get());
            let bytes = seat.pasteboard_image(cap)?;
            let image = Image::new(bytes, ego_core::generator::types::ImageMediaType::Png)
                .map_err(|error| format!("pasteboard image is invalid: {error}"))?;
            let label = format!("clipboard {}", draft.labels().len() + 1);
            draft.push(
                repl::DraftImage {
                    label,
                    source: repl::DraftImageSource::Inline(image),
                },
                &limits,
            )?;
        }
        PasteboardKind::Text => seat.insert_paste_text(seat.pasteboard_text()?),
        PasteboardKind::Empty => return Err("pasteboard has no image or text".to_owned()),
    }
    Ok(())
}

async fn stage_image_path(
    path: String,
    source: &Arc<model::CliGenerationModelSource>,
    effects: (
        &RunConfinement,
        &SessionEvidenceStore,
        Option<std::time::Instant>,
    ),
    draft: &repl::ImageDraft,
) -> Result<(), String> {
    let (confinement, evidence, deadline) = effects;
    let limits = paste_image_limits(source, confinement, evidence, deadline).await?;
    let bytes = std::fs::metadata(&path)
        .map_err(|error| format!("image paste could not inspect {path}: {error}"))?
        .len();
    let label = Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    draft.push(
        repl::DraftImage {
            label,
            source: repl::DraftImageSource::Path { path, bytes },
        },
        &limits,
    )
}

fn image_inputs(draft: &repl::ImageDraft) -> Vec<attachment::AttachmentInput> {
    draft
        .snapshot()
        .into_iter()
        .map(|image| match image.source {
            repl::DraftImageSource::Inline(image) => attachment::AttachmentInput::Inline(image),
            repl::DraftImageSource::Path { path, .. } => attachment::AttachmentInput::Spec(path),
            repl::DraftImageSource::Published(image) => {
                attachment::AttachmentInput::Published(image)
            }
        })
        .collect()
}

/// The recap shown before the first prompt after an absence. It never enters
/// the session history; the usage line states what it cost, and `/usage`
/// counts it with the main model. A failure is shown and the prompt runs.
fn render_recap(recap: Result<ego_core::turn::Recap, ego_core::turn::RecapFailure>) -> String {
    match recap {
        Ok(recap) => {
            let usage = recap.usage().map_or_else(
                || "usage not reported".to_owned(),
                |usage| {
                    format!(
                        "usage: {} input, {} cached, {} output tokens",
                        usage.uncached_input_tokens, usage.cache_read_tokens, usage.output_tokens
                    )
                },
            );
            format!("While you were away — recap:\n{}\n({usage})", recap.text())
        }
        Err(error) => format!("recap unavailable: {error}"),
    }
}

const SIDE_QUESTION_IDLE: &str =
    "/side needs a running turn; send the question as a normal prompt instead";

/// Answer one side question beside the running turn and render it as a notice.
/// The notice never enters the session history; the usage line states what the
/// answer cost, and `/usage` counts it with the main model.
async fn answer_side_question(
    port: &ego_core::turn::SideQuestionPort<'_, '_>,
    question: &str,
) -> String {
    match port.ask(question, &CancelToken::new()).await {
        Ok(answer) => {
            let usage = answer.usage().map_or_else(
                || "usage not reported".to_owned(),
                |usage| {
                    format!(
                        "usage: {} input, {} cached, {} output tokens",
                        usage.uncached_input_tokens, usage.cache_read_tokens, usage.output_tokens
                    )
                },
            );
            format!("side question: {question}\n{}\n({usage})", answer.text())
        }
        Err(error) => format!("side question failed: {error}"),
    }
}

async fn answer_image_request(
    request: &repl::ImageRequest,
    seat: &dyn InteractiveSeat,
    source: &Arc<model::CliGenerationModelSource>,
    effects: (
        &RunConfinement,
        &SessionEvidenceStore,
        Option<std::time::Instant>,
    ),
    cwd: &Path,
    extra_specs: &[String],
) -> bool {
    let (confinement, evidence, deadline) = effects;
    let draft = request.draft();
    let result = match request.operation() {
        repl::ImageOperation::Clipboard => stage_clipboard_image(seat, source, effects, draft)
            .await
            .map(|()| Vec::new()),
        repl::ImageOperation::Path(path) => stage_image_path(path.clone(), source, effects, draft)
            .await
            .map(|()| Vec::new()),
        repl::ImageOperation::Admit => {
            let mut inputs = image_inputs(draft);
            inputs.extend(
                extra_specs
                    .iter()
                    .cloned()
                    .map(attachment::AttachmentInput::Spec),
            );
            if inputs.is_empty() {
                Ok(Vec::new())
            } else {
                let cancel = CancelToken::new();
                match effect_scope(confinement, evidence, &cancel, deadline, None, None).await {
                    Ok((ports, _, _)) => acquire_image_inputs(
                        &inputs,
                        source,
                        &ports,
                        &cancel,
                        deadline,
                        cwd,
                        evidence.blob_store(),
                    )
                    .await
                    .map_err(|error| format!("{error:?}; the complete draft was retained")),
                    Err(error) => Err(format!(
                        "the image could not be added: {error:?}; your entire draft was kept"
                    )),
                }
            }
        }
    };
    let accepted = result.is_ok();
    if let Ok(images) = &result
        && matches!(request.operation(), repl::ImageOperation::Admit)
    {
        draft.published(images);
    }
    request.reply(result);
    accepted
}

async fn run_interactive_setup(
    prompt: String,
    root: &Path,
    status: Option<&InteractiveStatus>,
    effective: &Effective,
    diagnostics: &dyn Diagnostics,
) {
    use ego_core::question::{
        QuestionAnswer, QuestionMode, QuestionOption, QuestionOutcome, QuestionRequest,
    };

    if !prompt.is_empty() {
        diagnostics.note("Setup configures saved settings; use /doctor for diagnostics.");
        return;
    }
    let Some(status) = status.filter(|status| status.human) else {
        diagnostics.note("Setup needs a human terminal seat; no settings were changed.");
        return;
    };
    let Some(home) = crate::config::ego_home() else {
        diagnostics.note("Setup needs EGO_HOME or HOME; no settings were changed.");
        return;
    };
    struct SeatScreen<'a> {
        status: &'a InteractiveStatus,
        diagnostics: &'a dyn Diagnostics,
    }
    impl crate::setup_config::Screen for SeatScreen<'_> {
        fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
            let rows = options
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    let id = if label.starts_with("Review and save") {
                        "setup:save".to_owned()
                    } else {
                        format!("setup:{index}")
                    };
                    QuestionOption::new(
                        id,
                        crate::render::table::plain_table_with_width(
                            vec![vec![label.clone()]],
                            self.status.seat.width().saturating_sub(8).max(1),
                        )
                        .pop()
                        .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>();
            let request = QuestionRequest::new(title, QuestionMode::Single, rows, false);
            match self.status.seat.select_setup(request) {
                QuestionOutcome::Answered(QuestionAnswer::Single(id)) if id == "setup:save" => {
                    options
                        .iter()
                        .position(|label| label.starts_with("Review and save"))
                }
                QuestionOutcome::Answered(QuestionAnswer::Single(id)) => {
                    id.strip_prefix("setup:")?.parse().ok()
                }
                _ => None,
            }
        }
        fn input(&mut self, prompt: &str) -> Option<String> {
            let request = QuestionRequest::new(prompt, QuestionMode::FreeText, Vec::new(), true);
            match self.status.seat.select_setup(request) {
                QuestionOutcome::Answered(QuestionAnswer::FreeText(value)) => Some(value),
                _ => None,
            }
        }
        fn confirm(&mut self, preview: &str) -> bool {
            // The whole review is the modal prompt, ending in its question. A
            // review taller than the screen scrolls into the terminal
            // scrollback; a blank answer, Esc or Ctrl-C keeps the default No.
            matches!(
                self.status.seat.confirm_setup(format!("{preview} [y/N] ")),
                repl::Confirmation::Confirmed
            )
        }
        fn notice(&mut self, text: &str) {
            self.diagnostics.note(text);
        }
    }
    let mut screen = SeatScreen {
        status,
        diagnostics,
    };
    let providers = match crate::onboarding::captured_provider_readiness(&home, effective) {
        Ok(providers) => providers,
        Err(error) => {
            diagnostics.note(&format!("Setup: {error}"));
            return;
        }
    };
    if let Err(error) =
        crate::setup_config::run(&mut screen, root, &home, None, Some(effective), &providers)
    {
        diagnostics.note(&format!("Setup: {error}"));
    }
}

async fn run_interactive_config(
    arguments: &str,
    effective: &Effective,
    skill_profile_layers: Option<&[Sourced<ConfigLayer>]>,
    perimeter: &crate::perimeter::Perimeter,
    diagnostics: &dyn Diagnostics,
) {
    if arguments == "details" {
        diagnostics.note(&crate::config::repl_details(
            effective,
            skill_profile_layers.unwrap_or_default(),
            perimeter,
        ));
    } else if arguments == "perimeter" {
        diagnostics.note(&perimeter.text());
    } else {
        match crate::config::repl_set_args(arguments) {
            Ok(set) => match crate::config::repl_set(&set) {
                Ok(receipt) => {
                    diagnostics.note(&receipt);
                    diagnostics.note("Saved to your user configuration for future sessions. This session keeps its current settings.");
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
        report_saved_sessions(store, root, diagnostics);
        return None;
    }
    if pending_work {
        diagnostics.note("cannot switch sessions while work is pending; wait for it to finish");
        return None;
    }
    let mut source =
        crate::catalogue::ResumePicker::new(Arc::clone(store), root.to_owned(), Some(session_id));
    if !matches!(command, repl::HostCommand::Resume(_)) {
        // The explicit cross-workspace browsers retain their scope.
        source.root = None;
    }
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
        source.matching = Some(hits.into_iter().map(|hit| hit.id).collect());
    }
    let meta = if let repl::HostCommand::Resume(Some(id)) = &command {
        let id = match id.parse() {
            Ok(id) => id,
            Err(_) => {
                diagnostics.note("invalid session ID");
                return None;
            }
        };
        match store.readable_meta(&id) {
            Ok(Some(meta)) => meta,
            Ok(None) => return None,
            Err(error) => {
                diagnostics.note(&format!("the selected session could not be read: {error}"));
                return None;
            }
        }
    } else {
        match seat.pick_session(source) {
            crate::catalogue::SessionPick::Selected(meta) => *meta,
            crate::catalogue::SessionPick::Cancelled => return None,
            crate::catalogue::SessionPick::Quit => return Some(EpochTransition::Exit),
        }
    };
    if !meta.workspace_root.is_dir() {
        diagnostics.note("the selected session's workspace directory is unavailable");
        return None;
    }
    open_selected_session(seat, store, meta, clock, diagnostics)
}

fn report_saved_sessions(store: &JsonlStore, root: &Path, diagnostics: &dyn Diagnostics) {
    match store
        .resume_listing()
        .map(|mut listing| listing.next_page(store, Some(root), None, 10, |_| true))
    {
        Ok(metas) => diagnostics.note(&crate::catalogue::table(&metas, false).join("\n")),
        Err(error) => diagnostics.note(&format!(
            "the saved session list could not be read: {error}"
        )),
    }
}

fn open_selected_session<'store>(
    seat: &dyn InteractiveSeat,
    store: &'store Arc<JsonlStore>,
    meta: ego_core::store::session_store::SessionMeta,
    clock: &SystemClock,
    diagnostics: &dyn Diagnostics,
) -> Option<EpochTransition<'store>> {
    let target = meta.id;
    let opened = match session::open_candidate(store.as_ref(), &target, clock) {
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
            "session {target} has unfinished work; continue it with 'ego resume {target}'"
        ));
        return None;
    }
    // Reject terminal activation while the source still owns its PID binding and
    // writer lease. Full resume admission happens only after this seat accepts.
    if !seat.activate_session(target) {
        diagnostics.note("the terminal could not activate the selected session");
        return None;
    }
    // A picker switch must rebuild the target catalogue through normal admission,
    // even in the same workspace. Reusing this epoch would journal the source tools.
    drop(opened);
    Some(EpochTransition::Resume { meta })
}

async fn new_session_transition<'store>(
    seat: &dyn InteractiveSeat,
    pending_work: bool,
    store: &'store Arc<JsonlStore>,
    clock: &SystemClock,
    diagnostics: &dyn Diagnostics,
) -> Option<EpochTransition<'store>> {
    if pending_work {
        diagnostics.note("cannot start a new session while work is pending; wait for it to finish");
        return None;
    }
    let target = SessionId::new();
    let target_opened = match session::open_candidate(store.as_ref(), &target, clock) {
        Ok(opened) => opened,
        Err(error) => {
            diagnostics.note(&format!("the new session could not be opened: {error}"));
            return None;
        }
    };
    let mut fresh_state = SessionState::default();
    // No conversation has been sent in this session yet. The provider's
    // actual prompt occupancy remains unknown until the first generation.
    fresh_state.context.used_tokens = Some(0);
    fresh_state.context.provenance =
        Some(ego_core::event::domain::OccupancyProvenance::LocalEstimate);
    let target_owner = match ActiveSession::durable(target_opened, fresh_state, EVIDENCE_CAP) {
        Ok(owner) => owner,
        Err(error) => {
            diagnostics.note(&format!("the new session log could not be opened: {error}"));
            return None;
        }
    };
    if !seat.activate_session(target) {
        diagnostics.note("the terminal could not activate the new session");
        return None;
    }
    Some(EpochTransition::New {
        target: target_owner,
        replay: false,
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
        diagnostics.note("cannot fork the session while work is pending; wait for it to finish");
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
            diagnostics.note(&format!("the forked session could not be saved: {error}"));
            return None;
        }
    };
    let target_opened = match session::open_candidate(store.as_ref(), &target, clock) {
        Ok(opened) => opened,
        Err(error) => {
            diagnostics.note(&format!(
                "the forked session {target} was saved but could not be opened: {error}"
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
                "the forked session {target} was saved but its history could not be loaded: {error}"
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

#[expect(
    clippy::too_many_arguments,
    reason = "interactive input needs its admitted exec environment and turn controls"
)]
async fn drive_interactive_input<'session: 'turn, 'turn>(
    turns: &Turns<'_>,
    loop_: &mut TurnLoop<'session, 'turn>,
    resumed_turn: Option<RuntimeResume>,
    asked: Option<InteractiveTurnInput>,
    images: Vec<ImageRef>,
    exec_environment: ego_core::permit::EnvPolicy,
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
            Some(InteractiveTurnInput::Prompt { text: asked, .. }) => {
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
            Some(InteractiveTurnInput::Shell(command)) => Some(
                turns
                    .drive_user_shell(loop_, command, exec_environment, turn_cancel, diagnostics)
                    .await,
            ),
            Some(InteractiveTurnInput::Skill(operation)) => {
                let (termination, result) = turns
                    .drive_admitted_skill(loop_, operation, turn_cancel, diagnostics)
                    .await;
                Some((termination, result))
            }
            None => drive_queued_interactive_input(loop_).await,
        },
    }
}

async fn drive_queued_interactive_input<'session: 'turn, 'turn>(
    loop_: &mut TurnLoop<'session, 'turn>,
) -> Option<(RunTermination, Value)> {
    match loop_.run_queued_turn().await {
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
                message: format!("the session log could not be written: {}", abandoned.cause),
            },
            Value::String(last_assistant_message(&loop_.state())),
        )),
    }
}

struct EpochCapabilities<'a> {
    registry: &'a [RegisteredTool],
    system: Option<&'a SystemPrompt>,
    has_schema: bool,
    hooks_off: bool,
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
        hooks_off,
        mcp_errors,
    } = capabilities_input;
    match epoch_start {
        EpochStart::Initial => false,
        EpochStart::New { .. } => {
            sink.publish(
                &PublicEvent::SessionStarted {
                    v: PUBLIC_EVENT_VERSION,
                    session: session_id,
                    capabilities: capabilities(registry, system.is_some(), has_schema, hooks_off),
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
                    "forked session {} is available, but saving it to disk could not be confirmed: {diagnostic}",
                    session_id
                ));
            }
            sink.publish(
                &PublicEvent::SessionStarted {
                    v: PUBLIC_EVENT_VERSION,
                    session: session_id,
                    capabilities: capabilities(registry, system.is_some(), has_schema, hooks_off),
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
                    "the compacted session is available, but saving it to disk could not be confirmed: {diagnostic}"
                ));
            }
            sink.publish(
                &PublicEvent::SessionStarted {
                    v: PUBLIC_EVENT_VERSION,
                    session: session_id,
                    capabilities: capabilities(registry, system.is_some(), has_schema, hooks_off),
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

fn report_policy_notice(
    notice: String,
    status: Option<&InteractiveStatus>,
    diagnostics: &dyn Diagnostics,
) {
    if let Some(status) = status
        && status.human
    {
        status.seat.set_notice(notice);
    } else {
        diagnostics.note(&notice);
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
        report_policy_notice(notice, status, diagnostics);
        return false;
    };
    if matches!(
        request,
        PolicyControlRequest::SetMode(_) | PolicyControlRequest::CycleMode
    ) {
        let notice = apply_interactive_mode(owner, request, status);
        report_policy_notice(notice, status, diagnostics);
        return false;
    }
    let Some(output) = text_output else {
        let notice = "interactive permission output is unavailable".to_owned();
        report_policy_notice(notice, status, diagnostics);
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
            report_policy_notice(notice, status, diagnostics);
        }
    }

    false
}

fn apply_interactive_mode(
    owner: &PolicyApplicationOwner,
    request: PolicyControlRequest,
    status: Option<&InteractiveStatus>,
) -> String {
    match owner
        .apply(request)
        .and_then(|outcome| owner.selected_mode().map(|mode| (mode, outcome)))
    {
        Ok((mode, PolicyControlOutcome::Mode { staging })) => {
            if let Some(status) = status.as_ref() {
                status.seat.set_selected_mode(mode);
            }
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
    }
}

fn render_tool_catalogue(
    registry: &[RegisteredTool],
    mcp_registry: &McpRegistryController,
    details: bool,
    diagnostics: &dyn Diagnostics,
) {
    let mut tools = registry
        .iter()
        .map(|tool| tool.descriptor().name.to_string())
        .collect::<Vec<_>>();
    // The next provider request carries the admitted MCP tools too.
    match mcp_registry.capture() {
        Ok(capture) => tools.extend(
            capture
                .tools()
                .iter()
                .map(|tool| ego_core::permit::Tool::descriptor(&**tool).name.to_string()),
        ),
        Err(error) => diagnostics.note(&format!("MCP tools are not listed: {error}")),
    }
    diagnostics.note(&format!(
        "tools: {} available; inspect the full list with '/tools details'",
        tools.len()
    ));
    if details {
        let mut table = vec![vec!["Tool".into()]];
        table.extend(tools.iter().map(|name| vec![name.clone()]));
        diagnostics
            .note(&crate::render::table::plain_table_with_width(table, usize::MAX).join("\n"));
    }
}

fn render_skill_catalogue(
    catalogue: Option<&crate::skill::SkillCatalogue>,
    diagnose: bool,
    details: bool,
    diagnostics: &dyn Diagnostics,
) {
    let Some(catalogue) = catalogue else {
        diagnostics.note("the skill list is unavailable");
        return;
    };
    match catalogue.capture() {
        Ok(capture) if !diagnose && !details => diagnostics.notice_block(&capture.render_summary(
            diagnostics.terminal_capabilities(),
            crate::skill::light_skill_palette(),
        )),
        Ok(capture) => diagnostics.note(&if diagnose {
            capture.render_diagnostics()
        } else {
            capture.render_list()
        }),
        Err(error) => diagnostics.note(&format!("the skill list could not be read: {error}")),
    }
}

fn render_session_hooks(
    details: bool,
    hook_state: &SessionHookState<'_>,
    diagnostics: &dyn Diagnostics,
) {
    match hook_state.catalogue_snapshot() {
        Ok(catalogue) => {
            let rendered = if details {
                hooks::render_details(&catalogue)
            } else {
                hooks::render_summary(&catalogue)
            };
            if rendered.is_empty() {
                diagnostics.note(&crate::output::empty_listing(
                    hooks::EMPTY_HOOKS,
                    "ego config edit",
                ));
            } else {
                diagnostics.note(&rendered);
            }
        }
        Err(error) => diagnostics.note(&format!("the hook list could not be read: {error}")),
    }
}

async fn switch_session_model(
    request: ModelSwitchRequest,
    effective: &Effective,
    model_switch_owner: Option<RunModelSwitchOwner<'_>>,
    seat: &dyn InteractiveSeat,
    status: Option<&InteractiveStatus>,
    state: &SessionState,
    diagnostics: &dyn Diagnostics,
) {
    if model_switch_owner.is_some() {
        seat.busy();
    }
    apply_model_switch(
        request,
        effective,
        model_switch_owner.as_ref(),
        status,
        || state.clone(),
        diagnostics,
    )
    .await;
}

/// The one model-switch lifecycle for idle and busy ingress: the owner orders
/// account checks, PreModelSwitch, saved defaults, durable install and
/// PostModelSwitch. `state` is read after the await, so a busy switch never
/// repaints the status from a pre-await journal.
async fn apply_model_switch(
    request: ModelSwitchRequest,
    effective: &Effective,
    owner: Option<&RunModelSwitchOwner<'_>>,
    status: Option<&InteractiveStatus>,
    state: impl FnOnce() -> SessionState,
    diagnostics: &dyn Diagnostics,
) {
    let Some(owner) = owner else {
        diagnostics.note("model switching is not available in this session");
        return;
    };
    let receipt = format!(
        "Model: {}{}",
        request.target().0,
        if request.is_persistent() {
            ""
        } else {
            " (this session only)"
        }
    );
    let applied = owner.apply(request, effective).await;
    let publication = match &applied {
        Ok(publication) => publication.as_ref(),
        Err(error) => error.publication(),
    };
    if let Some(UserConfigPublication::PublishedDurabilityUncertain { diagnostic }) = publication {
        diagnostics.note(&format!(
            "model defaults have changed, but saving them to disk could not be confirmed: {diagnostic}"
        ));
    }
    match applied {
        Ok(_) => diagnostics.note(&receipt),
        Err(error) => diagnostics.note(&format!("model switch failed: {error}")),
    }
    if let Some(status) = status
        && status.human
    {
        match owner.selection_snapshot() {
            Ok(selected) => status.seat.refresh_status(&state(), selected),
            Err(error) => diagnostics.note(&format!(
                "the status display could not be updated after switching models: {error}"
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
    width: usize,
    diagnostics: &dyn Diagnostics,
) {
    let metadata = match store.readable_meta(&session_id) {
        Ok(metadata) => metadata.filter(|meta| meta.workspace_root == root),
        Err(error) => {
            diagnostics.note(&format!(
                "the saved session list could not be read: {error}"
            ));
            return;
        }
    };
    let Some(meta) = metadata else {
        diagnostics.note("the current session is missing from the saved session list");
        return;
    };
    match UsageReportService::new(store, clock)
        .report(std::slice::from_ref(&meta), UsagePeriod::All)
        .await
    {
        Ok(report) => {
            // `/usage` has no flags, so it
            // gets the compact default.
            let rendered = crate::usage::render_interactive(&report, context, width);
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
    root_grants: Option<Arc<dyn ego_core::turn::RootGrantPort>>,
    judge_available: bool,
    confinement: &'a RunConfinement,
    skill_trust: Trust,
    active: &'session ActiveSession<'session>,
    hooks: &'a SessionHookComposition<'session>,
    journal: &'a SharedJournal<'session>,
    skill_owner: Arc<CliSkillInvocationOwner<'session>>,
    mcp_task_port: Option<Arc<dyn ego_core::mcp::McpTaskPort>>,
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
        root_grants,
        judge_available,
        confinement,
        skill_trust,
        active,
        hooks,
        journal,
        skill_owner,
        mcp_task_port,
    } = input;
    let (ports, cancel, observation, probe_evidence) = effects;
    let http = source.provider_http(Arc::clone(ports));
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
        &active.checkpoint_fs,
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
    let tools = tools
        .with_named_agents(
            crate::named_agents::discover(cwd, skill_trust)
                .map_err(|error| RunTermination::UsageError { message: error.0 })?,
        )
        .with_child_defaults(effective.task.clone());
    let tools = match McpRuntimeBinding::resume_sealer(mcp_runtime) {
        Some(resume) => tools.with_resume(resume).with_mcp_batch(true),
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
    .with_skill_review(effective.skill_safety_review && !args.no_skill_safety_check)
    .with_context_policy(args.context_clear, args.context_interval)
    .with_goal(args.goal.clone(), args.goal_turns)
    .with_goal_policy(effective.ceilings.clone(), args.goal_check_command.clone())
    .with_external_reprompt(super::managed_by_tuic())
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
        .with_skill_invocation_port(skill_owner.clone())
        .with_background_task_port(skill_owner)
        .with_task_guidance()
        .with_orchestration_guidance();
    let loop_ = match mcp_task_port {
        Some(port) => loop_.with_mcp_task_port(port),
        None => loop_,
    };
    let loop_ = match active.replayed() {
        Some(replayed) => loop_.with_replayed_context(replayed),
        None => loop_,
    };
    let loop_ = match elicitor.as_ref() {
        Some(elicitor) => loop_.with_elicitor(Arc::clone(elicitor)),
        None => loop_,
    };
    let loop_ = match root_grants {
        Some(grants) => loop_.with_root_grants(grants),
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
    let fs = &active.checkpoint_fs;

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
        super::composition::selected_tools(args, effective),
        target_skills
            .as_ref()
            .map(crate::skill::SkillCatalogueCapture::core),
        effective
            .active_profile
            .as_ref()
            .map(|profile| profile.name.as_str()),
        Some(Arc::new(jsonl_store.history())),
        effective
            .exec_policy()
            .map_err(|error| CompactionError::Internal(error.to_string()))?,
        args.model
            .as_deref()
            .or_else(|| effective.model.as_ref().map(|model| model.0.as_str())),
        super::composition::opt_in_tools(effective, args, root),
    )
    .map_err(|error| CompactionError::Internal(error.to_string()))?;
    if !args.no_context_tool && super::composition::selected_tools(args, effective).is_none() {
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
        None,
    )
    .await
    .map_err(compaction_termination)?;
    let http = source.provider_http(Arc::clone(&ports));
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
        fs,
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
    .with_provider_recovery(
        args.provider_recovery
            .then_some(ego_core::turn::PROVIDER_RECOVERY_HORIZON),
    )
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
    seat.refresh_queue(state);
    if !state.unacknowledged_prompts().is_empty()
        || state.pending_question().is_some()
        || !state.live_attempts.is_empty()
        || (model_compaction && state.detached_execs.values().any(Option::is_none))
    {
        return Err(CompactionError::Internal(
            "compaction stopped before creating a new session because more work arrived".to_owned(),
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
            if !published {
                diagnostics.note(&format!(
                    "skills: retained last-good generation {}",
                    capture.snapshot_digest()
                ));
                diagnostics.note(&capture.render_admission_errors());
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
                             To return to the original session: 'ego resume {parent}' ({})",
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
        &Path,
    ),
    diagnostics: &dyn Diagnostics,
) -> Result<Option<Vec<RegisteredTool>>, RunTermination> {
    let (args, effective, jsonl_store, exec_environment, root) = inputs;

    if pending_work {
        diagnostics.note("cannot rescan skills while work is pending; wait for it to finish");
        return Ok(None);
    }
    let Some(catalogue) = skills else {
        diagnostics.note("the skill list is unavailable");
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
    let next_registry = rescanned_skill_tools(
        &prospective,
        args,
        effective,
        jsonl_store,
        exec_environment,
        root,
    )?;
    let capture = match catalogue.commit_rescan(prepared) {
        Ok(capture) => capture,
        Err(error) => {
            return Err(RunTermination::Failed {
                message: format!("the prepared skill catalogue could not be published: {error}"),
            });
        }
    };
    let mut registry = next_registry;
    register_context_tool(args, effective, &mut registry);
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
        diagnostics.note(&capture.render_admission_errors());
    }
    Ok(Some(registry))
}

fn rescanned_skill_tools(
    capture: &crate::skill::SkillCatalogueCapture,
    args: &Args,
    effective: &Effective,
    jsonl_store: &Arc<JsonlStore>,
    exec_environment: ego_core::permit::EnvPolicy,
    root: &Path,
) -> Result<Vec<RegisteredTool>, RunTermination> {
    let registry = match super::composition::tools_with_mode(
        super::composition::selected_tools(args, effective),
        Some(capture.core()),
        effective
            .active_profile
            .as_ref()
            .map(|profile| profile.name.as_str()),
        Some(Arc::new(jsonl_store.history())),
        exec_environment,
        args.model
            .as_deref()
            .or_else(|| effective.model.as_ref().map(|model| model.0.as_str())),
        super::composition::opt_in_tools(effective, args, root),
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
    Ok(registry)
}

enum CommandOutcome<'store> {
    Continue,
    Input(InteractiveTurnInput),
    Transition(Box<EpochTransition<'store>>),
}

struct InteractiveCommand<'a, 'session, 'store> {
    args: &'a Args,
    active: &'session ActiveSession<'store>,
    pending_attachments: &'a mut Vec<String>,
    seat: &'a Arc<dyn InteractiveSeat>,
    source: &'a Arc<model::CliGenerationModelSource>,
    confinement: &'a RunConfinement,
    run_deadline: Option<std::time::Instant>,
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
    /// The session's live MCP tasks, when `mcp_tasks` is on.
    mcp_task_port: Option<&'a Arc<dyn ego_core::mcp::McpTaskPort>>,
    session_evidence: &'a ego_tools::store::SessionEvidenceStore,
    task_owner: &'a CliSkillInvocationOwner<'session>,
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
    posture: CommandPosture,
) -> Result<CommandOutcome<'store>, RunTermination> {
    if let repl::Line::HostCommand(repl::HostCommand::Config(arguments)) = &line
        && arguments == "roots"
    {
        input
            .diagnostics
            .note(&root_revocation::listing(input.state));
        return Ok(CommandOutcome::Continue);
    }
    if let repl::Line::HostCommand(repl::HostCommand::Config(arguments)) = &line
        && arguments.starts_with("roots revoke ")
    {
        return match root_revocation::requested(
            arguments,
            input.seat.as_ref(),
            input.state,
            input.cwd,
        ) {
            Ok(path) => Ok(CommandOutcome::Transition(Box::new(
                EpochTransition::RevokeRoot { path },
            ))),
            Err(error) => {
                input.diagnostics.note(&error);
                Ok(CommandOutcome::Continue)
            }
        };
    }
    if let repl::Line::HostCommand(repl::HostCommand::ModelSwitch(request)) = &line
        && input.selection.snapshot().is_ok_and(|current| {
            model_route(&current.identity.0) != model_route(&request.target().0)
        })
    {
        return Ok(match prepare_model_fork(&input, request, posture.0).await {
            Ok(transition) => CommandOutcome::Transition(Box::new(transition)),
            Err(message) => {
                input.diagnostics.note(&message);
                CommandOutcome::Continue
            }
        });
    }
    match line {
        repl::Line::HostCommand(
            repl::HostCommand::Images(_)
            | repl::HostCommand::RemovePasteImage
            | repl::HostCommand::ClipboardPaste
            | repl::HostCommand::PastePath(_),
        ) => dispatch_image_command(input, line).await,
        repl::Line::HostCommand(
            repl::HostCommand::Fast(_)
            | repl::HostCommand::ModelSwitch(_)
            | repl::HostCommand::Policy(_),
        ) => dispatch_policy_command(input, line).await,
        repl::Line::HostCommand(
            repl::HostCommand::Init(_) | repl::HostCommand::Config(_) | repl::HostCommand::Setup(_),
        ) => dispatch_workspace_command(input, line).await,
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Compact)) => {
            compact_session_command(input, posture).await
        }
        _ => dispatch_session_command(input, line, posture).await,
    }
}

fn model_route(slug: &str) -> &str {
    slug.split_once('/').map_or(slug, |(route, _)| route)
}

async fn prepare_model_fork<'store>(
    input: &InteractiveCommand<'_, '_, 'store>,
    request: &ModelSwitchRequest,
    pending_work: bool,
) -> Result<EpochTransition<'store>, String> {
    if pending_work || input.control.status().queued > 0 {
        return Err("wait for queued prompts and pending work before switching providers".into());
    }
    let target = request.target();
    let route = model_route(&target.0);
    let effective = input.effective;
    let instance = effective
        .providers
        .instances
        .iter()
        .find(|(id, _)| id.as_str() == route)
        .map(|(_, instance)| &instance.value);
    let mut saved_account = None;
    let connection = if let Some(instance) = instance {
        if request.connection().is_some() {
            return Err("the selected provider instance owns its credential reference".into());
        }
        instance
            .credential_ref
            .as_ref()
            .map(|id| id.as_str().to_owned())
    } else if let Some(id) = request.connection() {
        let home =
            crate::config::ego_home().ok_or("user configuration directory is unavailable")?;
        let accounts =
            crate::auth::saved_key_accounts_at(&home).map_err(|error| error.to_string())?;
        let account = accounts
            .into_iter()
            .find(|account| account.id == id && account.route == route)
            .ok_or("the selected account is no longer available; reopen /model")?;
        saved_account = Some(account.config);
        Some(account.id)
    } else {
        let selected = ego_core::config::select_connection(
            &effective.connections,
            None,
            Some(crate::auth::connection_vendor(route)),
        )?;
        if let Some((_, account)) = selected
            && crate::auth::connection_route(account) != route
        {
            return Err(
                "the saved account belongs to another route; reopen /model and select its model"
                    .into(),
            );
        }
        selected.map(|(id, _)| id.to_owned())
    };
    let destination = ProviderDestination::new(effective, route, connection, saved_account)?;
    let state = input
        .session_journal
        .inspect(Clone::clone)
        .map_err(|error| error.to_string())?;
    let source = input.active.id();
    let status = input
        .status
        .as_ref()
        .filter(|status| status.human)
        .ok_or("provider switching needs an interactive terminal")?;
    refuse_pending_provider_switch(input, &state)?;
    let fresh = state.turns.is_empty() && state.lineage.is_none();
    if !fresh {
        let consent = status.seat.confirm_setup(format!(
            "Fork to {} using {}? Your next prompt will share conversation history and tool results with that provider. Private provider reasoning stays in the original session. [y/N] ",
            target.0,
            destination.description(),
        ));
        if !matches!(consent, repl::Confirmation::Confirmed) {
            return Err("provider switch cancelled; the source is unchanged".into());
        }
    }
    let (request_id, _) = input
        .model_switch_owner
        .as_ref()
        .ok_or("model switching is not available in this session")?
        .before_switch(&target)
        .await
        .map_err(|error| error.to_string())?;
    // Hooks and confirmation can wait. Recheck live work before ending the epoch.
    let state = input
        .session_journal
        .inspect(Clone::clone)
        .map_err(|error| error.to_string())?;
    refuse_pending_provider_switch(input, &state)?;
    let (id, fork) = if fresh {
        (source, None)
    } else {
        let selections = RunSelections::empty()
            .with_model_and_thinking(target, request.thinking())
            .with_connection(destination.connection.clone());
        let prepared = ego_core::session::PreparedFork::neutral(
            source,
            &state,
            session::now(input.clock),
            selections,
        )
        .map_err(|error| format!("cannot switch providers: {error}"))?;
        (SessionId::new(), Some(prepared))
    };
    Ok(EpochTransition::SwitchProvider(Box::new(ProviderSwitch {
        source,
        id,
        request: request.clone(),
        request_id,
        destination,
        fork,
    })))
}

fn refuse_pending_provider_switch(
    input: &InteractiveCommand<'_, '_, '_>,
    state: &SessionState,
) -> Result<(), String> {
    if interactive_work_pending(input.control, state)
        || !input.pending_attachments.is_empty()
        || state.detached_execs.values().any(Option::is_none)
        || input.task_owner.has_unsettled_tasks()?
        || input.task_owner.has_ready_tasks()?
        || !input
            .task_owner
            .background
            .lock()
            .map_err(|_| "background skill state unavailable")?
            .is_empty()
        || input
            .mcp_task_port
            .is_some_and(|port| port.has_pending_work())
    {
        return Err("wait for queued prompts or pending work before switching providers".into());
    }
    Ok(())
}

async fn complete_provider_switch(
    change: &ProviderSwitch,
    hooks: &SessionHookComposition<'_>,
    status: Option<&InteractiveStatus>,
    diagnostics: &dyn Diagnostics,
) {
    if change.request.is_persistent()
        && let Some(store) = status.and_then(|status| status.model_config_store.as_ref())
    {
        match crate::onboarding::persist_startup_selection(store.as_ref(), &change.request) {
            Ok(UserConfigPublication::PublishedDurabilityUncertain { diagnostic }) => {
                diagnostics.note(&diagnostic)
            }
            Ok(_) => {}
            Err(error) => diagnostics.note(&format!("model default was not saved: {error}")),
        }
    }
    let target = change.request.target();
    let post = model_switch_occurrence(
        HookLifecycle::PostModelSwitch,
        change.request_id,
        &target,
        HookCausality::request(change.request_id, TurnId::new(), None),
    );
    let result = match post {
        Ok(post) => hooks
            .runtime
            .dispatch(Producer::Main, post)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    if let Err(error) = result {
        diagnostics.note(&format!("the post-model-switch observer failed: {error}"));
    }
    if change.id == change.source {
        diagnostics.note(&format!("Model: {}", target.0));
    } else {
        diagnostics.note(&format!(
            "Forked to {} on {}; original: 'ego resume {}'",
            change.id, target.0, change.source
        ));
        diagnostics.note(&format!("resumed session {}", change.id));
    }
}

async fn dispatch_image_command<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        active,
        pending_attachments,
        seat,
        source,
        confinement,
        run_deadline,
        cwd,
        ..
    } = input;
    match line {
        repl::Line::HostCommand(repl::HostCommand::Images(request)) => {
            let admitted = matches!(request.operation(), repl::ImageOperation::Admit);
            if answer_image_request(
                &request,
                seat.as_ref(),
                source,
                (confinement, active.evidence(), run_deadline),
                cwd,
                pending_attachments,
            )
            .await
                && admitted
            {
                pending_attachments.clear();
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::RemovePasteImage) => {
            if let Some(draft) = seat.image_draft() {
                draft.pop();
                seat.set_image_chips(draft.labels());
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::ClipboardPaste) => {
            if let Some(draft) = seat.image_draft() {
                if let Err(error) = stage_clipboard_image(
                    seat.as_ref(),
                    source,
                    (confinement, active.evidence(), run_deadline),
                    &draft,
                )
                .await
                {
                    seat.set_notice(error);
                }
                seat.set_image_chips(draft.labels());
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::PastePath(path)) => {
            if let Some(draft) = seat.image_draft() {
                if let Err(error) = stage_image_path(
                    path,
                    source,
                    (confinement, active.evidence(), run_deadline),
                    &draft,
                )
                .await
                {
                    seat.set_notice(error);
                }
                seat.set_image_chips(draft.labels());
            }
            Ok(CommandOutcome::Continue)
        }
        _ => unreachable!("only image commands are dispatched here"),
    }
}

async fn dispatch_policy_command<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        seat,
        state,
        diagnostics,
        status,
        selection,
        effective,
        model_switch_owner,
        policy_application,
        text_output,
        ..
    } = input;
    match line {
        repl::Line::HostCommand(repl::HostCommand::Fast(enabled)) => {
            match selection.set_fast(enabled) {
                Ok(receipt) => {
                    diagnostics.note(&receipt);
                    if let Some(status) = status.as_ref()
                        && status.human
                        && let Ok(selected) = selection.snapshot()
                    {
                        status.seat.refresh_status(state, selected);
                    }
                }
                Err(error) => diagnostics.note(&error.to_string()),
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::ModelSwitch(request)) => {
            switch_session_model(
                request,
                effective,
                model_switch_owner.clone(),
                seat.as_ref(),
                status.as_ref(),
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
        _ => unreachable!("only policy commands are dispatched here"),
    }
}

fn confirm_workspace_setup(status: Option<&InteractiveStatus>, preview: String) -> bool {
    status.filter(|status| status.human).is_some_and(|status| {
        matches!(
            status.seat.confirm_setup(preview),
            repl::Confirmation::Confirmed
        )
    })
}

async fn dispatch_workspace_command<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        diagnostics,
        status,
        effective,
        skill_profile_layers,
        perimeter,
        root,
        skill_trust,
        ..
    } = input;
    match line {
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
        repl::Line::HostCommand(repl::HostCommand::Init(arguments)) => {
            if !arguments.is_empty() {
                diagnostics
                    .note("usage: /init (review a deterministic project instruction proposal)");
            } else {
                let admitted = crate::instruction_init::init_admitted(effective, root, skill_trust);
                let confirm = |preview: String| confirm_workspace_setup(status.as_ref(), preview);
                let result = if admitted {
                    crate::instruction_init::InitProposal::prepare(root)
                        .and_then(|proposal| proposal.apply(admitted, &confirm))
                } else {
                    Err("instruction initialization denied by effective write policy or workspace trust".into())
                };
                match result {
                    Ok(report) => diagnostics.note(&report),
                    Err(error) => diagnostics.note(&format!("Init: {error}")),
                }
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Setup(prompt)) => {
            run_interactive_setup(prompt, root, status.as_ref(), effective, diagnostics).await;
            Ok(CommandOutcome::Continue)
        }
        _ => unreachable!("only workspace commands are dispatched here"),
    }
}

async fn dispatch_session_command<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
    posture: CommandPosture,
) -> Result<CommandOutcome<'store>, RunTermination> {
    match line {
        repl::Line::HostCommand(
            repl::HostCommand::SubmittedDraft { .. }
            | repl::HostCommand::AdmitPrompt { .. }
            | repl::HostCommand::Shell(_)
            | repl::HostCommand::BackgroundExecSettled,
        ) => dispatch_session_input(input, line).await,
        repl::Line::HostCommand(
            repl::HostCommand::Pins(_)
            | repl::HostCommand::SessionMetadata(_)
            | repl::HostCommand::McpContext(_)
            | repl::HostCommand::Skill(_)
            | repl::HostCommand::Skills(_)
            | repl::HostCommand::Hooks
            | repl::HostCommand::Details(repl::actions::ActionId::Hooks)
            | repl::HostCommand::Tasks(_)
            | repl::HostCommand::SideQuestion(_),
        ) => dispatch_session_resources(input, line, posture).await,
        repl::Line::HostCommand(
            repl::HostCommand::Action(
                repl::actions::ActionId::Help
                | repl::actions::ActionId::Expand
                | repl::actions::ActionId::Config
                | repl::actions::ActionId::Tools
                | repl::actions::ActionId::Doctor
                | repl::actions::ActionId::Usage
                | repl::actions::ActionId::Context
                | repl::actions::ActionId::Refresh,
            )
            | repl::HostCommand::Details(
                repl::actions::ActionId::Help
                | repl::actions::ActionId::Tools
                | repl::actions::ActionId::Doctor,
            )
            | repl::HostCommand::Mcp(_)
            | repl::HostCommand::Login(_)
            | repl::HostCommand::Export(_),
        ) => dispatch_session_inspection(input, line, posture).await,
        repl::Line::HostCommand(_) | repl::Line::ExternalBreak(_) => {
            dispatch_session_navigation(input, line, posture).await
        }
        _ => dispatch_session_input(input, line).await,
    }
}

async fn dispatch_session_input<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        seat,
        last,
        state,
        diagnostics,
        session_hook_ports,
        task_owner,
        mcp_task_port,
        session_evidence,
        elicitor,
        ..
    } = input;
    match line {
        repl::Line::Submitted(text) => Ok(CommandOutcome::Input(InteractiveTurnInput::Prompt {
            text,
            images: None,
        })),
        repl::Line::HostCommand(repl::HostCommand::Shell(command)) => {
            Ok(CommandOutcome::Input(InteractiveTurnInput::Shell(command)))
        }
        repl::Line::HostCommand(repl::HostCommand::SubmittedDraft { text, images }) => {
            Ok(CommandOutcome::Input(InteractiveTurnInput::Prompt {
                text,
                images: Some(images),
            }))
        }
        repl::Line::HostCommand(repl::HostCommand::BackgroundExecSettled) => {
            // A stale/coalesced wake must not buy another generation. The
            // generation boundary settles every ready process and task in the journal.
            // Queued monitor lines are ready context too.
            let ready = state.unsettled_detached_execs().into_iter().any(|id| {
                if state.stopped_background_execs.contains(&id) {
                    return false;
                }
                let process = session_hook_ports.process();
                process.background_output_ready(id)
                    || process
                        .background_status(id)
                        .is_ok_and(|snapshot| snapshot.outcome.is_some())
            });
            let task_ready = task_owner
                .has_ready_tasks()
                .map_err(|message| RunTermination::Failed { message })?;
            // A wake of the MCP task timer: poll the due tasks here, so a task
            // that is still working costs no generation.
            let mcp_ready = match mcp_task_port {
                Some(port) => {
                    let fs = OsFs;
                    let effects = ToolEffectPorts::new(
                        &fs,
                        session_hook_ports.process(),
                        session_evidence,
                        session_hook_ports.resolver_handle(),
                        session_hook_ports.one_hop_handle(),
                        session_hook_ports.duplex_handle(),
                    );
                    let poll_failure = |message| RunTermination::Failed { message };
                    let mut settled = port.poll_due(effects.clone()).await.map_err(poll_failure)?;
                    // Deliver ready results first. While a form is visible, the
                    // same session may still poll its other admitted tasks.
                    if !ready
                        && !task_ready
                        && !settled
                        && let Some(elicitor) = elicitor
                    {
                        let mut answer = port.answer_pending(effects.clone(), elicitor.as_ref());
                        let interval = std::time::Duration::from_millis(500);
                        let mut poll = tokio::time::interval_at(
                            tokio::time::Instant::now() + interval,
                            interval,
                        );
                        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        loop {
                            tokio::select! {
                                result = &mut answer => {
                                    settled |= result.map_err(poll_failure)?;
                                    break;
                                }
                                _ = poll.tick() => {
                                    let mut polling = port.poll_due(effects.clone());
                                    // Keep the form deadline live during network I/O. If
                                    // the form settles first, still drain the admitted poll.
                                    tokio::select! {
                                        result = &mut answer => {
                                            let polled = polling.await;
                                            settled |= result.map_err(poll_failure)? | polled.map_err(poll_failure)?;
                                            break;
                                        }
                                        result = &mut polling => {
                                            settled |= result.map_err(poll_failure)?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    settled
                }
                None => false,
            };
            Ok(if ready || task_ready || mcp_ready {
                CommandOutcome::Input(InteractiveTurnInput::Checkpoint)
            } else {
                CommandOutcome::Continue
            })
        }
        repl::Line::HostCommand(repl::HostCommand::AdmitPrompt { .. }) => {
            unreachable!("native prompt admission is owned by the terminal")
        }
        repl::Line::Blank => Ok(CommandOutcome::Continue),
        // Ctrl+C at an idle prompt drops the line and nothing
        // else. Someone who pressed it to stop a turn must not
        // lose the session by pressing it once more.
        repl::Line::Interrupted => Ok(CommandOutcome::Continue),
        repl::Line::Ended => {
            // A human closes the session after seeing the turn's refusal.
            // Redirected EOF must preserve that turn's failure for its caller.
            // Preserve cancellation, permission, and other outcomes in both seats.
            if seat.has_human()
                && matches!(
                    &last.termination,
                    RunTermination::Provider { .. }
                        | RunTermination::ProviderUncertain { .. }
                        | RunTermination::UsageError { .. }
                        | RunTermination::ContextOverflow { .. }
                )
            {
                last.termination = RunTermination::Done;
            }
            Ok(CommandOutcome::Transition(Box::new(EpochTransition::Exit)))
        }
        repl::Line::Refused(why) => {
            diagnostics.note(why);
            Ok(CommandOutcome::Continue)
        }
        _ => unreachable!("only matching session commands are dispatched here"),
    }
}

async fn dispatch_session_resources<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
    posture: CommandPosture,
) -> Result<CommandOutcome<'store>, RunTermination> {
    if matches!(
        line,
        repl::Line::HostCommand(repl::HostCommand::Skills(
            repl::actions::SkillSurfaceAction::Rescan
        ))
    ) {
        return rescan_skill_command(input, posture.0);
    }
    let InteractiveCommand {
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
        session_hooks,
        session_hook_ports,
        task_owner,
        ..
    } = input;
    let (pending_work, _, _) = posture;
    match line {
        repl::Line::HostCommand(repl::HostCommand::Pins(command)) => {
            if pending_work && command != ego_core::compact::PinCommand::List {
                diagnostics.note("cannot change pins while work is pending; wait for it to finish");
                return Ok(CommandOutcome::Continue);
            }
            match session_journal.pins(command) {
                Ok(receipt) => diagnostics.note(&receipt),
                Err(error) => diagnostics.note(&format!("pin operation failed: {error}")),
            }
            *state = session_journal
                .inspect(Clone::clone)
                .map_err(hook_journal_termination)?;
            last.state = state.clone();
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
                    .note("cannot use MCP prompts or resources while work is pending; wait for it to finish");
                return Ok(CommandOutcome::Continue);
            }
            Ok(CommandOutcome::Input(InteractiveTurnInput::McpContext(
                command,
            )))
        }
        repl::Line::HostCommand(repl::HostCommand::Skill(operation)) => {
            if pending_work {
                diagnostics.note("cannot run a skill while work is pending; wait for it to finish");
                return Ok(CommandOutcome::Continue);
            }
            Ok(CommandOutcome::Input(InteractiveTurnInput::Skill(
                operation,
            )))
        }
        repl::Line::HostCommand(repl::HostCommand::Skills(
            repl::actions::SkillSurfaceAction::List,
        )) => {
            render_skill_catalogue(skills.as_deref(), false, false, diagnostics);
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Skills(
            repl::actions::SkillSurfaceAction::Details,
        )) => {
            render_skill_catalogue(skills.as_deref(), false, true, diagnostics);
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Skills(
            repl::actions::SkillSurfaceAction::Diagnose,
        )) => {
            render_skill_catalogue(skills.as_deref(), true, false, diagnostics);
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(
            command @ (repl::HostCommand::Hooks
            | repl::HostCommand::Details(repl::actions::ActionId::Hooks)),
        ) => {
            render_session_hooks(
                matches!(command, repl::HostCommand::Details(_)),
                &session_hooks.state,
                diagnostics,
            );
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::SideQuestion(_)) => {
            diagnostics.note(SIDE_QUESTION_IDLE);
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Tasks(arguments)) => {
            ego_core::turn::BackgroundTaskPort::refresh_tasks(
                task_owner,
                session_hooks.state.state(),
            )
            .await
            .map_err(|message| RunTermination::Failed { message })?;
            let current = session_hooks.state.state();
            diagnostics.note(
                &task_listing(
                    &arguments,
                    &current,
                    session_hook_ports.process(),
                    task_owner,
                )
                .await,
            );
            *state = current;
            Ok(CommandOutcome::Continue)
        }
        _ => unreachable!("only matching session commands are dispatched here"),
    }
}

fn rescan_skill_command<'store>(
    input: InteractiveCommand<'_, '_, 'store>,
    pending_work: bool,
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        args,
        seat,
        last,
        skills,
        effective,
        jsonl_store,
        exec_environment,
        registry,
        diagnostics,
        root,
        ..
    } = input;
    match rescan_session_skills(
        pending_work,
        skills.as_deref(),
        seat.as_ref(),
        (args, effective, jsonl_store, exec_environment.clone(), root),
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

fn named_session_login_refusal(pinned: Option<&str>, effective: &Effective) -> Option<String> {
    let id = pinned?;
    Some(match effective.connections.get(id) {
        Some(connection) => format!(
            "Login: this session is pinned to its saved account; {}",
            crate::auth::renewal_hint(&crate::auth::connection_route(connection)),
        ),
        None => "Login: the pinned account is absent from the run configuration; /login cannot create a replacement".to_owned(),
    })
}

/// One owner call for idle and busy login ingress: the pinned-account
/// restriction, then `auth::session_login`. Busy ingress admits only the bare
/// inventory (`provider == None`), which returns before any seat or network.
async fn note_session_login(
    provider: Option<&str>,
    seat: Option<&repl::Seat>,
    selection: &model::GenerationSelection,
    effective: &Effective,
    diagnostics: &dyn crate::output::Diagnostics,
) {
    if let Some(message) = named_session_login_refusal(selection.connection(), effective) {
        diagnostics.note(&message);
        return;
    }
    match crate::auth::session_login(provider, seat, diagnostics).await {
        Ok(message) => diagnostics.note(&message),
        Err(error) => diagnostics.note(&format!("Login: {error}")),
    }
}

async fn dispatch_session_inspection<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
    posture: CommandPosture,
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        args,
        active,
        seat,
        state,
        diagnostics,
        status,
        selection,
        effective,
        jsonl_store,
        registry,
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
        ..
    } = input;
    let (pending_work, _, _) = posture;
    match line {
        repl::Line::HostCommand(repl::HostCommand::Details(repl::actions::ActionId::Help)) => {
            diagnostics.note(&seat.action_help_details());
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Help)) => {
            diagnostics.note(&seat.action_help());
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Context)) => {
            let store = super::composition::memory_store(effective, args, root);
            diagnostics.note(&crate::memory::context_text(
                config.system.as_ref(),
                store.as_deref(),
            ));
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
        repl::Line::HostCommand(repl::HostCommand::Login(provider)) => {
            note_session_login(
                provider.as_deref(),
                status.as_ref().map(|status| status.seat.as_ref()),
                selection,
                effective,
                diagnostics,
            )
            .await;
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Mcp(arguments)) => {
            {
                let controller = mcp_registry;
                let result = crate::mcp::session_command(
                    &arguments,
                    controller,
                    session_mcp,
                    disabled_mcp,
                    status.as_ref().map(|status| status.seat.as_ref()),
                    diagnostics,
                )
                .await;
                if let Some(status) = status.as_ref() {
                    status.seat.set_mcp_session_servers(
                        session_mcp
                            .iter()
                            .map(|(name, _, _)| (name.clone(), disabled_mcp.contains(name)))
                            .collect(),
                    );
                }
                match result {
                    Ok(receipt) => diagnostics.note(&receipt),
                    Err(error) => diagnostics.note(&crate::mcp::session_error(&error)),
                }
            }
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(
            command @ (repl::HostCommand::Action(repl::actions::ActionId::Tools)
            | repl::HostCommand::Details(repl::actions::ActionId::Tools)),
        ) => {
            render_tool_catalogue(
                registry,
                mcp_registry,
                matches!(command, repl::HostCommand::Details(_)),
                diagnostics,
            );
            Ok(CommandOutcome::Continue)
        }
        // The same snapshot `ego doctor` prints,
        // built by the same probes. Two renderings
        // of one value is the whole contract; two
        // implementations would be two diagnoses.
        repl::Line::HostCommand(
            command @ (repl::HostCommand::Action(repl::actions::ActionId::Doctor)
            | repl::HostCommand::Details(repl::actions::ActionId::Doctor)),
        ) => {
            diagnostics.note(&crate::doctor::observed_session_view(
                root.to_path_buf(),
                seat.width(),
                perimeter,
                matches!(command, repl::HostCommand::Details(_)),
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
            let args = export_args_for(active.id(), selection, args);
            for note in export_notes(&args, selection) {
                diagnostics.note(&note);
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
                seat.width(),
                diagnostics,
            )
            .await;
            Ok(CommandOutcome::Continue)
        }
        repl::Line::HostCommand(repl::HostCommand::Action(repl::actions::ActionId::Refresh)) => {
            if pending_work {
                diagnostics.note(
                    "cannot reload instructions while work is pending; wait for it to finish",
                );
                return Ok(CommandOutcome::Continue);
            }
            match instructions(effective, args, root, cwd, diagnostics) {
                Ok(system) => {
                    config.system = system;
                    *refresh_prompt = true;
                    diagnostics
                        .note("instructions refreshed; they will apply to the next model request");
                }
                Err(error) => {
                    diagnostics.note(&format!("instructions were not refreshed: {error}"))
                }
            }
            Ok(CommandOutcome::Continue)
        }
        _ => unreachable!("only matching session commands are dispatched here"),
    }
}

async fn dispatch_session_navigation<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    line: repl::Line,
    posture: CommandPosture,
) -> Result<CommandOutcome<'store>, RunTermination> {
    let InteractiveCommand {
        active,
        seat,
        state,
        diagnostics,
        jsonl_store,
        root,
        clock,
        ..
    } = input;
    let (pending_work, _, _) = posture;
    if !state.session_roots.is_empty() && matches!(line, repl::Line::HostCommand(_)) {
        diagnostics.note(&session_root_recovery(active.id()));
        return Ok(CommandOutcome::Continue);
    }
    match line {
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
        repl::Line::HostCommand(repl::HostCommand::Action(
            action @ (repl::actions::ActionId::New | repl::actions::ActionId::Clear),
        )) => {
            if let Some(transition) =
                new_session_transition(seat.as_ref(), pending_work, jsonl_store, clock, diagnostics)
                    .await
            {
                if action == repl::actions::ActionId::Clear {
                    diagnostics.note(&format!(
                        "Conversation cleared; previous session remains available: ego resume {}",
                        active.id()
                    ));
                }
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
        repl::Line::HostCommand(_) => Ok(CommandOutcome::Continue),
        repl::Line::ExternalBreak(_) => {
            Ok(CommandOutcome::Transition(Box::new(EpochTransition::Exit)))
        }
        _ => unreachable!("only navigation commands are dispatched here"),
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

    if pending_work && !repl::HostCommand::SessionMetadata(operation.clone()).is_concurrent_native()
    {
        diagnostics.note(
            "cannot change the session name or goal while work is pending; wait for it to finish",
        );
        return Ok(CommandOutcome::Continue);
    }
    let goal_prompt = match &operation {
        crate::cli::SessionMetadataOperation::Goal(crate::cli::GoalOperation::Set(text)) => {
            Some(text.clone())
        }
        crate::cli::SessionMetadataOperation::Goal(crate::cli::GoalOperation::Resume) => {
            Some("Resume the existing automatic goal using useful already-authorized actions and verify the outcome.".into())
        }
        _ => None,
    };
    let updated = match crate::metadata::execute_interactive(
        interactive_metadata,
        leased_session_journal,
        session_journal,
        &operation,
        interactive_metadata_ceilings,
        &mut |message| diagnostics.note(message),
    ) {
        Ok(()) => true,
        Err(error) => {
            diagnostics.note(&format!("session metadata operation failed: {error}"));
            false
        }
    };
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
    Ok(match (updated, goal_prompt) {
        (true, Some(text)) => {
            CommandOutcome::Input(InteractiveTurnInput::Prompt { text, images: None })
        }
        _ => CommandOutcome::Continue,
    })
}

async fn compact_session_command<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveCommand<'a, 'session, 'store>,
    posture: CommandPosture,
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
    if !state.session_roots.is_empty() {
        *automatic_compaction_failed = true;
        diagnostics.note(&session_root_recovery(active.id()));
        return Ok(CommandOutcome::Continue);
    }
    if automatic_compaction.is_none() && !seat.confirm_compaction(active.id()) {
        return Ok(CommandOutcome::Continue);
    }
    // Failed or cancelled pressure recovery stays idle until fresh work or an
    // explicit /compact retry. The core owns backoff within each preparation.
    *automatic_compaction_failed = true;
    seat.compacting();
    seat.set_compaction_availability(repl::actions::Availability::BusyWithReason {
        reason: "ego is busy; compaction already running",
    });

    let target_runtime = Arc::new(Mutex::new(None));
    let prepared_runtime = Arc::clone(&target_runtime);
    let source_id = active.id();
    let request_id = RequestId::from_uuid(uuid::Uuid::now_v7());
    if let Err(error) = dispatch_request_hook(
        &session_hooks.runtime,
        request_id,
        session_hooks.turn,
        None,
        HookLifecycle::PreCompact,
    )
    .await
    {
        return Ok(compaction_failure_outcome(
            diagnostics,
            format!(
                "compaction was blocked before preparation: {}",
                error.exit_reason()
            ),
        ));
    }
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
            return Ok(compaction_failure_outcome(
                diagnostics,
                format!("compaction failed: {error}"),
            ));
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
    let target_opened = match session::open_candidate(jsonl_store.as_ref(), &target, clock) {
        Ok(opened) => Opened::Durable(Box::new(opened)),
        Err(error) => {
            return Ok(compaction_failure_outcome(
                diagnostics,
                format!("compaction target could not be opened: {error}"),
            ));
        }
    };
    let target_state = target_opened.state();
    let target_owner = match ActiveSession::durable(target_opened, target_state, EVIDENCE_CAP) {
        Ok(owner) => owner,
        Err(error) => {
            return Ok(compaction_failure_outcome(
                diagnostics,
                format!("compaction target evidence could not be opened: {error}"),
            ));
        }
    };
    let diagnostic = receipt.publication.diagnostic().map(str::to_string);
    if matches!(
        &receipt.publication,
        ego_core::session::TargetPublication::NotPublished { .. }
    ) {
        return Ok(compaction_failure_outcome(
            diagnostics,
            "compaction target was not published".to_owned(),
        ));
    }
    let runtime = target_runtime
        .lock()
        .expect("interactive compaction runtime")
        .take();
    // Only pressure recovery interrupted a task; cache-expiry compaction waits for the operator.
    *checkpoint_pending = automatic_compaction == Some(AutomaticCompaction::Pressure);
    Ok(CommandOutcome::Transition(Box::new(
        EpochTransition::Compact {
            target: target_owner,
            publication: receipt.publication,
            diagnostic,
            runtime,
            request: request_id,
        },
    )))
}

fn session_root_recovery(source: SessionId) -> String {
    format!(
        "Session change paused: session roots cannot cross into another session. The source session is still open: {source}. \
         To keep its grants, continue here; same-ID resume retains these roots and does not unblock the change. \
         For independent recovery, save a factual handoff, settle session-owned background work, then start a fresh session with ego (not ego resume). \
         The fresh session needs its own approvals. Alternatively, inspect /config roots and explicitly revoke every session root with /config roots revoke EXACT_PATH; \
         revocation stops live exec children and removes that access before re-admission. No grants or background work have been changed by this refusal."
    )
}

fn compaction_failure_outcome<'store>(
    diagnostics: &dyn Diagnostics,
    message: String,
) -> CommandOutcome<'store> {
    diagnostics.note(&format!(
        "Compaction paused; the session is still open: {message}. Retry with '/compact'."
    ));
    CommandOutcome::Continue
}

async fn end_session_epoch(
    skill_owner: &CliSkillInvocationOwner<'_>,
    session_hooks: &SessionHookComposition<'_>,
    recovered_epoch_policy: Option<Arc<PolicyEngine>>,
    policy_source: &dyn GenerationPolicySource,
    session_id: SessionId,
) -> Result<(), RunTermination> {
    let drained = skill_owner.drain_tasks().await;
    match drained.and(skill_owner.drain().await) {
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
        source,
        ports,
        cancel: turn_cancel,
        run_deadline,
        cwd,
        blobs,
        seat,
        diagnostics,
    } = input;
    let Some(InteractiveTurnInput::Prompt { images, .. }) = asked else {
        return Ok(Some(Vec::new()));
    };
    let draft = seat.image_draft();
    let mut inputs = match images {
        Some(_) => Vec::new(),
        None => draft.as_ref().map_or_else(Vec::new, image_inputs),
    };
    inputs.extend(
        pending_attachments
            .iter()
            .cloned()
            .map(attachment::AttachmentInput::Spec),
    );
    let mut acquired = match acquire_image_inputs(
        &inputs,
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
        Err(RunTermination::UsageError { message }) => {
            diagnostics.note(&format!(
                "{message}; the turn was not sent, remove an attachment and submit it again"
            ));
            return Ok(None);
        }
        Err(other) => return Err(other),
    };
    if let Some(images) = images {
        acquired.splice(0..0, images.iter().cloned());
    }
    pending_attachments.clear();
    if images.is_none()
        && let Some(draft) = draft
    {
        draft.clear();
        seat.set_image_chips(Vec::new());
    }
    Ok(Some(acquired))
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
        && let Some(output) = text_output
    {
        let environment = crate::render::caps::Environment::read();
        let capabilities = crate::render::Capabilities::of(&crate::render::caps::Terminal {
            flag_plain: args.plain,
            ..crate::render::caps::Terminal::from_process(args.no_color, &environment)
        });
        if let Err(error) = crate::render::replay::replay(
            active.writer(),
            active.id(),
            output,
            capabilities,
            workspace(args.directory.as_ref()).ok().as_deref(),
        ) {
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
                EpochStart::New { .. } => {
                    format!("the new session status selection could not be refreshed: {error}")
                }
                EpochStart::Fork { .. } => {
                    format!("the forked session status could not be updated: {error}")
                }
                EpochStart::Compact { .. } => {
                    format!("the compacted session status could not be updated: {error}")
                }
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
    if let EpochStart::Compact { request, .. } = epoch_start
        && let Err(error) = dispatch_request_hook(
            &session_hooks.runtime,
            *request,
            session_hooks.turn,
            // Compaction starts a clean event stream. The source PreCompact
            // closure is not a parent in this session; lineage retains the
            // source boundary and the request identifies this lifecycle.
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

fn report_settled_execs(
    state: &SessionState,
    settled_before: &std::collections::HashSet<ego_core::event::id::DetachedExecId>,
    diagnostics: &dyn Diagnostics,
) {
    let settled = state
        .detached_execs
        .iter()
        .filter_map(|(id, snapshot)| {
            (!settled_before.contains(id))
                .then_some(snapshot.as_ref())
                .flatten()
        })
        .collect::<Vec<_>>();
    if !settled.is_empty() {
        diagnostics.note(&crate::output::background_exec_notice(&settled));
    }
}

#[cfg(test)]
mod background_exec_notice_tests {
    use super::*;
    use ego_core::event::id::DetachedExecId;
    use ego_core::permit::{DetachedExecOutcome, DetachedExecSnapshot, ToolResult};

    #[derive(Default)]
    struct Notes(std::sync::Mutex<Vec<String>>);

    impl Diagnostics for Notes {
        fn note(&self, message: &str) {
            self.0.lock().unwrap().push(message.to_owned());
        }
    }

    fn snapshot(outcome: DetachedExecOutcome, output: &[u8]) -> DetachedExecSnapshot {
        DetachedExecSnapshot {
            id: DetachedExecId::new(),
            outcome: Some(outcome),
            stdout_tail: output.to_vec(),
            stderr_tail: output.to_vec(),
            stdout_omitted: 0,
            stderr_omitted: 0,
        }
    }

    // Catches: already monitored build output is replayed in three separate completion notices.
    #[test]
    fn settled_batch_reports_outcomes_once_without_replaying_output() {
        let monitored = b"Compiling big-dependency\nPASS visible already";
        let success = snapshot(
            DetachedExecOutcome::Exited {
                code: 0,
                result: ToolResult::new(Vec::new()),
            },
            monitored,
        );
        let failure = snapshot(
            DetachedExecOutcome::Exited {
                code: 2,
                result: ToolResult::new(Vec::new()),
            },
            b"actionable stderr remains in evidence",
        );
        let timeout = snapshot(
            DetachedExecOutcome::TimedOut { timeout_ms: 4000 },
            b"late progress",
        );
        let mut state = SessionState::default();
        for snapshot in [&success, &failure, &timeout] {
            state
                .detached_execs
                .insert(snapshot.id, Some(snapshot.clone()));
        }
        let notes = Notes::default();
        report_settled_execs(&state, &std::collections::HashSet::new(), &notes);
        let notes = notes.0.lock().unwrap();
        assert_eq!(notes.len(), 1);
        let text = &notes[0];
        assert_eq!(text, "Background commands: 3 settled, 2 not successful.");
        assert!(!text.contains("Compiling big-dependency"));
        assert!(!text.contains("PASS visible already"));
        assert!(!text.contains("actionable stderr remains in evidence"));
        assert!(success.model_text().contains("Compiling big-dependency"));
    }

    // Catches: a single successful command still floods the owner after the turn ends.
    #[test]
    fn single_success_is_one_short_receipt_and_old_settlements_stay_quiet() {
        let snapshot = snapshot(
            DetachedExecOutcome::Exited {
                code: 0,
                result: ToolResult::new(Vec::new()),
            },
            &vec![b'x'; 32 * 1024],
        );
        let mut state = SessionState::default();
        state
            .detached_execs
            .insert(snapshot.id, Some(snapshot.clone()));
        let sink = Notes::default();
        report_settled_execs(&state, &std::collections::HashSet::new(), &sink);
        {
            let notes = sink.0.lock().unwrap();
            assert_eq!(notes.len(), 1);
            assert_eq!(
                notes[0],
                "Background commands: 1 settled, 0 not successful."
            );
        }
        report_settled_execs(
            &state,
            &std::collections::HashSet::from([snapshot.id]),
            &sink,
        );
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    // Catches: a large batch expands into per-command rows or loses failed outcomes.
    #[test]
    fn large_batch_keeps_one_summary_with_failure_count() {
        let mut state = SessionState::default();
        for _ in 0..20 {
            let snapshot = snapshot(
                DetachedExecOutcome::Exited {
                    code: 0,
                    result: ToolResult::new(Vec::new()),
                },
                b"progress",
            );
            state.detached_execs.insert(snapshot.id, Some(snapshot));
        }
        let failed = snapshot(
            DetachedExecOutcome::Failed {
                reason: "bad".repeat(100),
            },
            b"private stderr",
        );
        state.detached_execs.insert(failed.id, Some(failed.clone()));
        let notes = Notes::default();
        report_settled_execs(&state, &std::collections::HashSet::new(), &notes);
        let notes = notes.0.lock().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0],
            "Background commands: 21 settled, 1 not successful."
        );
    }
}

fn report_interactive_turn_end(
    termination: &RunTermination,
    cancel: &CancelToken,
    seat: &dyn InteractiveSeat,
    turn_started: std::time::Instant,
    queued_continuation: bool,
    diagnostics: &dyn Diagnostics,
) {
    if matches!(termination, RunTermination::Signalled(Signal::Int)) && !cancel.is_cancelled() {
        if queued_continuation {
            // A steer stopped this turn on purpose; its queued message continues the work.
            diagnostics.note(repl::QUEUED_CONTINUATION_RECEIPT);
        } else {
            diagnostics.note(&format!(
                "{} after {:.1}s",
                repl::INTERRUPTED_RECEIPT,
                turn_started
                    .elapsed()
                    .saturating_sub(seat.modal_wait())
                    .as_secs_f64()
            ));
        }
    }

    if let RunTermination::PermissionRequired {
        action,
        by: DeniedBy::Decided(ego_core::permit::Authority::Mode { mode: Mode::Plan }),
    } = termination
    {
        use unicode_width::UnicodeWidthStr;

        let action = ego_core::permit::terminal_safe(action);
        let message = format!(
            "stopped: permission refused — {action}\n\
             Choose a mode that permits this action with '/mode'."
        );
        let prefix_width = format!("{}: ", crate::DISPLAY_NAME).width();
        let room = seat.width().saturating_sub(prefix_width + 1).max(1);
        let rows = message
            .lines()
            .flat_map(|line| crate::render::Line::plain(line).wrap(room))
            .map(|line| line.text())
            .collect::<Vec<_>>();
        diagnostics.note(&rows.join("\n"));
    }

    if let RunTermination::Failed { message } = termination {
        diagnostics.error(&format!("{message}; correct the error before trying again"));
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
/// Make a published headless compaction target the active session.
pub(super) fn activate_headless_successor<'store>(
    active: &mut ActiveSession<'store>,
    receipt: &ego_core::compact::CompactionReceipt,
    store: &'store JsonlStore,
    clock: &SystemClock,
    admission: (&EvidenceAdmissionService, &Option<RunContextEvidence>),
) -> Result<(), RunTermination> {
    let failed = |message: String| RunTermination::Failed {
        message: format!(
            "compacted session {} could not be activated: {message}; resume it with 'ego resume {}'",
            receipt.target, receipt.target
        ),
    };
    let opened = session::open_candidate(store, &receipt.target, clock)
        .map_err(|error| failed(error.to_string()))?;
    let opened = Opened::Durable(Box::new(opened));
    let state = opened.state();
    let target = ActiveSession::durable(opened, state, EVIDENCE_CAP).map_err(failed)?;
    if !active.apply_publication(target, &receipt.publication) {
        return Err(failed("the target was not published".to_owned()));
    }
    let (evidence, run_context) = admission;
    admit_successor_context(evidence, active, run_context)
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
    ),
    input: CompactionActivation<'_>,
) -> Result<EpochStart, RunTermination> {
    let (target, publication, diagnostic, runtime, request) = target;
    let CompactionActivation {
        evidence,
        run_context,
        diagnostics,
        skills,
        registry,
        config,
    } = input;
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
    Ok(EpochStart::Compact {
        diagnostic,
        request,
    })
}

struct InteractiveTurnPhase<'a, 'session, 'store> {
    root_transition: &'a RootTransitionSlot,
    seat: &'a dyn InteractiveSeat,
    status: Option<&'a InteractiveStatus>,
    confinement: &'a RunConfinement,
    active: &'session ActiveSession<'store>,
    run_deadline: Option<std::time::Instant>,
    residual: Option<Arc<ResidualWriteDesk>>,
    sandbox_grants: Option<Arc<dyn ego_core::permit::SandboxGrantPort>>,
    perimeter: crate::perimeter::Perimeter,
    session_hook_ports: &'a Arc<RunEffectPorts>,
    pending_attachments: &'a mut Vec<String>,
    source: &'a Arc<model::CliGenerationModelSource>,
    cwd: &'a Path,
    diagnostics: &'a dyn Diagnostics,
    text_output: Option<&'a dyn TextOutput>,
    state: &'a mut SessionState,
    args: &'a Args,
    effective: &'a Effective,
    policy_source: &'a Arc<dyn GenerationPolicySource>,
    gate: &'a SharedBudgetGate,
    config: &'a TurnConfig,
    evidence: &'a Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    registry: &'a [RegisteredTool],
    skills: Option<&'a crate::skill::SkillCatalogue>,
    mcp_registry: &'a McpRegistryController,
    session_mcp: &'a [(McpServerName, McpServerConfig, McpConfigProvenance)],
    disabled_mcp: &'a BTreeSet<McpServerName>,
    admitted_tools: &'a Option<Arc<dyn AdmittedToolSource>>,
    mcp_runtime: &'a dyn McpRuntimeBinding,
    skill_resource_roots: &'a Option<ego_core::skill::SkillResourceRoots>,
    elicitor: &'a Option<Arc<dyn Elicitor>>,
    judge_available: bool,
    skill_trust: Trust,
    session_hooks: &'a SessionHookComposition<'session>,
    session_journal: &'a SharedJournal<'session>,
    interactive_metadata: &'a SessionMetadata<JsonlStore>,
    leased_session_journal:
        &'a LeasedSharedJournal<'session, ego_tools::store::SessionWriter<'store>>,
    skill_owner: Arc<CliSkillInvocationOwner<'session>>,
    mcp_task_port: Option<Arc<dyn ego_core::mcp::McpTaskPort>>,
    refresh_prompt: &'a mut bool,
    control: &'a mut InteractiveControl,
    turns: &'a Turns<'a>,
    last_generation_request: &'a mut Option<GenRequest>,
    policy_selection: &'a GenerationPolicySelection,
    selection: &'a model::GenerationSelection,
    model_switch_owner: Option<&'a RunModelSwitchOwner<'session>>,
    policy_application: Option<&'a PolicyApplicationOwner>,
    between: &'a (dyn Fn() + Send + Sync),
    cancel: &'a CancelToken,
    last: &'a mut Ended,
    mcp_errors: &'a [String],
    jsonl_store: &'a JsonlStore,
    root: &'a Path,
    clock: &'a SystemClock,
    skill_profile_layers: Option<&'a [Sourced<ConfigLayer>]>,
}

/// The same workspace and store the session was opened against, so
/// `ego --directory X` exports X and not whatever the process happens to sit in.
fn export_args_for(
    source: SessionId,
    selection: repl::actions::ExportSelection,
    args: &Args,
) -> crate::cli::ExportArgs {
    crate::cli::ExportArgs {
        source,
        sanitize: selection.sanitize,
        format: selection.format,
        follow: false,
        after: None,
        file: None,
        session_dir: args.session_dir.clone(),
        directory: args.directory.clone(),
    }
}

/// Transcript notes for an interactive export: the rendering and its
/// receipt, or the refusal. Nothing is written to a file.
fn export_notes(
    args: &crate::cli::ExportArgs,
    selection: repl::actions::ExportSelection,
) -> Vec<String> {
    match exported(args) {
        Ok(rendered) => vec![
            rendered,
            format!(
                "Exported {} as {} to this transcript; no file written.",
                if selection.sanitize {
                    "sanitized evidence bundle"
                } else {
                    "local session"
                },
                selection.format
            ),
        ],
        Err(termination) => vec![format!(
            "the session could not be exported: {}",
            termination.exit_reason()
        )],
    }
}

async fn drive_interactive_turn_phase<'session: 'a, 'a, 'store: 'session>(
    input: InteractiveTurnPhase<'a, 'session, 'store>,
    resumed_turn: Option<RuntimeResume>,
    asked: Option<InteractiveTurnInput>,
) -> Result<(bool, crate::perimeter::Perimeter, Option<RootReadmission>), RunTermination> {
    let InteractiveTurnPhase {
        root_transition,
        seat,
        status,
        confinement,
        active,
        run_deadline,
        residual,
        sandbox_grants,
        mut perimeter,
        session_hook_ports,
        pending_attachments,
        source,
        cwd,
        diagnostics,
        text_output,
        state,
        args,
        effective,
        policy_source,
        gate,
        config,
        evidence,
        invocation,
        registry,
        skills,
        mcp_registry,
        session_mcp,
        disabled_mcp,
        admitted_tools,
        mcp_runtime,
        skill_resource_roots,
        elicitor,
        judge_available,
        skill_trust,
        session_hooks,
        session_journal,
        interactive_metadata,
        leased_session_journal,
        skill_owner,
        mcp_task_port,
        refresh_prompt,
        control,
        turns,
        last_generation_request,
        policy_selection,
        selection,
        model_switch_owner,
        policy_application,
        between,
        cancel,
        last,
        mcp_errors,
        jsonl_store,
        root,
        clock,
        skill_profile_layers,
    } = input;
    // Automatic turns have no accepted-line arming in the seat. Cancellation
    // must be admitted before Busy is visible on every turn entry path.
    control.begin_turn();
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
        sandbox_grants,
    )
    .await?;
    perimeter = perimeter.with_execution_observation(&observation, probe_evidence.as_ref());
    let ports = Arc::new(ports.with_session_processes(session_hook_ports));
    let Some(images) = acquire_interactive_images(InteractiveImages {
        asked: asked.as_ref(),
        pending_attachments,
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
        return Ok((false, perimeter, None));
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
            root_grants: (seat.has_human() && !effective.deny_prompts)
                .then(|| {
                    elicitor.as_ref().map(|elicitor| {
                        let target =
                            config::profile(args).map_or(UserConfigTarget::Base, |profile| {
                                UserConfigTarget::Profile(
                                    ego_core::config::ProfileName::parse(profile.name())
                                        .expect("selected profile was validated at admission"),
                                )
                            });
                        Arc::new(SeatRootGrants {
                            resume_turn: true,
                            transition: Some(RootTransitionContext {
                                confinement: confinement.clone(),
                                previous: observation.clone(),
                                session_roots: active.state().session_roots.clone(),
                                evidence: active.evidence().clone(),
                                cancel: turn_cancel.clone(),
                                deadline: run_deadline,
                                slot: Arc::clone(root_transition),
                            }),
                            elicitor: Arc::clone(elicitor),
                            effective: effective.clone(),
                            workspace: CanonPath::new(root),
                            rule_writer: crate::config::ego_home().and_then(|home| {
                                let layers = skill_profile_layers?;
                                Some(Arc::new(
                                    crate::acp_permission::CliAcpPermissionRules::new(
                                        home.join("config.toml"),
                                        target.clone(),
                                    )
                                    .with_live_policy(
                                        policy_selection.clone(),
                                        Arc::new(FrozenRuleResolver::new(
                                            layers.to_vec(),
                                            skill_trust,
                                            effective.active_profile.as_ref().map(|profile| {
                                                (profile.name.clone(), profile.selected_by)
                                            }),
                                            policy_selection.clone(),
                                        )),
                                    ),
                                )
                                    as Arc<dyn ego_acp::permission::PermissionRuleWriter>)
                            }),
                            target,
                            store_root: rooted_path(
                                root,
                                session::store_root(args.session_dir.as_ref())
                                    .expect("session store already admitted"),
                            ),
                        }) as Arc<dyn ego_core::turn::RootGrantPort>
                    })
                })
                .flatten(),
            judge_available,
            confinement,
            skill_trust,
            active,
            hooks: session_hooks,
            journal: session_journal,
            skill_owner: skill_owner.clone(),
            mcp_task_port: mcp_task_port.clone(),
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
    // As in the headless driver, a root-bearing session has no automatic
    // successor. Do not cut fresh work at the compaction threshold when the
    // host cannot recover it; the actual context-capacity limit still applies.
    let loop_ = if state.session_roots.is_empty() {
        loop_.with_automatic_compaction()
    } else {
        loop_
    };
    let mut loop_ = loop_
        .with_compaction_threshold(effective.compaction.threshold)
        .with_interactive_control(control);
    let mut revoked_root = None;
    let roots_before = (
        state.session_roots.clone(),
        state.permanent_root_consents.clone(),
    );
    if matches!(asked, Some(InteractiveTurnInput::Prompt { .. }))
        && resumed_turn.is_none()
        && effective
            .recap
            .due(state, ego_core::event::time::Clock::now(clock))
    {
        diagnostics.note(&render_recap(loop_.recap(&turn_cancel).await));
    }
    let side_port = loop_.side_question_port();
    let driven = {
        let driving = drive_interactive_input(
            turns,
            &mut loop_,
            resumed_turn,
            asked,
            images,
            effective.exec_policy().expect("admitted environment"),
            &turn_cancel,
            diagnostics,
        );
        futures::pin_mut!(driving);
        loop {
            let inspection = seat.next_inspection();
            match futures::future::select(driving.as_mut(), inspection).await {
                futures::future::Either::Left((result, _)) => break result,
                futures::future::Either::Right((command, _)) => {
                    if let repl::HostCommand::Config(arguments) = &command
                        && command.is_root_revocation()
                    {
                        let current = session_hooks.state.state();
                        match root_revocation::requested(arguments, seat, &current, cwd) {
                            Ok(path) => {
                                revoked_root = Some(path);
                                turn_cancel.cancel(CancelCause::User);
                                break driving.await;
                            }
                            Err(error) => diagnostics.note(&error),
                        }
                        continue;
                    }
                    if let repl::HostCommand::Images(request) = command {
                        let admission = answer_image_request(
                            &request,
                            seat,
                            source,
                            (confinement, active.evidence(), run_deadline),
                            cwd,
                            &[],
                        );
                        futures::pin_mut!(admission);
                        match futures::future::select(driving.as_mut(), admission).await {
                            futures::future::Either::Left((result, admission)) => {
                                // Settle the already accepted terminal request even when
                                // the active turn closes first. Its ingress may then
                                // reject submission, which returns the envelope to idle.
                                admission.await;
                                break result;
                            }
                            futures::future::Either::Right((_, _)) => continue,
                        }
                    }
                    if let repl::HostCommand::SideQuestion(question) = command {
                        let answer = answer_side_question(&side_port, &question);
                        futures::pin_mut!(answer);
                        match futures::future::select(driving.as_mut(), answer).await {
                            futures::future::Either::Left((result, answer)) => {
                                // The question was accepted: settle its attempt
                                // and show the answer even if the turn ended first.
                                diagnostics.note(&answer.await);
                                break result;
                            }
                            futures::future::Either::Right((note, _)) => {
                                diagnostics.note(&note);
                                continue;
                            }
                        }
                    }
                    if let repl::HostCommand::ModelSwitch(request) = command {
                        if selection.snapshot().is_ok_and(|current| {
                            model_route(&current.identity.0) != model_route(&request.target().0)
                        }) {
                            diagnostics
                                .note("wait for this turn to finish before switching providers");
                            continue;
                        }
                        // The owner keeps its own lifecycle and token: the run-level
                        // cancellation/deadline in the hook state, not `turn_cancel`.
                        // The captured parent keeps its request; only later generation
                        // boundaries read the installed selection. Selecting the same
                        // retained parent future settles an accepted switch even when
                        // the turn ends first. Other queued inspections wait for it.
                        let switching = apply_model_switch(
                            request,
                            effective,
                            model_switch_owner,
                            status,
                            || session_hooks.state.state(),
                            diagnostics,
                        );
                        futures::pin_mut!(switching);
                        match futures::future::select(driving.as_mut(), switching).await {
                            futures::future::Either::Left((result, switching)) => {
                                switching.await;
                                break result;
                            }
                            futures::future::Either::Right(((), _)) => continue,
                        }
                    }
                    assert!(
                        command.is_concurrent_native(),
                        "native inbox requires an explicit concurrent handler"
                    );
                    let current = session_hooks.state.state();
                    match command {
                        repl::HostCommand::Fast(enabled) => {
                            match selection.set_fast(enabled) {
                                Ok(receipt) => diagnostics.note(&receipt),
                                Err(error) => diagnostics.note(&error.to_string()),
                            }
                            refresh_turn_status(
                                status,
                                policy_selection,
                                selection,
                                &current,
                                diagnostics,
                            );
                        }
                        repl::HostCommand::Policy(
                            request @ (PolicyControlRequest::SetMode(_)
                            | PolicyControlRequest::CycleMode
                            | PolicyControlRequest::Rule(_)
                            | PolicyControlRequest::Permission(
                                repl::policy_controls::PermissionInput::List
                                | repl::policy_controls::PermissionInput::Explain { .. }
                                | repl::policy_controls::PermissionInput::Revoke(_),
                            )),
                        ) => {
                            handle_interactive_policy(
                                request,
                                policy_application,
                                status,
                                text_output,
                                diagnostics,
                            );
                        }
                        repl::HostCommand::Config(arguments)
                            if arguments == "details" || arguments.starts_with("set ") =>
                        {
                            run_interactive_config(
                                &arguments,
                                effective,
                                skill_profile_layers,
                                &perimeter,
                                diagnostics,
                            )
                            .await;
                        }
                        repl::HostCommand::Hooks
                        | repl::HostCommand::Details(repl::actions::ActionId::Hooks) => {
                            render_session_hooks(
                                matches!(command, repl::HostCommand::Details(_)),
                                &session_hooks.state,
                                diagnostics,
                            );
                        }
                        repl::HostCommand::Skills(action) => {
                            render_skill_catalogue(
                                skills,
                                action == repl::actions::SkillSurfaceAction::Diagnose,
                                action == repl::actions::SkillSurfaceAction::Details,
                                diagnostics,
                            );
                        }
                        repl::HostCommand::Pins(ego_core::compact::PinCommand::List) => {
                            match session_journal.pins(ego_core::compact::PinCommand::List) {
                                Ok(receipt) => diagnostics.note(&receipt),
                                Err(error) => {
                                    diagnostics.note(&format!("pin operation failed: {error}"))
                                }
                            }
                        }
                        repl::HostCommand::Login(None) => {
                            note_session_login(None, None, selection, effective, diagnostics).await;
                        }
                        repl::HostCommand::Mcp(_) => {
                            diagnostics.note(&crate::mcp::session_inventory(
                                &mcp_registry.status(),
                                session_mcp
                                    .iter()
                                    .map(|(name, _, _)| (name, disabled_mcp.contains(name))),
                            ));
                        }
                        repl::HostCommand::Action(repl::actions::ActionId::Tools)
                        | repl::HostCommand::Details(repl::actions::ActionId::Tools) => {
                            render_tool_catalogue(
                                registry,
                                mcp_registry,
                                matches!(command, repl::HostCommand::Details(_)),
                                diagnostics,
                            );
                        }
                        repl::HostCommand::Details(repl::actions::ActionId::Help) => {
                            diagnostics.note(&seat.action_help_details());
                        }
                        repl::HostCommand::Action(repl::actions::ActionId::Doctor)
                        | repl::HostCommand::Details(repl::actions::ActionId::Doctor) => {
                            let workspace = root.to_path_buf();
                            let width = seat.width();
                            let observed_perimeter = perimeter.clone();
                            let details = matches!(command, repl::HostCommand::Details(_));
                            // Configured probes can join network threads. Keep polling
                            // the admitted turn and its interrupt path while they run.
                            let doctor = async move {
                                match tokio::task::spawn_blocking(move || {
                                    crate::doctor::observed_session_view(
                                        workspace,
                                        width,
                                        &observed_perimeter,
                                        details,
                                    )
                                })
                                .await
                                {
                                    Ok(note) => note,
                                    Err(error) => format!("doctor inspection failed: {error}"),
                                }
                            };
                            futures::pin_mut!(doctor);
                            match futures::future::select(driving.as_mut(), doctor).await {
                                futures::future::Either::Left((result, doctor)) => {
                                    diagnostics.note(&doctor.await);
                                    break result;
                                }
                                futures::future::Either::Right((note, _)) => {
                                    diagnostics.note(&note)
                                }
                            }
                        }
                        repl::HostCommand::Export(selection) => {
                            // The same committed-prefix read as the idle handler:
                            // `follow=false`, `file=None`, no store lease. Rendering
                            // is synchronous, so run it off the executor thread and
                            // keep polling the admitted turn meanwhile.
                            let export_args = export_args_for(active.id(), selection, args);
                            let export = async move {
                                tokio::task::spawn_blocking(move || {
                                    export_notes(&export_args, selection)
                                })
                                .await
                                .unwrap_or_else(|error| {
                                    vec![format!("the session could not be exported: {error}")]
                                })
                            };
                            futures::pin_mut!(export);
                            match futures::future::select(driving.as_mut(), export).await {
                                futures::future::Either::Left((result, export)) => {
                                    for note in export.await {
                                        diagnostics.note(&note);
                                    }
                                    break result;
                                }
                                futures::future::Either::Right((notes, _)) => {
                                    for note in notes {
                                        diagnostics.note(&note);
                                    }
                                }
                            }
                        }
                        repl::HostCommand::Action(repl::actions::ActionId::Context) => {
                            let store = super::composition::memory_store(effective, args, root);
                            diagnostics.note(&crate::memory::context_text(
                                config.system.as_ref(),
                                store.as_deref(),
                            ));
                        }
                        repl::HostCommand::SessionMetadata(operation) => {
                            if matches!(
                                operation,
                                crate::cli::SessionMetadataOperation::Goal(
                                    crate::cli::GoalOperation::Stop
                                )
                            ) {
                                if let Err(error) = crate::metadata::stop_interactive_goal(
                                    session_journal,
                                    &mut |message| diagnostics.note(message),
                                ) {
                                    diagnostics.note(&format!("goal stop failed: {error}"));
                                }
                            } else if matches!(
                                operation,
                                crate::cli::SessionMetadataOperation::Name(
                                    crate::cli::NameOperation::Set(_)
                                        | crate::cli::NameOperation::Clear
                                )
                            ) {
                                // Title-only: the leased compare-and-append owner keeps
                                // stale-sequence and uncertain-publication errors.
                                if let Err(error) = crate::metadata::execute_interactive(
                                    interactive_metadata,
                                    leased_session_journal,
                                    session_journal,
                                    &operation,
                                    &effective.ceilings,
                                    &mut |message| diagnostics.note(message),
                                ) {
                                    diagnostics.note(&format!(
                                        "session metadata operation failed: {error}"
                                    ));
                                }
                            } else if let Some(text) =
                                crate::metadata::inspection_text(&current, &operation)
                            {
                                diagnostics.note(&text);
                            }
                        }
                        repl::HostCommand::Tasks(arguments) => {
                            diagnostics.note(
                                &task_listing(
                                    &arguments,
                                    &current,
                                    session_hook_ports.process(),
                                    skill_owner.as_ref(),
                                )
                                .await,
                            );
                        }
                        repl::HostCommand::Action(repl::actions::ActionId::Config) => {
                            diagnostics.note(&crate::config::repl_text(
                                effective,
                                skill_profile_layers.unwrap_or_default(),
                                &perimeter,
                            ));
                        }
                        repl::HostCommand::Config(arguments) if arguments == "perimeter" => {
                            diagnostics.note(&perimeter.text())
                        }
                        repl::HostCommand::Config(arguments) if arguments == "roots" => {
                            diagnostics.note(&root_revocation::listing(&current))
                        }
                        repl::HostCommand::Action(repl::actions::ActionId::Usage) => {
                            report_session_usage(
                                jsonl_store,
                                root,
                                active.id(),
                                clock,
                                current.context,
                                seat.width(),
                                diagnostics,
                            )
                            .await;
                        }
                        _ => unreachable!("native inbox commands have explicit handlers"),
                    }
                }
            }
        }
    };
    *last_generation_request = loop_.last_generation_request();
    let root_resume = loop_.root_readmission();
    drop(loop_);
    *state = session_hooks.state.state();
    if let Some(path) = revoked_root {
        between();
        return Ok((false, perimeter, Some(RootReadmission::Revoke(path))));
    }
    let granted = (root_resume.is_some()
        || state.session_roots != roots_before.0
        || state.permanent_root_consents != roots_before.1)
        .then_some(RootReadmission::Grant {
            resume: root_resume,
        });
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
    seat.refresh_queue(state);
    let Some((termination, result)) = driven else {
        return Ok((false, perimeter, granted));
    };
    if matches!(termination, RunTermination::Signalled(Signal::Int))
        && let Some(port) = mcp_task_port
    {
        port.stop_automatic_wakes(turn_started);
    }
    report_interactive_turn_end(
        &termination,
        cancel,
        seat,
        turn_started,
        state.interrupt_pending(),
        diagnostics,
    );
    between();
    background_indicator::report(
        args.output,
        seat.has_human(),
        seat.width(),
        state,
        ports.process(),
        &skill_owner,
        diagnostics,
    );
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
        return Ok((true, perimeter, None));
    }
    Ok((false, perimeter, granted))
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
        last.selected_session = Some(meta.into());
        seat.restore_control(control);
    }
}

/// Read the next operator line. `None` means the idle session reached its
/// cache-expiry compaction point first; that deadline is then marked attempted.
fn read_interactive_command(
    seat: &dyn InteractiveSeat,
    status: Option<&InteractiveStatus>,
    idle: Option<IdleCompactionWatch<'_>>,
) -> Option<repl::Line> {
    #[cfg(all(test, unix))]
    let modal_idle_gate = super::interactive_driver::modal_finish::before_idle();
    seat.idle();
    #[cfg(all(test, unix))]
    super::interactive_driver::modal_finish::after_idle(modal_idle_gate);
    if let Some(status) = status
        && status.human
    {
        status.seat.activate();
    }
    let Some(idle) = idle else {
        return Some(seat.read());
    };
    loop {
        match idle.decide() {
            ego_core::compact::IdleCompaction::Never => return Some(seat.read()),
            ego_core::compact::IdleCompaction::Now { expires_at } => {
                *idle.attempted = Some(expires_at);
                return None;
            }
            // Wall-clock deadline, monotonic wait: decide again after the wait.
            ego_core::compact::IdleCompaction::After(wait) => {
                if let Some(line) = seat.read_before(std::time::Instant::now() + wait) {
                    return Some(line);
                }
            }
        }
    }
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
) -> Result<(), RunTermination> {
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
    Ok(())
}

#[cfg(test)]
mod pid_index_tests {
    use super::*;
    use crate::run::tests::CompactionDecisionSeat;
    use ego_core::event::domain::{DomainEvent, Envelope};
    use ego_core::event::id::Seq;
    use ego_core::event::time::Timestamp;
    use tempfile::tempdir;

    #[tokio::test]
    async fn rejected_resume_of_unfinished_session_preserves_active_pid_binding() {
        rejected_resume_preserves_active_pid_binding(true, true).await;
    }

    #[tokio::test]
    async fn terminal_activation_rejection_preserves_active_pid_binding() {
        // Catches publication before the terminal accepts the successor session.
        rejected_resume_preserves_active_pid_binding(false, false).await;
    }

    async fn rejected_resume_preserves_active_pid_binding(unfinished: bool, activate: bool) {
        // Catches a rejected candidate replacing and then deleting the active PID binding.
        #[derive(Default)]
        struct Notes(Mutex<Vec<String>>);
        impl crate::output::Diagnostics for Notes {
            fn note(&self, message: &str) {
                self.0.lock().unwrap().push(message.to_owned());
            }
        }
        let home = tempdir().unwrap();
        let root = std::fs::canonicalize(home.path()).unwrap();
        let store = Arc::new(
            JsonlStore::new(root.join("sessions"), &root, Box::new(OsIo))
                .with_pid_index(root.clone()),
        );
        let first = SessionId::new();
        let candidate = SessionId::new();
        {
            let opened = session::open(&store, &candidate, &SystemClock).unwrap();
            opened
                .writer
                .append(
                    &candidate,
                    &DomainEvent {
                        env: Envelope {
                            id: EventId::new(),
                            seq: Seq::default().next(),
                            ts: Timestamp::from_millis(9000),
                            turn: TurnId::new(),
                            item: Some(ItemId::new()),
                            parent: None,
                            producer: Producer::Main,
                        },
                        kind: if unfinished {
                            EventKind::PromptQueued {
                                audio: Vec::new(),
                                id: ego_core::event::id::QueuedPromptId::new(),
                                text: "unfinished prompt".into(),
                                images: Vec::new(),
                            }
                        } else {
                            EventKind::TurnCompleted {
                                ok: true,
                                exit_reason: ego_core::event::domain::ExitReason::Completed,
                            }
                        },
                    },
                )
                .unwrap();
        }
        let active = session::open(&store, &first, &SystemClock).unwrap();
        let index = root
            .join("pids")
            .join(format!("{}.json", std::process::id()));
        let before: Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
        assert_eq!(before["session_id"], first.to_string());
        let notes = Notes::default();
        let transition = select_session_transition(
            repl::HostCommand::Resume(Some(candidate.to_string())),
            &CompactionDecisionSeat {
                confirm: true,
                activate,
            },
            false,
            &store,
            &root,
            first,
            (&SystemClock, &notes),
        )
        .await;
        assert!(transition.is_none());
        let expected = if unfinished {
            "has unfinished work"
        } else {
            "could not activate"
        };
        assert!(
            notes
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|note| note.contains(expected))
        );
        let after: Value = serde_json::from_slice(
            &std::fs::read(&index).expect("rejected resume must preserve active PID binding"),
        )
        .unwrap();
        assert_eq!(after["session_id"], active.writer.id().to_string());
        assert!(
            session::open_candidate(store.as_ref(), &first, &SystemClock).is_err(),
            "rejected activation must retain the source writer lease"
        );
        let candidate = session::open_candidate(store.as_ref(), &candidate, &SystemClock)
            .expect("rejected activation must release the candidate writer lease");
        drop(candidate);
        let after_candidate_drop: Value =
            serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
        assert_eq!(after_candidate_drop["session_id"], first.to_string());
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    // Catches: a later interactive epoch journals launch Effective over the live switch.
    #[test]
    fn epoch_selections_follow_live_model_instead_of_launch_defaults() {
        let root = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(
            root.path().to_owned(),
            root.path().to_owned(),
            Box::new(ego_tools::store::OsIo),
        );
        let id = SessionId::new();
        let clock = SystemClock;
        let opened = session::open(&store, &id, &clock).unwrap();
        let mut active = ActiveSession::durable(opened, SessionState::default(), 1024).unwrap();
        let mut effective = ego_core::config::resolve(&[], Trust::Trusted, None).unwrap();
        effective.model = Some(ModelRef("ollama/launch".to_owned()));
        effective.thinking = Some(ThinkingLevel::Low);
        let selection = model::GenerationSelection::new(
            ModelRef("ollama/launch".to_owned()),
            ThinkingLevel::Low,
        );
        let confinement =
            RunConfinement::new(Confinement::Off, false, CanonPath::new(root.path())).unwrap();
        record_session_selections(
            &mut active,
            &effective,
            None,
            &selection,
            &confinement,
            &clock,
            &NullSink,
        )
        .unwrap();
        selection
            .install(ModelRef("ollama/switched".to_owned()), ThinkingLevel::High)
            .unwrap();
        record_session_selections(
            &mut active,
            &effective,
            None,
            &selection,
            &confinement,
            &clock,
            &NullSink,
        )
        .unwrap();
        let recorded = active.state().session_selections.as_ref().unwrap();
        assert_eq!(
            recorded.model(),
            Some(&ModelRef("ollama/switched".to_owned()))
        );
        assert_eq!(recorded.thinking(), Some(ThinkingLevel::High));
    }
}

#[cfg(test)]
#[path = "driver_runtime_tests.rs"]
mod tests;

#[cfg(test)]
mod named_session_login_tests {
    use super::*;

    // Catches: /login repairs default A or creates another account instead of targeting pin B.
    #[test]
    fn named_login_refusal_targets_the_actual_pin_and_never_invents_an_account() {
        let layer = ConfigLayer::parse(
            Source::User,
            r#"
connection = "A"
[connections.A]
name = "Default"
vendor = "anthropic"
access = "oauth"
[connections.B]
name = "Pinned"
vendor = "openai"
access = "oauth"
"#,
        )
        .unwrap();
        let effective = ego_core::config::resolve(&[layer], Trust::Untrusted, None).unwrap();
        let message = named_session_login_refusal(Some("B"), &effective).unwrap();
        assert!(message.contains("ego auth login openai-codex --renew"));
        assert!(!message.contains("--connection A"));
        assert_eq!(named_session_login_refusal(None, &effective), None);
        let missing = named_session_login_refusal(Some("removed"), &effective).unwrap();
        assert!(missing.contains("pinned account is absent"));
        assert!(missing.contains("cannot create a replacement"));
    }
}
