//! Adapt run policy to external effects, hooks, children, and skills.

use super::*;

/// The wall-clock wait between two attempts of one generation plan.
pub(super) struct TokioRetryPause;

impl ego_core::turn::RetryPause for TokioRetryPause {
    fn jitter_basis_points(&self) -> u16 {
        let random = uuid::Uuid::new_v4();
        u16::from_be_bytes([random.as_bytes()[0], random.as_bytes()[1]]) % 2_501
    }

    fn pause(&self, delay: std::time::Duration) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(delay))
    }
}

pub(super) async fn effect_scope(
    confinement: &RunConfinement,
    evidence: &SessionEvidenceStore,
    cancel: &CancelToken,
    run_deadline: Option<std::time::Instant>,
) -> Result<
    (
        Arc<RunEffectPorts>,
        ExecObservation,
        Option<ego_core::store::evidence::EvidenceRef>,
    ),
    RunTermination,
> {
    run_effect_ports(
        confinement.clone(),
        evidence.clone(),
        cancel.clone(),
        run_deadline,
        None,
        None,
    )
    .await
    .map_err(|error| process_error_termination(error, confinement, cancel))
}

pub(super) fn process_error_termination(
    error: ProcessError,
    confinement: &RunConfinement,
    cancel: &CancelToken,
) -> RunTermination {
    match &error {
        ProcessError::MissingCapabilities(missing) => RunTermination::UsageError {
            message: unmet_confinement(confinement, missing),
        },
        ProcessError::Backend(_) => RunTermination::UsageError {
            message: error.to_string(),
        },
        ProcessError::Cancelled => cancelled_termination(cancel),
        _ => RunTermination::Failed {
            message: error.to_string(),
        },
    }
}

/// The confinement backend for this build, named the way an operator would
/// look it up.
pub(super) const fn confinement_backend() -> &'static str {
    if cfg!(target_os = "macos") {
        "the macOS Seatbelt backend (/usr/bin/sandbox-exec)"
    } else if cfg!(target_os = "linux") {
        "the Linux Landlock backend"
    } else {
        "no confinement backend, because this platform has none"
    }
}

/// Explain a refusal the way the person who typed the flag needs it.
///
/// `MissingExecCaps` states the measurement; it does not know which flag asked
/// for the guarantee or what would relax it. The CLI owns the sandbox flags,
/// so it owns the sentence: which mode asked, which guarantee is absent in
/// words, which backend failed to demonstrate it, and the two ways out - ask
/// for less, or fix what the recorded measurement blames.
pub(super) fn unmet_confinement(confinement: &RunConfinement, missing: &MissingExecCaps) -> String {
    let absent = missing.missing();
    let mut remedies = Vec::new();
    if [
        ExecCaps::READ_SCOPED,
        ExecCaps::WRITE_DENIED,
        ExecCaps::WRITE_SCOPED,
    ]
    .into_iter()
    .any(|guarantee| absent.contains(guarantee))
    {
        remedies.push(match confinement.level() {
            Confinement::Ro => "`--sandbox off`",
            _ => "`--sandbox ro` or `--sandbox off`",
        });
    }
    if absent.contains(ExecCaps::NO_IP_NETWORK) {
        remedies.push("dropping `--offline`");
    }
    let transcript = match missing.probe_evidence() {
        Some(reference) => format!(
            " The measurement that decided it is stored as run evidence {}.",
            reference.id
        ),
        None => String::new(),
    };
    // Name the measured host cause without treating Adaptive filesystem setup
    // failures as offline listener failures.
    let host_control = match missing.host_control_failure() {
        Some(failure) if failure.starts_with("adaptive.") => format!(
            " The Adaptive probe failed during setup or its unconfined host control ({failure}), before confinement could be demonstrated. Fix the recorded probe cause."
        ),
        Some(failure) if absent.contains(ExecCaps::NO_IP_NETWORK) => format!(
            " The offline probe failed in its unconfined host control ({failure}), before the sandbox was tested on that path, so no `--sandbox` level can change this. Drop `--offline`, or allow the probe's local listener (check the host firewall); the host cause is not proven."
        ),
        _ => String::new(),
    };
    format!(
        "--sandbox {} could not be demonstrated on this host: {} showed {}, but this run needs {}.{transcript}{host_control} Ask for less with {}, or fix the cause the measurement names.",
        confinement.level(),
        confinement_backend(),
        missing.available(),
        absent,
        remedies.join(", or "),
    )
}

/// The pictures one turn was given, by the two routes they can arrive on.
///
/// One value rather than two arguments because they are one question — what
/// this turn is looking at — answered by two transports: bytes that came inside
/// the request, and paths this process has yet to read. They are published
/// together, under one model's limits, or not at all.
pub(super) struct Attachments<'a> {
    /// Images that arrived with the prompt itself, already decoded and bounded.
    pub(super) inline: &'a [Image],
    /// `--attach` values, still to be read from the filesystem or fetched.
    pub(super) specs: &'a [String],
}

impl Attachments<'_> {
    pub(super) fn is_empty(&self) -> bool {
        self.inline.is_empty() && self.specs.is_empty()
    }
}

pub(super) async fn acquire_attachments(
    attachments: Attachments<'_>,
    source: &Arc<model::CliGenerationModelSource>,
    ports: &Arc<RunEffectPorts>,
    cancel: &CancelToken,
    run_deadline: Option<std::time::Instant>,
    cwd: &Path,
    blobs: &dyn ego_core::store::blob::BlobStore,
) -> Result<Vec<ImageRef>, RunTermination> {
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    // Resolving the profile is what makes the refusal truthful rather than
    // generic: both admission paths need this model's own image limits, and
    // this is the first point on either where they are known.
    let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(ports))) as Arc<dyn ego_ai::http::Http>;
    let profile = source
        .bind(http, cancel.clone(), run_deadline)
        .resolve(GenerationModelRequest::new(GenerationRole::Main))
        .await
        .map_err(generation_refusal)?;
    let usage = |error: crate::cli::UsageError| RunTermination::UsageError {
        message: error.to_string(),
    };
    // Images that came inside the request come first, because they came with
    // the prompt itself; `--attach` values follow in the order they were given.
    let mut images = attachment::publish(
        attachments.inline,
        &profile.capabilities,
        profile.identity.0.as_str(),
        &profile.request_model,
        blobs,
    )
    .map_err(usage)?;
    images.extend(
        attachment::acquire(
            attachments.specs,
            &profile.capabilities,
            profile.identity.0.as_str(),
            &profile.request_model,
            cwd,
            ports.as_ref(),
            blobs,
        )
        .await
        .map_err(usage)?,
    );
    Ok(images)
}

/// Map the first latched cancellation cause to the CLI termination contract.
pub fn cancelled_termination(cancel: &CancelToken) -> RunTermination {
    match cancel.cause() {
        Some(CancelCause::Timeout) => RunTermination::TimedOut,
        Some(CancelCause::User) => RunTermination::Signalled(Signal::Int),
        _ => RunTermination::Signalled(Signal::Term),
    }
}

pub(super) enum HeadlessInput<'a> {
    Prompt(&'a RunPrompt),
    Resumed(ego_core::event::id::TurnId),
    Answered {
        question: ItemId,
        answer: QuestionAnswer,
        policy: Arc<PolicyEngine>,
    },
}

pub(super) enum RuntimeResume {
    Model(TurnId),
    Foreign {
        question: ItemId,
        answer: QuestionAnswer,
        policy: Arc<PolicyEngine>,
    },
}

pub(super) struct SessionHookComposition<'a> {
    pub(super) state: SessionHookState<'a>,
    pub(super) binding: Arc<dyn HookInvocationBinding + 'a>,
    pub(super) mcp: Option<Arc<dyn McpRuntimeBinding>>,
    pub(super) runtime: SessionHookRuntime<'a>,
    pub(super) turn: TurnId,
}

impl<'a> SessionHookComposition<'a> {
    pub(super) fn bind_policy(
        &self,
        policy: Arc<PolicyEngine>,
    ) -> Result<SessionHookRuntime<'a>, RunTermination> {
        let invoker = self
            .binding
            .bind(policy, self.mcp.as_deref())
            .map_err(|message| RunTermination::Failed { message })?;
        Ok(self.state.bind(invoker))
    }

    pub(super) fn bind_current(
        &self,
        source: &dyn GenerationPolicySource,
    ) -> Result<SessionHookRuntime<'a>, RunTermination> {
        let policy = source
            .current()
            .map_err(|message| RunTermination::Failed { message })?;
        self.bind_policy(policy)
    }
}

pub(super) struct SessionHookInput<'a> {
    pub(super) session: SessionId,
    pub(super) journal: SharedJournal<'a>,
    pub(super) run_evidence: &'a dyn ego_core::evidence::EvidenceStore,
    pub(super) response_evidence: &'a dyn ego_core::store::evidence::EvidenceStore,
    pub(super) admission: Arc<EvidenceAdmissionService>,
    pub(super) invocation: RunInvocationId,
    pub(super) gate: SharedBudgetGate,
    pub(super) layers: Option<&'a [Sourced<ConfigLayer>]>,
    pub(super) trust: Trust,
    pub(super) source: Arc<model::CliGenerationModelSource>,
    pub(super) ports: Arc<RunEffectPorts>,
    pub(super) policy: Arc<PolicyEngine>,
    pub(super) effects: ToolEffectPorts<'a>,
    pub(super) mcp_runtime: Option<Arc<dyn McpRuntimeBinding>>,
    pub(super) cancel: CancelToken,
    pub(super) deadline: Option<Instant>,
    pub(super) instructions_loaded: bool,
}

pub(super) async fn compose_session_hooks<'a>(
    input: SessionHookInput<'a>,
) -> Result<SessionHookComposition<'a>, RunTermination> {
    let SessionHookInput {
        session,
        journal,
        run_evidence,
        response_evidence,
        admission,
        invocation,
        gate,
        layers,
        trust,
        source,
        ports,
        policy,
        effects,
        mcp_runtime,
        cancel,
        deadline,
        instructions_loaded,
    } = input;
    let catalogue =
        HookCatalogue::admit_typed(layers.unwrap_or_default(), &[], trust).map_err(|error| {
            RunTermination::Failed {
                message: format!("the session hook catalogue is invalid: {error}"),
            }
        })?;
    let admitted = catalogue.evidence();
    let (session_start_turn, session_end_turn) = journal
        .inspect(|state| {
            let lifecycle_turn = |lifecycle| {
                let key =
                    HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Session { session });
                state
                    .hook_occurrence(&key)
                    .map(|occurrence| occurrence.turn())
            };
            (
                lifecycle_turn(HookLifecycle::SessionStart),
                lifecycle_turn(HookLifecycle::SessionEnd),
            )
        })
        .map_err(hook_journal_termination)?;
    let turn = match (session_start_turn, session_end_turn) {
        (Some(start), Some(end)) if start != end => {
            return Err(RunTermination::Failed {
                message:
                    "the durable session lifecycle occurrences disagree about their causality turn"
                        .to_owned(),
            });
        }
        (Some(turn), _) | (_, Some(turn)) => turn,
        (None, None) => TurnId::new(),
    };
    let already_admitted = journal
        .inspect(|state| state.hook_catalogue() == &admitted)
        .map_err(hook_journal_termination)?;
    if !already_admitted {
        journal
            .append(
                turn,
                None,
                None,
                EventKind::HookCatalogueAdmitted {
                    descriptors: admitted,
                },
            )
            .map_err(hook_journal_termination)?;
    }
    let durable_started = journal
        .inspect(|state| {
            state
                .hook_occurrences()
                .iter()
                .flat_map(|occurrence| {
                    occurrence.starts().iter().map(|start| {
                        HookStartedEvidence::new(
                            start.event(),
                            occurrence.event(),
                            start.hook(),
                            start.invocation(),
                            start.activation(),
                        )
                    })
                })
                .collect::<Vec<_>>()
        })
        .map_err(hook_journal_termination)?;
    let prompt = Arc::new(RunToollessGenerationService::new(
        ToollessGenerationOwner::new(
            session,
            journal.clone(),
            run_evidence,
            response_evidence,
            admission,
            invocation,
            gate,
        ),
        source,
        ports,
    ));
    let binding: Arc<dyn HookInvocationBinding + 'a> =
        Arc::new(CliHookInvocationBinding::new(effects, prompt));
    let state = SessionHookState::new(
        HookLifecycleDispatcher::new(
            HookRunner::new_with_durable_started(durable_started),
            catalogue,
        ),
        journal,
        cancel,
        deadline.unwrap_or_else(unbounded_hook_deadline),
    );
    let invoker = binding
        .bind(policy, mcp_runtime.as_deref())
        .map_err(|message| RunTermination::Failed { message })?;
    let runtime = state.bind(invoker);
    let _ = dispatch_session_hook(&runtime, session, turn, HookLifecycle::SessionStart).await?;
    if instructions_loaded {
        let _ = dispatch_session_hook(&runtime, session, turn, HookLifecycle::InstructionsLoaded)
            .await?;
    }
    Ok(SessionHookComposition {
        state,
        binding,
        mcp: mcp_runtime,
        runtime,
        turn,
    })
}

pub(super) async fn dispatch_session_hook(
    runtime: &SessionHookRuntime<'_>,
    session: SessionId,
    turn: TurnId,
    lifecycle: HookLifecycle,
) -> Result<ego_core::hook::HookRunResult, RunTermination> {
    let occurrence = HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Session { session }),
        HookEvent::session(lifecycle),
        HookCausality::session(session, turn),
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    let result = runtime
        .dispatch(Producer::Main, occurrence)
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    if result.is_blocked() || result.is_observer_failure() {
        return Err(RunTermination::Failed {
            message: format!("the {} hook lifecycle did not complete", lifecycle.as_str()),
        });
    }
    Ok(result)
}

pub(super) async fn dispatch_request_hook(
    runtime: &SessionHookRuntime<'_>,
    request: RequestId,
    turn: TurnId,
    parent: Option<EventId>,
    lifecycle: HookLifecycle,
) -> Result<ego_core::hook::HookRunResult, RunTermination> {
    let occurrence = HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Request { request }),
        HookEvent::session(lifecycle),
        HookCausality::request(request, turn, parent),
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    let result = runtime
        .dispatch(Producer::Main, occurrence)
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    if result.is_blocked() || result.is_observer_failure() {
        return Err(RunTermination::Failed {
            message: format!("the {} hook lifecycle did not complete", lifecycle.as_str()),
        });
    }
    Ok(result)
}

pub(super) fn hook_journal_termination(error: impl std::fmt::Display) -> RunTermination {
    RunTermination::Failed {
        message: format!("the session hook journal failed: {error}"),
    }
}

pub(super) fn unbounded_hook_deadline() -> Instant {
    Instant::now()
        .checked_add(Duration::from_secs(u32::MAX.into()))
        .expect("the monotonic clock represents the process lifetime")
}

#[derive(Debug)]
pub(super) struct CliChildIds;

impl IdSource for CliChildIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}

/// The CLI's answer to everything a child run needs from its parent process.
///
/// One type where there were five, implementing one port where there were six.
/// They were never independently substitutable: the journal, the parent blob
/// store, the artifact store, the profile resolver and the selection admission
/// were composed together at exactly one call site and always all at once.
pub(super) struct CliChildHost {
    pub(super) store: Arc<JsonlStore>,
    pub(super) session: SessionId,
    pub(super) blobs: SessionEvidenceStore,
    pub(super) artifacts: Arc<dyn RunArtifactStore>,
    pub(super) executor: Arc<CliRunExecutor>,
    pub(super) root: RunRoot,
}

impl CliChildHost {
    pub(super) fn blob_error(error: impl std::fmt::Display) -> ChildResultReaderError {
        ChildResultReaderError::Read {
            diagnostic: error.to_string(),
        }
    }
}

impl ChildHost for CliChildHost {
    fn read_journal(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<Vec<ego_core::event::domain::DomainEvent>, ChildJournalError> {
        let mut stream = SessionStore::read(self.store.as_ref(), &self.session)
            .map_err(|error| ChildJournalError::new(error.to_string()))?;
        let mut events = Vec::new();
        loop {
            let entry = stream.next().now_or_never().ok_or_else(|| {
                ChildJournalError::new("the eager JSONL journal unexpectedly became asynchronous")
            })?;
            let Some(entry) = entry else {
                break;
            };
            let entry = entry.map_err(|error| ChildJournalError::new(error.to_string()))?;
            let SessionEntry::Event(event) = entry else {
                continue;
            };
            if matches!(
                &event.kind,
                EventKind::SubagentRequested { invocation: found, .. }
                    | EventKind::SubagentSpawned { invocation: found, .. }
                    | EventKind::SubagentSettled { invocation: found, .. }
                    if *found == invocation
            ) {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn read_rendered_task(&self, reference: &BlobRef) -> Result<Vec<u8>, ChildResultReaderError> {
        self.blobs.read(reference).map_err(Self::blob_error)
    }

    fn read_final_assistant(
        &self,
        run: RunId,
    ) -> Result<ChildResultEvidence, ChildResultReaderError> {
        let artifact = self
            .artifacts
            .read(run, RunArtifactId::output())
            .map_err(Self::blob_error)?;
        let id = BlobId::parse(ego_core::digest::sha256_hex(artifact.bytes()))
            .map_err(Self::blob_error)?;
        Ok(ChildResultEvidence::new(
            BlobRef::new(id, artifact.bytes().len() as u64),
            artifact.bytes().to_vec(),
        ))
    }

    fn copy_to_parent(
        &self,
        _child: RunId,
        evidence: &ChildResultEvidence,
    ) -> Result<BlobRef, ChildResultReaderError> {
        let mut writer = self.blobs.begin().map_err(Self::blob_error)?;
        writer.append(evidence.bytes()).map_err(Self::blob_error)?;
        writer.commit().map_err(Self::blob_error)
    }

    fn resolve_profile(&self, name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("child profile name is empty".to_owned());
        }
        let mut args = self.executor.args.clone();
        args.profile = Some(name.to_owned());
        args.profile_source = Some(ego_core::config::Source::Flags);
        match self.executor.frozen.as_deref() {
            Some(frozen) => frozen
                .resolve(&args)
                .map_err(|error| error.to_string())
                .and_then(|effective| {
                    self.executor.admit_child_authority(
                        &effective,
                        args.offline || effective.network == ego_core::config::Network::Off,
                    )
                }),
            None => rooted_settings(
                &args,
                self.root.as_path().to_path_buf(),
                |_| Trust::Untrusted,
                &Stderr,
            )
            .map_err(|error| error.to_string())
            .and_then(|settings| {
                self.executor
                    .admit_child_authority(&settings.effective, settings.confinement.offline())
            }),
        }
    }

    fn admit_selections(
        &self,
        request: &ChildRequest,
        bindings: &RunExecutionBindings,
    ) -> Result<(RunSelections, RunExecutionBindings), String> {
        let tools = request.requested_tools().ok_or_else(|| {
            "forked skill did not supply its admitted tool intersection".to_owned()
        })?;
        Ok((
            request.selections().clone(),
            bindings.clone().with_effect_tools(tools.clone()),
        ))
    }
}

pub(super) struct CliChildSinkFactory;

impl ego_core::run::EventSinkFactory for CliChildSinkFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

pub(super) struct ChildComposition {
    pub(super) parent_run: RunId,
    pub(super) parent_session: SessionId,
    pub(super) root: RunRoot,
    pub(super) store: Arc<JsonlStore>,
    pub(super) executor: Arc<CliRunExecutor>,
    pub(super) gate: SharedBudgetGate,
    pub(super) bindings: RunExecutionBindings,
    pub(super) blobs: SessionEvidenceStore,
}

pub(super) fn compose_child_service(composition: ChildComposition) -> ChildRunService {
    let ChildComposition {
        parent_run,
        parent_session,
        root,
        store,
        executor,
        gate,
        bindings,
        blobs,
    } = composition;
    let controller = Arc::new(RunController::new(
        Arc::clone(&store),
        Arc::clone(&executor),
        Arc::new(SystemClock),
        Arc::new(CliChildIds),
        RunRetention::new(NonZeroUsize::new(64).expect("non-zero child retention"))
            .expect("valid child retention"),
    ));
    let artifacts: Arc<dyn RunArtifactStore> = store.clone();
    ChildRunService::new(
        controller,
        gate,
        root.clone(),
        bindings,
        ChildServicePorts::new(
            parent_run,
            Arc::new(CliChildHost {
                store,
                session: parent_session,
                blobs,
                artifacts,
                executor,
                root,
            }),
        ),
    )
}

pub(super) type ChildExecutionTask =
    tokio::task::JoinHandle<Result<RunSnapshot, ChildServiceError>>;

pub(super) struct BackgroundSkill {
    pub(super) service: ChildRunService,
    pub(super) invocation: SkillInvocation,
    pub(super) turn: TurnId,
    pub(super) execution: Option<ChildExecutionTask>,
}

/// The posture a child is spawned at.
///
/// A subagent is subject to the rules its parent is subject to, so the default
/// is the parent's own posture: the same config layers, the same project MCP
/// servers, the same skills, the same hooks. A role declared `bare` is spent
/// without them.
///
/// The parent's posture is the ceiling and this function is why: `bare` can
/// only take it away, and there is no argument that returns `Trusted` for an
/// untrusted parent. A declaration in a repository cannot promote the run that
/// reads it.
pub(super) const fn child_posture(parent: Trust, bare: bool) -> Trust {
    match (parent, bare) {
        (Trust::Trusted, false) => Trust::Trusted,
        _ => Trust::Untrusted,
    }
}

pub(super) struct CliSkillInvocationOwner<'a> {
    pub(super) service: ChildRunService,
    pub(super) hooks: SessionHookRuntime<'a>,
    pub(super) parent_cancel: CancelToken,
    /// The posture this run was admitted at, and so the posture its children
    /// are spawned at. A child is subject to the rules its parent is subject
    /// to; the only thing an invocation can do with this value is lower it.
    pub(super) trust: Trust,
    pub(super) background: Arc<Mutex<BTreeMap<SubagentInvocationId, BackgroundSkill>>>,
}

impl<'a> CliSkillInvocationOwner<'a> {
    pub(super) fn new(
        service: ChildRunService,
        hooks: SessionHookRuntime<'a>,
        parent_cancel: CancelToken,
        trust: Trust,
    ) -> Self {
        Self {
            service,
            hooks,
            parent_cancel,
            trust,
            background: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub(super) fn launch(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<Option<ChildExecutionTask>, SkillError> {
        let Some(execution) = self
            .service
            .take_execution(invocation)
            .map_err(|error| SkillError::new(error.to_string()))?
        else {
            return Ok(None);
        };
        let service = self.service.clone();
        Ok(Some(tokio::spawn(async move {
            service.execute(execution).await
        })))
    }

    pub(super) async fn await_execution(task: ChildExecutionTask) -> Result<(), SkillError> {
        task.await
            .map_err(|error| SkillError::new(format!("child execution task failed: {error}")))?
            .map(|_| ())
            .map_err(|error| SkillError::new(error.to_string()))
    }

    pub(super) async fn drain(&self) -> Result<(), SkillError> {
        Self::drain_background(
            Arc::clone(&self.background),
            &self.hooks,
            self.parent_cancel.is_cancelled(),
        )
        .await
    }

    pub(super) async fn drain_background(
        background: Arc<Mutex<BTreeMap<SubagentInvocationId, BackgroundSkill>>>,
        hooks: &SessionHookRuntime<'_>,
        cancel_children: bool,
    ) -> Result<(), SkillError> {
        let pending =
            std::mem::take(&mut *background.lock().expect("background skill invocations"));
        let mut first_error = None;
        for (id, background) in pending {
            let BackgroundSkill {
                service,
                invocation,
                turn,
                execution,
            } = background;
            if cancel_children && let Err(error) = service.cancel_owned(id).await {
                first_error.get_or_insert_with(|| SkillError::new(error.to_string()));
            }
            if let Some(task) = execution
                && let Err(error) = Self::await_execution(task).await
            {
                first_error.get_or_insert(error);
            }
            let source = match invocation.hook_source() {
                Ok(source) => source,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            if let Err(error) = hooks.hook_state().replace_skill_sources(turn, &[source]) {
                first_error.get_or_insert_with(|| SkillError::new(error.to_string()));
                continue;
            }
            let collected = service
                .collect_background(id, hooks)
                .await
                .map_err(|error| SkillError::new(error.to_string()));
            let cleared = hooks
                .hook_state()
                .replace_skill_sources(turn, &[])
                .map_err(|error| SkillError::new(error.to_string()));
            if let Err(error) = collected {
                first_error.get_or_insert(error);
            }
            if let Err(error) = cleared {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl SkillInvocationPort for CliSkillInvocationOwner<'_> {
    fn invoke<'a>(
        &'a self,
        invocation: SkillInvocation,
        parent_turn: TurnId,
        parent_item: ItemId,
        parent_event: EventId,
        effect_tools: BTreeSet<String>,
    ) -> futures::future::BoxFuture<'a, Result<SkillInvocationOutcome, SkillError>> {
        Box::pin(async move {
            let Some(wait) = invocation.fork_wait() else {
                return Ok(SkillInvocationOutcome::Parent(Box::new(invocation)));
            };
            let rendered = invocation
                .rendered_ref()
                .cloned()
                .ok_or_else(|| SkillError::new("forked skill has no rendered task blob"))?;
            let thinking = invocation
                .effort()
                .map(|effort| {
                    ego_core::skill::parse_skill_effort(effort).ok_or_else(|| {
                        SkillError::new(format!(
                            "admitted child reasoning effort `{effort}` is unsupported"
                        ))
                    })
                })
                .transpose()?;
            let posture = child_posture(self.trust, invocation.bare());
            let selections = RunSelections::new(
                invocation.model().map(|model| ModelRef(model.to_owned())),
                thinking,
                None,
                None,
                invocation.profile().map(str::to_owned),
            )
            .with_trust(posture);
            let id = invocation.subagent_invocation();
            if self
                .service
                .recover_from_durable(
                    id,
                    RecoveryCut::BeforeParentSettlement,
                    &self.hooks,
                    wait,
                    effect_tools.clone(),
                )
                .await
                .map_err(|error| SkillError::new(error.to_string()))?
                .is_none()
            {
                let mut request = ChildRequest::new(
                    id,
                    ChildRequestOrigin::new(
                        self.service.parent_run(),
                        parent_turn,
                        parent_item,
                        parent_event,
                    ),
                    rendered,
                    selections,
                    wait,
                )
                .with_requested_tools(effect_tools);
                if let Some(profile) = invocation.profile() {
                    request = request.with_requested_profile(profile.to_owned());
                }
                self.service
                    .start(request, &self.hooks)
                    .await
                    .map_err(|error| SkillError::new(error.to_string()))?;
            }
            let execution = self.launch(id)?;
            self.background
                .lock()
                .expect("background skill invocations")
                .entry(id)
                .or_insert_with(|| BackgroundSkill {
                    service: self.service.clone(),
                    invocation: invocation.clone(),
                    turn: parent_turn,
                    execution,
                });
            match wait {
                ChildWait::Foreground => {
                    let collected = self
                        .service
                        .collect_background(id, &self.hooks)
                        .await
                        .map(SkillInvocationOutcome::Foreground)
                        .map_err(|error| SkillError::new(error.to_string()));
                    if collected.is_ok() {
                        self.background
                            .lock()
                            .expect("background skill invocations")
                            .remove(&id);
                    }
                    collected
                }
                ChildWait::Background => Ok(SkillInvocationOutcome::Background(id)),
            }
        })
    }
}
