//! Wire prepared CLI authority into executable run inputs.

use super::driver_runtime::activate_headless_successor;
use super::*;

pub(super) fn run_budget_spec(budget: &Budget) -> Result<String, RunTermination> {
    match budget {
        Budget::Money(_) => Ok(budget.to_string()),
        Budget::Tokens(tokens) if tokens.is_multiple_of(1_000) => {
            Ok(format!("{}k", tokens / 1_000))
        }
        Budget::Tokens(tokens) => Err(RunTermination::UsageError {
            message: format!(
                "the typed token budget {tokens} is not representable by the current budget grammar"
            ),
        }),
    }
}

/// The headless pipeline assembled from read-only command facts before admission.
pub struct CliPrepared {
    pub(super) _permit: Option<OwnedSemaphorePermit>,
    pub(super) store: Arc<JsonlStore>,
    pub(super) artifacts: Option<Arc<dyn RunArtifactStore>>,
    pub(super) run_id: RunId,
    pub(super) session_id: SessionId,
    pub(super) schema_retries: u32,
    pub(super) retry: u32,
    pub(super) provider_recovery: bool,
    pub(super) proxy: Option<ego_tools::http::ProviderProxy>,
    pub(super) fallbacks: Vec<ModelRef>,
    pub(super) max_turns: Option<u32>,
    pub(super) goal: Option<String>,
    pub(super) goal_turns: Option<u32>,
    pub(super) goal_check_command: Option<Vec<String>>,
    pub(super) run_deadline: Option<std::time::Instant>,
    pub(super) root: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) confinement: RunConfinement,
    /// The posture the config layers below were resolved at, kept so the hook
    /// catalogue is admitted at the same one.
    ///
    /// Resolved in `prepare` and read in `submit` because those are two calls:
    /// dropping it in between is what let a session a host had answered
    /// `trusted` for admit its hooks under `Untrusted` anyway.
    pub(super) trust: Trust,
    pub(super) profile_layers: Option<Vec<Sourced<ConfigLayer>>>,
    /// `--no-hooks`: no hook is admitted, whatever the layers hold.
    pub(super) no_hooks: bool,
    /// The exact ACP session fact to compare during effect construction.
    pub(super) admitted_sandbox_observation: Option<ExecObservation>,
    pub(super) effective: Effective,
    connection_binding: Option<String>,
    pub(super) system: Option<SystemPrompt>,
    pub(super) registry: Vec<RegisteredTool>,
    pub(super) compiled: Option<schema::Schema>,
    pub(super) slug: String,
    pub(super) run_context: RunContextEvidence,
    pub(super) admission: Arc<EvidenceAdmissionService>,
    pub(super) child_executor: Arc<CliRunExecutor>,
    child: bool,
}

pub(super) struct PreparedExecutionRuntime {
    pub(super) admission: Arc<EvidenceAdmissionService>,
    pub(super) child_executor: Arc<CliRunExecutor>,
    pub(super) child_authority: Option<Arc<ChildAuthorityCeiling>>,
    pub(super) child: bool,
}

/// Apply what the operator explicitly chose over what configuration resolved.
///
/// Only what was chosen: inheriting a configured default is not a decision,
/// and a run that recorded it as one would claim a person picked a model they
/// never saw.
///
/// A model lands in two places because it is spelled two ways — `slug` is the
/// resolved name the preflight builds its generation target from, and
/// `effective.model` is the configuration value the rest of the turn reads.
/// Moving one without the other answers the host `model: x` and generates with
/// `y`, which is worse than refusing the pick: the host is told it landed and
/// only the evidence disagrees. They are set together here so there is one
/// place that can be got wrong instead of two that can drift.
pub(super) fn apply_seat(
    state: &ego_core::control::SessionControlState,
    effective: &mut Effective,
    slug: &mut String,
) {
    if let Some(model) = state.model.chosen.clone() {
        slug.clone_from(&model.0);
        effective.model = Some(model);
    }
    if let Some(effort) = state.effort.chosen {
        effective.thinking = Some(effort);
    }
    if let Some(mode) = state.mode.chosen {
        effective.mode = Some(mode);
    }
    // `state.sandbox` is deliberately not applied here, and the omission is
    // the decision rather than an oversight. This runs at a generation
    // boundary, by which point `RunConfinement` has been built and the MCP
    // transports, the effect scope and the child authority ceiling have all
    // been derived from it. Writing `effective.sandbox` now would move
    // nothing and read as though it had. A host's pick travels instead as a
    // `RunSelections` sandbox, which lands before any of that exists, and
    // `session/set_config_option` refuses the pick outright once a run has
    // started.
}

/// Whether this run refused the posture the capture it was handed pins.
///
/// A subagent runs at its parent's posture, and a capture *is* that posture,
/// already resolved. The one run that must not use it is the child a parent
/// deliberately spent bare, because the capture is exactly what it is
/// refusing. Only a trusted capture has anything to refuse: a bare child of an
/// untrusted parent resolves the same layers either way, and keeping the
/// capture keeps the profile pinned with it.
pub(super) const fn spent_bare(captured: Trust, requested: Trust) -> bool {
    matches!((captured, requested), (Trust::Trusted, Trust::Untrusted))
}

fn apply_preparation_selections(
    args: &mut Args,
    selections: &RunSelections,
) -> Result<(), RunTermination> {
    if let Some(id) = selections.connection() {
        args.connection = Some(id.to_owned());
    }
    if let Some(model) = selections.model() {
        args.model = Some(model.0.clone());
    }
    if let Some(thinking) = selections.thinking() {
        args.thinking = Some(thinking);
    }
    if let Some(mode) = selections.mode() {
        args.mode = Some(mode);
    }
    // Before `rooted_settings` below, which is the point: the confinement
    // this becomes is what the MCP admission, the effect scope and the
    // child authority ceiling are all derived from, and none of them can
    // be re-decided once they exist.
    if let Some(sandbox) = selections.sandbox() {
        args.sandbox = Some(sandbox);
    }
    if let Some(budget) = selections.budget() {
        args.budget = Some(run_budget_spec(budget)?);
    }
    apply_preparation_prompt(args, selections);
    Ok(())
}

fn apply_preparation_prompt(args: &mut Args, selections: &RunSelections) {
    if let Some(profile) = selections.profile() {
        args.profile = Some(profile.to_owned());
        args.profile_source = Some(ego_core::config::Source::Flags);
    }
    if let Some(path) = selections.read_only_dir() {
        args.read_only_dir = Some(path.to_path_buf());
    }
    args.ceiling_profile = selections.ceiling_profile().map(str::to_owned);
    if let Some(options) = selections.prompt_options() {
        args.session_prompt = options.system_prompt.clone();
        // A host cannot undo a launch-wide bare selection.
        args.bare |= options.bare.unwrap_or(false);
    }
}

impl CliPrepared {
    fn apply_operator_seat(&mut self, bindings: &RunExecutionBindings) {
        // The operator seat speaks before anything reads the configuration.
        //
        // A protocol session continues one run across prompts, so its
        // selections were admitted once and cannot change; the seat is the
        // only channel a live picker has. Applying it here — ahead of the
        // model preflight and the policy selection below — is what makes a
        // change land at the next generation boundary rather than at the next
        // session. Only what the operator explicitly chose is applied: an
        // inherited configuration default is not a decision to re-record.
        let Some(controls) = bindings.controls() else {
            return;
        };
        match controls.state(self.session_id) {
            Ok(state) => apply_seat(&state, &mut self.effective, &mut self.slug),
            // An unreadable seat leaves the configured values in force.
            // Refusing the turn would let a poisoned picker lock a
            // session out of running at all.
            Err(error) => {
                tracing::warn!(%error, "the operator seat could not be read");
            }
        }
    }

    pub(super) async fn prepare(
        mut args: Args,
        store: Arc<JsonlStore>,
        artifacts: Option<Arc<dyn RunArtifactStore>>,
        request: &RunPreparation,
        permit: Option<OwnedSemaphorePermit>,
        frozen: Option<&config::FrozenRunConfig>,
        runtime: PreparedExecutionRuntime,
    ) -> Result<RunPrepared<Self>, RunTermination> {
        let PreparedExecutionRuntime {
            admission,
            child_executor,
            child_authority,
            child,
        } = runtime;
        let usage = |error: UsageError| RunTermination::UsageError {
            message: error.to_string(),
        };
        apply_preparation_selections(&mut args, &request.selections)?;
        ego_core::config::validate_prompt_options(args.bare, args.instructions.is_some())
            .map_err(|message| usage(UsageError(message.to_owned())))?;
        if args.profile.as_deref() == Some("ci") {
            return Err(RunTermination::UsageError {
                message: "profile 'ci' is available only through 'ego run' or 'ego resume'"
                    .to_owned(),
            });
        }
        let root = request.root.as_path().to_path_buf();
        // The closure never asks: it reports what a seat already answered, or
        // `Untrusted` when nothing was asked. This path has no terminal and no
        // stdin, and the substitution keeps it that way — a protocol seat does
        // its asking before preparation, where it can await the host, and
        // arrives here with a decision rather than with a question.
        //
        // A frozen config is a child of a run whose posture was decided when
        // the parent was prepared; it re-reads no layers and takes no answer.
        let trust = request.selections.trust().unwrap_or(Trust::Untrusted);
        let settings = match frozen {
            Some(frozen) if !spent_bare(frozen.trust(), trust) => {
                frozen_rooted_settings(&args, root, frozen, &Stderr)
            }
            _ => rooted_settings(&args, root, |_| trust, &Stderr),
        }
        .map_err(usage)?;
        if let Some(authority) = child_authority.as_deref() {
            authority
                .admit(&settings.effective, settings.confinement.offline())
                .map_err(|message| RunTermination::UsageError { message })?;
        }
        let Materials {
            system,
            registry,
            skills: _,
            compiled,
            text: _,
            slug,
        } = prepared_materials(
            &settings,
            &args,
            String::new(),
            Arc::new(store.history()),
            &Stderr,
        )
        .map_err(usage)?;
        let (system, compiled) = if child {
            (None, None)
        } else {
            (system, compiled)
        };
        let Settings {
            run_deadline,
            root,
            cwd,
            confinement,
            trust,
            profile_layers,
            effective,
        } = settings;
        let schema_retries = args.schema_retries;
        let retry = args.retry;
        let fallbacks = args.fallbacks.clone();
        let max_turns = args.max_turns;
        let goal = args.goal.clone();
        let goal_turns = args.goal_turns;
        let goal_check_command = args.goal_check_command.clone();
        let instructions = system
            .as_ref()
            .map(|prompt| prompt.sources.clone())
            .unwrap_or_default();
        let run_context = complete_run_context(
            &args,
            &effective,
            &confinement,
            &cwd,
            request.invocation,
            request.run_id,
            instructions,
        )
        .map_err(usage)?;

        Ok(RunPrepared::new(
            ModelRef(slug.clone()),
            Self {
                _permit: permit,
                child,
                store,
                artifacts,
                run_id: request.run_id,
                session_id: request.run_id.session_id(),
                schema_retries,
                retry,
                provider_recovery: args.provider_recovery,
                proxy: args.proxy.clone(),
                fallbacks,
                max_turns,
                goal,
                goal_turns,
                goal_check_command,
                run_deadline,
                root,
                cwd,
                confinement,
                trust,
                profile_layers,
                no_hooks: args.no_hooks,
                admitted_sandbox_observation: request.selections.sandbox_observation().cloned(),
                effective,
                connection_binding: request.selections.connection_binding().map(str::to_owned),
                system,
                registry,
                compiled,
                slug,
                run_context,
                admission,
                child_executor,
            },
        ))
    }

    pub(super) async fn submit(
        mut self,
        prompt: &RunPrompt,
        bindings: RunExecutionBindings,
        cancel: &CancelToken,
        pause: &PauseToken,
    ) -> Result<RunSettlement, RunExecutorError> {
        self.apply_operator_seat(&bindings);
        let run_id = self.run_id;
        let artifacts = self.artifacts.clone();
        let child_root = RunRoot::new(self.root.clone())
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let clock = SystemClock;
        let opened = Opened::Durable(Box::new(
            session::open(self.store.as_ref(), &self.session_id, &clock)
                .map_err(|error| RunExecutorError::new(error.to_string()))?,
        ));
        let repair_note = opened.repair_note();
        let opening = opened.state();
        if let Some(pending) = opening.pending_question() {
            if cancel.is_cancelled() {
                return settlement(cancelled_termination(cancel));
            }
            if let Some(termination) = super::composition::expired(self.run_deadline) {
                return settlement(termination);
            }
            // A pending pause still needs the normal turn boundary to record
            // its durable hold. Otherwise refuse before provider resolution,
            // effect ports, hooks or attachment acquisition can do any work.
            if pause.requested().is_none() && opening.pause.request().is_none() {
                return settlement(RunTermination::QuestionRequired {
                    question: Box::new(QuestionReport::from_request(
                        pending.item_id(),
                        pending.target(),
                        pending.request(),
                    )),
                });
            }
        }
        for grant in &opening.session_roots {
            let actual = ego_core::permit::resolve_root(
                grant.path().as_path(),
                grant.access(),
                &ego_tools::fs::OsFs,
            )
            .map_err(|error| {
                RunExecutorError::new(format!("session root could not be re-admitted: {error}"))
            })?;
            if &actual != grant.path() {
                return Err(RunExecutorError::new(
                    "session root changed since approval".to_owned(),
                ));
            }
            self.effective
                .roots
                .retain(|root| root.path != actual.as_path());
            self.effective.roots.push(ego_core::permit::RootCfg {
                path: actual.as_path().to_path_buf(),
                access: grant.access(),
                consent: None,
            });
        }
        if !opening.session_roots.is_empty() {
            let mut roots =
                super::profile_roots::admit(&self.effective, self.confinement.workspace())
                    .map_err(RunExecutorError::new)?;
            for root in &mut roots {
                if opening.session_roots.contains(root) {
                    *root = root.clone().with_terminal_write();
                }
            }
            if opening
                .session_roots
                .iter()
                .any(|grant| !roots.contains(grant))
            {
                return Err(RunExecutorError::new(
                    "session root is outside the admitted ceiling".to_owned(),
                ));
            }
            self.confinement = self.confinement.clone().with_roots(roots);
        }
        let mut active = ActiveSession::durable(opened, opening.clone(), EVIDENCE_CAP)
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        if let Some(desk) = bindings.residual_desk() {
            desk.configure_limits(self.effective.residual_limits);
        }
        let mut effects = Some(
            match segment_effects(
                &self.confinement,
                self.run_deadline,
                self.admitted_sandbox_observation.as_ref(),
                (&active, &bindings, cancel),
            )
            .await
            {
                Ok(effects) => effects,
                Err(settled) => return settled,
            },
        );
        if let Some(note_text) = repair_note {
            Stderr.note(&note_text);
        }
        let mut no_status = None;
        let preflight = generation_preflight(
            GenerationPreflightInput {
                proxy: self.proxy.clone(),
                session_id: self.session_id,
                confinement: &self.confinement,
                effective: &self.effective,
                connection_binding: self.connection_binding.as_deref(),
                slug: self.slug.clone(),
                cancel,
                run_deadline: self.run_deadline,
                mcp_registry: None,
                native_tools: &self.registry,
            },
            &mut no_status,
        )
        .await;
        let GenerationPreflight {
            source,
            selection,
            capabilities: model_capabilities,
            judge_available,
            proof_directory,
            proof_evidence,
        } = match preflight {
            Ok(preflight) => preflight,
            Err(RunTermination::UsageError { message }) => {
                return Ok(RunSettlement::Interrupted {
                    diagnostic: Some(format!("configuration error: {message}")),
                });
            }
            // Nothing was generated yet, so the failure's effect is known.
            Err(termination @ RunTermination::Provider { .. }) => return settlement(termination),
            // A host cancellation can reach the repeated preflight before a
            // provider request exists. Its effect is known; treating it as an
            // executor error would leave a cancelled continuation uncertain.
            Err(termination @ (RunTermination::TimedOut | RunTermination::Signalled(_))) => {
                return settlement(termination);
            }
            Err(error) => return Err(run_executor_error(error)),
        };
        drop(proof_directory);
        drop(proof_evidence);
        let child_executor = Arc::new(
            self.child_executor
                .with_pinned_child_authority(
                    &self.effective,
                    &self.confinement,
                    selection.connection(),
                )
                .with_live_child_selection(Arc::clone(&selection)),
        );
        let mcp_errors = bindings.mcp_errors().to_vec();
        let admitted_tools = bindings.admitted_tools();
        let mcp_runtime = bindings.mcp_runtime();
        let inherited_effect_tools = bindings.effect_tools();
        if !bindings.delegation_allowed() {
            self.registry
                .retain(|tool| !matches!(tool, RegisteredTool::Task | RegisteredTool::Skill(_)));
        }
        let sink = bindings.sink_factory.create(&model_capabilities);
        let started_capabilities = capabilities(
            &self.registry,
            self.system.is_some(),
            self.compiled.is_some(),
            self.no_hooks,
        );
        let instructions = self
            .system
            .as_ref()
            .map(|system| system.sources.clone())
            .unwrap_or_default();
        // The source and every compaction successor open with the same header.
        let session_started = |session| PublicEvent::SessionStarted {
            v: PUBLIC_EVENT_VERSION,
            session,
            capabilities: started_capabilities.clone(),
            mcp_errors: mcp_errors.clone(),
            hook_errors: Vec::new(),
            instructions: instructions.clone(),
        };
        sink.publish(&session_started(active.id()), &opening);

        let mode = self.effective.mode.unwrap_or(Mode::Default);
        let policy_selection =
            GenerationPolicySelection::new(mode, self.effective.permissions.clone());
        // The execution context, not this process, says whether a person is
        // reachable. A terminal run hands in nothing here and an unresolved
        // permission stays exit 3; an ACP run hands in the connected host, so
        // an escalation becomes a question the operator can answer instead of
        // an internal error the host can only render as a failure.
        let (profile_roots, config_notes) =
            super::profile_roots::admit_noted(&self.effective, self.confinement.workspace())
                .map_err(RunExecutorError::new)?;
        let policy_source: Arc<dyn GenerationPolicySource> = Arc::new(
            CliGenerationPolicySource::new(
                policy_selection.clone(),
                CanonPath::new(&self.root),
                bindings.human(),
            )
            .with_profile_roots(profile_roots)
            .with_read_perimeter(&self.confinement)
            .with_root_ceilings(super::profile_roots::write_ceilings(&self.effective).0),
        );
        let config = TurnConfig {
            cache: self.effective.cache,
            system: self.system,
            identity: Some(AgentIdentity {
                version: crate::build_info::VERSION.to_owned(),
                sandbox: self.confinement.level(),
                offline: self.confinement.offline(),
                host: bindings.host(),
                config_notes,
            }),
            token_ceiling: self.effective.ceilings.tokens,
            retry: self.retry,
            fallbacks: self.fallbacks,
        };
        let turns = Turns {
            parent_cancel: cancel.clone(),
            schema: self.compiled.as_ref(),
            retries: self.schema_retries,
            max_turns: self.max_turns,
            deadline: self.run_deadline,
        };
        let admission = Arc::clone(&self.admission);
        let invocation = self.run_context.invocation();
        let currency = CurrencyCode::new(SETTLEMENT).expect("a controlled currency");
        let gate = bindings.budget_gate().unwrap_or_else(|| {
            SharedBudgetGate::new(budget::gate(
                &self.effective.ceilings,
                self.effective.clamped.money,
                currency,
            ))
        });
        // A host that follows the transition gets the in-process compaction of
        // `ego run`; any other host keeps explicit recovery with `_ego/compact`.
        let compaction_host = !self.child && bindings.sink_factory.follows_session_transitions();
        // DEFERRED (2026-11-04) — only the first segment takes the host's
        // steering control: a `_ego/steer` accepted while the summary is
        // written is not delivered to the successor.
        let mut control = bindings.take_interactive_control();
        let mut opening = opening;
        let mut input = HeadlessInput::Prompt(prompt);
        let mut turns_taken: u32 = 0;
        let (termination, state, result) = loop {
            let (ports, observation, probe_evidence) = match effects.take() {
                Some(effects) => effects,
                None => match segment_effects(
                    &self.confinement,
                    self.run_deadline,
                    self.admitted_sandbox_observation.as_ref(),
                    (&active, &bindings, cancel),
                )
                .await
                {
                    Ok(effects) => effects,
                    Err(settled) => return settled,
                },
            };
            // Session roots cannot cross an in-process session transition.
            let automatic_compaction = (compaction_host && opening.session_roots.is_empty())
                .then_some(self.effective.compaction.threshold);
            let segment_turns = Turns {
                parent_cancel: turns.parent_cancel.clone(),
                schema: turns.schema,
                retries: turns.retries,
                max_turns: turns.max_turns.map(|max| max.saturating_sub(turns_taken)),
                deadline: turns.deadline,
            };
            let driven = drive_headless_turn(
                HeadlessSession {
                    active: &mut active,
                    opening,
                    clock: &clock,
                    sink: sink.as_ref(),
                    cancel,
                    pause,
                    control: control.take(),
                },
                HeadlessGeneration {
                    source: Arc::clone(&source),
                    model_ceiling: bindings.model_selection_ceiling().cloned(),
                    model_catalogue: bindings.model_catalogue(),
                    admission: Arc::clone(&admission),
                    invocation,
                    config: config.clone(),
                    provider_recovery: self.provider_recovery,
                    goal: self.goal.clone(),
                    goal_turns: self.goal_turns,
                    goal_check_command: self.goal_check_command.clone(),
                    goal_ceilings: self.effective.ceilings.clone(),
                    child_defaults: self.effective.task.clone(),
                    gate: gate.clone(),
                    hook_layers: self.profile_layers.as_deref(),
                    hooks_off: self.no_hooks,
                    hook_trust: self.trust,
                    policy_source: Arc::clone(&policy_source),
                    judge_available,
                },
                HeadlessChild {
                    worker: self.child,
                    parent_run: run_id,
                    root: child_root.clone(),
                    store: Arc::clone(&self.store),
                    executor: Arc::clone(&child_executor),
                    inherited_effect_tools: inherited_effect_tools.clone(),
                },
                HeadlessEffects {
                    mcp_runtime: mcp_runtime.clone(),
                    registry: &self.registry,
                    admitted_tools: admitted_tools.clone(),
                    cwd: &self.cwd,
                    confinement: &self.confinement,
                    run_deadline: self.run_deadline,
                    ports,
                    observation,
                    probe_evidence,
                    elicitor: bindings.elicitor.clone(),
                    root_grants: if !self.child
                        && bindings.human().is_some()
                        && !self.effective.deny_prompts
                    {
                        bindings.elicitor.as_ref().map(|elicitor| {
                            let target = self.effective.active_profile.as_ref().map_or(
                                UserConfigTarget::Base,
                                |profile| {
                                    UserConfigTarget::Profile(
                                        ego_core::config::ProfileName::parse(&profile.name).expect(
                                            "the active profile was validated at admission",
                                        ),
                                    )
                                },
                            );
                            Arc::new(SeatRootGrants {
                                resume_turn: false,
                                transition: None,
                                elicitor: Arc::clone(elicitor),
                                effective: self.effective.clone(),
                                workspace: CanonPath::new(&self.root),
                                rule_writer: crate::config::ego_home().and_then(|home| {
                                    let layers = self.profile_layers.as_ref()?;
                                    Some(Arc::new(
                                        crate::acp_permission::CliAcpPermissionRules::new(
                                            home.join("config.toml"),
                                            target.clone(),
                                        )
                                        .with_live_policy(
                                            policy_selection.clone(),
                                            Arc::new(FrozenRuleResolver::new(
                                                layers.clone(),
                                                self.trust,
                                                self.effective.active_profile.as_ref().map(
                                                    |profile| {
                                                        (profile.name.clone(), profile.selected_by)
                                                    },
                                                ),
                                                policy_selection.clone(),
                                            )),
                                        ),
                                    )
                                        as Arc<dyn ego_acp::permission::PermissionRuleWriter>)
                                }),
                                target,
                                store_root: self.store.root().to_path_buf(),
                            }) as Arc<dyn ego_core::turn::RootGrantPort>
                        })
                    } else {
                        None
                    },
                },
                HeadlessTurn {
                    turns: &segment_turns,
                    attachment_specs: &[],
                    input,
                    diagnostics: &Stderr,
                    automatic_compaction,
                },
            )
            .await;
            let (termination, state, result, compaction) = match driven {
                Ok(outcome) => outcome,
                Err(termination) => return run_settlement(termination, self.child),
            };
            let Some(compaction) = compaction else {
                break (termination, state, result);
            };
            active.replace_state(state);
            if let Err(termination) = activate_headless_successor(
                &mut active,
                &compaction.receipt,
                self.store.as_ref(),
                &clock,
                (self.admission.as_ref(), &Some(self.run_context.clone())),
            ) {
                return run_settlement(termination, self.child);
            }
            opening = active.state().clone();
            sink.publish(
                &PublicEvent::SessionCompacted {
                    source: compaction.receipt.source,
                    source_seq: compaction.receipt.source_seq,
                    target: active.id(),
                },
                &opening,
            );
            sink.publish(&session_started(active.id()), &opening);
            turns_taken = turns_taken.saturating_add(compaction.turns_taken);
            input = HeadlessInput::Checkpoint {
                request: compaction.request,
            };
        };
        active.replace_state(state);
        settle_run_artifact(termination, artifacts, run_id, result, self.child)
    }
}

/// Effect ports, exec observation and probe evidence of one session segment.
type SegmentEffects = (
    Arc<RunEffectPorts>,
    ExecObservation,
    Option<ego_core::store::evidence::EvidenceRef>,
);

/// The effect ports of the active session; an error is already the run's settlement.
async fn segment_effects(
    confinement: &RunConfinement,
    run_deadline: Option<std::time::Instant>,
    admitted_observation: Option<&ExecObservation>,
    (active, bindings, cancel): (&ActiveSession<'_>, &RunExecutionBindings, &CancelToken),
) -> Result<SegmentEffects, Result<RunSettlement, RunExecutorError>> {
    match run_effect_ports(
        confinement.clone(),
        active.evidence().clone(),
        cancel.clone(),
        run_deadline,
        admitted_observation,
        bindings.sandbox_grants(),
        bindings.residual_desk(),
    )
    .await
    {
        Ok(effects) => Ok(effects),
        Err(error @ ProcessError::ConfinementChanged { .. }) => {
            Err(Ok(RunSettlement::Interrupted {
                diagnostic: Some(error.to_string()),
            }))
        }
        Err(error) => Err(settlement(process_error_termination(
            error,
            confinement,
            cancel,
        ))),
    }
}

fn run_settlement(
    termination: RunTermination,
    child: bool,
) -> Result<RunSettlement, RunExecutorError> {
    if child {
        let stopped = match &termination {
            RunTermination::BudgetExceeded => Some("Budget"),
            RunTermination::MaxTurns => Some("MaxTurns"),
            RunTermination::TaskFailed => Some("TaskFailed"),
            _ => None,
        };
        if let Some(reason) = stopped {
            return Ok(RunSettlement::Interrupted {
                diagnostic: Some(format!("Child ended: {reason}")),
            });
        }
    }
    settlement(termination)
}

fn settle_run_artifact(
    termination: RunTermination,
    artifacts: Option<Arc<dyn RunArtifactStore>>,
    run_id: RunId,
    result: Value,
    child: bool,
) -> Result<RunSettlement, RunExecutorError> {
    let settlement = run_settlement(termination, child)?;
    if matches!(&settlement, RunSettlement::Completed)
        && let Some(artifacts) = artifacts
    {
        match artifacts
            .publish(run_id, public_result_artifact(result)?)
            .map_err(|error| RunExecutorError::new(error.to_string()))?
        {
            ArtifactPublication::PublishedDurably => {}
            ArtifactPublication::PublishedDurabilityUncertain { diagnostic } => {
                return Err(RunExecutorError::new(format!(
                    "the run artifact is available, but saving it to disk could not be confirmed: {diagnostic}"
                )));
            }
        }
    }
    Ok(settlement)
}

/// The exact interactive guard for the clean-session transition.
pub(super) fn compaction_availability(pending_work: bool) -> repl::actions::Availability {
    if pending_work {
        repl::actions::Availability::Busy
    } else {
        repl::actions::Availability::Enabled
    }
}

pub(super) enum InteractiveTurnInput {
    Checkpoint,
    Prompt {
        text: String,
        images: Option<Vec<ImageRef>>,
    },
    Skill(AdmittedSkillOperation),
    McpContext(repl::actions::McpContextCommand),
    Shell(String),
}

pub(super) struct ExecutionPreflight {
    pub(super) settings: Settings,
    pub(super) ci: Option<CiInvocation>,
    pub(super) chosen: Chosen,
    pub(super) refresh: ego_mcp::McpRegistryRefresh,
    pub(super) mcp_refusals: Vec<McpServerName>,
    pub(super) ready: Option<ExecutionReady>,
}

pub(super) struct ExecutionReady {
    pub(super) system: Option<SystemPrompt>,
    pub(super) registry: Vec<RegisteredTool>,
    pub(super) skills: Option<Arc<crate::skill::SkillCatalogue>>,
    pub(super) compiled: Option<schema::Schema>,
    pub(super) text: String,
    pub(super) generation: GenerationReady,
}

pub(super) struct GenerationReady {
    pub(super) source: Arc<model::CliGenerationModelSource>,
    pub(super) selection: Arc<model::GenerationSelection>,
    pub(super) main_capabilities: ModelCapabilities,
    pub(super) judge_available: bool,
}

/// Opaque result of the single direct-CI preflight pass.
pub struct DirectPreflight(pub(super) ExecutionPreflight);

pub(super) async fn select_startup_model(
    args: &Args,
    settings: &mut Settings,
    driver: &Driver<'_>,
    cancel: &CancelToken,
    diagnostics: &dyn Diagnostics,
) -> Result<(), RunTermination> {
    if args.model.is_some() || settings.effective.model.is_some() {
        return Ok(());
    }
    let home = config::ego_home();
    let observed = match home.as_ref() {
        Some(home) => crate::auth::AuthStore::new(home)
            .observed_provider_ids()
            .map_err(|error| RunTermination::UsageError {
                message: error.to_string(),
            })?,
        None => Default::default(),
    };
    let scope = crate::provider_http::CommandHttpScope::confined(
        settings.confinement.clone(),
        cancel.clone(),
        settings.run_deadline,
        None,
    )
    .await
    .map_err(|error| match error {
        crate::provider_http::McpProcessScopeError::Effects(error) => {
            process_error_termination(error, &settings.confinement, cancel)
        }
        crate::provider_http::McpProcessScopeError::Setup(message) => {
            RunTermination::Failed { message }
        }
    })?;
    let (snapshot, defaults) = models::build_startup_catalogue_snapshot(
        scope.http(),
        &crate::onboarding::ProcessEnvironment,
        &observed,
        settings.effective.connection.as_deref(),
        None,
        &settings.effective,
    )
    .await?;
    if !settings.effective.connections.is_empty() {
        let selected = ego_core::config::select_connection(
            &settings.effective.connections,
            settings.effective.connection.as_deref(),
            None,
        )
        .map_err(|message| RunTermination::UsageError { message })?;
        if let Some((_, connection)) = selected {
            let route = crate::auth::connection_route(connection);
            let model = defaults
                .get(&route)
                .ok_or_else(|| RunTermination::UsageError {
                    message: format!(
                        "selected account has no default model; pass --model for route {route}"
                    ),
                })?;
            settings.effective.model = Some(ModelRef(model.to_string()));
            return Ok(());
        }
    }
    let usable = snapshot
        .models
        .iter()
        .filter(|entry| entry.availability == ego_ai::catalogue::ModelAvailability::Available)
        .map(|entry| entry.slug.provider.clone())
        .collect();
    let seat = match driver {
        Driver::Interactive { seat, .. } if seat.has_human() => Some(seat),
        Driver::Once | Driver::Interactive { .. } => None,
    };
    let decision = crate::onboarding::decide_missing_model(
        Arc::new(snapshot),
        &defaults,
        &usable,
        settings.effective.thinking,
        seat.is_some(),
    );
    let selection = match decision {
        crate::onboarding::StartupDecision::Headless(error) => {
            return Err(RunTermination::UsageError {
                message: error.to_string(),
            });
        }
        crate::onboarding::StartupDecision::UseForRun(model) => {
            diagnostics.note(&format!(
                "using {}, the default model of the only usable provider, for this run; it is not saved; pass --model to pin a model",
                model.0
            ));
            settings.effective.model = Some(model);
            return Ok(());
        }
        crate::onboarding::StartupDecision::AutoSelect(model) => (Some(model), None),
        crate::onboarding::StartupDecision::Pick(snapshot, notice) => {
            let picker = Arc::new(Mutex::new(ModelPicker::for_startup(snapshot)));
            match seat
                .expect("an interactive decision has a human seat")
                .read_startup_model(picker, notice.map(|error| error.to_string()))
            {
                crate::model_picker::PickerOutcome::ModelSwitch(request) => (None, Some(request)),
                crate::model_picker::PickerOutcome::Cancelled => {
                    cancel.cancel(CancelCause::User);
                    return Err(cancelled_termination(cancel));
                }
            }
        }
    };
    let home = home.ok_or_else(|| RunTermination::UsageError {
        message: "the startup model selection needs EGO_HOME or HOME for persistent configuration"
            .to_owned(),
    })?;
    let config_store = OsUserConfigStore::new(Arc::new(OsIo), home.join("config.toml"));
    let (publication, selected_model, selected_thinking, automatic) = match selection {
        (Some(model), None) => {
            let publication = crate::onboarding::persist_startup_model(&config_store, &model)
                .map_err(|error| RunTermination::UsageError {
                    message: format!("could not persist the startup model configuration: {error}"),
                })?;
            (publication, model, None, true)
        }
        (None, Some(request)) => {
            let publication = crate::onboarding::persist_startup_selection(&config_store, &request)
                .map_err(|error| RunTermination::UsageError {
                    message: format!("could not persist the startup model configuration: {error}"),
                })?;
            (
                publication,
                request.target(),
                Some(request.thinking()),
                false,
            )
        }
        _ => unreachable!("startup selection has exactly one source"),
    };
    if let UserConfigPublication::PublishedDurabilityUncertain { diagnostic } = publication {
        diagnostics.note(&format!(
            "model defaults have changed, but saving them to disk could not be confirmed: {diagnostic}"
        ));
    }
    if automatic {
        diagnostics.note(&format!(
            "selected {} as the default model and saved it to user configuration",
            selected_model.0
        ));
    }
    settings.effective.model = Some(selected_model);
    if let Some(thinking) = selected_thinking {
        settings.effective.thinking = Some(thinking);
    }
    Ok(())
}

/// Complete direct-CI preflight before the process opens its MCP journals or
/// any durable session. Resume answers retain only settings and MCP selection
/// here so policy recovery can precede material and generation preparation.
pub async fn preflight_ci(
    args: &Args,
    cancel: &CancelToken,
) -> Result<DirectPreflight, RunTermination> {
    let mut status = None;
    let prepared =
        execution_preflight(args, &Driver::Once, cancel, &mut status, &Stderr, None).await?;
    debug_assert!(prepared.ci.is_some());
    Ok(DirectPreflight(prepared))
}

pub(super) async fn execution_preflight(
    args: &Args,
    driver: &Driver<'_>,
    cancel: &CancelToken,
    status: &mut Option<InteractiveStatus>,
    diagnostics: &dyn Diagnostics,
    mcp_registry: Option<Arc<McpRegistryController>>,
) -> Result<ExecutionPreflight, RunTermination> {
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };
    let home = config::ego_home();
    let mut settings = settings(
        args,
        |root| run_trust(args, driver, root, home.as_deref(), diagnostics),
        diagnostics,
    )
    .map_err(usage)?;
    let ci = ci_invocation(args, &settings, driver).map_err(usage)?;
    let chosen = chosen(
        args,
        &settings.root,
        ci.as_ref().map(|ci| ci.session_root.as_path()),
    )
    .map_err(usage)?;
    let mut resumed = args.clone();
    let resume_connection =
        super::execution::restore_resume_connection(&mut resumed, &chosen).map_err(usage)?;
    if resumed.connection != args.connection {
        let trust = settings.trust;
        settings = rooted_settings(&resumed, settings.cwd.clone(), |_| trust, diagnostics)
            .map_err(usage)?;
    }
    super::execution::admit_resume_connection(&resume_connection, &settings.effective, &chosen.id)
        .map_err(usage)?;
    let args = &resumed;
    admit_windows_unconfined_start(args, &mut settings, driver, home.as_deref(), diagnostics)
        .map_err(usage)?;
    if ci.is_none() {
        crate::session::start_cold_storage(
            Arc::clone(&chosen.store),
            &crate::session::cold_storage_config(
                &settings.effective.sessions,
                args.no_session_compression,
            ),
        );
    }
    if args.answers.is_empty() {
        select_startup_model(args, &mut settings, driver, cancel, diagnostics).await?;
    }
    let attachment = attach_mcp(
        args,
        &settings.effective,
        &settings.confinement,
        &settings.cwd,
    )
    .map_err(usage)?;
    if !args.quiet {
        note_mcp_attachment(diagnostics, &attachment);
    }
    settings.confinement =
        with_selected_mcp_executables(settings.confinement.clone(), &attachment.attached);
    let mcp_refusals = attachment
        .refused
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let selected_mcp = ego_tools::mcp::materialize_admissions(attachment.attached);
    let refresh = ego_mcp::McpRegistryRefresh::new(
        selected_mcp,
        ego_mcp::RegistryTrigger::ConfigChanged,
        ego_mcp::RegistryScope::All,
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    if !args.answers.is_empty() {
        return Ok(ExecutionPreflight {
            settings,
            ci,
            chosen,
            refresh,
            mcp_refusals,
            ready: None,
        });
    }
    let Materials {
        system,
        registry,
        skills,
        compiled,
        text,
        slug,
    } = materials(
        &settings,
        args,
        driver,
        Arc::new(chosen.store.history()),
        diagnostics,
    )
    .map_err(|err| {
        super::composition::expired(settings.run_deadline).unwrap_or_else(|| usage(err))
    })?;
    let GenerationPreflight {
        source,
        selection,
        capabilities: main_capabilities,
        judge_available,
        proof_directory,
        proof_evidence,
    } = generation_preflight(
        GenerationPreflightInput {
            proxy: args.proxy.clone(),
            session_id: SessionId::new(),
            confinement: &settings.confinement,
            effective: &settings.effective,
            connection_binding: None,
            slug,
            cancel,
            run_deadline: settings.run_deadline,
            mcp_registry,
            native_tools: &registry,
        },
        status,
    )
    .await
    .map_err(|termination| ci_preflight_termination(ci.is_some(), termination))?;
    drop(proof_directory);
    drop(proof_evidence);
    Ok(ExecutionPreflight {
        settings,
        ci,
        chosen,
        refresh,
        mcp_refusals,
        ready: Some(ExecutionReady {
            system,
            registry,
            skills,
            compiled,
            text,
            generation: GenerationReady {
                source,
                selection,
                main_capabilities,
                judge_available,
            },
        }),
    })
}

pub(super) fn admit_mcp_transports(
    confinement: &RunConfinement,
    selected: &[(McpServerName, McpServerConfig, McpConfigProvenance)],
) -> Result<(), UsageError> {
    for (name, config, _) in selected {
        match config {
            McpServerConfig::Http(_) if !confinement.admits_mcp() => {
                return Err(UsageError(format!(
                    "MCP HTTP server `{name}` is unavailable under --offline"
                )));
            }
            McpServerConfig::Stdio(_) if !confinement.admits_mcp() => {
                return Err(UsageError(format!(
                    "MCP stdio server `{name}` is unavailable under --offline"
                )));
            }
            McpServerConfig::Http(_) | McpServerConfig::Stdio(_) | McpServerConfig::Acp { .. } => {}
        }
    }
    Ok(())
}

pub(super) type McpSelection = Vec<(McpServerName, McpServerConfig, McpConfigProvenance)>;

/// The servers a session attaches: every explicit `--mcp` selection plus every
/// enabled configured server (none under `--mcp-only`), unless the run has no tools. Both pass
/// `admit_mcp_transports`. An explicit
/// refusal is a usage error; a refused configured server is returned in
/// `refused` with its cause so the session continues without it.
pub(super) struct McpAttachment {
    pub(super) attached: McpSelection,
    pub(super) refused: Vec<(McpServerName, UsageError)>,
}

pub(super) fn attach_mcp(
    args: &Args,
    effective: &Effective,
    confinement: &RunConfinement,
    cwd: &Path,
) -> Result<McpAttachment, UsageError> {
    if args.exclusive_read_only || selected_tools(args, effective).is_some_and(Vec::is_empty) {
        return Ok(McpAttachment {
            attached: Default::default(),
            refused: Vec::new(),
        });
    }
    let mut attached = config::selected_mcp(&args.mcp, effective, cwd)?;
    admit_mcp_transports(confinement, &attached)?;
    let mut refused = Vec::new();
    let configured = if args.mcp_only {
        Vec::new()
    } else {
        config::configured_mcp(effective)
    };
    for entry in configured {
        if attached.iter().any(|(name, _, _)| name == &entry.0) {
            continue;
        }
        match admit_mcp_transports(confinement, std::slice::from_ref(&entry)) {
            Ok(()) => attached.push(entry),
            Err(error) => refused.push((entry.0, error)),
        }
    }
    attached.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(McpAttachment { attached, refused })
}

/// The sandbox exec perimeter plus the exact canonical executable of each stdio server
/// this invocation attached (`--mcp NAME|@FILE` or an enabled configured entry).
///
/// The user chose these servers, so the sandbox must not refuse to start them. Each
/// command is resolved once, here, against the host `PATH`: no directory, wildcard or
/// later lookup is admitted. A command that does not resolve adds nothing and fails at
/// spawn with its own diagnostic. An argument that names an existing file (the server's
/// script) is admitted the same way.
pub(super) fn with_selected_mcp_executables(
    confinement: RunConfinement,
    attached: &McpSelection,
) -> RunConfinement {
    let mut paths = confinement.exec_paths().to_vec();
    for (_, config, _) in attached {
        let McpServerConfig::Stdio(stdio) = config else {
            continue;
        };
        // The server's script (`python server.py`, `node server.js`) is part of what
        // the user selected: an interpreter that cannot read it fails to start.
        // A bare argument is looked up in the working directory only, never on `PATH`.
        let candidates =
            std::iter::once(stdio.command.clone()).chain(stdio.args.iter().map(|arg| {
                if arg.contains('/') {
                    arg.clone()
                } else {
                    format!("./{arg}")
                }
            }));
        for candidate in candidates {
            if let Some(path) = resolve_executable(&candidate, stdio.cwd.as_deref()) {
                let path = CanonPath::new(path);
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    confinement.with_exec_paths(paths)
}

fn resolve_executable(command: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if command.contains('/') {
        let base = match cwd {
            Some(cwd) => cwd.to_path_buf(),
            None => std::env::current_dir().ok()?,
        };
        vec![base.join(command)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|directory| directory.join(command))
            .collect()
    };
    candidates
        .into_iter()
        .filter_map(|candidate| std::fs::canonicalize(candidate).ok())
        .find(|resolved| resolved.is_file())
}

/// Tell the user once why a configured server did not start, and which
/// attached server sends its OAuth credential unencrypted.
pub(super) fn note_mcp_attachment(diagnostics: &dyn Diagnostics, attachment: &McpAttachment) {
    for (name, error) in &attachment.refused {
        diagnostics.note(&format!("MCP server '{name}' was not connected: {error}"));
    }
    for (name, config, _) in &attachment.attached {
        if let McpServerConfig::Http(http) = config
            && let Some(warning) = crate::mcp::plaintext_credential_warning(name, http)
        {
            diagnostics.note(&warning);
        }
    }
}

#[cfg(all(test, unix))]
mod selected_mcp_exec_tests {
    use super::*;
    use ego_core::config::{McpStdioConfig, Source};
    use ego_core::permit::Confinement;
    use std::os::unix::fs::PermissionsExt;

    fn executable(directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let path = directory.join("srv");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::canonicalize(path).unwrap()
    }

    /// Catches: the sandbox refusing the server the user selected, and
    /// the selection widening the perimeter to a different file with the same name.
    #[test]
    fn only_the_selected_server_executable_joins_the_exec_perimeter() {
        let base = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(base.path()).unwrap().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let selected = executable(&base.path().join("selected"));
        let other = executable(&base.path().join("other"));
        let stdio = McpStdioConfig::new(selected.display().to_string(), vec![], None, vec![])
            .expect("stdio config");
        let attached: McpSelection = vec![(
            "srv".parse::<McpServerName>().unwrap(),
            McpServerConfig::Stdio(stdio),
            McpConfigProvenance::Layer {
                source: Source::Flags,
            },
        )];
        let before = ego_tools::exec::admit_confinement(
            Confinement::Workspace,
            false,
            CanonPath::new(&workspace),
        )
        .unwrap();
        let reachable = |policy: &RunConfinement, path: &Path| {
            ego_tools::exec::setup_exec_reachable(policy, &CanonPath::new(path)).unwrap()
        };
        assert!(!reachable(&before, &selected), "fixture must start refused");
        let after = with_selected_mcp_executables(before, &attached);
        assert!(reachable(&after, &selected));
        assert!(
            !reachable(&after, &other),
            "same basename elsewhere stays refused"
        );
        assert_eq!(after.exec_paths(), [CanonPath::new(&selected)]);
    }

    /// Catches: a selected `interpreter script` server failing to start
    /// under the workspace sandbox because its script sits outside the workspace, and the
    /// script's neighbours joining the perimeter with it.
    #[test]
    fn the_selected_server_script_joins_the_exec_perimeter() {
        let base = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(base.path()).unwrap().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let directory = std::fs::canonicalize(base.path()).unwrap().join("scripts");
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("server.py");
        std::fs::write(&script, "print()\n").unwrap();
        let neighbour = directory.join("other.py");
        std::fs::write(&neighbour, "print()\n").unwrap();
        let stdio = McpStdioConfig::new(
            "/bin/sh".to_owned(),
            vec![script.display().to_string(), "--flag".to_owned()],
            None,
            vec![],
        )
        .expect("stdio config");
        let attached: McpSelection = vec![(
            "srv".parse::<McpServerName>().unwrap(),
            McpServerConfig::Stdio(stdio),
            McpConfigProvenance::Layer {
                source: Source::Flags,
            },
        )];
        let before = ego_tools::exec::admit_confinement(
            Confinement::Workspace,
            false,
            CanonPath::new(&workspace),
        )
        .unwrap();
        let after = with_selected_mcp_executables(before, &attached);
        assert!(after.exec_paths().contains(&CanonPath::new(&script)));
        assert!(!after.exec_paths().contains(&CanonPath::new(&neighbour)));
    }

    /// Catches: a `python server.py` server with a `cwd` outside the
    /// workspace and a bare script name staying unstartable, because only arguments
    /// containing `/` were admitted.
    #[test]
    fn a_bare_script_name_resolves_in_the_server_cwd_only() {
        let base = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(base.path()).unwrap().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let directory = std::fs::canonicalize(base.path()).unwrap().join("scripts");
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("server.py");
        std::fs::write(&script, "print()\n").unwrap();
        std::fs::write(directory.join("other.py"), "print()\n").unwrap();
        let stdio = McpStdioConfig::new(
            "/bin/sh".to_owned(),
            vec!["server.py".to_owned(), "sh".to_owned()],
            Some(directory.clone()),
            vec![],
        )
        .expect("stdio config");
        let attached: McpSelection = vec![(
            "srv".parse::<McpServerName>().unwrap(),
            McpServerConfig::Stdio(stdio),
            McpConfigProvenance::Layer {
                source: Source::Flags,
            },
        )];
        let before = ego_tools::exec::admit_confinement(
            Confinement::Workspace,
            false,
            CanonPath::new(&workspace),
        )
        .unwrap();
        let after = with_selected_mcp_executables(before, &attached);
        assert!(after.exec_paths().contains(&CanonPath::new(&script)));
        assert!(
            !after
                .exec_paths()
                .contains(&CanonPath::new(directory.join("other.py")))
        );
        // `sh` as an argument must not resolve through PATH to /bin/sh's twin.
        assert_eq!(after.exec_paths().len(), 2);
    }
}
