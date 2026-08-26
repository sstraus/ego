//! RED contract for regression case's rooted durable RunController.
//!
//! The store and executor below are complete local adapters. Their only
//! purpose is to expose durable transaction order, admission-before-effect,
//! recovery evidence, and interrupt ownership to these tests.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::task::{Poll, Waker};

use futures::future::{self, BoxFuture};
use futures::stream::{self, BoxStream};

use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::run::{
    CancelRequest, ContinueRequest, IdSource, RequestDigest, RequestId, RunAppend, RunController,
    RunExecutor, RunExecutorError, RunHandle, RunId, RunRetention, RunRoot, RunStart, RunStatus,
    RunRevision, RunTransaction, StartRequest,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
enum StoreFault {
    Reject,
    PublishedUncertain,
}

#[derive(Default)]
struct RecordingSessionStore {
    entries: Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>,
    faults: Mutex<VecDeque<StoreFault>>,
    sequence: Arc<Mutex<Vec<&'static str>>>,
}

impl RecordingSessionStore {
    fn with_sequence(sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            faults: Mutex::new(VecDeque::new()),
            sequence,
        }
    }

    fn fail_next(&self, fault: StoreFault) {
        self.faults.lock().expect("store faults").push_back(fault);
    }

    fn transactions(&self, session: SessionId) -> Vec<RunTransaction> {
        self.entries
            .lock()
            .expect("session entries")
            .get(&session)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| match entry {
                        SessionEntry::RunTransaction(transaction) => Some(transaction.clone()),
                        SessionEntry::Event(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl SessionStore for RecordingSessionStore {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("session entries")
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
            .expect("session entries")
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound)?;
        Ok(Box::pin(stream::iter(
            entries.into_iter().map(Ok::<_, StoreError>),
        )))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(self
            .entries
            .lock()
            .expect("session entries")
            .keys()
            .map(|id| SessionMeta {
                id: *id,
                workspace_root: PathBuf::from("/workspace"),
                model: None,
                created: Timestamp::EPOCH,
                updated: Timestamp::EPOCH,
                title: None,
                lineage: None,
            })
            .collect())
    }

    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        let fault = self.faults.lock().expect("store faults").pop_front();
        if let Some(fault) = fault {
            match fault {
                StoreFault::Reject => return Err(StoreError::Io(std::io::Error::other("rejected"))),
                StoreFault::PublishedUncertain => {
                    self.sequence.lock().expect("transaction sequence").push("publish");
                    self.entries
                        .lock()
                        .expect("session entries")
                        .entry(*session)
                        .or_default()
                        .push(SessionEntry::RunTransaction(transaction));
                    return Ok(RunAppend::CommittedDurabilityUncertain {
                        diagnostic: "publication durability uncertain".to_string(),
                    });
                }
            }
        }
        let mut entries = self.entries.lock().expect("session entries");
        let current = entries
            .get(session)
            .and_then(|entries| {
                entries.iter().rev().find_map(|entry| match entry {
                    SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                    SessionEntry::Event(_) => None,
                })
            });
        if expected_revision != current {
            return match current {
                Some(actual) => Ok(RunAppend::Conflict { actual }),
                None => Err(StoreError::NotFound),
            };
        }
        entries
            .entry(*session)
            .or_default()
            .push(SessionEntry::RunTransaction(transaction));
        self.sequence.lock().expect("transaction sequence").push("commit");
        Ok(RunAppend::Committed {
            revision: current.map_or_else(|| RunRevision::new(1), RunRevision::next),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecutorCall {
    Start(RunId),
    Recover(RunId),
    Submit(RunId),
    Interrupt(RunId),
}

struct ScriptedRunExecutor {
    calls: Mutex<Vec<ExecutorCall>>,
    recoveries: Mutex<VecDeque<Result<ego_core::run::RecoveryEvidence, RunExecutorError>>>,
    submit_gate: Mutex<SubmitGate>,
    submit_gate_changed: Condvar,
    started_roots: Mutex<Vec<(RunId, RunRoot)>>,
    start_barrier: Option<Arc<Barrier>>,
    sequence: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Default)]
struct SubmitGate {
    released: bool,
    started: bool,
    waker: Option<Waker>,
}

impl Default for ScriptedRunExecutor {
    fn default() -> Self {
        Self::with_sequence(Arc::new(Mutex::new(Vec::new())))
    }
}

impl ScriptedRunExecutor {
    fn with_sequence(sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            recoveries: Mutex::new(VecDeque::new()),
            submit_gate: Mutex::new(SubmitGate::default()),
            submit_gate_changed: Condvar::new(),
            started_roots: Mutex::new(Vec::new()),
            start_barrier: None,
            sequence,
        }
    }

    fn block_effects(&self) {
        let mut gate = self.submit_gate.lock().expect("executor gate");
        gate.released = false;
        gate.started = false;
        gate.waker = None;
    }

    fn with_start_barrier(
        sequence: Arc<Mutex<Vec<&'static str>>>,
        barrier: Arc<Barrier>,
    ) -> Self {
        let mut executor = Self::with_sequence(sequence);
        executor.start_barrier = Some(barrier);
        executor
    }

    fn recovery_result(&self, result: Result<ego_core::run::RecoveryEvidence, RunExecutorError>) {
        self.recoveries
            .lock()
            .expect("executor recoveries")
            .push_back(result);
    }

    fn allow_effects(&self) {
        let waker = {
            let mut gate = self.submit_gate.lock().expect("executor gate");
            gate.released = true;
            self.submit_gate_changed.notify_all();
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn wait_for_submit_start(&self) {
        let mut gate = self.submit_gate.lock().expect("executor gate");
        while !gate.started {
            gate = self
                .submit_gate_changed
                .wait(gate)
                .expect("executor gate wait");
        }
    }

    fn calls(&self) -> Vec<ExecutorCall> {
        self.calls.lock().expect("executor calls").clone()
    }

    fn started_roots(&self) -> Vec<(RunId, RunRoot)> {
        self.started_roots
            .lock()
            .expect("executor roots")
            .clone()
    }
}

impl RunExecutor for ScriptedRunExecutor {
    fn start<'a>(
        &'a self,
        request: &'a RunStart,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        let run = request.run_id;
        if let Some(barrier) = self.start_barrier.as_ref() {
            barrier.wait();
        }
        self.started_roots
            .lock()
            .expect("executor roots")
            .push((run, request.root.clone()));
        self.sequence.lock().expect("transaction sequence").push("start");
        self.calls.lock().expect("executor calls").push(ExecutorCall::Start(run));
        Box::pin(future::ready(Ok(RunHandle::new(run))))
    }

    fn recover<'a>(
        &'a self,
        run: &'a RunId,
    ) -> BoxFuture<'a, Result<ego_core::run::RecoveryEvidence, RunExecutorError>> {
        self.calls.lock().expect("executor calls").push(ExecutorCall::Recover(*run));
        let result = self
            .recoveries
            .lock()
            .expect("executor recoveries")
            .pop_front()
            .unwrap_or(Ok(ego_core::run::RecoveryEvidence::Ambiguous));
        Box::pin(future::ready(result))
    }

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        _prompt: &'a str,
    ) -> BoxFuture<'a, Result<ego_core::run::RunSettlement, RunExecutorError>> {
        self.calls.lock().expect("executor calls").push(ExecutorCall::Submit(handle.run_id()));
        self.sequence.lock().expect("transaction sequence").push("submit");
        let gate = &self.submit_gate;
        let changed = &self.submit_gate_changed;
        Box::pin(futures::future::poll_fn(move |cx| {
            let mut gate = gate.lock().expect("executor gate");
            gate.started = true;
            changed.notify_all();
            if gate.released {
                return Poll::Ready(Ok(ego_core::run::RunSettlement::completed()));
            }
            gate.waker = Some(cx.waker().clone());
            Poll::Pending
        }))
    }

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        self.calls
            .lock()
            .expect("executor calls")
            .push(ExecutorCall::Interrupt(handle.run_id()));
        self.sequence.lock().expect("transaction sequence").push("interrupt");
        Box::pin(future::ready(Ok(())))
    }
}

struct FixedClock {
    now: Timestamp,
}

impl Default for FixedClock {
    fn default() -> Self {
        Self {
            now: Timestamp::EPOCH,
        }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.now.clone()
    }
}

#[derive(Default)]
struct SequenceIds {
    next: Mutex<u128>,
}

impl IdSource for SequenceIds {
    fn next_uuid(&self) -> uuid::Uuid {
        let mut next = self.next.lock().expect("sequence ids");
        *next += 1;
        uuid::Uuid::from_u128(*next)
    }
}

fn request(value: u128) -> RequestId {
    RequestId::from_uuid(uuid::Uuid::from_u128(value))
}

fn digest(value: &str) -> RequestDigest {
    RequestDigest::parse(value).expect("request digest")
}

fn start(root: &str, value: u128, body: &str) -> StartRequest {
    StartRequest::new(
        RunRoot::new(root).expect("absolute run root"),
        request(value),
        digest(body),
        "model/test".parse().expect("model"),
    )
}

fn controller(
    store: Arc<RecordingSessionStore>,
    executor: Arc<ScriptedRunExecutor>,
) -> RunController<RecordingSessionStore, ScriptedRunExecutor, FixedClock, SequenceIds> {
    RunController::new(
        store,
        executor,
        Arc::new(FixedClock::default()),
        Arc::new(SequenceIds::default()),
        RunRetention::new(NonZeroUsize::new(2).expect("non-zero retention"))
            .expect("retention"),
    )
}

#[test]
fn revision_one_is_atomically_published_before_any_executor_start() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(ScriptedRunExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = futures::executor::block_on(controller.start(start("/a", 1, "a")))
        .expect("run admission");
    assert_eq!(
        sequence
            .lock()
            .expect("transaction sequence")
            .as_slice(),
        ["commit", "start"]
    );
    assert_eq!(store.transactions(admission.session_id()).len(), 1);
    assert_eq!(executor.calls(), vec![ExecutorCall::Start(admission.run_id())]);
    assert_eq!(admission.revision(), RunRevision::new(1));
}

#[test]
fn start_derives_uuidv5_session_identity_and_mints_a_distinct_run_nonce() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(store, executor);
    let admission = futures::executor::block_on(controller.start(start("/a", 2, "a")))
        .expect("run admission");
    assert_eq!(
        admission.session_id(),
        RequestId::session_id(request(2))
    );
    assert_ne!(admission.run_id().nonce(), admission.session_id().uuid());
    assert_eq!(admission.run_id().session_id(), admission.session_id());
}

#[test]
fn exact_replay_is_idempotent_but_digest_mismatch_and_identity_collision_have_no_effect() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = futures::executor::block_on(controller.start(start("/a", 3, "a")))
        .expect("run admission");
    let replay = futures::executor::block_on(controller.start(start("/a", 3, "a")))
        .expect("exact replay");
    assert_eq!(replay, first);
    assert_eq!(executor.calls(), vec![ExecutorCall::Start(first.run_id())]);
    assert!(matches!(
        futures::executor::block_on(controller.start(start("/a", 3, "b"))),
        Err(ego_core::run::RunError::RequestIdMismatch { .. })
    ));
    assert_eq!(store.transactions(first.session_id()).len(), 1);
    assert_eq!(executor.calls(), vec![ExecutorCall::Start(first.run_id())]);

    let collision = futures::executor::block_on(controller.start(start("/b", 3, "a")));
    assert!(matches!(collision, Err(ego_core::run::RunError::IdentityCollision { .. })));
    assert_eq!(executor.calls(), vec![ExecutorCall::Start(first.run_id())]);
}

#[test]
fn concurrent_same_request_has_one_winner_and_one_replay() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = Arc::new(controller(Arc::clone(&store), Arc::clone(&executor)));
    let left = Arc::clone(&controller);
    let right = Arc::clone(&controller);
    let (a, b) = std::thread::scope(|scope| {
        let first = scope.spawn(|| futures::executor::block_on(left.start(start("/a", 4, "a"))));
        let second = scope.spawn(|| futures::executor::block_on(right.start(start("/a", 4, "a"))));
        (first.join().expect("first start"), second.join().expect("second start"))
    });
    let first = a.expect("first admission");
    let second = b.expect("second admission");
    assert_eq!(first, second);
    assert_eq!(store.transactions(first.session_id()).len(), 1);
    assert_eq!(executor.calls().len(), 1);
}

#[test]
fn prepublication_failure_starts_no_executor_and_published_uncertain_starts_no_executor() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    store.fail_next(StoreFault::Reject);
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    assert!(futures::executor::block_on(controller.start(start("/a", 5, "a"))).is_err());
    assert!(store.transactions(RequestId::session_id(request(5))).is_empty());
    assert!(executor.calls().is_empty());

    store.fail_next(StoreFault::PublishedUncertain);
    let uncertain = futures::executor::block_on(controller.start(start("/b", 6, "b")));
    let run_id = store
        .transactions(RequestId::session_id(request(6)))
        .first()
        .expect("published uncertain transaction")
        .run_id();
    assert!(matches!(
        uncertain,
        Err(ego_core::run::RunError::PublishedDurabilityUncertain { diagnostic, .. })
            if diagnostic == "publication durability uncertain"
    ));
    let uncertain_transactions = store.transactions(run_id.session_id());
    assert_eq!(uncertain_transactions.len(), 1);
    assert_eq!(uncertain_transactions[0].revision(), RunRevision::new(1));
    assert_eq!(
        controller.status(run_id).expect("new authority snapshot").status(),
        RunStatus::Uncertain
    );
    assert!(executor.calls().is_empty());
}

#[test]
fn continuation_and_cancel_commit_receipts_before_submit_or_interrupt_and_never_retarget() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(ScriptedRunExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = futures::executor::block_on(controller.start(start("/a", 7, "a")))
        .expect("run admission");
    executor.allow_effects();
    let next = futures::executor::block_on(controller.continue_run(ContinueRequest::new(
        first.run_id(),
        request(8),
        digest("b"),
        first.revision(),
        "continue",
    )))
    .expect("continuation admission");
    let transactions = store.transactions(next.session_id());
    assert_eq!(transactions.len(), 2);
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert!(matches!(transactions[1].command(), ego_core::run::RunCommand::Continue { .. }));
    assert!(executor.calls().contains(&ExecutorCall::Submit(next.run_id())));
    let sequence_after_submit = sequence.lock().expect("transaction sequence").clone();
    assert!(sequence_after_submit
        .windows(2)
        .any(|pair| pair == ["commit", "submit"]));
    let cancel = CancelRequest::new(next.run_id(), request(9), digest("c"), next.revision());
    futures::executor::block_on(controller.cancel(cancel)).expect("cancel admission");
    let transactions = store.transactions(next.session_id());
    assert_eq!(transactions.len(), 3);
    assert_eq!(transactions[2].revision(), RunRevision::new(3));
    assert!(executor.calls().contains(&ExecutorCall::Interrupt(next.run_id())));
    let sequence_after_interrupt = sequence.lock().expect("transaction sequence").clone();
    assert!(sequence_after_interrupt
        .windows(2)
        .any(|pair| pair == ["commit", "interrupt"]));
    assert!(futures::executor::block_on(controller.cancel(CancelRequest::new(
        next.run_id(),
        request(10),
        digest("stale"),
        next.revision(),
    )))
    .is_err());
    assert_eq!(store.transactions(next.session_id()).len(), 3);
}

#[test]
fn distinct_roots_run_concurrently_without_sharing_a_lease() {
    let store = Arc::new(RecordingSessionStore::default());
    let barrier = Arc::new(Barrier::new(2));
    let executor = Arc::new(ScriptedRunExecutor::with_start_barrier(
        Arc::new(Mutex::new(Vec::new())),
        Arc::clone(&barrier),
    ));
    let controller = Arc::new(controller(Arc::clone(&store), Arc::clone(&executor)));
    let left = Arc::clone(&controller);
    let right = Arc::clone(&controller);
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| futures::executor::block_on(left.start(start("/one", 11, "a"))));
        let second = scope.spawn(|| futures::executor::block_on(right.start(start("/two", 12, "b"))));
        (
            first.join().expect("first run").expect("first admission"),
            second.join().expect("second run").expect("second admission"),
        )
    });
    assert_ne!(first.run_id(), second.run_id());
    let started_roots = executor.started_roots();
    assert_eq!(started_roots.len(), 2);
    assert!(started_roots.iter().any(|(run, root)| {
        *run == first.run_id() && root == &RunRoot::new("/one").expect("root")
    }));
    assert!(started_roots.iter().any(|(run, root)| {
        *run == second.run_id() && root == &RunRoot::new("/two").expect("root")
    }));
    assert!(futures::executor::block_on(controller.continue_run(ContinueRequest::new(
        first.run_id(),
        request(13),
        digest("busy"),
        first.revision(),
        "blocked",
    )))
    .is_err());
    assert_eq!(controller.status(second.run_id()).expect("status").root(), second.root());
}

fn recovery_controller(
    evidence: ego_core::run::RecoveryEvidence,
) -> (
    RunController<RecordingSessionStore, ScriptedRunExecutor, FixedClock, SequenceIds>,
    Arc<RecordingSessionStore>,
    Arc<ScriptedRunExecutor>,
    RunId,
) {
    let store = Arc::new(RecordingSessionStore::default());
    let first_executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&first_executor));
    let admission = futures::executor::block_on(first_controller.start(start(
        "/recover",
        20,
        "recovery",
    )))
    .expect("persisted running admission");
    let run_id = admission.run_id();
    drop(admission);
    drop(first_controller);
    drop(first_executor);

    let executor = Arc::new(ScriptedRunExecutor::default());
    executor.recovery_result(Ok(evidence));
    (
        controller(Arc::clone(&store), Arc::clone(&executor)),
        store,
        executor,
        run_id,
    )
}

#[test]
fn recovery_running_reattaches_live_execution() {
    let (controller, store, executor, run) =
        recovery_controller(ego_core::run::RecoveryEvidence::Running);
    assert_eq!(store.transactions(run.session_id()).len(), 1);
    let status = controller.status(run).expect("running status");
    assert_eq!(status.status(), RunStatus::Running);
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
}

#[test]
fn recovery_terminal_preserves_terminal_settlement() {
    let (controller, store, executor, run) =
        recovery_controller(ego_core::run::RecoveryEvidence::Terminal);
    assert_eq!(store.transactions(run.session_id())[0].status(), RunStatus::Running);
    assert_eq!(controller.status(run).expect("terminal status").status(), RunStatus::Settled);
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
}

#[test]
fn recovery_cancelled_before_start_settles_interrupted() {
    let (controller, store, executor, run) =
        recovery_controller(ego_core::run::RecoveryEvidence::CancelledBeforeStart);
    assert_eq!(store.transactions(run.session_id())[0].status(), RunStatus::Running);
    assert_eq!(
        controller.status(run).expect("interrupted status").status(),
        RunStatus::Interrupted
    );
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
}

#[test]
fn recovery_ambiguous_external_effect_is_uncertain_and_not_retried() {
    let (controller, store, executor, run) =
        recovery_controller(ego_core::run::RecoveryEvidence::Ambiguous);
    assert_eq!(store.transactions(run.session_id())[0].status(), RunStatus::Running);
    assert_eq!(controller.status(run).expect("uncertain status").status(), RunStatus::Uncertain);
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
    assert!(controller.status(run).expect("stable uncertain status").blocks_new_turn());
}

#[test]
fn recovery_of_an_already_terminal_durable_run_never_calls_recover() {
    let store = Arc::new(RecordingSessionStore::default());
    let first_executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&first_executor));
    let admission = futures::executor::block_on(first_controller.start(start(
        "/already-terminal",
        21,
        "terminal",
    )))
    .expect("running admission");
    first_executor.allow_effects();
    let settled = futures::executor::block_on(first_controller.continue_run(
        ContinueRequest::new(
            admission.run_id(),
            request(22),
            digest("terminal-settlement"),
            admission.revision(),
            "settle",
        ),
    ))
    .expect("terminal settlement");
    let run = settled.run_id();
    assert_eq!(store.transactions(run.session_id()).last().expect("terminal transaction").status(), RunStatus::Settled);
    drop(settled);
    drop(admission);
    drop(first_controller);
    drop(first_executor);

    let fresh_executor = Arc::new(ScriptedRunExecutor::default());
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&fresh_executor));
    assert_eq!(fresh_controller.status(run).expect("terminal replay").status(), RunStatus::Settled);
    assert!(fresh_executor.calls().is_empty());
}

#[test]
fn retention_prunes_completed_receipts_but_preserves_active_work_across_replay() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(ScriptedRunExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = Arc::new(controller(Arc::clone(&store), Arc::clone(&executor)));
    let active = futures::executor::block_on(controller.start(start("/active", 30, "active")))
        .expect("active run");

    let active_session = active.session_id();
    let active_replay = futures::executor::block_on(controller.start(start("/active", 30, "active")))
        .expect("active exact replay");
    assert_eq!(active_replay.run_id(), active.run_id());
    assert_eq!(store.transactions(active_session).len(), 1);
    assert_eq!(controller.status(active.run_id()).expect("active status").status(), RunStatus::Running);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Start(_)))
            .count(),
        1
    );

    executor.allow_effects();
    let first_expected = active.revision();
    let first = futures::executor::block_on(controller.continue_run(ContinueRequest::new(
        active.run_id(),
        request(31),
        digest("continue-31"),
        first_expected,
        "continue 31",
    )))
    .expect("first completed continuation");
    let second_expected = first.revision();
    let second = futures::executor::block_on(controller.continue_run(ContinueRequest::new(
        active.run_id(),
        request(32),
        digest("continue-32"),
        second_expected,
        "continue 32",
    )))
    .expect("second completed continuation");
    let third_expected = second.revision();
    let third = futures::executor::block_on(controller.continue_run(ContinueRequest::new(
        active.run_id(),
        request(33),
        digest("continue-33"),
        third_expected,
        "continue 33",
    )))
    .expect("third completed continuation");
    let current_revision = third.revision();
    assert_eq!(store.transactions(active_session).len(), 4);

    let calls_before_retained_replay = executor.calls();
    let transactions_before_retained_replay = store.transactions(active_session);
    let middle_replay = futures::executor::block_on(controller.continue_run(ContinueRequest::new(
        active.run_id(),
        request(32),
        digest("continue-32"),
        second_expected,
        "continue 32",
    )))
    .expect("middle retained replay");
    assert_eq!(middle_replay.run_id(), active.run_id());
    assert_eq!(executor.calls(), calls_before_retained_replay);
    assert_eq!(store.transactions(active_session), transactions_before_retained_replay);

    let recent_replay = futures::executor::block_on(controller.continue_run(ContinueRequest::new(
        active.run_id(),
        request(33),
        digest("continue-33"),
        third_expected,
        "continue 33",
    )))
    .expect("recent retained replay");
    assert_eq!(recent_replay.run_id(), active.run_id());
    assert_eq!(executor.calls(), calls_before_retained_replay);
    assert_eq!(store.transactions(active_session), transactions_before_retained_replay);

    let calls_before_occupied_start = executor.calls();
    let transactions_before_occupied_start = store.transactions(active_session);
    let occupied_start = futures::executor::block_on(
        controller.start(start("/active", 30, "active")),
    );
    assert!(matches!(
        occupied_start,
        Err(ego_core::run::RunError::IdentityCollision { .. })
    ));
    assert_eq!(executor.calls(), calls_before_occupied_start);
    assert_eq!(store.transactions(active_session), transactions_before_occupied_start);

    let calls_before_pruned_resubmit = executor.calls();
    let transactions_before_pruned_resubmit = store.transactions(active_session);
    let pruned_continuation = futures::executor::block_on(controller.continue_run(
        ContinueRequest::new(
            active.run_id(),
            request(31),
            digest("continue-31"),
            current_revision,
            "continue 31 again",
        ),
    ))
    .expect("pruned continuation is a new receipt at current revision");
    assert_eq!(pruned_continuation.run_id(), active.run_id());
    assert_eq!(
        store.transactions(active_session).len(),
        transactions_before_pruned_resubmit.len() + 1
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Submit(_)))
            .count(),
        calls_before_pruned_resubmit
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Submit(_)))
            .count()
            + 1
    );

    executor.block_effects();
    let blocked_expected = pruned_continuation.revision();
    let transactions_before_blocked = store.transactions(active_session);
    let submits_before_blocked = executor
        .calls()
        .iter()
        .filter(|call| matches!(call, ExecutorCall::Submit(_)))
        .count();
    let blocked_controller = Arc::clone(&controller);
    let blocked_run = active.run_id();
    let blocked = std::thread::scope(|scope| {
        let blocked_expected_for_worker = blocked_expected.clone();
        let worker = scope.spawn(move || {
            futures::executor::block_on(blocked_controller.continue_run(
                ContinueRequest::new(
                    blocked_run,
                    request(34),
                    digest("continue-34"),
                    blocked_expected_for_worker,
                    "continue 34",
                ),
            ))
        });
        executor.wait_for_submit_start();
        let transactions_after_admission = store.transactions(active_session);
        assert_eq!(
            transactions_after_admission.len(),
            transactions_before_blocked.len() + 1
        );
        let replay = futures::executor::block_on(controller.continue_run(
            ContinueRequest::new(
                active.run_id(),
                request(34),
                digest("continue-34"),
                blocked_expected,
                "continue 34",
            ),
        ))
        .expect("blocked continuation replay");
        assert_eq!(replay.run_id(), active.run_id());
        assert_eq!(store.transactions(active_session), transactions_after_admission);
        assert_eq!(
            executor
                .calls()
                .iter()
                .filter(|call| matches!(call, ExecutorCall::Submit(_)))
                .count(),
            submits_before_blocked + 1
        );
        assert_eq!(
            controller
                .status(active.run_id())
                .expect("active lease while submit is blocked")
                .status(),
            RunStatus::Running
        );
        executor.allow_effects();
        let settled = worker
            .join()
            .expect("blocked continuation task")
            .expect("blocked continuation settlement");
        (replay, settled, transactions_after_admission)
    });
    assert_eq!(blocked.0.run_id(), active.run_id());
    assert_eq!(blocked.1.run_id(), active.run_id());
    assert_eq!(
        store.transactions(active_session).len(),
        blocked.2.len() + 1
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Submit(_)))
            .count(),
        submits_before_blocked + 1
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Start(_)))
            .count(),
        1
    );
}

#[test]
fn dropping_a_subscription_detaches_only_and_does_not_cancel_the_run() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = futures::executor::block_on(controller.start(start("/a", 40, "a")))
        .expect("run admission");
    let subscription = admission.subscription();
    drop(subscription);
    assert_eq!(controller.status(admission.run_id()).expect("status").status(), RunStatus::Running);
    assert!(!executor.calls().contains(&ExecutorCall::Interrupt(admission.run_id())));
    assert_eq!(store.transactions(admission.session_id()).len(), 1);
}
