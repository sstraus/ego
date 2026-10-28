//! runtime contract for one-turn question suspension.
//!
//! This module deliberately exercises the core-owned async port and the
//! TurnLoop attachment point. Host adapters are not allowed to become a
//! second parser or to manufacture a question id: the id passed to Elicitor
//! is the persisted QuestionRequested envelope ItemId.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ego_core::budget::{BudgetGate, Metering, SharedBudgetGate};
use ego_core::config::{Mode, ModelRef, Ruleset};
use ego_core::cost::{Amount, CurrencyCode, MeteringRevision, Money};
use ego_core::event::domain::{DomainEvent, EventKind, ExitReason};
use ego_core::event::hook::{HookDecision, HookOutcome};
use ego_core::event::id::ItemId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{
    EvidenceAdmissionService, EvidenceError as AdmissionError, EvidenceKey, EvidenceRecord,
    EvidenceStore as AdmissionStore, EvidenceWrite, RunInvocationId,
};
use ego_core::generation_model_source::{
    GenerationModelError, GenerationModelRequest, GenerationModelSource, GenerationProfile,
};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::StopReason;
use ego_core::hook::{
    HookCatalogue, HookFuture, HookInvocationBinding, HookInvocationPort, HookInvocationRequest,
    HookLifecycleDispatcher, HookRunner, SessionHookState,
};
use ego_core::mcp::McpRuntimeBinding;
use ego_core::mcp::{ForeignInputRequired, ForeignQuestionPort, ToolExecution};
use ego_core::permit::fake::{FakeFs, FakeWriteTool};
use ego_core::permit::{
    CanonPath, DuplexHandle, DuplexSpawner, GuardedIo, HttpRequest, OneHop, OneHopResponse, Op,
    PolicyEngine, PrepareCtx, PreparedCall, ProcessError, ProcessExecutor, ResolvedConnect,
    Resolver, SpawnSpec, Tool, ToolDescriptor, ToolError, ToolResult, TransportError, Typed,
};
use ego_core::question::{
    AskQuestionTool, Elicitor, ElicitorError, QuestionAnswer, QuestionMode, QuestionOption,
    QuestionOutcome, QuestionRequest, QuestionTarget,
};
use ego_core::run::{RunAppend, RunRevision, RunTransaction};
use ego_core::sink::CollectingSink;
use ego_core::state::{ItemKind, SessionState};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};

mod common;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::{
    CancelToken, GenerationPolicyCapture, GenerationPolicySnapshot, GenerationRuntime,
    RegisteredTool, Session, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
};
use futures::future::BoxFuture;
use futures::stream::BoxStream;

const CONTINUATION_ID: &str =
    "mcp-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SUCCESSOR_CONTINUATION_ID: &str =
    "mcp-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

#[derive(Default)]
struct RecordingElicitor {
    calls: Mutex<Vec<(ItemId, QuestionRequest)>>,
    outcomes: Mutex<VecDeque<Result<QuestionOutcome, ElicitorError>>>,
    trace: Option<Arc<Mutex<Vec<&'static str>>>>,
}

impl RecordingElicitor {
    fn answered(answer: QuestionAnswer) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            outcomes: Mutex::new(VecDeque::from([Ok(QuestionOutcome::Answered(answer))])),
            trace: None,
        }
    }

    fn answered_with_trace(answer: QuestionAnswer, trace: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self::scripted(vec![Ok(QuestionOutcome::Answered(answer))], Some(trace))
    }

    fn scripted(
        outcomes: Vec<Result<QuestionOutcome, ElicitorError>>,
        trace: Option<Arc<Mutex<Vec<&'static str>>>>,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes.into()),
            trace,
        }
    }
}

impl Elicitor for RecordingElicitor {
    fn elicit<'a>(
        &'a self,
        question: ItemId,
        request: QuestionRequest,
    ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>> {
        if let Some(trace) = &self.trace {
            trace.lock().expect("question trace").push("elicitor");
        }
        self.calls
            .lock()
            .expect("elicitor calls")
            .push((question, request));
        let outcome = self
            .outcomes
            .lock()
            .expect("elicitor outcome")
            .pop_front()
            .expect("one scripted elicitation");
        Box::pin(async move { outcome })
    }
}

#[test]
fn elicitor_receives_the_persisted_question_item_and_returns_a_typed_outcome() {
    let question = ItemId::new();
    let request = QuestionRequest::new(
        "Choose",
        QuestionMode::Single,
        vec![QuestionOption::new("yes", "Yes")],
        false,
    );
    let elicitor = RecordingElicitor::answered(QuestionAnswer::single("yes"));

    let outcome = futures::executor::block_on(elicitor.elicit(question, request.clone()))
        .expect("scripted answer");
    assert!(matches!(outcome, QuestionOutcome::Answered(_)));
    assert_eq!(
        elicitor.calls.lock().unwrap().as_slice(),
        &[(question, request)]
    );
}

// Keep the attachment itself explicit as a narrow compile contract; the
// integration test below exercises the same port through a real model turn.
#[allow(dead_code)]
fn turn_loop_accepts_the_core_elicitor<'session, 'turn>(
    turn: TurnLoop<'session, 'turn>,
    elicitor: Arc<dyn Elicitor>,
) -> TurnLoop<'session, 'turn>
where
    'session: 'turn,
{
    turn.with_elicitor(elicitor)
}

#[derive(Default)]
struct MemoryStore {
    fail_append: Mutex<Option<(&'static str, bool)>>,
    events: Mutex<Vec<DomainEvent>>,
    trace: Option<Arc<Mutex<Vec<&'static str>>>>,
}

impl MemoryStore {
    fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().expect("memory store events").clone()
    }

    fn with_trace(trace: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            fail_append: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            trace: Some(trace),
        }
    }
}

impl SessionStore for MemoryStore {
    fn append(
        &self,
        _id: &ego_core::event::id::SessionId,
        event: &DomainEvent,
    ) -> Result<(), StoreError> {
        let failure = {
            let mut fail = self.fail_append.lock().unwrap();
            if fail
                .as_ref()
                .is_some_and(|(kind, _)| *kind == event.kind.name())
            {
                fail.take()
            } else {
                None
            }
        };
        if failure == Some((event.kind.name(), false)) {
            return Err(StoreError::Io(std::io::Error::other(
                "crash before event publication",
            )));
        }
        if let Some(trace) = &self.trace {
            let label = if matches!(&event.kind, EventKind::QuestionRequested { .. }) {
                "question-requested"
            } else if matches!(&event.kind, EventKind::QuestionSettled { .. }) {
                "question-settled"
            } else if matches!(&event.kind, EventKind::ToolCallCompleted { .. }) {
                "tool-completed"
            } else {
                "other"
            };
            trace.lock().expect("question trace").push(label);
        }
        self.events
            .lock()
            .expect("memory store events")
            .push(event.clone());
        if failure.is_some() {
            return Err(StoreError::Io(std::io::Error::other(
                "crash after event publication",
            )));
        }

        Ok(())
    }

    fn compare_and_append_run(
        &self,
        _session: &ego_core::event::id::SessionId,
        _expected_revision: Option<RunRevision>,
        _transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this event-only fixture does not accept run transactions",
        )))
    }

    fn read(
        &self,
        _id: &ego_core::event::id::SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        unimplemented!("this integration test only appends")
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        unimplemented!("this integration test only appends")
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}

struct NoProcess;

impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(futures::future::ready(Err(ProcessError::Invariant(
            "process execution is not used by question suspension".into(),
        ))))
    }
}

struct NoEvidence;

impl AdmissionStore for NoEvidence {
    fn write(
        &self,
        _key: EvidenceKey,
        _record: EvidenceRecord,
    ) -> Result<EvidenceWrite, AdmissionError> {
        Ok(EvidenceWrite::Published)
    }
}

impl EvidenceStore for NoEvidence {
    fn blob_store(&self) -> &dyn ego_core::store::blob::BlobStore {
        &common::EMPTY_BLOB_STORE
    }

    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Err(EvidenceError::Limit { limit: 0 })
    }

    fn read_page<'a>(
        &'a self,
        _request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(futures::future::ready(Err(EvidenceError::Limit {
            limit: 0,
        })))
    }
}

struct NoResolver;

impl Resolver for NoResolver {
    fn resolve(
        &self,
        _target: ego_core::permit::NetworkTarget,
        _deadline: Instant,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, TransportError>> {
        panic!("question suspension unexpectedly resolved an HTTP target")
    }
}

struct NoHop;

impl OneHop for NoHop {
    fn begin_call(&self) -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }
    fn execute(
        &self,
        _connect: ResolvedConnect,
        _request: HttpRequest,
        _deadline: Instant,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        panic!("question suspension unexpectedly executed an HTTP hop")
    }
}

struct NoDuplex;

impl DuplexSpawner for NoDuplex {
    fn spawn(&self, _spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        panic!("question suspension unexpectedly spawned a duplex")
    }
}

struct FixedSource {
    generator: Arc<FakeGenerator>,
}

impl GenerationModelSource for FixedSource {
    fn resolve<'a>(
        &'a self,
        _request: GenerationModelRequest,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
        Box::pin(futures::future::ready(Ok(GenerationProfile {
            output_limit: None,
            identity: ModelRef("test-model".into()),
            request_model: "test-model".into(),
            thinking: ego_core::generator::types::ThinkingLevel::Off,
            generator: self.generator.clone(),
            capabilities: ModelCapabilities {
                schema_version: ModelCapabilities::SCHEMA_VERSION,
                audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
                max_context_tokens: Some(10_000),
                max_output_tokens: Some(1_000),
                max_tools: Some(16),
                supports_parallel_tool_calls: false,
                reasoning_carryover: ReasoningCarryover::Unsupported,
                reasoning_efforts: ReasoningEffortSupport::Any,
                reasoning_state_dialect: None,
                cancel: CancelSemantics::Cooperative,
                cache: CacheRetention::Short,
                transport: Transport::Sse,
                image_input: ego_core::generator::capabilities::ImageInputSupport::Unknown,
                default_reasoning_effort: None,
            },
            metering: Metering::Unmetered {
                revision: MeteringRevision("question-runtime".into()),
            },
            pricing_unavailable: None,
            rates: None,
        })))
    }
}

#[derive(Default)]
struct UnavailableElicitor;

impl Elicitor for UnavailableElicitor {
    fn elicit<'a>(
        &'a self,
        _question: ItemId,
        _request: QuestionRequest,
    ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>> {
        Box::pin(futures::future::ready(Ok(QuestionOutcome::Unavailable)))
    }
}

fn foreign_question_request() -> QuestionRequest {
    QuestionRequest::new(
        "Confirm the deployment target",
        QuestionMode::Form {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "enum": ["staging", "production"]}
                },
                "required": ["target"]
            }),
        },
        Vec::new(),
        false,
    )
}

struct ForeignInputTool {
    request: QuestionRequest,
}

#[derive(Default)]
struct RecordingForeignQuestionPort {
    cancelled: Mutex<Vec<String>>,
    calls: Mutex<Vec<(String, String)>>,
    outcomes: Mutex<VecDeque<Result<ToolExecution, String>>>,
    trace: Option<Arc<Mutex<Vec<&'static str>>>>,
}

impl RecordingForeignQuestionPort {
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().expect("foreign port calls").clone()
    }

    fn scripted(
        outcomes: Vec<Result<ToolExecution, String>>,
        trace: Option<Arc<Mutex<Vec<&'static str>>>>,
    ) -> Self {
        Self {
            cancelled: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes.into()),
            trace,
        }
    }
}

impl ForeignQuestionPort for RecordingForeignQuestionPort {
    fn cancel(&self, id: &str) -> Result<(), String> {
        self.cancelled.lock().unwrap().push(id.to_owned());
        Ok(())
    }
    fn resume<'a>(
        &'a self,
        continuation_id: &'a str,
        canonical_json: &'a str,
        _causality: ego_core::hook::HookCausality,
    ) -> BoxFuture<'a, Result<ToolExecution, String>> {
        if let Some(trace) = &self.trace {
            trace.lock().expect("question trace").push("foreign-port");
        }
        self.calls
            .lock()
            .expect("foreign port calls")
            .push((continuation_id.to_owned(), canonical_json.to_owned()));
        let outcome = self
            .outcomes
            .lock()
            .expect("foreign port outcomes")
            .pop_front()
            .unwrap_or_else(|| {
                Ok(ToolExecution::Complete(ToolResult::builtin(
                    "foreign-complete",
                )))
            });
        Box::pin(std::future::ready(outcome))
    }
}

#[derive(Clone, Default)]
struct QuestionHookPort;

impl HookInvocationPort for QuestionHookPort {
    fn invoke<'a>(&'a self, _request: HookInvocationRequest) -> HookFuture<'a> {
        Box::pin(futures::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }
}

struct QuestionHookBinding {
    trace: Option<Arc<Mutex<Vec<&'static str>>>>,
    bind_calls: AtomicUsize,
    fail_at: Option<usize>,
}

impl HookInvocationBinding for QuestionHookBinding {
    fn bind<'a>(
        &self,
        _policy: Arc<PolicyEngine>,
        _mcp: Option<&dyn McpRuntimeBinding>,
    ) -> Result<Arc<dyn HookInvocationPort + 'a>, String>
    where
        Self: 'a,
    {
        if let Some(trace) = &self.trace {
            trace.lock().expect("question trace").push("bind");
        }
        let call = self.bind_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_at == Some(call) {
            return Err("question hook binding failed".into());
        }
        Ok(Arc::new(QuestionHookPort))
    }
}

struct QuestionMcpRuntime {
    foreign: Arc<RecordingForeignQuestionPort>,
    trace: Option<Arc<Mutex<Vec<&'static str>>>>,
}

struct TracingPolicySource {
    policy: Arc<PolicyEngine>,
    snapshot: Arc<GenerationPolicySnapshot>,
    trace: Arc<Mutex<Vec<&'static str>>>,
}

impl TracingPolicySource {
    fn new(trace: Arc<Mutex<Vec<&'static str>>>) -> Self {
        let capture = GenerationPolicyCapture::new(
            CanonPath::new("/ws"),
            Mode::Default,
            Ruleset::default(),
            None,
        )
        .expect("valid generation policy capture");
        Self {
            policy: capture.policy(),
            snapshot: capture.snapshot(),
            trace,
        }
    }
}

impl ego_core::turn::GenerationPolicySource for TracingPolicySource {
    fn current(&self) -> Result<Arc<PolicyEngine>, String> {
        self.trace.lock().expect("question trace").push("current");
        Ok(Arc::clone(&self.policy))
    }

    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        self.trace.lock().expect("question trace").push("capture");
        GenerationPolicyCapture::restore(Arc::clone(&self.snapshot), None)
            .map_err(|error| error.to_string())
    }

    fn restore(
        &self,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(snapshot, None).map_err(|error| error.to_string())
    }
}

impl McpRuntimeBinding for QuestionMcpRuntime {
    fn load_pending_foreign_policy(
        &self,
        _continuation_id: &str,
    ) -> Result<Arc<GenerationPolicySnapshot>, String> {
        Err("question fixture has no pending foreign policy".to_owned())
    }

    fn bind_pending_foreign_continuation(
        &self,
        _continuation_id: &str,
        _snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), String> {
        Err("question fixture cannot bind a pending continuation".to_owned())
    }

    fn hook_invocation_port<'a>(
        &self,
        _policy: Arc<PolicyEngine>,
        _effects: ToolEffectPorts<'a>,
    ) -> Result<Arc<dyn HookInvocationPort + 'a>, String> {
        Ok(Arc::new(QuestionHookPort))
    }

    fn foreign_question_port<'session, 'turn>(
        &self,
        _runtime: ego_core::hook::SessionHookRuntime<'session>,
        _floor: ego_core::suspension::ResumeFloor,
        _effects: ToolEffectPorts<'turn>,
    ) -> Arc<dyn ForeignQuestionPort + 'turn>
    where
        'session: 'turn,
    {
        if let Some(trace) = &self.trace {
            trace
                .lock()
                .expect("question trace")
                .push("foreign-factory");
        }
        Arc::clone(&self.foreign) as Arc<dyn ForeignQuestionPort + 'turn>
    }
}

impl Tool for ForeignInputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "mcp_input".to_owned(),
            description: "fixture MCP operation that suspends for foreign input".to_owned(),
            schema: serde_json::from_value(serde_json::json!({"type": "object"}))
                .expect("fixture schema"),
        }
    }

    fn prepare(
        &self,
        _params: serde_json::Value,
        _ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall::new(
            self.descriptor().name,
            Op::Read {
                path: CanonPath::new("/ws/fixture"),
            },
        ))
    }

    fn execute<'a, 'io>(
        &'a self,
        _call: PreparedCall,
        _io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async {
            Ok(ToolExecution::InputRequired(
                ForeignInputRequired::new(CONTINUATION_ID, self.request.clone())
                    .expect("valid continuation id"),
            ))
        })
    }
}

fn with_foreign_turn<R>(
    generator: Arc<FakeGenerator>,
    elicitor: Arc<dyn Elicitor>,
    foreign_port: Arc<RecordingForeignQuestionPort>,
    trace: Option<Arc<Mutex<Vec<&'static str>>>>,
    fail_at: Option<usize>,
    trace_store: bool,
    test: impl for<'a> FnOnce(TurnLoop<'a, 'a>, &'a MemoryStore) -> R,
) -> R {
    with_foreign_turn_request(
        foreign_question_request(),
        generator,
        elicitor,
        foreign_port,
        (trace, trace_store),
        fail_at,
        test,
    )
}

fn with_foreign_turn_request<R>(
    request: QuestionRequest,
    generator: Arc<FakeGenerator>,
    elicitor: Arc<dyn Elicitor>,
    foreign_port: Arc<RecordingForeignQuestionPort>,
    store_trace: (Option<Arc<Mutex<Vec<&'static str>>>>, bool),
    fail_at: Option<usize>,
    test: impl for<'a> FnOnce(TurnLoop<'a, 'a>, &'a MemoryStore) -> R,
) -> R {
    let (trace, trace_store) = store_trace;
    let store = if trace_store {
        trace
            .clone()
            .map_or_else(MemoryStore::default, MemoryStore::with_trace)
    } else {
        MemoryStore::default()
    };
    let clock = FixedClock;
    let sink = CollectingSink::new();
    let gate = SharedBudgetGate::new(BudgetGate::new(Money::new(
        "1000".parse::<Amount>().expect("amount"),
        CurrencyCode::new("USD").expect("currency"),
    )));
    let _policy = ego_core::permit::PolicyEngine::new(
        CanonPath::new("/ws"),
        Mode::Default,
        Ruleset::default(),
    );
    let fs = FakeFs::default();
    let process = NoProcess;
    let evidence = NoEvidence;
    let resolver = Arc::new(NoResolver);
    let one_hop = Arc::new(NoHop);
    let duplex = Arc::new(NoDuplex);
    let registry = vec![RegisteredTool::Effect(
        Arc::new(ForeignInputTool { request }) as Arc<dyn Tool>,
    )];
    let tools = Tools::from_registry(
        &registry,
        ToolEffectPorts::new(&fs, &process, &evidence, resolver, one_hop, duplex),
        CanonPath::new("/ws"),
    );
    let source = Arc::new(FixedSource { generator });
    let policy_source: Arc<dyn ego_core::turn::GenerationPolicySource> = match trace.clone() {
        Some(trace) => Arc::new(TracingPolicySource::new(trace)),
        None => common::default_policy_source(),
    };
    let mut turn = TurnLoop::new(
        Session {
            id: ego_core::event::id::SessionId::new(),
            state: SessionState::default(),
            store: &store,
            evidence: &evidence,
            clock: &clock,
            sink: &sink,
            cancel: CancelToken::new(),
        },
        GenerationRuntime {
            policy_source,
            source,
            gate,
            config: TurnConfig {
                cache: ego_core::generator::capabilities::CacheRetention::Short,
                system: None,
                identity: None,
                token_ceiling: None,
                retry: 0,
                fallbacks: Vec::new(),
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
            retry_pause: Arc::new(NoRetryPause),
        },
        tools,
    )
    .expect("budget gate is available")
    .with_elicitor(elicitor);
    let state = SessionHookState::new(
        HookLifecycleDispatcher::new(
            HookRunner::new(),
            HookCatalogue::admit_typed(&[], &[], ego_core::config::Trust::Trusted)
                .expect("empty hook catalogue"),
        ),
        turn.journal(),
        CancelToken::new(),
        Instant::now() + std::time::Duration::from_secs(60),
    );
    turn = turn
        .with_hook_binding(
            state,
            Arc::new(QuestionHookBinding {
                trace: trace.clone(),
                bind_calls: AtomicUsize::new(0),
                fail_at,
            }),
            Some(Arc::new(QuestionMcpRuntime {
                foreign: foreign_port,
                trace: trace.clone(),
            })),
        )
        .expect("hook and foreign binding shares the turn journal");
    test(turn, &store)
}

fn foreign_input_generator() -> Arc<FakeGenerator> {
    Arc::new(FakeGenerator::new(vec![
        vec![
            AiEvent::ToolCall {
                call_id: "mcp-input-call".into(),
                name: "mcp_input".into(),
                args: "{}".into(),
            },
            AiEvent::Stop(StopReason::ToolUse),
        ],
        vec![AiEvent::Stop(StopReason::Stop)],
    ]))
}

#[test]
fn unavailable_elicitor_persists_question_closes_unstarted_siblings_and_leaves_turn_pending() {
    let request = QuestionRequest::new(
        "Choose a release channel",
        QuestionMode::Single,
        vec![QuestionOption::new("stable", "Stable")],
        false,
    );
    let generator = Arc::new(FakeGenerator::new(vec![vec![
        AiEvent::ToolCall {
            call_id: "question-call".into(),
            name: "ask_question".into(),
            args: serde_json::to_string(&request).expect("question request JSON"),
        },
        AiEvent::ToolCall {
            call_id: "sibling-call".into(),
            name: "fake_write".into(),
            args: r#"{"path":"/ws/out.txt","contents":"must not run"}"#.into(),
        },
        AiEvent::Stop(StopReason::ToolUse),
    ]]));
    let store = MemoryStore::default();
    let clock = FixedClock;
    let sink = CollectingSink::new();
    let gate = SharedBudgetGate::new(BudgetGate::new(Money::new(
        "1000".parse::<Amount>().expect("amount"),
        CurrencyCode::new("USD").expect("currency"),
    )));
    let _policy = ego_core::permit::PolicyEngine::new(
        CanonPath::new("/ws"),
        Mode::Default,
        Ruleset::default(),
    );
    let fs = FakeFs::default();
    let process = NoProcess;
    let evidence = NoEvidence;
    let resolver = Arc::new(NoResolver);
    let one_hop = Arc::new(NoHop);
    let duplex = Arc::new(NoDuplex);
    let write: Arc<dyn ego_core::permit::Tool> = Arc::new(Typed(FakeWriteTool));
    let registry = vec![
        RegisteredTool::HumanInput(Arc::new(AskQuestionTool)),
        RegisteredTool::Effect(write),
    ];
    let tools = Tools::from_registry(
        &registry,
        ToolEffectPorts::new(
            &fs,
            &process,
            &evidence,
            resolver.clone(),
            one_hop.clone(),
            duplex.clone(),
        ),
        CanonPath::new("/ws"),
    );
    let source = Arc::new(FixedSource { generator });
    let mut turn = TurnLoop::new(
        Session {
            id: ego_core::event::id::SessionId::new(),
            state: SessionState::default(),
            store: &store,
            evidence: &evidence,
            clock: &clock,
            sink: &sink,
            cancel: CancelToken::new(),
        },
        GenerationRuntime {
            policy_source: common::default_policy_source(),
            source,
            gate,
            config: TurnConfig {
                cache: ego_core::generator::capabilities::CacheRetention::Short,
                system: None,
                identity: None,
                token_ceiling: None,
                retry: 0,
                fallbacks: Vec::new(),
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
            retry_pause: Arc::new(NoRetryPause),
        },
        tools,
    )
    .expect("budget gate is available")
    .with_elicitor(Arc::new(UnavailableElicitor));

    let outcome = futures::executor::block_on(turn.run("pick a channel"))
        .expect("question suspension is a handled turn outcome");
    assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
    assert!(!outcome.ok);

    let state = turn.state();
    let pending = state
        .pending_question()
        .expect("unavailable elicitation leaves the durable question pending");
    assert_eq!(
        state
            .items
            .get(&pending.item_id())
            .expect("question item")
            .kind,
        ItemKind::Question
    );
    let sibling = state
        .items
        .values()
        .find(|item| item.call_id.as_deref() == Some("sibling-call"))
        .expect("the unstarted sibling remains represented");
    assert_eq!(sibling.ok, Some(false));
    assert!(
        sibling
            .tool_result
            .as_ref()
            .expect("sibling closure result")
            .model_text()
            .contains("interrupted: not run")
    );

    let events = store.events();
    let question = events
        .iter()
        .position(|event| matches!(event.kind, EventKind::QuestionRequested { .. }))
        .expect("QuestionRequested is durable before elicitor access");
    let sibling_close = events
        .iter()
        .position(|event| matches!(event.kind, EventKind::ToolCallCompleted { ok: false, .. }))
        .expect("not-started sibling closure is durable");
    assert!(question < sibling_close);
    let sibling_started = events
        .iter()
        .find(|event| {
            matches!(
                event.kind,
                EventKind::ToolCallStarted {
                    ref call_id,
                    ..
                } if call_id.as_deref() == Some("sibling-call")
            )
        })
        .expect("sibling start is durable");
    assert_eq!(
        events[sibling_close].env.parent,
        Some(sibling_started.env.id),
        "sibling closure remains causally attached to its not-started call"
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.kind, EventKind::TurnCompleted { .. })),
        "a suspended turn must not publish TurnCompleted"
    );
}

#[test]
fn resumed_foreign_question_does_not_silently_skip_active_goal_check() {
    // Catches: answering an MCP question completes work without checking the active goal.
    // This fixture deliberately has no verdict evidence store: checking must expose
    // that evaluator failure instead of silently reporting ordinary completion.
    let generator = foreign_input_generator();
    let foreign_port = Arc::new(RecordingForeignQuestionPort::scripted(Vec::new(), None));
    with_foreign_turn(
        generator,
        Arc::new(UnavailableElicitor),
        foreign_port,
        None,
        None,
        false,
        |turn, _store| {
            let mut turn = turn.with_goal(Some("verify the MCP operation".into()), 1);
            let outcome = futures::executor::block_on(turn.run("invoke the MCP operation"))
                .expect("question suspends normally");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            let state = turn.state();
            let pending = state.pending_question().expect("pending MCP question");
            let question = pending.item_id();
            let answer = QuestionAnswer::from_json(pending.request(), r#"{"target":"staging"}"#)
                .expect("valid answer");
            let resumed = futures::executor::block_on(turn.resume_question(question, answer))
                .expect("answered question completes normally");
            assert!(resumed.ok);
            let goal = turn.state().goal.expect("active goal remains durable");
            assert!(
                goal.check_stop.is_some(),
                "resumed work must check its goal and expose unavailable evaluator evidence"
            );
        },
    );
}

#[test]
fn form_question_accepts_object_answers_and_rejects_schema_or_type_before_mutation() {
    let request = foreign_question_request();
    let original = request.clone();
    let accepted = QuestionAnswer::from_json(&request, r#"{"target":"staging"}"#)
        .expect("schema-valid form answer");
    assert!(accepted.validate_for(&request).is_ok());
    assert_eq!(request, original);

    assert!(QuestionAnswer::from_json(&request, r#"{"target":42}"#).is_err());
    assert_eq!(request, original);
    assert!(QuestionAnswer::from_json(&request, r#"{"other":"staging"}"#).is_err());
    assert_eq!(request, original);
}

#[test]
fn foreign_input_required_carries_question_request_and_non_secret_continuation_id() {
    let request = foreign_question_request();
    let foreign =
        ForeignInputRequired::new(CONTINUATION_ID, request.clone()).expect("valid continuation id");
    assert_eq!(foreign.continuation_id(), CONTINUATION_ID);
    assert_eq!(foreign.question_request(), &request);
}

#[test]
fn foreign_input_required_is_projected_to_a_durable_pending_question() {
    let generator = foreign_input_generator();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let foreign_port = Arc::new(RecordingForeignQuestionPort::scripted(
        Vec::new(),
        Some(trace.clone()),
    ));
    with_foreign_turn(
        generator,
        Arc::new(UnavailableElicitor),
        foreign_port.clone(),
        Some(trace.clone()),
        None,
        true,
        |mut turn, store| {
            let outcome = futures::executor::block_on(turn.run("invoke the MCP operation"))
                .expect("foreign input suspension is a handled turn outcome");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            assert!(!outcome.ok);
            let state = turn.state();
            let pending = state
                .pending_question()
                .expect("ForeignInputRequired must become a durable pending question");
            assert!(pending.request().validate().is_ok());
            let QuestionTarget::ForeignContinuation {
                tool_call,
                continuation_id,
            } = pending.target()
            else {
                panic!("foreign input must retain a closed ForeignContinuation target");
            };
            assert_eq!(*tool_call, pending.tool_call());
            assert_eq!(continuation_id, CONTINUATION_ID);
            let question = pending.item_id();
            assert!(
                store
                    .events()
                    .iter()
                    .all(|event| !matches!(event.kind, EventKind::TurnCompleted { .. })),
                "a foreign-input suspension must not publish TurnCompleted before resume"
            );
            let answer = QuestionAnswer::from_json(pending.request(), r#"{"target":"staging"}"#)
                .expect("schema-valid form answer");
            trace.lock().expect("question trace").clear();
            let resumed = futures::executor::block_on(turn.resume_question(question, answer))
                .expect("foreign continuation resumes the turn");
            assert!(resumed.ok);
            assert_eq!(
                foreign_port.calls(),
                vec![(
                    CONTINUATION_ID.to_owned(),
                    r#"{"target":"staging"}"#.to_owned()
                )]
            );
            let labels = trace.lock().expect("question trace").clone();
            assert!(labels.starts_with(&["question-settled", "foreign-port"]));
            let foreign_port_index = labels
                .iter()
                .position(|label| *label == "foreign-port")
                .expect("foreign adapter resumes");
            assert!(labels[..foreign_port_index].iter().all(|label| !matches!(
                *label,
                "current" | "bind" | "foreign-factory" | "capture"
            )));
            let completed = store
                .events()
                .into_iter()
                .find_map(|event| match event.kind {
                    EventKind::ToolCallCompleted { result, .. }
                        if result.model_text() == "foreign-complete" =>
                    {
                        Some(result)
                    }
                    _ => None,
                })
                .expect("foreign completion is the actual tool result");
            assert_eq!(completed.model_text(), "foreign-complete");
            let events = store.events();
            let settled = events
                .iter()
                .position(|event| matches!(event.kind, EventKind::QuestionSettled { .. }))
                .expect("QuestionSettled is durable before foreign resume");
            let foreign_completion = events
                .iter()
                .position(|event| {
                    matches!(
                        event.kind,
                        EventKind::ToolCallCompleted { ref result, .. }
                            if result.model_text() == "foreign-complete"
                    )
                })
                .expect("foreign completion is durable");
            assert!(settled < foreign_completion);
            let duplicate = futures::executor::block_on(
                turn.resume_question(
                    question,
                    QuestionAnswer::from_json(
                        &foreign_question_request(),
                        r#"{"target":"staging"}"#,
                    )
                    .expect("schema-valid duplicate answer"),
                ),
            );
            assert!(
                duplicate.is_err(),
                "a settled foreign question cannot retry"
            );
            assert_eq!(foreign_port.calls().len(), 1);
            assert!(
                store
                    .events()
                    .iter()
                    .any(|event| matches!(event.kind, EventKind::QuestionRequested { .. })),
                "QuestionRequested must be durable before exposing the suspension"
            );
        },
    );
}

#[test]
fn reconstructed_resume_without_captured_policy_fails_closed_before_settlement() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let original_foreign = Arc::new(RecordingForeignQuestionPort::scripted(
        Vec::new(),
        Some(trace.clone()),
    ));
    with_foreign_turn(
        foreign_input_generator(),
        Arc::new(UnavailableElicitor),
        original_foreign,
        Some(trace.clone()),
        None,
        true,
        |turn, store| {
            let mut turn = turn;
            let outcome = futures::executor::block_on(turn.run("invoke the MCP operation"))
                .expect("foreign input suspension is handled");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            let turn_state = turn.state();
            let pending = turn_state
                .pending_question()
                .expect("durable pending question");
            let question = pending.item_id();
            let answer = QuestionAnswer::from_json(pending.request(), r#"{"target":"staging"}"#)
                .expect("schema-valid form answer");

            // A restarted coordinator sees the same durable journal, but has no
            // generation capture or bound foreign port to restore.
            let restart_trace = Arc::new(Mutex::new(Vec::new()));
            let restart_foreign = Arc::new(RecordingForeignQuestionPort::scripted(
                Vec::new(),
                Some(restart_trace.clone()),
            ));
            let restart_generator = Arc::new(FakeGenerator::new(vec![vec![AiEvent::Stop(
                StopReason::Stop,
            )]]));
            let gate = SharedBudgetGate::new(BudgetGate::new(Money::new(
                "1000".parse::<Amount>().expect("amount"),
                CurrencyCode::new("USD").expect("currency"),
            )));
            let fs = FakeFs::default();
            let process = NoProcess;
            let evidence: &'static NoEvidence = Box::leak(Box::new(NoEvidence));
            let resolver = Arc::new(NoResolver);
            let one_hop = Arc::new(NoHop);
            let duplex = Arc::new(NoDuplex);
            let registry = vec![RegisteredTool::Effect(Arc::new(ForeignInputTool {
                request: foreign_question_request(),
            }) as Arc<dyn Tool>)];
            let tools = Tools::from_registry(
                &registry,
                ToolEffectPorts::new(&fs, &process, evidence, resolver, one_hop, duplex),
                CanonPath::new("/ws"),
            );
            let policy_source: Arc<dyn ego_core::turn::GenerationPolicySource> =
                Arc::new(TracingPolicySource::new(restart_trace.clone()));
            let runtime = GenerationRuntime {
                policy_source,
                source: Arc::new(FixedSource {
                    generator: restart_generator.clone(),
                }),
                gate,
                config: TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 0,
                    fallbacks: Vec::new(),
                },
                admission: Arc::new(EvidenceAdmissionService::new()),
                invocation: RunInvocationId::new(),
                retry_pause: Arc::new(NoRetryPause),
            };
            let state = SessionHookState::new(
                HookLifecycleDispatcher::new(
                    HookRunner::new(),
                    HookCatalogue::admit_typed(&[], &[], ego_core::config::Trust::Trusted)
                        .expect("empty hook catalogue"),
                ),
                turn.journal(),
                CancelToken::new(),
                Instant::now() + std::time::Duration::from_secs(60),
            );
            let mut restarted = TurnLoop::from_shared_journal(
                ego_core::event::id::SessionId::new(),
                turn.journal(),
                evidence,
                CancelToken::new(),
                runtime,
                tools,
            )
            .expect("restart budget gate is available")
            .with_elicitor(Arc::new(UnavailableElicitor))
            .with_hook_binding(
                state,
                Arc::new(QuestionHookBinding {
                    trace: Some(restart_trace.clone()),
                    bind_calls: AtomicUsize::new(0),
                    fail_at: None,
                }),
                Some(Arc::new(QuestionMcpRuntime {
                    foreign: restart_foreign.clone(),
                    trace: Some(restart_trace.clone()),
                })),
            )
            .expect("restart hook and foreign binding shares the journal");

            restart_trace.lock().expect("restart trace").clear();
            let resumed = futures::executor::block_on(restarted.resume_question(question, answer))
                .expect("restart refusal is a handled outcome");
            assert_eq!(resumed.exit_reason, ExitReason::Internal);
            assert!(!resumed.ok);
            drop(restarted);
            assert!(
                restart_trace
                    .lock()
                    .expect("restart trace")
                    .iter()
                    .all(|label| *label != "question-settled")
            );
            assert!(restart_foreign.calls().is_empty());
            assert!(
                turn.state().pending_question().is_some(),
                "failed restart must leave the durable question pending"
            );
            assert!(store.events().iter().all(|event| !matches!(
                event.kind,
                EventKind::QuestionSettled { .. } | EventKind::ToolCallCompleted { .. }
            )));
        },
    );
}

#[test]
fn available_elicitor_settles_foreign_input_before_one_port_resume_and_completion() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let elicitor = Arc::new(RecordingElicitor::answered_with_trace(
        QuestionAnswer::from_json(&foreign_question_request(), r#"{"target":"staging"}"#)
            .expect("schema-valid form answer"),
        trace.clone(),
    ));
    let foreign_port = Arc::new(RecordingForeignQuestionPort::scripted(
        vec![Ok(ToolExecution::Complete(ToolResult::builtin(
            "foreign-complete",
        )))],
        Some(trace.clone()),
    ));

    with_foreign_turn(
        foreign_input_generator(),
        elicitor.clone(),
        foreign_port.clone(),
        Some(trace.clone()),
        None,
        true,
        |mut turn, store| {
            let outcome = futures::executor::block_on(turn.run("invoke the MCP operation"))
                .expect("available elicitation completes the foreign operation");
            assert_eq!(outcome.exit_reason, ExitReason::Completed);
            assert!(outcome.ok);
            assert!(
                turn.state().pending_question().is_none(),
                "an available elicitor must not leave an avoidable suspension"
            );

            let events = store.events();
            let request = events
                .iter()
                .enumerate()
                .find_map(|(index, event)| {
                    matches!(event.kind, EventKind::QuestionRequested { .. })
                        .then_some((index, event))
                })
                .expect("foreign QuestionRequested is durable");
            let question_item = request.1.env.item.expect("question item identity");
            let settled = events
                .iter()
                .position(|event| matches!(event.kind, EventKind::QuestionSettled { .. }))
                .expect("foreign QuestionSettled is durable");
            let completed = events
                .iter()
                .position(|event| {
                    matches!(
                        event.kind,
                        EventKind::ToolCallCompleted { ref result, .. }
                            if result.model_text() == "foreign-complete"
                    )
                })
                .expect("the actual foreign completion is durable");
            assert!(request.0 < settled && settled < completed);

            let calls = elicitor.calls.lock().expect("elicitor calls");
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, question_item);
            assert_eq!(calls[0].1, foreign_question_request());
            assert_eq!(
                foreign_port.calls(),
                vec![(
                    CONTINUATION_ID.to_owned(),
                    r#"{"target":"staging"}"#.to_owned()
                )]
            );

            let labels = trace.lock().expect("question trace");
            let requested = labels
                .iter()
                .position(|label| *label == "question-requested")
                .expect("request trace");
            let elicited = labels
                .iter()
                .position(|label| *label == "elicitor")
                .expect("elicitor trace");
            let resumed = labels
                .iter()
                .position(|label| *label == "foreign-port")
                .expect("foreign resume trace");
            assert!(requested < elicited && elicited < resumed);
        },
    );
}

#[test]
fn successor_foreign_input_is_exposed_after_settlement_and_keeps_predecessor_closed() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let first_answer =
        QuestionAnswer::from_json(&foreign_question_request(), r#"{"target":"staging"}"#)
            .expect("schema-valid first answer");
    let elicitor = Arc::new(RecordingElicitor::scripted(
        vec![
            Ok(QuestionOutcome::Answered(first_answer)),
            Ok(QuestionOutcome::Unavailable),
        ],
        Some(trace.clone()),
    ));
    let successor =
        ForeignInputRequired::new(SUCCESSOR_CONTINUATION_ID, foreign_question_request())
            .expect("valid successor continuation");
    let foreign_port = Arc::new(RecordingForeignQuestionPort::scripted(
        vec![Ok(ToolExecution::InputRequired(successor))],
        Some(trace.clone()),
    ));

    with_foreign_turn(
        foreign_input_generator(),
        elicitor.clone(),
        foreign_port.clone(),
        Some(trace.clone()),
        None,
        true,
        |mut turn, store| {
            let outcome = futures::executor::block_on(turn.run("invoke the MCP operation"))
                .expect("successor input is a handled suspension");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            assert!(!outcome.ok);

            let events = store.events();
            let questions: Vec<_> = events
                .iter()
                .filter_map(|event| {
                    if let EventKind::QuestionRequested { target, .. } = &event.kind {
                        Some((
                            event.env.item.expect("question item identity"),
                            target.clone(),
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(
                questions.len(),
                2,
                "the successor gets a new durable question"
            );
            assert_ne!(questions[0].0, questions[1].0);
            let first_settled = events
                .iter()
                .position(|event| matches!(event.kind, EventKind::QuestionSettled { .. }))
                .expect("the predecessor is settled");
            let successor_requested = events
                .iter()
                .rposition(|event| matches!(event.kind, EventKind::QuestionRequested { .. }))
                .expect("the successor request is durable");
            assert!(first_settled < successor_requested);

            let state = turn.state();
            let pending = state
                .pending_question()
                .expect("unavailable successor elicitation leaves it pending");
            assert_eq!(pending.item_id(), questions[1].0);
            assert_ne!(pending.item_id(), questions[0].0);
            assert!(matches!(
                pending.target(),
                QuestionTarget::ForeignContinuation {
                    continuation_id,
                    ..
                } if continuation_id == SUCCESSOR_CONTINUATION_ID
            ));
            assert_eq!(foreign_port.calls().len(), 1);

            let labels = trace.lock().expect("question trace");
            let first_request = labels
                .iter()
                .position(|label| *label == "question-requested")
                .expect("first request trace");
            let first_elicitor = labels
                .iter()
                .position(|label| *label == "elicitor")
                .expect("first elicitor trace");
            let resumed = labels
                .iter()
                .position(|label| *label == "foreign-port")
                .expect("foreign resume trace");
            let successor_request = labels
                .iter()
                .rposition(|label| *label == "question-requested")
                .expect("successor request trace");
            let successor_elicitor = labels
                .iter()
                .rposition(|label| *label == "elicitor")
                .expect("successor elicitor trace");
            assert!(first_request < first_elicitor);
            assert!(first_elicitor < resumed);
            assert!(resumed < successor_request && successor_request < successor_elicitor);
        },
    );
}

#[test]
fn foreign_port_failure_after_settlement_does_not_complete_or_retry_the_call() {
    let elicitor = Arc::new(RecordingElicitor::answered(
        QuestionAnswer::from_json(&foreign_question_request(), r#"{"target":"staging"}"#)
            .expect("schema-valid form answer"),
    ));
    let foreign_port = Arc::new(RecordingForeignQuestionPort::scripted(
        vec![Err(
            "foreign continuation failed after acceptance".to_owned()
        )],
        None,
    ));

    with_foreign_turn(
        foreign_input_generator(),
        elicitor,
        foreign_port.clone(),
        None,
        None,
        true,
        |mut turn, store| {
            let abandoned = futures::executor::block_on(turn.run("invoke the MCP operation"));
            assert!(
                abandoned.is_err(),
                "a failed foreign continuation must abandon without a tool completion"
            );
            let events = store.events();
            let question = events
                .iter()
                .find_map(|event| {
                    if matches!(event.kind, EventKind::QuestionRequested { .. }) {
                        Some(event.env.item.expect("question item identity"))
                    } else {
                        None
                    }
                })
                .expect("foreign question request");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event.kind, EventKind::QuestionSettled { .. })),
                "the answer is settled before the port failure"
            );
            assert!(
                events
                    .iter()
                    .all(|event| { !matches!(event.kind, EventKind::ToolCallCompleted { .. }) }),
                "the failed foreign continuation has no ToolCallCompleted"
            );
            let event_count = events.len();
            let duplicate = futures::executor::block_on(
                turn.resume_question(
                    question,
                    QuestionAnswer::from_json(
                        &foreign_question_request(),
                        r#"{"target":"staging"}"#,
                    )
                    .expect("schema-valid duplicate answer"),
                ),
            );
            assert!(
                duplicate.is_err(),
                "a failed accepted continuation cannot retry"
            );
            assert_eq!(foreign_port.calls().len(), 1);
            assert_eq!(store.events().len(), event_count);
        },
    );
}

/// Retries in these tests do not wait on a wall clock.
struct NoRetryPause;

impl ego_core::turn::RetryPause for NoRetryPause {
    fn pause(&self, _delay: std::time::Duration) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(futures::future::ready(()))
    }
}

#[test]
fn declined_or_cancelled_foreign_question_closes_remote_authority_without_retry() {
    // Catches: local question settlement leaves remote continuation resumable.
    for outcome in [QuestionOutcome::Declined, QuestionOutcome::Cancelled] {
        let foreign = Arc::new(RecordingForeignQuestionPort::default());
        with_foreign_turn(
            foreign_input_generator(),
            Arc::new(RecordingElicitor::scripted(vec![Ok(outcome.clone())], None)),
            foreign.clone(),
            None,
            None,
            false,
            |mut turn, store| {
                futures::executor::block_on(turn.run("invoke the MCP operation")).unwrap();
                assert_eq!(*foreign.cancelled.lock().unwrap(), vec![CONTINUATION_ID]);
                assert!(foreign.calls().is_empty());
                assert!(store.events().iter().any(|event| matches!(&event.kind, EventKind::QuestionSettled { outcome: recorded } if recorded == &outcome)));
            },
        );
    }
}

#[test]
fn member_persistence_survives_restart_before_final_retry_consent() {
    // Catches: a completed URL is lost on restart or partial answers trigger a retry.
    use ego_core::question::{ElicitationKind, ElicitationMember, ElicitationRound};
    let request = QuestionRequest::new(
        "MCP server alpha requires input",
        QuestionMode::Round(ElicitationRound {
            expires_at_ms: None,
            requests: std::collections::BTreeMap::from([(
                "verify".to_owned(),
                ElicitationMember {
                    prompt: "Complete on the page".to_owned(),
                    kind: ElicitationKind::Url {
                        url: "https://example.test/verify?private=secret".to_owned(),
                        elicitation_id: "verify-42".to_owned(),
                    },
                },
            )]),
            completed: Default::default(),
        }),
        vec![],
        false,
    );
    let elicitor = Arc::new(RecordingElicitor::scripted(
        vec![
            Ok(QuestionOutcome::MemberCompleted {
                key: "verify".to_owned(),
                answer: serde_json::json!({"completed":true}),
            }),
            Ok(QuestionOutcome::Unavailable),
        ],
        None,
    ));
    let foreign = Arc::new(RecordingForeignQuestionPort::default());
    with_foreign_turn_request(
        request,
        foreign_input_generator(),
        elicitor.clone(),
        foreign.clone(),
        (None, false),
        None,
        |mut turn, store| {
            futures::executor::block_on(turn.run("invoke MCP")).unwrap();
            assert!(
                foreign.calls().is_empty(),
                "a member outcome does not consent to retry"
            );
            let calls = elicitor.calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            let QuestionMode::Round(round) = &calls[1].1.mode else {
                panic!("round")
            };
            assert_eq!(
                round.completed["verify"],
                serde_json::json!({"completed":true})
            );
            assert!(
                round.next_member().is_none(),
                "already completed URL is never opened again"
            );
            let mut replay = SessionState::default();
            for event in store.events() {
                replay.apply(&event);
            }
            let pending = replay.pending_question().expect("round survives restart");
            let QuestionMode::Round(restored) = &pending.request().mode else {
                panic!("round")
            };
            assert_eq!(restored, round);
            assert!(restored.next_member().is_none());
            let (conversation, _) =
                ego_core::generator::history::conversation_from(&replay).unwrap();
            let model_context = serde_json::to_string(conversation.messages()).unwrap();
            assert!(!model_context.contains("private=secret"));
            assert!(!model_context.contains("verify-42"));
            assert!(!model_context.contains("Complete on the page"));
        },
    );
}

#[test]
fn crash_after_question_settlement_requires_explicit_same_answer_before_retry() {
    // Catches: a settlement before remote acceptance strands the operation or replays it automatically.
    use ego_core::question::{ElicitationAnswers, ElicitationRound};
    let request = QuestionRequest::new(
        "Retry MCP?",
        QuestionMode::Round(ElicitationRound {
            expires_at_ms: None,
            requests: Default::default(),
            completed: Default::default(),
        }),
        vec![],
        false,
    );
    let answer = QuestionAnswer::Round(ElicitationAnswers {
        confirmed: true,
        responses: Default::default(),
    });
    let foreign = Arc::new(RecordingForeignQuestionPort::scripted(
        vec![Err("crash before acceptance".to_owned())],
        None,
    ));
    with_foreign_turn_request(
        request,
        foreign_input_generator(),
        Arc::new(RecordingElicitor::answered(answer.clone())),
        foreign.clone(),
        (None, false),
        None,
        |mut turn, store| {
            assert!(futures::executor::block_on(turn.run("invoke MCP")).is_err());
            let state = turn.state();
            let pending = state
                .pending_question()
                .expect("settled but uncompleted foreign question remains recoverable");
            let question = pending.item_id();
            let mut replay = SessionState::default();
            for event in store.events() {
                replay.apply(&event);
            }
            assert_eq!(replay.pending_question().unwrap().item_id(), question);
            assert_eq!(foreign.calls().len(), 1, "replay must not drive effects");
            let outcome =
                futures::executor::block_on(turn.resume_question(question, answer)).unwrap();
            assert_eq!(outcome.exit_reason, ExitReason::Completed);
            assert_eq!(
                foreign.calls().len(),
                2,
                "only explicit user resume retries acceptance"
            );
            assert_eq!(
                store
                    .events()
                    .iter()
                    .filter(|event| matches!(event.kind, EventKind::QuestionSettled { .. }))
                    .count(),
                1
            );
        },
    );
}

#[test]
fn crashes_at_each_question_persistence_boundary_do_not_retry_or_repeat_completed_members() {
    // Catches: uncertain question/member/final writes trigger effects before durable recovery.
    use ego_core::question::{
        ElicitationAnswers, ElicitationKind, ElicitationMember, ElicitationRound,
    };
    for kind in [
        "question_requested",
        "question_member_settled",
        "question_settled",
    ] {
        for published in [false, true] {
            let request = QuestionRequest::new(
                "MCP server alpha",
                QuestionMode::Round(ElicitationRound {
                    expires_at_ms: None,
                    requests: std::collections::BTreeMap::from([(
                        "url-key".to_owned(),
                        ElicitationMember {
                            prompt: "Verify on page".to_owned(),
                            kind: ElicitationKind::Url {
                                url: "https://example.test/verify".to_owned(),
                                elicitation_id: "url-id".to_owned(),
                            },
                        },
                    )]),
                    completed: Default::default(),
                }),
                vec![],
                false,
            );
            let responses = std::collections::BTreeMap::from([(
                "url-key".to_owned(),
                serde_json::json!({"completed":true}),
            )]);
            let elicitor = Arc::new(RecordingElicitor::scripted(
                vec![
                    Ok(QuestionOutcome::MemberCompleted {
                        key: "url-key".to_owned(),
                        answer: serde_json::json!({"completed":true}),
                    }),
                    Ok(QuestionOutcome::Answered(QuestionAnswer::Round(
                        ElicitationAnswers {
                            confirmed: true,
                            responses,
                        },
                    ))),
                ],
                None,
            ));
            let foreign = Arc::new(RecordingForeignQuestionPort::default());
            with_foreign_turn_request(
                request,
                foreign_input_generator(),
                elicitor,
                foreign.clone(),
                (None, false),
                None,
                |mut turn, store| {
                    *store.fail_append.lock().unwrap() = Some((kind, published));
                    assert!(futures::executor::block_on(turn.run("invoke MCP")).is_err());
                    assert!(
                        foreign.calls().is_empty(),
                        "no remote retry across an uncertain session write"
                    );
                    let mut replay = SessionState::default();
                    for event in store.events() {
                        replay.apply(&event);
                    }
                    if kind != "question_requested" || published {
                        let pending = replay
                            .pending_question()
                            .expect("durable unfinished operation remains visible");
                        let QuestionMode::Round(round) = &pending.request().mode else {
                            panic!("round")
                        };
                        let member_was_durable = kind == "question_settled"
                            || (kind == "question_member_settled" && published);
                        assert_eq!(round.completed.contains_key("url-key"), member_was_durable);
                        if member_was_durable {
                            assert!(round.next_member().is_none());
                        }
                    } else {
                        assert!(replay.pending_question().is_none());
                    }
                },
            );
        }
    }
}

fn empty_round_request() -> QuestionRequest {
    QuestionRequest::new(
        "Retry MCP?",
        QuestionMode::Round(ego_core::question::ElicitationRound {
            expires_at_ms: None,
            requests: Default::default(),
            completed: Default::default(),
        }),
        vec![],
        false,
    )
}

#[test]
fn declining_successor_round_does_not_resurrect_predecessor() {
    // Catches: declining a successor leaves its tool incomplete and revives an answered predecessor.
    let request = empty_round_request();
    let answer = QuestionAnswer::Round(ego_core::question::ElicitationAnswers {
        confirmed: true,
        responses: Default::default(),
    });
    let foreign = Arc::new(RecordingForeignQuestionPort::scripted(
        vec![Ok(ToolExecution::InputRequired(
            ForeignInputRequired::new(SUCCESSOR_CONTINUATION_ID, request.clone()).unwrap(),
        ))],
        None,
    ));
    with_foreign_turn_request(
        request,
        foreign_input_generator(),
        Arc::new(RecordingElicitor::scripted(
            vec![
                Ok(QuestionOutcome::Answered(answer)),
                Ok(QuestionOutcome::Declined),
            ],
            None,
        )),
        foreign.clone(),
        (None, false),
        None,
        |mut turn, store| {
            futures::executor::block_on(turn.run("invoke MCP")).unwrap();
            assert_eq!(
                *foreign.cancelled.lock().unwrap(),
                vec![SUCCESSOR_CONTINUATION_ID]
            );
            assert_eq!(foreign.calls().len(), 1);
            let replay = SessionState::replay(store.events());
            assert!(
                replay.pending_question().is_none(),
                "decline resurrected an answered predecessor"
            );
            assert!(
                store.events().iter().any(|event| matches!(
                    event.kind,
                    EventKind::ToolCallCompleted { ok: false, .. }
                )),
                "cancelled original tool must be durably closed"
            );
        },
    );
}

struct InterruptingElicitor(ego_core::turn::InteractiveIngress);
impl Elicitor for InterruptingElicitor {
    fn elicit<'a>(
        &'a self,
        _: ItemId,
        _: QuestionRequest,
    ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>> {
        Box::pin(async move {
            self.0.interrupt().await.unwrap();
            futures::future::pending().await
        })
    }
}

#[test]
fn interrupted_foreign_round_has_no_pending_question_after_replay() {
    // Catches: an interrupted elicitor cancels remote authority but leaves the local question pending.
    let (ingress, mut control) = ego_core::turn::interactive_control();
    let foreign = Arc::new(RecordingForeignQuestionPort::default());
    with_foreign_turn_request(
        empty_round_request(),
        foreign_input_generator(),
        Arc::new(InterruptingElicitor(ingress)),
        foreign.clone(),
        (None, false),
        None,
        |turn, store| {
            let mut turn = turn.with_interactive_control(&mut control);
            let outcome = futures::executor::block_on(turn.run("invoke MCP")).unwrap();
            assert!(matches!(outcome.exit_reason, ExitReason::Cancelled(_)));
            assert_eq!(*foreign.cancelled.lock().unwrap(), vec![CONTINUATION_ID]);
            let replay = SessionState::replay(store.events());
            assert!(
                replay.pending_question().is_none(),
                "interruption left a pending foreign question after replay"
            );
            assert!(
                store.events().iter().any(|event| matches!(
                    event.kind,
                    EventKind::ToolCallCompleted { ok: false, .. }
                )),
                "interrupted original tool must be durably closed"
            );
        },
    );
}

#[test]
fn reopened_foreign_round_keeps_question_identity_and_completed_members() {
    // Catches: re-presentation mints a replacement question or drops durable member outcomes.
    let mut request = empty_round_request();
    let QuestionMode::Round(round) = &mut request.mode else {
        unreachable!()
    };
    round.requests.insert(
        "done".into(),
        ego_core::question::ElicitationMember {
            prompt: "Already completed".into(),
            kind: ego_core::question::ElicitationKind::Url {
                url: "https://example.test/done".into(),
                elicitation_id: "done".into(),
            },
        },
    );
    let foreign = Arc::new(RecordingForeignQuestionPort::default());
    let first = Arc::new(RecordingElicitor::scripted(
        vec![
            Ok(QuestionOutcome::MemberCompleted {
                key: "done".into(),
                answer: serde_json::json!({"completed":true}),
            }),
            Ok(QuestionOutcome::Unavailable),
        ],
        None,
    ));
    with_foreign_turn_request(
        request,
        foreign_input_generator(),
        first,
        foreign.clone(),
        (None, false),
        None,
        |mut turn, store| {
            futures::executor::block_on(turn.run("invoke MCP")).unwrap();
            let restored = SessionState::replay(store.events());
            let pending = restored.pending_question().unwrap();
            let question = pending.item_id();
            let QuestionMode::Round(round) = &pending.request().mode else {
                unreachable!()
            };
            let next = Arc::new(RecordingElicitor::answered(QuestionAnswer::Round(
                ego_core::question::ElicitationAnswers {
                    confirmed: true,
                    responses: round.completed.clone(),
                },
            )));
            let mut reopened = turn.with_elicitor(next.clone());
            let outcome =
                futures::executor::block_on(reopened.present_foreign_question(question)).unwrap();
            assert_eq!(outcome.exit_reason, ExitReason::Completed);
            assert_eq!(
                next.calls.lock().unwrap().as_slice(),
                &[(question, pending.request().clone())]
            );
            assert_eq!(
                store
                    .events()
                    .iter()
                    .filter(|event| matches!(event.kind, EventKind::QuestionRequested { .. }))
                    .count(),
                1
            );
            assert_eq!(foreign.calls().len(), 1);
            assert!(
                SessionState::replay(store.events())
                    .pending_question()
                    .is_none()
            );
        },
    );
}
