//! RED contract for the concrete MCP continuation owner.
//!
//! This crosses the real stdio operation and guarded-authority seams.  It
//! intentionally remains RED until `McpContinuationOwner` owns both the
//! asynchronous input-required projection and accepted-answer resume.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_core::config::{
    ConfigLayer, Mode, PermCfg, ResolvedMcpEntry, Ruleset, Source, Trust, resolve,
};
use ego_core::event::hook::{
    HookActivation, HookDecision, HookInvocationId, HookLifecycle, HookOutcome, HookPlan,
    HookResult,
};
use ego_core::event::id::Producer;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::hook::{
    HookCatalogue, HookEvent, HookFuture, HookLifecycleDispatcher, HookOccurrence, HookPort,
    HookReachFuture, HookReachedEvidence, HookRecordFuture, HookRunner, HookSpec, HookStartFuture,
    HookStartedEvidence,
};
use ego_core::mcp::ToolExecution;
use ego_core::permit::{
    Authorized, CanonPath, Classification, CleanupCertainty, DnsResolution, DuplexEnd,
    DuplexHandle, DuplexSession, DuplexSpawner, GuardedEffects, OneHop, OneHopResponse,
    PolicyEngine, PrepareCtx, PreparedCall, ProcessError, ProcessExecutor, Resolver, SpawnSpec,
    Tool, ToolResult, TransportError, TransportFailure,
};
use ego_core::store::blob::{BlobError, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter};
use ego_core::store::evidence::{
    EvidenceError, EvidenceId, EvidencePage, EvidenceRead, EvidenceRef, EvidenceStore,
    EvidenceWriter,
};
use ego_core::suspension::{
    ClaimedContinuation, ContinuationError, ContinuationJournal, ContinuationRecord,
    ContinuationState, ResumeKeySource, ResumeSealer,
};
use ego_core::turn::{
    McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError, ToolEffectPorts,
};
use ego_mcp::{
    McpAdmittedCandidate, McpCacheMetadata, McpCandidateAdmission, McpCandidateError,
    McpCandidateIdentity, McpCandidateInventory, McpCapabilities, McpContinuationOwner, McpFuture,
    McpInputMapping, McpInputRequired, McpRegistryCandidate, McpRegistryCapture,
    McpRegistryController, McpRegistryRefresh, McpServerAdmission, McpStdioOperationFactory,
    McpTransportMaterial, RegistryScope, RegistryTrigger,
};
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite};
use serde_json::json;

const SCHEMA: &[u8] = br#"{"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"],"additionalProperties":false}"#;

const MCP_OWNERS: [(&str, HookLifecycle); 2] = [
    (
        "McpContinuationOwner::record_input_required",
        HookLifecycle::Elicitation,
    ),
    (
        "McpContinuationOwner::accept_answer",
        HookLifecycle::ElicitationResult,
    ),
];

#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<String>>>);
impl Trace {
    fn push(&self, item: impl Into<String>) {
        self.0.lock().expect("trace").push(item.into());
    }
    fn events(&self) -> Vec<String> {
        self.0.lock().expect("trace").clone()
    }
}

#[derive(Clone, Copy, Default)]
enum HookMode {
    #[default]
    Continue,
    BlockResult,
    UncertainResult,
}

#[derive(Clone, Default)]
struct RecordingPort {
    calls: Arc<Mutex<Vec<HookLifecycle>>>,
    trace: Trace,
    mode: HookMode,
}
impl RecordingPort {
    fn new(trace: Trace, mode: HookMode) -> Self {
        Self {
            calls: Arc::default(),
            trace,
            mode,
        }
    }
}
impl HookPort for RecordingPort {
    fn reached<'a>(
        &'a self,
        producer: &'a Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        Box::pin(std::future::ready(Ok(
            ego_core::hook::HookReachOutcome::Fresh(HookReachedEvidence::new(
                ego_core::event::id::EventId::new(),
                occurrence.key().clone(),
                occurrence.event().subject(),
                producer.clone(),
                plan.clone(),
            )),
        )))
    }
    fn started<'a>(
        &'a self,
        spec: &'a HookSpec,
        occurrence: &'a HookOccurrence,
        reached: &'a HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> HookStartFuture<'a> {
        let lifecycle = occurrence.event().lifecycle();
        self.calls.lock().expect("hook calls").push(lifecycle);
        self.trace.push(lifecycle.as_str());
        Box::pin(std::future::ready(Ok(HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        ))))
    }
    fn invoke<'a>(&'a self, _spec: &'a HookSpec, event: HookEvent) -> HookFuture<'a> {
        let decision = match (self.mode, event.lifecycle()) {
            (HookMode::BlockResult, HookLifecycle::ElicitationResult) => HookDecision::Block,
            _ => HookDecision::Continue,
        };
        Box::pin(std::future::ready(Ok(HookOutcome::new(decision, None))))
    }
    fn settled<'a>(
        &'a self,
        spec: &'a HookSpec,
        _started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        if matches!(self.mode, HookMode::UncertainResult)
            && spec.event_name() == HookLifecycle::ElicitationResult.as_str()
        {
            return Box::pin(std::future::ready(Err(ego_core::hook::HookRunError::new(
                "simulated delivery uncertainty",
            ))));
        }
        Box::pin(std::future::ready(Ok(())))
    }
}

fn hooks_with_mode(mode: HookMode, trace: Trace) -> HookLifecycleDispatcher {
    let source = "[[hooks]]\nevent = \"Elicitation\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n[[hooks]]\nevent = \"ElicitationResult\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n";
    let catalogue = HookCatalogue::admit_typed(
        &[ConfigLayer::parse(Source::User, source).expect("hooks")],
        &[],
        Trust::Trusted,
    )
    .expect("catalogue");
    HookLifecycleDispatcher::new(
        HookRunner::new_with_durable_started(RecordingPort::new(trace, mode), Vec::new()),
        catalogue,
    )
}

#[derive(Default)]
struct Journal {
    events: Mutex<Vec<&'static str>>,
    record: Mutex<Option<ContinuationRecord>>,
    trace: Trace,
}
impl Journal {
    fn with_trace(trace: Trace) -> Self {
        Self {
            events: Mutex::default(),
            record: Mutex::default(),
            trace,
        }
    }
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().expect("journal events").clone()
    }
    fn state(&self) -> Option<ContinuationState> {
        self.record
            .lock()
            .expect("record")
            .as_ref()
            .map(ContinuationRecord::state)
    }
    fn id(&self) -> String {
        self.record
            .lock()
            .expect("record")
            .as_ref()
            .expect("recorded continuation")
            .id()
            .to_owned()
    }
    fn push(&self, event: &'static str) {
        self.events.lock().expect("journal events").push(event);
        self.trace.push(event);
    }
}
impl ContinuationJournal for Journal {
    fn record(&self, record: ContinuationRecord) -> Result<(), ContinuationError> {
        let mut slot = self.record.lock().expect("record");
        if slot.is_some() {
            return Err(ContinuationError::InvalidTransition);
        }
        *slot = Some(record);
        self.push("record");
        Ok(())
    }
    fn accept_answer(
        &self,
        id: &str,
        answer: &[u8],
        now: Timestamp,
    ) -> Result<(), ContinuationError> {
        let mut slot = self.record.lock().expect("record");
        let record = slot.as_mut().ok_or(ContinuationError::NotFound)?;
        if record.id() != id {
            return Err(ContinuationError::NotFound);
        }
        record.accept_answer(answer.to_vec(), now)?;
        self.push("accepted_answer");
        Ok(())
    }
    fn load(&self, id: &str) -> Result<ContinuationRecord, ContinuationError> {
        let slot = self.record.lock().expect("record");
        let record = slot
            .as_ref()
            .filter(|record| record.id() == id)
            .ok_or(ContinuationError::NotFound)?;
        serde_json::from_slice(
            &serde_json::to_vec(record).map_err(|_| ContinuationError::Corrupt("encode".into()))?,
        )
        .map_err(|_| ContinuationError::Corrupt("decode".into()))
    }
    fn claim(&self, id: &str, now: Timestamp) -> Result<ClaimedContinuation, ContinuationError> {
        let mut slot = self.record.lock().expect("record");
        let record = slot.take().ok_or(ContinuationError::NotFound)?;
        if record.id() != id {
            *slot = Some(record);
            return Err(ContinuationError::NotFound);
        }
        let bytes =
            serde_json::to_vec(&record).map_err(|_| ContinuationError::Corrupt("encode".into()))?;
        let claimed = match record.claim(now) {
            Ok(claimed) => claimed,
            Err(error) => {
                *slot = Some(
                    serde_json::from_slice(&bytes)
                        .map_err(|_| ContinuationError::Corrupt("decode".into()))?,
                );
                return Err(error);
            }
        };
        self.push("claim");
        Ok(claimed)
    }
    fn settle(
        &self,
        _claimed: ClaimedContinuation,
        _state: ContinuationState,
    ) -> Result<(), ContinuationError> {
        self.push("settle");
        Ok(())
    }
}

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}
struct FixedResumeKey;
impl ResumeKeySource for FixedResumeKey {
    fn resume_key(&self) -> Result<Arc<[u8]>, ContinuationError> {
        Ok(Arc::from(*b"mcp-hook-lifecycle-test-key-32bytes!"))
    }
}

struct NoProcess;
impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(std::future::ready(Err(ProcessError::Invariant(
            "fixture does not execute processes".into(),
        ))))
    }
}
struct EmptyBlobs;
impl BlobStore for EmptyBlobs {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        Err(BlobError::Limit { limit: 0 })
    }
    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Err(BlobError::NotFound {
            reference: reference.id.clone(),
        })
    }
    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        Box::pin(std::future::ready(Err(BlobError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }
    fn collect_garbage(&self, _reachable: &[BlobRef]) -> Result<usize, BlobError> {
        Ok(0)
    }
}
static EMPTY_BLOBS: EmptyBlobs = EmptyBlobs;
struct EmptyEvidence;
struct FixtureEvidenceWriter {
    bytes: Vec<u8>,
}
impl EvidenceWriter for FixtureEvidenceWriter {
    fn append(&mut self, chunk: &[u8]) -> Result<(), EvidenceError> {
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<EvidenceRef, EvidenceError> {
        Ok(EvidenceRef::new(
            EvidenceId::parse("0000000000000000000000000000000000000000000000000000000000000000")
                .expect("fixture evidence id"),
            self.bytes.len() as u64,
        ))
    }

    fn abort(self: Box<Self>) -> Result<(), EvidenceError> {
        Ok(())
    }
}
impl EvidenceStore for EmptyEvidence {
    fn blob_store(&self) -> &dyn BlobStore {
        &EMPTY_BLOBS
    }
    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Ok(Box::new(FixtureEvidenceWriter { bytes: Vec::new() }))
    }
    fn read_page<'a>(
        &'a self,
        _request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(std::future::ready(Err(EvidenceError::Limit { limit: 0 })))
    }
}
struct OfflineResolver;
impl Resolver for OfflineResolver {
    fn resolve(
        &self,
        _target: ego_core::permit::NetworkTarget,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            TransportFailure::Offline,
        ))))
    }
}
struct OfflineOneHop;
impl OneHop for OfflineOneHop {
    fn execute(
        &self,
        _connect: ego_core::permit::ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            TransportFailure::Offline,
        ))))
    }
}

struct ScriptedDuplex {
    input: VecDeque<u8>,
    output: Vec<u8>,
    reader_waker: Option<std::task::Waker>,
    complete: bool,
}
impl ScriptedDuplex {
    fn new(complete: bool) -> Self {
        Self {
            input: VecDeque::new(),
            output: Vec::new(),
            reader_waker: None,
            complete,
        }
    }

    fn enqueue_response(&mut self, request: &serde_json::Value, result: serde_json::Value) {
        let response = json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
            "result": result,
        });
        self.input
            .extend(serde_json::to_vec(&response).expect("response"));
        self.input.push_back(b'\n');
        if let Some(waker) = self.reader_waker.take() {
            waker.wake();
        }
    }

    fn respond_to(&mut self, request: &serde_json::Value) {
        match request.get("method").and_then(serde_json::Value::as_str) {
            Some("server/discover") => self.enqueue_response(
                request,
                json!({
                    "resultType":"complete",
                    "supportedVersions":["2026-07-28"],
                    "capabilities":{"tools":{"listChanged":false}},
                    "ttlMs":0,
                    "cacheScope":"private",
                    "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"local","version":"1.0"}}
                }),
            ),
            Some("tools/list") => self.enqueue_response(
                request,
                json!({
                    "resultType":"complete",
                    "tools":[{
                        "name":"lookup",
                        "description":"continuation fixture",
                        "inputSchema":{"type":"object","additionalProperties":false}
                    }],
                    "ttlMs":0,
                    "cacheScope":"private"
                }),
            ),
            Some("tools/call") => {
                let result = if self.complete {
                    json!({"resultType":"complete","content":[{"type":"text","text":"retry-complete"}],"isError":false})
                } else {
                    json!({"resultType":"input_required","inputRequests":{"answer":{"method":"elicitation/create","params":{"mode":"form","message":"confirm","requestedSchema":{"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"],"additionalProperties":false}}}},"requestState":"opaque-state"})
                };
                self.enqueue_response(request, result);
            }
            Some("notifications/initialized") | None => {}
            Some(_) => {}
        }
    }
}
impl AsyncRead for ScriptedDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.input.is_empty() {
            self.reader_waker = Some(cx.waker().clone());
            return std::task::Poll::Pending;
        }
        let amount = bytes.len().min(self.input.len());
        for slot in &mut bytes[..amount] {
            *slot = self.input.pop_front().expect("input byte");
        }
        std::task::Poll::Ready(Ok(amount))
    }
}
impl AsyncWrite for ScriptedDuplex {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.output.extend_from_slice(bytes);
        while let Some(index) = self.output.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.output.drain(..=index).collect();
            if let Ok(request) = serde_json::from_slice::<serde_json::Value>(&line[..index]) {
                self.respond_to(&request);
            }
        }
        let _ = cx;
        std::task::Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let _ = cx;
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let _ = cx;
        std::task::Poll::Ready(Ok(()))
    }
}
impl DuplexSession for ScriptedDuplex {
    fn delivery(&self) -> DeliveryCertainty {
        DeliveryCertainty::DefinitelyUnsent
    }
    fn settle(self: Box<Self>) -> BoxFuture<'static, ego_core::permit::DuplexSettlement> {
        Box::pin(std::future::ready(ego_core::permit::DuplexSettlement::new(
            DuplexEnd::Exited { code: 0 },
            CleanupCertainty::Complete,
            DeliveryCertainty::DefinitelyUnsent,
            Ok(ToolResult::builtin("settled")),
        )))
    }
    fn interrupt(self: Box<Self>) -> BoxFuture<'static, ego_core::permit::DuplexSettlement> {
        Box::pin(std::future::ready(ego_core::permit::DuplexSettlement::new(
            DuplexEnd::Interrupted,
            CleanupCertainty::Complete,
            DeliveryCertainty::DefinitelyUnsent,
            Ok(ToolResult::builtin("interrupted")),
        )))
    }
}
#[derive(Clone)]
struct ScriptedSpawner {
    calls: Arc<AtomicUsize>,
}
impl ScriptedSpawner {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
impl DuplexSpawner for ScriptedSpawner {
    fn spawn(&self, _spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        let complete = self.calls.fetch_add(1, Ordering::SeqCst) > 0;
        Box::pin(std::future::ready(Ok(
            Box::new(ScriptedDuplex::new(complete)) as DuplexHandle,
        )))
    }
}

fn server_admission() -> Arc<McpServerAdmission> {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.server]\ntransport='stdio'\ncommand='mcp-server'\n",
    )
    .expect("config");
    let resolved = resolve(&[layer], Trust::Trusted, None)
        .expect("resolved")
        .mcp;
    let (name, config) = resolved
        .entries()
        .iter()
        .find_map(|(name, entry)| match entry {
            ResolvedMcpEntry::Configured { server, .. } => Some((name.clone(), server.clone())),
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .expect("server");
    Arc::new(
        McpServerAdmission::new(name, config, McpTransportMaterial::stdio(Vec::new()))
            .expect("admission"),
    )
}
fn captured_lookup(factory: Arc<McpStdioOperationFactory>) -> (McpRegistryCapture, Arc<dyn Tool>) {
    let capture = registry_capture(factory);
    let tool = capture
        .tools()
        .iter()
        .find(|tool| tool.descriptor().name == "lookup")
        .cloned()
        .expect("captured lookup tool");
    (capture, tool)
}
fn prepare_lookup(tool: &Arc<dyn Tool>) -> PreparedCall {
    let resolver = ego_core::permit::fake::FakeFs::default();
    let ctx = PrepareCtx {
        cwd: CanonPath::new("/fixture"),
        resolver: &resolver,
    };
    tool.prepare(json!({}), &ctx).expect("prepare")
}
fn authorized(call: PreparedCall) -> Authorized {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Yolo,
        Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![
                    ego_core::permit::rule::Rule::load("tool(server/lookup)").expect("tool rule"),
                    ego_core::permit::rule::Rule::load("exec(mcp-server *)").expect("process rule"),
                ],
                deny: Vec::new(),
            },
            Source::Project,
        ),
    );
    let Classification::Settled(Ok(authorized)) = policy.classify(call) else {
        panic!("call must be authorized")
    };
    authorized
}
fn mapped_input() -> ego_mcp::McpMappedInput {
    McpInputMapping::map(
        McpInputRequired::elicitation(SCHEMA, "answer", b"opaque-state").expect("elicitation"),
    )
    .expect("mapped input")
}
fn prepared_resume(authorized: &Authorized) -> (ResumeSealer, ego_core::suspension::ResumeFloor) {
    let policy = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Yolo, Ruleset::default());
    let floor = policy.resume_floor();
    let sealer =
        ResumeSealer::new(Arc::new(FixedResumeKey), Arc::new(FixedClock), 30_000).expect("sealer");
    let _ = authorized;
    (sealer, floor)
}
fn ports(spawner: Arc<ScriptedSpawner>) -> ToolEffectPorts<'static> {
    let fs: &'static ego_core::permit::fake::FakeFs =
        Box::leak(Box::new(ego_core::permit::fake::FakeFs::default()));
    let process: &'static NoProcess = Box::leak(Box::new(NoProcess));
    let evidence: &'static EmptyEvidence = Box::leak(Box::new(EmptyEvidence));
    ToolEffectPorts::new(
        fs,
        process,
        evidence,
        Arc::new(OfflineResolver),
        Arc::new(OfflineOneHop),
        spawner,
    )
}
#[test]
fn mcp_matrix_has_one_real_owner_for_each_mrtr_lifecycle() {
    assert_eq!(
        MCP_OWNERS,
        [
            (
                "McpContinuationOwner::record_input_required",
                HookLifecycle::Elicitation
            ),
            (
                "McpContinuationOwner::accept_answer",
                HookLifecycle::ElicitationResult
            )
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn real_operation_records_before_elicitation_and_returns_foreign_id() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let owner = Arc::new(McpContinuationOwner::new(
        journal.clone(),
        hooks_with_mode(HookMode::Continue, trace.clone()),
        Arc::new(FixedResumeKey),
        Arc::new(FixedClock),
    ));
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (_capture, tool) = captured_lookup(Arc::clone(&factory));
    let call = prepare_lookup(&tool);
    let auth = authorized(call);
    let (sealer, floor) = prepared_resume(&auth);
    let resume = sealer.prepare(&auth, floor).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new_with_resume(
        permit,
        Box::leak(Box::new(ego_core::permit::fake::FakeFs::default())),
        Box::leak(Box::new(NoProcess)),
        Box::leak(Box::new(EmptyEvidence)),
        Arc::new(OfflineResolver),
        Arc::new(OfflineOneHop),
        spawner.clone(),
        Some(resume),
    );
    let outcome = tool.execute(call, &guarded).await.expect("outcome");
    let ToolExecution::InputRequired(foreign) = outcome else {
        panic!("expected input-required")
    };
    assert_eq!(foreign.continuation_id(), journal.id());
    assert_eq!(journal.events(), vec!["record"]);
    assert_eq!(trace.events(), vec!["record", "Elicitation"]);
    assert_eq!(spawner.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_answer_validates_hooks_claims_and_retries_once() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let owner = Arc::new(McpContinuationOwner::new(
        journal.clone(),
        hooks_with_mode(HookMode::Continue, trace.clone()),
        Arc::new(FixedResumeKey),
        Arc::new(FixedClock),
    ));
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    let call = prepare_lookup(&tool);
    let auth = authorized(call);
    let (sealer, floor) = prepared_resume(&auth);
    let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new_with_resume(
        permit,
        Box::leak(Box::new(ego_core::permit::fake::FakeFs::default())),
        Box::leak(Box::new(NoProcess)),
        Box::leak(Box::new(EmptyEvidence)),
        Arc::new(OfflineResolver),
        Arc::new(OfflineOneHop),
        spawner.clone(),
        Some(resume),
    );
    let ToolExecution::InputRequired(foreign) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected required")
    };
    let id = foreign.continuation_id().to_owned();
    let retried = owner
        .accept_answer(
            &id,
            br#"{"choice":"yes"}"#,
            capture,
            floor,
            ports(spawner.clone()),
        )
        .await
        .expect("resume");
    assert!(matches!(retried, ToolExecution::Complete(_)));
    assert_eq!(
        journal.events(),
        vec!["record", "accepted_answer", "claim", "settle"]
    );
    assert_eq!(
        trace.events(),
        vec![
            "record",
            "Elicitation",
            "accepted_answer",
            "ElicitationResult",
            "claim",
            "settle"
        ]
    );
    assert_eq!(spawner.calls(), 2);
    assert_eq!(journal.state(), None);
}

#[test]
fn schema_or_authority_failure_precedes_durable_acceptance() {
    let journal = Arc::new(Journal::default());
    let owner = Arc::new(McpContinuationOwner::new(
        journal.clone(),
        hooks_with_mode(HookMode::Continue, Trace::default()),
        Arc::new(FixedResumeKey),
        Arc::new(FixedClock),
    ));
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    let call = prepare_lookup(&tool);
    let auth = authorized(call.clone());
    let (sealer, floor) = prepared_resume(&auth);
    let mapped = mapped_input();
    let envelope = call.mcp_envelope().expect("envelope").clone();
    journal
        .record(
            sealer
                .prepare(&auth, floor.clone())
                .expect("resume")
                .into_record(
                    "continuation-schema-authority",
                    envelope.parameters().to_vec(),
                    mapped.request_state().to_vec(),
                    mapped.correlation_key(),
                    serde_json::to_vec(mapped.schema()).expect("schema"),
                    envelope.transport_fingerprint(),
                )
                .expect("record"),
        )
        .expect("journal");
    assert!(
        block_on(owner.accept_answer(
            "continuation-schema-authority",
            br#"{"unexpected":true}"#,
            capture,
            floor,
            ports(Arc::new(ScriptedSpawner::new()))
        ))
        .is_err()
    );
    assert_eq!(journal.state(), Some(ContinuationState::Pending));
    assert_eq!(journal.events(), vec!["record"]);
}

#[tokio::test(flavor = "current_thread")]
async fn pre_block_or_uncertainty_leaves_accepted_answer_unclaimed_without_reentry() {
    for mode in [HookMode::BlockResult, HookMode::UncertainResult] {
        let trace = Trace::default();
        let journal = Arc::new(Journal::with_trace(trace.clone()));
        let owner = Arc::new(McpContinuationOwner::new(
            journal.clone(),
            hooks_with_mode(mode, trace.clone()),
            Arc::new(FixedResumeKey),
            Arc::new(FixedClock),
        ));
        let factory = Arc::new(
            McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
                .expect("factory"),
        );
        let (capture, tool) = captured_lookup(Arc::clone(&factory));
        let call = prepare_lookup(&tool);
        let auth = authorized(call);
        let (sealer, floor) = prepared_resume(&auth);
        let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
        let spawner = Arc::new(ScriptedSpawner::new());
        let (call, permit) = auth.into_parts();
        let guarded = GuardedEffects::new_with_resume(
            permit,
            Box::leak(Box::new(ego_core::permit::fake::FakeFs::default())),
            Box::leak(Box::new(NoProcess)),
            Box::leak(Box::new(EmptyEvidence)),
            Arc::new(OfflineResolver),
            Arc::new(OfflineOneHop),
            spawner.clone(),
            Some(resume),
        );
        let ToolExecution::InputRequired(foreign) =
            tool.execute(call, &guarded).await.expect("required")
        else {
            panic!("expected required")
        };
        assert!(
            owner
                .accept_answer(
                    foreign.continuation_id(),
                    br#"{"choice":"yes"}"#,
                    capture,
                    floor,
                    ports(spawner.clone())
                )
                .await
                .is_err()
        );
        assert_eq!(journal.state(), Some(ContinuationState::Accepted));
        assert!(!journal.events().contains(&"claim"));
        assert_eq!(spawner.calls(), 1);
        assert_eq!(
            journal.trace.events(),
            vec![
                "record",
                "Elicitation",
                "accepted_answer",
                "ElicitationResult"
            ]
        );
    }
}

#[derive(Default)]
struct RegistryJournal {
    events: Mutex<Vec<McpRegistryEvent>>,
}
impl McpRegistryJournal for RegistryJournal {
    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        self.events.lock().expect("registry").push(event);
        Ok(())
    }
    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        Ok(self.events.lock().expect("registry").clone())
    }
    fn begin_refresh(
        &self,
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
    ) -> Result<u64, McpRegistryJournalError> {
        self.events
            .lock()
            .expect("registry")
            .push(McpRegistryEvent::RefreshRequested {
                request_id,
                trigger,
                server_scope,
                config_revision: 1,
            });
        Ok(1)
    }
}
struct OneCandidate(Mutex<Option<McpAdmittedCandidate>>);
impl McpCandidateAdmission for OneCandidate {
    fn admit(
        &self,
        _request: ego_mcp::McpCandidateRequest,
    ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
        let candidate = self.0.lock().expect("candidate").take();
        Box::pin(async move {
            candidate.ok_or_else(|| McpCandidateError::Invalid {
                cause: "candidate already used".into(),
            })
        })
    }
}

fn registry_capture(factory: Arc<McpStdioOperationFactory>) -> McpRegistryCapture {
    let admission = server_admission();
    let inventory = McpCandidateInventory::new(
        McpCapabilities::tools(),
        vec![ego_core::generator::types::ToolDescriptor {
            name: "lookup".into(),
            description: "continuation fixture".into(),
            parameters: json!({"type":"object","additionalProperties":false}),
        }],
        Vec::new(),
        Vec::new(),
        McpCacheMetadata::default(),
        None,
    );
    let candidate = McpAdmittedCandidate::new(
        Arc::clone(&admission),
        McpCandidateIdentity::validated("server"),
        inventory,
        factory,
    )
    .expect("candidate");
    let candidate_port = Arc::new(OneCandidate(Mutex::new(Some(candidate))));
    let registry = McpRegistryController::new(Arc::new(RegistryJournal::default()), candidate_port)
        .expect("registry");
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.server]\ntransport='stdio'\ncommand='mcp-server'\n",
    )
    .expect("config");
    let provenance = resolve(&[layer], Trust::Trusted, None)
        .expect("resolved")
        .mcp
        .entries()
        .values()
        .find_map(|entry| match entry {
            ResolvedMcpEntry::Configured { provenance, .. } => Some(provenance.clone()),
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .expect("provenance");
    let refresh = McpRegistryRefresh::new(
        vec![McpRegistryCandidate::ready(admission, provenance)],
        RegistryTrigger::Add,
        RegistryScope::All,
    )
    .expect("refresh");
    block_on(registry.reconcile(refresh)).expect("publication");
    registry.capture().expect("capture")
}

#[test]
fn ordinary_question_path_does_not_add_an_mcp_hook_owner() {
    assert_eq!(MCP_OWNERS.len(), 2);
    assert!(MCP_OWNERS.iter().all(|(_, lifecycle)| matches!(
        lifecycle,
        HookLifecycle::Elicitation | HookLifecycle::ElicitationResult
    )));
}
