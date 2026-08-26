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
    CancelKey, CancelRequest, ContinueRequest, IdSource, RequestDigest, RequestId, RunAdmission,
    EventSinkFactory, RunAppend, RunCommandOutcome, RunController, RunExecution,
    RunExecutionBindings, RunExecutor, RunExecutorError, RunHandle, RunId, RunPreparation,
    RunRetention, RunRoot, RunStart, RunStatus, RunRevision, RunTransaction, StartRequest,
};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::sink::{EventSink, NullSink};
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
    Prepare,
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

struct PreparedToken(u64);

#[derive(Default)]
struct NullSinkFactory {
    creations: Mutex<usize>,
}

impl EventSinkFactory for NullSinkFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        *self.creations.lock().expect("sink factory creations") += 1;
        Arc::new(NullSink)
    }
}

#[derive(Default)]
struct SubmitGate {
    released: bool,
    interrupted: bool,
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
        gate.interrupted = false;
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
    type Prepared = PreparedToken;

    fn prepare<'a>(
        &'a self,
        _request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<Self::Prepared, RunExecutorError>> {
        self.sequence
            .lock()
            .expect("transaction sequence")
            .push("prepare");
        self.calls.lock().expect("executor calls").push(ExecutorCall::Prepare);
        Box::pin(future::ready(Ok(PreparedToken(1))))
    }

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
        prepared: Self::Prepared,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<ego_core::run::RunSettlement, RunExecutorError>> {
        let _prepared_serial = prepared.0;
        self.calls.lock().expect("executor calls").push(ExecutorCall::Submit(handle.run_id()));
        self.sequence.lock().expect("transaction sequence").push("submit");
        let gate = &self.submit_gate;
        let changed = &self.submit_gate_changed;
        Box::pin(futures::future::poll_fn(move |cx| {
            let mut gate = gate.lock().expect("executor gate");
            gate.started = true;
            changed.notify_all();
            if gate.interrupted {
                return Poll::Ready(Ok(ego_core::run::RunSettlement::cancelled(
                    ego_core::run::CancelReason::User,
                )));
            }
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
        let waker = {
            let mut gate = self.submit_gate.lock().expect("executor gate");
            gate.interrupted = true;
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
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
        body.to_owned(),
    )
}

type Controller = RunController<RecordingSessionStore, ScriptedRunExecutor, FixedClock, SequenceIds>;

fn controller(store: Arc<RecordingSessionStore>, executor: Arc<ScriptedRunExecutor>) -> Controller {
    RunController::new(
        store,
        executor,
        Arc::new(FixedClock::default()),
        Arc::new(SequenceIds::default()),
        RunRetention::new(NonZeroUsize::new(2).expect("non-zero retention"))
            .expect("retention"),
    )
}

fn bindings() -> RunExecutionBindings {
    RunExecutionBindings::new(Arc::new(NullSinkFactory::default()), None)
}

fn admitted_start(controller: &Controller, request: StartRequest) -> RunAdmission {
    match futures::executor::block_on(controller.start(
        request,
        bindings(),
    ))
    .expect("run command")
    {
        RunCommandOutcome::Admitted(admission) => admission,
        RunCommandOutcome::Replayed(_) => panic!("fresh start unexpectedly replayed"),
    }
}

fn execute(controller: &Controller, execution: RunExecution) {
    futures::executor::block_on(controller.execute(execution)).expect("run execution");
}

fn admitted_continue(
    controller: &Controller,
    request: ContinueRequest,
) -> RunAdmission {
    match futures::executor::block_on(controller.continue_run(
        request,
        bindings(),
    ))
    .expect("run command")
    {
        RunCommandOutcome::Admitted(admission) => admission,
        RunCommandOutcome::Replayed(_) => panic!("fresh continuation unexpectedly replayed"),
    }
}

fn start_command(
    controller: &Controller,
    request: StartRequest,
) -> Result<RunCommandOutcome, ego_core::run::RunError> {
    futures::executor::block_on(controller.start(
        request,
        bindings(),
    ))
}

fn continue_command(
    controller: &Controller,
    request: ContinueRequest,
) -> Result<RunCommandOutcome, ego_core::run::RunError> {
    futures::executor::block_on(controller.continue_run(
        request,
        bindings(),
    ))
}

fn replayed_run_id(outcome: RunCommandOutcome) -> RunId {
    match outcome {
        RunCommandOutcome::Replayed(observation) => observation.snapshot().run_id(),
        RunCommandOutcome::Admitted(_) => panic!("expected replay"),
    }
}

fn effect_calls(executor: &ScriptedRunExecutor) -> Vec<ExecutorCall> {
    executor
        .calls()
        .into_iter()
        .filter(|call| !matches!(call, ExecutorCall::Prepare))
        .collect()
}

#[test]
fn revision_one_is_atomically_published_before_any_executor_start() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(ScriptedRunExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/a", 1, "a"));
    let session = admission.session_id();
    let run = admission.run_id();
    assert_eq!(
        sequence
            .lock()
            .expect("transaction sequence")
            .as_slice(),
        ["prepare", "commit"]
    );
    assert_eq!(store.transactions(session).len(), 1);
    assert!(matches!(
        store.transactions(session)[0].command(),
        ego_core::run::RunCommand::CommandAdmitted { .. }
    ));
    assert!(effect_calls(&executor).is_empty());
    assert_eq!(admission.revision(), RunRevision::new(1));

    executor.allow_effects();
    execute(&controller, admission.into_execution());
    assert_eq!(
        sequence
            .lock()
            .expect("transaction sequence")
            .as_slice(),
        ["prepare", "commit", "start", "submit", "commit"]
    );
    assert_eq!(store.transactions(session).len(), 2);
    assert!(matches!(
        store.transactions(session)[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(store.transactions(session)[1].status(), RunStatus::Settled);
    assert_eq!(
        executor.calls(),
        vec![
            ExecutorCall::Prepare,
            ExecutorCall::Start(run),
            ExecutorCall::Submit(run),
        ]
    );
}

#[test]
fn start_derives_uuidv5_session_identity_and_mints_a_distinct_run_nonce() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(store, executor);
    let admission = admitted_start(&controller, start("/a", 2, "a"));
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
    let first = admitted_start(&controller, start("/a", 3, "a"));
    let replay = start_command(&controller, start("/a", 3, "a"))
        .expect("exact replay");
    assert_eq!(replayed_run_id(replay), first.run_id());
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Prepare))
            .count(),
        1
    );
    assert!(effect_calls(&executor).is_empty());
    assert!(matches!(
        start_command(&controller, start("/a", 3, "b")),
        Err(ego_core::run::RunError::RequestIdMismatch { .. })
    ));
    assert_eq!(store.transactions(first.session_id()).len(), 1);
    assert!(effect_calls(&executor).is_empty());

    let transactions_before_collision = store.transactions(first.session_id());
    let collision = start_command(&controller, start("/b", 3, "a"));
    assert!(matches!(collision, Err(ego_core::run::RunError::IdentityCollision { .. })));
    assert_eq!(store.transactions(first.session_id()), transactions_before_collision);
    assert!(effect_calls(&executor).is_empty());
}

#[test]
fn concurrent_same_request_has_one_winner_and_one_replay() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = Arc::new(controller(Arc::clone(&store), Arc::clone(&executor)));
    let left = Arc::clone(&controller);
    let right = Arc::clone(&controller);
    let (a, b) = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            start_command(&left, start("/a", 4, "a"))
        });
        let second = scope.spawn(|| {
            start_command(&right, start("/a", 4, "a"))
        });
        (first.join().expect("first start"), second.join().expect("second start"))
    });
    let first = a.expect("first start");
    let second = b.expect("second start");
    let (winner, replay) = match (&first, &second) {
        (RunCommandOutcome::Admitted(winner), RunCommandOutcome::Replayed(replay))
        | (RunCommandOutcome::Replayed(replay), RunCommandOutcome::Admitted(winner)) => {
            (winner, replay)
        }
        _ => panic!("concurrent starts did not produce one admission and one replay"),
    };
    assert_eq!(replay.snapshot().run_id(), winner.run_id());
    assert_eq!(store.transactions(winner.session_id()).len(), 1);
    assert!(effect_calls(&executor).is_empty());
}

#[test]
fn prepublication_failure_starts_no_executor_and_published_uncertain_starts_no_executor() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    store.fail_next(StoreFault::Reject);
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    assert!(start_command(&controller, start("/a", 5, "a")).is_err());
    assert!(store.transactions(RequestId::session_id(request(5))).is_empty());
    assert!(effect_calls(&executor).is_empty());

    store.fail_next(StoreFault::PublishedUncertain);
    let uncertain = start_command(&controller, start("/b", 6, "b"));
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
        futures::executor::block_on(controller.status(run_id))
            .expect("new authority snapshot")
            .status(),
        RunStatus::Uncertain
    );
    assert!(effect_calls(&executor).is_empty());
}

#[test]
fn continuation_and_cancel_commit_receipts_before_submit_or_interrupt_and_never_retarget() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(ScriptedRunExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = Arc::new(controller(Arc::clone(&store), Arc::clone(&executor)));
    let first = admitted_start(&controller, start("/a", 7, "a"));
    let run = first.run_id();
    let session = first.session_id();
    executor.allow_effects();
    execute(&controller, first.into_execution());
    let settled_start = futures::executor::block_on(controller.status(run)).expect("start status");
    assert_eq!(settled_start.status(), RunStatus::Settled);

    let next = admitted_continue(&controller, ContinueRequest::new(
        run,
        request(8),
        digest("b"),
        settled_start.revision(),
        "continue",
    ));
    let next_revision = next.revision();
    let cancel_key: CancelKey = next.cancel_key().clone();
    let execution = next.into_execution();
    let transactions = store.transactions(session);
    assert_eq!(transactions.len(), 3);
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert_eq!(transactions[2].revision(), RunRevision::new(3));
    assert!(matches!(
        transactions[2].command(),
        ego_core::run::RunCommand::CommandAdmitted { .. }
    ));
    assert!(matches!(
        transactions[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Submit(_)))
            .count(),
        1
    );
    assert_eq!(
        sequence.lock().expect("transaction sequence").as_slice(),
        ["prepare", "commit", "start", "submit", "commit", "prepare", "commit"]
    );

    executor.block_effects();
    let worker_controller = Arc::clone(&controller);
    let worker = std::thread::spawn(move || execute(&worker_controller, execution));
    executor.wait_for_submit_start();
    assert_eq!(
        executor.calls(),
        vec![
            ExecutorCall::Prepare,
            ExecutorCall::Start(run),
            ExecutorCall::Submit(run),
            ExecutorCall::Prepare,
            ExecutorCall::Submit(run),
        ]
    );
    assert_eq!(
        sequence.lock().expect("transaction sequence").as_slice(),
        [
            "prepare", "commit", "start", "submit", "commit", "prepare", "commit", "submit"
        ]
    );

    let cancelling = futures::executor::block_on(controller.cancel_admitted(&cancel_key))
        .expect("cancel admission");
    assert_eq!(cancelling.status(), RunStatus::Cancelling);
    assert_eq!(
        sequence.lock().expect("transaction sequence").as_slice(),
        [
            "prepare", "commit", "start", "submit", "commit", "prepare", "commit", "submit",
            "commit", "interrupt"
        ]
    );
    worker.join().expect("cancelled execution");
    assert_eq!(
        sequence.lock().expect("transaction sequence").as_slice(),
        [
            "prepare", "commit", "start", "submit", "commit", "prepare", "commit", "submit",
            "commit", "interrupt", "commit"
        ]
    );
    let final_status = futures::executor::block_on(controller.status(run)).expect("final status");
    assert_eq!(final_status.status(), RunStatus::Cancelled);
    let transactions = store.transactions(session);
    assert_eq!(transactions.len(), 6);
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert_eq!(transactions[2].revision(), RunRevision::new(3));
    assert_eq!(transactions[3].revision(), RunRevision::new(4));
    assert_eq!(transactions[4].revision(), RunRevision::new(5));
    assert_eq!(transactions[5].revision(), RunRevision::new(6));
    assert!(matches!(
        transactions[4].command(),
        ego_core::run::RunCommand::CancelRequested { .. }
    ));
    assert!(matches!(
        transactions[3].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert!(matches!(
        transactions[5].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(transactions[0].status(), RunStatus::Running);
    assert_eq!(transactions[1].status(), RunStatus::Settled);
    assert_eq!(transactions[2].status(), RunStatus::Running);
    assert_eq!(transactions[3].status(), RunStatus::Settled);
    assert_eq!(transactions[4].status(), RunStatus::Cancelling);
    assert_eq!(transactions[5].status(), RunStatus::Cancelled);
    assert_eq!(executor.calls(), vec![
        ExecutorCall::Prepare,
        ExecutorCall::Start(run),
        ExecutorCall::Submit(run),
        ExecutorCall::Prepare,
        ExecutorCall::Submit(run),
        ExecutorCall::Interrupt(run),
    ]);

    let calls_after_cancel = executor.calls();
    assert!(futures::executor::block_on(controller.cancel(CancelRequest::new(
        run,
        request(10),
        digest("stale"),
        next_revision,
    )))
    .is_err());
    assert_eq!(store.transactions(session).len(), 6);
    assert_eq!(executor.calls(), calls_after_cancel);
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
        let first = scope.spawn(|| admitted_start(&left, start("/one", 11, "a")));
        let second = scope.spawn(|| admitted_start(&right, start("/two", 12, "b")));
        (
            first.join().expect("first admission"),
            second.join().expect("second admission"),
        )
    });
    let first_run = first.run_id();
    let second_run = second.run_id();
    let first_root = first.root();
    let second_root = second.root();
    assert_ne!(first_run, second_run);
    assert!(continue_command(&controller, ContinueRequest::new(
        first_run,
        request(13),
        digest("busy"),
        first.revision(),
        "blocked",
    ))
    .is_err());

    executor.allow_effects();
    let first_execution = first.into_execution();
    let second_execution = second.into_execution();
    let first_controller = Arc::clone(&controller);
    let second_controller = Arc::clone(&controller);
    std::thread::scope(|scope| {
        let first = scope.spawn(move || execute(&first_controller, first_execution));
        let second = scope.spawn(move || execute(&second_controller, second_execution));
        first.join().expect("first execution");
        second.join().expect("second execution");
    });
    let started_roots = executor.started_roots();
    assert_eq!(started_roots.len(), 2);
    assert!(started_roots.iter().any(|(run, root)| {
        *run == first_run && root == &first_root
    }));
    assert!(started_roots.iter().any(|(run, root)| {
        *run == second_run && root == &second_root
    }));
    assert_eq!(
        futures::executor::block_on(controller.status(second_run))
            .expect("status")
            .root(),
        second_root
    );
}

fn recovery_controller() -> (
    RunController<RecordingSessionStore, ScriptedRunExecutor, FixedClock, SequenceIds>,
    Arc<RecordingSessionStore>,
    Arc<ScriptedRunExecutor>,
    RunId,
) {
    let store = Arc::new(RecordingSessionStore::default());
    let first_executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&first_executor));
    let admission = admitted_start(&first_controller, start("/recover", 20, "recovery"));
    let run_id = admission.run_id();
    drop(admission);
    drop(first_controller);
    drop(first_executor);

    let executor = Arc::new(ScriptedRunExecutor::default());
    (
        controller(Arc::clone(&store), Arc::clone(&executor)),
        store,
        executor,
        run_id,
    )
}

#[test]
fn recovery_running_reattaches_live_execution() {
    let (controller, store, executor, run) = recovery_controller();
    let calls_before_status = executor.calls();
    let status_before_recovery = futures::executor::block_on(controller.status(run))
        .expect("running status before explicit recovery");
    assert_eq!(status_before_recovery.status(), RunStatus::Running);
    assert_eq!(executor.calls(), calls_before_status);
    assert_eq!(store.transactions(run.session_id()).len(), 1);
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::Running(
        RunHandle::new(run),
    )));
    assert_eq!(store.transactions(run.session_id()).len(), 1);
    futures::executor::block_on(controller.recover(run)).expect("running recovery");
    let status = futures::executor::block_on(controller.status(run)).expect("running status");
    assert_eq!(status.status(), RunStatus::Running);
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
    assert_eq!(store.transactions(run.session_id()).len(), 1);
}

#[test]
fn recovery_terminal_preserves_terminal_settlement() {
    let (controller, store, executor, run) = recovery_controller();
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::Terminal));
    assert_eq!(store.transactions(run.session_id())[0].status(), RunStatus::Running);
    futures::executor::block_on(controller.recover(run)).expect("terminal recovery");
    assert_eq!(
        futures::executor::block_on(controller.status(run))
            .expect("terminal status")
            .status(),
        RunStatus::Settled
    );
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
    assert_eq!(store.transactions(run.session_id()).len(), 2);
    assert!(matches!(
        store.transactions(run.session_id())[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(store.transactions(run.session_id())[1].status(), RunStatus::Settled);
}

#[test]
fn recovery_cancelled_before_start_settles_interrupted() {
    let (controller, store, executor, run) = recovery_controller();
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::CancelledBeforeStart));
    assert_eq!(store.transactions(run.session_id())[0].status(), RunStatus::Running);
    futures::executor::block_on(controller.recover(run)).expect("interrupted recovery");
    assert_eq!(
        futures::executor::block_on(controller.status(run))
            .expect("interrupted status")
            .status(),
        RunStatus::Interrupted
    );
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
    assert_eq!(store.transactions(run.session_id()).len(), 2);
    assert!(matches!(
        store.transactions(run.session_id())[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(store.transactions(run.session_id())[1].status(), RunStatus::Interrupted);
}

#[test]
fn recovery_ambiguous_external_effect_is_uncertain_and_not_retried() {
    let (controller, store, executor, run) = recovery_controller();
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::Ambiguous));
    assert_eq!(store.transactions(run.session_id())[0].status(), RunStatus::Running);
    futures::executor::block_on(controller.recover(run)).expect("uncertain recovery");
    assert_eq!(
        futures::executor::block_on(controller.status(run))
            .expect("uncertain status")
            .status(),
        RunStatus::Uncertain
    );
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
    assert!(futures::executor::block_on(controller.status(run))
        .expect("stable uncertain status")
        .blocks_new_turn());
    assert_eq!(store.transactions(run.session_id()).len(), 2);
    assert!(matches!(
        store.transactions(run.session_id())[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(store.transactions(run.session_id())[1].status(), RunStatus::Uncertain);
}

#[test]
fn recovery_of_an_already_terminal_durable_run_never_calls_recover() {
    let store = Arc::new(RecordingSessionStore::default());
    let first_executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&first_executor));
    let admission = admitted_start(
        &first_controller,
        start("/already-terminal", 21, "terminal"),
    );
    let run = admission.run_id();
    first_executor.allow_effects();
    execute(&first_controller, admission.into_execution());
    let start_status = futures::executor::block_on(first_controller.status(run))
        .expect("initial terminal status");
    let settled = admitted_continue(
        &first_controller,
        ContinueRequest::new(
            run,
            request(22),
            digest("terminal-settlement"),
            start_status.revision(),
            "settle",
        ),
    );
    execute(&first_controller, settled.into_execution());
    assert_eq!(
        futures::executor::block_on(first_controller.status(run))
            .expect("terminal status")
            .status(),
        RunStatus::Settled
    );
    drop(first_controller);
    drop(first_executor);

    let fresh_executor = Arc::new(ScriptedRunExecutor::default());
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&fresh_executor));
    assert_eq!(
        futures::executor::block_on(fresh_controller.status(run))
            .expect("terminal replay")
            .status(),
        RunStatus::Settled
    );
    assert!(fresh_executor.calls().is_empty());
}

#[test]
fn retention_prunes_completed_receipts_but_preserves_active_work_across_replay() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(RecordingSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(ScriptedRunExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = Arc::new(controller(Arc::clone(&store), Arc::clone(&executor)));
    let active = admitted_start(&controller, start("/active", 30, "active"));
    let active_run = active.run_id();
    let active_session = active.session_id();
    executor.allow_effects();
    execute(&controller, active.into_execution());
    let active_status = futures::executor::block_on(controller.status(active_run))
        .expect("active status");
    assert_eq!(active_status.status(), RunStatus::Settled);

    let active_replay = start_command(&controller, start("/active", 30, "active"))
        .expect("active exact replay");
    assert_eq!(replayed_run_id(active_replay), active_run);
    assert_eq!(store.transactions(active_session).len(), 2);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Start(_)))
            .count(),
        1
    );

    let first = admitted_continue(&controller, ContinueRequest::new(
        active_run,
        request(31),
        digest("continue-31"),
        active_status.revision(),
        "continue 31",
    ));
    execute(&controller, first.into_execution());
    let first_status = futures::executor::block_on(controller.status(active_run))
        .expect("first continuation status");

    let second = admitted_continue(&controller, ContinueRequest::new(
        active_run,
        request(32),
        digest("continue-32"),
        first_status.revision(),
        "continue 32",
    ));
    execute(&controller, second.into_execution());
    let second_status = futures::executor::block_on(controller.status(active_run))
        .expect("second continuation status");

    let third = admitted_continue(&controller, ContinueRequest::new(
        active_run,
        request(33),
        digest("continue-33"),
        second_status.revision(),
        "continue 33",
    ));
    execute(&controller, third.into_execution());
    let third_status = futures::executor::block_on(controller.status(active_run))
        .expect("third continuation status");
    let current_revision = third_status.revision();

    let calls_before_retained_replay = executor.calls();
    let transactions_before_retained_replay = store.transactions(active_session);
    let middle_replay = continue_command(&controller, ContinueRequest::new(
        active_run,
        request(32),
        digest("continue-32"),
        first_status.revision(),
        "continue 32",
    ))
    .expect("middle retained replay");
    assert_eq!(replayed_run_id(middle_replay), active_run);
    assert_eq!(executor.calls(), calls_before_retained_replay);
    assert_eq!(store.transactions(active_session), transactions_before_retained_replay);

    let recent_replay = continue_command(&controller, ContinueRequest::new(
        active_run,
        request(33),
        digest("continue-33"),
        second_status.revision(),
        "continue 33",
    ))
    .expect("recent retained replay");
    assert_eq!(replayed_run_id(recent_replay), active_run);
    assert_eq!(executor.calls(), calls_before_retained_replay);
    assert_eq!(store.transactions(active_session), transactions_before_retained_replay);

    let calls_before_occupied_start = executor.calls();
    let transactions_before_occupied_start = store.transactions(active_session);
    let sequence_before_occupied_start = sequence.lock().expect("transaction sequence").clone();
    let occupied_start = start_command(&controller, start("/active", 30, "active"));
    assert!(matches!(
        occupied_start,
        Err(ego_core::run::RunError::IdentityCollision { .. })
    ));
    assert_eq!(executor.calls(), calls_before_occupied_start);
    assert_eq!(store.transactions(active_session), transactions_before_occupied_start);
    assert_eq!(sequence.lock().expect("transaction sequence").as_slice(), sequence_before_occupied_start.as_slice());

    let calls_before_pruned_resubmit = executor.calls();
    let transactions_before_pruned_resubmit = store.transactions(active_session);
    let pruned_continuation = admitted_continue(&controller,
        ContinueRequest::new(
            active_run,
            request(31),
            digest("continue-31"),
            current_revision,
            "continue 31 again",
        ),
    );
    assert_eq!(pruned_continuation.run_id(), active_run);
    assert_eq!(
        store.transactions(active_session).len(),
        transactions_before_pruned_resubmit.len() + 1
    );
    assert_eq!(executor.calls(), calls_before_pruned_resubmit);
    execute(&controller, pruned_continuation.into_execution());
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
    assert_eq!(
        store.transactions(active_session).len(),
        transactions_before_pruned_resubmit.len() + 2
    );
    let after_pruned_status = futures::executor::block_on(controller.status(active_run))
        .expect("pruned continuation status");
    assert_eq!(after_pruned_status.status(), RunStatus::Settled);

    executor.block_effects();
    let blocked_expected = after_pruned_status.revision();
    let transactions_before_blocked = store.transactions(active_session);
    let submits_before_blocked = executor
        .calls()
        .iter()
        .filter(|call| matches!(call, ExecutorCall::Submit(_)))
        .count();
    let blocked_controller = Arc::clone(&controller);
    let blocked_run = active_run;
    let blocked = std::thread::scope(|scope| {
        let blocked_expected_for_worker = blocked_expected.clone();
        let worker = scope.spawn(move || {
            let admission = admitted_continue(
                &blocked_controller,
                ContinueRequest::new(
                    blocked_run,
                    request(34),
                    digest("continue-34"),
                    blocked_expected_for_worker,
                    "continue 34",
                ),
            );
            let run = admission.run_id();
            execute(&blocked_controller, admission.into_execution());
            run
        });
        executor.wait_for_submit_start();
        let transactions_after_admission = store.transactions(active_session);
        assert_eq!(
            transactions_after_admission.len(),
            transactions_before_blocked.len() + 1
        );
        let sequence_after_admission = sequence.lock().expect("transaction sequence").clone();
        let replay = continue_command(&controller,
            ContinueRequest::new(
                active_run,
                request(34),
                digest("continue-34"),
                blocked_expected,
                "continue 34",
            ),
        )
        .expect("blocked continuation replay");
        assert_eq!(replayed_run_id(replay), active_run);
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
            sequence.lock().expect("transaction sequence").as_slice(),
            sequence_after_admission.as_slice()
        );
        assert_eq!(
            futures::executor::block_on(controller.status(active_run))
                .expect("active lease while submit is blocked")
                .status(),
            RunStatus::Running
        );
        executor.allow_effects();
        let settled_run = worker
            .join()
            .expect("blocked continuation task");
        (settled_run, transactions_after_admission)
    });
    assert_eq!(blocked.0, active_run);
    assert_eq!(
        store.transactions(active_session).len(),
        blocked.1.len() + 1
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
        futures::executor::block_on(controller.status(active_run))
            .expect("settled blocked continuation")
            .status(),
        RunStatus::Settled
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
    let admission = admitted_start(&controller, start("/a", 40, "a"));
    let run = admission.run_id();
    let session = admission.session_id();
    let subscription = admission.subscription();
    drop(subscription);
    assert_eq!(
        futures::executor::block_on(controller.status(run))
            .expect("status before execution")
            .status(),
        RunStatus::Running
    );
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    assert_eq!(
        futures::executor::block_on(controller.status(run))
            .expect("status after execution")
            .status(),
        RunStatus::Settled
    );
    assert!(matches!(
        store.transactions(session)[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert!(!executor.calls().contains(&ExecutorCall::Interrupt(run)));
    assert_eq!(store.transactions(session).len(), 2);
}
