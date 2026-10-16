use super::*;
use crate::render::caps::{Depth, OscProfile};
use ego_core::event::domain::{DomainEvent, Envelope};
use ego_core::event::hook::{HookDecision, HookOutcome, HookSource};
use ego_core::event::id::Seq;
use ego_core::event::skill::{SkillInvocationId, SkillSourceAuthority};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::hook::{
    HookConfigEntry, HookFuture, HookHandlerConfig, HookInvocationPort, HookInvocationRequest,
};
use ego_core::question::{
    QuestionAnswer, QuestionMode, QuestionOption, QuestionRequest, QuestionTarget,
};
use ego_core::run::RunOrigin;
use ego_core::store::evidence::{EvidenceId, EvidenceRef};
use ego_core::store::session_store::SessionMeta;
use ego_core::store::session_store::{SessionStore, StoreError};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::tempdir;

#[test]
fn trusted_project_instructions_begin_at_the_shared_gits_root() {
    let tree = tempdir().expect("fixture directory");
    let gits = tree.path().join("Gits");
    let project = gits.join("personal/ego");
    let cwd = project.join("src");
    std::fs::create_dir_all(&cwd).expect("fixture hierarchy");
    for (relative, text) in [
        ("AGENTS.md", "shared"),
        ("personal/AGENTS.md", "ancestor"),
        ("personal/ego/AGENTS.md", "project"),
        ("personal/ego/src/AGENTS.md", "local"),
    ] {
        std::fs::write(gits.join(relative), text).expect("fixture instruction");
    }

    let root = crate::trust::instruction_discovery_root(&project, tree.path())
        .expect("canonical instruction discovery root");
    let cwd = std::fs::canonicalize(cwd).expect("canonical working directory");
    let loaded = ego_core::config::load(
        &ego_tools::instructions::OsInstructionIo,
        &root,
        &cwd,
        &ego_core::config::Selection::Ambient,
        ego_core::config::Limits::default(),
    )
    .expect("nested project is inside the discovery root");
    let text: Vec<_> = loaded
        .loaded
        .iter()
        .map(|file| file.text.as_str())
        .collect();
    assert_eq!(text, ["shared", "ancestor", "project", "local"]);

    let bare = ego_core::config::load(
        &ego_tools::instructions::OsInstructionIo,
        &root,
        &cwd,
        &ego_core::config::Selection::Disabled,
        ego_core::config::Limits::default(),
    )
    .expect("bare discovery is inert");
    assert!(bare.loaded.is_empty());

    let lookalike = tree.path().join("Gits-elsewhere/project");
    std::fs::create_dir_all(&lookalike).expect("lookalike workspace");
    assert_eq!(
        crate::trust::instruction_discovery_root(&lookalike, tree.path())
            .expect("canonical lookalike root"),
        std::fs::canonicalize(lookalike).expect("canonical lookalike"),
        "a similarly named directory does not inherit shared instructions"
    );

    let scratch = gits.join(".tmp/test-fixture");
    std::fs::create_dir_all(&scratch).expect("scratch workspace");
    assert_eq!(
        crate::trust::instruction_discovery_root(&scratch, tree.path())
            .expect("canonical scratch root"),
        std::fs::canonicalize(scratch).expect("canonical scratch"),
        "scratch fixtures do not inherit the human's shared instructions"
    );
}

struct UnusedStore;

impl ego_core::config::UserConfigStore for UnusedStore {
    fn snapshot(
        &self,
        _target: ego_core::config::UserConfigTarget,
    ) -> Result<ego_core::config::UserConfigSnapshot, ego_core::config::UserConfigError> {
        panic!("remember preview must not read the store")
    }

    fn rule_snapshot(
        &self,
        _target: ego_core::config::UserConfigTarget,
    ) -> Result<ego_core::config::RuleSnapshot, ego_core::config::UserConfigError> {
        panic!("remember preview must not read the store")
    }

    fn mutate(
        &self,
        _mutation: ego_core::config::UserConfigMutation,
    ) -> Result<ego_core::config::UserConfigMutationResult, ego_core::config::UserConfigError> {
        panic!("remember preview must not mutate the store")
    }
}

#[derive(Default)]
struct HookCaptureStore;

impl SessionStore for HookCaptureStore {
    fn append(&self, _id: &SessionId, _event: &DomainEvent) -> Result<(), StoreError> {
        Ok(())
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<futures::stream::BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError>
    {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<ego_core::run::RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        Err(StoreError::NotFound)
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

struct HookCaptureClock;

impl Clock for HookCaptureClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1_000)
    }
}

#[derive(Default)]
struct HookCaptureInvoker;

impl HookInvocationPort for HookCaptureInvoker {
    fn invoke<'a>(&'a self, _request: HookInvocationRequest) -> HookFuture<'a> {
        Box::pin(std::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }
}

fn pending_resume_state(target: QuestionTarget) -> (SessionState, ResumeAnswer, TurnId, EventId) {
    let question = ItemId::new();
    let turn = TurnId::new();
    let request_event = EventId::new();
    let request = QuestionRequest::new(
        "Continue?",
        QuestionMode::Single,
        vec![QuestionOption::new("yes", "Yes")],
        false,
    );
    let mut state = SessionState::default();
    state.apply(&DomainEvent {
        env: Envelope {
            seq: Seq::default(),
            id: request_event,
            turn,
            item: Some(question),
            parent: None,
            ts: Timestamp::from_millis(1_000),
            producer: Producer::Main,
        },
        kind: EventKind::QuestionRequested { request, target },
    });
    assert_eq!(
        state.pending_question().map(|pending| pending.item_id()),
        Some(question)
    );
    (
        state,
        ResumeAnswer {
            id: question,
            json: r#""yes""#.to_owned(),
        },
        turn,
        request_event,
    )
}

#[derive(Default, Clone)]
struct LaunchBarrierStore {
    events: Arc<Mutex<Vec<DomainEvent>>>,
    transactions: Arc<Mutex<Vec<ego_core::run::RunTransaction>>>,
}

impl SessionStore for LaunchBarrierStore {
    fn append(&self, _id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<futures::stream::BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError>
    {
        let events = self.events.lock().unwrap().clone();
        let transactions = self.transactions.lock().unwrap().clone();
        Ok(Box::pin(futures::stream::iter(
            events
                .into_iter()
                .map(|event| Ok(SessionEntry::Event(event)))
                .chain(
                    transactions
                        .into_iter()
                        .map(|transaction| Ok(SessionEntry::RunTransaction(transaction))),
                ),
        )))
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<ego_core::run::RunRevision>,
        transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        let revision = transaction.revision();
        self.transactions.lock().unwrap().push(transaction);
        Ok(ego_core::run::RunAppend::Committed { revision })
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct LaunchBarrierClock;

impl Clock for LaunchBarrierClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1_000)
    }
}

#[derive(Default)]
struct LaunchBarrierIds;

impl ego_core::run::IdSource for LaunchBarrierIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}

struct LaunchBarrierSink;

impl ego_core::run::EventSinkFactory for LaunchBarrierSink {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

struct LaunchBarrierExecutor {
    crossed: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl LaunchBarrierExecutor {
    fn arm(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        *self.crossed.lock().unwrap() = Some(sender);
        receiver
    }
}

impl RunExecutor for LaunchBarrierExecutor {
    type Prepared = ();

    fn prepare<'a>(
        &'a self,
        _request: &'a RunPreparation,
    ) -> futures::future::BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        Box::pin(std::future::ready(Ok(RunPrepared::new(
            ModelRef("launch-test-model".to_owned()),
            (),
        ))))
    }

    fn admit_run_context<'a>(
        &'a self,
        _prepared: &'a Self::Prepared,
    ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        Box::pin(std::future::ready(Ok(EvidenceWrite::Published)))
    }

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> futures::future::BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        Box::pin(std::future::ready(Ok(RunHandle::new(request.run_id, ()))))
    }

    fn recover<'a>(
        &'a self,
        _run: &'a RunId,
    ) -> futures::future::BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
        Box::pin(std::future::ready(Ok(RecoveryEvidence::NotRecoverable)))
    }

    fn observe<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> futures::future::BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(std::future::ready(Ok(RunSettlement::Completed)))
    }

    fn submit<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _prompt: &'a RunPrompt,
        _prepared: Self::Prepared,
        _bindings: RunExecutionBindings,
    ) -> futures::future::BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        if let Some(sender) = self.crossed.lock().unwrap().take() {
            sender
                .send(())
                .expect("launch barrier receiver remains alive");
        }
        Box::pin(std::future::ready(Ok(RunSettlement::Completed)))
    }

    /// A no-op for the same reason `interrupt` is one: this double never
    /// runs a turn, so there is nothing here that a hold could hold back.
    fn pause<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _request: RequestId,
    ) -> futures::future::BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn interrupt<'a>(
        &'a self,
        _handle: &'a RunHandle,
    ) -> futures::future::BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// The launch-only fixture host: it answers what a launch needs and
/// refuses what a launch does not reach.
///
/// One fake for one port. Six fakes existed because six ports did, and
/// every one of them was constructed in the same breath as the others —
/// which is the evidence that none of them was a boundary.
struct LaunchBarrierHost(&'static LaunchBarrierStore);

impl ChildHost for LaunchBarrierHost {
    fn read_journal(
        &self,
        _invocation: SubagentInvocationId,
    ) -> Result<Vec<DomainEvent>, ChildJournalError> {
        Ok(self.0.events.lock().unwrap().clone())
    }

    fn read_rendered_task(&self, _reference: &BlobRef) -> Result<Vec<u8>, ChildResultReaderError> {
        Ok(b"background launch task".to_vec())
    }

    fn read_final_assistant(
        &self,
        run: RunId,
    ) -> Result<ChildResultEvidence, ChildResultReaderError> {
        Err(ChildResultReaderError::Missing { run })
    }

    fn copy_to_parent(
        &self,
        _child: RunId,
        _evidence: &ChildResultEvidence,
    ) -> Result<BlobRef, ChildResultReaderError> {
        Err(ChildResultReaderError::Read {
            diagnostic: "collection is outside this launch-only fixture".to_owned(),
        })
    }

    fn resolve_profile(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }

    fn admit_selections(
        &self,
        request: &ChildRequest,
        bindings: &RunExecutionBindings,
    ) -> Result<(RunSelections, RunExecutionBindings), String> {
        Ok((request.selections().clone(), bindings.clone()))
    }
}

fn launch_barrier_parent_hooks(
    store: &'static LaunchBarrierStore,
    session: SessionId,
) -> SessionHookRuntime<'static> {
    let journal = SharedJournal::new(Journal::new(
        session,
        store,
        &LaunchBarrierClock,
        &NullSink,
        Producer::Main,
        {
            let mut state = SessionState::default();
            for event in store.events.lock().unwrap().iter() {
                state.apply(event);
            }
            state
        },
    ));
    let catalogue = HookCatalogue::admit_typed(&[], &[], Trust::Trusted)
        .expect("empty launch barrier hook catalogue");
    SessionHookState::new(
        HookLifecycleDispatcher::new(HookRunner::new(), catalogue),
        journal,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(30),
    )
    .bind(Arc::new(HookCaptureInvoker))
}

#[tokio::test]
async fn cli_background_launch_crosses_execution_barrier_without_collection_future() {
    let store: &'static LaunchBarrierStore = Box::leak(Box::new(LaunchBarrierStore::default()));
    let executor = Arc::new(LaunchBarrierExecutor {
        crossed: Mutex::new(None),
    });
    let crossed = executor.arm();
    let parent_run = RunId::new(SessionId::new(), uuid::Uuid::now_v7());
    let parent_session = parent_run.session_id();
    let gate = SharedBudgetGate::new(ego_core::budget::BudgetGate::unlimited(
        CurrencyCode::new("USD").expect("launch barrier currency"),
    ));
    let bindings =
        RunExecutionBindings::new(Arc::new(LaunchBarrierSink), None).with_budget_gate(gate.clone());
    let ports = ChildServicePorts::new(parent_run, Arc::new(LaunchBarrierHost(store)));
    let controller = Arc::new(RunController::new(
        Arc::new(store.clone()),
        Arc::clone(&executor),
        Arc::new(LaunchBarrierClock),
        Arc::new(LaunchBarrierIds),
        RunRetention::new(NonZeroUsize::new(8).expect("retention")).expect("retention"),
    ));
    let service = ChildRunService::new(
        controller,
        gate,
        RunRoot::new(
            std::env::current_dir()
                .expect("launch barrier current directory")
                .join("workspace"),
        )
        .expect("launch barrier root"),
        bindings,
        ports,
    );
    let invocation = SubagentInvocationId::new();
    let parent_turn = TurnId::new();
    let parent_item = ItemId::new();
    let parent_event = EventId::new();
    store
        .append(
            &parent_session,
            &DomainEvent {
                env: Envelope {
                    seq: Seq::default(),
                    id: parent_event,
                    turn: parent_turn,
                    item: Some(parent_item),
                    parent: None,
                    ts: Timestamp::from_millis(1_000),
                    producer: Producer::Main,
                },
                kind: EventKind::ToolCallStarted {
                    name: "skill".to_owned(),
                    args: "{}".to_owned(),
                    call_id: Some("launch-barrier".to_owned()),
                },
            },
        )
        .expect("parent event is durable before child admission");
    let hooks = launch_barrier_parent_hooks(store, parent_session);
    let task = b"background launch task";
    service
        .start(
            ChildRequest::new(
                invocation,
                ChildRequestOrigin::new(parent_run, parent_turn, parent_item, parent_event),
                BlobRef::new(
                    BlobId::parse(ego_core::digest::sha256_hex(task)).expect("task blob id"),
                    task.len() as u64,
                ),
                RunSelections::empty(),
                ChildWait::Background,
            )
            .with_requested_tools(["read"]),
            &hooks,
        )
        .await
        .expect("background child admission");
    let owner = CliSkillInvocationOwner::new(service, hooks, CancelToken::new(), Trust::Untrusted);
    let execution = owner
        .launch(invocation)
        .expect("launch does not fail")
        .expect("admitted background execution is transferred");
    let completed = tokio::time::timeout(Duration::from_secs(1), execution)
        .await
        .expect("background execution does not stall")
        .expect("background execution task does not panic");
    completed.expect("background execution settles");
    tokio::time::timeout(Duration::from_secs(1), crossed)
        .await
        .expect("child execution crosses its barrier before collection exists")
        .expect("launch barrier sender remains alive");
}

#[test]
fn workspace_accepts_a_directory_but_rejects_an_existing_regular_file() {
    let owner = tempdir().expect("workspace owner");
    let directory = owner.path().join("workspace");
    std::fs::create_dir(&directory).expect("workspace directory");
    let regular_file = owner.path().join("workspace-file");
    std::fs::write(&regular_file, b"not a workspace").expect("workspace file");

    let directory_arg = directory.clone();
    assert_eq!(
        workspace(Some(&directory_arg)).expect("directory is a workspace"),
        std::fs::canonicalize(directory).expect("canonical workspace")
    );

    let file_arg = regular_file.clone();
    assert!(
        workspace(Some(&file_arg)).is_err(),
        "an existing regular file must be rejected as a workspace root: {}",
        regular_file.display()
    );
}

/// A refusal an operator can act on names four things.
///
/// The mode that asked, the guarantee that is missing in words, the
/// backend that failed to demonstrate it, and the way out. The bitflag
/// `Debug` text named none of them.
#[test]
fn a_capability_refusal_names_the_mode_the_guarantee_the_backend_and_the_remedies() {
    let owner = tempdir().expect("refusal fixture owner");
    let workspace = CanonPath::new(owner.path().to_path_buf());
    let confinement = RunConfinement::new(Confinement::Workspace, false, workspace)
        .expect("workspace confinement");
    let missing = ExecCaps::READ_SCOPED
        .union(ExecCaps::WRITE_SCOPED)
        .check(ExecCaps::READ_SCOPED)
        .expect_err("a backend without scoped writes cannot serve workspace confinement")
        .with_probe_evidence(Some(EvidenceRef {
            id: EvidenceId::parse("a".repeat(64)).expect("probe transcript identity"),
            bytes: 128,
        }));

    let termination = process_error_termination(
        ProcessError::MissingCapabilities(missing),
        &confinement,
        &CancelToken::new(),
    );
    let RunTermination::UsageError { message } = termination else {
        panic!("a backend that cannot demonstrate the requested sandbox is a usage refusal");
    };

    assert!(
        message.contains("--sandbox workspace"),
        "the refusal names the mode that asked: {message}"
    );
    assert!(
        message.contains("write only inside the workspace and ego's private scratch directory"),
        "the refusal names the missing guarantee in words: {message}"
    );
    assert!(
        message.contains(if cfg!(target_os = "macos") {
            "macOS Seatbelt"
        } else if cfg!(target_os = "linux") {
            "Linux Landlock"
        } else {
            "no confinement backend"
        }),
        "the refusal names the platform backend that failed: {message}"
    );
    assert!(
        message.contains("`--sandbox ro`") && message.contains("`--sandbox off`"),
        "the refusal offers the remedy of asking for less: {message}"
    );
    assert!(
        message.contains(&"a".repeat(64)),
        "the refusal cites the measurement that diagnosed the cause: {message}"
    );
    assert!(
        !message.contains("ExecCaps("),
        "the refusal must not fall back on the bitflag debug text: {message}"
    );
}

/// The recorded host-control failure of the 2026-10-10 E2E run: the confined
/// child was never the part that failed, so no `--sandbox` level can help.
///
/// Catches: the refusal blames the sandbox level when the host control failed.
#[test]
fn an_offline_refusal_from_a_failed_host_control_does_not_blame_the_sandbox_level() {
    let owner = tempdir().expect("refusal fixture owner");
    let workspace = CanonPath::new(owner.path().to_path_buf());
    let confinement =
        RunConfinement::new(Confinement::Adaptive, true, workspace).expect("adaptive offline");
    let failure = "offline.tcp-nonloopback.control.receiver: accept failed: \
                   Resource temporarily unavailable (os error 35)";
    let missing = ExecCaps::NO_IP_NETWORK
        .check(ExecCaps::empty())
        .expect_err("a host that proved no network denial cannot serve offline")
        .with_host_control_failure(Some(failure.to_owned()));

    let RunTermination::UsageError { message } = process_error_termination(
        ProcessError::MissingCapabilities(missing),
        &confinement,
        &CancelToken::new(),
    ) else {
        panic!("an unmet offline guarantee is a usage refusal");
    };

    assert!(
        message.contains(failure) && message.contains("host control"),
        "the refusal says the host control failed, with the recorded outcome: {message}"
    );
    assert!(
        message.contains("dropping `--offline`") && message.contains("firewall"),
        "the refusal names the actionable choices: {message}"
    );
    assert!(
        message.contains("no `--sandbox` level"),
        "the refusal says another sandbox level cannot help: {message}"
    );
    assert!(
        message.contains("not proven"),
        "the refusal does not claim the host cause as proven: {message}"
    );
}

/// Catches: a sandbox-side offline refusal gains host-control wording.
#[test]
fn an_offline_refusal_without_a_host_control_failure_keeps_the_sandbox_wording() {
    let owner = tempdir().expect("refusal fixture owner");
    let workspace = CanonPath::new(owner.path().to_path_buf());
    let confinement =
        RunConfinement::new(Confinement::Workspace, true, workspace).expect("workspace offline");
    let missing = ExecCaps::NO_IP_NETWORK
        .check(ExecCaps::empty())
        .expect_err("a host that proved no network denial cannot serve offline");

    let RunTermination::UsageError { message } = process_error_termination(
        ProcessError::MissingCapabilities(missing),
        &confinement,
        &CancelToken::new(),
    ) else {
        panic!("an unmet offline guarantee is a usage refusal");
    };

    assert!(
        message.contains("dropping `--offline`") && !message.contains("host control"),
        "{message}"
    );
}

#[tokio::test]
async fn protocol_ci_profile_is_refused_before_session_or_provider_effects() {
    let owner = tempdir().expect("protocol profile fixture owner");
    let workspace = owner.path().join("workspace");
    let session_root = owner.path().join("sessions");
    std::fs::create_dir(&workspace).expect("protocol fixture workspace");
    let store = Arc::new(JsonlStore::new(
        session_root.clone(),
        workspace.clone(),
        Box::new(OsIo),
    ));
    let invocation = RunInvocationId::new();
    let request = RunPreparation {
        run_id: RunId::new(SessionId::new(), invocation.uuid()),
        invocation,
        root: ego_core::run::RunRoot::new(workspace).expect("protocol fixture root"),
        selections: ego_core::run::RunSelections::new(
            None,
            None,
            None,
            None,
            Some("ci".to_owned()),
        ),
        continuation: false,
        origin: RunOrigin::TopLevel,
    };

    let admission = Arc::new(EvidenceAdmissionService::new());
    let child_executor = Arc::new(
        CliRunExecutor::for_acp(
            None,
            SessionShape::default(),
            Arc::clone(&store),
            Arc::clone(&admission),
        )
        .for_children(),
    );
    let result = CliPrepared::prepare(
        protocol_args(None, SessionShape::default()),
        store,
        None,
        &request,
        None,
        None,
        PreparedExecutionRuntime {
            admission,
            child_executor,
            child: false,
            child_authority: None,
        },
    )
    .await;
    let message = match result {
        Err(RunTermination::UsageError { message }) => message,
        Err(_) => panic!("protocol ci selection must be a usage refusal"),
        Ok(_) => panic!("protocol ci selection must be refused"),
    };
    assert_eq!(
        message,
        "profile ci available only through ego run or ego resume"
    );
    assert!(
        !session_root.exists(),
        "protocol profile refusal must precede session creation"
    );
}

#[test]
fn durable_compact_is_enabled_when_idle_before_confirmation_resolution_generation_or_publication() {
    let source = SessionId::new();
    let root: &'static tempfile::TempDir =
        Box::leak(Box::new(tempdir().expect("durable evidence owner")));
    let store = Box::leak(Box::new(crate::session::jsonl(
        root.path().to_path_buf(),
        PathBuf::from("/workspace"),
    )));
    let opened =
        crate::session::open(store, &source, &SystemClock).expect("durable session writer");
    let active = crate::session::ActiveSession::durable(opened, SessionState::default(), 1024)
        .expect("durable active session");

    assert_eq!(
        super::compaction_availability(false),
        repl::actions::Availability::Enabled
    );
    assert_eq!(active.id(), source);
    assert_eq!(active.state(), &SessionState::default());
}

#[derive(Debug)]
struct CompactionDecisionSeat {
    confirm: bool,
    activate: bool,
}

impl Reads for CompactionDecisionSeat {
    fn read(&self) -> repl::Line {
        repl::Line::Ended
    }
}

impl InteractiveSeat for CompactionDecisionSeat {
    fn has_human(&self) -> bool {
        true
    }

    fn read_startup_model(
        &self,
        _picker: Arc<Mutex<ModelPicker>>,
        _notice: Option<String>,
    ) -> crate::model_picker::PickerOutcome {
        panic!("compaction fixture must not ask for startup model selection");
    }

    fn trust(&self, _workspace: &Path, _context: &str) -> Trust {
        Trust::Trusted
    }

    fn permission(&self, _request: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }

    fn confirm_compaction(&self, _source: SessionId) -> bool {
        self.confirm
    }

    fn activate_session(&self, _target: SessionId) -> bool {
        self.activate
    }
}

#[test]
fn compaction_decline_is_a_control_decision_and_keeps_the_source_selected() {
    let source = SessionId::new();
    let seat = CompactionDecisionSeat {
        confirm: false,
        activate: true,
    };

    assert!(!seat.confirm_compaction(source));
}

#[test]
fn activation_failure_is_reported_after_publication_without_a_revert_request() {
    let target = SessionId::new();
    let seat = CompactionDecisionSeat {
        confirm: true,
        activate: false,
    };

    assert!(seat.confirm_compaction(SessionId::new()));
    assert!(!seat.activate_session(target));
}

#[derive(Debug)]
struct TrustingSeat {
    asked: AtomicUsize,
}

impl Reads for TrustingSeat {
    fn read(&self) -> repl::Line {
        repl::Line::Ended
    }
}

impl InteractiveSeat for TrustingSeat {
    fn has_human(&self) -> bool {
        true
    }

    fn read_startup_model(
        &self,
        _picker: Arc<Mutex<ModelPicker>>,
        _notice: Option<String>,
    ) -> crate::model_picker::PickerOutcome {
        panic!("trust fixture must not ask for startup model selection");
    }

    fn trust(&self, _workspace: &Path, _context: &str) -> Trust {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Trust::Trusted
    }

    fn permission(&self, _request: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }
}

fn interactive_args(extra: &[&str]) -> Args {
    let argv = std::iter::once("ego".to_string())
        .chain(extra.iter().map(|arg| arg.to_string()))
        .collect();
    let crate::cli::Invocation::Interactive(args) = crate::cli::parse(argv).unwrap() else {
        panic!("the test invocation is interactive");
    };
    *args
}

#[test]
fn only_an_interactive_non_bare_run_can_ask_for_trust() {
    let seat = Arc::new(TrustingSeat {
        asked: AtomicUsize::new(0),
    });
    let dyn_seat: Arc<dyn InteractiveSeat> = seat.clone();
    let between = || {};
    let interactive = Driver::Interactive {
        seat: dyn_seat,
        between: &between,
    };
    // A workspace with something to trust and a home of its own, so the
    // routing under test is the only thing that decides whether the seat
    // is consulted. What the *store* then does with the answer is
    // `trust.rs`'s contract, not this one's.
    let workspace = tempfile::tempdir().expect("a temp dir");
    let home = tempfile::tempdir().expect("a temp dir");
    std::fs::write(workspace.path().join("AGENTS.md"), "# here\n").expect("a temp dir is writable");
    let root = workspace.path();
    let home = Some(home.path());

    assert_eq!(
        run_trust(&interactive_args(&[]), &interactive, root, home, &Stderr),
        Trust::Trusted
    );
    assert_eq!(seat.asked.load(Ordering::SeqCst), 1);

    assert_eq!(
        run_trust(
            &interactive_args(&["--bare"]),
            &interactive,
            root,
            home,
            &Stderr
        ),
        Trust::Untrusted
    );
    assert_eq!(
        seat.asked.load(Ordering::SeqCst),
        1,
        "--bare must skip the seat"
    );

    assert_eq!(
        run_trust(&interactive_args(&[]), &Driver::Once, root, home, &Stderr),
        Trust::Trusted,
        "headless reuses standing trust without consulting a seat"
    );
    assert_eq!(seat.asked.load(Ordering::SeqCst), 1);
}

fn capabilities(osc: OscProfile) -> Capabilities {
    Capabilities {
        depth: Depth::TrueColor,
        width: 80,
        hyperlinks: true,
        progress: true,
        osc,
    }
}

fn announce(osc: OscProfile, quiet: bool) -> (Stdout, Capabilities, bool) {
    (Stdout::new(CancelToken::new()), capabilities(osc), quiet)
}

/// The reason any of this exists: TUICommander segments a pane into command
/// blocks from 133 A/C/D and otherwise falls back to pattern-matching
/// tool-call headers (`tuicommander/docs/FEATURES.md:187`). C opens the
/// block, D closes it carrying the exit status, and nothing may come after
/// D — anything that did would belong to the next command.
#[test]
fn the_block_opens_with_c_and_closes_with_d_carrying_the_status() {
    let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
    let announce = Announce::new(&out, capabilities, quiet);

    assert!(announce.preamble(Some("hello")).ends_with("\x1b]133;C\x07"));

    let failed = announce.postamble(&RunTermination::TaskFailed, Duration::ZERO);
    assert!(failed.ends_with("\x1b]133;D;1\x07"), "{failed:?}");
    let done = announce.postamble(&RunTermination::Done, Duration::ZERO);
    assert!(done.ends_with("\x1b]133;D;0\x07"), "{done:?}");
}

#[test]
fn a_title_is_branded_once_and_closing_keeps_only_the_fixed_prefix() {
    let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
    let announce = Announce::new(&out, capabilities, quiet);

    assert_eq!(
        announce.preamble(Some("explain the diff")),
        "\x1b]0;ɛ । explain the diff\x07\x1b]9;4;3;0\x07\x1b]133;C\x07"
    );
    assert_eq!(
        announce.preamble(None),
        "\x1b]0;ɛ । εgo\x07\x1b]9;4;3;0\x07\x1b]133;C\x07"
    );
    assert_eq!(
        announce.postamble(&RunTermination::Done, Duration::ZERO),
        "\x1b]9;4;0;0\x07\x1b]0;ɛ । \x07\x1b]133;D;0\x07"
    );
}

/// A progress indicator left set outlives the process that set it: the tab
/// stays marked until the terminal is restarted. So it is cleared on the
/// failure path too, and the failure is reported by 133 D instead.
#[test]
fn the_indicator_is_always_taken_down_however_the_run_ended() {
    let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
    let announce = Announce::new(&out, capabilities, quiet);

    assert!(
        announce.preamble(None).contains("\x1b]9;4;3;0\x07"),
        "a run of unknown length is indeterminate"
    );
    for termination in [
        RunTermination::Done,
        RunTermination::TaskFailed,
        RunTermination::Signalled(Signal::Int),
    ] {
        let closing = announce.postamble(&termination, Duration::ZERO);
        let state = if termination.exit_code() == 0 { 0 } else { 2 };
        assert!(
            closing.contains(&format!("\x1b]9;4;{state};0\x07")),
            "{termination:?} left the indicator: {closing:?}"
        );
    }
}

/// §4.5, applied to the out-of-band channel. A JSON document with an OSC in
/// it is not JSON, and the gate is what prevents that rather than a rule
/// each call site remembers.
#[test]
fn a_machine_readable_run_announces_nothing_at_all() {
    let out = Stdout::new(CancelToken::new());
    let announce = Announce::new(&out, Capabilities::plain(), false);
    assert_eq!(announce.preamble(Some("hello")), "");
    assert_eq!(
        announce.postamble(&RunTermination::Done, Duration::from_secs(3_600)),
        ""
    );
}

/// A notification is an interruption. It is worth it for a run the operator
/// stopped watching, and it is noise for one they saw finish — so the
/// duration decides, and `-q` overrules the duration.
#[test]
fn only_a_long_run_is_worth_interrupting_someone_over() {
    let (out, capabilities, _) = announce(OscProfile::Standard, false);
    let loud = Announce::new(&out, capabilities, false);

    // `\x1b]9;εgo`, not `\x1b]9;` — OSC 9;4 is the progress indicator and
    // shares the prefix.
    assert!(
        !loud
            .postamble(&RunTermination::Done, NOTIFY_AFTER - Duration::from_secs(1))
            .contains("\x1b]9;εgo"),
        "a run somebody watched finish"
    );
    let long = loud.postamble(&RunTermination::Done, Duration::from_secs(252));
    assert!(
        long.contains("\x1b]9;εgo: done after 4m 12s\x07"),
        "{long:?}"
    );

    let quiet = Announce::new(&out, capabilities, true);
    assert!(
        !quiet
            .postamble(&RunTermination::Done, Duration::from_secs(252))
            .contains("\x1b]9;εgo"),
        "-q suppresses it at any duration"
    );
}

/// The private protocol reaches TUIC and nowhere else, and it is the
/// capability gate that decides — not a call site that checks a variable.
#[test]
fn only_tuic_is_told_what_ego_is_doing() {
    let (out, standard, _) = announce(OscProfile::Standard, false);
    let elsewhere = Announce::new(&out, standard, false);
    assert!(!elsewhere.preamble(None).contains("7770"));

    let inside = Announce::new(&out, capabilities(OscProfile::Tuic), false);
    assert!(inside.preamble(None).contains("\x1b]7770;state=busy\x07"));
    assert!(
        inside
            .postamble(&RunTermination::Done, Duration::ZERO)
            .contains("\x1b]7770;state=idle\x07")
    );
}

#[test]
fn the_title_says_what_was_asked_and_never_says_a_dash() {
    assert_eq!(title(Some("explain the diff")), "explain the diff");
    // `-` is the spelling for "the prompt is on stdin".
    assert_eq!(title(Some("-")), "εgo");
    assert_eq!(title(None), "εgo");
    assert_eq!(title(Some("   ")), "εgo");
    // A prompt is often a paragraph; a tab is one line.
    assert_eq!(title(Some("first line\nsecond line")), "first line");
}

/// Cut in cells, like everything else that reaches a terminal. A title cut
/// by bytes puts a replacement character in the tab.
#[test]
fn a_long_title_is_cut_where_a_grapheme_ends() {
    let flag = "\u{1f1ee}\u{1f1f9}";
    let title = title(Some(&flag.repeat(40)));
    assert!(!title.contains('\u{fffd}'));
    assert_eq!(crate::render::line::cells(&title), 48);
}

#[test]
fn a_duration_is_written_for_a_person() {
    assert_eq!(duration(Duration::from_secs(9)), "9s");
    assert_eq!(duration(Duration::from_secs(60)), "1m 0s");
    assert_eq!(duration(Duration::from_secs(252)), "4m 12s");
}

#[test]
fn failed_skill_tool_preflight_keeps_owner_view_and_model_registry_unchanged() {
    let owner = tempdir().expect("skill preflight owner");
    let system = owner.path().join("system/skills");
    let user = owner.path().join("user/skills");
    let project = owner.path().join("project/skills");
    for root in [&system, &user, &project] {
        std::fs::create_dir_all(root).expect("skill root");
    }
    let explain = user.join("explain");
    std::fs::create_dir_all(&explain).expect("skill directory");
    std::fs::write(
        explain.join("SKILL.md"),
        "---\nname: explain\ndescription: Explain evidence.\n---\nExplain.\n",
    )
    .expect("skill file");
    let discovery = crate::skill::SkillDiscovery::new(system, user.clone(), project, false);
    let catalogue =
        crate::skill::SkillCatalogue::discover(discovery, &["help", "skills", "hooks", "quit"])
            .expect("skill catalogue");
    let before = catalogue.capture().expect("initial capture");
    let before_registry = tools(
        None,
        Some(before.core()),
        None,
        None,
        ego_core::permit::EnvPolicy::Inherited,
    )
    .expect("initial tool registry");
    std::fs::create_dir_all(user.join("new-valid")).expect("new skill directory");
    std::fs::write(
        user.join("new-valid/SKILL.md"),
        "---\nname: new-valid\ndescription: New evidence.\n---\nNew.\n",
    )
    .expect("new skill file");
    let prepared = catalogue.prepare_rescan().expect("prospective rescan");
    let prospective = prepared.capture();
    assert!(
        prospective
            .descriptors()
            .iter()
            .any(|spec| spec.name() == "new-valid")
    );
    let prospective_registry = tools(
        None,
        Some(prospective.core()),
        None,
        None,
        ego_core::permit::EnvPolicy::Inherited,
    )
    .expect("prospective capture supplies model tool preflight");
    assert!(
        prospective_registry
            .iter()
            .any(|tool| tool.descriptor().name == "skill")
    );

    let selected = vec!["not-a-tool".to_owned()];
    assert!(
        tools(
            Some(&selected),
            Some(prospective.core()),
            None,
            None,
            ego_core::permit::EnvPolicy::Inherited
        )
        .is_err()
    );
    let after = catalogue.capture().expect("capture after failed preflight");
    assert!(std::sync::Arc::ptr_eq(&before.core(), &after.core()));
    assert_eq!(before.snapshot_digest(), after.snapshot_digest());
    let after_registry = tools(
        None,
        Some(after.core()),
        None,
        None,
        ego_core::permit::EnvPolicy::Inherited,
    )
    .expect("unchanged registry");
    assert_eq!(
        before_registry
            .iter()
            .map(|tool| tool.descriptor().name)
            .collect::<Vec<_>>(),
        after_registry
            .iter()
            .map(|tool| tool.descriptor().name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn hooks_listing_renders_the_exact_runtime_arc_with_active_skill_scope() {
    let invocation = SkillInvocationId::new();
    let source = HookSource::skill_entries_with_authority(
        "skills/explain/SKILL.md",
        invocation,
        SkillSourceAuthority::User,
        [HookConfigEntry::new(
            HookLifecycle::PreToolUse,
            None,
            HookHandlerConfig::Prompt {
                prompt: "private handler payload".to_owned(),
                model: None,
                timeout_seconds: None,
            },
        )],
    );
    let catalogue = HookCatalogue::admit_typed(&[], &[source], Trust::Trusted)
        .expect("admitted hook catalogue");
    let store: &'static HookCaptureStore = Box::leak(Box::new(HookCaptureStore));
    let clock: &'static HookCaptureClock = Box::leak(Box::new(HookCaptureClock));
    let sink: &'static NullSink = Box::leak(Box::new(NullSink));
    let journal = SharedJournal::new(Journal::new(
        SessionId::new(),
        store,
        clock,
        sink,
        Producer::Main,
        SessionState::default(),
    ));
    let runtime = SessionHookState::new(
        ego_core::hook::HookLifecycleDispatcher::new(HookRunner::new(), catalogue),
        journal,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(30),
    )
    .bind(Arc::new(HookCaptureInvoker));
    let exact = runtime
        .hook_state()
        .catalogue_snapshot()
        .expect("runtime hook catalogue snapshot");
    let exact_again = runtime
        .hook_state()
        .catalogue_snapshot()
        .expect("runtime hook catalogue snapshot remains stable");
    assert!(Arc::ptr_eq(&exact, &exact_again));
    let rendered = crate::hooks::render(exact.as_ref(), false).expect("render hooks");
    assert!(rendered.contains(&invocation.to_string()));
    assert!(rendered.contains("skill:"));
    assert!(!rendered.contains("private handler payload"));
}

#[test]
fn resume_plan_parses_one_answer_and_leaves_classification_pure() {
    let model_tool = ItemId::new();
    let (model_state, model_answer, model_turn, model_request_event) =
        pending_resume_state(QuestionTarget::ModelTool {
            tool_call: model_tool,
        });
    let model_before = model_state.clone();
    let model_plan = plan_resume_answer(&model_state, std::slice::from_ref(&model_answer))
        .expect("one model answer is planned");
    assert_eq!(model_state, model_before);
    match model_plan {
        ResumePlan::Model {
            answer,
            continuation,
            request_event,
        } => {
            assert_eq!(answer, QuestionAnswer::single("yes"));
            assert_eq!(continuation.question, model_answer.id);
            assert_eq!(continuation.turn, model_turn);
            assert_eq!(continuation.tool_call, model_tool);
            assert_eq!(request_event, model_request_event);
        }
        ResumePlan::Foreign { .. } => panic!("model answer was misclassified as foreign"),
    }

    let foreign_tool = ItemId::new();
    let continuation_id = "mcp-generation-policy-restart";
    let (foreign_state, foreign_answer, _, _) =
        pending_resume_state(QuestionTarget::ForeignContinuation {
            tool_call: foreign_tool,
            continuation_id: continuation_id.to_owned(),
        });
    let foreign_before = foreign_state.clone();
    let foreign_plan = plan_resume_answer(&foreign_state, std::slice::from_ref(&foreign_answer))
        .expect("one foreign answer is planned");
    assert_eq!(foreign_state, foreign_before);
    match foreign_plan {
        ResumePlan::Foreign {
            question,
            continuation_id: planned_id,
            answer,
        } => {
            assert_eq!(question, foreign_answer.id);
            assert_eq!(planned_id, continuation_id);
            assert_eq!(answer, QuestionAnswer::single("yes"));
        }
        ResumePlan::Model { .. } => panic!("foreign answer was misclassified as model"),
    }
}

#[test]
fn frozen_rule_resolver_reuses_startup_layers_and_replaces_only_base_user_permissions() {
    let system = ConfigLayer::parse(
        ego_core::config::Source::System,
        "[permissions]\nallow = [\"write(src/**)\"]\ndeny = [\"read(src/**)\"]\n",
    )
    .expect("system layer");
    let user = ConfigLayer::parse(
        ego_core::config::Source::User,
        "[permissions]\nallow = [\"read(src/**)\"]\njudge = \"ollama/judge\"\n[profile.ci]\nmodel = \"ollama/ci\"\n",
    )
    .expect("user layer");
    let project = ConfigLayer::parse(
        ego_core::config::Source::Project,
        "[permissions]\nallow = [\"read(src/private/**)\"]\n",
    )
    .expect("project layer");
    let selection = GenerationPolicySelection::new(Mode::Plan, Ruleset::default());
    let resolver = FrozenRuleResolver::new(
        vec![system, user, project],
        Trust::Trusted,
        Some(("ci".to_owned(), ego_core::config::Source::Flags)),
        selection,
    );
    let before = ego_core::config::RuleSnapshot {
        target: ego_core::config::UserConfigTarget::Base,
        permissions_present: true,
        allow: vec![ego_core::permit::rule::Rule::load("read(src/**)").unwrap()],
        deny: Vec::new(),
    };
    let after = ego_core::config::RuleSnapshot {
        target: ego_core::config::UserConfigTarget::Base,
        permissions_present: true,
        allow: vec![ego_core::permit::rule::Rule::load("read(src/private/**)").unwrap()],
        deny: Vec::new(),
    };
    let result = resolver
        .resolve_rule_transaction(
            &before,
            &after,
            &[ego_core::config::RuleMutation {
                effect: ego_core::config::ManagedRuleEffect::Allow,
                rule: ego_core::permit::rule::Rule::load("read(src/private/**)").unwrap(),
            }],
        )
        .expect("frozen resolution");
    assert_eq!(result.next.mode, Mode::Plan);
    assert_eq!(result.next.ruleset.allow.len(), 1);
    assert_eq!(
        result.next.ruleset.allow[0].rule(),
        &ego_core::permit::rule::Rule::load("read(src/private/**)").unwrap()
    );
    assert_eq!(
        result.next.ruleset.allow[0].origins(),
        ego_core::config::OriginSet::just(ego_core::config::Source::Project)
    );
    assert_eq!(
        result
            .next
            .ruleset
            .deny
            .iter()
            .filter(|entry| entry.authority() != ego_core::config::Source::Default)
            .count(),
        1
    );
    assert_eq!(
        result.next.ruleset.deny[0].rule(),
        &ego_core::permit::rule::Rule::load("read(src/**)").unwrap()
    );
    assert_eq!(
        result.next.ruleset.deny[0].origins(),
        ego_core::config::OriginSet::just(ego_core::config::Source::System)
    );

    let unchanged = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let unknown_profile = FrozenRuleResolver::new(
        vec![
            ConfigLayer::parse(
                ego_core::config::Source::User,
                "[profile.ci]\nmodel = \"ollama/ci\"\n",
            )
            .expect("profile fixture"),
        ],
        Trust::Trusted,
        Some(("missing".to_owned(), ego_core::config::Source::Flags)),
        unchanged.clone(),
    );
    let error = unknown_profile
        .resolve_rule_transaction(&before, &after, &[])
        .expect_err("unknown selected profile must fail closed");
    assert!(matches!(
        error,
        crate::repl::policy_controls::PolicyControlError::Resolution { .. }
    ));
    assert_eq!(unchanged.snapshot().unwrap().mode, Mode::Default);
    assert!(unchanged.staged_snapshot().unwrap().is_none());
}

#[test]
fn captured_auto_preflight_has_closed_repeatable_outcomes_without_new_io() {
    let cases = [
        (CapturedAutoPreflight::DeferredDynamic, true),
        (CapturedAutoPreflight::Valid, true),
        (
            CapturedAutoPreflight::Refused {
                diagnostic: "captured judge cannot encode request".to_owned(),
            },
            false,
        ),
    ];
    for (state, accepted) in cases {
        let preflight = CliAutoPreflight::new(state);
        let first = preflight.validate_auto();
        let second = preflight.validate_auto();
        assert_eq!(first, second);
        assert_eq!(first.is_ok(), accepted);
        if !accepted {
            assert!(matches!(
                first,
                Err(crate::repl::policy_controls::PolicyControlError::JudgeUnavailable { .. })
            ));
        }
    }
}

#[derive(Default)]
struct PolicyTextOutput(std::sync::Mutex<Vec<String>>, AtomicUsize);

impl crate::output::Diagnostics for PolicyTextOutput {
    fn note(&self, _message: &str) {}
}

impl crate::output::TextOutput for PolicyTextOutput {
    fn write(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_owned());
    }

    fn line(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_owned());
    }

    fn finish(&self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

fn real_remember_preview() -> crate::rules::RememberPreview {
    crate::rules::RuleApplication::new(
        Arc::new(UnusedStore),
        ego_core::config::UserConfigTarget::Base,
    )
    .preview_remember(crate::rules::RememberRequest::new(
        ego_core::permit::PreparedCall::new(
            "read",
            ego_core::permit::Op::Read {
                path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
            },
        ),
        ego_core::config::ManagedRuleEffect::Allow,
        ego_core::permit::CanonPath::new("/workspace"),
    ))
    .unwrap()
}

#[test]
fn presented_remember_uses_the_atomic_owner_once_and_stages_the_written_rule() {
    use crate::repl::policy_controls::PermissionRuleApplication as _;

    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let owner = PolicyApplicationOwner::new(
        selection.clone(),
        Arc::new(ego_tools::config::OsUserConfigStore::new(
            Arc::new(ego_tools::store::OsIo),
            &path,
        )),
        Arc::new(CliAutoPreflight::new(CapturedAutoPreflight::Valid)),
        Arc::new(FrozenRuleResolver::new(
            Vec::new(),
            Trust::Trusted,
            None,
            selection.clone(),
        )),
    );
    let preview = real_remember_preview();

    let receipt = owner
        .remember_allow(preview.plan().clone())
        .expect("the displayed plan is one atomic transaction");

    // Catches the machine receipt reaching the operator: one sentence that
    // names the stored rule, never the tab-separated RESULT header.
    assert_eq!(
        receipt,
        "Remembered: you will not be asked again for read(/workspace/src/main.rs)."
    );
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("read(/workspace/src/main.rs)"));
    let staged = selection.staged_snapshot().unwrap().unwrap();
    assert!(
        staged
            .ruleset
            .allow
            .iter()
            .any(|rule| rule.rule().to_string() == "read(/workspace/src/main.rs)")
    );
}

fn real_rule_receipt() -> crate::repl::policy_controls::RuleMutationReceipt {
    let rule = ego_core::permit::rule::Rule::load("read(src/**)").unwrap();
    let mutation = ego_core::config::RuleMutation {
        effect: ego_core::config::ManagedRuleEffect::Allow,
        rule: rule.clone(),
    };
    crate::repl::policy_controls::RuleMutationReceipt::from_atomic_facts(
        ego_core::config::UserConfigMutationResult {
            target: ego_core::config::UserConfigTarget::Base,
            facts: ego_core::config::UserConfigMutationFacts::RuleTransaction {
                request: Box::new(ego_core::config::UserConfigMutationFacts::Batch {
                    changes: vec![mutation.clone()],
                }),
                before: ego_core::config::RuleSnapshot {
                    target: ego_core::config::UserConfigTarget::Base,
                    permissions_present: true,
                    allow: vec![],
                    deny: vec![],
                },
                after: ego_core::config::RuleSnapshot {
                    target: ego_core::config::UserConfigTarget::Base,
                    permissions_present: true,
                    allow: vec![rule],
                    deny: vec![],
                },
            },
            changed: true,
            publication: ego_core::config::UserConfigPublication::PublishedDurably,
        },
        vec![mutation],
        vec![ego_core::config::rule_management::RuleMutationComparison {
            stored_changed: true,
            effective_changed: true,
            authorities: vec![],
        }],
    )
    .unwrap()
}

#[test]
fn handle_policy_request_renders_list_explain_and_rule_without_confirmation() {
    let output = PolicyTextOutput::default();
    let mut applied = Vec::new();
    let rules = ego_core::config::Ruleset::from_layer(
        ego_core::config::PermCfg {
            judge: None,
            allow: vec![ego_core::permit::rule::Rule::load("read(src/**)").unwrap()],
            deny: vec![],
        },
        ego_core::config::Source::Project,
    );
    let list = ego_core::config::rule_management::RuleManagement::rows(&rules);
    let list_request =
        PolicyControlRequest::Permission(crate::repl::policy_controls::PermissionInput::List);
    let list_rows = crate::rules::render_list_rows(&list);
    assert_eq!(
        handle_policy_request(
            list_request.clone(),
            |request| {
                applied.push(request);
                Ok(PolicyControlOutcome::Permission(
                    crate::repl::policy_controls::PermissionOutcome::List(list.clone()),
                ))
            },
            &output,
            |_preview| unreachable!("confirmation is not requested for list"),
        )
        .expect("list request"),
        None
    );
    assert_eq!(*output.0.lock().unwrap(), list_rows);
    assert_eq!(applied, vec![list_request]);

    let call = ego_core::permit::PreparedCall::new(
        "read",
        ego_core::permit::Op::Read {
            path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
        },
    );
    let explanation = ego_core::config::rule_management::RuleManagement::explain(
        &call,
        &rules,
        std::path::Path::new("/workspace"),
    );
    let explain_request =
        PolicyControlRequest::Permission(crate::repl::policy_controls::PermissionInput::Explain {
            call: call.clone(),
            workspace: ego_core::permit::CanonPath::new("/workspace"),
        });
    let explain_rows = crate::rules::render_explanation_rows(&call, &explanation);
    let explain_output = PolicyTextOutput::default();
    assert_eq!(
        handle_policy_request(
            explain_request.clone(),
            |request| {
                applied.push(request);
                Ok(PolicyControlOutcome::Permission(
                    crate::repl::policy_controls::PermissionOutcome::Explain(explanation.clone()),
                ))
            },
            &explain_output,
            |_preview| unreachable!("confirmation is not requested for explain"),
        )
        .expect("explain request"),
        None
    );
    assert_eq!(*explain_output.0.lock().unwrap(), explain_rows);

    let receipt = real_rule_receipt();
    let rule_request = PolicyControlRequest::Rule(ego_core::config::RuleMutation {
        effect: ego_core::config::ManagedRuleEffect::Allow,
        rule: ego_core::permit::rule::Rule::load("read(src/**)").unwrap(),
    });
    let rule_rows = crate::rules::render_mutation_receipt_rows(
        &receipt,
        ego_core::config::ManagedRuleEffect::Allow,
    );
    let rule_output = PolicyTextOutput::default();
    assert_eq!(
        handle_policy_request(
            rule_request.clone(),
            |request| {
                applied.push(request);
                Ok(PolicyControlOutcome::Rule {
                    receipt: receipt.clone(),
                    staging: crate::repl::policy_controls::PolicyStageOutcome { changed: true },
                })
            },
            &rule_output,
            |_preview| unreachable!("confirmation is not requested for rule"),
        )
        .expect("rule request"),
        None
    );
    assert_eq!(*rule_output.0.lock().unwrap(), rule_rows);
    assert_eq!(applied.len(), 3);
}

#[test]
fn handle_policy_request_remember_confirmed_applies_preview_once_in_order() {
    let output = PolicyTextOutput::default();
    let mut applied = Vec::new();
    let request =
        PolicyControlRequest::Permission(crate::repl::policy_controls::PermissionInput::Remember(
            crate::rules::RememberRequest::new(
                ego_core::permit::PreparedCall::new(
                    "read",
                    ego_core::permit::Op::Read {
                        path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
                    },
                ),
                ego_core::config::ManagedRuleEffect::Allow,
                ego_core::permit::CanonPath::new("/workspace"),
            ),
        ));
    let preview = real_remember_preview();
    let result = handle_policy_request(
        request,
        |request| {
            applied.push(request);
            if applied.len() == 1 {
                Ok(PolicyControlOutcome::RememberPreview(preview.clone()))
            } else {
                Ok(PolicyControlOutcome::Rule {
                    receipt: real_rule_receipt(),
                    staging: crate::repl::policy_controls::PolicyStageOutcome { changed: true },
                })
            }
        },
        &output,
        |confirmed_preview| {
            assert_eq!(confirmed_preview, &preview);
            assert_eq!(
                *output.0.lock().unwrap(),
                crate::rules::render_remember_preview_rows(&preview)
            );
            assert_eq!(output.1.load(Ordering::SeqCst), 1);
            crate::repl::Confirmation::Confirmed
        },
    );
    assert!(result.is_ok());
    assert_eq!(output.1.load(Ordering::SeqCst), 1);
    assert_eq!(applied.len(), 2);
    assert!(matches!(
        &applied[0],
        PolicyControlRequest::Permission(crate::repl::policy_controls::PermissionInput::Remember(
            _
        ))
    ));
    assert!(matches!(
        &applied[1],
        PolicyControlRequest::Permission(
            crate::repl::policy_controls::PermissionInput::ConfirmRemember(observed)
        ) if observed == &preview
    ));
    let mut expected_output = crate::rules::render_remember_preview_rows(&preview);
    expected_output.extend(crate::rules::render_remember_result_rows(
        &ego_core::config::UserConfigTarget::Base,
        Some(&real_rule_receipt()),
    ));
    assert_eq!(*output.0.lock().unwrap(), expected_output);
}

#[test]
fn handle_policy_request_declined_remember_does_not_apply_again() {
    let output = PolicyTextOutput::default();
    let preview = real_remember_preview();
    let mut applies = 0;
    let result = handle_policy_request(
        PolicyControlRequest::Permission(crate::repl::policy_controls::PermissionInput::Remember(
            crate::rules::RememberRequest::new(
                ego_core::permit::PreparedCall::new(
                    "read",
                    ego_core::permit::Op::Read {
                        path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
                    },
                ),
                ego_core::config::ManagedRuleEffect::Allow,
                ego_core::permit::CanonPath::new("/workspace"),
            ),
        )),
        |_request| {
            applies += 1;
            Ok(PolicyControlOutcome::RememberPreview(preview.clone()))
        },
        &output,
        |_preview| crate::repl::Confirmation::Declined,
    );
    assert!(result.is_ok());
    assert_eq!(applies, 1);
    let mut expected_output = crate::rules::render_remember_preview_rows(&preview);
    expected_output.extend(crate::rules::render_remember_result_rows(
        &ego_core::config::UserConfigTarget::Base,
        None,
    ));
    assert_eq!(*output.0.lock().unwrap(), expected_output);
}

#[test]
fn handle_policy_request_propagates_confirmation_terminal_lines_without_second_apply() {
    let output = PolicyTextOutput::default();
    for propagated in [
        crate::repl::Line::Ended,
        crate::repl::Line::ExternalBreak("draft".to_owned()),
    ] {
        let preview = real_remember_preview();
        let mut applies = 0;
        let result = handle_policy_request(
            PolicyControlRequest::Permission(
                crate::repl::policy_controls::PermissionInput::Remember(
                    crate::rules::RememberRequest::new(
                        ego_core::permit::PreparedCall::new(
                            "read",
                            ego_core::permit::Op::Read {
                                path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
                            },
                        ),
                        ego_core::config::ManagedRuleEffect::Allow,
                        ego_core::permit::CanonPath::new("/workspace"),
                    ),
                ),
            ),
            |_request| {
                applies += 1;
                Ok(PolicyControlOutcome::RememberPreview(preview.clone()))
            },
            &output,
            |_preview| crate::repl::Confirmation::Propagate(propagated.clone()),
        );
        assert_eq!(result.unwrap(), Some(propagated));
        assert_eq!(applies, 1);
    }
}

#[test]
fn handle_policy_request_rejects_mismatched_request_and_outcome_without_rows() {
    let output = PolicyTextOutput::default();
    let result = handle_policy_request(
        PolicyControlRequest::Permission(crate::repl::policy_controls::PermissionInput::List),
        |_request| {
            Ok(PolicyControlOutcome::Mode {
                staging: crate::repl::policy_controls::PolicyStageOutcome { changed: false },
            })
        },
        &output,
        |_preview| unreachable!("mismatched outcome cannot request confirmation"),
    );
    assert!(result.is_err());
    assert!(output.0.lock().unwrap().is_empty());
}

/// A chosen model has to reach the generation, not just the configuration.
///
/// The seat writes into two places that spell the same fact: `slug`, which
/// the preflight builds its generation target from, and `effective.model`,
/// which the rest of the turn reads. When only the second moved, every
/// control test still passed — the host was answered with its pick and the
/// session reported it — while every generation ran the model the run
/// started with. Nothing on the control surface can see that, so the
/// assertion has to be on the value the preflight is handed.
#[test]
fn a_chosen_model_moves_the_name_the_generation_is_built_from() {
    use ego_core::config::{Mode, ModelRef, Trust};
    use ego_core::control::{ControlSelect, SessionControlState};

    let mut effective = ego_core::config::resolve(&[], Trust::Trusted, None).expect("empty config");
    let mut slug = "started-with".to_owned();
    let state = SessionControlState {
        model: ControlSelect {
            current: Some(ModelRef("started-with".to_owned())),
            chosen: Some(ModelRef("picked".to_owned())),
            available: Vec::new(),
        },
        effort: ControlSelect {
            current: Some(ThinkingLevel::High),
            chosen: Some(ThinkingLevel::High),
            available: Vec::new(),
        },
        mode: ControlSelect {
            current: Some(Mode::Plan),
            chosen: Some(Mode::Plan),
            available: Vec::new(),
        },
        sandbox: ControlSelect {
            current: Some(ego_core::permit::Confinement::Workspace),
            chosen: None,
            available: Vec::new(),
        },
    };

    apply_seat(&state, &mut effective, &mut slug);

    assert_eq!(slug, "picked", "the name the generation is built from");
    assert_eq!(effective.model, Some(ModelRef("picked".to_owned())));
    assert_eq!(effective.thinking, Some(ThinkingLevel::High));
    assert_eq!(effective.mode, Some(Mode::Plan));
}

/// What was not chosen is left exactly as configuration resolved it.
/// Inheriting a default is not a decision, and a run that overwrote the
/// resolved name with an unchosen one would claim a pick nobody made.
#[test]
fn an_unchosen_control_leaves_what_configuration_resolved_alone() {
    use ego_core::config::{Mode, ModelRef, Trust};
    use ego_core::control::{ControlSelect, SessionControlState};

    let mut effective = ego_core::config::resolve(&[], Trust::Trusted, None).expect("empty config");
    effective.model = Some(ego_core::config::ModelRef("configured".to_owned()));
    let mut slug = "configured".to_owned();
    let state = SessionControlState {
        model: ControlSelect {
            current: Some(ModelRef("configured".to_owned())),
            chosen: None,
            available: Vec::new(),
        },
        effort: ControlSelect {
            current: Some(ThinkingLevel::Off),
            chosen: None,
            available: Vec::new(),
        },
        mode: ControlSelect {
            current: Some(Mode::Default),
            chosen: None,
            available: Vec::new(),
        },
        sandbox: ControlSelect {
            current: Some(ego_core::permit::Confinement::Workspace),
            chosen: None,
            available: Vec::new(),
        },
    };

    apply_seat(&state, &mut effective, &mut slug);

    assert_eq!(slug, "configured");
    assert_eq!(
        effective.model,
        Some(ego_core::config::ModelRef("configured".to_owned()))
    );
}

/// A tool-less served session offers no tool, so nothing in it can ever ask a
/// host for a permission seat.
///
/// `ego acp --no-tools` is worth having only if that chain holds end to end: a
/// permission request originates from a tool call and nowhere else
/// (`crates/ego-acp/src/permission.rs`), so an empty catalogue is the same
/// guarantee stated one link earlier. The second half of the test is what
/// stops it passing for the wrong reason — a protocol that had quietly lost
/// its tools would satisfy the first assertion just as well.
#[test]
fn a_tool_less_protocol_session_offers_no_tool_to_ask_a_seat_for() {
    let tool_less = protocol_args(
        None,
        crate::cli::SessionShape {
            tools: Some(Vec::new()),
            ..crate::cli::SessionShape::default()
        },
    );
    let offered = composition::tools(
        tool_less.tools.as_ref(),
        None,
        None,
        None,
        ego_core::permit::EnvPolicy::Inherited,
    )
    .expect("an empty selection");
    assert!(
        offered.is_empty(),
        "a tool-less session offered {} tools",
        offered.len()
    );

    let ordinary = protocol_args(None, crate::cli::SessionShape::default());
    assert!(
        !composition::tools(
            ordinary.tools.as_ref(),
            None,
            None,
            None,
            ego_core::permit::EnvPolicy::Inherited
        )
        .expect("no selection at all")
        .is_empty(),
        "an unshaped session must still offer the builtins"
    );
}

/// The parent's posture is the ceiling, and `bare` is the only lever on it.
///
/// Two claims in one table. A child is subject to the rules its parent is
/// subject to, so a trusted parent spawns trusted children. And nothing a
/// repository declares can promote the run that reads it: there is no pair
/// here where an untrusted parent produces a trusted child.
#[test]
fn a_child_runs_at_its_parents_posture_and_bare_can_only_lower_it() {
    use super::port_adapters::child_posture;

    assert_eq!(child_posture(Trust::Trusted, false), Trust::Trusted);
    assert_eq!(
        child_posture(Trust::Trusted, true),
        Trust::Untrusted,
        "a role declared bare must be spent without its parent's posture"
    );
    assert_eq!(child_posture(Trust::Untrusted, false), Trust::Untrusted);
    assert_eq!(
        child_posture(Trust::Untrusted, true),
        Trust::Untrusted,
        "no declaration may promote a child above the parent that read it"
    );
}

/// Only the child that refused the posture refuses the capture of it.
#[test]
fn only_a_bare_child_of_a_trusted_parent_resolves_without_its_parents_capture() {
    use super::composition_root::spent_bare;

    assert!(spent_bare(Trust::Trusted, Trust::Untrusted));
    assert!(!spent_bare(Trust::Trusted, Trust::Trusted));
    assert!(
        !spent_bare(Trust::Untrusted, Trust::Untrusted),
        "an untrusted capture resolves the same layers either way, and keeping \
         it keeps the profile pinned with it"
    );
}

/// What a child's posture decides, on a workspace that has something to hide.
///
/// This is the shape a child with no capture resolves through — every ACP
/// subagent, because `for_acp` pins no configuration. The trusted case is the
/// defect this story exists for: the child of a session a person trusted was
/// reading none of the project its parent was reading.
#[test]
fn a_childs_posture_decides_whether_it_sees_the_project_at_all() {
    let owner = tempdir().expect("workspace owner");
    let root = std::fs::canonicalize(owner.path()).expect("canonical workspace");
    std::fs::create_dir_all(root.join(".ego")).expect("project directory");
    std::fs::write(
        root.join(".ego/config.toml"),
        "model = \"a-project-model\"\n",
    )
    .expect("project config");
    let args = interactive_args(&[]);

    for (posture, project, expected) in [
        (Trust::Trusted, true, ego_core::config::Posture::Ambient),
        (Trust::Untrusted, false, ego_core::config::Posture::Bare),
    ] {
        let settings = super::composition::rooted_settings(
            &args,
            root.clone(),
            |_| posture,
            &crate::output::Stderr,
        )
        .expect("a child with no capture resolves for itself");
        assert_eq!(settings.effective.posture, expected, "{posture:?}");
        assert_eq!(
            settings
                .profile_layers
                .as_ref()
                .expect("a resolving child carries its layers")
                .iter()
                .any(|layer| layer.source == ego_core::config::Source::Project),
            project,
            "a {posture:?} child saw the wrong side of the project configuration"
        );
    }
}

/// A subagent of a trusted terminal run prepares at its parent's posture.
///
/// A child does not re-ask anything: it runs at the posture its parent's
/// answer already settled, frozen at capture. Reporting `Untrusted` here
/// while resolving trusted layers was not a conservative choice, it was two
/// answers to one question — and the half that read the posture rather than
/// the layers, the hook catalogue, admitted nothing. Every rule the parent
/// runs under, the child runs under.
#[test]
fn a_frozen_child_of_a_trusted_parent_prepares_the_layers_its_parent_admitted() {
    let owner = tempdir().expect("workspace owner");
    let root = std::fs::canonicalize(owner.path()).expect("canonical workspace");
    std::fs::create_dir_all(root.join(".ego")).expect("project directory");
    std::fs::write(
        root.join(".ego/config.toml"),
        "model = \"a-project-model\"\n",
    )
    .expect("project config");
    let args = interactive_args(&[]);
    let frozen = crate::config::FrozenRunConfig::capture(&root, &args, Trust::Trusted)
        .expect("a parent that was trusted captures its layers");

    let settings = super::composition::frozen_rooted_settings(
        &args,
        root.clone(),
        &frozen,
        &crate::output::Stderr,
    )
    .expect("a child of a trusted parent prepares");

    assert_eq!(
        settings.effective.posture,
        ego_core::config::Posture::Ambient,
        "the child dropped the project layers its parent's answer admitted"
    );
    assert_eq!(
        settings.trust,
        Trust::Trusted,
        "the child reported a posture its own layers contradict"
    );
    let layers = settings
        .profile_layers
        .as_ref()
        .expect("a frozen child carries the layers it resolved");
    assert!(
        layers
            .iter()
            .any(|layer| layer.source == ego_core::config::Source::Project),
        "the hook catalogue is admitted from these layers, and the project layer is missing"
    );
}

fn permission_started(tool: &str) -> PublicEvent {
    PublicEvent::ItemStarted {
        turn: TurnId::new(),
        item: ItemId::new(),
        kind: ego_core::public::ItemKind::Permission,
        name: Some(tool.to_string()),
        text: Some("write outside the workspace".to_string()),
    }
}

fn permission_answered(allowed: bool) -> PublicEvent {
    PublicEvent::ItemCompleted {
        turn: TurnId::new(),
        item: ItemId::new(),
        kind: ego_core::public::ItemKind::Permission,
        ok: allowed,
        text: None,
    }
}

fn prompt_submitted(text: &str) -> PublicEvent {
    PublicEvent::ItemCompleted {
        turn: TurnId::new(),
        item: ItemId::new(),
        kind: ego_core::public::ItemKind::UserMessage,
        ok: true,
        text: Some(text.to_string()),
    }
}

/// catches: maintainer being notified, and the tab badged as waiting, for a request
/// that a judge or a stored rule settles by itself. The request event is raised
/// before that decision, so it must announce nothing; the terminal owner
/// announces the wait when it actually puts the question (`Osc::awaiting`).
#[test]
fn an_auto_decidable_permission_request_announces_nothing() {
    let out = Stdout::new(CancelToken::new());
    let inside = Announce::new(&out, capabilities(OscProfile::Tuic), false);
    let state = SessionState::default();

    assert_eq!(inside.signal(&permission_started("bash"), &state), "");
    // A judge's verdict completes the same item; with no wait announced there
    // is nothing to take back, so no stray busy either.
    assert_eq!(inside.signal(&permission_answered(true), &state), "");
}

/// catches: a TUIC tab with no intent or title for a turn, and a first turn
/// whose title is empty because the session has none yet.
#[test]
fn a_turn_starts_with_the_title_the_intent_and_busy() {
    let out = Stdout::new(CancelToken::new());
    let inside = Announce::new(&out, capabilities(OscProfile::Tuic), false);

    assert_eq!(
        inside.signal(
            &prompt_submitted("explain the diff"),
            &SessionState::default()
        ),
        "\x1b]0;ɛ । explain the diff\x07\
         \x1b]7770;intent=explain the diff (εgo)\x07\
         \x1b]7770;state=busy\x07"
    );

    // A session that already has a title shows it, not the latest prompt.
    let mut titled = SessionState::default();
    titled.title = Some("Refactor the store".to_string());
    assert!(
        inside
            .signal(&prompt_submitted("and now the tests"), &titled)
            .starts_with("\x1b]0;ɛ । Refactor the store\x07"),
    );
}

/// catches: TUIC reading the tail of the prompt as the intent's title. It
/// splits the payload at the last `(`, so a prompt ending in a call must be
/// followed by the fixed title.
#[test]
fn a_prompt_ending_in_a_parenthesis_keeps_its_tail_out_of_the_title_field() {
    let out = Stdout::new(CancelToken::new());
    let inside = Announce::new(&out, capabilities(OscProfile::Tuic), false);
    assert!(
        inside
            .signal(
                &prompt_submitted("why does foo(bar)"),
                &SessionState::default()
            )
            .contains("\x1b]7770;intent=why does foo(bar) (εgo)\x07")
    );
}

/// catches: a pane that stays marked busy after the turn ends, now that the
/// seat announces busy per turn.
#[test]
fn a_finished_turn_returns_the_pane_to_idle() {
    let out = Stdout::new(CancelToken::new());
    let inside = Announce::new(&out, capabilities(OscProfile::Tuic), false);
    let done = PublicEvent::TurnCompleted {
        turn: TurnId::new(),
        ok: true,
        exit_reason: ego_core::event::domain::ExitReason::Completed,
    };
    assert_eq!(
        inside.signal(&done, &SessionState::default()),
        "\x1b]7770;state=idle\x07"
    );
}

/// catches: an escape sequence reaching a pipe or an unknown terminal through
/// the per-event path, which the one-shot gate tests do not cover.
#[test]
fn a_plain_host_is_told_nothing_per_event() {
    let out = Stdout::new(CancelToken::new());
    let plain = Announce::new(&out, Capabilities::plain(), false);
    let state = SessionState::default();
    for event in [
        permission_started("bash"),
        permission_answered(true),
        prompt_submitted("hello"),
    ] {
        assert_eq!(plain.signal(&event, &state), "");
    }
}

// Catches: the process composition admits exec_paths outside a system root
// ceiling even though profile roots are checked.
#[tokio::test]
async fn system_root_ceiling_is_checked_before_run_effects() {
    let tree = tempdir().unwrap();
    let workspace = tree.path().canonicalize().unwrap();
    let extra = workspace.parent().unwrap();
    let session_root = workspace.join("sessions");
    let layers = vec![
        ConfigLayer::parse(Source::System, "roots=[]").unwrap(),
        ConfigLayer::parse(Source::User, &format!("exec_paths=['{}']", extra.display())).unwrap(),
    ];
    let store = Arc::new(JsonlStore::new(
        session_root.clone(),
        workspace.clone(),
        Box::new(OsIo),
    ));
    let mut executor = CliRunExecutor::for_acp(
        None,
        SessionShape::default(),
        store,
        Arc::new(EvidenceAdmissionService::new()),
    );
    executor.frozen = Some(Arc::new(config::FrozenRunConfig::from_layers(
        layers,
        &executor.args,
        Trust::Untrusted,
    )));
    let invocation = RunInvocationId::new();
    let request = RunPreparation {
        run_id: RunId::new(SessionId::new(), invocation.uuid()),
        invocation,
        root: ego_core::run::RunRoot::new(workspace).unwrap(),
        selections: ego_core::run::RunSelections::new(None, None, None, None, None),
        continuation: false,
        origin: RunOrigin::TopLevel,
    };
    let error = match executor.prepare(&request).await {
        Err(error) => error,
        Ok(_) => panic!("exec_paths outside system roots must be refused"),
    };
    assert!(error.to_string().contains("system roots ceiling"));
    assert!(
        !session_root.exists(),
        "refusal must precede session creation"
    );
}

// Catches: /yolo bypasses the system maximum when staging the next generation.
#[test]
fn repl_mode_selection_cannot_raise_system_maximum() {
    use ego_core::config::{ConfigLayer, Source, resolve};

    let layers = vec![ConfigLayer::parse(Source::System, "max_mode = 'default'").unwrap()];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    let selection = GenerationPolicySelection::new(
        effective.mode.unwrap_or(Mode::Default),
        effective.permissions,
    );
    let owner = PolicyApplicationOwner::new(
        selection.clone(),
        Arc::new(UnusedStore),
        Arc::new(CliAutoPreflight::new(CapturedAutoPreflight::Valid)),
        Arc::new(FrozenRuleResolver::new(
            layers,
            Trust::Trusted,
            None,
            selection.clone(),
        )),
    );

    let result = owner.apply(PolicyControlRequest::SetMode(Mode::Yolo));
    assert!(
        result.is_err(),
        "REPL accepted yolo above system max_mode=default"
    );
    assert_eq!(selection.capture().unwrap().mode, Mode::Default);
}

// Catches: auto startup applies Off to fallback models that reject it.
#[test]
fn auto_preflight_accepts_fallback_and_explicit_judges_without_off_effort() {
    let confinement =
        RunConfinement::new(Confinement::Off, false, CanonPath::new("/workspace")).unwrap();
    for (main, judge) in [
        ("openai-codex/gpt-6-sol", None),
        ("anthropic/claude-sonnet-5-5", None),
        ("openai-codex/gpt-6-sol", Some("openai-codex/gpt-6-sol")),
        ("anthropic/claude-sonnet-5-5", Some("none")),
    ] {
        let mut effective = ego_core::config::resolve(&[], Trust::Trusted, None).unwrap();
        effective.model = Some(ModelRef(main.to_owned()));
        effective.mode = Some(Mode::Auto);
        effective.judge = judge.map(|value| ModelRef(value.to_owned()));
        let captured = CliAutoPreflight::capture(&effective, &confinement);
        assert_eq!(
            captured.state,
            CapturedAutoPreflight::Valid,
            "{main}, {judge:?}"
        );
        assert!(captured.validate_startup().is_ok());
    }
}

// Catches: judge effort selection falls back to unsupported Off when Low is absent.
#[test]
fn judge_effort_uses_the_lowest_supported_level_and_refuses_unknown_facts() {
    use ego_core::generator::capabilities::ReasoningEffortSupport;
    let profile = model::supplied_request_profile(&ModelRef("openai-codex/gpt-6-sol".into()))
        .unwrap()
        .unwrap();
    let mut capabilities = profile.capabilities().clone();
    for (support, expected) in [
        (ReasoningEffortSupport::Any, Some(ThinkingLevel::Off)),
        (
            ReasoningEffortSupport::Only([ThinkingLevel::Medium, ThinkingLevel::High].into()),
            Some(ThinkingLevel::Medium),
        ),
        (
            ReasoningEffortSupport::Only([ThinkingLevel::Off].into()),
            Some(ThinkingLevel::Off),
        ),
        (ReasoningEffortSupport::Only(BTreeSet::new()), None),
        (ReasoningEffortSupport::Unknown, None),
    ] {
        capabilities.reasoning_efforts = support;
        assert_eq!(model::judge_thinking(&capabilities).ok(), expected);
    }
}

/// Catches: a real Adaptive setup failure reaches the transcript but is omitted
/// from the CLI refusal, leaving the operator with no setup cause to fix.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn adaptive_setup_failure_is_named_in_the_user_refusal() {
    let owner = tempdir().expect("adaptive refusal fixture");
    let blocker = owner.path().join("not-a-directory");
    std::fs::write(&blocker, "fixture").expect("non-directory probe parent");
    let confinement = RunConfinement::new(
        Confinement::Adaptive,
        false,
        CanonPath::new(blocker.join("workspace")),
    )
    .expect("absolute workspace")
    .with_adaptive_perimeter(ego_core::permit::AdaptivePerimeter {
        home: CanonPath::new(&blocker),
        writable: vec![],
        credentials: vec![],
        controls: vec![],
    });
    let evidence = SessionEvidenceStore::open(owner.path(), SessionId::new(), 1024 * 1024)
        .expect("refusal evidence store");
    let cancel = CancelToken::new();
    let config = ego_tools::exec::ExecRunConfig::new(
        confinement.clone(),
        evidence.clone(),
        cancel.clone(),
        ego_tools::exec::ExecLimits::default(),
        ego_tools::exec::HttpCallTimeout::default(),
        None,
        Arc::new(ego_tools::http::SystemResolver::new()),
    );
    let missing = match RunEffectPorts::for_run(config).await {
        Err(ProcessError::MissingCapabilities(missing)) => missing,
        Err(other) => panic!("expected measured Adaptive refusal, got {other}"),
        Ok(_) => panic!("non-directory fixture parent admitted Adaptive"),
    };
    let cause = missing
        .host_control_failure()
        .expect("real measurement recorded its host cause")
        .to_owned();
    assert!(
        cause.contains("adaptive.setup") && cause.contains("os error"),
        "{cause}"
    );
    use ego_core::store::evidence::{EvidenceRead, EvidenceStore};
    let reference = missing
        .probe_evidence()
        .expect("persisted transcript")
        .clone();
    let request = EvidenceRead::new(reference.clone(), 0).unwrap();
    let page = EvidenceStore::read_page(&evidence, &request)
        .await
        .expect("read measured evidence");
    let transcript = String::from_utf8(page.bytes).expect("JSON Lines transcript");
    let setup = transcript
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|record| record["suite"] == "adaptive.setup")
        .expect("persisted setup failure");
    assert_eq!(
        cause,
        format!("adaptive.setup: {}", setup["outcome"].as_str().unwrap())
    );
    assert!(
        setup["outcome"]
            .as_str()
            .unwrap()
            .contains("create outside fixture")
    );
    let RunTermination::UsageError { message } = process_error_termination(
        ProcessError::MissingCapabilities(missing),
        &confinement,
        &cancel,
    ) else {
        panic!("unsupported Adaptive is a usage refusal");
    };
    assert!(message.contains(&reference.id.to_string()), "{message}");
    assert!(
        !message.contains("firewall") && !message.contains("offline probe"),
        "{message}"
    );
    assert!(
        message.contains(&cause),
        "Adaptive refusal omitted the measured setup cause {cause:?}: {message}"
    );
}

/// Catches: Adaptive setup/control failures disappear or gain firewall advice
/// when the missing guarantees include (or exclude) offline confinement.
#[test]
fn adaptive_host_failures_are_named_without_offline_listener_advice() {
    for (offline, failure) in [
        (
            false,
            "adaptive.setup: create outside fixture: Not a directory (os error 20)",
        ),
        (
            false,
            "adaptive.write-outside.control: remove control fixture: Permission denied (os error 13)",
        ),
        (
            true,
            "adaptive.setup: create workspace fixture: Not a directory (os error 20)",
        ),
    ] {
        let confinement =
            RunConfinement::new(Confinement::Adaptive, offline, CanonPath::new("/workspace"))
                .unwrap();
        let missing = confinement
            .requirements()
            .check(ExecCaps::empty())
            .expect_err("unmeasured Adaptive is refused")
            .with_host_control_failure(Some(failure.to_owned()));
        let RunTermination::UsageError { message } = process_error_termination(
            ProcessError::MissingCapabilities(missing),
            &confinement,
            &CancelToken::new(),
        ) else {
            panic!("unmet Adaptive is a usage refusal");
        };
        assert!(message.contains(failure), "{message}");
        assert!(
            message.contains("Fix the recorded probe cause"),
            "{message}"
        );
        assert!(
            !message.contains("firewall") && !message.contains("offline probe"),
            "{message}"
        );
    }
}

/// catches: recovery remaining invisible in TUIC and surviving success or cancellation.
#[test]
fn recovery_waiting_and_settlement_emit_exact_osc() {
    let out = Stdout::new(CancelToken::new());
    let inside = Announce::new(&out, capabilities(OscProfile::Tuic), false);
    let state = SessionState::default();
    let waiting = PublicEvent::ProviderRetry {
        turn: TurnId::new(),
        attempt: 3,
        limit: 6,
        delay_ms: 0,
        kind: ego_core::generator::types::ErrorKind::Transient,
        cause: "connection failed".into(),
        recovery_remaining_ms: Some(60_000),
    };
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let bytes = inside.signal(&waiting, &state);
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let json = bytes
        .strip_prefix("\x1b]7770;ego-recovery=")
        .expect("waiting OSC")
        .strip_suffix('\x07')
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    let next = value["next_attempt_ms"].as_u64().unwrap();
    assert!((before..=after).contains(&u128::from(next)));
    assert_eq!(
        bytes,
        format!(
            "\x1b]7770;ego-recovery={{\"phase\":\"waiting\",\"next_attempt_ms\":{next},\"attempt\":3}}\x07"
        )
    );
    for exit_reason in [
        ego_core::event::domain::ExitReason::Completed,
        ego_core::event::domain::ExitReason::Cancelled(ego_core::event::domain::CancelCause::User),
        ego_core::event::domain::ExitReason::Provider,
    ] {
        // A distinct live recovery before each terminal outcome.
        inside.signal(&waiting, &state);
        assert_eq!(
            inside.signal(
                &PublicEvent::TurnCompleted {
                    turn: TurnId::new(),
                    ok: false,
                    exit_reason
                },
                &state
            ),
            "\x1b]7770;ego-recovery=clear\x07\x1b]7770;state=idle\x07"
        );
    }
}

/// catches: a retry countdown guessing dispatch, or recovery sticking after output.
#[test]
fn recovery_retrying_output_and_non_tty_boundaries() {
    let out = Stdout::new(CancelToken::new());
    let state = SessionState::default();
    let waiting = PublicEvent::ProviderRetry {
        turn: TurnId::new(),
        attempt: u32::MAX,
        limit: 6,
        delay_ms: u64::MAX,
        kind: ego_core::generator::types::ErrorKind::Transient,
        cause: "untrusted \x1b\x07".into(),
        recovery_remaining_ms: Some(60_000),
    };
    let dispatch = PublicEvent::ProviderRetrying {
        turn: TurnId::new(),
        attempt: u32::MAX,
    };
    let output = PublicEvent::ItemDelta {
        turn: TurnId::new(),
        item: ItemId::new(),
        kind: ego_core::public::ItemKind::AgentMessage,
        text: "back online".into(),
    };
    let inside = Announce::new(&out, capabilities(OscProfile::Tuic), true);
    let bytes = inside.signal(&waiting, &state);
    assert_eq!(
        bytes,
        "\x1b]7770;ego-recovery={\"phase\":\"waiting\",\"next_attempt_ms\":8640000000000000,\"attempt\":4294967295}\x07"
    );
    assert!(bytes.len() < 256);
    assert_eq!(
        inside.signal(&dispatch, &state),
        "\x1b]7770;ego-recovery={\"phase\":\"retrying\",\"next_attempt_ms\":null,\"attempt\":4294967295}\x07"
    );
    assert_eq!(
        inside.signal(&output, &state),
        "\x1b]7770;ego-recovery=clear\x07"
    );
    assert_eq!(inside.signal(&output, &state), "");
    for caps in [Capabilities::plain(), capabilities(OscProfile::Standard)] {
        let outside = Announce::new(&out, caps, false);
        for event in [&waiting, &dispatch, &output] {
            assert_eq!(outside.signal(event, &state), "");
        }
    }
}

/// catches: recovery OSC held in the interactive row buffer until provider output.
#[test]
fn recovery_interactive_signals_do_not_wait_for_a_text_row() {
    let (sender, received) = crossbeam::channel::unbounded();
    let output = crate::output::InteractiveOutput::new(crate::repl::SurfaceOutput::new(
        sender,
        CancelToken::new(),
    ));
    let announce = Announce::new(&output, capabilities(OscProfile::Tuic), false);
    for sequence in [
        "\x1b]7770;ego-recovery={\"phase\":\"waiting\",\"next_attempt_ms\":1000,\"attempt\":1}\x07",
        "\x1b]7770;ego-recovery={\"phase\":\"retrying\",\"next_attempt_ms\":null,\"attempt\":1}\x07",
        "\x1b]7770;ego-recovery=clear\x07",
    ] {
        announce.emit(sequence.to_owned());
        assert_eq!(
            received
                .try_recv()
                .expect("signal must reach terminal before the next provider event"),
            sequence
        );
        // A finish barrier is allowed, but never a synthetic transcript newline.
        for row in received.try_iter() {
            assert!(row.is_empty());
        }
    }
}

#[test]
fn resumed_terminal_round_prompts_remaining_members_and_fresh_retry_consent() {
    // Catches: an answerless terminal restart has no plan for its durable foreign round.
    use ego_core::question::{ElicitationKind, ElicitationMember, ElicitationRound};
    let dir = tempdir().unwrap();
    let store = ego_tools::store::JsonlStore::new(dir.path().join("sessions"), dir.path().to_path_buf(), Box::new(ego_tools::store::OsIo));
    let session = SessionId::new();
    let question = ItemId::new();
    let turn = TurnId::new();
    let request_event = EventId::new();
    let round = ElicitationRound {
        expires_at_ms: None,
        requests: std::collections::BTreeMap::from([
            ("a".into(), ElicitationMember { prompt: "Already completed".into(), kind: ElicitationKind::Url { url: "https://example.test/done".into(), elicitation_id: "done".into() } }),
            ("b".into(), ElicitationMember { prompt: "Remaining action".into(), kind: ElicitationKind::Url { url: "https://example.test/remaining".into(), elicitation_id: "remaining".into() } }),
        ]),
        completed: Default::default(),
    };
    let request = QuestionRequest::new("MCP input", QuestionMode::Round(round), vec![], false);
    let mut sequence = Seq::default();
    for (seq, kind) in [
        EventKind::QuestionRequested { request, target: QuestionTarget::ForeignContinuation { tool_call: ItemId::new(), continuation_id: "mcp-reopened".into() } },
        EventKind::QuestionMemberSettled { key: "a".into(), answer: serde_json::json!({"completed":true}) },
    ].into_iter().enumerate() {
        store.append(&session, &DomainEvent { env: Envelope { seq: sequence.next(), id: if seq == 0 { request_event } else { EventId::new() }, turn, item: Some(question), parent: None, ts: Timestamp::from_millis(1000), producer: Producer::Main }, kind }).unwrap();
    }
    let entries = futures::executor::block_on(async {
        use futures::StreamExt;
        SessionStore::read(&store, &session).unwrap().collect::<Vec<_>>().await
    });
    let state = SessionState::replay(entries.into_iter().filter_map(|entry| match entry.unwrap() {
        ego_core::store::session_store::SessionEntry::Event(event) => Some(event),
        _ => None,
    }));
    let plan = plan_resume_answer(&state, &[]).expect("terminal resume must present the pending foreign round without a hand-constructed answer");
    assert!(matches!(plan, ResumePlan::Foreign { question: id, .. } if id == question));
    let pending = state.pending_question().unwrap();
    crate::repl::test_resumed_round_terminal(pending.request().clone());
}
