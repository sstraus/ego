//! RED wire contract for ACP's adaptation of the core RunController.
//!
//! This deliberately reuses the connected JSON-RPC line harness pattern from
//! `prompt.rs`. ACP owns only wire correlation. The real `Arc<RunController>`
//! owns store transactions, leases, status and interruption.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Lines};
use agent_client_protocol_schema::{ProtocolVersion, v1};
use futures::channel::mpsc;
use futures::future::{self, BoxFuture};
use futures::{SinkExt, StreamExt};

use ego_acp::question::{AcpQuestionAdapter, CapabilityCapture};
use ego_acp::serve::{
    McpSessionAdmissionError, McpSessionAdmitter, RunBindingsBuilder, SessionMcpAdmission,
};
use ego_core::config::ModelRef;
use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{EvidenceError, EvidenceWrite};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::run::{
    EventSinkFactory, IdSource, RequestId, RunAppend, RunAttachment, RunController,
    RunExecutionBindings, RunExecutor, RunExecutorError, RunHandle, RunId, RunPreparation,
    RunPrepared, RunPrompt, RunRetention, RunRevision, RunRoot, RunSelections, RunSettlement,
    RunTransaction,
};
use ego_core::sink::{EventSink, NullSink};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};

struct NoMcpAdmitter;

impl McpSessionAdmitter for NoMcpAdmitter {
    fn admit(
        &self,
        _servers: Vec<v1::McpServer>,
    ) -> BoxFuture<'static, Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError>>
    {
        Box::pin(future::ready(Ok(None)))
    }
}

#[derive(Default)]
struct AcpStore {
    entries: Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>,
    sequence: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Default)]
struct FixedRootResolver {
    calls: Mutex<Vec<PathBuf>>,
}

impl FixedRootResolver {
    fn calls(&self) -> Vec<PathBuf> {
        self.calls.lock().expect("root resolver calls").clone()
    }
}

impl ego_acp::serve::RootResolver for FixedRootResolver {
    fn resolve(&self, cwd: &Path) -> Result<RunRoot, ego_acp::serve::RootResolveError> {
        self.calls
            .lock()
            .expect("root resolver calls")
            .push(cwd.to_path_buf());
        RunRoot::new(cwd.to_string_lossy().to_string())
            .map_err(|error| ego_acp::serve::RootResolveError::new(error.to_string()))
    }
}

#[derive(Default)]
struct RecordingRunBindingsBuilder {
    creations: Arc<Mutex<usize>>,
    builds: Mutex<Vec<(v1::SessionId, CapabilityCapture)>>,
}

impl RecordingRunBindingsBuilder {
    fn creations(&self) -> usize {
        *self.creations.lock().expect("sink factory creations")
    }

    fn builds(&self) -> Vec<(v1::SessionId, CapabilityCapture)> {
        self.builds.lock().expect("bindings builds").clone()
    }
}

struct RecordingSinkFactory {
    creations: Arc<Mutex<usize>>,
    _connection: ConnectionTo<Client>,
    _session: v1::SessionId,
    _capabilities: CapabilityCapture,
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_tools: Some(128),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
    }
}

impl EventSinkFactory for RecordingSinkFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        *self.creations.lock().expect("sink factory creations") += 1;
        Arc::new(NullSink)
    }
}

impl RunBindingsBuilder for RecordingRunBindingsBuilder {
    fn build(
        &self,
        connection: ConnectionTo<Client>,
        session: v1::SessionId,
        capabilities: CapabilityCapture,
    ) -> RunExecutionBindings {
        self.builds
            .lock()
            .expect("bindings builds")
            .push((session.clone(), capabilities));
        let elicitor = AcpQuestionAdapter::live(capabilities, connection.clone()).elicitor();
        RunExecutionBindings::new(
            Arc::new(RecordingSinkFactory {
                creations: Arc::clone(&self.creations),
                _connection: connection,
                _session: session,
                _capabilities: capabilities,
            }),
            Some(elicitor),
        )
    }
}

#[derive(Default)]
struct RecordingExecutionSpawner {
    spawned: Mutex<usize>,
}

impl RecordingExecutionSpawner {
    fn spawned(&self) -> usize {
        *self.spawned.lock().expect("execution spawns")
    }
}

impl ego_acp::serve::ExecutionSpawner for RecordingExecutionSpawner {
    fn spawn(&self, execution: BoxFuture<'static, ()>) {
        *self.spawned.lock().expect("execution spawns") += 1;
        tokio::spawn(execution);
    }
}

impl AcpStore {
    fn with_sequence(sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            sequence,
        }
    }

    fn transactions(&self, id: SessionId) -> Vec<RunTransaction> {
        self.entries
            .lock()
            .expect("ACP session entries")
            .get(&id)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| match entry {
                        SessionEntry::RunTransaction(transaction) => Some(transaction.clone()),
                        SessionEntry::Event(_) | SessionEntry::Evidence(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl SessionStore for AcpStore {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("ACP session entries")
            .entry(*id)
            .or_default()
            .push(SessionEntry::Event(event.clone()));
        Ok(())
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<futures::stream::BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError>
    {
        let entries = self
            .entries
            .lock()
            .expect("ACP session entries")
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound)?;
        Ok(Box::pin(futures::stream::iter(
            entries.into_iter().map(Ok::<_, StoreError>),
        )))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(self
            .entries
            .lock()
            .expect("ACP session entries")
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
        let mut entries = self.entries.lock().expect("ACP session entries");
        let actual = entries.get(session).and_then(|entries| {
            entries.iter().rev().find_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                SessionEntry::Event(_) | SessionEntry::Evidence(_) => None,
            })
        });
        if expected_revision != actual {
            return match actual {
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
            .expect("ACP transaction sequence")
            .push("commit");
        Ok(RunAppend::Committed {
            revision: actual.map_or_else(|| RunRevision::new(1), RunRevision::next),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecutorCall {
    Prepare,
    Start(RunId),
    Recover(RunId),
    Observe(RunId),
    Submit(RunId),
    Interrupt(RunId),
    Pause(RunId),
}

struct AcpExecutor {
    calls: Mutex<Vec<ExecutorCall>>,
    settlements: Mutex<VecDeque<Result<ego_core::run::RunSettlement, RunExecutorError>>>,
    next_prepared: Mutex<u64>,
    prepared_models: Mutex<VecDeque<ModelRef>>,
    prepared_selections: Mutex<Vec<RunSelections>>,
    gate: Mutex<SubmitGate>,
    started_roots: Mutex<Vec<(RunId, RunRoot)>>,
    started_models: Mutex<Vec<(RunId, ModelRef)>>,
    bindings_received: Mutex<usize>,
    sequence: Arc<Mutex<Vec<&'static str>>>,
}

struct PreparedToken(u64);

#[derive(Default)]
struct RecordingAuthority {
    _observations: Arc<Mutex<Vec<RunId>>>,
}

#[derive(Default)]
struct SubmitGate {
    released: bool,
    interrupted: bool,
    started: bool,
    finished: bool,
    waker: Option<Waker>,
    started_waker: Option<Waker>,
    finished_waker: Option<Waker>,
}

impl Default for AcpExecutor {
    fn default() -> Self {
        Self::with_sequence(Arc::new(Mutex::new(Vec::new())))
    }
}

impl AcpExecutor {
    fn with_sequence(sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            settlements: Mutex::new(VecDeque::new()),
            next_prepared: Mutex::new(0),
            prepared_models: Mutex::new(VecDeque::new()),
            prepared_selections: Mutex::new(Vec::new()),
            gate: Mutex::new(SubmitGate::default()),
            started_roots: Mutex::new(Vec::new()),
            started_models: Mutex::new(Vec::new()),
            bindings_received: Mutex::new(0),
            sequence,
        }
    }

    fn calls(&self) -> Vec<ExecutorCall> {
        self.calls.lock().expect("ACP executor calls").clone()
    }

    fn started_roots(&self) -> Vec<(RunId, RunRoot)> {
        self.started_roots
            .lock()
            .expect("ACP executor roots")
            .clone()
    }

    fn started_models(&self) -> Vec<(RunId, ModelRef)> {
        self.started_models
            .lock()
            .expect("ACP executor models")
            .clone()
    }

    fn prepared_requests(&self) -> Vec<Option<ModelRef>> {
        self.prepared_selections
            .lock()
            .expect("ACP prepared selections")
            .iter()
            .map(|selections| selections.model().cloned())
            .collect()
    }

    fn bindings_received(&self) -> usize {
        *self
            .bindings_received
            .lock()
            .expect("ACP execution bindings")
    }

    fn allow_submit(&self) {
        let waker = {
            let mut gate = self.gate.lock().expect("ACP submit gate");
            gate.released = true;
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn reset_submit(&self) {
        let mut gate = self.gate.lock().expect("ACP submit gate");
        gate.released = false;
        gate.interrupted = false;
        gate.started = false;
        gate.finished = false;
        gate.waker = None;
        gate.started_waker = None;
        gate.finished_waker = None;
    }

    async fn wait_for_submit_start(&self) {
        futures::future::poll_fn(|cx| {
            let mut gate = self.gate.lock().expect("ACP submit gate");
            if gate.started {
                Poll::Ready(())
            } else {
                gate.started_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }

    async fn wait_for_submit_finish(&self) {
        futures::future::poll_fn(|cx| {
            let mut gate = self.gate.lock().expect("ACP submit gate");
            if gate.finished {
                Poll::Ready(())
            } else {
                gate.finished_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }

    fn settle_next(&self, settlement: RunSettlement) {
        self.settlements
            .lock()
            .expect("ACP executor settlements")
            .push_back(Ok(settlement));
        self.allow_submit();
    }

    fn any_submit_started(&self) -> bool {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .iter()
            .any(|call| matches!(call, ExecutorCall::Submit(_)))
    }

    fn submit_finished(&self) -> bool {
        self.gate.lock().expect("ACP submit gate").finished
    }
}

impl RunExecutor for AcpExecutor {
    type Prepared = PreparedToken;

    fn prepare<'a>(
        &'a self,
        request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        self.prepared_selections
            .lock()
            .expect("ACP prepared selections")
            .push(request.selections.clone());
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Prepare);
        self.sequence
            .lock()
            .expect("ACP executor sequence")
            .push("prepare");
        let mut next = self.next_prepared.lock().expect("ACP prepared sequence");
        *next += 1;
        let model = self
            .prepared_models
            .lock()
            .expect("ACP prepared models")
            .pop_front()
            .unwrap_or_else(|| ModelRef("model/test".to_owned()));
        Box::pin(future::ready(Ok(RunPrepared::new(
            model,
            PreparedToken(*next),
        ))))
    }

    fn admit_run_context<'a>(
        &'a self,
        _prepared: &'a Self::Prepared,
    ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        Box::pin(future::ready(Ok(EvidenceWrite::Published)))
    }

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        self.started_roots
            .lock()
            .expect("ACP executor roots")
            .push((request.run_id, request.root.clone()));
        self.started_models
            .lock()
            .expect("ACP executor models")
            .push((request.run_id, request.model.clone()));
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Start(request.run_id));
        self.sequence
            .lock()
            .expect("ACP executor sequence")
            .push("start");
        Box::pin(future::ready(Ok(RunHandle::new(
            request.run_id,
            Arc::new(RecordingAuthority::default()),
        ))))
    }

    fn recover<'a>(
        &'a self,
        run: &'a RunId,
    ) -> BoxFuture<'a, Result<ego_core::run::RecoveryEvidence, RunExecutorError>> {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Recover(*run));
        Box::pin(future::ready(Ok(ego_core::run::RecoveryEvidence::Running(
            RunHandle::new(*run, Arc::new(RecordingAuthority::default())),
        ))))
    }

    fn observe<'a>(
        &'a self,
        handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Observe(handle.run_id()));
        Box::pin(future::ready(Ok(RunSettlement::completed())))
    }

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        _prompt: &'a RunPrompt,
        prepared: Self::Prepared,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<ego_core::run::RunSettlement, RunExecutorError>> {
        let _prepared_serial = prepared.0;
        let _sink = bindings.sink_factory.create(&capabilities());
        *self
            .bindings_received
            .lock()
            .expect("ACP execution bindings") += 1;
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Submit(handle.run_id()));
        self.sequence
            .lock()
            .expect("ACP executor sequence")
            .push("submit");
        let gate = &self.gate;
        let mut result = self
            .settlements
            .lock()
            .expect("ACP executor settlements")
            .pop_front();
        Box::pin(futures::future::poll_fn(move |cx| {
            let (started_waker, outcome) = {
                let mut gate = gate.lock().expect("ACP submit gate");
                gate.started = true;
                let started_waker = gate.started_waker.take();
                let outcome = if gate.interrupted {
                    Some(Ok(ego_core::run::RunSettlement::cancelled(
                        ego_core::run::CancelReason::User,
                    )))
                } else if gate.released {
                    Some(
                        result
                            .take()
                            .unwrap_or_else(|| Ok(ego_core::run::RunSettlement::completed())),
                    )
                } else {
                    gate.waker = Some(cx.waker().clone());
                    None
                };
                (started_waker, outcome)
            };
            if let Some(waker) = started_waker {
                waker.wake();
            }
            if let Some(outcome) = outcome {
                let finished_waker = {
                    let mut gate = gate.lock().expect("ACP submit gate");
                    gate.finished = true;
                    gate.finished_waker.take()
                };
                if let Some(waker) = finished_waker {
                    waker.wake();
                }
                return Poll::Ready(outcome);
            }
            Poll::Pending
        }))
    }

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Interrupt(handle.run_id()));
        self.sequence
            .lock()
            .expect("ACP executor sequence")
            .push("interrupt");
        let waker = {
            let mut gate = self.gate.lock().expect("ACP submit gate");
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
        handle: &'a RunHandle,
        _request: RequestId,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Pause(handle.run_id()));
        self.sequence
            .lock()
            .expect("ACP executor sequence")
            .push("pause");
        Box::pin(future::ready(Ok(())))
    }
}

struct AcpClock {
    now: Timestamp,
}

impl Default for AcpClock {
    fn default() -> Self {
        Self {
            now: Timestamp::EPOCH,
        }
    }
}

impl Clock for AcpClock {
    fn now(&self) -> Timestamp {
        self.now
    }
}

#[derive(Default)]
struct AcpIds {
    next: Mutex<u128>,
}

impl IdSource for AcpIds {
    fn next_uuid(&self) -> uuid::Uuid {
        let mut next = self.next.lock().expect("ACP ids");
        *next += 1;
        uuid::Uuid::from_u128(*next)
    }
}

type Controller = RunController<AcpStore, AcpExecutor, AcpClock, AcpIds>;

fn controller(store: Arc<AcpStore>, executor: Arc<AcpExecutor>) -> Arc<Controller> {
    controller_with_ids(store, executor, Arc::new(AcpIds::default()))
}

fn controller_with_ids(
    store: Arc<AcpStore>,
    executor: Arc<AcpExecutor>,
    ids: Arc<AcpIds>,
) -> Arc<Controller> {
    Arc::new(RunController::new(
        store,
        executor,
        Arc::new(AcpClock::default()),
        ids,
        RunRetention::new(NonZeroUsize::new(4).expect("retention")).expect("retention"),
    ))
}

fn request(value: u128) -> RequestId {
    RequestId::from_uuid(uuid::Uuid::from_u128(value))
}

fn test_ports() -> (
    Arc<FixedRootResolver>,
    Arc<RecordingExecutionSpawner>,
    Arc<RecordingRunBindingsBuilder>,
) {
    (
        Arc::new(FixedRootResolver::default()),
        Arc::new(RecordingExecutionSpawner::default()),
        Arc::new(RecordingRunBindingsBuilder::default()),
    )
}

struct Connected {
    to_agent: mpsc::UnboundedSender<String>,
    from_agent: mpsc::UnboundedReceiver<String>,
    agent: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    next_id: u64,
}

impl Connected {
    fn with(
        controller: Arc<Controller>,
        roots: Arc<FixedRootResolver>,
        spawner: Arc<RecordingExecutionSpawner>,
        bindings_builder: Arc<RecordingRunBindingsBuilder>,
    ) -> Self {
        Self::with_ids(
            controller,
            roots,
            spawner,
            bindings_builder,
            Arc::new(AcpIds::default()),
        )
    }

    fn with_ids(
        controller: Arc<Controller>,
        roots: Arc<FixedRootResolver>,
        spawner: Arc<RecordingExecutionSpawner>,
        bindings_builder: Arc<RecordingRunBindingsBuilder>,
        ids: Arc<AcpIds>,
    ) -> Self {
        let (to_agent, to_agent_rx) = mpsc::unbounded::<String>();
        let (from_agent, from_agent_rx) = mpsc::unbounded::<String>();
        let transport = Lines::new(
            from_agent.sink_map_err(std::io::Error::other),
            to_agent_rx.map(Ok::<_, std::io::Error>),
        );
        Self {
            to_agent,
            from_agent: from_agent_rx,
            agent: tokio::spawn(
                ego_acp::serve::agent(
                    controller,
                    roots,
                    spawner,
                    ids,
                    bindings_builder,
                    Arc::new(NoMcpAdmitter) as Arc<dyn McpSessionAdmitter>,
                )
                .connect_to(transport),
            ),
            next_id: 1,
        }
    }

    async fn send(&mut self, method: &str, params: serde_json::Value) -> u64 {
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
            .expect("ACP agent listening");
        id
    }

    async fn notify(&mut self, method: &str, params: serde_json::Value) {
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
            .expect("ACP agent listening");
    }

    async fn response_to(&mut self, id: u64) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut notifications = Vec::new();
        loop {
            let line = self.from_agent.next().await.expect("ACP response");
            let message: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC JSON");
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return (message, notifications);
            }
            notifications.push(message);
        }
    }

    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let id = self.send(method, params).await;
        self.response_to(id).await
    }

    async fn open(&mut self, cwd: &str) -> String {
        self.open_with_capabilities(cwd, serde_json::json!({}))
            .await
    }

    async fn open_with_capabilities(
        &mut self,
        cwd: &str,
        client_capabilities: serde_json::Value,
    ) -> String {
        let (initialize, _) = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": ProtocolVersion::V1.as_u16(),
                    "clientCapabilities": client_capabilities,
                }),
            )
            .await;
        assert!(
            initialize.get("result").is_some(),
            "initialize: {initialize}"
        );
        let (opened, _) = self
            .request(
                "session/new",
                serde_json::json!({ "cwd": cwd, "mcpServers": [] }),
            )
            .await;
        opened["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string()
    }

    async fn finish(self) {
        self.to_agent.close_channel();
        let _ = self.agent.await;
    }

    fn close(self) -> tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>> {
        self.to_agent.close_channel();
        self.agent
    }
}

/// One reusable server boundary whose connections share durable ports and
/// identity allocation while each connection receives its own ACP router.
struct ReusableBoundary {
    controller: Arc<Controller>,
    roots: Arc<FixedRootResolver>,
    spawner: Arc<RecordingExecutionSpawner>,
    bindings_builder: Arc<RecordingRunBindingsBuilder>,
    ids: Arc<AcpIds>,
}

impl ReusableBoundary {
    fn connect(&self) -> Connected {
        Connected::with_ids(
            Arc::clone(&self.controller),
            Arc::clone(&self.roots),
            Arc::clone(&self.spawner),
            Arc::clone(&self.bindings_builder),
            Arc::clone(&self.ids),
        )
    }
}

#[tokio::test]
async fn session_new_prompt_and_baseline_cancel_notification_drive_the_shared_controller() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let (roots, spawner, sink_factory) = test_ports();
    let mut connection = Connected::with(
        Arc::clone(&controller),
        Arc::clone(&roots),
        Arc::clone(&spawner),
        Arc::clone(&sink_factory),
    );
    let session = connection.open("/workspace/project").await;
    assert_eq!(roots.calls(), vec![PathBuf::from("/workspace/project")]);
    assert!(store.entries.lock().expect("session entries").is_empty());
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "run" }],
            }),
        )
        .await;
    executor.wait_for_submit_start().await;
    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session }),
        )
        .await;
    let (response, _) = connection.response_to(prompt_id).await;
    assert_eq!(response["result"]["stopReason"], "cancelled");
    assert_eq!(sink_factory.creations(), 1);
    assert_eq!(
        sink_factory.builds(),
        vec![(
            v1::SessionId::new(session.clone()),
            CapabilityCapture::default()
        )]
    );
    assert_eq!(executor.bindings_received(), 1);
    assert_eq!(executor.prepared_requests(), vec![None]);
    assert_eq!(spawner.spawned(), 1);
    let id = session.parse::<SessionId>().expect("session identity");
    assert_eq!(id, RequestId::session_id(request(1)));
    let run_id = store
        .transactions(id)
        .first()
        .expect("start transaction")
        .run_id();
    let run = controller.status(run_id).await.expect("run status");
    assert_eq!(run.status(), ego_core::run::RunStatus::Cancelled);
    assert_eq!(
        executor.started_roots(),
        vec![(run_id, RunRoot::new("/workspace/project").expect("root"))]
    );
    assert_eq!(
        executor.started_models(),
        vec![(run_id, ModelRef("model/test".to_owned()))]
    );
    let transactions = store.transactions(id);
    assert_eq!(transactions.len(), 3);
    assert!(matches!(
        transactions[0].command(),
        ego_core::run::RunCommand::CommandAdmitted { model, .. }
            if model == &ModelRef("model/test".to_owned())
    ));
    assert_eq!(
        transactions[0].root(),
        &RunRoot::new("/workspace/project").expect("root")
    );
    assert!(matches!(
        transactions[1].command(),
        ego_core::run::RunCommand::CancelRequested { .. }
    ));
    assert!(matches!(
        transactions[2].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert_eq!(transactions[2].revision(), RunRevision::new(3));
    assert_eq!(transactions[0].status(), ego_core::run::RunStatus::Running);
    assert_eq!(
        transactions[1].status(),
        ego_core::run::RunStatus::Cancelling
    );
    assert_eq!(
        transactions[2].status(),
        ego_core::run::RunStatus::Cancelled
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        1
    );
    let sequence = sequence.lock().expect("ACP order").clone();
    assert_eq!(
        sequence.as_slice(),
        [
            "prepare",
            "commit",
            "start",
            "submit",
            "commit",
            "interrupt",
            "commit"
        ]
    );
    connection.finish().await;
}

#[tokio::test]
async fn session_new_rejects_invalid_raw_cwd_before_transaction_or_execution() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let (roots, spawner, sink_factory) = test_ports();
    let mut connection = Connected::with(
        Arc::clone(&controller),
        Arc::clone(&roots),
        Arc::clone(&spawner),
        Arc::clone(&sink_factory),
    );
    let (initialize, _) = connection
        .request(
            "initialize",
            serde_json::json!({
                "protocolVersion": ProtocolVersion::V1.as_u16(),
                "clientCapabilities": {},
            }),
        )
        .await;
    assert!(
        initialize.get("result").is_some(),
        "initialize: {initialize}"
    );
    let (response, _) = connection
        .request(
            "session/new",
            serde_json::json!({ "cwd": "relative/project", "mcpServers": [] }),
        )
        .await;
    assert!(response.get("error").is_some(), "invalid cwd: {response}");
    assert_eq!(roots.calls(), vec![PathBuf::from("relative/project")]);
    assert!(store.entries.lock().expect("session entries").is_empty());
    assert!(executor.calls().is_empty());
    assert_eq!(spawner.spawned(), 0);
    assert_eq!(sink_factory.creations(), 0);
    connection.finish().await;
}

#[tokio::test]
async fn duplicate_cancel_replays_one_receipt_and_one_interrupt() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let (roots, spawner, sink_factory) = test_ports();
    let mut connection = Connected::with(
        Arc::clone(&controller),
        Arc::clone(&roots),
        Arc::clone(&spawner),
        Arc::clone(&sink_factory),
    );
    let session = connection.open("/workspace/project").await;
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({"sessionId": session, "prompt": [{"type":"text","text":"blocked"}]}),
        )
        .await;
    executor.wait_for_submit_start().await;
    let cancel = serde_json::json!({ "sessionId": session });
    connection.notify("session/cancel", cancel.clone()).await;
    connection.notify("session/cancel", cancel).await;
    let (response, _) = connection.response_to(prompt_id).await;
    assert_eq!(response["result"]["stopReason"], "cancelled");
    let id = session.parse::<SessionId>().expect("session identity");
    let transactions = store.transactions(id);
    assert_eq!(transactions.len(), 3);
    assert!(matches!(
        transactions[0].command(),
        ego_core::run::RunCommand::CommandAdmitted { .. }
    ));
    assert!(matches!(
        transactions[1].command(),
        ego_core::run::RunCommand::CancelRequested { .. }
    ));
    assert!(matches!(
        transactions[2].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert_eq!(transactions[2].revision(), RunRevision::new(3));
    assert_eq!(transactions[0].status(), ego_core::run::RunStatus::Running);
    assert_eq!(
        transactions[1].status(),
        ego_core::run::RunStatus::Cancelling
    );
    assert_eq!(
        transactions[2].status(),
        ego_core::run::RunStatus::Cancelled
    );
    let run_id = transactions[0].run_id();
    assert_eq!(
        controller
            .status(run_id)
            .await
            .expect("cancelled status")
            .status(),
        ego_core::run::RunStatus::Cancelled
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        1
    );
    let sequence = sequence.lock().expect("ACP order").clone();
    assert_eq!(
        sequence.as_slice(),
        [
            "prepare",
            "commit",
            "start",
            "submit",
            "commit",
            "interrupt",
            "commit"
        ]
    );
    connection.finish().await;
}

/// A settlement nobody could grant is an answer, not a wait.
///
/// The executor here is a fixture that hands back an already-settled
/// `PermissionRequired` with `DeniedBy::Nobody`, so what this proves is the
/// projection: such a settlement leaves the run `Settled` and answers
/// `session/prompt` with an error, instead of parking the turn on a seat that
/// by definition cannot answer it. Whether the production path *has* a seat is
/// a different question, proved against the real adapter in
/// `permission.rs`; this test says nothing about it and its name no
/// longer claims to.
#[tokio::test]
async fn a_settlement_nobody_could_grant_answers_on_the_wire_and_settles_the_run() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    executor.settle_next(RunSettlement::permission_required(
        "foreign network access",
        ego_core::run::DeniedBy::Nobody,
    ));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let (roots, spawner, sink_factory) = test_ports();
    let mut connection = Connected::with(
        Arc::clone(&controller),
        Arc::clone(&roots),
        Arc::clone(&spawner),
        Arc::clone(&sink_factory),
    );
    let session = connection.open("/workspace/project").await;
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "need access" }],
            }),
        )
        .await;
    let (response, _) = connection.response_to(prompt_id).await;
    let message = response["error"]["message"]
        .as_str()
        .expect("permission error message");
    assert_eq!(
        message,
        "the turn stopped because foreign network access needed a permission nobody could grant"
    );
    let id = session.parse::<SessionId>().expect("session identity");
    let transactions = store.transactions(id);
    assert_eq!(transactions.len(), 2);
    assert!(matches!(
        transactions[0].command(),
        ego_core::run::RunCommand::CommandAdmitted { .. }
    ));
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert!(matches!(
        transactions[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(transactions[0].status(), ego_core::run::RunStatus::Running);
    assert_eq!(transactions[1].status(), ego_core::run::RunStatus::Settled);
    let run_id = transactions[0].run_id();
    assert_eq!(
        controller
            .status(run_id)
            .await
            .expect("permission status")
            .status(),
        ego_core::run::RunStatus::Settled
    );
    assert_eq!(
        sequence.lock().expect("permission order").as_slice(),
        ["prepare", "commit", "start", "submit", "commit"]
    );
    assert!(executor.any_submit_started());
    assert!(
        !executor
            .calls()
            .iter()
            .any(|call| matches!(call, ExecutorCall::Interrupt(_)))
    );
    connection.finish().await;
}

#[tokio::test]
async fn immediate_cancel_uses_the_first_admitted_turn_and_does_not_retarget_a_later_turn() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let (roots, spawner, sink_factory) = test_ports();
    let mut connection = Connected::with(Arc::clone(&controller), roots, spawner, sink_factory);
    let session = connection.open("/workspace/project").await;
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "run" }],
            }),
        )
        .await;
    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session }),
        )
        .await;
    let id = session.parse::<SessionId>().expect("session identity");
    executor.wait_for_submit_start().await;
    assert!(
        executor
            .calls()
            .iter()
            .any(|call| matches!(call, ExecutorCall::Submit(_)))
    );
    let (response, _) = connection.response_to(prompt_id).await;
    assert_eq!(response["result"]["stopReason"], "cancelled");
    let transactions = store.transactions(id);
    assert_eq!(transactions.len(), 3);
    assert!(matches!(
        transactions[0].command(),
        ego_core::run::RunCommand::CommandAdmitted { .. }
    ));
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert!(matches!(
        transactions[1].command(),
        ego_core::run::RunCommand::CancelRequested { .. }
    ));
    assert_eq!(transactions[2].revision(), RunRevision::new(3));
    assert!(matches!(
        transactions[2].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(transactions[0].status(), ego_core::run::RunStatus::Running);
    assert_eq!(
        transactions[1].status(),
        ego_core::run::RunStatus::Cancelling
    );
    assert_eq!(
        transactions[2].status(),
        ego_core::run::RunStatus::Cancelled
    );
    let run_id = transactions[0].run_id();
    assert_eq!(
        controller
            .status(run_id)
            .await
            .expect("successful status")
            .status(),
        ego_core::run::RunStatus::Cancelled
    );
    assert_eq!(
        sequence.lock().expect("successful order").as_slice(),
        [
            "prepare",
            "commit",
            "commit",
            "start",
            "interrupt",
            "submit",
            "commit"
        ]
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        1
    );

    executor.reset_submit();
    let later_prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "later" }],
            }),
        )
        .await;
    executor.wait_for_submit_start().await;
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        1
    );
    executor.allow_submit();
    let (later_response, _) = connection.response_to(later_prompt_id).await;
    assert_eq!(later_response["result"]["stopReason"], "end_turn");
    let later_transactions = store.transactions(id);
    assert_eq!(later_transactions.len(), 5);
    assert_eq!(later_transactions[3].revision(), RunRevision::new(4));
    assert_eq!(later_transactions[4].revision(), RunRevision::new(5));
    assert!(matches!(
        later_transactions[3].command(),
        ego_core::run::RunCommand::CommandAdmitted { .. }
    ));
    assert!(matches!(
        later_transactions[4].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        1
    );
    connection.finish().await;
}

#[tokio::test]
async fn unknown_and_between_turn_cancel_notifications_have_no_store_or_executor_effect() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let (roots, spawner, sink_factory) = test_ports();
    let mut connection = Connected::with(Arc::clone(&controller), roots, spawner, sink_factory);
    let session = connection.open("/workspace/project").await;
    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": "00000000-0000-0000-0000-000000000099" }),
        )
        .await;
    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session }),
        )
        .await;
    let id = session.parse::<SessionId>().expect("session identity");
    assert!(store.transactions(id).is_empty());
    assert!(executor.calls().is_empty());
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "later" }],
            }),
        )
        .await;
    executor.wait_for_submit_start().await;
    assert!(
        !executor
            .calls()
            .iter()
            .any(|call| matches!(call, ExecutorCall::Interrupt(_)))
    );
    executor.allow_submit();
    let (response, _) = connection.response_to(prompt_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let transactions = store.transactions(id);
    assert_eq!(transactions.len(), 2);
    assert!(matches!(
        transactions[0].command(),
        ego_core::run::RunCommand::CommandAdmitted { .. }
    ));
    assert_eq!(transactions[0].revision(), RunRevision::new(1));
    assert_eq!(transactions[1].revision(), RunRevision::new(2));
    assert!(matches!(
        transactions[1].command(),
        ego_core::run::RunCommand::CommandSettled { .. }
    ));
    assert_eq!(transactions[0].status(), ego_core::run::RunStatus::Running);
    assert_eq!(transactions[1].status(), ego_core::run::RunStatus::Settled);
    let run_id = transactions[0].run_id();
    assert_eq!(
        controller
            .status(run_id)
            .await
            .expect("successful status")
            .status(),
        ego_core::run::RunStatus::Settled
    );
    assert_eq!(
        sequence.lock().expect("successful order").as_slice(),
        ["prepare", "commit", "start", "submit", "commit"]
    );
    assert!(
        !executor
            .calls()
            .iter()
            .any(|call| matches!(call, ExecutorCall::Interrupt(_)))
    );
    connection.finish().await;
}

#[tokio::test]
async fn disconnect_detaches_wire_subscription_without_cancelling_the_durable_run() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let (roots, spawner, sink_factory) = test_ports();
    let mut connection = Connected::with(
        Arc::clone(&controller),
        Arc::clone(&roots),
        Arc::clone(&spawner),
        Arc::clone(&sink_factory),
    );
    let session = connection.open("/workspace/project").await;
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({"sessionId": session, "prompt": [{"type":"text","text":"run"}]}),
        )
        .await;
    assert!(prompt_id > 0);
    executor.wait_for_submit_start().await;
    assert!(executor.any_submit_started());
    connection.finish().await;
    let id = session.parse::<SessionId>().expect("session identity");
    let run_id = store
        .transactions(id)
        .first()
        .expect("run transaction after disconnect")
        .run_id();
    assert_eq!(store.transactions(id).len(), 1);
    assert_eq!(store.transactions(id)[0].revision(), RunRevision::new(1));
    let run = controller
        .status(run_id)
        .await
        .expect("run after disconnect");
    assert_eq!(run.status(), ego_core::run::RunStatus::Running);
    assert!(
        !executor
            .calls()
            .iter()
            .any(|call| matches!(call, ExecutorCall::Interrupt(_)))
    );
    assert_eq!(
        sequence.lock().expect("disconnect order").as_slice(),
        ["prepare", "commit", "start", "submit"]
    );
    assert_eq!(spawner.spawned(), 1);
}

#[tokio::test]
async fn reusable_boundary_scopes_two_connections_and_preserves_disconnected_execution() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let ids = Arc::new(AcpIds::default());
    let (roots, spawner, bindings_builder) = test_ports();
    let boundary = ReusableBoundary {
        controller: controller_with_ids(
            Arc::clone(&store),
            Arc::clone(&executor),
            Arc::clone(&ids),
        ),
        roots: Arc::clone(&roots),
        spawner: Arc::clone(&spawner),
        bindings_builder: Arc::clone(&bindings_builder),
        ids,
    };

    let mut connection_a = boundary.connect();
    let mut connection_b = boundary.connect();
    let session_a = connection_a.open("/workspace/a").await;
    let prompt_a = connection_a
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session_a,
                "prompt": [{ "type": "text", "text": "A" }],
            }),
        )
        .await;
    assert!(prompt_a > 0);
    executor.wait_for_submit_start().await;

    let session_b = connection_b
        .open_with_capabilities(
            "/workspace/b",
            serde_json::json!({ "elicitation": { "form": {} } }),
        )
        .await;
    assert_ne!(
        session_a, session_b,
        "each connection must receive its own session"
    );
    let (cross_connection, _) = connection_b
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session_a,
                "prompt": [{ "type": "text", "text": "B targeting A" }],
            }),
        )
        .await;
    assert!(
        cross_connection.get("error").is_some(),
        "connection B must not prompt connection A's session: {cross_connection}"
    );
    connection_b
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session_a }),
        )
        .await;
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        0,
        "connection B must not cancel connection A's run"
    );

    let mut agent_a = connection_a.close();
    let joined_a = tokio::time::timeout(std::time::Duration::from_secs(1), &mut agent_a).await;
    assert!(
        joined_a.is_ok(),
        "disconnect must let the ACP router join while submit remains blocked"
    );
    assert!(
        !executor.submit_finished(),
        "closing the ACP connection must not release its independent execution"
    );
    let (after_disconnect, _) = connection_b
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session_a,
                "prompt": [{ "type": "text", "text": "B after disconnect" }],
            }),
        )
        .await;
    assert!(
        after_disconnect.get("error").is_some(),
        "a fresh connection must not use a disconnected session: {after_disconnect}"
    );
    let id_a = session_a.parse::<SessionId>().expect("session A identity");
    let run_a = store
        .transactions(id_a)
        .first()
        .expect("A admission survives disconnect")
        .run_id();
    assert_eq!(
        boundary
            .controller
            .status(run_a)
            .await
            .expect("A status after disconnect")
            .status(),
        ego_core::run::RunStatus::Running
    );
    executor.settle_next(RunSettlement::completed());
    executor.wait_for_submit_finish().await;
    assert_eq!(store.transactions(id_a).len(), 2);
    assert_eq!(
        boundary
            .controller
            .status(run_a)
            .await
            .expect("A status after durable settlement")
            .status(),
        ego_core::run::RunStatus::Settled
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        0,
        "disconnect must detach A without interrupting its execution"
    );
    assert_eq!(
        bindings_builder.builds(),
        vec![(
            v1::SessionId::new(session_a.clone()),
            CapabilityCapture::default()
        )],
        "B's capabilities must not retroactively affect A's bindings"
    );

    executor.reset_submit();
    let prompt_b = connection_b
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session_b,
                "prompt": [{ "type": "text", "text": "B" }],
            }),
        )
        .await;
    executor.wait_for_submit_start().await;
    executor.settle_next(RunSettlement::completed());
    let (response_b, _) = connection_b.response_to(prompt_b).await;
    assert_eq!(response_b["result"]["stopReason"], "end_turn");
    assert_eq!(
        bindings_builder.builds(),
        vec![
            (v1::SessionId::new(session_a), CapabilityCapture::default()),
            (v1::SessionId::new(session_b), CapabilityCapture::form()),
        ],
        "capability capture must remain connection-scoped"
    );
    connection_b.finish().await;
}
