//! RED contract for one immutable admitted-tool capture per generation.
//!
//! The loop in these tests is the production `TurnLoop`; only the generator,
//! store, effect ports, and admitted source are test-owned ports.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ego_core::budget::{BudgetGate, Metering};
use ego_core::config::{Mode, Ruleset, SystemPrompt};
use ego_core::cost::MeteringRevision;
use ego_core::event::domain::{DomainEvent, EventKind, ExitReason};
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::attempt::{AttemptStream, GenerationAttempt};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::port::{AiEvent, Generator, RequestExpressibilityError};
use ego_core::generator::types::GenRequest;
use ego_core::generator::types::StopReason;
use ego_core::permit::{
    CanonPath, DuplexSpawner, OneHop, OneHopResponse, PolicyEngine, ProcessError, ProcessExecutor,
    Resolver, Tool, ToolError, ToolResult, Typed, TypedTool,
};
use ego_core::sink::CollectingSink;
use ego_core::state::SessionState;
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::{
    AdmittedToolSource, CapturedToolCatalogue, GenerationRuntime, RegisteredTool, Session,
    ToolCatalogueError, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

#[derive(Default)]
struct SourceState {
    captures: usize,
    catalogues: VecDeque<CapturedToolCatalogue>,
    order: Option<Arc<Mutex<Vec<&'static str>>>>,
    trace: Option<Arc<Mutex<Vec<&'static str>>>>,
}

struct SequencedSource {
    state: Arc<Mutex<SourceState>>,
}

struct OrderedGenerator {
    inner: Arc<FakeGenerator>,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl Generator for OrderedGenerator {
    fn ensure_expressible(&self, request: &GenRequest) -> Result<(), RequestExpressibilityError> {
        self.inner.ensure_expressible(request)
    }

    fn generate(&self, request: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
        self.order
            .lock()
            .expect("generation order")
            .push("generate");
        self.inner.generate(request, attempt)
    }
}

impl AdmittedToolSource for SequencedSource {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        let mut state = self.state.lock().expect("source state");
        state.captures += 1;
        if let Some(order) = &state.order {
            order.lock().expect("capture order").push("capture");
        }
        if let Some(trace) = &state.trace {
            trace.lock().expect("execution trace").push("capture");
        }
        state
            .catalogues
            .pop_front()
            .ok_or(ToolCatalogueError::CaptureUnavailable)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
struct EmptyParams {}

struct TaggedTool {
    label: String,
    on_execute: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl TaggedTool {
    fn new(label: &str) -> Arc<Typed<Self>> {
        Arc::new(Typed(Self {
            label: label.to_owned(),
            on_execute: None,
        }))
    }

    fn with_callback(label: &str, callback: Arc<dyn Fn() + Send + Sync>) -> Arc<Typed<Self>> {
        Arc::new(Typed(Self {
            label: label.to_owned(),
            on_execute: Some(callback),
        }))
    }
}

impl TypedTool for TaggedTool {
    type Params = EmptyParams;

    fn name(&self) -> &str {
        &self.label
    }

    fn description(&self) -> &str {
        "test tool whose result identifies the captured value"
    }

    fn prepare_typed(
        &self,
        _params: EmptyParams,
        _ctx: &ego_core::permit::PrepareCtx<'_>,
    ) -> Result<ego_core::permit::PreparedCall, ToolError> {
        Ok(ego_core::permit::PreparedCall::new(
            self.label.clone(),
            Vec::new(),
        ))
    }

    fn execute<'a>(
        &'a self,
        _call: ego_core::permit::PreparedCall,
        _io: &'a dyn ego_core::permit::GuardedIo,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        if let Some(callback) = &self.on_execute {
            callback();
        }
        Box::pin(futures::future::ready(Ok(ToolResult::builtin(
            self.label.clone(),
        ))))
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1_787_000_000_000)
    }
}

#[derive(Default)]
struct MemoryStore {
    events: Mutex<Vec<DomainEvent>>,
}

impl SessionStore for MemoryStore {
    fn append(&self, _id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.events.lock().expect("event store").push(event.clone());
        Ok(())
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<ego_core::run::RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        Err(StoreError::Io(std::io::Error::other(
            "run transactions are not used",
        )))
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

struct NoEvidence;

impl EvidenceStore for NoEvidence {
    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Err(EvidenceError::Limit { limit: 0 })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(futures::future::ready(Err(EvidenceError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }
}

struct NoProcess;

impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(futures::future::ready(Err(ProcessError::Invariant(
            "process execution is outside this generation test".to_owned(),
        ))))
    }
}

struct NoResolver;

impl Resolver for NoResolver {
    fn resolve(
        &self,
        _target: ego_core::permit::NetworkTarget,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, ego_core::permit::TransportError>>
    {
        panic!("the catalogue test must not resolve a network target")
    }
}

struct NoHop;

impl OneHop for NoHop {
    fn execute(
        &self,
        _connect: ego_core::permit::ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<OneHopResponse, ego_core::permit::TransportError>> {
        panic!("the catalogue test must not execute an HTTP hop")
    }
}

struct NoDuplex;

impl DuplexSpawner for NoDuplex {
    fn spawn(
        &self,
        _spec: ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'static, Result<ego_core::permit::DuplexHandle, ego_core::permit::TransportError>>
    {
        panic!("the catalogue test must not spawn a duplex")
    }
}

struct TestGenerationSource {
    generator: Arc<dyn ego_core::generator::port::Generator>,
}

impl ego_core::generation_model_source::GenerationModelSource for TestGenerationSource {
    fn resolve<'a>(
        &'a self,
        _role: ego_core::generation_model_source::GenerationRole,
    ) -> BoxFuture<
        'a,
        Result<
            ego_core::generation_model_source::GenerationProfile,
            ego_core::generation_model_source::GenerationModelError,
        >,
    > {
        let capabilities = ego_core::generator::capabilities::ModelCapabilities {
            schema_version: 1,
            max_context_tokens: Some(8_192),
            max_output_tokens: Some(256),
            max_tools: Some(32),
            supports_parallel_tool_calls: false,
            reasoning_carryover: ego_core::generator::capabilities::ReasoningCarryover::Unsupported,
            reasoning_efforts: ego_core::generator::capabilities::ReasoningEffortSupport::Any,
            reasoning_state_dialect: None,
            cancel: ego_core::generator::capabilities::CancelSemantics::Cooperative,
            cache: ego_core::generator::capabilities::CacheRetention::Short,
            transport: ego_core::generator::capabilities::Transport::Sse,
        };
        Box::pin(futures::future::ready(Ok(
            ego_core::generation_model_source::GenerationProfile {
                identity: ego_core::config::ModelRef("catalogue-test".to_owned()),
                request_model: "catalogue-test".to_owned(),
                thinking: ego_core::generator::types::ThinkingLevel::Off,
                generator: self.generator.clone(),
                capabilities,
                metering: Metering::Unmetered {
                    revision: MeteringRevision("catalogue-test".to_owned()),
                },
                rates: None,
            },
        )))
    }
}

fn loop_with(
    generator: Arc<dyn ego_core::generator::port::Generator>,
    static_tools: &[RegisteredTool],
    source: Arc<dyn AdmittedToolSource>,
) -> Result<(TurnLoop<'static>, &'static MemoryStore), ToolCatalogueError> {
    let store: &'static MemoryStore = Box::leak(Box::new(MemoryStore::default()));
    let clock = Box::leak(Box::new(FixedClock));
    let sink = Box::leak(Box::new(CollectingSink::new()));
    let policy = Box::leak(Box::new(PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        Ruleset::default(),
    )));
    let fs = Box::leak(Box::new(ego_core::permit::fake::FakeFs::default()));
    let process = Box::leak(Box::new(NoProcess));
    let evidence = Box::leak(Box::new(NoEvidence));
    let resolver = Arc::new(NoResolver);
    let hop = Arc::new(NoHop);
    let duplex = Arc::new(NoDuplex);
    let tools = Tools::from_registry_with_source(
        static_tools,
        source,
        policy,
        ToolEffectPorts::new(fs, process, evidence, resolver, hop, duplex),
        CanonPath::new("/workspace"),
    )?;
    let gate = Box::leak(Box::new(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    )));
    let runtime = GenerationRuntime {
        source: Arc::new(TestGenerationSource { generator }),
        gate,
        config: TurnConfig {
            system: None::<SystemPrompt>,
            token_ceiling: None,
        },
    };
    let session = Session {
        id: SessionId::new(),
        state: SessionState::default(),
        store,
        clock,
        sink,
        cancel: ego_core::turn::CancelToken::new(),
    };
    Ok((TurnLoop::new(session, runtime, tools), store))
}

fn call(call_id: &str, name: &str) -> AiEvent {
    AiEvent::ToolCall {
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        args: "{}".to_owned(),
    }
}

#[test]
fn each_real_generation_captures_once_and_sends_static_then_dynamic_order() {
    let static_a: Arc<dyn Tool> = TaggedTool::new("static-a");
    let static_b: Arc<dyn Tool> = TaggedTool::new("static-b");
    let dynamic_a: Arc<dyn Tool> = TaggedTool::new("dynamic-a");
    let dynamic_b: Arc<dyn Tool> = TaggedTool::new("dynamic-b");
    let order = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(Mutex::new(SourceState {
        captures: 0,
        order: Some(Arc::clone(&order)),
        trace: None,
        catalogues: VecDeque::from([
            CapturedToolCatalogue::new(
                "g1",
                vec![
                    RegisteredTool::from(dynamic_a),
                    RegisteredTool::from(dynamic_b),
                ],
            )
            .expect("distinct dynamic tools"),
            CapturedToolCatalogue::new(
                "g2",
                vec![RegisteredTool::from(TaggedTool::new("dynamic-c"))],
            )
            .expect("distinct dynamic tools"),
        ]),
    }));
    let source: Arc<dyn AdmittedToolSource> = Arc::new(SequencedSource {
        state: Arc::clone(&state),
    });
    let generator = Arc::new(FakeGenerator::new([
        vec![AiEvent::Stop(StopReason::Stop)],
        vec![AiEvent::Stop(StopReason::Stop)],
    ]));
    let static_tools = vec![
        RegisteredTool::from(static_a),
        RegisteredTool::from(static_b),
    ];
    let ordered = Arc::new(OrderedGenerator {
        inner: Arc::clone(&generator),
        order,
    });
    let (mut turn, _store) = loop_with(ordered, &static_tools, source).expect("valid catalogue");
    futures::executor::block_on(turn.run("first")).expect("first turn");
    futures::executor::block_on(turn.run("second")).expect("second turn");
    assert_eq!(state.lock().expect("source state").captures, 2);
    assert_eq!(
        state
            .lock()
            .expect("source state")
            .order
            .as_ref()
            .expect("capture order")
            .lock()
            .expect("capture order")
            .as_slice(),
        ["capture", "generate", "capture", "generate"]
    );
    let requests = generator.calls();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["static-a", "static-b", "dynamic-a", "dynamic-b"]
    );
    assert_eq!(
        requests[1]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["static-a", "static-b", "dynamic-c"]
    );
}

#[test]
fn static_static_dynamic_dynamic_and_cross_source_collisions_refuse_before_generation() {
    let names = [
        ("static-static", vec!["same", "same"], Vec::<&str>::new()),
        (
            "dynamic-dynamic",
            Vec::<&str>::new(),
            vec!["dynamic", "dynamic"],
        ),
        ("static-dynamic", vec!["same"], vec!["same"]),
    ];
    for (label, static_names, dynamic_names) in names {
        let static_tools = static_names
            .iter()
            .map(|name| RegisteredTool::from(TaggedTool::new(name)))
            .collect::<Vec<_>>();
        let dynamic_tools = dynamic_names
            .iter()
            .map(|name| RegisteredTool::from(TaggedTool::new(name)))
            .collect::<Vec<_>>();
        if label == "dynamic-dynamic" {
            assert!(matches!(
                CapturedToolCatalogue::new(label, dynamic_tools),
                Err(ToolCatalogueError::DuplicateName { name }) if name == "dynamic"
            ));
            continue;
        }
        let state = Arc::new(Mutex::new(SourceState {
            captures: 0,
            order: None,
            trace: None,
            catalogues: VecDeque::from([CapturedToolCatalogue::new(label, dynamic_tools)
                .expect("dynamic names are distinct")]),
        }));
        let source: Arc<dyn AdmittedToolSource> = Arc::new(SequencedSource {
            state: Arc::clone(&state),
        });
        let generator = Arc::new(FakeGenerator::new([vec![AiEvent::Stop(StopReason::Stop)]]));
        if label == "static-static" {
            assert!(matches!(
                loop_with(generator.clone(), &static_tools, source),
                Err(ToolCatalogueError::DuplicateName { name }) if name == "same"
            ));
            continue;
        }
        let (mut turn, _store) =
            loop_with(generator.clone(), &static_tools, source).expect("valid static catalogue");
        let outcome = futures::executor::block_on(turn.run("collision"))
            .expect("collision refusal is a turn outcome");
        assert!(!outcome.ok, "collision was accepted: {label}");
        assert!(
            generator.calls().is_empty(),
            "provider was reached for {label}"
        );
        assert_eq!(state.lock().expect("source state").captures, 1);
    }
}

#[test]
fn sibling_calls_keep_old_arcs_after_refresh_and_removal_then_next_generation_sees_new_capture() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(Mutex::new(SourceState {
        captures: 0,
        order: None,
        trace: Some(Arc::clone(&trace)),
        catalogues: VecDeque::new(),
    }));
    let old_a_callback = {
        let state = Arc::clone(&state);
        let trace = Arc::clone(&trace);
        Arc::new(move || {
            trace.lock().expect("execution trace").push("execute-old-a");
            let mut state = state.lock().expect("source state");
            state.catalogues.extend([
                CapturedToolCatalogue::new(
                    "new",
                    vec![
                        RegisteredTool::from(TaggedTool::with_callback(
                            "new-a",
                            Arc::new({
                                let trace = Arc::clone(&trace);
                                move || {
                                    trace.lock().expect("execution trace").push("execute-new-a");
                                }
                            }),
                        )),
                        RegisteredTool::from(TaggedTool::new("new-c")),
                    ],
                )
                .expect("replacement catalogue"),
                CapturedToolCatalogue::new(
                    "newer",
                    vec![RegisteredTool::from(TaggedTool::new("new-a"))],
                )
                .expect("final catalogue"),
            ]);
            trace
                .lock()
                .expect("execution trace")
                .push("refresh-published");
        }) as Arc<dyn Fn() + Send + Sync>
    };
    let old_b_callback = {
        let trace = Arc::clone(&trace);
        Arc::new(move || {
            trace.lock().expect("execution trace").push("execute-old-b");
        }) as Arc<dyn Fn() + Send + Sync>
    };
    state.lock().expect("source state").catalogues.push_back(
        CapturedToolCatalogue::new(
            "old",
            vec![
                RegisteredTool::from(TaggedTool::with_callback("old-a", old_a_callback)),
                RegisteredTool::from(TaggedTool::with_callback("old-b", old_b_callback)),
            ],
        )
        .expect("initial catalogue"),
    );
    let source: Arc<dyn AdmittedToolSource> = Arc::new(SequencedSource {
        state: Arc::clone(&state),
    });
    let generator = Arc::new(FakeGenerator::new([
        vec![
            call("a", "old-a"),
            call("b", "old-b"),
            AiEvent::Stop(StopReason::ToolUse),
        ],
        vec![call("c", "new-a"), AiEvent::Stop(StopReason::ToolUse)],
        vec![
            AiEvent::TextDelta {
                text: "done".to_owned(),
            },
            AiEvent::Stop(StopReason::Stop),
        ],
    ]));
    let (mut turn, store) = loop_with(generator.clone(), &[], source).expect("valid catalogue");
    let first = futures::executor::block_on(turn.run("refresh")).expect("first turn");
    assert_eq!(first.exit_reason, ExitReason::Completed);
    let calls = generator.calls();
    assert_eq!(
        calls[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["old-a", "old-b"]
    );
    assert_eq!(
        calls[1]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["new-a", "new-c"]
    );
    assert_eq!(state.lock().expect("source state").captures, 3);
    assert_eq!(
        trace.lock().expect("execution trace").as_slice(),
        [
            "capture",
            "execute-old-a",
            "refresh-published",
            "execute-old-b",
            "capture",
            "execute-new-a",
            "capture",
        ]
    );
    let completed = store
        .events
        .lock()
        .expect("event store")
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolCallCompleted { result, .. } => Some(result.model_text()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(completed.iter().any(|text| text == "old-a"));
    assert!(completed.iter().any(|text| text == "old-b"));
    assert!(completed.iter().any(|text| text == "new-a"));
}
