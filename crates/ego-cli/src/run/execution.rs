//! Advance one run through its ordered startup phases and into its driver.

use super::*;

struct ResolvedInvocation {
    settings: Option<Settings>,
    ci: Option<CiInvocation>,
    refresh: Option<ego_mcp::McpRegistryRefresh>,
    ready: Option<ExecutionReady>,
    auto_preflight: Option<Arc<CliAutoPreflight>>,
    chosen_session: Option<Chosen>,
}

async fn resolve_invocation_phase(
    args: &Args,
    driver: &Driver<'_>,
    diagnostics: &dyn Diagnostics,
    cancel: &CancelToken,
    preflight: Option<ExecutionPreflight>,
) -> Result<ResolvedInvocation, RunTermination> {
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };
    match preflight {
        Some(ExecutionPreflight {
            settings,
            ci,
            refresh,
            ready,
            chosen,
        }) => Ok(ResolvedInvocation {
            settings: Some(settings),
            ci,
            refresh: Some(refresh),
            ready,
            auto_preflight: None,
            chosen_session: Some(chosen),
        }),
        None => {
            let home = config::ego_home();
            let mut settings = settings(
                args,
                |root| run_trust(args, driver, root, home.as_deref(), diagnostics),
                diagnostics,
            )
            .map_err(usage)?;
            admit_windows_unconfined_start(
                args,
                &mut settings,
                driver,
                home.as_deref(),
                diagnostics,
            )
            .map_err(usage)?;
            enable_trusted_interactive_auto(&mut settings, driver);
            let auto_preflight = Arc::new(CliAutoPreflight::capture(
                &settings.effective,
                &settings.confinement,
            ));
            let ci = ci_invocation(args, &settings, driver).map_err(usage)?;
            if ci.is_some() {
                return Err(RunTermination::Failed {
                    message: "direct CI execution is missing its preflight token".to_owned(),
                });
            }
            let chosen_session = chosen(args, &settings.root, None).map_err(usage)?;
            let recorded_model = if matches!(args.session, crate::cli::SessionChoice::Resume(_)) {
                chosen_session
                    .store
                    .list()
                    .map_err(|error| RunTermination::Failed {
                        message: format!("could not read the resumed session model: {error}"),
                    })?
                    .into_iter()
                    .find(|session| session.id == chosen_session.id)
                    .and_then(|session| session.model)
                    .map(ModelRef)
            } else {
                None
            };
            if settings.effective.model.is_none() && args.model.is_none() {
                settings.effective.model = recorded_model.clone();
            }
            select_startup_model(args, &mut settings, driver, cancel, diagnostics).await?;
            if !args.quiet
                && let Some(previous) = recorded_model.as_ref()
                && let Some(selected) = settings.effective.model.as_ref()
                && previous != selected
            {
                diagnostics.note(&format!(
                    "Resume model changed: {previous} -> {selected} (current configuration or flags)."
                ));
            }
            let attachment = attach_mcp(
                args,
                &settings.effective,
                &settings.confinement,
                &settings.cwd,
            )
            .map_err(usage)?;
            if !args.quiet {
                note_refused_mcp(diagnostics, &attachment.refused);
            }
            let selected_mcp = attachment.attached;
            let selected_mcp = ego_tools::mcp::materialize_admissions(selected_mcp);
            let refresh = ego_mcp::McpRegistryRefresh::new(
                selected_mcp,
                ego_mcp::RegistryTrigger::ConfigChanged,
                ego_mcp::RegistryScope::All,
            )
            .map_err(|error| RunTermination::Failed {
                message: error.to_string(),
            })?;
            if settings.effective.mode.unwrap_or(Mode::Default) == Mode::Auto {
                auto_preflight.validate_startup()?;
            }
            Ok(ResolvedInvocation {
                settings: Some(settings),
                ci,
                refresh: Some(refresh),
                ready: None,
                auto_preflight: Some(auto_preflight),
                chosen_session: Some(chosen_session),
            })
        }
    }
}

/// The terminal's normal trusted-workspace flow is automatic analysis. An
/// explicit command/config choice remains authoritative, and noninteractive
/// runs keep their existing default because they have no human fallback.
fn enable_trusted_interactive_auto(settings: &mut Settings, driver: &Driver<'_>) {
    if settings.trust == Trust::Trusted
        && settings.effective.mode.is_none()
        && settings
            .effective
            .max_mode
            .is_none_or(|maximum| Mode::Auto.within(maximum))
        && matches!(driver, Driver::Interactive { seat, .. } if seat.has_human())
    {
        settings.effective.mode = Some(Mode::Auto);
        settings.effective.mode_provenance =
            ego_core::config::SettingProvenance::TrustedInteractive;
    }
}

pub(super) struct ScopedInvocation {
    pub(super) mcp_scope: crate::provider_http::McpProcessScope,
    pub(super) mcp_runtime: Arc<ego_mcp::McpRegistryRuntime>,
    pub(super) mcp_registry: Arc<McpRegistryController>,
    pub(super) artifact_store: Option<JsonlStore>,
    pub(super) jsonl_store: Arc<JsonlStore>,
    pub(super) session_id: SessionId,
    pub(super) clock: SystemClock,
}

async fn open_scope_phase(
    args: &mut Args,
    host: &RunHost<'_>,
    cancel: &CancelToken,
    resolved: &mut ResolvedInvocation,
) -> Result<ScopedInvocation, RunTermination> {
    let settings = resolved
        .settings
        .as_ref()
        .expect("the resolved invocation owns its settings until materialization");
    if !args.quiet
        && let Driver::Interactive { seat, .. } = &host.driver
        && seat.has_human()
        && let Some(model) = args.model.as_deref().or_else(|| {
            settings
                .effective
                .model
                .as_ref()
                .map(|model| model.0.as_str())
        })
    {
        let effective = &settings.effective;
        host.diagnostics.note(&format!(
            "model {model}\nmode {} ({}) · sandbox {} ({})\n{}",
            effective.mode.unwrap_or(Mode::Default),
            effective.mode_provenance,
            settings.confinement.level(),
            effective.sandbox_provenance,
            seat.key_hints()
        ));
    }
    let mcp_scope = crate::provider_http::McpProcessScope::for_run(
        settings.confinement.clone(),
        cancel.clone(),
        settings.run_deadline,
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
    let mcp_runtime = Arc::clone(mcp_scope.runtime());
    let mcp_registry = Arc::clone(mcp_runtime.controller());
    if let Some(ci) = &resolved.ci {
        args.session_dir = Some(ci.session_root.clone());
    }
    let artifact_store = resolved
        .ci
        .as_ref()
        .map(|ci| session::jsonl(ci.artifact_root.clone(), settings.root.clone()));
    let Chosen {
        store: jsonl_store,
        id: session_id,
    } = resolved
        .chosen_session
        .take()
        .expect("the effect scope consumes the chosen session once");
    Ok(ScopedInvocation {
        mcp_scope,
        mcp_runtime,
        mcp_registry,
        artifact_store,
        jsonl_store,
        session_id,
        clock: SystemClock,
    })
}

struct ResumePreparation<'store> {
    early_opened: Option<Opened<'store>>,
    mode: Mode,
    policy_selection: GenerationPolicySelection,
    policy_source: Arc<dyn GenerationPolicySource>,
    perimeter: crate::perimeter::Perimeter,
    invocation: RunInvocationId,
    run_id: RunId,
    resume_plan: Option<ResumePlan>,
    recovered_capture: Option<ego_core::turn::GenerationPolicyCapture>,
}

fn prepare_resume_phase<'store>(
    args: &Args,
    host: &RunHost<'_>,
    scoped: &'store ScopedInvocation,
    resolved: &ResolvedInvocation,
) -> Result<ResumePreparation<'store>, RunTermination> {
    let settings = resolved
        .settings
        .as_ref()
        .expect("resume preparation observes resolved settings before materialization");
    let direct_ci_ready = resolved.ready.is_some();
    let terminal_resume = matches!(host.driver, Driver::Interactive { .. })
        && matches!(args.session, crate::cli::SessionChoice::Resume(_));
    let early_opened = if direct_ci_ready || !args.answers.is_empty() || terminal_resume {
        Some(Opened::Durable(Box::new(
            session::open(
                scoped.jsonl_store.as_ref(),
                &scoped.session_id,
                &scoped.clock,
            )
            .map_err(|err| RunTermination::Failed {
                message: err.to_string(),
            })?,
        )))
    } else {
        None
    };
    let mode = settings.effective.mode.unwrap_or(Mode::Default);
    let human: Option<Arc<dyn HumanApprover>> = match &host.driver {
        Driver::Interactive { seat, .. } if seat.has_human() => Some(Arc::new(SeatApprover {
            seat: Arc::clone(seat),
        })),
        Driver::Once | Driver::Interactive { .. } => None,
    };
    let policy_selection =
        GenerationPolicySelection::new(mode, settings.effective.permissions.clone());
    let profile_roots =
        super::profile_roots::admit(&settings.effective, settings.confinement.workspace())
            .map_err(|message| RunTermination::Failed { message })?;
    let perimeter = crate::perimeter::Perimeter::session(
        &settings.confinement,
        &settings.effective,
        &profile_roots,
    );
    let policy_source: Arc<dyn GenerationPolicySource> = Arc::new(
        CliGenerationPolicySource::new(
            policy_selection.clone(),
            CanonPath::new(&settings.root),
            human,
        )
        .with_profile_roots(profile_roots)
        .with_read_perimeter(&settings.confinement)
        .with_coordinator_profile(settings.effective.active_profile.as_ref()),
    );
    let invocation = RunInvocationId::new();
    let run_id = RunId::new(scoped.session_id, invocation.uuid());
    let finalize = |termination| {
        finalize_ci_error(
            scoped.jsonl_store.as_ref(),
            scoped.artifact_store.as_ref(),
            run_id,
            termination,
            host.diagnostics,
        )
    };
    let resume_plan = if args.answers.is_empty() {
        if terminal_resume {
            early_opened
                .as_ref()
                .and_then(|opened| plan_resume_answer(&opened.state(), &[]).ok())
        } else {
            None
        }
    } else {
        let opened = early_opened
            .as_ref()
            .expect("a resume answer opens its session before planning");
        Some(plan_resume_answer(&opened.state(), &args.answers).map_err(&finalize)?)
    };
    let recovered_capture = match resume_plan.as_ref() {
        Some(ResumePlan::Foreign {
            continuation_id, ..
        }) => {
            let snapshot = scoped
                .mcp_runtime
                .load_pending_foreign_policy(continuation_id)
                .map_err(|message| finalize(RunTermination::Failed { message }))?;
            Some(
                policy_source
                    .restore(snapshot)
                    .map_err(|message| finalize(RunTermination::Failed { message }))?,
            )
        }
        Some(ResumePlan::Model { .. }) | None => None,
    };
    Ok(ResumePreparation {
        early_opened,
        mode,
        policy_selection,
        policy_source,
        perimeter,
        invocation,
        run_id,
        resume_plan,
        recovered_capture,
    })
}

pub(super) struct MaterializedRun<'store> {
    pub(super) opened: Option<Opened<'store>>,
    pub(super) system: Option<SystemPrompt>,
    pub(super) registry: Vec<RegisteredTool>,
    pub(super) skills: Option<Arc<crate::skill::SkillCatalogue>>,
    pub(super) compiled: Option<schema::Schema>,
    pub(super) text: String,
    pub(super) slug: Option<String>,
    pub(super) generation: Option<GenerationReady>,
    pub(super) skill_trust: Trust,
    pub(super) skill_profile_layers: Option<Vec<Sourced<ConfigLayer>>>,
    pub(super) run_deadline: Option<Instant>,
    pub(super) root: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) confinement: RunConfinement,
    pub(super) effective: Effective,
    pub(super) skill_resource_roots: Option<ego_core::skill::SkillResourceRoots>,
    pub(super) currency: CurrencyCode,
    pub(super) child_root: RunRoot,
    pub(super) child_executor: Arc<CliRunExecutor>,
    pub(super) context: Option<RunContextEvidence>,
    pub(super) mode: Mode,
    pub(super) policy_selection: GenerationPolicySelection,
    pub(super) policy_source: Arc<dyn GenerationPolicySource>,
    pub(super) perimeter: crate::perimeter::Perimeter,
    pub(super) invocation: RunInvocationId,
    pub(super) run_id: RunId,
    pub(super) resume_plan: Option<ResumePlan>,
    pub(super) recovered_capture: Option<ego_core::turn::GenerationPolicyCapture>,
}

fn materialize_run_phase<'store>(
    args: &Args,
    host: &RunHost<'_>,
    scoped: &'store ScopedInvocation,
    resolved: &mut ResolvedInvocation,
    resume: ResumePreparation<'store>,
) -> Result<MaterializedRun<'store>, RunTermination> {
    let settings = resolved
        .settings
        .take()
        .expect("materialization consumes resolved settings once");
    let ready = resolved.ready.take();
    let ResumePreparation {
        early_opened,
        mode,
        policy_selection,
        policy_source,
        perimeter,
        invocation,
        run_id,
        resume_plan,
        recovered_capture,
    } = resume;
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };
    let (system, registry, skills, compiled, text, slug, generation) = match ready {
        Some(ExecutionReady {
            system,
            registry,
            skills,
            compiled,
            text,
            generation,
        }) => (
            system,
            registry,
            skills,
            compiled,
            text,
            None,
            Some(generation),
        ),
        None => {
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
                matches!(&host.driver, Driver::Interactive { .. }),
                Arc::new(scoped.jsonl_store.history()),
                host.diagnostics,
            )
            .map_err(usage)?;
            (system, registry, skills, compiled, text, Some(slug), None)
        }
    };
    let skill_trust = settings.trust;
    let skill_profile_layers = settings.profile_layers.clone();
    let Settings {
        run_deadline,
        root,
        cwd,
        confinement,
        effective,
        ..
    } = settings;
    let skill_resource_roots = crate::config::ego_home().map(|user| {
        ego_core::skill::SkillResourceRoots::new(PathBuf::from("/etc/ego"), user, root.join(".ego"))
    });
    let currency = CurrencyCode::new(SETTLEMENT).expect("a constant this file controls");
    let opened = match early_opened {
        Some(opened) => opened,
        None => Opened::Durable(Box::new(
            session::open(
                scoped.jsonl_store.as_ref(),
                &scoped.session_id,
                &scoped.clock,
            )
            .map_err(|err| RunTermination::Failed {
                message: err.to_string(),
            })?,
        )),
    };
    let child_root = RunRoot::new(root.clone()).map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    let child_artifacts: Arc<dyn RunArtifactStore> = scoped.jsonl_store.clone();
    let child_executor = Arc::new(CliRunExecutor {
        args: args.clone(),
        store: Arc::clone(&scoped.jsonl_store),
        concurrency: None,
        frozen: Some(Arc::new(
            config::FrozenRunConfig::capture(&root, args, skill_trust).map_err(usage)?,
        )),
        artifacts: Some(child_artifacts),
        admission: Arc::clone(&host.evidence),
        child_authority: Some(Arc::new(ChildAuthorityCeiling::from_parent(
            &effective,
            &confinement,
        ))),
        child: true,
        token_family: None,
    });
    let instruction_descriptors = system
        .as_ref()
        .map(|prompt| prompt.sources.clone())
        .unwrap_or_default();
    let context = complete_run_context(
        args,
        &effective,
        &confinement,
        &cwd,
        invocation,
        run_id,
        instruction_descriptors,
    )
    .map_err(usage)?;
    Ok(MaterializedRun {
        opened: Some(opened),
        system,
        registry,
        skills,
        compiled,
        text,
        slug,
        generation,
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
        context: Some(context),
        mode,
        policy_selection,
        policy_source,
        perimeter,
        invocation,
        run_id,
        resume_plan,
        recovered_capture,
    })
}

struct AdmittedRun<'store> {
    opened: Opened<'store>,
    resume: Option<RuntimeResume>,
    mcp_errors: Vec<String>,
    admitted_tools: Option<Arc<dyn AdmittedToolSource>>,
}

async fn admit_run_phase<'store>(
    host: &RunHost<'_>,
    scoped: &ScopedInvocation,
    resolved: &mut ResolvedInvocation,
    materialized: &mut MaterializedRun<'store>,
) -> Result<AdmittedRun<'store>, RunTermination> {
    let run_id = materialized.run_id;
    let finalize = |termination| {
        finalize_ci_error(
            scoped.jsonl_store.as_ref(),
            scoped.artifact_store.as_ref(),
            run_id,
            termination,
            host.diagnostics,
        )
    };
    let admit_context = |opened: &Opened<'_>, context: RunContextEvidence| {
        let Opened::Durable(opened_session) = opened;
        match host
            .evidence
            .admit_run_context(&opened_session.writer, context)
        {
            Ok(EvidenceWrite::PublishedWithProjectionWarning { diagnostic }) => {
                host.diagnostics.note(&format!(
                    "run evidence was durable but its manifest projection failed: {diagnostic}"
                ));
                Ok(())
            }
            Ok(EvidenceWrite::Published | EvidenceWrite::AlreadyRecorded) => Ok(()),
            Err(error) => Err(RunTermination::Failed {
                message: error.to_string(),
            }),
        }
    };
    let (mut resume, foreign_plan) = match materialized.resume_plan.take() {
        Some(ResumePlan::Model {
            answer,
            continuation,
            request_event,
        }) => {
            let continuation = apply_model_resume_once(
                materialized
                    .opened
                    .as_mut()
                    .expect("the materialized run owns its opened session"),
                answer,
                continuation,
                request_event,
            )
            .map_err(&finalize)?;
            (Some(RuntimeResume::Model(continuation.turn)), None)
        }
        Some(plan @ ResumePlan::Foreign { .. }) => (None, Some(plan)),
        None => (None, None),
    };
    scoped
        .mcp_registry
        .reconcile(
            resolved
                .refresh
                .take()
                .expect("run admission consumes the registry refresh once"),
        )
        .await
        .map_err(|error| {
            finalize(RunTermination::Failed {
                message: error.to_string(),
            })
        })?;
    match (materialized.recovered_capture.take(), foreign_plan) {
        (None, None) => admit_context(
            materialized
                .opened
                .as_ref()
                .expect("the materialized run owns its opened session"),
            materialized
                .context
                .clone()
                .expect("the run context is admitted exactly once"),
        )?,
        (
            Some(capture),
            Some(ResumePlan::Foreign {
                question,
                continuation_id,
                answer,
            }),
        ) => {
            scoped
                .mcp_runtime
                .bind_pending_foreign_continuation(&continuation_id, capture.snapshot())
                .map_err(|message| finalize(RunTermination::Failed { message }))?;
            admit_context(
                materialized
                    .opened
                    .as_ref()
                    .expect("the materialized run owns its opened session"),
                materialized
                    .context
                    .clone()
                    .expect("the foreign run context is admitted after recovery"),
            )?;
            resume = Some(RuntimeResume::Foreign {
                question,
                answer,
                policy: capture.policy(),
            });
        }
        (None, Some(ResumePlan::Foreign { .. })) => {
            unreachable!("foreign recovery always restores its policy capture")
        }
        (Some(_), None) | (Some(_), Some(ResumePlan::Model { .. })) => {
            unreachable!("only foreign recovery restores a policy capture")
        }
        (None, Some(ResumePlan::Model { .. })) => {
            unreachable!("model recovery is converted before context admission")
        }
    }
    if let Some(status) = host.status.as_ref() {
        status
            .seat
            .action_registry()
            .bind_mcp_view(scoped.mcp_runtime.clone());
    }
    if let (Some(status), Some(skills)) = (host.status.as_ref(), materialized.skills.as_ref()) {
        status
            .seat
            .action_registry()
            .bind_skill_view(skills.view())
            .map_err(|error| RunTermination::Failed {
                message: format!("interactive skill actions could not be installed: {error}"),
            })?;
    }
    if let Some(note_text) = materialized
        .opened
        .as_ref()
        .expect("the materialized run owns its opened session")
        .repair_note()
    {
        host.diagnostics.note(&note_text);
    }
    if !materialized.text.trim().is_empty()
        && let Driver::Interactive { seat, .. } = &host.driver
    {
        seat.busy();
    }
    debug_assert!(materialized.context.is_some());
    let mcp_status = scoped.mcp_registry.status();
    Ok(AdmittedRun {
        opened: materialized
            .opened
            .take()
            .expect("the admitted run takes its opened session once"),
        resume,
        mcp_errors: mcp_status.mcp_errors(),
        admitted_tools: Some(scoped.mcp_registry.admitted_tools()),
    })
}

async fn prepare_generation_phase(
    host: &mut RunHost<'_>,
    cancel: &CancelToken,
    scoped: &ScopedInvocation,
    resolved: &ResolvedInvocation,
    materialized: &MaterializedRun<'_>,
) -> Result<(GenerationReady, Option<Arc<PolicyApplicationOwner>>), RunTermination> {
    let generation = match &materialized.generation {
        Some(generation) => GenerationReady {
            source: Arc::clone(&generation.source),
            selection: Arc::clone(&generation.selection),
            main_capabilities: generation.main_capabilities.clone(),
            judge_available: generation.judge_available,
        },
        None => {
            let GenerationPreflight {
                source,
                selection,
                capabilities: main_capabilities,
                judge_available,
                proof_directory,
                proof_evidence,
            } = generation_preflight(
                GenerationPreflightInput {
                    session_id: scoped.session_id,
                    confinement: &materialized.confinement,
                    effective: &materialized.effective,
                    slug: materialized
                        .slug
                        .clone()
                        .expect("ordinary execution prepared its model slug"),
                    cancel,
                    run_deadline: materialized.run_deadline,
                    mcp_registry: Some(Arc::clone(&scoped.mcp_registry)),
                    native_tools: &materialized.registry,
                },
                &mut host.status,
            )
            .await
            .map_err(|termination| {
                finalize_ci_error(
                    scoped.jsonl_store.as_ref(),
                    scoped.artifact_store.as_ref(),
                    materialized.run_id,
                    ci_preflight_termination(resolved.ci.is_some(), termination),
                    host.diagnostics,
                )
            })?;
            drop(proof_directory);
            drop(proof_evidence);
            GenerationReady {
                source,
                selection,
                main_capabilities,
                judge_available,
            }
        }
    };
    let policy_application = match (
        host.status.as_ref().filter(|status| status.human),
        resolved.auto_preflight.as_ref(),
        materialized.skill_profile_layers.as_ref(),
    ) {
        (Some(status), Some(preflight), Some(layers)) => {
            status.model_config_store.as_ref().map(|store| {
                let preflight = Arc::clone(preflight) as Arc<dyn PreflightPort>;
                let resolver = Arc::new(FrozenRuleResolver::new(
                    layers.clone(),
                    materialized.skill_trust,
                    materialized
                        .effective
                        .active_profile
                        .as_ref()
                        .map(|profile| (profile.name.clone(), profile.selected_by)),
                    materialized.policy_selection.clone(),
                ));
                Arc::new(PolicyApplicationOwner::new(
                    materialized.policy_selection.clone(),
                    Arc::clone(store),
                    preflight,
                    resolver,
                ))
            })
        }
        _ => None,
    };
    if let (Some(status), Some(owner)) = (host.status.as_ref(), policy_application.as_ref()) {
        let rules: Arc<dyn repl::PermissionRuleApplication> = owner.clone();
        status.seat.enable_policy_controls(rules);
    }
    Ok((generation, policy_application))
}

pub(super) struct ActivatedRun<'store, S: EventSink> {
    pub(super) active: ActiveSession<'store>,
    pub(super) opening: SessionState,
    pub(super) sink: S,
    pub(super) resume: Option<RuntimeResume>,
    pub(super) mcp_errors: Vec<String>,
    pub(super) admitted_tools: Option<Arc<dyn AdmittedToolSource>>,
}

fn activate_session_phase<'store, S: EventSink>(
    admitted: AdmittedRun<'store>,
    destination: impl FnOnce(&ModelCapabilities) -> S,
    generation: &GenerationReady,
    host: &RunHost<'_>,
    materialized: &MaterializedRun<'_>,
    scoped: &ScopedInvocation,
) -> Result<ActivatedRun<'store, S>, RunTermination> {
    let AdmittedRun {
        opened,
        resume,
        mcp_errors,
        admitted_tools,
    } = admitted;
    let opening = opened.state();
    let active = ActiveSession::durable(opened, opening.clone(), EVIDENCE_CAP).map_err(|err| {
        finalize_ci_error(
            scoped.jsonl_store.as_ref(),
            scoped.artifact_store.as_ref(),
            materialized.run_id,
            RunTermination::Failed {
                message: format!("could not open this session's tool evidence: {err}"),
            },
            host.diagnostics,
        )
    })?;
    let sink = destination(&generation.main_capabilities);
    if let Some(status) = host.status.as_ref()
        && status.human
    {
        match generation.selection.snapshot() {
            Ok(selected) => status.seat.configure_status(repl::StatusSetup {
                session: active.id(),
                state: opening.clone(),
                selection: selected,
                mode: materialized.mode,
                cwd: materialized.cwd.clone(),
                trust: materialized.skill_trust,
                segments: repl::effective_statusline_segments(
                    materialized.effective.ui.statusline.as_ref(),
                ),
                width: status.width,
                native: status.native,
                git: Some(Arc::new(repl::status::GitCommand::status(
                    &materialized.cwd,
                ))),
            }),
            Err(error) => host.diagnostics.note(&format!(
                "the native status selection could not be read: {error}"
            )),
        }
    }
    sink.publish(
        &PublicEvent::SessionStarted {
            v: PUBLIC_EVENT_VERSION,
            session: active.id(),
            capabilities: capabilities(
                &materialized.registry,
                materialized.system.is_some(),
                materialized.compiled.is_some(),
            ),
            mcp_errors: mcp_errors.clone(),
            hook_errors: Vec::new(),
            instructions: materialized
                .system
                .as_ref()
                .map(|prompt| prompt.sources.clone())
                .unwrap_or_default(),
        },
        &opening,
    );
    Ok(ActivatedRun {
        active,
        opening,
        sink,
        resume,
        mcp_errors,
        admitted_tools,
    })
}
pub(super) async fn execute<S: EventSink>(
    mut args: Args,
    destination: impl FnOnce(&ModelCapabilities) -> S,
    cancel: CancelToken,
    preflight: Option<ExecutionPreflight>,
    mut host: RunHost<'_>,
) -> Result<Ended, RunTermination> {
    // Phase order: resolve invocation, open effect scope, restore resumptions,
    // materialize inputs, admit context, prepare generation, activate, drive.
    let mut resolved =
        resolve_invocation_phase(&args, &host.driver, host.diagnostics, &cancel, preflight).await?;
    let scoped = open_scope_phase(&mut args, &host, &cancel, &mut resolved).await?;
    let outcome = async {
        let resume = prepare_resume_phase(&args, &host, &scoped, &resolved)?;
        let mut materialized = materialize_run_phase(&args, &host, &scoped, &mut resolved, resume)?;
        let admitted = admit_run_phase(&host, &scoped, &mut resolved, &mut materialized).await?;
        let (generation, policy_application) =
            prepare_generation_phase(&mut host, &cancel, &scoped, &resolved, &materialized).await?;
        let activated = activate_session_phase(
            admitted,
            destination,
            &generation,
            &host,
            &materialized,
            &scoped,
        )?;
        drive_execution(DriverExecution {
            args,
            cancel,
            host,
            scoped: &scoped,
            materialized,
            generation,
            activated,
            policy_application,
        })
        .await
    }
    .await;
    scoped.mcp_scope.close().await;
    outcome
}
