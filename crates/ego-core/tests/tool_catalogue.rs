//! contract for one immutable admitted-tool capture per generation.
//!
//! The loop in these tests is the production `TurnLoop`; only the generator,
//! store, effect ports, and admitted source are test-owned ports.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ego_core::budget::{BudgetGate, Metering, SharedBudgetGate};
use ego_core::config::{Mode, Ruleset, SystemPrompt};
use ego_core::cost::MeteringRevision;
use ego_core::event::domain::{DomainEvent, EventKind, ExitReason};
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{
    EvidenceAdmissionService, EvidenceError as AdmissionError, EvidenceKey, EvidenceRecord,
    EvidenceStore as AdmissionStore, EvidenceWrite, RunInvocationId,
};
use ego_core::generator::attempt::{AttemptStream, GenerationAttempt};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::port::{AiEvent, Generator, RequestExpressibilityError};
use ego_core::generator::types::GenRequest;
use ego_core::generator::types::{ContentBlock, Role, StopReason};
use ego_core::mcp::McpServerInstructions;
use ego_core::permit::{
    CanonPath, Confinement, DuplexSpawner, OneHop, OneHopResponse, ProcessError, ProcessExecutor,
    Resolver, Tool, ToolError, ToolResult, Typed, TypedTool,
};
use ego_core::sink::CollectingSink;
use ego_core::state::SessionState;
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::suspension::McpCapturePin;
use ego_core::turn::{
    AdmittedToolSource, AgentIdentity, CapturedToolCatalogue, GenerationRuntime, HostClient,
    RegisteredTool, Session, ToolCatalogueError, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

mod common;
#[path = "common/permission_guide.rs"]
mod permission_guide;

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
            ego_core::permit::Op::Read {
                path: ego_core::permit::CanonPath::new("/workspace/fixture"),
            },
        ))
    }

    fn execute<'a, 'io>(
        &'a self,
        _call: ego_core::permit::PreparedCall,
        _io: &'a dyn ego_core::permit::GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
    where
        'io: 'a,
    {
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
        _deadline: std::time::Instant,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, ego_core::permit::TransportError>>
    {
        panic!("the catalogue test must not resolve a network target")
    }
}

struct NoHop;

impl OneHop for NoHop {
    fn begin_call(&self) -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }
    fn execute(
        &self,
        _connect: ego_core::permit::ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: std::time::Instant,
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
        _request: ego_core::generation_model_source::GenerationModelRequest,
    ) -> BoxFuture<
        'a,
        Result<
            ego_core::generation_model_source::GenerationProfile,
            ego_core::generation_model_source::GenerationModelError,
        >,
    > {
        let capabilities = ego_core::generator::capabilities::ModelCapabilities {
            schema_version: ego_core::generator::capabilities::ModelCapabilities::SCHEMA_VERSION,
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
            image_input: ego_core::generator::capabilities::ImageInputSupport::Unknown,
            default_reasoning_effort: None,
        };
        Box::pin(futures::future::ready(Ok(
            ego_core::generation_model_source::GenerationProfile {
                output_limit: None,
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

fn loop_with_policy(
    generator: Arc<dyn ego_core::generator::port::Generator>,
    static_tools: &[RegisteredTool],
    source: Arc<dyn AdmittedToolSource>,
    policy_source: Arc<dyn ego_core::turn::GenerationPolicySource>,
    system: Option<SystemPrompt>,
    identity: Option<AgentIdentity>,
) -> Result<(TurnLoop<'static, 'static>, &'static MemoryStore), ToolCatalogueError> {
    loop_with_session_state(
        generator,
        static_tools,
        source,
        policy_source,
        system,
        identity,
        SessionState::default(),
    )
}

fn loop_with_session_state(
    generator: Arc<dyn ego_core::generator::port::Generator>,
    static_tools: &[RegisteredTool],
    source: Arc<dyn AdmittedToolSource>,
    policy_source: Arc<dyn ego_core::turn::GenerationPolicySource>,
    system: Option<SystemPrompt>,
    identity: Option<AgentIdentity>,
    state: SessionState,
) -> Result<(TurnLoop<'static, 'static>, &'static MemoryStore), ToolCatalogueError> {
    let store: &'static MemoryStore = Box::leak(Box::new(MemoryStore::default()));
    let clock = Box::leak(Box::new(FixedClock));
    let sink = Box::leak(Box::new(CollectingSink::new()));
    let fs = Box::leak(Box::new(ego_core::permit::fake::FakeFs::default()));
    let process = Box::leak(Box::new(NoProcess));
    let evidence = Box::leak(Box::new(NoEvidence));
    let resolver = Arc::new(NoResolver);
    let hop = Arc::new(NoHop);
    let duplex = Arc::new(NoDuplex);
    let tools = Tools::from_registry_with_source(
        static_tools,
        source,
        ToolEffectPorts::new(fs, process, evidence, resolver, hop, duplex),
        CanonPath::new("/workspace"),
    )?;
    let gate = SharedBudgetGate::new(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let runtime = GenerationRuntime {
        policy_source,
        source: Arc::new(TestGenerationSource { generator }),
        gate,
        config: TurnConfig {
            cache: ego_core::generator::capabilities::CacheRetention::Short,
            system,
            identity,
            token_ceiling: None,
            retry: 0,
            fallbacks: Vec::new(),
        },
        admission: Arc::new(EvidenceAdmissionService::new()),
        invocation: RunInvocationId::new(),
        retry_pause: Arc::new(NoRetryPause),
    };
    let session = Session {
        id: SessionId::new(),
        state,
        store,
        evidence,
        clock,
        sink,
        cancel: ego_core::turn::CancelToken::new(),
    };
    Ok((
        TurnLoop::new(session, runtime, tools).expect("budget gate is available"),
        store,
    ))
}

fn loop_with(
    generator: Arc<dyn ego_core::generator::port::Generator>,
    static_tools: &[RegisteredTool],
    source: Arc<dyn AdmittedToolSource>,
) -> Result<(TurnLoop<'static, 'static>, &'static MemoryStore), ToolCatalogueError> {
    loop_with_policy(
        generator,
        static_tools,
        source,
        common::default_policy_source(),
        None,
        None,
    )
}

/// `loop_with`, plus the session's own system prompt, so a test can assert on
/// what a foreign block is composed *around*.
fn loop_with_system(
    generator: Arc<dyn ego_core::generator::port::Generator>,
    static_tools: &[RegisteredTool],
    source: Arc<dyn AdmittedToolSource>,
    system: SystemPrompt,
) -> Result<(TurnLoop<'static, 'static>, &'static MemoryStore), ToolCatalogueError> {
    loop_with_policy(
        generator,
        static_tools,
        source,
        common::default_policy_source(),
        Some(system),
        None,
    )
}

fn loop_with_yolo(
    generator: Arc<dyn ego_core::generator::port::Generator>,
    static_tools: &[RegisteredTool],
    source: Arc<dyn AdmittedToolSource>,
) -> Result<(TurnLoop<'static, 'static>, &'static MemoryStore), ToolCatalogueError> {
    loop_with_policy(
        generator,
        static_tools,
        source,
        common::policy_source_with_root("/workspace", Mode::Yolo, Ruleset::default()),
        None,
        None,
    )
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
    let (mut turn, store) =
        loop_with_yolo(generator.clone(), &[], source).expect("valid catalogue");
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

/// A capture pin that says only what its servers said, for tests that care
/// about the instructions and not about which registry produced them.
struct InstructingPin {
    instructions: Vec<McpServerInstructions>,
    servers: Vec<String>,
}

impl McpCapturePin for InstructingPin {
    fn stable_fingerprint(&self) -> &str {
        "instructing-pin"
    }

    fn instructions(&self) -> &[McpServerInstructions] {
        &self.instructions
    }

    fn server_names(&self) -> Vec<String> {
        self.servers.clone()
    }
}

/// The identity block opens every generation and names what this capture
/// admitted — including a server that said nothing about itself, which the
/// instruction blocks alone would leave invisible.
#[test]
fn the_runtime_identity_names_the_model_workspace_posture_tools_and_every_mcp_server() {
    let pin = InstructingPin {
        instructions: vec![McpServerInstructions::new("alpha", "Prefer alpha_tool.")],
        servers: vec!["alpha".to_owned(), "silent".to_owned()],
    };
    let state = Arc::new(Mutex::new(SourceState {
        captures: 0,
        order: None,
        trace: None,
        catalogues: VecDeque::from([CapturedToolCatalogue::new_mcp(
            "g1",
            vec![RegisteredTool::from(TaggedTool::new("dynamic-a"))],
            pin,
        )
        .expect("distinct dynamic tools")]),
    }));
    let source: Arc<dyn AdmittedToolSource> = Arc::new(SequencedSource {
        state: Arc::clone(&state),
    });
    let generator = Arc::new(FakeGenerator::new([vec![AiEvent::Stop(StopReason::Stop)]]));
    let (mut turn, _store) = loop_with_policy(
        generator.clone(),
        &[],
        source,
        common::policy_source_with_root("/workspace", Mode::Plan, Ruleset::default()),
        Some(SystemPrompt::verbatim("the workspace speaks second")),
        Some(AgentIdentity {
            version: "9.9.9".to_owned(),
            sandbox: Confinement::Ro,
            offline: true,
            host: Some(HostClient {
                name: "tuicommander".to_owned(),
                version: "1.7.7".to_owned(),
            }),
        }),
    )
    .expect("valid catalogue");
    futures::executor::block_on(turn.run("which model?")).expect("first turn");

    let systems = system_texts(&generator.calls()[0]);
    assert!(systems[0].starts_with("You are ego 9.9.9, a coding agent."));
    let calls = generator.calls();
    let identity = calls[0]
        .messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.contains("Current runtime facts") => Some(text),
            _ => None,
        })
        .next()
        .expect("live identity");
    for line in [
        "- Model: catalogue-test (reasoning effort: off)",
        "- Workspace root: /workspace",
        "- Posture: permission mode: plan; sandbox: ro; network: offline",
        "- Host: tuicommander 1.7.7 over ACP",
        "- Tools: `dynamic-a`",
        "- MCP servers: `alpha`, `silent`",
    ] {
        assert!(identity.contains(line), "missing {line:?} in:\n{identity}");
    }
    assert_eq!(systems[1], "the workspace speaks second");
}

/// Catches: the model-visible system context omits permission recovery guidance.
#[test]
fn permission_recovery_guidance_reaches_the_stable_system_context() {
    let source: Arc<dyn AdmittedToolSource> = Arc::new(SequencedSource {
        state: Arc::new(Mutex::new(SourceState {
            catalogues: VecDeque::from([
                CapturedToolCatalogue::new("g1", Vec::new()).expect("empty catalogue"),
                CapturedToolCatalogue::new("g2", Vec::new()).expect("empty catalogue"),
            ]),
            ..SourceState::default()
        })),
    });
    let generator = Arc::new(FakeGenerator::new([
        vec![AiEvent::Stop(StopReason::Stop)],
        vec![AiEvent::Stop(StopReason::Stop)],
    ]));
    let (mut turn, _store) = loop_with_policy(
        generator.clone(),
        &[],
        source,
        common::policy_source_with_root("/workspace", Mode::Auto, Ruleset::default()),
        None,
        Some(AgentIdentity {
            version: "9.9.9".to_owned(),
            sandbox: Confinement::Workspace,
            offline: false,
            host: None,
        }),
    )
    .expect("valid catalogue");
    futures::executor::block_on(turn.run("how do I unblock a tool?")).expect("first turn");
    futures::executor::block_on(turn.run("continue")).expect("second turn");

    let calls = generator.calls();
    let first = system_texts(&calls[0]);
    let guidance_text = first.first().expect("stable system identity");
    let second = system_texts(&calls[1]);
    assert_eq!(
        Some(guidance_text),
        second.first(),
        "permission guidance must stay in the stable system prefix"
    );
    for guidance in [
        "Permissions and OS confinement are separate",
        "auto",
        "permissions.judge",
        "~/Gits",
        "exec_paths",
        "ego roots add",
        "roots=[]",
        "ego config unset roots",
        "exit and resume",
        "/config perimeter",
        "ego config ls --effective",
        "ego doctor",
        "ego rules",
        "do not bypass",
    ] {
        assert!(
            guidance_text.contains(guidance),
            "missing permission recovery guidance {guidance:?} in system context: {}",
            guidance_text
        );
    }
}

#[test]
fn server_instructions_reach_the_model_after_the_sessions_own_prompt() {
    // Built in the order a registry would *not* have produced, so passing the
    // order assertion means the capture ordered them rather than the fixture.
    let pin = InstructingPin {
        instructions: vec![
            McpServerInstructions::new("alpha", "Prefer alpha_tool for live lookups."),
            McpServerInstructions::new("zulu", "Prefer zulu_tool for archival lookups."),
        ],
        servers: Vec::new(),
    };
    let state = Arc::new(Mutex::new(SourceState {
        captures: 0,
        order: None,
        trace: None,
        catalogues: VecDeque::from([CapturedToolCatalogue::new_mcp(
            "g1",
            vec![RegisteredTool::from(TaggedTool::new("dynamic-a"))],
            pin,
        )
        .expect("distinct dynamic tools")]),
    }));
    let source: Arc<dyn AdmittedToolSource> = Arc::new(SequencedSource {
        state: Arc::clone(&state),
    });
    let generator = Arc::new(FakeGenerator::new([vec![AiEvent::Stop(StopReason::Stop)]]));
    let (mut turn, _store) = loop_with_system(
        generator.clone(),
        &[],
        source,
        SystemPrompt::verbatim("the workspace speaks first"),
    )
    .expect("valid catalogue");
    futures::executor::block_on(turn.run("first")).expect("first turn");
    let requests = generator.calls();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        system_texts(&requests[0]),
        [
            "the workspace speaks first".to_owned(),
            "Instructions from MCP server \"alpha\":\nPrefer alpha_tool for live lookups."
                .to_owned(),
            "Instructions from MCP server \"zulu\":\nPrefer zulu_tool for archival lookups."
                .to_owned(),
        ]
    );
}

#[test]
fn a_silent_server_adds_no_block_and_a_cut_one_says_it_was_cut() {
    let long = "x".repeat(ego_core::mcp::FOREIGN_VISIBLE_BYTES + 1_024);
    let pin = InstructingPin {
        instructions: vec![
            McpServerInstructions::new("quiet", ""),
            McpServerInstructions::new("verbose", &long),
        ],
        servers: Vec::new(),
    };
    assert!(
        pin.instructions()[1].truncated(),
        "the fixture must exceed the bound or it proves nothing"
    );
    let state = Arc::new(Mutex::new(SourceState {
        captures: 0,
        order: None,
        trace: None,
        catalogues: VecDeque::from([CapturedToolCatalogue::new_mcp(
            "g1",
            vec![RegisteredTool::from(TaggedTool::new("dynamic-a"))],
            pin,
        )
        .expect("distinct dynamic tools")]),
    }));
    let source: Arc<dyn AdmittedToolSource> = Arc::new(SequencedSource {
        state: Arc::clone(&state),
    });
    let generator = Arc::new(FakeGenerator::new([vec![AiEvent::Stop(StopReason::Stop)]]));
    let (mut turn, _store) = loop_with_system(
        generator.clone(),
        &[],
        source,
        SystemPrompt::verbatim("the workspace speaks first"),
    )
    .expect("valid catalogue");
    futures::executor::block_on(turn.run("first")).expect("first turn");
    let systems = system_texts(&generator.calls()[0]);
    assert_eq!(
        systems.len(),
        2,
        "the silent server must contribute no block at all: {systems:?}"
    );
    assert!(!systems[1].contains("quiet"));
    assert!(systems[1].starts_with("Instructions from MCP server \"verbose\":\n"));
    assert!(
        systems[1].ends_with(&format!(
            "\n…[truncated at {} bytes]",
            ego_core::mcp::FOREIGN_VISIBLE_BYTES
        )),
        "a cut instruction must say so in the text the model reads"
    );
    assert!(
        systems[1].len() <= ego_core::mcp::FOREIGN_VISIBLE_BYTES + 128,
        "the bound must be applied, not merely reported"
    );
}

/// Every `Role::System` block of one request, in order, as plain text.
fn system_texts(request: &GenRequest) -> Vec<String> {
    request
        .messages
        .messages()
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

struct HostNotes(Arc<Mutex<String>>);

impl AdmittedToolSource for HostNotes {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        CapturedToolCatalogue::new("stable", Vec::new())
    }
    fn context_notes(&self) -> Vec<String> {
        vec![self.0.lock().expect("host notes").clone()]
    }
}

// Catches: volatile host snapshots becoming System text or preceding the cached history.
#[test]
fn host_updates_preserve_the_prefix_and_append_context_after_current_user() {
    let notes = Arc::new(Mutex::new("host workspace revision one".to_owned()));
    let generator = Arc::new(FakeGenerator::new([
        vec![AiEvent::Stop(StopReason::Stop)],
        vec![AiEvent::Stop(StopReason::Stop)],
    ]));
    let (mut turn, store) = loop_with_policy(
        generator.clone(),
        &[],
        Arc::new(HostNotes(notes.clone())),
        common::default_policy_source(),
        Some(SystemPrompt::verbatim("stable instructions")),
        None,
    )
    .expect("catalogue");
    futures::executor::block_on(turn.run("first intent")).expect("first turn");
    *notes.lock().expect("notes") = "host workspace revision two".to_owned();
    futures::executor::block_on(turn.run("second intent")).expect("second turn");
    let calls = generator.calls();
    assert_eq!(system_texts(&calls[0]), vec!["stable instructions"]);
    assert_eq!(system_texts(&calls[0]), system_texts(&calls[1]));
    for (call, intent, note) in [
        (&calls[0], "first intent", "revision one"),
        (&calls[1], "second intent", "revision two"),
    ] {
        let messages = call.messages.messages();
        assert_eq!(
            call.cache.as_ref().unwrap().stable_messages,
            messages.len() - 1,
            "the boundary excludes the new context delta"
        );
        let user = &messages[messages.len() - 2];
        assert_eq!(user.role, Role::User);
        assert_eq!(
            user.content,
            vec![ContentBlock::Text {
                text: intent.to_owned()
            }]
        );
        let data = messages.last().expect("context tail");
        assert_eq!(data.role, Role::User);
        assert!(format!("{:?}", data.content).contains(note));
    }
    let durable = store.events.lock().expect("events");
    assert!(
        serde_json::to_string(&*durable)
            .expect("journal")
            .contains("host workspace revision")
    );
}

// Catches: fork/resume re-rendering edited instructions instead of inheriting the source epoch.
#[test]
fn fork_and_resume_reuse_the_snapshot_until_explicit_refresh() {
    let generator = Arc::new(FakeGenerator::new([vec![AiEvent::Stop(StopReason::Stop)]]));
    let (mut source, _) = loop_with_policy(
        generator,
        &[],
        Arc::new(HostNotes(Arc::new(Mutex::new("source host".into())))),
        common::default_policy_source(),
        Some(SystemPrompt::verbatim("source instructions")),
        None,
    )
    .expect("source");
    futures::executor::block_on(source.run("source prompt")).expect("source turn");
    let source_state = source.state();
    let inherited =
        ego_core::session::PreparedFork::new(SessionId::new(), &source_state, Timestamp::EPOCH)
            .expect("idle source");
    let child_state = SessionState::replay(inherited.seed().events().cloned());
    assert_eq!(child_state.prompt_snapshot, source_state.prompt_snapshot);
    let resumed_generator = Arc::new(FakeGenerator::new([
        vec![AiEvent::Stop(StopReason::Stop)],
        vec![AiEvent::Stop(StopReason::Stop)],
    ]));
    let (mut child, child_store) = loop_with_session_state(
        resumed_generator.clone(),
        &[],
        Arc::new(HostNotes(Arc::new(Mutex::new("child host".into())))),
        common::default_policy_source(),
        Some(SystemPrompt::verbatim("edited instructions")),
        None,
        child_state,
    )
    .expect("child");
    futures::executor::block_on(child.run("child prompt")).expect("child turn");
    assert_eq!(
        system_texts(&resumed_generator.calls()[0]),
        vec!["source instructions"]
    );
    let inherited_epoch = child.state().prompt_snapshot.expect("epoch").epoch;
    child = child.with_prompt_refresh();
    futures::executor::block_on(child.run("refresh prompt")).expect("refreshed turn");
    assert_eq!(
        system_texts(&resumed_generator.calls()[1]),
        vec!["edited instructions"]
    );
    assert_ne!(
        child.state().prompt_snapshot.expect("new epoch").epoch,
        inherited_epoch
    );
    let replayed = SessionState::replay(child_store.events.lock().expect("journal").clone());
    assert_eq!(replayed.prompt_snapshot, child.state().prompt_snapshot);
    assert!(
        serde_json::to_string(&*child_store.events.lock().expect("journal"))
            .expect("json")
            .contains("child host")
    );
}

// Catches: oversized Unicode host data panicking at a byte boundary or replacing user intent.
#[test]
fn oversized_host_context_is_bounded_without_splitting_unicode_or_user_text() {
    let generator = Arc::new(FakeGenerator::new([vec![AiEvent::Stop(StopReason::Stop)]]));
    let (mut turn, _) = loop_with_policy(
        generator.clone(),
        &[],
        Arc::new(HostNotes(Arc::new(Mutex::new("漢".repeat(30_000))))),
        common::default_policy_source(),
        None,
        None,
    )
    .expect("source");
    futures::executor::block_on(turn.run("the user decides")).expect("turn");
    let calls = generator.calls();
    let messages = calls[0].messages.messages();
    let ContentBlock::Text { text } = &messages.last().unwrap().content[0] else {
        panic!("data text");
    };
    assert!(text.len() <= 65_536 + "\n[context truncated]".len());
    assert!(text.ends_with("[context truncated]"));
    assert_eq!(
        messages.first().expect("user").content,
        vec![ContentBlock::Text {
            text: "the user decides".into()
        }]
    );
}

/// Retries in these tests do not wait on a wall clock.
struct NoRetryPause;

impl ego_core::turn::RetryPause for NoRetryPause {
    fn pause(&self, _delay: std::time::Duration) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(futures::future::ready(()))
    }
}

// Catches: live compaction discards the captured MCP instructions, tool order,
// or volatile host note when reconstructing its source prefix.
#[test]
fn live_compaction_preserves_the_captured_mcp_and_host_prefix() {
    let pin = InstructingPin {
        instructions: vec![McpServerInstructions::new(
            "alpha",
            "Stable server guidance",
        )],
        servers: vec!["alpha".to_owned()],
    };
    let state = Arc::new(Mutex::new(SourceState {
        captures: 0,
        order: None,
        trace: None,
        catalogues: VecDeque::from([CapturedToolCatalogue::new_mcp(
            "g1",
            vec![RegisteredTool::from(TaggedTool::new("dynamic-a"))],
            pin,
        )
        .unwrap()]),
    }));
    let checkpoint = "# Objective\nContinue source\n# Binding constraints and preferences\nRead only\n# Completed work and evidence\nSource answered\n# Work in progress\nNone\n# Decisions and rationale\nRetain prefix\n# Blockers and uncertainty\nNone\n# Ordered next steps\nContinue\n# Critical references\nSource";
    let generator = Arc::new(FakeGenerator::new([
        vec![
            AiEvent::TextDelta {
                text: "source answer".into(),
            },
            AiEvent::Stop(StopReason::Stop),
        ],
        vec![
            AiEvent::TextDelta {
                text: checkpoint.into(),
            },
            AiEvent::Stop(StopReason::Stop),
        ],
    ]));
    let (mut turn, _) = loop_with_policy(
        generator.clone(),
        &[],
        Arc::new(SequencedSource { state }),
        common::default_policy_source(),
        Some(SystemPrompt::verbatim("stable system")),
        None,
    )
    .unwrap();
    futures::executor::block_on(turn.run("source intent")).unwrap();
    futures::executor::block_on(turn.prepare_compaction(None, Vec::new())).unwrap();
    let calls = generator.calls();
    assert_eq!(system_texts(&calls[0]), system_texts(&calls[1]));
    assert_eq!(calls[0].tools, calls[1].tools);
    assert_eq!(calls[0].cache, calls[1].cache);
    assert!(
        calls[1]
            .messages
            .messages()
            .starts_with(calls[0].messages.messages())
    );
    assert!(
        serde_json::to_string(&calls[1])
            .unwrap()
            .contains("source answer")
    );
}

// Catches: refreshing volatile context removes the prior host item and breaks
// the source request prefix at the next user turn or after cold replay.
#[test]
fn host_context_deltas_append_after_history_and_survive_replay() {
    let notes = Arc::new(Mutex::new("host revision one".to_owned()));
    let generator = Arc::new(FakeGenerator::new([
        vec![
            AiEvent::TextDelta {
                text: "first answer".into(),
            },
            AiEvent::Stop(StopReason::Stop),
        ],
        vec![AiEvent::Stop(StopReason::Stop)],
    ]));
    let (mut turn, store) = loop_with_policy(
        generator.clone(),
        &[],
        Arc::new(HostNotes(notes.clone())),
        common::default_policy_source(),
        Some(SystemPrompt::verbatim("stable system")),
        None,
    )
    .unwrap();
    futures::executor::block_on(turn.run("first intent")).unwrap();
    *notes.lock().unwrap() = "host revision two".into();
    futures::executor::block_on(turn.run("second intent")).unwrap();
    let calls = generator.calls();
    assert!(
        calls[1]
            .messages
            .messages()
            .starts_with(calls[0].messages.messages()),
        "host refresh must extend the previous request, not remove or insert an item"
    );
    let first = calls[0].messages.messages();
    assert!(format!("{:?}", first.last().unwrap()).contains("host revision one"));
    let replayed = SessionState::replay(store.events.lock().unwrap().clone());
    let resumed_generator = Arc::new(FakeGenerator::new([vec![AiEvent::Stop(StopReason::Stop)]]));
    let (mut resumed, _) = loop_with_session_state(
        resumed_generator.clone(),
        &[],
        Arc::new(HostNotes(notes)),
        common::default_policy_source(),
        None,
        None,
        replayed,
    )
    .unwrap();
    futures::executor::block_on(resumed.run("third intent")).unwrap();
    let resumed_calls = resumed_generator.calls();
    let text = serde_json::to_string(&resumed_calls[0]).unwrap();
    assert!(text.contains("host revision one") && text.contains("host revision two"));
    assert_eq!(
        text.matches("host revision two").count(),
        1,
        "unchanged context is not appended again"
    );
}
