//! Wire prepared CLI authority into executable run inputs.

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
    pub(super) fallbacks: Vec<ModelRef>,
    pub(super) max_turns: Option<u32>,
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
    /// The exact ACP session fact to compare during effect construction.
    pub(super) admitted_sandbox_observation: Option<ExecObservation>,
    pub(super) effective: Effective,
    pub(super) system: Option<SystemPrompt>,
    pub(super) registry: Vec<RegisteredTool>,
    pub(super) compiled: Option<schema::Schema>,
    pub(super) slug: String,
    pub(super) run_context: RunContextEvidence,
    pub(super) admission: Arc<EvidenceAdmissionService>,
    pub(super) child_executor: Arc<CliRunExecutor>,
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

impl CliPrepared {
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
        if let Some(model) = request.selections.model() {
            args.model = Some(model.0.clone());
        }
        if let Some(thinking) = request.selections.thinking() {
            args.thinking = Some(thinking);
        }
        if let Some(mode) = request.selections.mode() {
            args.mode = Some(mode);
        }
        // Before `rooted_settings` below, which is the point: the confinement
        // this becomes is what the MCP admission, the effect scope and the
        // child authority ceiling are all derived from, and none of them can
        // be re-decided once they exist.
        if let Some(sandbox) = request.selections.sandbox() {
            args.sandbox = Some(sandbox);
        }
        if let Some(budget) = request.selections.budget() {
            args.budget = Some(run_budget_spec(budget)?);
        }
        if let Some(profile) = request.selections.profile() {
            args.profile = Some(profile.to_owned());
            args.profile_source = Some(ego_core::config::Source::Flags);
        }
        if args.profile.as_deref() == Some("ci") {
            return Err(RunTermination::UsageError {
                message: "profile ci available only through ego run or ego resume".to_owned(),
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
        } = prepared_materials(&settings, &args, String::new(), &Stderr).map_err(usage)?;
        let system = if child { None } else { system };
        let compiled = if child { None } else { compiled };
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
                store,
                artifacts,
                run_id: request.run_id,
                session_id: request.run_id.session_id(),
                schema_retries,
                retry,
                fallbacks,
                max_turns,
                run_deadline,
                root,
                cwd,
                confinement,
                trust,
                profile_layers,
                admitted_sandbox_observation: request.selections.sandbox_observation().cloned(),
                effective,
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
        // The operator seat speaks before anything reads the configuration.
        //
        // A protocol session continues one run across prompts, so its
        // selections were admitted once and cannot change; the seat is the
        // only channel a live picker has. Applying it here — ahead of the
        // model preflight and the policy selection below — is what makes a
        // change land at the next generation boundary rather than at the next
        // session. Only what the operator explicitly chose is applied: an
        // inherited configuration default is not a decision to re-record.
        if let Some(controls) = bindings.controls() {
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
        let run_id = self.run_id;
        let artifacts = self.artifacts.clone();
        let child_root = RunRoot::new(self.root.clone())
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let child_executor = Arc::new(
            self.child_executor
                .with_child_authority(&self.effective, &self.confinement),
        );
        let clock = SystemClock;
        let opened = Opened::Durable(Box::new(
            session::open(self.store.as_ref(), &self.session_id, &clock)
                .map_err(|error| RunExecutorError::new(error.to_string()))?,
        ));
        let repair_note = opened.repair_note();
        let opening = opened.state();
        let mut active = ActiveSession::durable(opened, opening.clone(), EVIDENCE_CAP)
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let (ports, observation, probe_evidence) = match run_effect_ports(
            self.confinement.clone(),
            active.evidence().clone(),
            cancel.clone(),
            self.run_deadline,
            self.admitted_sandbox_observation.as_ref(),
            bindings.sandbox_grants(),
        )
        .await
        {
            Ok(effects) => effects,
            Err(error @ ProcessError::ConfinementChanged { .. }) => {
                return Ok(RunSettlement::Interrupted {
                    diagnostic: Some(error.to_string()),
                });
            }
            Err(error) => {
                return settlement(process_error_termination(error, &self.confinement, cancel));
            }
        };
        if let Some(note_text) = repair_note {
            Stderr.note(&note_text);
        }
        let mut no_status = None;
        let preflight = generation_preflight(
            GenerationPreflightInput {
                session_id: self.session_id,
                confinement: &self.confinement,
                effective: &self.effective,
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
            selection: _,
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
            Err(error) => return Err(run_executor_error(error)),
        };
        drop(proof_directory);
        drop(proof_evidence);
        let mcp_errors = bindings.mcp_errors().to_vec();
        let admitted_tools = bindings.admitted_tools();
        let mcp_runtime = bindings.mcp_runtime();
        let inherited_effect_tools = bindings.effect_tools();
        let sink = bindings.sink_factory.create(&model_capabilities);
        sink.publish(
            &PublicEvent::SessionStarted {
                v: PUBLIC_EVENT_VERSION,
                session: active.id(),
                capabilities: capabilities(
                    &self.registry,
                    self.system.is_some(),
                    self.compiled.is_some(),
                ),
                mcp_errors,
                hook_errors: Vec::new(),
                instructions: self
                    .system
                    .as_ref()
                    .map(|system| system.sources.clone())
                    .unwrap_or_default(),
            },
            &opening,
        );

        let mode = self.effective.mode.unwrap_or(Mode::Default);
        let policy_selection =
            GenerationPolicySelection::new(mode, self.effective.permissions.clone());
        // The execution context, not this process, says whether a person is
        // reachable. A terminal run hands in nothing here and an unresolved
        // permission stays exit 3; an ACP run hands in the connected host, so
        // an escalation becomes a question the operator can answer instead of
        // an internal error the host can only render as a failure.
        let profile_roots =
            super::profile_roots::admit(&self.effective).map_err(RunExecutorError::new)?;
        let policy_source: Arc<dyn GenerationPolicySource> = Arc::new(
            CliGenerationPolicySource::new(
                policy_selection,
                CanonPath::new(&self.root),
                bindings.human(),
            )
            .with_profile_roots(profile_roots),
        );
        let config = TurnConfig {
            system: self.system,
            identity: Some(AgentIdentity {
                version: crate::build_info::VERSION.to_owned(),
                sandbox: self.confinement.level(),
                offline: self.confinement.offline(),
                host: bindings.host(),
            }),
            token_ceiling: self.effective.ceilings.tokens,
            retry: self.retry,
            fallbacks: self.fallbacks,
        };
        let turns = Turns {
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
        let (termination, state, result) = match drive_headless_turn(
            HeadlessSession {
                active: &mut active,
                opening,
                clock: &clock,
                sink: sink.as_ref(),
                cancel,
                pause,
            },
            HeadlessGeneration {
                source,
                admission,
                invocation,
                config,
                gate,
                hook_layers: self.profile_layers.as_deref(),
                hook_trust: self.trust,
                policy_source,
                judge_available,
            },
            HeadlessChild {
                parent_run: run_id,
                root: child_root,
                store: Arc::clone(&self.store),
                executor: child_executor,
                inherited_effect_tools,
            },
            HeadlessEffects {
                mcp_runtime,
                registry: &self.registry,
                admitted_tools,
                cwd: &self.cwd,
                confinement: &self.confinement,
                run_deadline: self.run_deadline,
                ports,
                observation,
                probe_evidence,
                elicitor: bindings.elicitor,
            },
            HeadlessTurn {
                turns: &turns,
                attachment_specs: &[],
                input: HeadlessInput::Prompt(prompt),
                diagnostics: &Stderr,
            },
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(termination) => return settlement(termination),
        };
        active.replace_state(state);
        let settlement = settlement(termination)?;
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
                        "run artifact is visible but its durability is uncertain: {diagnostic}"
                    )));
                }
            }
        }
        Ok(settlement)
    }
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
    Prompt(String),
    Skill(AdmittedSkillOperation),
}

pub(super) struct ExecutionPreflight {
    pub(super) settings: Settings,
    pub(super) ci: Option<CiInvocation>,
    pub(super) chosen: Chosen,
    pub(super) refresh: ego_mcp::McpRegistryRefresh,
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
    )
    .await?;
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
            "model defaults are authoritative with a durability warning: {diagnostic}"
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
    admit_windows_unconfined_start(args, &mut settings, driver, home.as_deref(), diagnostics)
        .map_err(usage)?;
    let ci = ci_invocation(args, &settings, driver).map_err(usage)?;
    let chosen = chosen(
        args,
        &settings.root,
        ci.as_ref().map(|ci| ci.session_root.as_path()),
    )
    .map_err(usage)?;
    if args.answers.is_empty() {
        select_startup_model(args, &mut settings, driver, cancel, diagnostics).await?;
    }
    let selected_mcp =
        config::selected_mcp(args, &settings.effective, &settings.cwd).map_err(usage)?;
    admit_mcp_transports(&settings.confinement, &selected_mcp).map_err(usage)?;
    let selected_mcp = ego_tools::mcp::materialize_admissions(selected_mcp);
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
        matches!(driver, Driver::Interactive { .. }),
        diagnostics,
    )
    .map_err(usage)?;
    let GenerationPreflight {
        source,
        selection,
        capabilities: main_capabilities,
        judge_available,
        proof_directory,
        proof_evidence,
    } = generation_preflight(
        GenerationPreflightInput {
            session_id: SessionId::new(),
            confinement: &settings.confinement,
            effective: &settings.effective,
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
            McpServerConfig::Http(_) if !confinement.admits_mcp_http() && confinement.offline() => {
                return Err(UsageError(format!(
                    "MCP HTTP server `{name}` is unavailable under --offline"
                )));
            }
            McpServerConfig::Http(_) if !confinement.admits_mcp_http() => {
                return Err(UsageError(format!(
                    "MCP HTTP server `{name}` requires --sandbox off"
                )));
            }
            McpServerConfig::Stdio(_) if !confinement.admits_mcp_stdio() => {
                return Err(UsageError(format!(
                    "MCP stdio server `{name}` is unavailable under --offline"
                )));
            }
            McpServerConfig::Http(_) | McpServerConfig::Stdio(_) | McpServerConfig::Acp { .. } => {}
        }
    }
    Ok(())
}
