//! Minimal deterministic RunController adapters for the MCP contract tests.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Poll, Waker};
use std::{future::Future, pin::Pin};

use futures::future::{self, BoxFuture};
use futures::stream::{self, BoxStream};

use ego_core::compact::{
    CompactionCommand, CompactionFuture, CompactionReceipt, CompactionTransitionError,
    CompactionTransitionRequest,
};
use ego_core::config::ModelRef;
use ego_core::event::domain::DomainEvent;
use ego_core::event::id::{Seq, SessionId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{EvidenceError, EvidenceWrite};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::run::{
    DeniedBy, EventSinkFactory, IdSource, RequestId, RunAppend, RunAttachment, RunController,
    RunExecutionBindings, RunExecutor, RunExecutorError, RunFuture, RunHandle, RunId,
    RunPreparation, RunPrepared, RunRetention, RunRevision, RunRoot, RunSelections, RunSettlement,
    RunTransaction,
};
use ego_core::session::TargetPublication;

use ego_core::sink::{EventSink, NullSink};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_mcp::{
    McpError, McpFuture, McpRunBindingsBuilder, McpRunExecutionSpawner, McpRunResourceReader,
    McpRunResourceRequest, McpRunRootResolver, McpRunServerPorts, McpServerToolCall,
    SanitizedMcpRunResource,
};
use serde_json::{Map, json};

pub const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/mcp-run-controller");

pub fn root() -> RunRoot {
    RunRoot::new(ROOT).expect("fixture root")
}

fn invalid_error() -> McpError {
    match McpServerToolCall::new(String::new(), Map::new(), None) {
        Ok(_) => panic!("invalid fixture call unexpectedly succeeded"),
        Err(error) => error,
    }
}

#[derive(Default)]
pub struct SessionStoreFixture {
    entries: Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>,
}

impl SessionStore for SessionStoreFixture {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("entries")
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
            .expect("entries")
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
            .expect("entries")
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
        let mut entries = self.entries.lock().expect("entries");
        let current = entries.get(session).and_then(|items| {
            items.iter().rev().find_map(|entry| match entry {
                SessionEntry::RunTransaction(tx) => Some(tx.revision()),
                SessionEntry::Event(_) => None,
                SessionEntry::Evidence(_) | SessionEntry::NoticeDelivered(_) => None,
            })
        });
        if current != expected_revision {
            return Ok(current.map_or(
                RunAppend::Conflict {
                    actual: RunRevision::new(0),
                },
                |actual| RunAppend::Conflict { actual },
            ));
        }
        entries
            .entry(*session)
            .or_default()
            .push(SessionEntry::RunTransaction(transaction));
        Ok(RunAppend::Committed {
            revision: current.map_or_else(|| RunRevision::new(1), RunRevision::next),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ExecutorCall {
    Prepare,
    Start,
    Recover,
    Observe,
    Submit,
    Interrupt,
    Pause,
}

pub struct ExecutorFixture {
    calls: Mutex<Vec<ExecutorCall>>,
    submissions: Mutex<VecDeque<Result<RunSettlement, RunExecutorError>>>,
    gate: Mutex<Gate>,
    changed: Condvar,
    next_prepared: Mutex<u64>,
    prepared_selections: Mutex<Vec<RunSelections>>,
    capacity: Mutex<Option<usize>>,
}

#[derive(Default)]
struct Gate {
    released: bool,
    interrupted: bool,
    started: bool,
    waker: Option<Waker>,
}

pub struct Prepared;

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

impl Default for ExecutorFixture {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            submissions: Mutex::new(VecDeque::new()),
            gate: Mutex::new(Gate::default()),
            changed: Condvar::new(),
            next_prepared: Mutex::new(0),
            prepared_selections: Mutex::new(Vec::new()),
            capacity: Mutex::new(None),
        }
    }
}

impl ExecutorFixture {
    pub fn block_effects(&self) {
        let mut gate = self.gate.lock().expect("gate");
        *gate = Gate::default();
    }
    pub fn allow_effects(&self) {
        let waker = {
            let mut gate = self.gate.lock().expect("gate");
            gate.released = true;
            self.changed.notify_all();
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    pub fn permission_required(&self) {
        self.submissions.lock().expect("submissions").push_back(Ok(
            RunSettlement::permission_required("write file", DeniedBy::Nobody),
        ));
    }

    /// Used by the compaction integration crate while shared support is compiled per test target.
    #[allow(dead_code)]
    pub fn push_settlement(&self, settlement: RunSettlement) {
        self.submissions
            .lock()
            .expect("submissions")
            .push_back(Ok(settlement));
    }
    pub fn calls(&self) -> Vec<ExecutorCall> {
        self.calls.lock().expect("calls").clone()
    }
    pub fn prepared_selections(&self) -> Vec<RunSelections> {
        self.prepared_selections
            .lock()
            .expect("prepared selections")
            .clone()
    }
    pub fn set_capacity(&self, capacity: usize) {
        *self.capacity.lock().expect("capacity") = Some(capacity);
    }
}

impl RunExecutor for ExecutorFixture {
    type Prepared = Prepared;

    fn prepare<'a>(
        &'a self,
        request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        if let Some(capacity) = self.capacity.lock().expect("capacity").as_mut() {
            if *capacity == 0 {
                return Box::pin(future::ready(Err(RunExecutorError::new(
                    "executor concurrency ceiling",
                ))));
            }
            *capacity -= 1;
        }
        self.calls
            .lock()
            .expect("calls")
            .push(ExecutorCall::Prepare);
        self.prepared_selections
            .lock()
            .expect("prepared selections")
            .push(request.selections.clone());
        let mut next = self.next_prepared.lock().expect("prepared");
        *next += 1;
        Box::pin(future::ready(Ok(RunPrepared::new(
            ModelRef("model/allowed".to_owned()),
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
        self.calls.lock().expect("calls").push(ExecutorCall::Start);
        Box::pin(future::ready(Ok(RunHandle::new(
            request.run_id,
            Arc::new(()),
        ))))
    }
    fn recover<'a>(
        &'a self,
        _run: &'a RunId,
    ) -> BoxFuture<'a, Result<ego_core::run::RecoveryEvidence, RunExecutorError>> {
        self.calls
            .lock()
            .expect("calls")
            .push(ExecutorCall::Recover);
        Box::pin(future::ready(Ok(
            ego_core::run::RecoveryEvidence::Ambiguous,
        )))
    }
    fn observe<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.calls
            .lock()
            .expect("calls")
            .push(ExecutorCall::Observe);
        Box::pin(future::ready(Ok(RunSettlement::completed())))
    }
    fn submit<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _: &'a ego_core::run::RunPrompt,
        _: Self::Prepared,
        _: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.calls.lock().expect("calls").push(ExecutorCall::Submit);
        let gate = &self.gate;
        let changed = &self.changed;
        let submissions = &self.submissions;
        Box::pin(futures::future::poll_fn(move |cx| {
            let mut gate = gate.lock().expect("gate");
            gate.started = true;
            changed.notify_all();
            if gate.interrupted {
                return Poll::Ready(Ok(RunSettlement::cancelled(
                    ego_core::run::CancelReason::User,
                )));
            }
            if gate.released {
                return Poll::Ready(
                    submissions
                        .lock()
                        .expect("submissions")
                        .pop_front()
                        .unwrap_or_else(|| Ok(RunSettlement::completed())),
                );
            }
            gate.waker = Some(cx.waker().clone());
            Poll::Pending
        }))
    }
    fn interrupt<'a>(
        &'a self,
        _handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        self.calls
            .lock()
            .expect("calls")
            .push(ExecutorCall::Interrupt);
        let waker = {
            let mut gate = self.gate.lock().expect("gate");
            gate.interrupted = true;
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Box::pin(future::ready(Ok(())))
    }

    /// Recorded and not gated, unlike `interrupt`: a pause does not abandon
    /// the effect in flight, so there is no submit gate for it to release.
    fn pause<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _request: RequestId,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        self.calls.lock().expect("calls").push(ExecutorCall::Pause);
        Box::pin(future::ready(Ok(())))
    }
}

#[derive(Default)]
pub struct ClockFixture;
impl Clock for ClockFixture {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}

#[derive(Default)]
pub struct IdFixture {
    next: Mutex<u128>,
}
impl IdSource for IdFixture {
    fn next_uuid(&self) -> uuid::Uuid {
        let mut next = self.next.lock().expect("ids");
        *next += 1;
        uuid::Uuid::from_u128(*next)
    }
}

#[derive(Default)]
pub struct SinkFixture;
impl EventSinkFactory for SinkFixture {
    fn create(&self, _: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

pub type Controller = RunController<SessionStoreFixture, ExecutorFixture, ClockFixture, IdFixture>;

pub fn controller() -> (Arc<Controller>, Arc<ExecutorFixture>) {
    let store = Arc::new(SessionStoreFixture::default());
    let executor = Arc::new(ExecutorFixture::default());
    (controller_from(store, executor.clone()), executor)
}

pub fn controller_from(
    store: Arc<SessionStoreFixture>,
    executor: Arc<ExecutorFixture>,
) -> Arc<Controller> {
    Arc::new(RunController::new(
        store,
        executor.clone(),
        Arc::new(ClockFixture),
        Arc::new(IdFixture::default()),
        RunRetention::new(std::num::NonZeroUsize::new(2).expect("retention")).expect("retention"),
    ))
}

pub fn mcp_controller_fixture() -> (Arc<Controller>, Arc<ExecutorFixture>) {
    controller()
}

pub struct RootResolver;
impl McpRunRootResolver for RootResolver {
    fn resolve(&self, raw: &std::path::Path) -> Result<RunRoot, McpError> {
        RunRoot::new(raw).map_err(|_| invalid_error())
    }
}

#[derive(Default)]
pub struct ExecutionSpawner {
    pending: Mutex<Vec<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>>,
    fail: Mutex<bool>,
}
impl McpRunExecutionSpawner for ExecutionSpawner {
    fn spawn(
        &self,
        future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<(), McpError> {
        if *self.fail.lock().expect("spawner failure") {
            return Err(invalid_error());
        }
        self.pending
            .lock()
            .expect("pending executions")
            .push(future);
        Ok(())
    }
}
impl ExecutionSpawner {
    pub fn set_fail(&self, fail: bool) {
        *self.fail.lock().expect("spawner failure") = fail;
    }
    pub fn pending(&self) -> usize {
        self.pending.lock().expect("pending executions").len()
    }

    pub fn take_pending(&self) -> Vec<Pin<Box<dyn Future<Output = ()> + Send + 'static>>> {
        std::mem::take(&mut *self.pending.lock().expect("pending executions"))
    }
}

pub struct BindingsBuilder;
impl McpRunBindingsBuilder for BindingsBuilder {
    fn build_sink(
        &self,
        _: SessionId,
        _: Option<&str>,
    ) -> Result<Arc<dyn EventSinkFactory>, McpError> {
        Ok(Arc::new(SinkFixture))
    }
}

pub struct ResourceReader;
impl McpRunResourceReader for ResourceReader {
    fn read<'a>(
        &'a self,
        request: McpRunResourceRequest,
    ) -> McpFuture<'a, Result<SanitizedMcpRunResource, McpError>> {
        Box::pin(async move {
            match request {
                McpRunResourceRequest::Summary { run_id } => Ok(SanitizedMcpRunResource::Text {
                    mime_type: Arc::from("application/json"),
                    text: Arc::from(
                        json!({
                            "run_id": run_id.to_string(),
                            "revision": 1,
                            "root": ROOT,
                            "status": "settled",
                            "settlement": null,
                        })
                        .to_string(),
                    ),
                }),
                McpRunResourceRequest::Artifact { artifact_id, .. } => {
                    Ok(SanitizedMcpRunResource::Blob {
                        mime_type: Arc::from("application/octet-stream"),
                        bytes: Arc::from(format!("artifact:{artifact_id}").into_bytes()),
                    })
                }
            }
        })
    }
}

pub fn ports() -> McpRunServerPorts {
    ports_with_spawner().0
}

pub fn ports_with_spawner() -> (McpRunServerPorts, Arc<ExecutionSpawner>) {
    let spawner = Arc::new(ExecutionSpawner::default());
    let ports = McpRunServerPorts::new(
        Arc::new(RootResolver),
        spawner.clone(),
        Arc::new(BindingsBuilder),
        Arc::new(ResourceReader),
    )
    .with_compaction(Arc::new(CompactionCommandFixture));
    (ports, spawner)
}
