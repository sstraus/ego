//! RED runtime contract for one-turn question suspension.
//!
//! This module deliberately exercises the core-owned async port and the
//! TurnLoop attachment point.  Host adapters are not allowed to become a
//! second parser or to manufacture a question id: the id passed to Elicitor
//! is the persisted QuestionRequested envelope ItemId.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ego_core::budget::{BudgetGate, Metering, SharedBudgetGate};
use ego_core::config::{Mode, ModelRef, Ruleset};
use ego_core::cost::{Amount, CurrencyCode, MeteringRevision, Money};
use ego_core::event::domain::{DomainEvent, EventKind, ExitReason};
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
use ego_core::mcp::{ForeignInputRequired, ForeignQuestionPort, ToolExecution};
use ego_core::permit::fake::{FakeFs, FakeWriteTool};
use ego_core::permit::{
    CanonPath, DuplexHandle, DuplexSpawner, GuardedIo, HttpRequest, OneHop, OneHopResponse,
    PrepareCtx, PreparedCall, ProcessError, ProcessExecutor, ResolvedConnect, Resolver, SpawnSpec,
    Tool, ToolDescriptor, ToolError, ToolResult, TransportError, Typed,
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
    CancelToken, GenerationRuntime, RegisteredTool, Session, ToolEffectPorts, Tools, TurnConfig,
    TurnLoop,
};
use futures::future::BoxFuture;
use futures::stream::BoxStream;

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
fn turn_loop_accepts_the_core_elicitor(
    turn: TurnLoop<'_>,
    elicitor: Arc<dyn Elicitor>,
) -> TurnLoop<'_> {
    turn.with_elicitor(elicitor)
}

#[derive(Default)]
struct MemoryStore {
    events: Mutex<Vec<DomainEvent>>,
    trace: Option<Arc<Mutex<Vec<&'static str>>>>,
}

impl MemoryStore {
    fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().expect("memory store events").clone()
    }

    fn with_trace(trace: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
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
        _deadline: Option<Instant>,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, TransportError>> {
        panic!("question suspension unexpectedly resolved an HTTP target")
    }
}

struct NoHop;

impl OneHop for NoHop {
    fn execute(
        &self,
        _connect: ResolvedConnect,
        _request: HttpRequest,
        _deadline: Option<Instant>,
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
            identity: ModelRef("test-model".into()),
            request_model: "test-model".into(),
            thinking: ego_core::generator::types::ThinkingLevel::Off,
            generator: self.generator.clone(),
            capabilities: ModelCapabilities {
                schema_version: ModelCapabilities::SCHEMA_VERSION,
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
            },
            metering: Metering::Unmetered {
                revision: MeteringRevision("question-runtime".into()),
            },
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
            calls: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes.into()),
            trace,
        }
    }
}

impl ForeignQuestionPort for RecordingForeignQuestionPort {
    fn resume<'a>(
        &'a self,
        continuation_id: &'a str,
        canonical_json: &'a str,
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
        Ok(PreparedCall::new(self.descriptor().name, Vec::new()))
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
                ForeignInputRequired::new("regression_case", self.request.clone())
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
    test: impl FnOnce(TurnLoop<'_>, &MemoryStore) -> R,
) -> R {
    let store = trace.map_or_else(MemoryStore::default, MemoryStore::with_trace);
    let clock = FixedClock;
    let sink = CollectingSink::new();
    let gate = SharedBudgetGate::new(BudgetGate::new(Money::new(
        "1000".parse::<Amount>().expect("amount"),
        CurrencyCode::new("USD").expect("currency"),
    )));
    let policy = ego_core::permit::PolicyEngine::new(
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
    let registry = vec![RegisteredTool::Effect(Arc::new(ForeignInputTool {
        request: foreign_question_request(),
    }) as Arc<dyn Tool>)];
    let tools = Tools::from_registry(
        &registry,
        &policy,
        ToolEffectPorts::new(&fs, &process, &evidence, resolver, one_hop, duplex),
        CanonPath::new("/ws"),
    );
    let source = Arc::new(FixedSource { generator });
    let turn = TurnLoop::new(
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
            source,
            gate,
            config: TurnConfig {
                system: None,
                token_ceiling: None,
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
        },
        tools,
    )
    .expect("budget gate is available")
    .with_elicitor(elicitor)
    .with_foreign_question_port(foreign_port);
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
    let policy = ego_core::permit::PolicyEngine::new(
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
        &policy,
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
            source,
            gate,
            config: TurnConfig {
                system: None,
                token_ceiling: None,
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
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
    let foreign = ForeignInputRequired::new("regression_case", request.clone())
        .expect("valid continuation id");
    assert_eq!(foreign.continuation_id(), "regression_case");
    assert_eq!(foreign.question_request(), &request);
}

#[test]
fn foreign_input_required_is_projected_to_a_durable_pending_question() {
    let generator = foreign_input_generator();
    let foreign_port = Arc::new(RecordingForeignQuestionPort::default());
    with_foreign_turn(
        generator,
        Arc::new(UnavailableElicitor),
        foreign_port.clone(),
        None,
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
            assert_eq!(continuation_id, "regression_case");
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
            let resumed = futures::executor::block_on(turn.resume_question(question, answer))
                .expect("foreign continuation resumes the turn");
            assert!(resumed.ok);
            assert_eq!(
                foreign_port.calls(),
                vec![(
                    "regression_case".to_owned(),
                    r#"{"target":"staging"}"#.to_owned()
                )]
            );
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
                    "regression_case".to_owned(),
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
    let successor = ForeignInputRequired::new("regression_case", foreign_question_request())
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
                } if continuation_id == "regression_case"
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
