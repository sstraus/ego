//! The `RunExecutor` this binary hands the controller, and the adapters it
//! owns.
//!
//! `ego-core` decides *whether* a turn may run; everything here is what
//! actually happens when it may — the executor, the authority that answers a
//! child's ceiling, the compaction command, and the small projections between
//! a `RunTermination` and the typed answers the controller expects.
//!
//! Split out of `run.rs` because it is the one cluster in that file with a
//! single caller and a single trait to satisfy: the controller calls
//! `RunExecutor`, and nothing else in the CLI needs to see how it is met.

//!
//! The parent's namespace is glob-imported deliberately. This is a file split
//! for navigability, not a decoupling: the executor legitimately reaches back
//! into the composition pipeline it sits in — `settings`, `prepared_materials`,
//! `generation_preflight`, `effect_scope` — because preparing a turn is what
//! it exists to do. Ninety explicit imports would restate the parent module
//! and claim an independence this code does not have.
use super::*;

/// Retries an ACP turn spends on a transient provider failure that showed
/// nothing, before it settles as a known failure.
///
/// maintainer decision 2026-10-05 (regression case): a human watching an AI chat is
/// served better by six visible retries with backoff than by a turn that dies
/// on the first overload. A headless `ego run` keeps `--retry 0` by default,
/// because a script owns its own retry policy and reads exit 4 for it.
pub(super) const ACP_PROVIDER_RETRIES: u32 = crate::cli::INTERACTIVE_PROVIDER_RETRIES;

pub(super) fn public_result_artifact(
    result: Value,
) -> Result<PublishedRunArtifact, RunExecutorError> {
    match result {
        Value::String(text) => Ok(PublishedRunArtifact::output(
            "text/plain",
            text.into_bytes(),
        )),
        structured => serde_json::to_vec(&structured)
            .map(|bytes| PublishedRunArtifact::output("application/json", bytes))
            .map_err(|error| RunExecutorError::new(error.to_string())),
    }
}

pub(super) fn run_executor_error(termination: RunTermination) -> RunExecutorError {
    RunExecutorError::new(match termination {
        RunTermination::UsageError { message }
        | RunTermination::Provider { message }
        | RunTermination::ProviderUncertain { message }
        | RunTermination::ContextOverflow { message }
        | RunTermination::Failed { message } => message,
        other => format!(
            "the prepared run stopped before submission: {}",
            other.exit_reason()
        ),
    })
}

pub(super) fn settlement(termination: RunTermination) -> Result<RunSettlement, RunExecutorError> {
    match termination {
        RunTermination::Done | RunTermination::TaskFailed | RunTermination::MaxTurns => {
            Ok(RunSettlement::completed())
        }
        RunTermination::PermissionRequired { action, by } => {
            Ok(RunSettlement::permission_required(action, by))
        }
        RunTermination::TimedOut => Ok(RunSettlement::cancelled(CancelReason::Timeout)),
        RunTermination::Signalled(Signal::Int) => Ok(RunSettlement::cancelled(CancelReason::User)),
        RunTermination::Signalled(Signal::Term) => Ok(RunSettlement::cancelled(CancelReason::Host)),
        RunTermination::BudgetExceeded => Ok(RunSettlement::completed()),
        RunTermination::ContextOverflow { .. } => Ok(RunSettlement::ContextOverflow),
        // A known failure, not an executor error: the turn ended and its
        // effect is known, so the run settles and the next prompt is admitted.
        // A post-submission UsageError also records a known refusal (such as
        // HTTP 400), not transport ambiguity. ProviderUncertain still falls
        // to the catch-all and leaves the run uncertain.
        RunTermination::UsageError { message } | RunTermination::Provider { message } => {
            Ok(RunSettlement::Interrupted {
                diagnostic: Some(message),
            })
        }
        // Not the catch-all below: an executor error says the run could not
        // be carried out, and a paused run was carried out exactly as far as
        // it was asked to go. It is a settlement, and the one the host that
        // asked for it is waiting to read.
        RunTermination::Paused => Ok(RunSettlement::Paused),
        RunTermination::QuestionRequired { question } => Err(RunExecutorError::new(format!(
            "question {} still requires an answer",
            question.as_ref().id
        ))),
        termination => Err(run_executor_error(termination)),
    }
}

#[derive(Default)]
struct CliRunAuthority {
    state: Mutex<CliRunAuthorityState>,
}

#[derive(Default)]
struct CliRunAuthorityState {
    active: Option<CancelToken>,
    pending_interrupt: bool,
    /// The hold this run is under, or the one it will start under.
    ///
    /// Two fields for one fact, exactly as cancellation has two: `holding`
    /// is the token a live turn is watching, and `pending_pause` is a
    /// request that arrived before there was one. Without the second, a
    /// pause racing the start of a run would be dropped and the host would
    /// be told a hold was in place that nothing was honouring.
    holding: Option<PauseToken>,
    pending_pause: Option<RequestId>,
    settlement: Option<RunSettlement>,
    observers: Vec<Waker>,
}

impl CliRunAuthority {
    fn begin(&self, cancel: CancelToken) -> (CancelToken, PauseToken) {
        let mut state = self.state.lock().expect("CLI run authority");
        if state.pending_interrupt {
            state.pending_interrupt = false;
            cancel.cancel(CancelCause::User);
        }
        let pause = PauseToken::new();
        if let Some(request) = state.pending_pause.take() {
            pause.pause(request);
        }
        state.active = Some(cancel.clone());
        state.holding = Some(pause.clone());
        state.settlement = None;
        (cancel, pause)
    }

    fn interrupt(&self) {
        let mut state = self.state.lock().expect("CLI run authority");
        match state.active.as_ref() {
            Some(cancel) => cancel.cancel(CancelCause::User),
            None => state.pending_interrupt = true,
        }
    }

    fn hold(&self, request: RequestId) {
        let mut state = self.state.lock().expect("CLI run authority");
        match state.holding.as_ref() {
            Some(pause) => pause.pause(request),
            None => {
                // First writer wins here too, and for the reason
                // `PauseToken::pause` gives: the boundary that eventually
                // happens belongs to whoever asked for it first.
                state.pending_pause.get_or_insert(request);
            }
        }
    }

    fn complete(&self, settlement: RunSettlement) {
        let observers = {
            let mut state = self.state.lock().expect("CLI run authority");
            state.active = None;
            state.holding = None;
            state.settlement = Some(settlement);
            std::mem::take(&mut state.observers)
        };
        for observer in observers {
            observer.wake();
        }
    }

    async fn observe(&self) -> RunSettlement {
        futures::future::poll_fn(|context| {
            let mut state = self.state.lock().expect("CLI run authority");
            match state.settlement.clone() {
                Some(settlement) => std::task::Poll::Ready(settlement),
                None => {
                    if !state
                        .observers
                        .iter()
                        .any(|observer| observer.will_wake(context.waker()))
                    {
                        state.observers.push(context.waker().clone());
                    }
                    std::task::Poll::Pending
                }
            }
        })
        .await
    }
}

/// Production executor adapter for controller-owned headless turns.
#[derive(Clone)]
pub struct CliRunExecutor {
    pub(super) args: Args,
    pub(super) store: Arc<JsonlStore>,
    pub(super) concurrency: Option<Arc<Semaphore>>,
    pub(super) frozen: Option<Arc<config::FrozenRunConfig>>,
    pub(super) artifacts: Option<Arc<dyn RunArtifactStore>>,
    pub(super) admission: Arc<EvidenceAdmissionService>,
    pub(super) child_authority: Option<Arc<ChildAuthorityCeiling>>,
    pub(super) child: bool,
}

#[derive(Clone)]
pub(super) struct ChildAuthorityCeiling {
    mode: Mode,
    sandbox: Confinement,
    offline: bool,
    permissions: Ruleset,
    ceilings: Ceilings,
}

impl ChildAuthorityCeiling {
    pub(super) fn from_parent(effective: &Effective, confinement: &RunConfinement) -> Self {
        Self {
            mode: effective.mode.unwrap_or(Mode::Default),
            sandbox: effective.sandbox.unwrap_or_default(),
            offline: confinement.offline(),
            permissions: effective.permissions.clone(),
            ceilings: effective.ceilings.clone(),
        }
    }

    pub(super) fn admit(&self, effective: &Effective, offline: bool) -> Result<(), String> {
        let mode = effective.mode.unwrap_or(Mode::Default);
        if !mode_is_no_wider(mode, self.mode) {
            return Err(format!(
                "child profile mode {mode} widens parent mode {}",
                self.mode
            ));
        }
        let sandbox = effective.sandbox.unwrap_or_default();
        if !sandbox.within(self.sandbox) {
            return Err(format!(
                "child profile sandbox {sandbox} widens parent sandbox {}",
                self.sandbox
            ));
        }
        if self.offline && !offline {
            return Err("child profile cannot clear the parent offline ceiling".to_owned());
        }
        let child = &effective.permissions;
        let allows_no_more = child.allow.iter().all(|candidate| {
            self.permissions
                .allow
                .iter()
                .any(|parent| parent.rule() == candidate.rule())
        });
        let denies_no_less = self.permissions.deny.iter().all(|parent| {
            child
                .deny
                .iter()
                .any(|candidate| candidate.rule() == parent.rule())
        });
        if !allows_no_more || !denies_no_less {
            return Err(
                "child profile permissions are not a syntactic narrowing of the parent authority"
                    .to_owned(),
            );
        }
        let mut bounded = effective.ceilings.clone();
        bounded.clamp(&self.ceilings);
        if bounded != effective.ceilings {
            return Err("child profile budget widens the parent ceiling".to_owned());
        }
        Ok(())
    }
}

fn mode_is_no_wider(child: Mode, parent: Mode) -> bool {
    child == Mode::Plan
        || child == parent
        || matches!(parent, Mode::Edits | Mode::Yolo) && !matches!(child, Mode::Yolo)
}

#[derive(Clone)]
pub(crate) struct CliMcpRunPolicy {
    pub(crate) root: ego_core::run::RunRoot,
    pub(crate) model: ModelRef,
    pub(crate) thinking: ThinkingLevel,
    pub(crate) mode: Mode,
    pub(crate) ceilings: Ceilings,
    pub(crate) profile: Option<String>,
}

/// The configuration a protocol server resolves for its workspace.
///
/// Exactly the capture `for_mcp` performs, hoisted so `ego acp` seeds its
/// operator seat from the same resolution its runs will use. Resolving it
/// twice from two code paths is how a picker starts disagreeing with the run
/// it is meant to be steering.
pub(crate) fn protocol_effective(
    root: &Path,
    session_dir: Option<PathBuf>,
    shape: SessionShape,
) -> Result<ego_core::config::Effective, UsageError> {
    let args = protocol_args(session_dir, shape);
    let frozen = config::FrozenRunConfig::capture(root, &args, Trust::Untrusted)?;
    frozen.resolve(&args)
}

/// The headless `Args` every served protocol runs under.
///
/// Everything a protocol cannot have is fixed here; `shape` is the part the
/// launch line owns, so a host that named nothing gets exactly the session it
/// got before these flags existed ([`SessionShape::default`]).
pub(crate) fn protocol_args(session_dir: Option<PathBuf>, shape: SessionShape) -> Args {
    Args {
        read_only_dir: shape.read_only_dir,
        prompt: None,
        agent_file: None,
        attach: Vec::new(),
        stdin: crate::cli::Stdin::Ambient,
        model: None,
        offline: false,
        sandbox: None,
        thinking: None,
        system: shape.system,
        session_prompt: None,
        append_system: shape.append_system,
        instructions: shape.instructions,
        bare: shape.bare,
        trust_workspace: false,
        mode: None,
        allow: shape.allow,
        deny: shape.deny,
        answers: Vec::new(),
        session: SessionChoice::New,
        session_dir,
        artifact_dir: None,
        output: Output::Json,
        output_explicit: false,
        schema: None,
        schema_retries: 0,
        retry: 0,
        provider_recovery: false,
        fallbacks: Vec::new(),
        budget: None,
        max_output_tokens: None,
        goal: None,
        goal_turns: 20,
        no_context_tool: false,
        context_clear: false,
        context_interval: 10,
        goal_check_command: None,
        max_turns: None,
        timeout: None,
        tools: shape.tools,
        mcp: Vec::new(),
        directory: None,
        set: Vec::new(),
        profile: shape.profile,
        profile_source: shape.profile_source,
        ceiling_profile: None,
        quiet: false,
        no_color: false,
        plain: false,
        vim: false,
    }
}

/// Compose the one complete pre-effect evidence value used by every CLI host.
pub(super) fn complete_run_context(
    args: &Args,
    effective: &Effective,
    confinement: &RunConfinement,
    cwd: &Path,
    invocation: RunInvocationId,
    run_id: RunId,
    instructions: Vec<InstructionDescriptor>,
) -> Result<RunContextEvidence, UsageError> {
    let session = run_id.session_id();
    let mut auth = effective
        .auth
        .iter()
        .map(|(provider, layer)| (provider.clone(), format!("{layer:?}")))
        .collect::<Vec<_>>();
    auth.sort();
    let mcp_config = effective
        .mcp
        .entries()
        .iter()
        .map(|(name, entry)| (name.as_str().to_owned(), format!("{entry:?}")))
        .collect::<Vec<_>>();
    let config_facts = vec![
        format!("model={:?}", effective.model),
        format!("thinking={:?}", effective.thinking),
        format!("mode={:?}", effective.mode),
        format!("sandbox={:?}", effective.sandbox),
        format!("permissions={:?}", effective.permissions),
        format!("judge={:?}", effective.judge),
        format!("posture={:?}", effective.posture),
        format!("profile={:?}", effective.active_profile),
        format!("session_dir={:?}", effective.session_dir),
        format!("artifact_dir={:?}", effective.artifact_dir),
        format!("ceilings={:?}", effective.ceilings),
        format!("clamped={:?}", effective.clamped),
        format!("ui={:?}", effective.ui),
    ];
    let config_digest = canonical_evidence_digest(&(config_facts, auth, mcp_config))?;
    let mcp = attach_mcp(args, effective, confinement, cwd)?
        .attached
        .into_iter()
        .map(|(name, server, provenance)| {
            let transport = match &server {
                McpServerConfig::Stdio(_) => McpTransportKind::Stdio,
                McpServerConfig::Http(_) => McpTransportKind::Http,
                McpServerConfig::Acp { .. } => McpTransportKind::Acp,
            };
            let provenance_digest = canonical_evidence_digest(&(
                name.as_str(),
                format!("{server:?}"),
                format!("{provenance:?}"),
            ))?;
            McpContextDescriptor::new(name.as_str(), transport, Vec::new(), provenance_digest)
                .map_err(|error| UsageError(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let policy = ego_core::digest::sha256_hex(format!("{:?}", effective.permissions).as_bytes());
    let policy = PolicyDigest::try_from(policy).expect("sha256_hex is a valid policy digest");
    let workspace_digest = canonical_evidence_digest(
        &confinement
            .workspace()
            .as_path()
            .to_string_lossy()
            .into_owned(),
    )?;
    let capabilities_digest = canonical_evidence_digest(&confinement.requirements().required())?;
    let execution = ExecutionAdmissionDescriptor::new(
        confinement.level(),
        !confinement.offline(),
        workspace_digest,
        capabilities_digest,
    );
    Ok(RunContextEvidence::complete(
        session,
        invocation,
        run_id,
        EgoVersion::current(),
        ConfigProvenance::resolved(effective.active_profile.clone()),
        config_digest,
        policy,
        effective.ceilings.clone(),
        instructions,
        mcp,
        execution,
    ))
}

fn canonical_evidence_digest<T: serde::Serialize>(value: &T) -> Result<EvidenceDigest, UsageError> {
    EvidenceAdmissionService::new()
        .canonical_digest(value)
        .map_err(|error| UsageError(error.to_string()))
}

/// Production composition for every host-facing compaction command.
pub struct CliCompactionCommand {
    args: Args,
    store: Arc<JsonlStore>,
    admission: Arc<EvidenceAdmissionService>,
}

impl CliCompactionCommand {
    pub fn new(
        args: Args,
        store: Arc<JsonlStore>,
        admission: Arc<EvidenceAdmissionService>,
    ) -> Self {
        Self {
            args,
            store,
            admission,
        }
    }

    pub fn for_protocol(
        root: PathBuf,
        session_dir: Option<PathBuf>,
        store: Arc<JsonlStore>,
        admission: Arc<EvidenceAdmissionService>,
    ) -> Self {
        let mut args = protocol_args(session_dir, SessionShape::default());
        args.directory = Some(root);
        Self::new(args, store, admission)
    }

    async fn prepare_source(
        &self,
        source_id: SessionId,
    ) -> Result<PreparedCompaction, CompactionError> {
        // Cold compaction has no live selection. Recover the last admitted
        // source model rather than silently using the user's current default.
        use ego_core::evidence::EvidenceLogSource;
        let records = self
            .store
            .evidence_records(&source_id)
            .map_err(|error| CompactionError::Internal(error.to_string()))?;
        let source_admission = latest_main_admission(&records);
        let source_catalogue = compaction_source_catalogue(&records, source_admission)?;
        let args = compaction_source_args(self.args.clone(), source_admission);
        let settings = settings(&args, |_| Trust::Untrusted, &Stderr)
            .map_err(|error| CompactionError::Internal(error.to_string()))?;
        let Materials {
            system,
            registry,
            skills: _,
            compiled: _,
            text: _,
            slug,
        } = prepared_materials(
            &settings,
            &self.args,
            String::new(),
            Arc::new(self.store.history()),
            &Stderr,
        )
        .map_err(|error| CompactionError::Internal(error.to_string()))?;
        let Settings {
            run_deadline,
            root,
            cwd,
            confinement,
            effective,
            ..
        } = settings;
        let clock = SystemClock;
        let opened = Opened::Durable(Box::new(
            session::open(self.store.as_ref(), &source_id, &clock)
                .map_err(|error| CompactionError::Internal(error.to_string()))?,
        ));
        let invocation = RunInvocationId::new();
        let context = complete_run_context(
            &self.args,
            &effective,
            &confinement,
            &cwd,
            invocation,
            RunId::new(source_id, invocation.uuid()),
            system
                .as_ref()
                .map(|prompt| prompt.sources.clone())
                .unwrap_or_default(),
        )
        .map_err(|error| CompactionError::Internal(error.to_string()))?;
        let Opened::Durable(opened_session) = &opened;
        self.admission
            .admit_run_context(&opened_session.writer, context)
            .map_err(|error| CompactionError::Internal(error.to_string()))?;

        let cancel = CancelToken::new();
        let mut no_status = None;
        let GenerationPreflight {
            source,
            selection: _,
            capabilities: _,
            judge_available,
            proof_directory,
            proof_evidence,
        } = generation_preflight(
            GenerationPreflightInput {
                session_id: source_id,
                confinement: &confinement,
                effective: &effective,
                slug,
                cancel: &cancel,
                run_deadline,
                mcp_registry: None,
                native_tools: &registry,
            },
            &mut no_status,
        )
        .await
        .map_err(compaction_termination)?;
        let opening = opened.state();
        drop(proof_directory);
        drop(proof_evidence);
        let mut active = ActiveSession::durable(opened, opening.clone(), EVIDENCE_CAP)
            .map_err(CompactionError::Internal)?;
        let mode = effective.mode.unwrap_or(Mode::Default);
        let policy_selection = GenerationPolicySelection::new(mode, effective.permissions.clone());
        let policy_source: Arc<dyn GenerationPolicySource> = Arc::new(
            CliGenerationPolicySource::new(policy_selection, CanonPath::new(&root), None),
        );
        let currency = CurrencyCode::new(SETTLEMENT).expect("a constant this file controls");
        let gate = SharedBudgetGate::new(budget::gate(
            &effective.ceilings,
            effective.clamped.money,
            currency,
        ));
        let config = TurnConfig {
            cache: effective.cache,
            system: system.clone(),
            identity: None,
            token_ceiling: effective.ceilings.tokens,
            retry: 0,
            fallbacks: Vec::new(),
        };
        let (ports, observation, probe_evidence) =
            effect_scope(&confinement, active.evidence(), &cancel, run_deadline, None)
                .await
                .map_err(compaction_termination)?;
        let fs = OsFs;
        let sink = NullSink;
        let session = Session {
            id: active.id(),
            state: opening,
            store: active.store(),
            evidence: active.run_evidence(),
            clock: &clock,
            sink: &sink,
            cancel: cancel.clone(),
        };
        let http =
            Arc::new(ProviderHttpAdapter::new(Arc::clone(&ports))) as Arc<dyn ego_ai::http::Http>;
        let runtime = GenerationRuntime {
            source: source.bind(http, cancel.clone(), run_deadline),
            policy_source,
            gate,
            config,
            admission: Arc::clone(&self.admission),
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
        .with_secret_values(ego_tools::secrets::from_env());
        let tools = Tools::from_registry(&registry, effect_ports, CanonPath::new(&cwd));
        let mut loop_ = TurnLoop::new(session, runtime, tools)
            .map_err(|error| CompactionError::Internal(error.to_string()))?;
        if let Some(tools) = source_catalogue {
            loop_ = loop_.with_compaction_source_tools(tools);
        }
        if let Some(replayed) = active.replayed() {
            loop_ = loop_.with_replayed_context(replayed);
        }
        if judge_available {
            loop_ = loop_.with_judge(JudgeContext {
                confinement: confinement.clone(),
                exec_caps: observation.capabilities(),
            });
        }
        loop_ = super::composition::with_permission_analysis(loop_, Trust::Untrusted)
            .with_execution_observation(observation, probe_evidence);
        let target_tools = registry.iter().map(RegisteredTool::descriptor).collect();
        let prepared = loop_.prepare_compaction(system, target_tools).await?;
        let state = loop_
            .into_state()
            .map_err(|error| CompactionError::Internal(error.to_string()))?;
        active.replace_state(state);
        Ok(prepared)
    }
}

fn latest_main_admission(
    records: &[EvidenceRecord],
) -> Option<&ego_core::evidence::GenerationAdmissionEvidence> {
    records.iter().rev().find_map(|record| match record {
        EvidenceRecord::GenerationAdmission(admission)
            if admission.role() == GenerationRole::Main =>
        {
            Some(admission)
        }
        _ => None,
    })
}

fn compaction_source_catalogue(
    records: &[EvidenceRecord],
    source_admission: Option<&ego_core::evidence::GenerationAdmissionEvidence>,
) -> Result<Option<Vec<ToolDescriptor>>, CompactionError> {
    let Some(digest) = source_admission.and_then(|admission| admission.tool_catalogue()) else {
        eprintln!(
            "Warning: source has no durable tool catalogue; cold compaction uses the current catalogue (best effort, cache prefix may differ)."
        );
        return Ok(None);
    };
    records
        .iter()
        .find_map(|record| match record {
            EvidenceRecord::ToolCatalogue(catalogue) if &catalogue.digest == digest => {
                Some(catalogue.tools.clone())
            }
            _ => None,
        })
        .map(Some)
        .ok_or_else(|| {
            CompactionError::Internal("source tool catalogue evidence is missing".to_owned())
        })
}

fn compaction_source_args(
    mut args: Args,
    source_admission: Option<&ego_core::evidence::GenerationAdmissionEvidence>,
) -> Args {
    if let Some(admission) = source_admission {
        args.model = Some(admission.target().canonical_model().0.clone());
    }
    if let Some(thinking) = source_admission.and_then(|admission| admission.thinking()) {
        args.thinking = Some(thinking);
    } else {
        eprintln!(
            "Warning: source has no durable thinking effort; cold compaction uses current thinking (best effort)."
        );
    }
    args
}

struct CliCompactionPreparation<'a> {
    command: &'a CliCompactionCommand,
    source: SessionId,
}

pub(super) struct DeferredCompactionPreparation<'a> {
    pub(super) source: SessionId,
    pub(super) future: Option<CompactionFuture<'a, Result<PreparedCompaction, CompactionError>>>,
}

impl CompactionPreparation for DeferredCompactionPreparation<'_> {
    fn source(&self) -> SessionId {
        self.source
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> CompactionFuture<'a, Result<PreparedCompaction, CompactionError>> {
        self.future.take().unwrap_or_else(|| {
            Box::pin(std::future::ready(Err(CompactionError::Internal(
                "compaction preparation was requested more than once".to_owned(),
            ))))
        })
    }
}

impl CompactionPreparation for CliCompactionPreparation<'_> {
    fn source(&self) -> SessionId {
        self.source
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> CompactionFuture<'a, Result<PreparedCompaction, CompactionError>> {
        Box::pin(async move { self.command.prepare_source(self.source).await })
    }
}

impl CompactionCommand for CliCompactionCommand {
    fn compact<'a>(
        &'a self,
        request: CompactionTransitionRequest,
    ) -> CompactionFuture<'a, Result<CompactionReceipt, CompactionTransitionError>> {
        Box::pin(async move {
            let mut preparation = CliCompactionPreparation {
                command: self,
                source: request.source,
            };
            CompactionTransitionService::new(SystemClock)
                .compact(request, &mut preparation, self.store.as_ref())
                .await
        })
    }
}

pub(super) fn compaction_termination(termination: RunTermination) -> CompactionError {
    CompactionError::Internal(run_executor_error(termination).to_string())
}

impl CliRunExecutor {
    pub(super) fn for_children(&self) -> Self {
        let mut executor = self.clone();
        let artifacts: Arc<dyn RunArtifactStore> = self.store.clone();
        executor.artifacts = Some(artifacts);
        executor.child = true;
        executor
    }

    pub(super) fn with_child_authority(
        &self,
        effective: &Effective,
        confinement: &RunConfinement,
    ) -> Self {
        let mut executor = self.for_children();
        executor.child_authority = Some(Arc::new(ChildAuthorityCeiling::from_parent(
            effective,
            confinement,
        )));
        executor
    }

    pub(super) fn admit_child_authority(
        &self,
        effective: &Effective,
        offline: bool,
    ) -> Result<(), String> {
        match self.child_authority.as_deref() {
            Some(authority) => authority.admit(effective, offline),
            None => Ok(()),
        }
    }

    /// Build the fixed headless inputs for the ACP host once, before admission.
    pub fn for_acp(
        session_dir: Option<PathBuf>,
        shape: SessionShape,
        store: Arc<JsonlStore>,
        admission: Arc<EvidenceAdmissionService>,
    ) -> Self {
        let mut args = protocol_args(session_dir, shape);
        args.retry = ACP_PROVIDER_RETRIES;
        args.provider_recovery = true;
        Self {
            args,
            store,
            concurrency: None,
            frozen: None,
            artifacts: None,
            admission,
            child_authority: None,
            child: false,
        }
    }

    /// Compose the fixed MCP deployment policy.
    ///
    /// An explicit thinking ceiling is already complete policy metadata. Only
    /// an absent ceiling needs catalogue facts to resolve the model default;
    /// actual generation still resolves and validates current model facts at
    /// its ordinary preflight boundary.
    pub(crate) fn for_mcp(
        root: ego_core::run::RunRoot,
        session_dir: Option<PathBuf>,
        store: Arc<JsonlStore>,
        artifacts: Arc<dyn RunArtifactStore>,
        max_concurrent_runs: NonZeroUsize,
        admission: Arc<EvidenceAdmissionService>,
        catalogue: Option<&ego_ai::catalogue::ModelCatalogueSnapshot>,
    ) -> Result<(Self, CliMcpRunPolicy), UsageError> {
        let args = protocol_args(session_dir, SessionShape::default());
        let frozen = Arc::new(config::FrozenRunConfig::capture(
            root.as_path(),
            &args,
            Trust::Untrusted,
        )?);
        let effective = frozen.resolve(&args)?;
        let model = effective.model.clone().ok_or_else(|| {
            UsageError("mcp-server requires an effective configured model".to_owned())
        })?;
        let thinking = match effective.thinking {
            Some(thinking) => thinking,
            None => {
                let model_facts = catalogue
                    .and_then(|catalogue| {
                        catalogue
                            .models
                            .iter()
                            .find(|entry| entry.slug.to_string() == model.0)
                    })
                    .ok_or_else(|| {
                        UsageError(format!(
                            "mcp-server has no current capability facts for configured model \
                             `{model}` and no explicit thinking ceiling"
                        ))
                    })?;
                model_facts
                    .capabilities
                    .resolve_reasoning_effort(None)
                    .map_err(|error| UsageError(format!("configured model `{model}`: {error}")))?
            }
        };
        if effective.ceilings.money.is_none() && effective.ceilings.tokens.is_none() {
            return Err(UsageError(
                "mcp-server requires at least one effective budget ceiling".to_owned(),
            ));
        }
        let policy = CliMcpRunPolicy {
            root,
            model,
            thinking,
            mode: effective.mode.unwrap_or(Mode::Default),
            ceilings: effective.ceilings,
            profile: frozen.profile().map(str::to_owned),
        };
        Ok((
            Self {
                args,
                store,
                concurrency: Some(Arc::new(Semaphore::new(max_concurrent_runs.get()))),
                frozen: Some(frozen),
                artifacts: Some(artifacts),
                admission,
                child_authority: None,
                child: false,
            },
            policy,
        ))
    }

    fn authority(handle: &RunHandle) -> Result<Arc<CliRunAuthority>, RunExecutorError> {
        handle
            .authority::<Arc<CliRunAuthority>>()
            .cloned()
            .ok_or_else(|| RunExecutorError::new("run handle carries another executor authority"))
    }
}

impl RunExecutor for CliRunExecutor {
    type Prepared = CliPrepared;

    fn execution_observation<'a>(
        &'a self,
        prepared: &'a mut Self::Prepared,
        cancel: CancelToken,
    ) -> futures::future::BoxFuture<
        'a,
        Result<Option<ego_core::run::RunExecutionObservation>, RunExecutorError>,
    > {
        Box::pin(async move {
            if prepared.admitted_sandbox_observation.is_none() {
                return Ok(None);
            }
            if cancel.is_cancelled() {
                return Err(RunExecutorError::new("execution observation cancelled"));
            }
            let evidence = prepared
                .store
                .probe_evidence_store(prepared.session_id, EVIDENCE_CAP)
                .map_err(|error| RunExecutorError::new(error.to_string()))?;
            let config = ego_tools::exec::ExecRunConfig::new(
                prepared.confinement.clone(),
                evidence,
                cancel.clone(),
                ego_tools::exec::ExecLimits::default(),
                ego_tools::exec::HttpCallTimeout::default(),
                prepared.run_deadline,
                Arc::new(ego_tools::http::SystemResolver::new()),
            );
            let (observed, probe_evidence) = RunEffectPorts::observe(&config)
                .await
                .map_err(|error| RunExecutorError::new(error.to_string()))?;
            if cancel.is_cancelled() {
                return Err(RunExecutorError::new("execution observation cancelled"));
            }
            prepared.admitted_sandbox_observation = Some(observed.clone());
            Ok(Some((observed, probe_evidence)))
        })
    }

    fn prepare<'a>(
        &'a self,
        request: &'a RunPreparation,
    ) -> futures::future::BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        Box::pin(async move {
            let permit = match &self.concurrency {
                Some(concurrency) => {
                    Some(Arc::clone(concurrency).try_acquire_owned().map_err(|_| {
                        RunExecutorError::new("run concurrency ceiling is unavailable")
                    })?)
                }
                None => None,
            };
            CliPrepared::prepare(
                self.args.clone(),
                Arc::clone(&self.store),
                self.artifacts.clone(),
                request,
                permit,
                self.frozen.as_deref(),
                PreparedExecutionRuntime {
                    admission: Arc::clone(&self.admission),
                    child_executor: Arc::new(self.for_children()),
                    child_authority: self.child_authority.clone(),
                    child: self.child,
                },
            )
            .await
            .map_err(run_executor_error)
        })
    }

    fn admit_run_context<'a>(
        &'a self,
        prepared: &'a Self::Prepared,
    ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        Box::pin(async move {
            let key = EvidenceRecord::RunContext(prepared.run_context.clone()).key();
            let clock = SystemClock;
            let opened = session::open(self.store.as_ref(), &prepared.session_id, &clock).map_err(
                |error| EvidenceError::Store {
                    key,
                    diagnostic: error.to_string(),
                },
            )?;
            self.admission
                .admit_run_context(&opened.writer, prepared.run_context.clone())
        })
    }

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> futures::future::BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        Box::pin(async move {
            Ok(RunHandle::new(
                request.run_id,
                Arc::new(CliRunAuthority::default()),
            ))
        })
    }

    fn recover<'a>(
        &'a self,
        run: &'a ego_core::run::RunId,
    ) -> futures::future::BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
        Box::pin(async move {
            let clock = SystemClock;
            let opened = session::open(self.store.as_ref(), &run.session_id(), &clock)
                .map_err(|error| RunExecutorError::new(error.to_string()))?;
            let state = &opened.state;
            // Generation streams belong to the dead process. Unlike an admitted
            // tool effect, they cannot continue locally and must never be replayed.
            let unresolved_effect = state
                .items
                .values()
                .any(|item| item.tool_call_started.is_some() && !item.completed);
            Ok(if !state.live_attempts.is_empty() && !unresolved_effect {
                RecoveryEvidence::Interrupted
            } else {
                RecoveryEvidence::NotRecoverable
            })
        })
    }

    fn observe<'a>(
        &'a self,
        handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> futures::future::BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(async move { Ok(Self::authority(handle)?.observe().await) })
    }

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        prompt: &'a RunPrompt,
        prepared: Self::Prepared,
        bindings: RunExecutionBindings,
    ) -> futures::future::BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(async move {
            let authority = Self::authority(handle)?;
            let (cancel, pause) = authority.begin(bindings.cancel());
            let result = prepared.submit(prompt, bindings, &cancel, &pause).await;
            match result.as_ref() {
                Ok(settlement) => authority.complete(settlement.clone()),
                // The same sentence the controller would have settled this
                // turn with, so the authority's record of how it ended and the
                // durable receipt do not disagree about why.
                Err(error) => authority.complete(RunSettlement::Uncertain {
                    diagnostic: Some(error.to_string()),
                }),
            }
            result
        })
    }

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
    ) -> futures::future::BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(async move {
            Self::authority(handle)?.interrupt();
            Ok(())
        })
    }

    fn pause<'a>(
        &'a self,
        handle: &'a RunHandle,
        request: RequestId,
    ) -> futures::future::BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(async move {
            Self::authority(handle)?.hold(request);
            Ok(())
        })
    }
}

#[cfg(test)]
mod regression_case_tests;

#[cfg(test)]
mod regression_case_tests {
    use super::*;

    // Catches: widening known-failure settlement to include a lost exchange.
    #[test]
    fn ambiguous_provider_failure_remains_an_executor_error() {
        let error = settlement(RunTermination::ProviderUncertain {
            message: "response lost after output".to_owned(),
        })
        .expect_err("an ambiguous exchange must not settle as known");
        assert_eq!(error.to_string(), "response lost after output");
    }

    // Catches: regression of the existing known transient-failure path.
    #[test]
    fn known_provider_failure_keeps_its_failed_settlement() {
        assert_eq!(
            settlement(RunTermination::Provider {
                message: "the provider returned HTTP 503".to_owned(),
            })
            .expect("known provider failure"),
            RunSettlement::Interrupted {
                diagnostic: Some("the provider returned HTTP 503".to_owned()),
            }
        );
    }

    // Catches: an internal defect being classified as a known provider refusal.
    #[test]
    fn internal_failure_is_not_reclassified_as_known() {
        assert!(
            settlement(RunTermination::Failed {
                message: "journal write failed".to_owned(),
            })
            .is_err()
        );
    }

    // Catches: a known post-submission usage failure reaches the executor-error
    // path and is persisted as Uncertain instead of a failed turn.
    #[test]
    fn known_usage_failure_does_not_become_uncertain() {
        let message = "the provider returned HTTP 400".to_owned();
        assert_eq!(
            settlement(RunTermination::UsageError {
                message: message.clone(),
            })
            .expect("a known usage failure settles"),
            RunSettlement::Interrupted {
                diagnostic: Some(message),
            }
        );
    }
}

#[cfg(test)]
mod sandbox_ceiling_tests {
    use super::*;
    // Catches: a strict child is rejected under Adaptive, or Adaptive escapes a strict parent.
    #[test]
    fn adaptive_parent_admits_strict_children_but_strict_parent_rejects_adaptive() {
        let mut parent =
            ego_core::config::resolve(&[], ego_core::config::Trust::Trusted, None).unwrap();
        parent.sandbox = Some(Confinement::Adaptive);
        let policy =
            RunConfinement::new(Confinement::Adaptive, false, CanonPath::new("/workspace"))
                .unwrap();
        let ceiling = ChildAuthorityCeiling::from_parent(&parent, &policy);
        for level in [Confinement::Ro, Confinement::Workspace] {
            let mut child = parent.clone();
            child.sandbox = Some(level);
            ceiling.admit(&child, false).unwrap();
            let strict = ChildAuthorityCeiling::from_parent(
                &child,
                &RunConfinement::new(level, false, CanonPath::new("/workspace")).unwrap(),
            );
            assert!(strict.admit(&parent, false).is_err());
        }
    }
}

#[cfg(test)]
mod readmission_tests;
