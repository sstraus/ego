//! RED runtime contract for one-turn question suspension.
//!
//! This module deliberately exercises the core-owned async port and the
//! TurnLoop attachment point.  Host adapters are not allowed to become a
//! second parser or to manufacture a question id: the id passed to Elicitor
//! is the persisted QuestionRequested envelope ItemId.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use ego_core::budget::{BudgetGate, Metering};
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
use ego_core::mcp::{ForeignInputRequired, ToolExecution};
use ego_core::permit::fake::{FakeFs, FakeWriteTool};
use ego_core::permit::{
    CanonPath, DuplexHandle, DuplexSpawner, GuardedIo, HttpRequest, OneHop, OneHopResponse,
    PrepareCtx, PreparedCall, ProcessError, ProcessExecutor, ResolvedConnect, Resolver, SpawnSpec,
    Tool, ToolDescriptor, ToolError, ToolResult, TransportError, Typed,
};
use ego_core::question::{
    AskQuestionTool, Elicitor, ElicitorError, QuestionAnswer, QuestionMode, QuestionOption,
    QuestionOutcome, QuestionRequest,
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
    outcome: Mutex<Option<Result<QuestionOutcome, ElicitorError>>>,
}

impl RecordingElicitor {
    fn answered(answer: QuestionAnswer) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            outcome: Mutex::new(Some(Ok(QuestionOutcome::Answered(answer)))),
        }
    }
}

impl Elicitor for RecordingElicitor {
    fn elicit<'a>(
        &'a self,
        question: ItemId,
        request: QuestionRequest,
    ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>> {
        self.calls
            .lock()
            .expect("elicitor calls")
            .push((question, request));
        let outcome = self
            .outcome
            .lock()
            .expect("elicitor outcome")
            .take()
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
}

impl MemoryStore {
    fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().expect("memory store events").clone()
    }
}

impl SessionStore for MemoryStore {
    fn append(
        &self,
        _id: &ego_core::event::id::SessionId,
        event: &DomainEvent,
    ) -> Result<(), StoreError> {
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

struct ForeignInputTool;

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

    fn execute<'a>(
        &'a self,
        _call: PreparedCall,
        _io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>> {
        Box::pin(async {
            Ok(ToolExecution::InputRequired(
                ForeignInputRequired::new("regression_case").expect("valid continuation id"),
            ))
        })
    }
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
    let mut gate = BudgetGate::new(Money::new(
        "1000".parse::<Amount>().expect("amount"),
        CurrencyCode::new("USD").expect("currency"),
    ));
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
            gate: &mut gate,
            config: TurnConfig {
                system: None,
                token_ceiling: None,
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
        },
        tools,
    )
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
fn foreign_input_required_is_projected_to_a_durable_pending_question() {
    let generator = Arc::new(FakeGenerator::new(vec![vec![
        AiEvent::ToolCall {
            call_id: "mcp-input-call".into(),
            name: "mcp_input".into(),
            args: "{}".into(),
        },
        AiEvent::Stop(StopReason::ToolUse),
    ]]));
    let store = MemoryStore::default();
    let clock = FixedClock;
    let sink = CollectingSink::new();
    let mut gate = BudgetGate::new(Money::new(
        "1000".parse::<Amount>().expect("amount"),
        CurrencyCode::new("USD").expect("currency"),
    ));
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
    let registry = vec![RegisteredTool::Effect(
        Arc::new(ForeignInputTool) as Arc<dyn Tool>
    )];
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
            gate: &mut gate,
            config: TurnConfig {
                system: None,
                token_ceiling: None,
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
        },
        tools,
    )
    .with_elicitor(Arc::new(UnavailableElicitor));

    let outcome = futures::executor::block_on(turn.run("invoke the MCP operation"))
        .expect("foreign input suspension is a handled turn outcome");
    assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
    assert!(!outcome.ok);
    let pending = turn
        .state()
        .pending_question()
        .expect("ForeignInputRequired must become a durable pending question");
    assert!(pending.request().validate().is_ok());
    assert!(
        store
            .events()
            .iter()
            .all(|event| !matches!(event.kind, EventKind::TurnCompleted { .. })),
        "a foreign-input suspension must not publish TurnCompleted"
    );
    assert!(
        store
            .events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::QuestionRequested { .. })),
        "QuestionRequested must be durable before exposing the suspension"
    );
}
