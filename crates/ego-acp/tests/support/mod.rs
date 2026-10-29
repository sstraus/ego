#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::{ConnectTo, Lines};
use agent_client_protocol_schema::ProtocolVersion;
use ego_acp::serve::{
    AcpNotices, AcpRunBindingsBuilder, ExecutionSpawner, McpSessionAdmissionError,
    McpSessionAdmitter, RootResolver, SessionMcpAdmission,
};
use ego_core::compact::{
    CompactionCommand, CompactionFuture, CompactionReceipt, CompactionTransitionError,
    CompactionTransitionRequest,
};
use ego_core::config::ModelRef;
use ego_core::event::domain::{
    DomainEvent, Envelope, EventKind, GenerationRole, OccupancyProvenance,
};
use ego_core::event::id::{EventId, GenerationId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{
    EvidenceError, EvidenceLogSource, EvidenceRecord, EvidenceWrite, GenerationAdmissionEvidence,
    GenerationTarget, ManifestEvidenceError, PricingEvidence, RunInvocationId,
};
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::public::PublicEvent;
use ego_core::run::{
    IdSource, RequestId, RunAppend, RunAttachment, RunCommand, RunController, RunExecutionBindings,
    RunExecutor, RunExecutorError, RunFuture, RunHandle, RunPreparation, RunPrepared, RunPrompt,
    RunRetention, RunRevision, RunRoot, RunSelections, RunSettlement, RunStatus, RunTransaction,
    StartRequest,
};
use ego_core::session::{
    ReplaySource, ReplaySourceError, SessionBlobSource, SessionLineage, TargetPublication,
    TargetSeed,
};
use ego_core::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, HoldSignalFuture, InactiveSessionLease,
    LeasedLifecycleAppend, LifecycleAppendError, LifecyclePublication, SessionActivity,
    SessionLifecycle, SessionLifecycleStore, SessionRunLifecycle,
};
use ego_core::sink::EventSink;
use ego_core::state::SessionState;
use ego_core::store::blob::{BlobError, BlobRef};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use futures::channel::mpsc;
use futures::future::{self, BoxFuture};
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tokio::sync::Notify;

#[path = "../../../ego-core/tests/support/session_writer.rs"]
pub(super) mod session_writer;

pub const SESSION_PAGE_SIZE: usize = 50;

pub struct SeededReplay {
    pub session_id: String,
    pub expected_replay_updates: Vec<serde_json::Value>,
}

pub struct DurableAcpFixture {
    pub store: Arc<Store>,
    ids: Arc<TestIds>,
    effects: Arc<EffectCounts>,
    pub executor: Arc<Executor>,
    spawner: Arc<TestSpawner>,
}

#[derive(Default)]
struct EffectCounts {
    roots: AtomicUsize,
    mcp: AtomicUsize,
    lifecycle: AtomicUsize,
}

struct InactiveRuns;

impl SessionRunLifecycle for InactiveRuns {
    fn activity(&self, _session: SessionId) -> ActivityFuture<'_> {
        Box::pin(async { Ok(SessionActivity::Inactive) })
    }

    fn cancel_and_settle(&self, session: SessionId) -> CloseFuture<'_> {
        Box::pin(async move {
            Ok(ClosedSession {
                id: session,
                cancelled: false,
            })
        })
    }

    /// `false`, matching the name: `InactiveRuns` reports no live run, so a
    /// pause has no turn to reach and the caller writes the boundary itself.
    fn hold(&self, _session: SessionId, _request: RequestId) -> HoldSignalFuture<'_> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

impl DurableAcpFixture {
    pub fn set_current_goal(&self, session: &str, objective: &str) {
        let session: SessionId = session.parse().unwrap();
        let lease = self.store.acquire_inactive_lease(session).unwrap();
        ego_core::session_lifecycle::SessionMetadata::new(
            Arc::clone(&self.store),
            Arc::new(TestClock),
        )
        .set_goal(
            lease.as_ref(),
            self.store.semantic_seq(&session),
            objective.to_owned(),
            ego_core::goal::GoalOwner::Operator,
            ego_core::ceiling::Ceilings::default(),
        )
        .unwrap();
    }
    /// Two successful turns, produced with the same domain journal facts as a run.
    pub fn seed_settled_messages(&self, root: &str) -> (String, [ItemId; 2]) {
        let lifecycle = SessionLifecycle::new(
            Arc::clone(&self.store),
            Arc::new(InactiveRuns),
            Arc::new(TestClock),
        );
        let session = lifecycle.create(PathBuf::from(root)).unwrap().id;
        let items = [
            self.append_settled_message(&session.to_string(), "turn 1"),
            self.append_settled_message(&session.to_string(), "turn 2"),
        ];
        (session.to_string(), items)
    }

    pub fn append_settled_message(&self, session: &str, text: &str) -> ItemId {
        let session: SessionId = session.parse().unwrap();
        let item = ItemId::new();
        let turn = TurnId::new();
        let mut seq = self.store.semantic_seq(&session);
        for (item, kind) in [
            (
                Some(item),
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: text.to_owned(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            ),
            (
                None,
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ego_core::event::domain::ExitReason::Completed,
                },
            ),
        ] {
            seq.next();
            self.store
                .append(
                    &session,
                    &DomainEvent {
                        env: Envelope {
                            seq,
                            id: EventId::new(),
                            turn,
                            item,
                            parent: None,
                            ts: Timestamp::EPOCH,
                            producer: Producer::Main,
                        },
                        kind,
                    },
                )
                .unwrap();
        }
        item
    }

    pub fn seed_compaction_child(&self, source: &str) -> String {
        let source: SessionId = source.parse().unwrap();
        let target = SessionId::new();
        let seed =
            TargetSeed::compaction(source, Seq::ZERO, 1, "summary".to_owned(), Timestamp::EPOCH);
        assert!(self.store.publish_fork(target, &seed).switch_target());
        target.to_string()
    }
    pub fn new() -> Self {
        let effects = Arc::new(EffectCounts::default());
        Self {
            store: Arc::new(Store::with_effects(Arc::clone(&effects))),
            ids: Arc::new(TestIds::default()),
            effects,
            executor: Arc::new(Executor::default()),
            spawner: Arc::new(TestSpawner::default()),
        }
    }

    pub fn router(&self) -> agent_client_protocol::AgentProtocolRouter {
        let clock = Arc::new(TestClock);
        let controller = Arc::new(RunController::new(
            Arc::clone(&self.store),
            Arc::clone(&self.executor),
            Arc::clone(&clock),
            Arc::clone(&self.ids),
            RunRetention::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
        ));
        let runs: Arc<dyn SessionRunLifecycle> = controller.clone();
        let lifecycle = Arc::new(SessionLifecycle::new(Arc::clone(&self.store), runs, clock));
        ego_acp::serve::durable_agent(
            controller,
            lifecycle,
            Arc::new(TestRoots::with_effects(Arc::clone(&self.effects))),
            self.spawner.clone(),
            self.ids.clone(),
            Arc::new(AcpRunBindingsBuilder::default()),
            Arc::new(RejectingMcpAdmitter::with_effects(Arc::clone(
                &self.effects,
            ))),
        )
    }

    pub fn router_with_confinement(
        &self,
        confinement: Option<Arc<dyn ego_acp::serve::SessionConfinementAdmission>>,
    ) -> agent_client_protocol::AgentProtocolRouter {
        self.router_with_confinement_and_controls(None, confinement)
    }

    pub fn router_with_confinement_and_controls(
        &self,
        controls: Option<Arc<dyn ego_core::control::SessionControls>>,
        confinement: Option<Arc<dyn ego_acp::serve::SessionConfinementAdmission>>,
    ) -> agent_client_protocol::AgentProtocolRouter {
        self.router_with_trust(controls, confinement, None)
    }

    pub fn router_with_trust(
        &self,
        controls: Option<Arc<dyn ego_core::control::SessionControls>>,
        confinement: Option<Arc<dyn ego_acp::serve::SessionConfinementAdmission>>,
        trust: Option<Arc<dyn ego_acp::serve::SessionTrustAdmission>>,
    ) -> agent_client_protocol::AgentProtocolRouter {
        self.router_with_trust_and_notices(controls, confinement, trust, AcpNotices::default())
    }

    pub fn router_with_notices(
        &self,
        notices: AcpNotices,
    ) -> agent_client_protocol::AgentProtocolRouter {
        self.router_with_trust_and_notices(None, None, None, notices)
    }

    fn router_with_trust_and_notices(
        &self,
        controls: Option<Arc<dyn ego_core::control::SessionControls>>,
        confinement: Option<Arc<dyn ego_acp::serve::SessionConfinementAdmission>>,
        trust: Option<Arc<dyn ego_acp::serve::SessionTrustAdmission>>,
        notices: AcpNotices,
    ) -> agent_client_protocol::AgentProtocolRouter {
        let admitter = Arc::new(RejectingMcpAdmitter::with_effects(Arc::clone(
            &self.effects,
        )));
        self.router_with_admitter(admitter, controls, confinement, trust, notices)
    }

    /// The durable router with a caller-supplied MCP admitter in place of the
    /// rejecting default.
    #[allow(dead_code)]
    pub fn router_with_mcp_admitter(
        &self,
        admitter: Arc<dyn McpSessionAdmitter>,
        controls: Option<Arc<dyn ego_core::control::SessionControls>>,
        confinement: Option<Arc<dyn ego_acp::serve::SessionConfinementAdmission>>,
    ) -> agent_client_protocol::AgentProtocolRouter {
        self.router_with_admitter(admitter, controls, confinement, None, AcpNotices::default())
    }

    fn router_with_admitter(
        &self,
        admitter: Arc<dyn McpSessionAdmitter>,
        controls: Option<Arc<dyn ego_core::control::SessionControls>>,
        confinement: Option<Arc<dyn ego_acp::serve::SessionConfinementAdmission>>,
        trust: Option<Arc<dyn ego_acp::serve::SessionTrustAdmission>>,
        notices: AcpNotices,
    ) -> agent_client_protocol::AgentProtocolRouter {
        let notice_store = Arc::clone(&self.store);
        notices.set_delivery_recorder(Arc::new(move |session, notice| {
            notice_store
                .entries
                .lock()
                .map_err(|error| error.to_string())?
                .entry(session)
                .or_default()
                .push(SessionEntry::NoticeDelivered(notice));
            Ok(())
        }));
        let clock = Arc::new(TestClock);
        let controller = Arc::new(RunController::new(
            Arc::clone(&self.store),
            Arc::clone(&self.executor),
            Arc::clone(&clock),
            Arc::clone(&self.ids),
            RunRetention::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
        ));
        let runs: Arc<dyn SessionRunLifecycle> = controller.clone();
        let lifecycle = Arc::new(SessionLifecycle::new(Arc::clone(&self.store), runs, clock));
        ego_acp::serve::durable_agent_with_compaction(
            controller,
            lifecycle,
            Arc::new(TestRoots::with_effects(Arc::clone(&self.effects))),
            self.spawner.clone(),
            self.ids.clone(),
            Arc::new(AcpRunBindingsBuilder::default()),
            admitter,
            Arc::new(CompactionCommandFixture),
            controls,
            confinement,
            trust,
            notices,
        )
    }

    pub fn seed_sessions(&self, root: &str, count: usize) {
        let runs: Arc<dyn SessionRunLifecycle> = Arc::new(InactiveRuns);
        let lifecycle = SessionLifecycle::new(Arc::clone(&self.store), runs, Arc::new(TestClock));
        for _ in 0..count {
            lifecycle.create(PathBuf::from(root)).unwrap();
        }
    }

    /// One session with a caller-chosen id, so a test can place it exactly
    /// relative to another id in the list order.
    pub fn seed_session_with_id(&self, root: &str, id: SessionId) {
        let runs: Arc<dyn SessionRunLifecycle> = Arc::new(InactiveRuns);
        let lifecycle = SessionLifecycle::new(Arc::clone(&self.store), runs, Arc::new(TestClock));
        lifecycle.create_with_id(id, PathBuf::from(root)).unwrap();
    }

    pub fn seed_replay_session(
        &self,
        root: &str,
        title: &str,
        context_window: u64,
    ) -> SeededReplay {
        let runs: Arc<dyn SessionRunLifecycle> = Arc::new(InactiveRuns);
        let lifecycle = SessionLifecycle::new(Arc::clone(&self.store), runs, Arc::new(TestClock));
        let session = lifecycle.create(PathBuf::from(root)).unwrap().id;
        let event = DomainEvent {
            env: Envelope {
                seq: serde_json::from_value(serde_json::json!(1)).unwrap(),
                id: EventId::new(),
                turn: TurnId::new(),
                item: Some(ItemId::new()),
                parent: None,
                ts: Timestamp::EPOCH,
                producer: Producer::Main,
            },
            kind: EventKind::UserPrompt {
                audio: Vec::new(),
                text: "durable prompt".to_owned(),
                images: Vec::new(),
                queued_prompt: None,
            },
        };
        self.store.append(&session, &event).unwrap();
        let mut next = event.env.seq;
        let context = DomainEvent {
            env: Envelope {
                seq: next.next(),
                id: EventId::new(),
                turn: event.env.turn,
                item: None,
                parent: Some(event.env.id),
                ts: Timestamp::EPOCH,
                producer: Producer::Main,
            },
            kind: EventKind::ContextWindowUpdated {
                used_tokens: 1_234,
                provenance: OccupancyProvenance::GenerationBoundary,
                window: None,
            },
        };
        self.store.append(&session, &context).unwrap();
        self.store
            .entries
            .lock()
            .expect("support entries")
            .entry(session)
            .or_default()
            .push(SessionEntry::Evidence(generation_evidence(
                session,
                context_window,
            )));
        let lease = self.store.acquire_inactive_lease(session).unwrap();
        ego_core::session_lifecycle::SessionMetadata::new(
            Arc::clone(&self.store),
            Arc::new(TestClock),
        )
        .set_title(lease.as_ref(), context.env.seq, Some(title.to_owned()))
        .unwrap();
        let attachment =
            futures::executor::block_on(lifecycle.load(session, Path::new(root))).unwrap();
        SeededReplay {
            session_id: session.to_string(),
            expected_replay_updates: ego_acp::project::project_replay_v1(
                &attachment.replay,
                attachment.context_window,
                &attachment.root,
            )
            .into_iter()
            .map(|update| serde_json::to_value(update).unwrap())
            .collect(),
        }
    }

    pub fn seed_tool_replay_session(&self, root: &str) -> String {
        let runs: Arc<dyn SessionRunLifecycle> = Arc::new(InactiveRuns);
        let lifecycle = SessionLifecycle::new(Arc::clone(&self.store), runs, Arc::new(TestClock));
        let session = lifecycle.create(PathBuf::from(root)).unwrap().id;
        let turn = TurnId::new();
        let item = ItemId::new();
        let mut started = DomainEvent {
            env: Envelope {
                seq: serde_json::from_value(serde_json::json!(1)).unwrap(),
                id: EventId::new(),
                turn,
                item: Some(item),
                parent: None,
                ts: Timestamp::EPOCH,
                producer: Producer::Main,
            },
            kind: EventKind::ToolCallStarted {
                name: "read".to_owned(),
                args: r#"{"path":"src/main.rs"}"#.to_owned(),
                call_id: Some("read-call".to_owned()),
            },
        };
        self.store.append(&session, &started).unwrap();
        self.store
            .append(
                &session,
                &DomainEvent {
                    env: Envelope {
                        seq: started.env.seq.next(),
                        id: EventId::new(),
                        turn,
                        item: Some(item),
                        parent: Some(started.env.id),
                        ts: Timestamp::EPOCH,
                        producer: Producer::Main,
                    },
                    kind: EventKind::ToolCallCompleted {
                        ok: false,
                        result: ego_core::permit::ToolResult::builtin("file unavailable"),
                    },
                },
            )
            .unwrap();
        session.to_string()
    }

    pub fn seed_running_session(&self, root: &str) -> String {
        self.seed_pending_session(root, false)
    }

    pub fn seed_cancelling_session(&self, root: &str) -> String {
        self.seed_pending_session(root, true)
    }

    fn seed_pending_session(&self, root: &str, cancelling: bool) -> String {
        let runs: Arc<dyn SessionRunLifecycle> = Arc::new(InactiveRuns);
        let lifecycle = SessionLifecycle::new(Arc::clone(&self.store), runs, Arc::new(TestClock));
        let session = lifecycle.create(PathBuf::from(root)).unwrap().id;
        let controller = RunController::new(
            Arc::clone(&self.store),
            Arc::clone(&self.executor),
            Arc::new(TestClock),
            Arc::clone(&self.ids),
            RunRetention::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
        );
        let request_id = RequestId::from_uuid(self.ids.next_uuid());
        let root = RunRoot::new(PathBuf::from(root)).unwrap();
        let outcome = futures::executor::block_on(controller.start(
            StartRequest::new(
                session,
                root,
                request_id,
                ego_core::run::RequestDigest::parse("seed-running").unwrap(),
                RunSelections::empty(),
                "seed running recovery",
            ),
            RunExecutionBindings::new(Arc::new(NullSinkFactory), None),
        ))
        .unwrap();
        let ego_core::run::RunCommandOutcome::Admitted(admission) = outcome else {
            panic!("the recovery fixture must admit its seed run");
        };
        if cancelling {
            futures::executor::block_on(controller.cancel_admitted(admission.cancel_key()))
                .unwrap();
        }
        session.to_string()
    }

    /// Inject a store-port failure before the quarantine marker is published.
    pub fn fail_quarantine(&self) {
        *self.store.fail_quarantine.lock().unwrap() = true;
    }

    /// Hold this session's writer lease for as long as the guard lives.
    ///
    /// A lease race decided by a scheduler is not a test. This takes the lease
    /// the way another writer would, so the request under test loses it every
    /// run rather than two runs in three.
    pub fn hold_lease(&self, session: &str) -> impl Drop + use<> {
        let session = session.parse::<SessionId>().unwrap();
        assert!(
            self.store
                .leases
                .lock()
                .expect("support leases")
                .insert(session),
            "the fixture takes the lease before the request under test"
        );
        struct Held(Arc<Store>, SessionId);
        impl Drop for Held {
            fn drop(&mut self) {
                self.0
                    .leases
                    .lock()
                    .expect("support leases")
                    .remove(&self.1);
            }
        }
        Held(self.store.clone(), session)
    }

    /// How many holds this session's log actually records.
    ///
    /// The only way to tell a hold that was left alone from one that was
    /// re-established: both leave the session paused, and only the event count
    /// says whether the second request wrote anything.
    pub fn recorded_holds(&self, session: &str) -> usize {
        let session = session.parse::<SessionId>().unwrap();
        self.store
            .entries
            .lock()
            .expect("support entries")
            .get(&session)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| match entry {
                        SessionEntry::Event(event) => {
                            matches!(event.kind, EventKind::SessionPauseRequested { .. })
                        }
                        _ => false,
                    })
                    .count()
            })
            .unwrap_or_default()
    }

    pub fn run_status(&self, session: &str) -> RunStatus {
        let session = session.parse::<SessionId>().unwrap();
        let controller = RunController::new(
            Arc::clone(&self.store),
            Arc::clone(&self.executor),
            Arc::new(TestClock),
            Arc::clone(&self.ids),
            RunRetention::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
        );
        futures::executor::block_on(controller.status_by_session(session))
            .unwrap()
            .status()
    }

    pub fn lineage_source(&self, child: &str) -> Option<String> {
        let id = child.parse::<SessionId>().ok()?;
        let meta = self.store.metadata.lock().ok()?.get(&id)?.clone();
        match meta.lineage? {
            SessionLineage::Fork { source, .. } | SessionLineage::Compaction { source, .. } => {
                Some(source.to_string())
            }
        }
    }

    pub fn effect_counts(&self) -> (usize, usize, usize) {
        (
            self.effects.roots.load(Ordering::SeqCst),
            self.effects.mcp.load(Ordering::SeqCst),
            self.effects.lifecycle.load(Ordering::SeqCst),
        )
    }

    pub fn executor_effect_counts(&self) -> (usize, usize, usize) {
        (
            self.executor.attach_calls.load(Ordering::SeqCst),
            self.executor.submit_calls.load(Ordering::SeqCst),
            self.executor.observe_calls.load(Ordering::SeqCst),
        )
    }

    pub fn prepared_selections(&self) -> Option<ego_core::run::RunSelections> {
        self.executor.prepared_selections()
    }

    pub fn has_run(&self, session: &str) -> bool {
        let Ok(session) = session.parse::<SessionId>() else {
            return false;
        };
        self.store
            .entries
            .lock()
            .expect("support entries")
            .get(&session)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| matches!(entry, SessionEntry::RunTransaction(_)))
            })
    }

    pub fn block_next_turn(&self) {
        self.executor.block_next.store(true, Ordering::SeqCst);
    }

    pub fn ask_permission_next_turn(&self) {
        self.executor.ask_permission_next();
    }

    pub fn fail_next_attach(&self, diagnostic: &str) {
        *self.executor.attach_error.lock().expect("attach failure") = Some(diagnostic.to_owned());
    }

    pub fn fail_next_submit(&self, diagnostic: &str) {
        *self.executor.submit_error.lock().expect("submit failure") = Some(diagnostic.to_owned());
    }

    pub fn fail_next_settlement_publication(&self, diagnostic: &str) {
        *self
            .store
            .uncertain_settlement
            .lock()
            .expect("uncertain settlement") = Some(diagnostic.to_owned());
    }

    pub fn fail_next_observe(&self, diagnostic: &str) {
        *self.executor.observe_error.lock().expect("observe failure") = Some(diagnostic.to_owned());
    }

    pub async fn turn_began(&self) {
        self.executor.started.notified().await;
    }

    pub fn release_turn(&self) {
        self.executor.release.notify_one();
    }

    pub fn park_recovery_watcher(&self) {
        self.spawner.park_second.store(true, Ordering::SeqCst);
    }

    pub async fn recovery_watcher_parked(&self) {
        self.spawner.second_started.notified().await;
    }

    pub async fn recovery_watcher_finished(&self) {
        self.spawner.second_finished.notified().await;
    }

    pub async fn w2_execution_spawned(&self) {
        self.spawner.third_started.notified().await;
    }

    pub fn release_recovery_watcher(&self) {
        self.spawner.second_release.notify_one();
    }

    pub fn enable_running_recovery(&self) {
        self.executor.recover_running.store(true, Ordering::SeqCst);
    }

    pub fn block_recovery_observe(&self) {
        self.executor.block_observe.store(true, Ordering::SeqCst);
    }

    pub async fn recovery_observe_began(&self) {
        self.executor.observe_started.notified().await;
    }

    pub fn release_recovery_observe(&self) {
        self.executor.observe_release.notify_one();
    }

    pub async fn wait_for_run_status(&self, session: &str, expected: RunStatus) {
        for _ in 0..1_000 {
            if self.run_status(session) == expected {
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("run {session} did not reach {expected:?}");
    }
}

struct NullSink;

impl EventSink for NullSink {
    fn publish(&self, _event: &PublicEvent, _state: &SessionState) {}
}

struct NullSinkFactory;

impl ego_core::run::EventSinkFactory for NullSinkFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

fn generation_evidence(session: SessionId, context_window: u64) -> EvidenceRecord {
    EvidenceRecord::GenerationAdmission(GenerationAdmissionEvidence::new(
        session,
        RunInvocationId::new(),
        AttemptRef::first(GenerationId::new()),
        GenerationRole::Main,
        GenerationTarget::new(
            "fixture-provider",
            ModelRef("fixture-provider/model".to_owned()),
            "fixture-model",
        ),
        ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
            max_context_tokens: Some(context_window),
            max_output_tokens: None,
            max_tools: None,
            supports_parallel_tool_calls: false,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: ReasoningEffortSupport::Any,
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Unsupported,
            cache: CacheRetention::None,
            transport: Transport::Sse,
            image_input: ImageInputSupport::Unknown,
            default_reasoning_effort: None,
        },
        PricingEvidence::Unpriced {
            source: "fixture".to_owned(),
            reason: "not relevant to the ACP replay fixture".to_owned(),
        },
    ))
}

#[derive(Clone, Default)]
pub struct Store {
    entries: Arc<Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>>,
    metadata: Arc<Mutex<BTreeMap<SessionId, SessionMeta>>>,
    effects: Arc<EffectCounts>,
    /// Sessions whose inactive writer lease is currently held.
    ///
    /// The real store's lease is an fd lock on a per-session file, so a second
    /// acquisition while the first is alive fails. A double that hands one out
    /// unconditionally makes every concurrency test on this surface vacuous:
    /// two writers both pass their read-decide step and both append, and the
    /// test blames the code for what the double allowed.
    leases: Arc<Mutex<BTreeSet<SessionId>>>,
    /// Sessions hidden for good, as the real store keeps a marker beside the log.
    quarantined: Arc<Mutex<BTreeSet<SessionId>>>,
    fail_quarantine: Arc<Mutex<bool>>,
    uncertain_settlement: Arc<Mutex<Option<String>>>,
}

impl Store {
    fn with_effects(effects: Arc<EffectCounts>) -> Self {
        Self {
            entries: Arc::default(),
            metadata: Arc::default(),
            effects,
            leases: Arc::default(),
            quarantined: Arc::default(),
            fail_quarantine: Arc::default(),
            uncertain_settlement: Arc::default(),
        }
    }

    /// The semantic sequence a lifecycle append must expect, as the real store
    /// recomputes it from the log before every compare-and-append.
    fn semantic_seq(&self, id: &SessionId) -> Seq {
        self.entries
            .lock()
            .expect("support entries")
            .get(id)
            .into_iter()
            .flatten()
            .rev()
            .find_map(|entry| match entry {
                SessionEntry::Event(event) => Some(event.env.seq),
                SessionEntry::RunTransaction(_)
                | SessionEntry::Evidence(_)
                | SessionEntry::NoticeDelivered(_)
                | SessionEntry::ModelSelected(_) => None,
            })
            .unwrap_or(Seq::ZERO)
    }
}

impl SessionStore for Store {
    fn open_writer_lease(
        &self,
        id: &SessionId,
        _: Timestamp,
    ) -> Result<ego_core::store::session_store::SessionWriterLease<'_>, StoreError> {
        session_writer::open(self, &self.leases, *id)
    }

    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("support entries")
            .entry(*id)
            .or_default()
            .push(SessionEntry::Event(event.clone()));
        Ok(())
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let entries = self
            .entries
            .lock()
            .expect("support entries")
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound)?;
        Ok(Box::pin(futures::stream::iter(
            entries.into_iter().map(Ok::<_, StoreError>),
        )))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(self
            .metadata
            .lock()
            .expect("support metadata")
            .values()
            .cloned()
            .collect())
    }

    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        let mut entries = self.entries.lock().expect("support entries");
        let actual = entries.get(session).and_then(|entries| {
            entries.iter().rev().find_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                SessionEntry::Event(_) => None,
                SessionEntry::Evidence(_)
                | SessionEntry::NoticeDelivered(_)
                | SessionEntry::ModelSelected(_) => None,
            })
        });
        if expected_revision != actual {
            return match actual {
                Some(actual) => Ok(RunAppend::Conflict { actual }),
                None => Err(StoreError::NotFound),
            };
        }
        let revision = transaction.revision();
        let uncertain = matches!(transaction.command(), RunCommand::CommandSettled { .. })
            .then(|| {
                self.uncertain_settlement
                    .lock()
                    .expect("uncertain settlement")
                    .take()
            })
            .flatten();
        entries
            .entry(*session)
            .or_default()
            .push(SessionEntry::RunTransaction(transaction));
        match uncertain {
            Some(diagnostic) => Ok(RunAppend::CommittedDurabilityUncertain { diagnostic }),
            None => Ok(RunAppend::Committed { revision }),
        }
    }
}

impl EvidenceLogSource for Store {
    fn evidence_records(
        &self,
        session: &SessionId,
    ) -> Result<Vec<EvidenceRecord>, ManifestEvidenceError> {
        Ok(self
            .entries
            .lock()
            .expect("support entries")
            .get(session)
            .into_iter()
            .flatten()
            .filter_map(|entry| match entry {
                SessionEntry::Evidence(value) => Some(value.clone()),
                _ => None,
            })
            .collect())
    }
}

impl ReplaySource for Store {
    fn lineage(&self, id: &SessionId) -> Result<Option<SessionLineage>, ReplaySourceError> {
        Ok(self
            .metadata
            .lock()
            .expect("support metadata")
            .get(id)
            .and_then(|meta| meta.lineage))
    }

    fn entries(&self, id: &SessionId) -> Result<Option<Vec<SessionEntry>>, ReplaySourceError> {
        Ok(self
            .entries
            .lock()
            .expect("support entries")
            .get(id)
            .cloned())
    }
}

impl SessionBlobSource for Store {
    fn read(&self, _session: &SessionId, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Err(BlobError::NotFound {
            reference: reference.id.clone(),
        })
    }
}

struct SupportLease {
    session: SessionId,
    store: Store,
}

impl LeasedLifecycleAppend for SupportLease {
    fn session_id(&self) -> SessionId {
        self.session
    }

    fn compare_and_append_lifecycle(
        &self,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        if self.store.semantic_seq(&self.session) != expected {
            return Err(LifecycleAppendError::Conflict);
        }
        if let EventKind::SessionTitleChanged { title } = &event.kind
            && let Some(meta) = self
                .store
                .metadata
                .lock()
                .expect("support metadata")
                .get_mut(&self.session)
        {
            meta.title = title.clone();
            meta.updated = event.env.ts;
        }
        self.store
            .append(&self.session, &event)
            .map_err(LifecycleAppendError::Store)?;
        Ok(LifecyclePublication::Published)
    }
}

impl Drop for SupportLease {
    fn drop(&mut self) {
        self.store
            .leases
            .lock()
            .expect("support leases")
            .remove(&self.session);
    }
}

impl InactiveSessionLease for SupportLease {}

impl SessionLifecycleStore for Store {
    fn publish_session(
        &self,
        id: SessionId,
        root: &Path,
        created: Timestamp,
        profiles: ego_core::store::session_store::SessionProfiles,
    ) -> Result<LifecyclePublication, StoreError> {
        self.effects.lifecycle.fetch_add(1, Ordering::SeqCst);
        self.entries
            .lock()
            .expect("support entries")
            .insert(id, Vec::new());
        self.metadata.lock().expect("support metadata").insert(
            id,
            SessionMeta {
                profiles,
                id,
                workspace_root: root.to_path_buf(),
                model: None,
                created,
                updated: created,
                title: None,
                summary: None,
                summary_failures: 0,
                lineage: None,
            },
        );
        Ok(LifecyclePublication::Published)
    }

    fn lifecycle_meta(&self, id: SessionId) -> Result<Option<SessionMeta>, StoreError> {
        Ok(self
            .metadata
            .lock()
            .expect("support metadata")
            .get(&id)
            .cloned())
    }

    fn acquire_inactive_lease(
        &self,
        id: SessionId,
    ) -> Result<Box<dyn InactiveSessionLease + '_>, LifecycleAppendError> {
        if !self.leases.lock().expect("support leases").insert(id) {
            return Err(LifecycleAppendError::LeaseHeld);
        }
        Ok(Box::new(SupportLease {
            session: id,
            store: self.clone(),
        }))
    }

    fn quarantine(&self, id: SessionId) -> Result<(), StoreError> {
        if *self.fail_quarantine.lock().unwrap() {
            return Err(StoreError::Io(std::io::Error::other(
                "injected quarantine marker write failure",
            )));
        }
        self.quarantined
            .lock()
            .expect("support quarantine")
            .insert(id);
        Ok(())
    }

    fn is_quarantined(&self, id: SessionId) -> Result<bool, StoreError> {
        Ok(self
            .quarantined
            .lock()
            .expect("support quarantine")
            .contains(&id))
    }

    fn publish_fork(&self, id: SessionId, seed: &TargetSeed) -> TargetPublication {
        self.effects.lifecycle.fetch_add(1, Ordering::SeqCst);
        let source = match seed.lineage() {
            SessionLineage::Fork { source, .. } | SessionLineage::Compaction { source, .. } => {
                source
            }
        };
        let Some(source_meta) = self
            .metadata
            .lock()
            .expect("support metadata")
            .get(&source)
            .cloned()
        else {
            return TargetPublication::NotPublished {
                diagnostic: "source missing".to_owned(),
            };
        };
        let entries = seed
            .events()
            .cloned()
            .map(SessionEntry::Event)
            .collect::<Vec<_>>();
        let title = entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::Event(DomainEvent {
                kind: EventKind::SessionTitleChanged { title },
                ..
            }) => title.clone(),
            _ => None,
        });
        self.entries
            .lock()
            .expect("support entries")
            .insert(id, entries);
        self.metadata.lock().expect("support metadata").insert(
            id,
            SessionMeta {
                profiles: Default::default(),
                id,
                workspace_root: source_meta.workspace_root,
                model: source_meta.model,
                created: Timestamp::EPOCH,
                updated: Timestamp::EPOCH,
                title,
                summary: source_meta.summary,
                summary_failures: source_meta.summary_failures,
                lineage: Some(seed.lineage()),
            },
        );
        TargetPublication::PublishedDurably { diagnostic: None }
    }
}

// Shared by integration crates; some crates exercise only the alternate injected adapters.
#[allow(dead_code)]
#[derive(Default)]
pub struct Executor {
    block_next: AtomicBool,
    ask_permission_next: AtomicBool,
    recover_running: AtomicBool,
    block_observe: AtomicBool,
    attach_calls: AtomicUsize,
    submit_calls: AtomicUsize,
    observe_calls: AtomicUsize,
    attach_error: Mutex<Option<String>>,
    submit_error: Mutex<Option<String>>,
    observe_error: Mutex<Option<String>>,
    started: Notify,
    release: Notify,
    observe_started: Notify,
    observe_release: Notify,
    block_prepare: AtomicBool,
    prepare_started: Notify,
    prepare_release: Notify,
    prepared_selections: Mutex<Option<ego_core::run::RunSelections>>,
}

#[allow(dead_code)]
pub struct Prepared;

impl Executor {
    pub fn ask_permission_next(&self) {
        self.ask_permission_next.store(true, Ordering::SeqCst);
    }
    pub fn block_preparation(&self) {
        self.block_prepare.store(true, Ordering::SeqCst);
    }

    pub async fn preparation_started(&self) {
        self.prepare_started.notified().await;
    }

    pub fn release_preparation(&self) {
        self.prepare_release.notify_one();
    }

    pub fn prepared_selections(&self) -> Option<ego_core::run::RunSelections> {
        self.prepared_selections
            .lock()
            .expect("prepare selections")
            .clone()
    }
}

#[derive(Default)]
pub struct CompactionCommandFixture;

impl CompactionCommand for CompactionCommandFixture {
    fn compact<'a>(
        &'a self,
        request: CompactionTransitionRequest,
    ) -> CompactionFuture<'a, Result<CompactionReceipt, CompactionTransitionError>> {
        Box::pin(future::ready(Ok(CompactionReceipt {
            source: request.source,
            source_seq: Seq::ZERO,
            target: RequestId::session_id(request.request_id),
            publication: TargetPublication::PublishedDurably { diagnostic: None },
            successor_start_request_id: request.request_id,
        })))
    }
}

impl RunExecutor for Executor {
    type Prepared = Prepared;

    fn prepare<'a>(
        &'a self,
        request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        *self.prepared_selections.lock().expect("prepare selections") =
            Some(request.selections.clone());
        if self.block_prepare.swap(false, Ordering::SeqCst) {
            self.prepare_started.notify_one();
            return Box::pin(async move {
                self.prepare_release.notified().await;
                Ok(RunPrepared::new(
                    ModelRef("model/test".to_owned()),
                    Prepared,
                ))
            });
        }
        Box::pin(future::ready(Ok(RunPrepared::new(
            ModelRef("model/test".to_owned()),
            Prepared,
        ))))
    }

    fn admit_run_context<'a>(
        &'a self,
        _prepared: &'a Self::Prepared,
    ) -> RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        Box::pin(future::ready(Ok(EvidenceWrite::Published)))
    }

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        self.attach_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(diagnostic) = self.attach_error.lock().expect("attach failure").take() {
            return Box::pin(future::ready(Err(RunExecutorError::new(diagnostic))));
        }
        Box::pin(future::ready(Ok(RunHandle::new(
            request.run_id,
            Arc::new(AtomicBool::new(false)),
        ))))
    }

    fn recover<'a>(
        &'a self,
        run: &'a ego_core::run::RunId,
    ) -> BoxFuture<'a, Result<ego_core::run::RecoveryEvidence, RunExecutorError>> {
        if self.recover_running.load(Ordering::SeqCst) {
            return Box::pin(future::ready(Ok(ego_core::run::RecoveryEvidence::Running(
                RunHandle::new(*run, Arc::new(AtomicBool::new(false))),
            ))));
        }
        Box::pin(future::ready(Ok(
            ego_core::run::RecoveryEvidence::Ambiguous,
        )))
    }

    fn observe<'a>(
        &'a self,
        handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.observe_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(diagnostic) = self.observe_error.lock().expect("observe failure").take() {
                return Err(RunExecutorError::new(diagnostic));
            }
            if self.block_observe.swap(false, Ordering::SeqCst) {
                self.observe_started.notify_one();
                self.observe_release.notified().await;
            }
            let cancelled = handle
                .authority::<Arc<AtomicBool>>()
                .expect("support cancel authority")
                .load(Ordering::SeqCst);
            if cancelled {
                handle
                    .authority::<Arc<AtomicBool>>()
                    .expect("support cancel authority")
                    .store(false, Ordering::SeqCst);
                Ok(RunSettlement::cancelled(ego_core::run::CancelReason::User))
            } else {
                Ok(RunSettlement::completed())
            }
        })
    }

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        _prompt: &'a RunPrompt,
        _prepared: Self::Prepared,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.submit_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.ask_permission_next.swap(false, Ordering::SeqCst) {
                use ego_core::permit::{
                    Authority, CanonPath, Escalation, Op, OpName, PermissionDecision,
                    PermissionRequest,
                };
                let human = bindings.human().expect("ACP supplies a human seat");
                let request = PermissionRequest::new(
                    "exec",
                    vec![Op::Read {
                        path: CanonPath::new("/workspace/file"),
                    }],
                    Escalation::ModeAsks {
                        mode: ego_core::config::Mode::Default,
                        op: OpName::Spawn,
                        path: CanonPath::new("/workspace"),
                    },
                );
                let decision = human.decide(ItemId::new(), &request).await;
                return Ok(match decision {
                    PermissionDecision::Allow => RunSettlement::completed(),
                    _ => RunSettlement::permission_required(
                        "exec: permission was refused",
                        ego_core::run::DeniedBy::Decided(Authority::Party),
                    ),
                });
            }
            if let Some(diagnostic) = self.submit_error.lock().expect("submit failure").take() {
                return Err(RunExecutorError::new(diagnostic));
            }
            if self.block_next.swap(false, Ordering::SeqCst) {
                self.started.notify_one();
                self.release.notified().await;
            }
            let cancelled = handle
                .authority::<Arc<AtomicBool>>()
                .expect("support cancel authority")
                .load(Ordering::SeqCst);
            if cancelled {
                Ok(RunSettlement::cancelled(ego_core::run::CancelReason::User))
            } else {
                Ok(RunSettlement::completed())
            }
        })
    }

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        handle
            .authority::<Arc<AtomicBool>>()
            .expect("support cancel authority")
            .store(true, Ordering::SeqCst);
        Box::pin(future::ready(Ok(())))
    }

    /// Refused rather than answered `Ok`. This double's authority is a single
    /// cancel flag and it runs no turn, so it has nothing that could reach a
    /// boundary; answering `Ok` would tell a caller a hold is in force that
    /// nothing in this harness is honouring. A test that needs a real pause
    /// gives this executor an authority that can hold one.
    fn pause<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _request: RequestId,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(future::ready(Err(RunExecutorError::new(
            "this test executor cannot hold a run at a boundary",
        ))))
    }
}

#[allow(dead_code)]
#[derive(Default)]
pub struct TestClock;

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}

#[allow(dead_code)]
#[derive(Default)]
pub struct TestIds {
    next: Mutex<u128>,
}

impl IdSource for TestIds {
    fn next_uuid(&self) -> uuid::Uuid {
        let mut next = self.next.lock().expect("support ids");
        *next += 1;
        uuid::Uuid::from_u128(*next)
    }
}

#[derive(Default)]
struct TestRoots {
    effects: Option<Arc<EffectCounts>>,
}

impl TestRoots {
    fn with_effects(effects: Arc<EffectCounts>) -> Self {
        Self {
            effects: Some(effects),
        }
    }
}

impl RootResolver for TestRoots {
    fn resolve(&self, cwd: &Path) -> Result<RunRoot, ego_acp::serve::RootResolveError> {
        if let Some(effects) = &self.effects {
            effects.roots.fetch_add(1, Ordering::SeqCst);
        }
        RunRoot::new(cwd.to_path_buf())
            .map_err(|error| ego_acp::serve::RootResolveError::new(error.to_string()))
    }
}

struct TestSpawner {
    spawn_count: AtomicUsize,
    park_second: AtomicBool,
    second_started: Arc<Notify>,
    second_release: Arc<Notify>,
    second_finished: Arc<Notify>,
    third_started: Arc<Notify>,
}

impl Default for TestSpawner {
    fn default() -> Self {
        Self {
            spawn_count: AtomicUsize::new(0),
            park_second: AtomicBool::new(false),
            second_started: Arc::new(Notify::new()),
            second_release: Arc::new(Notify::new()),
            second_finished: Arc::new(Notify::new()),
            third_started: Arc::new(Notify::new()),
        }
    }
}

impl ExecutionSpawner for TestSpawner {
    fn spawn(&self, execution: BoxFuture<'static, ()>) {
        let ordinal = self.spawn_count.fetch_add(1, Ordering::SeqCst) + 1;
        let park = ordinal == 2 && self.park_second.swap(false, Ordering::SeqCst);
        let started = self.second_started.clone();
        let release = self.second_release.clone();
        let finished = self.second_finished.clone();
        let third_started = self.third_started.clone();
        if ordinal == 3 {
            third_started.notify_one();
        }
        tokio::spawn(async move {
            if park {
                started.notify_one();
                release.notified().await;
            }
            execution.await;
            if park {
                finished.notify_one();
            }
        });
    }
}

#[allow(dead_code)]
pub fn agent() -> agent_client_protocol::AgentProtocolRouter {
    let ids = Arc::new(TestIds::default());
    agent_with_compaction(
        Arc::new(Store::default()),
        Arc::new(Executor::default()),
        Arc::new(TestClock),
        ids,
        Arc::new(CompactionCommandFixture),
        None,
    )
}

#[allow(dead_code)]
pub fn agent_with_compaction<S, E, C, I>(
    store: Arc<S>,
    executor: Arc<E>,
    clock: Arc<C>,
    ids: Arc<I>,
    compaction: Arc<dyn CompactionCommand>,
    controls: Option<Arc<dyn ego_core::control::SessionControls>>,
) -> agent_client_protocol::AgentProtocolRouter
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    agent_with_compaction_and_confinement(store, executor, clock, ids, compaction, controls, None)
}

#[allow(clippy::too_many_arguments)]
pub fn agent_with_compaction_and_confinement<S, E, C, I>(
    store: Arc<S>,
    executor: Arc<E>,
    clock: Arc<C>,
    ids: Arc<I>,
    compaction: Arc<dyn CompactionCommand>,
    controls: Option<Arc<dyn ego_core::control::SessionControls>>,
    confinement: Option<Arc<dyn ego_acp::serve::SessionConfinementAdmission>>,
) -> agent_client_protocol::AgentProtocolRouter
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    agent_with_compaction_confinement_and_trust(
        store,
        executor,
        clock,
        ids,
        compaction,
        controls,
        confinement,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn agent_with_compaction_confinement_and_trust<S, E, C, I>(
    store: Arc<S>,
    executor: Arc<E>,
    clock: Arc<C>,
    ids: Arc<I>,
    compaction: Arc<dyn CompactionCommand>,
    controls: Option<Arc<dyn ego_core::control::SessionControls>>,
    confinement: Option<Arc<dyn ego_acp::serve::SessionConfinementAdmission>>,
    trust: Option<Arc<dyn ego_acp::serve::SessionTrustAdmission>>,
) -> agent_client_protocol::AgentProtocolRouter
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    let controller = Arc::new(RunController::new(
        store,
        executor,
        clock,
        Arc::clone(&ids),
        RunRetention::new(std::num::NonZeroUsize::new(2).expect("retention")).expect("retention"),
    ));
    ego_acp::serve::agent_with_compaction(
        controller,
        Arc::new(TestRoots::default()),
        Arc::new(TestSpawner::default()),
        ids,
        Arc::new(AcpRunBindingsBuilder::default()),
        Arc::new(RejectingMcpAdmitter::default()),
        compaction,
        controls,
        confinement,
        trust,
    )
}

#[derive(Default)]
struct RejectingMcpAdmitter {
    effects: Option<Arc<EffectCounts>>,
}

impl RejectingMcpAdmitter {
    fn with_effects(effects: Arc<EffectCounts>) -> Self {
        Self {
            effects: Some(effects),
        }
    }
}

impl McpSessionAdmitter for RejectingMcpAdmitter {
    fn admit(
        &self,
        _servers: Vec<agent_client_protocol_schema::v1::McpServer>,
        _connection: Option<agent_client_protocol::ConnectionTo<agent_client_protocol::Client>>,
        _context: ego_acp::serve::McpAdmissionContext,
    ) -> BoxFuture<'static, Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError>>
    {
        if let Some(effects) = &self.effects {
            effects.mcp.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(future::ready(Ok(None)))
    }
}

#[allow(dead_code)]
pub fn agent_with_mcp<S, E, C, I>(
    store: Arc<S>,
    executor: Arc<E>,
    clock: Arc<C>,
    ids: Arc<I>,
    admitter: Arc<dyn McpSessionAdmitter>,
) -> agent_client_protocol::AgentProtocolRouter
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    let controller = Arc::new(RunController::new(
        store,
        executor,
        clock,
        Arc::clone(&ids),
        RunRetention::new(std::num::NonZeroUsize::new(2).expect("retention")).expect("retention"),
    ));
    ego_acp::serve::agent(
        controller,
        Arc::new(TestRoots::default()),
        Arc::new(TestSpawner::default()),
        ids,
        Arc::new(AcpRunBindingsBuilder::default()),
        admitter,
    )
}

/// One in-memory ACP connection to the real pinned SDK router.
///
/// Three suites grew their own copy of this before it had a home; those are
/// left unchanged here; these tests do not reconcile it.
/// New router tests use this one so the fourth copy is the last.
pub struct Connected {
    to_agent: mpsc::UnboundedSender<String>,
    from_agent: mpsc::UnboundedReceiver<String>,
    agent: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    next_id: u64,
}

impl Connected {
    pub fn new(router: agent_client_protocol::AgentProtocolRouter) -> Self {
        let (to_agent, to_agent_rx) = mpsc::unbounded::<String>();
        let (from_agent, from_agent_rx) = mpsc::unbounded::<String>();
        let transport = Lines::new(
            from_agent.sink_map_err(std::io::Error::other),
            to_agent_rx.map(Ok::<_, std::io::Error>),
        );
        Self {
            to_agent,
            from_agent: from_agent_rx,
            agent: tokio::spawn(router.connect_to(transport)),
            next_id: 1,
        }
    }

    pub async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let id = self.send_request(method, params).await;
        self.response(id).await
    }

    pub async fn send_request(&mut self, method: &str, params: serde_json::Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.to_agent
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                })
                .to_string(),
            )
            .await
            .expect("the ACP agent is listening");
        id
    }

    /// Send one notification, which by definition is never answered.
    pub async fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.to_agent
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params,
                })
                .to_string(),
            )
            .await
            .expect("the ACP agent is listening");
    }

    pub async fn response(&mut self, id: u64) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut notifications = Vec::new();
        loop {
            let line = self
                .from_agent
                .next()
                .await
                .expect("every request is answered");
            let message: serde_json::Value =
                serde_json::from_str(&line).expect("the agent writes JSON-RPC");
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return (message, notifications);
            }
            notifications.push(message);
        }
    }

    pub async fn announced(&mut self, root: &str) -> Vec<serde_json::Value> {
        let fence = self
            .send_request("session/list", serde_json::json!({"cwd": root}))
            .await;
        self.response(fence).await.1
    }

    pub async fn initialize(&mut self) -> serde_json::Value {
        self.initialize_with_capabilities(serde_json::json!({}))
            .await
    }

    pub async fn initialize_with_capabilities(
        &mut self,
        capabilities: serde_json::Value,
    ) -> serde_json::Value {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": ProtocolVersion::V1.as_u16(),
                "clientCapabilities": capabilities,
            }),
        )
        .await
        .0
    }

    /// Read one wire frame, including deferred notifications without a request.
    pub async fn frame(&mut self) -> serde_json::Value {
        let line = self
            .from_agent
            .next()
            .await
            .expect("the agent is connected");
        serde_json::from_str(&line).expect("the agent writes JSON-RPC")
    }

    /// The next message the agent sends that is a *request* rather than a
    /// response or a notification, dropping everything before it.
    ///
    /// A request carries both an id and a method; that pair is what tells it
    /// apart from a response to one of ours, whose id comes from a counter
    /// this side owns and may coincidentally match.
    pub async fn agent_request(&mut self) -> serde_json::Value {
        loop {
            let line = self
                .from_agent
                .next()
                .await
                .expect("the agent is still connected");
            let message: serde_json::Value =
                serde_json::from_str(&line).expect("the agent writes JSON-RPC");
            if message.get("id").is_some() && message.get("method").is_some() {
                return message;
            }
        }
    }

    /// Answer one agent-originated request.
    pub async fn answer(&mut self, request: &serde_json::Value, result: serde_json::Value) {
        self.to_agent
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": result,
                })
                .to_string(),
            )
            .await
            .expect("the ACP agent is listening");
    }

    /// Fail one agent-originated request the way a host that cannot serve it
    /// does: an error response, not an answer and not silence.
    #[allow(dead_code)]
    pub async fn refuse_request(&mut self, request: &serde_json::Value, message: &str) {
        self.to_agent
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": { "code": -32603, "message": message },
                })
                .to_string(),
            )
            .await
            .expect("the ACP agent is listening");
    }

    pub async fn new_session(&mut self, root: &str) -> serde_json::Value {
        self.request(
            "session/new",
            serde_json::json!({"cwd": root, "mcpServers": []}),
        )
        .await
        .0
    }

    pub async fn abort(self) {
        self.to_agent.close_channel();
        self.agent.abort();
        let _ = self.agent.await;
    }
}
