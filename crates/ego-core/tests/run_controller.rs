//! contract for regression case's rooted durable RunController.
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

use ego_core::ceiling::Budget;
use ego_core::config::{Mode, ModelRef};
use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::run::{
    CancelKey, CancelRequest, ContinueRequest, EventSinkFactory, IdSource, RequestDigest,
    RequestId, RunAdmission, RunAppend, RunAttachment, RunCommandOutcome, RunController,
    RunExecution, RunExecutionBindings, RunExecutor, RunExecutorError, RunHandle, RunId,
    RunPreparation, RunPrepared, RunPrompt, RunRecoveryOutcome, RunRetention, RunRevision, RunRoot,
    RunSelections, RunSettlement, RunStatus, RunTransaction, StartRequest,
};
use ego_core::sink::{EventSink, NullSink};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};

#[path = "run_controller/readmission.rs"]
mod readmission;

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
                        SessionEntry::Event(_)
                        | SessionEntry::Evidence(_)
                        | SessionEntry::NoticeDelivered(_)
                        | SessionEntry::ModelSelected(_) => None,
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
                profiles: Default::default(),
                id: *id,
                workspace_root: PathBuf::from("/workspace"),
                model: None,
                created: Timestamp::EPOCH,
                updated: Timestamp::EPOCH,
                title: None,
                summary: None,
                summary_failures: 0,
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
                StoreFault::Reject => {
                    return Err(StoreError::Io(std::io::Error::other("rejected")));
                }
                StoreFault::PublishedUncertain => {
                    self.sequence
                        .lock()
                        .expect("transaction sequence")
                        .push("publish");
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
        let current = entries.get(session).and_then(|entries| {
            entries.iter().rev().find_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                SessionEntry::Event(_)
                | SessionEntry::Evidence(_)
                | SessionEntry::NoticeDelivered(_)
                | SessionEntry::ModelSelected(_) => None,
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
        self.sequence
            .lock()
            .expect("transaction sequence")
            .push("commit");
        Ok(RunAppend::Committed {
            revision: current.map_or_else(|| RunRevision::new(1), RunRevision::next),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecutorCall {
    Prepare,
    Attach(RunId),
    Recover(RunId),
    Observe(RunId),
    Submit(RunId),
    Interrupt(RunId),
    Pause(RunId),
}

struct ScriptedRunExecutor {
    execution_observation: Mutex<Option<ego_core::permit::ExecObservation>>,
    calls: Mutex<Vec<ExecutorCall>>,
    attachments: Mutex<VecDeque<Result<RunHandle, RunExecutorError>>>,
    recoveries: Mutex<VecDeque<Result<ego_core::run::RecoveryEvidence, RunExecutorError>>>,
    observations: Mutex<VecDeque<Result<RunSettlement, RunExecutorError>>>,
    submissions: Mutex<VecDeque<Result<RunSettlement, RunExecutorError>>>,
    interrupts: Mutex<VecDeque<Result<(), RunExecutorError>>>,
    next_prepared: Mutex<u64>,
    prepared_models: Mutex<VecDeque<ModelRef>>,
    prepared_selections: Mutex<Vec<RunSelections>>,
    prepared_tokens: Mutex<Vec<u64>>,
    bindings_received: Mutex<usize>,
    submit_gate: Mutex<SubmitGate>,
    submit_gate_changed: Condvar,
    started_roots: Mutex<Vec<(RunId, RunRoot)>>,
    started_models: Mutex<Vec<(RunId, ModelRef)>>,
    attached_origins: Mutex<Vec<ego_core::run::RunOrigin>>,
    authorities: Mutex<Vec<Arc<RecordingAuthority>>>,
    start_barrier: Option<Arc<Barrier>>,
    sequence: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Default)]
struct RecordingAuthority {
    _observations: Arc<Mutex<Vec<RunId>>>,
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
            execution_observation: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            attachments: Mutex::new(VecDeque::new()),
            recoveries: Mutex::new(VecDeque::new()),
            observations: Mutex::new(VecDeque::new()),
            submissions: Mutex::new(VecDeque::new()),
            interrupts: Mutex::new(VecDeque::new()),
            next_prepared: Mutex::new(0),
            prepared_models: Mutex::new(VecDeque::new()),
            prepared_selections: Mutex::new(Vec::new()),
            prepared_tokens: Mutex::new(Vec::new()),
            bindings_received: Mutex::new(0),
            submit_gate: Mutex::new(SubmitGate::default()),
            submit_gate_changed: Condvar::new(),
            started_roots: Mutex::new(Vec::new()),
            started_models: Mutex::new(Vec::new()),
            attached_origins: Mutex::new(Vec::new()),
            authorities: Mutex::new(Vec::new()),
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

    fn with_start_barrier(sequence: Arc<Mutex<Vec<&'static str>>>, barrier: Arc<Barrier>) -> Self {
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

    fn attachment_result(&self, result: Result<RunHandle, RunExecutorError>) {
        self.attachments
            .lock()
            .expect("executor attachments")
            .push_back(result);
    }

    fn observation_result(&self, result: Result<RunSettlement, RunExecutorError>) {
        self.observations
            .lock()
            .expect("executor observations")
            .push_back(result);
    }

    fn submission_result(&self, result: Result<RunSettlement, RunExecutorError>) {
        self.submissions
            .lock()
            .expect("executor submissions")
            .push_back(result);
    }

    fn interrupt_result(&self, result: Result<(), RunExecutorError>) {
        self.interrupts
            .lock()
            .expect("executor interrupts")
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
        self.started_roots.lock().expect("executor roots").clone()
    }

    fn prepared_tokens(&self) -> Vec<u64> {
        self.prepared_tokens
            .lock()
            .expect("prepared tokens")
            .clone()
    }

    fn prepare_model(&self, model: ModelRef) {
        self.prepared_models
            .lock()
            .expect("prepared models")
            .push_back(model);
    }

    fn prepared_requests(&self) -> Vec<Option<ModelRef>> {
        self.prepared_selections
            .lock()
            .expect("prepared selections")
            .iter()
            .map(|selections| selections.model().cloned())
            .collect()
    }

    fn started_models(&self) -> Vec<(RunId, ModelRef)> {
        self.started_models.lock().expect("started models").clone()
    }

    fn attached_origins(&self) -> Vec<ego_core::run::RunOrigin> {
        self.attached_origins
            .lock()
            .expect("attached origins")
            .clone()
    }

    fn authority_observations(&self) -> Vec<RunId> {
        self.authorities
            .lock()
            .expect("executor authorities")
            .iter()
            .flat_map(|authority| {
                authority
                    ._observations
                    .lock()
                    .expect("authority observations")
                    .clone()
            })
            .collect()
    }

    fn bindings_received(&self) -> usize {
        *self.bindings_received.lock().expect("execution bindings")
    }
}

impl RunExecutor for ScriptedRunExecutor {
    type Prepared = PreparedToken;

    fn request_execution_policy_permission<'a>(
        &'a self,
        _prepared: &'a mut Self::Prepared,
        question: ego_core::permit::PermissionRequest,
        bindings: &'a RunExecutionBindings,
    ) -> BoxFuture<'a, Result<bool, RunExecutorError>> {
        Box::pin(async move {
            let Some(human) = bindings.human() else {
                return Ok(false);
            };
            Ok(matches!(
                human
                    .decide(ego_core::event::id::ItemId::new(), &question)
                    .await,
                ego_core::permit::PermissionDecision::Allow
            ))
        })
    }

    fn execution_observation<'a>(
        &'a self,
        _prepared: &'a mut Self::Prepared,
    ) -> BoxFuture<'a, Result<Option<ego_core::permit::ExecObservation>, RunExecutorError>> {
        Box::pin(future::ready(Ok(self
            .execution_observation
            .lock()
            .unwrap()
            .clone())))
    }

    fn admit_run_context<'a>(
        &'a self,
        _prepared: &'a Self::Prepared,
    ) -> ego_core::run::RunFuture<
        'a,
        Result<ego_core::evidence::EvidenceWrite, ego_core::evidence::EvidenceError>,
    > {
        Box::pin(future::ready(Ok(
            ego_core::evidence::EvidenceWrite::Published,
        )))
    }

    fn prepare<'a>(
        &'a self,
        request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        self.prepared_selections
            .lock()
            .expect("prepared selections")
            .push(request.selections.clone());
        self.sequence
            .lock()
            .expect("transaction sequence")
            .push("prepare");
        self.calls
            .lock()
            .expect("executor calls")
            .push(ExecutorCall::Prepare);
        let mut next = self.next_prepared.lock().expect("prepared sequence");
        *next += 1;
        let model = self
            .prepared_models
            .lock()
            .expect("prepared models")
            .pop_front()
            .unwrap_or_else(|| ModelRef("model/test".to_owned()));
        Box::pin(future::ready(Ok(RunPrepared::new(
            model,
            PreparedToken(*next),
        ))))
    }

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        let run = request.run_id;
        if let Some(barrier) = self.start_barrier.as_ref() {
            barrier.wait();
        }
        self.started_roots
            .lock()
            .expect("executor roots")
            .push((run, request.root.clone()));
        self.started_models
            .lock()
            .expect("started models")
            .push((run, request.model.clone()));
        self.attached_origins
            .lock()
            .expect("attached origins")
            .push(request.origin.clone());
        self.sequence
            .lock()
            .expect("transaction sequence")
            .push("attach");
        self.calls
            .lock()
            .expect("executor calls")
            .push(ExecutorCall::Attach(run));
        let authority = Arc::new(RecordingAuthority::default());
        self.authorities
            .lock()
            .expect("executor authorities")
            .push(Arc::clone(&authority));
        let result = self
            .attachments
            .lock()
            .expect("executor attachments")
            .pop_front()
            .unwrap_or_else(|| Ok(RunHandle::new(run, authority)));
        Box::pin(future::ready(result))
    }

    fn recover<'a>(
        &'a self,
        run: &'a RunId,
    ) -> BoxFuture<'a, Result<ego_core::run::RecoveryEvidence, RunExecutorError>> {
        self.calls
            .lock()
            .expect("executor calls")
            .push(ExecutorCall::Recover(*run));
        let result = self
            .recoveries
            .lock()
            .expect("executor recoveries")
            .pop_front()
            .unwrap_or(Ok(ego_core::run::RecoveryEvidence::Ambiguous));
        Box::pin(future::ready(result))
    }

    fn observe<'a>(
        &'a self,
        handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.calls
            .lock()
            .expect("executor calls")
            .push(ExecutorCall::Observe(handle.run_id()));
        let authority = handle
            .authority::<Arc<RecordingAuthority>>()
            .expect("recovered handle authority");
        authority
            ._observations
            .lock()
            .expect("authority observations")
            .push(handle.run_id());
        *self.bindings_received.lock().expect("execution bindings") += 1;
        let result = self
            .observations
            .lock()
            .expect("executor observations")
            .pop_front()
            .unwrap_or_else(|| Ok(RunSettlement::completed()));
        Box::pin(future::ready(result))
    }

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        _prompt: &'a RunPrompt,
        prepared: Self::Prepared,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<ego_core::run::RunSettlement, RunExecutorError>> {
        self.prepared_tokens
            .lock()
            .expect("prepared tokens")
            .push(prepared.0);
        let authority = handle
            .authority::<Arc<RecordingAuthority>>()
            .expect("started handle authority");
        authority
            ._observations
            .lock()
            .expect("authority observations")
            .push(handle.run_id());
        *self.bindings_received.lock().expect("execution bindings") += 1;
        self.calls
            .lock()
            .expect("executor calls")
            .push(ExecutorCall::Submit(handle.run_id()));
        self.sequence
            .lock()
            .expect("transaction sequence")
            .push("submit");
        let mut scripted = self
            .submissions
            .lock()
            .expect("executor submissions")
            .pop_front();
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
                return Poll::Ready(
                    scripted
                        .take()
                        .unwrap_or_else(|| Ok(ego_core::run::RunSettlement::completed())),
                );
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
        self.sequence
            .lock()
            .expect("transaction sequence")
            .push("interrupt");
        let waker = {
            let mut gate = self.submit_gate.lock().expect("executor gate");
            gate.interrupted = true;
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        let result = self
            .interrupts
            .lock()
            .expect("executor interrupts")
            .pop_front()
            .unwrap_or(Ok(()));
        Box::pin(future::ready(result))
    }

    /// Recorded and not gated, unlike `interrupt`: a pause does not abandon
    /// the effect in flight, so there is no submit gate for it to release.
    fn pause<'a>(
        &'a self,
        handle: &'a RunHandle,
        _request: RequestId,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        self.calls
            .lock()
            .expect("executor calls")
            .push(ExecutorCall::Pause(handle.run_id()));
        self.sequence
            .lock()
            .expect("transaction sequence")
            .push("pause");
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
        self.now
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

fn fixture_root(name: &str) -> RunRoot {
    let name = name.strip_prefix('/').unwrap_or(name);
    RunRoot::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/run-controller")
            .join(name),
    )
    .expect("fixture run root is absolute")
}

fn start(root: &str, value: u128, body: &str) -> StartRequest {
    StartRequest::new(
        RequestId::session_id(request(value)),
        fixture_root(root),
        request(value),
        digest(body),
        selections(),
        body.to_owned(),
    )
}

fn selections() -> RunSelections {
    RunSelections::new(
        None,
        Some(ego_core::generator::types::ThinkingLevel::Low),
        Some(Mode::Default),
        Some("40k".parse::<Budget>().expect("typed budget")),
        Some("ci".to_owned()),
    )
}

#[test]
fn selections_are_persisted_folded_and_pin_the_continuation_model() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let request = StartRequest::new(
        RequestId::session_id(request_id(58)),
        fixture_root("selected"),
        request_id(58),
        digest("selected"),
        selections_with_model(),
        "selected",
    );
    let admission = admitted_start(&controller, request);
    let session = admission.session_id();
    let run = admission.run_id();
    let transaction = store.transactions(session).remove(0);
    assert_eq!(transaction.selections(), &selections_with_model());
    let encoded = serde_json::to_value(&transaction).expect("transaction encoding");
    assert_eq!(
        encoded["selections"],
        serde_json::to_value(selections_with_model()).expect("selection encoding")
    );
    assert_eq!(admission.snapshot().selections(), &selections_with_model());
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    let status = futures::executor::block_on(controller.status(run)).expect("status");
    let continuation = admitted_continue(
        &controller,
        ContinueRequest::new(
            run,
            request_id(59),
            digest("next"),
            status.revision(),
            "next",
        ),
    );
    assert_eq!(
        continuation.snapshot().selections(),
        &selections_with_model()
    );
}

#[test]
fn explicit_durable_session_target_is_independent_of_request_identity() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let command = request_id(5_800);
    let target: SessionId = "00000000-0000-0000-0000-000000005801"
        .parse()
        .expect("session id");
    assert_ne!(target, RequestId::session_id(command));
    let request = StartRequest::new(
        target,
        fixture_root("explicit"),
        command,
        digest("explicit target"),
        selections(),
        "prompt",
    );

    let admission = admitted_start(&controller, request);

    assert_eq!(admission.session_id(), target);
    assert_eq!(
        futures::executor::block_on(controller.status_by_session(target))
            .expect("status by durable session")
            .session_id(),
        target
    );
}

fn request_id(value: u128) -> RequestId {
    RequestId::from_uuid(uuid::Uuid::from_u128(value))
}

fn selections_with_model() -> RunSelections {
    RunSelections::new(
        Some(model_ref("model/test")),
        Some(ego_core::generator::types::ThinkingLevel::High),
        Some(Mode::Edits),
        Some("80k".parse::<Budget>().expect("typed budget")),
        Some("ci".to_owned()),
    )
}

fn model_ref(value: &str) -> ModelRef {
    ModelRef(value.to_owned())
}

type Controller =
    RunController<RecordingSessionStore, ScriptedRunExecutor, FixedClock, SequenceIds>;

fn controller(store: Arc<RecordingSessionStore>, executor: Arc<ScriptedRunExecutor>) -> Controller {
    RunController::new(
        store,
        executor,
        Arc::new(FixedClock::default()),
        Arc::new(SequenceIds::default()),
        RunRetention::new(NonZeroUsize::new(2).expect("non-zero retention")).expect("retention"),
    )
}

fn bindings() -> RunExecutionBindings {
    RunExecutionBindings::new(Arc::new(NullSinkFactory::default()), None)
}

fn admitted_start(controller: &Controller, request: StartRequest) -> RunAdmission {
    match futures::executor::block_on(controller.start(request, bindings())).expect("run command") {
        RunCommandOutcome::Admitted(admission) => admission,
        RunCommandOutcome::Replayed(_) => panic!("fresh start unexpectedly replayed"),
    }
}

fn execute(controller: &Controller, execution: RunExecution) {
    futures::executor::block_on(controller.execute(execution)).expect("run execution");
}

fn admitted_continue(controller: &Controller, request: ContinueRequest) -> RunAdmission {
    match futures::executor::block_on(controller.continue_run(request, bindings()))
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
    futures::executor::block_on(controller.start(request, bindings()))
}

fn continue_command(
    controller: &Controller,
    request: ContinueRequest,
) -> Result<RunCommandOutcome, ego_core::run::RunError> {
    futures::executor::block_on(controller.continue_run(request, bindings()))
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

fn rewrite_transaction(
    store: &RecordingSessionStore,
    session: SessionId,
    index: usize,
    mutate: impl FnOnce(&mut serde_json::Value),
) {
    let mut entries = store.entries.lock().expect("session entries");
    let entry = entries
        .get_mut(&session)
        .and_then(|entries| entries.get_mut(index))
        .expect("transaction fixture entry");
    let SessionEntry::RunTransaction(transaction) = entry else {
        panic!("transaction fixture entry was an event");
    };
    let mut value = serde_json::to_value(&*transaction).expect("transaction serialises");
    mutate(&mut value);
    *transaction = serde_json::from_value(value).expect("mutated transaction remains valid");
}

fn assert_corrupt_run_is_rejected_everywhere(
    controller: &Controller,
    executor: &ScriptedRunExecutor,
    store: &RecordingSessionStore,
    run: RunId,
    session: SessionId,
    request_id: u128,
    expected_revision: RunRevision,
) {
    let calls_before = executor.calls();
    let transactions_before = store.transactions(session);
    assert!(matches!(
        futures::executor::block_on(controller.status(run)),
        Err(ego_core::run::RunError::Integrity { .. })
    ));
    assert!(matches!(
        futures::executor::block_on(controller.continue_run(
            ContinueRequest::new(
                run,
                request(request_id),
                digest("corrupt-fold"),
                expected_revision,
                "corrupt fold",
            ),
            bindings(),
        )),
        Err(ego_core::run::RunError::Integrity { .. })
    ));
    assert!(matches!(
        futures::executor::block_on(controller.recover(run, bindings())),
        Err(ego_core::run::RunError::Integrity { .. })
    ));
    assert_eq!(store.transactions(session), transactions_before);
    assert_eq!(executor.calls(), calls_before);
}

#[test]
fn run_prepared_exposes_resolved_model_and_opaque_capability_parts() {
    let prepared = RunPrepared::new(model_ref("model/test"), PreparedToken(7));
    assert_eq!(prepared.model(), &model_ref("model/test"));
    let (model, capability) = prepared.into_parts();
    assert_eq!(model, model_ref("model/test"));
    assert_eq!(capability.0, 7);
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
        sequence.lock().expect("transaction sequence").as_slice(),
        ["prepare", "commit"]
    );
    assert_eq!(store.transactions(session).len(), 1);
    assert!(matches!(
        store.transactions(session)[0].command(),
        ego_core::run::RunCommand::CommandAdmitted { .. }
    ));
    assert!(matches!(
        store.transactions(session)[0].command(),
        ego_core::run::RunCommand::CommandAdmitted { model, .. }
            if model == &model_ref("model/test")
    ));
    assert!(effect_calls(&executor).is_empty());
    assert_eq!(admission.revision(), RunRevision::new(1));
    assert_eq!(executor.prepared_requests(), vec![None]);

    executor.allow_effects();
    execute(&controller, admission.into_execution());
    assert_eq!(
        sequence.lock().expect("transaction sequence").as_slice(),
        ["prepare", "commit", "attach", "submit", "commit"]
    );
    assert_eq!(store.transactions(session).len(), 2);
    assert!(matches!(
        store.transactions(session)[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(store.transactions(session)[1].status(), RunStatus::Settled);
    assert_eq!(executor.prepared_tokens(), vec![1]);
    assert_eq!(executor.bindings_received(), 1);
    assert_eq!(
        executor.started_models(),
        vec![(run, model_ref("model/test"))]
    );
    assert_eq!(executor.authority_observations(), vec![run]);
    assert_eq!(
        executor.calls(),
        vec![
            ExecutorCall::Prepare,
            ExecutorCall::Attach(run),
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
    assert_eq!(admission.session_id(), RequestId::session_id(request(2)));
    assert_ne!(admission.run_id().nonce(), admission.session_id().uuid());
    assert_eq!(admission.run_id().session_id(), admission.session_id());
}

#[test]
fn exact_replay_is_idempotent_but_digest_mismatch_and_identity_collision_have_no_effect() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&controller, start("/a", 3, "a"));
    let replay = start_command(&controller, start("/a", 3, "a")).expect("exact replay");
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
    assert!(matches!(
        collision,
        Err(ego_core::run::RunError::IdentityCollision { .. })
    ));
    assert_eq!(
        store.transactions(first.session_id()),
        transactions_before_collision
    );
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
        let first = scope.spawn(|| start_command(&left, start("/a", 4, "a")));
        let second = scope.spawn(|| start_command(&right, start("/a", 4, "a")));
        (
            first.join().expect("first start"),
            second.join().expect("second start"),
        )
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
    assert!(
        store
            .transactions(RequestId::session_id(request(5)))
            .is_empty()
    );
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
        RunStatus::Running
    );
    assert!(effect_calls(&executor).is_empty());
}

#[test]
fn submit_error_publishes_uncertain_settlement_and_completes_subscription() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    executor.submission_result(Err(RunExecutorError::new("submission failed")));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/submit-error", 7, "error"));
    let run = admission.run_id();
    let session = admission.session_id();
    let subscription = admission.subscription();
    executor.allow_effects();

    let error = futures::executor::block_on(controller.execute(admission.into_execution()))
        .expect_err("submit error must reach the caller");
    assert!(matches!(
        error,
        ego_core::run::RunError::Executor(error) if error.to_string() == "submission failed"
    ));
    let settled = futures::executor::block_on(subscription);
    assert_eq!(settled.status(), RunStatus::Uncertain);
    // The settlement keeps the reason, not just the kind: the caller above was
    // handed "submission failed" and a retry that replays this receipt has no
    // execution to ask, so this record is the only place that sentence lives.
    assert_eq!(
        settled.settlement(),
        Some(&RunSettlement::Uncertain {
            diagnostic: Some("submission failed".to_owned()),
        })
    );
    assert_eq!(store.transactions(session).len(), 2);
    assert!(
        matches!(
            store.transactions(session)[1].command(),
            ego_core::run::RunCommand::CommandSettled {
                settlement: RunSettlement::Uncertain { diagnostic: Some(diagnostic) },
                ..
            } if diagnostic == "submission failed"
        ),
        "the durable transaction dropped the diagnostic: {:?}",
        store.transactions(session)[1].command()
    );
    assert_eq!(
        executor.calls(),
        vec![
            ExecutorCall::Prepare,
            ExecutorCall::Attach(run),
            ExecutorCall::Submit(run),
        ]
    );
}

#[test]
fn a_settlement_whose_durability_is_uncertain_still_completes_its_subscription() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/uncertain-settlement", 8, "settle"));
    let subscription = admission.subscription();
    executor.allow_effects();
    store.fail_next(StoreFault::PublishedUncertain);

    let error = futures::executor::block_on(controller.execute(admission.into_execution()))
        .expect_err("an uncertain settlement publication must reach the caller");
    assert!(
        error
            .to_string()
            .contains("publication durability uncertain"),
        "the settlement diagnostic was lost: {error}"
    );
    // The turn is over for anyone waiting on it. A settlement this process
    // cannot confirm is terminal *and* uncertain, so the observer is released
    // with that outcome rather than left waiting for a wake that never comes.
    let settled = futures::executor::block_on(subscription);
    assert_eq!(settled.status(), RunStatus::Uncertain);
    let diagnostic = settled
        .settlement()
        .and_then(RunSettlement::diagnostic)
        .unwrap_or_else(|| panic!("released without saying why: {:?}", settled.settlement()));
    assert!(
        diagnostic.contains("publication durability uncertain"),
        "the observer was released with an uncertainty that does not name the \
         durability failure that caused it: {diagnostic}"
    );
}

#[test]
fn terminal_continuation_on_a_fresh_controller_attaches_once_and_preserves_identity() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&first_controller, start("/terminal", 101, "first"));
    let run = first.run_id();
    executor.allow_effects();
    execute(&first_controller, first.into_execution());
    let settled = futures::executor::block_on(first_controller.status(run)).expect("settled start");
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let continuation = admitted_continue(
        &fresh_controller,
        ContinueRequest::new(
            run,
            request(102),
            digest("terminal-continuation"),
            settled.revision(),
            "continue after close",
        ),
    );
    assert_eq!(continuation.run_id(), run);
    let subscription = continuation.subscription();
    execute(&fresh_controller, continuation.into_execution());
    let final_status = futures::executor::block_on(subscription);
    assert_eq!(final_status.status(), RunStatus::Settled);
    assert_eq!(final_status.run_id(), run);
    assert_eq!(final_status.root(), fixture_root("terminal"));
    assert_eq!(executor.attached_origins().len(), 2);
    assert_eq!(
        executor.attached_origins()[0],
        executor.attached_origins()[1]
    );
    assert_eq!(executor.started_models().len(), 2);
    assert_eq!(executor.started_models()[0], executor.started_models()[1]);
    assert_eq!(executor.started_roots().len(), 2);
    assert_eq!(executor.started_roots()[0], executor.started_roots()[1]);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Attach(_)))
            .count(),
        2
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Submit(_)))
            .count(),
        2
    );
}

#[test]
fn cancel_and_settle_removes_cached_handle_before_fresh_controller_continuation() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&first_controller, start("/closed", 112, "first"));
    let run = first.run_id();
    executor.allow_effects();
    execute(&first_controller, first.into_execution());
    let closed = futures::executor::block_on(
        ego_core::session_lifecycle::SessionRunLifecycle::cancel_and_settle(
            &first_controller,
            run.session_id(),
        ),
    )
    .expect("terminal cancel_and_settle");
    assert!(!closed.cancelled);

    let settled = futures::executor::block_on(first_controller.status(run)).expect("settled run");
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let continuation = admitted_continue(
        &fresh_controller,
        ContinueRequest::new(
            run,
            request(113),
            digest("after-close"),
            settled.revision(),
            "after close",
        ),
    );
    let subscription = continuation.subscription();
    execute(&fresh_controller, continuation.into_execution());
    assert_eq!(
        futures::executor::block_on(subscription).status(),
        RunStatus::Settled
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Attach(_)))
            .count(),
        2
    );
}

#[test]
fn cancellation_between_continuation_admission_and_attach_interrupts_before_cancelled_submit() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(
        &first_controller,
        start("/cancel-before-attach", 103, "first"),
    );
    let run = first.run_id();
    executor.allow_effects();
    execute(&first_controller, first.into_execution());
    let settled = futures::executor::block_on(first_controller.status(run)).expect("settled start");
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let continuation = admitted_continue(
        &fresh_controller,
        ContinueRequest::new(
            run,
            request(104),
            digest("cancel-before-attach"),
            settled.revision(),
            "cancel before attach",
        ),
    );
    let key = continuation.cancel_key().clone();
    let subscription = continuation.subscription();
    let cancelling = futures::executor::block_on(fresh_controller.cancel_admitted(&key))
        .expect("continuation cancellation");
    assert_eq!(cancelling.status(), RunStatus::Cancelling);
    execute(&fresh_controller, continuation.into_execution());
    let cancelled = futures::executor::block_on(subscription);
    assert_eq!(cancelled.status(), RunStatus::Cancelled);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Attach(_)))
            .count(),
        2
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        1
    );
    let calls = executor.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Submit(_)))
            .count(),
        2,
        "cancellation remains latched while the continuation submit is attempted"
    );
    let continuation_attach = calls
        .iter()
        .rposition(|call| matches!(call, ExecutorCall::Attach(_)))
        .expect("continuation attachment");
    let continuation_interrupt = calls
        .iter()
        .rposition(|call| matches!(call, ExecutorCall::Interrupt(_)))
        .expect("continuation interrupt");
    let continuation_submit = calls
        .iter()
        .rposition(|call| matches!(call, ExecutorCall::Submit(_)))
        .expect("continuation submit");
    assert!(continuation_attach < continuation_interrupt);
    assert!(continuation_interrupt < continuation_submit);
}

#[test]
fn exact_terminal_continuation_replay_does_not_attach_or_submit_again() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&first_controller, start("/replay", 105, "first"));
    let run = first.run_id();
    executor.allow_effects();
    execute(&first_controller, first.into_execution());
    let settled = futures::executor::block_on(first_controller.status(run)).expect("settled start");
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let continuation_request = ContinueRequest::new(
        run,
        request(106),
        digest("exact-continuation"),
        settled.revision(),
        "exact continuation",
    );
    let continuation = admitted_continue(&fresh_controller, continuation_request);
    execute(&fresh_controller, continuation.into_execution());
    let calls_before_replay = executor.calls();
    let replay = continue_command(
        &fresh_controller,
        ContinueRequest::new(
            run,
            request(106),
            digest("exact-continuation"),
            settled.revision(),
            "exact continuation",
        ),
    )
    .expect("exact continuation replay");
    assert!(matches!(replay, RunCommandOutcome::Replayed(_)));
    assert_eq!(executor.calls(), calls_before_replay);
}

/// A, then B, then retry A: the retry must answer with A's outcome.
///
/// The replay used to be built from the run's *current* snapshot, so retrying
/// a retained older receipt reported whatever turn had settled most recently.
/// Retention here is 2, so both receipts survive and the older one is genuinely
/// reachable — which is the condition the ACP adapter's one-command memory has
/// been hiding.
#[test]
fn replaying_an_older_receipt_reports_its_own_outcome_and_not_a_later_turn() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&controller, start("/receipt-faithful", 200, "first"));
    let run = first.run_id();
    executor.allow_effects();
    execute(&controller, first.into_execution());
    let after_start = futures::executor::block_on(controller.status(run)).expect("settled start");

    // Turn A.
    let turn_a = admitted_continue(
        &controller,
        ContinueRequest::new(
            run,
            request(201),
            digest("turn-a"),
            after_start.revision(),
            "a",
        ),
    );
    execute(&controller, turn_a.into_execution());
    let after_a = futures::executor::block_on(controller.status(run)).expect("settled A");

    // Turn B, which advances the run past A.
    let turn_b = admitted_continue(
        &controller,
        ContinueRequest::new(run, request(202), digest("turn-b"), after_a.revision(), "b"),
    );
    execute(&controller, turn_b.into_execution());
    let after_b = futures::executor::block_on(controller.status(run)).expect("settled B");
    assert_ne!(
        after_a.revision(),
        after_b.revision(),
        "the two turns must be distinguishable for this test to mean anything"
    );

    let replay = continue_command(
        &controller,
        ContinueRequest::new(
            run,
            request(201),
            digest("turn-a"),
            after_start.revision(),
            "a",
        ),
    )
    .expect("A is still retained and replays");
    let RunCommandOutcome::Replayed(observation) = replay else {
        panic!("retrying a settled receipt admitted a second execution");
    };
    assert_eq!(
        observation.snapshot().revision(),
        after_a.revision(),
        "replaying A reported a later turn's revision"
    );
    assert_eq!(
        observation.snapshot().status(),
        after_a.status(),
        "replaying A reported a later turn's status"
    );
}

/// A failed turn, retried after the process that ran it is gone, answers the
/// sentence the original caller got — not a generic one.
///
/// The two halves are one test on purpose. A retry is only interesting once
/// the controller that produced the failure no longer exists: the diagnostic
/// was returned up the original call stack and nothing in memory survives, so
/// a fresh controller reading the same store either finds the reason on the
/// durable receipt or invents a generic answer to a request it already
/// answered specifically. That is the defect — the same request, two different
/// answers, decided by whether the agent had been restarted in between.
#[test]
fn retrying_an_uncertain_turn_after_a_reload_answers_the_original_diagnostic() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&first_controller, start("/replay-diagnostic", 210, "first"));
    let run = first.run_id();
    executor.allow_effects();
    execute(&first_controller, first.into_execution());
    let after_start =
        futures::executor::block_on(first_controller.status(run)).expect("settled start");

    executor.submission_result(Err(RunExecutorError::new(
        "the provider connection dropped",
    )));
    let failing = ContinueRequest::new(
        run,
        request(211),
        digest("uncertain-turn"),
        after_start.revision(),
        "ask something",
    );
    let admitted = admitted_continue(&first_controller, failing);
    let error = futures::executor::block_on(first_controller.execute(admitted.into_execution()))
        .expect_err("the staged submit failure must reach the caller");
    assert_eq!(
        error.to_string(),
        "the provider connection dropped",
        "the original caller was told something other than the staged failure"
    );

    // The process that knew why is gone; only the store remains.
    let reloaded = controller(Arc::clone(&store), Arc::clone(&executor));
    drop(first_controller);
    let calls_before_replay = executor.calls();
    let replay = continue_command(
        &reloaded,
        ContinueRequest::new(
            run,
            request(211),
            digest("uncertain-turn"),
            after_start.revision(),
            "ask something",
        ),
    )
    .expect("the retained receipt replays rather than being refused");
    let RunCommandOutcome::Replayed(observation) = replay else {
        panic!("retrying a settled receipt admitted a second execution after a reload");
    };
    assert_eq!(
        executor.calls(),
        calls_before_replay,
        "the replay reached the executor instead of answering from the receipt"
    );
    assert_eq!(
        observation.snapshot().settlement(),
        Some(&RunSettlement::Uncertain {
            diagnostic: Some("the provider connection dropped".to_owned()),
        }),
        "the retry answered a generic uncertainty instead of the reason the \
         original request was given"
    );
}

#[test]
fn attach_failure_settles_uncertain_and_completes_the_continuation_subscription() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&first_controller, start("/attach-error", 107, "first"));
    let run = first.run_id();
    executor.allow_effects();
    execute(&first_controller, first.into_execution());
    let settled = futures::executor::block_on(first_controller.status(run)).expect("settled start");
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let continuation = admitted_continue(
        &fresh_controller,
        ContinueRequest::new(
            run,
            request(108),
            digest("attach-error"),
            settled.revision(),
            "attach failure",
        ),
    );
    let subscription = continuation.subscription();
    executor.attachment_result(Err(RunExecutorError::new("attachment failed")));
    let error =
        futures::executor::block_on(fresh_controller.execute(continuation.into_execution()))
            .expect_err("attach failure must reach the caller");
    assert!(matches!(
        error,
        ego_core::run::RunError::Executor(error) if error.to_string() == "attachment failed"
    ));
    let settled = futures::executor::block_on(subscription);
    assert_eq!(settled.status(), RunStatus::Uncertain);
    assert_eq!(
        settled.settlement(),
        Some(&RunSettlement::Uncertain {
            diagnostic: Some("attachment failed".to_owned()),
        })
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Submit(_)))
            .count(),
        1
    );
}

#[test]
fn cancelling_continuation_interrupt_failure_settles_uncertain_without_orphaning() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&first_controller, start("/interrupt-error", 114, "first"));
    let run = first.run_id();
    executor.allow_effects();
    execute(&first_controller, first.into_execution());
    let settled = futures::executor::block_on(first_controller.status(run)).expect("settled start");
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let continuation = admitted_continue(
        &fresh_controller,
        ContinueRequest::new(
            run,
            request(115),
            digest("interrupt-error"),
            settled.revision(),
            "interrupt failure",
        ),
    );
    let key = continuation.cancel_key().clone();
    let subscription = continuation.subscription();
    futures::executor::block_on(fresh_controller.cancel_admitted(&key)).expect("cancel admission");
    executor.interrupt_result(Err(RunExecutorError::new("interrupt failed")));
    let error =
        futures::executor::block_on(fresh_controller.execute(continuation.into_execution()))
            .expect_err("interrupt failure must reach the caller");
    assert!(matches!(
        error,
        ego_core::run::RunError::Executor(error) if error.to_string() == "interrupt failed"
    ));
    let settled = futures::executor::block_on(subscription);
    assert_eq!(settled.status(), RunStatus::Uncertain);
}

#[test]
fn recovered_observe_failure_settles_uncertain_and_completes_subscription() {
    let (controller, _store, executor, run, _original_cancel_key) = recovery_controller();
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::Running(
        RunHandle::new(run, Arc::new(RecordingAuthority::default())),
    )));
    executor.observation_result(Err(RunExecutorError::new("observe failed")));
    let recovery =
        futures::executor::block_on(controller.recover(run, bindings())).expect("running recovery");
    let reattachment = match recovery {
        RunRecoveryOutcome::Reattached(reattachment) => reattachment,
        RunRecoveryOutcome::Observed(_) => panic!("running recovery did not reattach"),
    };
    let subscription = reattachment.subscription();
    let error = futures::executor::block_on(controller.execute(reattachment.into_execution()))
        .expect_err("observe failure must reach the caller");
    assert!(matches!(
        error,
        ego_core::run::RunError::Executor(error) if error.to_string() == "observe failed"
    ));
    assert_eq!(
        futures::executor::block_on(subscription).status(),
        RunStatus::Uncertain
    );
}

#[test]
fn wrong_attachment_run_id_is_interrupted_without_submit_or_orphaned_subscription() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let first = admitted_start(&first_controller, start("/wrong-attach", 109, "first"));
    let run = first.run_id();
    executor.allow_effects();
    execute(&first_controller, first.into_execution());
    let settled = futures::executor::block_on(first_controller.status(run)).expect("settled start");
    let fresh_controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let continuation = admitted_continue(
        &fresh_controller,
        ContinueRequest::new(
            run,
            request(110),
            digest("wrong-attach"),
            settled.revision(),
            "wrong attachment",
        ),
    );
    let subscription = continuation.subscription();
    let wrong_run = RunId::new(run.session_id(), uuid::Uuid::from_u128(999_999));
    executor.attachment_result(Ok(RunHandle::new(
        wrong_run,
        Arc::new(RecordingAuthority::default()),
    )));
    let error =
        futures::executor::block_on(fresh_controller.execute(continuation.into_execution()))
            .expect_err("wrong attachment identity must fail closed");
    assert!(matches!(error, ego_core::run::RunError::Integrity { .. }));
    let settled = futures::executor::block_on(subscription);
    assert_eq!(settled.status(), RunStatus::Interrupted);
    assert_eq!(
        settled.settlement(),
        Some(&RunSettlement::Interrupted {
            diagnostic: Some("executor returned a handle for another run".to_owned()),
        })
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Submit(_)))
            .count(),
        1,
        "wrong attachment identity must prevent a second submit"
    );
}

#[test]
fn settlement_failure_preserves_execution_and_settlement_diagnostics() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    executor.submission_result(Err(RunExecutorError::new("submission failed")));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/settlement-error", 111, "failure"));
    store.fail_next(StoreFault::Reject);
    executor.allow_effects();
    let error = futures::executor::block_on(controller.execute(admission.into_execution()))
        .expect_err("execution and settlement failures must both reach the caller");
    let diagnostic = error.to_string();
    assert!(
        diagnostic.contains("submission failed"),
        "missing execution diagnostic: {error}"
    );
    assert!(
        diagnostic.contains("rejected"),
        "missing settlement diagnostic: {error}"
    );
}

#[test]
fn folding_rejects_a_checksum_valid_admitted_status_that_is_not_running() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-admitted", 30, "admitted"));
    let run = admission.run_id();
    let session = admission.session_id();
    rewrite_transaction(&store, session, 0, |value| {
        value["status"] = serde_json::json!("settled");
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        130,
        RunRevision::new(1),
    );
}

#[test]
fn folding_rejects_a_checksum_valid_cancel_status_that_is_not_cancelling() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-cancel", 31, "cancel"));
    let run = admission.run_id();
    let session = admission.session_id();
    let key = admission.cancel_key().clone();
    futures::executor::block_on(controller.cancel_admitted(&key)).expect("cancel admission");
    rewrite_transaction(&store, session, 1, |value| {
        value["status"] = serde_json::json!("running");
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        131,
        RunRevision::new(2),
    );
}

#[test]
fn folding_rejects_a_checksum_valid_settled_status_that_disagrees_with_its_settlement() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-settled", 32, "settled"));
    let run = admission.run_id();
    let session = admission.session_id();
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    rewrite_transaction(&store, session, 1, |value| {
        value["status"] = serde_json::json!("running");
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        132,
        RunRevision::new(2),
    );
}

#[test]
fn folding_rejects_a_malformed_first_admission_start_flag() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-first", 33, "first"));
    let run = admission.run_id();
    let session = admission.session_id();
    rewrite_transaction(&store, session, 0, |value| {
        value["command"]["start"] = serde_json::json!(false);
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        133,
        RunRevision::new(1),
    );
}

#[test]
fn folding_rejects_a_malformed_first_admission_expected_revision() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-first-revision", 34, "first"));
    let run = admission.run_id();
    let session = admission.session_id();
    rewrite_transaction(&store, session, 0, |value| {
        value["command"]["expected_revision"] = serde_json::json!(1);
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        134,
        RunRevision::new(1),
    );
}

#[test]
fn folding_rejects_a_malformed_continuation_start_flag() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-continuation", 34, "first"));
    let run = admission.run_id();
    let session = admission.session_id();
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    let settled = futures::executor::block_on(controller.status(run)).expect("settled start");
    let continuation = admitted_continue(
        &controller,
        ContinueRequest::new(
            run,
            request(35),
            digest("continuation"),
            settled.revision(),
            "continuation",
        ),
    );
    drop(continuation);
    rewrite_transaction(&store, session, 2, |value| {
        value["command"]["start"] = serde_json::json!(true);
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        135,
        RunRevision::new(3),
    );
}

#[test]
fn folding_rejects_a_malformed_continuation_expected_revision() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(
        &controller,
        start("/fold-continuation-revision", 35, "first"),
    );
    let run = admission.run_id();
    let session = admission.session_id();
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    let settled = futures::executor::block_on(controller.status(run)).expect("settled start");
    let continuation = admitted_continue(
        &controller,
        ContinueRequest::new(
            run,
            request(36),
            digest("continuation-revision"),
            settled.revision(),
            "continuation",
        ),
    );
    drop(continuation);
    rewrite_transaction(&store, session, 2, |value| {
        value["command"]["expected_revision"] = serde_json::Value::Null;
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        136,
        RunRevision::new(3),
    );
}

#[test]
fn folding_rejects_an_admission_whose_executor_id_is_not_the_run_nonce() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-executor", 37, "executor"));
    let run = admission.run_id();
    let session = admission.session_id();
    rewrite_transaction(&store, session, 0, |value| {
        value["command"]["executor_id"] = serde_json::json!("00000000-0000-0000-0000-000000000099");
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        137,
        RunRevision::new(1),
    );
}

#[test]
fn folding_rejects_a_continuation_with_a_different_persisted_model() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-model", 38, "first"));
    let run = admission.run_id();
    let session = admission.session_id();
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    let settled = futures::executor::block_on(controller.status(run)).expect("settled start");
    let continuation = admitted_continue(
        &controller,
        ContinueRequest::new(
            run,
            request(39),
            digest("continuation-model"),
            settled.revision(),
            "continuation",
        ),
    );
    drop(continuation);
    rewrite_transaction(&store, session, 2, |value| {
        value["command"]["model"] = serde_json::json!("model/other");
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        139,
        RunRevision::new(3),
    );
}

#[test]
fn folding_rejects_a_continuation_that_reuses_a_settled_lease() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-lease", 40, "first"));
    let run = admission.run_id();
    let session = admission.session_id();
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    let settled = futures::executor::block_on(controller.status(run)).expect("settled start");
    let continuation = admitted_continue(
        &controller,
        ContinueRequest::new(
            run,
            request(41),
            digest("continuation-lease"),
            settled.revision(),
            "continuation",
        ),
    );
    drop(continuation);
    let first_lease = serde_json::to_value(store.transactions(session)[0].command())
        .expect("settled admission serialises")["lease"]
        .clone();
    rewrite_transaction(&store, session, 2, |value| {
        value["command"]["lease"] = first_lease;
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        141,
        RunRevision::new(3),
    );
}

#[test]
fn folding_rejects_pruning_an_admitted_receipt_before_its_settlement() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-pruning", 42, "pruning"));
    let run = admission.run_id();
    let session = admission.session_id();
    rewrite_transaction(&store, session, 0, |value| {
        value["pruned_receipts"] = serde_json::json!(["00000000-0000-0000-0000-000000000002"]);
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        143,
        RunRevision::new(1),
    );
}

#[test]
fn folding_rejects_pruning_an_unknown_receipt() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-unknown-pruning", 43, "pruning"));
    let run = admission.run_id();
    let session = admission.session_id();
    rewrite_transaction(&store, session, 0, |value| {
        value["pruned_receipts"] = serde_json::json!(["00000000-0000-0000-0000-000000000099"]);
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        144,
        RunRevision::new(1),
    );
}

#[test]
fn folding_rejects_pruning_a_settled_receipt_on_a_later_admission() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/fold-later-pruning", 44, "first"));
    let run = admission.run_id();
    let session = admission.session_id();
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    let settled = futures::executor::block_on(controller.status(run)).expect("settled start");
    let continuation = admitted_continue(
        &controller,
        ContinueRequest::new(
            run,
            request(45),
            digest("later-pruning"),
            settled.revision(),
            "continuation",
        ),
    );
    drop(continuation);

    let first_lease = serde_json::to_value(store.transactions(session)[0].command())
        .expect("settled admission serialises")["lease"]
        .clone();
    rewrite_transaction(&store, session, 2, |value| {
        value["pruned_receipts"] = serde_json::json!([first_lease]);
    });

    assert_corrupt_run_is_rejected_everywhere(
        &controller,
        &executor,
        &store,
        run,
        session,
        145,
        RunRevision::new(3),
    );
}

#[test]
fn continuation_rejects_a_prepared_model_that_differs_from_persisted_identity() {
    let store = Arc::new(RecordingSessionStore::default());
    let executor = Arc::new(ScriptedRunExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let admission = admitted_start(&controller, start("/model", 8, "first"));
    let run = admission.run_id();
    let session = admission.session_id();
    executor.allow_effects();
    execute(&controller, admission.into_execution());
    let status = futures::executor::block_on(controller.status(run)).expect("settled start");
    let calls_before = executor.calls();
    let transactions_before = store.transactions(session);
    executor.prepare_model(model_ref("model/other"));

    let result = continue_command(
        &controller,
        ContinueRequest::new(
            run,
            request(9),
            digest("second"),
            status.revision(),
            "second",
        ),
    );
    assert!(matches!(
        result,
        Err(ego_core::run::RunError::Integrity { .. })
    ));
    let mut expected_calls = calls_before;
    expected_calls.push(ExecutorCall::Prepare);
    assert_eq!(executor.calls(), expected_calls);
    assert_eq!(store.transactions(session), transactions_before);
    assert_eq!(
        futures::executor::block_on(controller.status(run))
            .expect("status after model mismatch")
            .status(),
        RunStatus::Settled
    );
}

// Catches: interruption before cancel admission, duplicate settlement, and continuation retargeting.
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

    let next = admitted_continue(
        &controller,
        ContinueRequest::new(
            run,
            request(8),
            digest("b"),
            settled_start.revision(),
            "continue",
        ),
    );
    let next_revision = next.revision();
    let cancel_key: CancelKey = next.cancel_key().clone();
    let execution = next.into_execution();
    assert_eq!(
        executor.prepared_requests(),
        vec![None, Some(model_ref("model/test"))]
    );
    let transactions = store.transactions(session);
    assert_eq!(transactions.len(), 3);
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert_eq!(transactions[2].revision(), RunRevision::new(3));
    assert!(matches!(
        transactions[2].command(),
        ego_core::run::RunCommand::CommandAdmitted { model, .. }
            if model == &model_ref("model/test")
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
        [
            "prepare", "commit", "attach", "submit", "commit", "prepare", "commit"
        ]
    );

    executor.block_effects();
    let worker_controller = Arc::clone(&controller);
    let worker = std::thread::spawn(move || execute(&worker_controller, execution));
    executor.wait_for_submit_start();
    assert_eq!(
        executor.calls(),
        vec![
            ExecutorCall::Prepare,
            ExecutorCall::Attach(run),
            ExecutorCall::Submit(run),
            ExecutorCall::Prepare,
            ExecutorCall::Submit(run),
        ]
    );
    assert_eq!(
        sequence.lock().expect("transaction sequence").as_slice(),
        [
            "prepare", "commit", "attach", "submit", "commit", "prepare", "commit", "submit"
        ]
    );

    let cancelling = futures::executor::block_on(controller.cancel_admitted(&cancel_key))
        .expect("cancel admission");
    assert_eq!(cancelling.status(), RunStatus::Cancelling);
    // Interrupt wakes the worker, so settlement may commit before cancel_admitted returns.
    // Join before checking the complete order, including exactly one final settlement.
    worker.join().expect("cancelled execution");
    assert_eq!(
        sequence.lock().expect("transaction sequence").as_slice(),
        [
            "prepare",
            "commit",
            "attach",
            "submit",
            "commit",
            "prepare",
            "commit",
            "submit",
            "commit",
            "interrupt",
            "commit"
        ]
    );
    let final_status = futures::executor::block_on(controller.status(run)).expect("final status");
    assert_eq!(final_status.status(), RunStatus::Cancelled);
    let transactions = store.transactions(session);
    assert_eq!(transactions.len(), 5);
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert_eq!(transactions[2].revision(), RunRevision::new(3));
    assert_eq!(transactions[3].revision(), RunRevision::new(4));
    assert_eq!(transactions[4].revision(), RunRevision::new(5));
    assert!(matches!(
        transactions[3].command(),
        ego_core::run::RunCommand::CancelRequested { .. }
    ));
    assert!(matches!(
        transactions[4].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(transactions[0].status(), RunStatus::Running);
    assert_eq!(transactions[1].status(), RunStatus::Settled);
    assert_eq!(transactions[2].status(), RunStatus::Running);
    assert_eq!(transactions[3].status(), RunStatus::Cancelling);
    assert_eq!(transactions[4].status(), RunStatus::Cancelled);
    assert_eq!(
        executor.calls(),
        vec![
            ExecutorCall::Prepare,
            ExecutorCall::Attach(run),
            ExecutorCall::Submit(run),
            ExecutorCall::Prepare,
            ExecutorCall::Submit(run),
            ExecutorCall::Interrupt(run),
        ]
    );

    let calls_after_cancel = executor.calls();
    assert!(
        futures::executor::block_on(controller.cancel(CancelRequest::new(
            run,
            request(10),
            digest("stale"),
            next_revision,
        )))
        .is_err()
    );
    assert_eq!(store.transactions(session).len(), 5);
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
    assert!(
        continue_command(
            &controller,
            ContinueRequest::new(
                first_run,
                request(13),
                digest("busy"),
                first.revision(),
                "blocked",
            )
        )
        .is_err()
    );

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
    assert!(
        started_roots
            .iter()
            .any(|(run, root)| { *run == first_run && root == &first_root })
    );
    assert!(
        started_roots
            .iter()
            .any(|(run, root)| { *run == second_run && root == &second_root })
    );
    assert_eq!(
        futures::executor::block_on(controller.status(second_run))
            .expect("status")
            .root(),
        second_root
    );
}

type RecoveryController =
    RunController<RecordingSessionStore, ScriptedRunExecutor, FixedClock, SequenceIds>;
type RecoveryFixture = (
    RecoveryController,
    Arc<RecordingSessionStore>,
    Arc<ScriptedRunExecutor>,
    RunId,
    CancelKey,
);

fn recovery_controller() -> RecoveryFixture {
    let store = Arc::new(RecordingSessionStore::default());
    let first_executor = Arc::new(ScriptedRunExecutor::default());
    let first_controller = controller(Arc::clone(&store), Arc::clone(&first_executor));
    let admission = admitted_start(&first_controller, start("/recover", 20, "recovery"));
    let run_id = admission.run_id();
    let cancel_key = admission.cancel_key().clone();
    drop(admission);
    drop(first_controller);
    drop(first_executor);

    let executor = Arc::new(ScriptedRunExecutor::default());
    (
        controller(Arc::clone(&store), Arc::clone(&executor)),
        store,
        executor,
        run_id,
        cancel_key,
    )
}

#[test]
fn recovery_running_reattaches_live_execution() {
    let (controller, store, executor, run, original_cancel_key) = recovery_controller();
    let calls_before_status = executor.calls();
    let status_before_recovery = futures::executor::block_on(controller.status(run))
        .expect("running status before explicit recovery");
    assert_eq!(status_before_recovery.status(), RunStatus::Running);
    assert_eq!(executor.calls(), calls_before_status);
    assert_eq!(store.transactions(run.session_id()).len(), 1);
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::Running(
        RunHandle::new(run, Arc::new(RecordingAuthority::default())),
    )));
    executor.observation_result(Ok(RunSettlement::completed()));
    assert_eq!(store.transactions(run.session_id()).len(), 1);
    let recovery =
        futures::executor::block_on(controller.recover(run, bindings())).expect("running recovery");
    let reattachment = match recovery {
        RunRecoveryOutcome::Reattached(reattachment) => reattachment,
        RunRecoveryOutcome::Observed(_) => panic!("running recovery returned observation"),
    };
    assert_eq!(reattachment.snapshot().status(), RunStatus::Running);
    assert_ne!(reattachment.cancel_key(), &original_cancel_key);
    assert_eq!(
        reattachment.subscription().snapshot().status(),
        RunStatus::Running
    );
    execute(&controller, reattachment.into_execution());
    let status = futures::executor::block_on(controller.status(run)).expect("running status");
    assert_eq!(status.status(), RunStatus::Settled);
    assert_eq!(
        executor.calls(),
        vec![ExecutorCall::Recover(run), ExecutorCall::Observe(run)]
    );
    assert_eq!(executor.bindings_received(), 1);
    assert_eq!(store.transactions(run.session_id()).len(), 2);
    assert!(matches!(
        store.transactions(run.session_id())[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
}

#[test]
fn recovery_terminal_preserves_terminal_settlement() {
    let (controller, store, executor, run, _) = recovery_controller();
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::Terminal));
    assert_eq!(
        store.transactions(run.session_id())[0].status(),
        RunStatus::Running
    );
    let recovery = futures::executor::block_on(controller.recover(run, bindings()))
        .expect("terminal recovery");
    let observation = match recovery {
        RunRecoveryOutcome::Observed(observation) => observation,
        RunRecoveryOutcome::Reattached(_) => panic!("terminal recovery reattached"),
    };
    assert_eq!(
        observation.snapshot().settlement(),
        Some(&RunSettlement::completed())
    );
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
    assert_eq!(
        store.transactions(run.session_id())[1].status(),
        RunStatus::Settled
    );
}

#[test]
fn recovery_cancelled_before_start_settles_cancelled() {
    let (controller, store, executor, run, _) = recovery_controller();
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::CancelledBeforeStart));
    assert_eq!(
        store.transactions(run.session_id())[0].status(),
        RunStatus::Running
    );
    let recovery = futures::executor::block_on(controller.recover(run, bindings()))
        .expect("cancelled-before-start recovery");
    let observation = match recovery {
        RunRecoveryOutcome::Observed(observation) => observation,
        RunRecoveryOutcome::Reattached(_) => panic!("a run cancelled before start reattached"),
    };
    // Renamed from `..._settles_interrupted`, which pinned the conflation this
    // story removes: a cancellation that beat the executor settled the same
    // value an internal invariant violation settles, so a host could not tell
    // "you cancelled in time" from "this agent broke".
    assert_eq!(
        observation.snapshot().settlement(),
        Some(&RunSettlement::CancelledBeforeStart)
    );
    assert_eq!(
        futures::executor::block_on(controller.status(run))
            .expect("cancelled status")
            .status(),
        RunStatus::Cancelled
    );
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
    assert_eq!(store.transactions(run.session_id()).len(), 2);
    assert!(matches!(
        store.transactions(run.session_id())[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(
        store.transactions(run.session_id())[1].status(),
        RunStatus::Cancelled,
        "the durable transaction disagrees with the observation"
    );
}

#[test]
fn recovery_ambiguous_external_effect_is_uncertain_and_not_retried() {
    let (controller, store, executor, run, _) = recovery_controller();
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::Ambiguous));
    assert_eq!(
        store.transactions(run.session_id())[0].status(),
        RunStatus::Running
    );
    let recovery = futures::executor::block_on(controller.recover(run, bindings()))
        .expect("uncertain recovery");
    let observation = match recovery {
        RunRecoveryOutcome::Observed(observation) => observation,
        RunRecoveryOutcome::Reattached(_) => panic!("ambiguous recovery reattached"),
    };
    assert_eq!(
        observation.snapshot().settlement(),
        Some(&RunSettlement::Uncertain {
            diagnostic: Some("the executor could not say whether this turn ran".to_owned()),
        })
    );
    assert_eq!(
        futures::executor::block_on(controller.status(run))
            .expect("uncertain status")
            .status(),
        RunStatus::Uncertain
    );
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
    assert!(
        futures::executor::block_on(controller.status(run))
            .expect("stable uncertain status")
            .blocks_new_turn()
    );
    assert_eq!(store.transactions(run.session_id()).len(), 2);
    assert!(matches!(
        store.transactions(run.session_id())[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(
        store.transactions(run.session_id())[1].status(),
        RunStatus::Uncertain
    );
}

#[test]
fn recovery_not_recoverable_is_observed_as_uncertain() {
    let (controller, store, executor, run, _) = recovery_controller();
    executor.recovery_result(Ok(ego_core::run::RecoveryEvidence::NotRecoverable));
    let recovery = futures::executor::block_on(controller.recover(run, bindings()))
        .expect("not recoverable recovery");
    let observation = match recovery {
        RunRecoveryOutcome::Observed(observation) => observation,
        RunRecoveryOutcome::Reattached(_) => panic!("not recoverable recovery reattached"),
    };
    assert_eq!(observation.snapshot().status(), RunStatus::Uncertain);
    // Distinct from the ambiguous case above: two causes that used to settle
    // as one indistinguishable `Uncertain` now say which happened.
    assert_eq!(
        observation.snapshot().settlement(),
        Some(&RunSettlement::Uncertain {
            diagnostic: Some("the executor cannot recover this turn after a restart".to_owned()),
        })
    );
    assert_eq!(executor.calls(), vec![ExecutorCall::Recover(run)]);
    assert_eq!(store.transactions(run.session_id()).len(), 2);
    assert!(matches!(
        store.transactions(run.session_id())[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
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
    let start_status =
        futures::executor::block_on(first_controller.status(run)).expect("initial terminal status");
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
    let recovery = futures::executor::block_on(fresh_controller.recover(run, bindings()))
        .expect("already terminal recovery");
    assert!(matches!(recovery, RunRecoveryOutcome::Observed(_)));
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
    let active_status =
        futures::executor::block_on(controller.status(active_run)).expect("active status");
    assert_eq!(active_status.status(), RunStatus::Settled);

    let active_replay =
        start_command(&controller, start("/active", 30, "active")).expect("active exact replay");
    assert_eq!(replayed_run_id(active_replay), active_run);
    assert_eq!(store.transactions(active_session).len(), 2);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Attach(_)))
            .count(),
        1
    );

    let first = admitted_continue(
        &controller,
        ContinueRequest::new(
            active_run,
            request(31),
            digest("continue-31"),
            active_status.revision(),
            "continue 31",
        ),
    );
    execute(&controller, first.into_execution());
    let first_status = futures::executor::block_on(controller.status(active_run))
        .expect("first continuation status");

    let second = admitted_continue(
        &controller,
        ContinueRequest::new(
            active_run,
            request(32),
            digest("continue-32"),
            first_status.revision(),
            "continue 32",
        ),
    );
    execute(&controller, second.into_execution());
    let second_status = futures::executor::block_on(controller.status(active_run))
        .expect("second continuation status");

    let third = admitted_continue(
        &controller,
        ContinueRequest::new(
            active_run,
            request(33),
            digest("continue-33"),
            second_status.revision(),
            "continue 33",
        ),
    );
    execute(&controller, third.into_execution());
    let third_status = futures::executor::block_on(controller.status(active_run))
        .expect("third continuation status");
    let current_revision = third_status.revision();

    let calls_before_retained_replay = executor.calls();
    let transactions_before_retained_replay = store.transactions(active_session);
    let middle_replay = continue_command(
        &controller,
        ContinueRequest::new(
            active_run,
            request(32),
            digest("continue-32"),
            first_status.revision(),
            "continue 32",
        ),
    )
    .expect("middle retained replay");
    assert_eq!(replayed_run_id(middle_replay), active_run);
    assert_eq!(executor.calls(), calls_before_retained_replay);
    assert_eq!(
        store.transactions(active_session),
        transactions_before_retained_replay
    );

    let recent_replay = continue_command(
        &controller,
        ContinueRequest::new(
            active_run,
            request(33),
            digest("continue-33"),
            second_status.revision(),
            "continue 33",
        ),
    )
    .expect("recent retained replay");
    assert_eq!(replayed_run_id(recent_replay), active_run);
    assert_eq!(executor.calls(), calls_before_retained_replay);
    assert_eq!(
        store.transactions(active_session),
        transactions_before_retained_replay
    );

    let calls_before_occupied_start = executor.calls();
    let transactions_before_occupied_start = store.transactions(active_session);
    let sequence_before_occupied_start = sequence.lock().expect("transaction sequence").clone();
    let occupied_start = start_command(&controller, start("/active", 30, "active"));
    assert!(matches!(
        occupied_start,
        Err(ego_core::run::RunError::IdentityCollision { .. })
    ));
    assert_eq!(executor.calls(), calls_before_occupied_start);
    assert_eq!(
        store.transactions(active_session),
        transactions_before_occupied_start
    );
    assert_eq!(
        sequence.lock().expect("transaction sequence").as_slice(),
        sequence_before_occupied_start.as_slice()
    );

    let calls_before_pruned_resubmit = executor.calls();
    let transactions_before_pruned_resubmit = store.transactions(active_session);
    let pruned_continuation = admitted_continue(
        &controller,
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
    let mut calls_after_pruned_admission = calls_before_pruned_resubmit.clone();
    calls_after_pruned_admission.push(ExecutorCall::Prepare);
    assert_eq!(executor.calls(), calls_after_pruned_admission);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Attach(_) | ExecutorCall::Submit(_)))
            .count(),
        calls_before_pruned_resubmit
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Attach(_) | ExecutorCall::Submit(_)))
            .count()
    );
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
        let blocked_expected_for_worker = blocked_expected;
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
        let replay = continue_command(
            &controller,
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
        assert_eq!(
            store.transactions(active_session),
            transactions_after_admission
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
        let settled_run = worker.join().expect("blocked continuation task");
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
            .filter(|call| matches!(call, ExecutorCall::Attach(_)))
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
