//! RED contract for lifecycle production at the real core owners (M4 S7).

mod common;

use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use ego_core::budget::{BudgetGate, Metering};
use ego_core::config::{ConfigLayer, Mode, ModelRef, PermCfg, Rule, Ruleset, Source, Trust};
use ego_core::cost::MeteringRevision;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::hook::{
    HookActivation, HookDecision, HookFailure, HookFailureKind, HookInvocationId, HookLifecycle,
    HookOccurrenceKey, HookOccurrenceOwner, HookOutcome, HookPlan, HookResult,
};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::skill::{
    SkillContentDigest, SkillDescriptorEvidence, SkillId, SkillSourceAuthority, SkillSourceLocation,
};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{
    EvidenceAdmissionService, EvidenceKey, EvidenceRecord, EvidenceWrite, RunInvocationId,
};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{StopReason, ThinkingLevel};
use ego_core::hook::{
    HookCatalogue, HookEvent, HookFuture, HookLifecycleDispatcher, HookOccurrence, HookPort,
    HookReachFuture, HookReachedEvidence, HookRecordFuture, HookRunner, HookSpec, HookStartFuture,
    HookStartedEvidence,
};
use ego_core::mcp::ToolExecution;
use ego_core::permit::{
    CanonPath, DnsResolution, DuplexHandle, DuplexSpawner, EnvPolicy, GuardedIo, OneHop,
    OneHopResponse, Op, PolicyEngine, PrepareCtx, PreparedCall, ProcessError, ProcessExecutor,
    Resolver, SpawnSpec, Tool, ToolDescriptor, ToolError, ToolResult, TransportError,
    TransportFailure,
};
use ego_core::sink::CollectingSink;
use ego_core::skill::{SkillInvocation, SkillOperation, SkillSpec};
use ego_core::state::{HookReplayDisposition, RefusalReason, SessionState};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::{
    CancelToken, GenerationRuntime, Session, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
};
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::{Value, json};

const LIFECYCLES: [HookLifecycle; 21] = [
    HookLifecycle::SessionStart,
    HookLifecycle::InstructionsLoaded,
    HookLifecycle::UserPromptSubmit,
    HookLifecycle::UserPromptExpansion,
    HookLifecycle::PreToolUse,
    HookLifecycle::PermissionRequest,
    HookLifecycle::PermissionDenied,
    HookLifecycle::PostToolUse,
    HookLifecycle::PostToolUseFailure,
    HookLifecycle::PostToolBatch,
    HookLifecycle::PreModelSwitch,
    HookLifecycle::PostModelSwitch,
    HookLifecycle::SubagentStart,
    HookLifecycle::SubagentStop,
    HookLifecycle::Stop,
    HookLifecycle::StopFailure,
    HookLifecycle::PreCompact,
    HookLifecycle::PostCompact,
    HookLifecycle::Elicitation,
    HookLifecycle::ElicitationResult,
    HookLifecycle::SessionEnd,
];

fn catalogue() -> HookCatalogue {
    let source = LIFECYCLES
        .iter()
        .map(|event| format!("[[hooks]]\nevent = \"{}\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n", event.as_str()))
        .collect::<String>();
    HookCatalogue::admit_typed(
        &[ConfigLayer::parse(Source::User, &source).expect("admitted hooks")],
        &[],
        Trust::Trusted,
    )
    .expect("complete lifecycle catalogue")
}

fn occurrence(lifecycle: HookLifecycle, owner: HookOccurrenceOwner) -> HookOccurrence {
    HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, owner),
        HookEvent::session(lifecycle),
    )
    .expect("matching lifecycle occurrence")
}

#[derive(Clone, Default)]
struct RecordingPort {
    starts: Arc<Mutex<Vec<HookStartedEvidence>>>,
    calls: Arc<Mutex<Vec<HookLifecycle>>>,
}

#[derive(Clone, Default)]
struct BlockingPort(RecordingPort);

#[derive(Clone, Default)]
struct FailingPort(RecordingPort);

impl RecordingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.calls.lock().expect("hook calls").clone()
    }
}

impl BlockingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.0.calls()
    }
}

impl FailingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.0.calls()
    }
}

macro_rules! delegate_hook_port {
    ($name:ty, $outcome:expr) => {
        impl HookPort for $name {
            fn reached<'a>(
                &'a self,
                producer: &'a Producer,
                occurrence: &'a HookOccurrence,
                plan: &'a HookPlan,
            ) -> HookReachFuture<'a> {
                self.0.reached(producer, occurrence, plan)
            }
            fn started<'a>(
                &'a self,
                spec: &'a HookSpec,
                occurrence: &'a HookOccurrence,
                reached: &'a HookReachedEvidence,
                activation: HookActivation,
                invocation: HookInvocationId,
            ) -> HookStartFuture<'a> {
                self.0
                    .started(spec, occurrence, reached, activation, invocation)
            }
            fn invoke<'a>(&'a self, _spec: &'a HookSpec, _event: HookEvent) -> HookFuture<'a> {
                Box::pin(std::future::ready($outcome))
            }
            fn settled<'a>(
                &'a self,
                spec: &'a HookSpec,
                started: HookStartedEvidence,
                result: &'a HookResult,
            ) -> HookRecordFuture<'a> {
                self.0.settled(spec, started, result)
            }
        }
    };
}

delegate_hook_port!(
    BlockingPort,
    Ok(HookOutcome::new(HookDecision::Block, None))
);

impl HookPort for FailingPort {
    fn reached<'a>(
        &'a self,
        producer: &'a Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        self.0.reached(producer, occurrence, plan)
    }

    fn started<'a>(
        &'a self,
        spec: &'a HookSpec,
        occurrence: &'a HookOccurrence,
        reached: &'a HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> HookStartFuture<'a> {
        self.0
            .started(spec, occurrence, reached, activation, invocation)
    }

    fn invoke<'a>(&'a self, spec: &'a HookSpec, event: HookEvent) -> HookFuture<'a> {
        if event.lifecycle() == HookLifecycle::PostToolUseFailure {
            Box::pin(std::future::ready(Err(HookFailure::new(
                HookFailureKind::HandlerFailed,
                None,
            ))))
        } else {
            self.0.invoke(spec, event)
        }
    }

    fn settled<'a>(
        &'a self,
        spec: &'a HookSpec,
        started: HookStartedEvidence,
        result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        self.0.settled(spec, started, result)
    }
}

impl HookPort for RecordingPort {
    fn reached<'a>(
        &'a self,
        producer: &'a Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        let evidence = HookReachedEvidence::new(
            EventId::new(),
            occurrence.key().clone(),
            occurrence.event().subject(),
            producer.clone(),
            plan.clone(),
        );
        Box::pin(std::future::ready(Ok(
            ego_core::hook::HookReachOutcome::Fresh(evidence),
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
        let evidence = HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        );
        self.starts.lock().expect("hook starts").push(evidence);
        self.calls
            .lock()
            .expect("hook calls")
            .push(occurrence.event().lifecycle());
        Box::pin(std::future::ready(Ok(evidence)))
    }

    fn invoke<'a>(&'a self, _spec: &'a HookSpec, _event: HookEvent) -> HookFuture<'a> {
        Box::pin(std::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

#[derive(Default)]
struct Store {
    events: Mutex<Vec<DomainEvent>>,
}
impl Store {
    fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().expect("events").clone()
    }
}
impl SessionStore for Store {
    fn append(&self, _session: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.events.lock().expect("events").push(event.clone());
        Ok(())
    }
    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _revision: Option<ego_core::run::RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        Err(StoreError::Io(std::io::Error::other("run path unused")))
    }
    fn read(
        &self,
        _session: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        Err(StoreError::Io(std::io::Error::other("read path unused")))
    }
    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Err(StoreError::Io(std::io::Error::other("list path unused")))
    }
}

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1_787_000_000_000)
    }
}

struct NoProcess;
impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(std::future::ready(Err(ProcessError::Invariant(
            "unused process".to_owned(),
        ))))
    }
}

struct FailingExecTool;

impl Tool for FailingExecTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "failing_exec".to_owned(),
            description: "executes a process that fails in the guarded test port".to_owned(),
            schema: serde_json::from_value(json!({"type": "object"})).expect("schema"),
        }
    }

    fn prepare(&self, _params: Value, _ctx: &PrepareCtx<'_>) -> Result<PreparedCall, ToolError> {
        let spec = SpawnSpec::new(
            vec![OsString::from("hook-lifecycle-failure")],
            None,
            None,
            EnvPolicy::Inherited,
        )
        .expect("valid test process");
        Ok(PreparedCall::new(
            self.descriptor().name,
            vec![Op::Spawn { spec }],
        ))
    }

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>> {
        Box::pin(async move {
            let Some(Op::Spawn { spec }) = call.ops.into_iter().next() else {
                return Err(ToolError::InvalidParams("expected one Spawn op".to_owned()));
            };
            io.exec(&spec)
                .await
                .map(ToolExecution::Complete)
                .map_err(ToolError::from)
        })
    }
}
struct NoResolver;
impl Resolver for NoResolver {
    fn resolve(
        &self,
        _target: ego_core::permit::NetworkTarget,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            TransportFailure::Resolution("unused resolver".to_owned()),
        ))))
    }
}
struct NoHop;
impl OneHop for NoHop {
    fn execute(
        &self,
        _connect: ego_core::permit::ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            TransportFailure::Connect("unused hop".to_owned()),
        ))))
    }
}
struct NoDuplex;
impl DuplexSpawner for NoDuplex {
    fn spawn(
        &self,
        _spec: ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            TransportFailure::Connect("unused duplex".to_owned()),
        ))))
    }
}
struct NoEvidence;
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
        Box::pin(std::future::ready(Err(EvidenceError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }
}

struct NoAdmission;
impl ego_core::evidence::EvidenceStore for NoAdmission {
    fn write(
        &self,
        _key: EvidenceKey,
        _record: EvidenceRecord,
    ) -> Result<EvidenceWrite, ego_core::evidence::EvidenceError> {
        Ok(EvidenceWrite::Published)
    }
}
static NO_ADMISSION: NoAdmission = NoAdmission;

struct GeneratorSource {
    generator: Arc<FakeGenerator>,
}
impl ego_core::generation_model_source::GenerationModelSource for GeneratorSource {
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
        Box::pin(std::future::ready(Ok(
            ego_core::generation_model_source::GenerationProfile {
                identity: ModelRef("test/model".to_owned()),
                request_model: "test-model".to_owned(),
                thinking: ThinkingLevel::Off,
                generator: self.generator.clone() as Arc<dyn ego_core::generator::port::Generator>,
                capabilities: ModelCapabilities {
                    schema_version: ModelCapabilities::SCHEMA_VERSION,
                    max_context_tokens: Some(128_000),
                    max_output_tokens: Some(16_000),
                    max_tools: Some(128),
                    supports_parallel_tool_calls: true,
                    reasoning_carryover: ReasoningCarryover::Unsupported,
                    reasoning_efforts: ReasoningEffortSupport::Any,
                    reasoning_state_dialect: None,
                    cancel: CancelSemantics::Cooperative,
                    cache: CacheRetention::Short,
                    transport: Transport::Sse,
                    image_input: ImageInputSupport::Unknown,
                },
                metering: Metering::Unmetered {
                    revision: MeteringRevision("regression_case".to_owned()),
                },
                rates: None,
            },
        )))
    }
}

fn run_turn<P: HookPort + Clone>(
    script: Vec<AiEvent>,
    port: &P,
) -> (Vec<DomainEvent>, SessionState) {
    run_turn_scripts(vec![script], port)
}

fn run_turn_scripts<P: HookPort + Clone>(
    scripts: Vec<Vec<AiEvent>>,
    port: &P,
) -> (Vec<DomainEvent>, SessionState) {
    let store = Store::default();
    let clock = FixedClock;
    let sink = CollectingSink::new();
    let generator = Arc::new(FakeGenerator::new(scripts));
    let mut gate = BudgetGate::new(ego_core::cost::Money::new(
        ego_core::cost::Amount::new(1000, 0).expect("amount"),
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let policy = PolicyEngine::new(
        CanonPath::new("/ws"),
        Mode::Default,
        Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![Rule::load("exec(*)").expect("exec rule")],
                deny: Vec::new(),
            },
            Source::Project,
        ),
    );
    let process = NoProcess;
    let evidence = NoEvidence;
    let fs = ego_core::permit::fake::FakeFs::default();
    let registry: [Arc<dyn Tool>; 1] = [Arc::new(FailingExecTool)];
    let tools = Tools::new(
        &registry,
        &policy,
        ToolEffectPorts::new(
            &fs,
            &process,
            &evidence,
            Arc::new(NoResolver),
            Arc::new(NoHop),
            Arc::new(NoDuplex),
        ),
        CanonPath::new("/ws"),
    );
    let mut turn = TurnLoop::new(
        Session {
            id: SessionId::new(),
            state: SessionState::default(),
            store: &store,
            evidence: &NO_ADMISSION,
            clock: &clock,
            sink: &sink,
            cancel: CancelToken::new(),
        },
        GenerationRuntime {
            source: Arc::new(GeneratorSource { generator }),
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
    .with_hook_dispatcher(HookLifecycleDispatcher::new(
        HookRunner::new_with_durable_started(port.clone(), Vec::<HookStartedEvidence>::new()),
        catalogue(),
    ));
    let outcome = block_on(turn.run("prompt")).expect("TurnLoop owner");
    (store.events(), turn.into_state())
}

#[test]
fn turn_loop_real_owner_records_prompt_and_stop_once_in_order() {
    let port = RecordingPort::default();
    let (_events, _state) = run_turn(
        vec![
            AiEvent::TextDelta {
                text: "answer".to_owned(),
            },
            AiEvent::Stop(StopReason::Stop),
        ],
        &port,
    );
    assert_eq!(
        port.calls(),
        vec![HookLifecycle::UserPromptSubmit, HookLifecycle::Stop]
    );
    assert_eq!(port.starts.lock().expect("hook starts").len(), 2);
}

#[test]
fn dispatcher_accepts_every_admitted_lifecycle_once_in_deterministic_order() {
    let port = RecordingPort::default();
    let dispatcher = HookLifecycleDispatcher::new(
        HookRunner::new_with_durable_started(port.clone(), Vec::<HookStartedEvidence>::new()),
        catalogue(),
    );
    let session = SessionId::new();
    for lifecycle in LIFECYCLES {
        block_on(dispatcher.dispatch(
            Producer::Main,
            occurrence(lifecycle, HookOccurrenceOwner::Session { session }),
        ))
        .expect("admitted lifecycle dispatch");
    }
    assert_eq!(port.calls(), LIFECYCLES);
}

#[test]
fn turn_loop_pre_block_stops_before_tool_effect_and_post_failure_observer_does_not_revert() {
    let blocking = BlockingPort::default();
    let (_events, blocked_state) = run_turn(
        vec![
            AiEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: "failing_exec".to_owned(),
                args: "{}".to_owned(),
            },
            AiEvent::Stop(StopReason::Stop),
        ],
        &blocking,
    );
    assert!(
        blocked_state.unacknowledged_prompts().is_empty(),
        "a blocked pre-effect prompt cannot alter session state"
    );
    let failing = FailingPort::default();
    let (events, _state) = run_turn_scripts(
        vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call-2".to_owned(),
                    name: "failing_exec".to_owned(),
                    args: "{}".to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "answer after failed tool".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ],
        &failing,
    );
    assert_eq!(
        failing.calls(),
        vec![
            HookLifecycle::UserPromptSubmit,
            HookLifecycle::PreToolUse,
            HookLifecycle::PostToolUseFailure,
            HookLifecycle::PostToolBatch,
            HookLifecycle::Stop,
        ]
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolCallCompleted { ok: false, .. }))
    );
}

#[test]
fn turn_loop_real_owner_reaches_tool_batch_and_failure_boundaries_then_stops() {
    let port = RecordingPort::default();
    let (_events, _state) = run_turn_scripts(
        vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call-1".to_owned(),
                    name: "failing_exec".to_owned(),
                    args: "{}".to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ],
        &port,
    );
    let calls = port.calls();
    assert_eq!(
        calls,
        vec![
            HookLifecycle::UserPromptSubmit,
            HookLifecycle::PreToolUse,
            HookLifecycle::PostToolUseFailure,
            HookLifecycle::PostToolBatch,
            HookLifecycle::Stop,
        ]
    );
}

struct ReducerFixture {
    turn: TurnId,
    item: ItemId,
    seq: Seq,
    parent: Option<EventId>,
}

impl ReducerFixture {
    fn new() -> Self {
        Self {
            turn: TurnId::new(),
            item: ItemId::new(),
            seq: Seq::default(),
            parent: None,
        }
    }

    fn event(&mut self, kind: EventKind, producer: Producer) -> DomainEvent {
        let event = DomainEvent {
            env: Envelope {
                seq: self.seq.next(),
                id: EventId::new(),
                turn: self.turn,
                item: Some(self.item),
                parent: self.parent,
                ts: Timestamp::from_millis(1_000),
                producer,
            },
            kind,
        };
        self.parent = Some(event.env.id);
        event
    }

    fn admission(
        &mut self,
        descriptors: Vec<ego_core::event::hook::HookDescriptorEvidence>,
    ) -> DomainEvent {
        self.event(
            EventKind::HookCatalogueAdmitted {
                descriptors: ego_core::event::hook::HookCatalogueEvidence::new(descriptors)
                    .expect("descriptor evidence"),
            },
            Producer::Main,
        )
    }

    fn reached(
        &mut self,
        occurrence: HookOccurrenceKey,
        subject: ego_core::event::hook::HookSubject,
        plan: HookPlan,
    ) -> DomainEvent {
        self.event(
            EventKind::HookLifecycleReached {
                occurrence,
                subject,
                plan,
            },
            Producer::Main,
        )
    }

    fn start(
        &mut self,
        occurrence: EventId,
        hook: ego_core::event::hook::HookId,
        invocation: HookInvocationId,
        lifecycle: HookLifecycle,
    ) -> DomainEvent {
        self.event(
            EventKind::HookInvocationStarted {
                occurrence,
                hook,
                invocation,
                lifecycle,
                subject: ego_core::event::hook::HookSubject::Item { item: self.item },
                activation: HookActivation::Persistent,
            },
            Producer::Main,
        )
    }

    fn settle(
        &mut self,
        occurrence: EventId,
        hook: ego_core::event::hook::HookId,
        invocation: HookInvocationId,
    ) -> DomainEvent {
        self.event(
            EventKind::HookInvocationSettled {
                occurrence,
                hook,
                invocation,
                result: ego_core::event::hook::HookResult::Outcome {
                    outcome: HookOutcome::new(HookDecision::Continue, None),
                },
            },
            Producer::Hook(hook),
        )
    }
}

fn descriptor(ordinal: u32) -> ego_core::event::hook::HookDescriptorEvidence {
    ego_core::event::hook::HookDescriptorEvidence::new(
        ego_core::event::hook::HookSourceAuthority::Project,
        ego_core::event::hook::HookSourceLocation::parse("hooks/project.toml")
            .expect("hook source"),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .parse()
            .expect("digest"),
        ordinal,
    )
}

#[test]
fn reducer_replays_real_hook_reached_start_and_settle_events_without_rerun() {
    let mut fixture = ReducerFixture::new();
    let admitted = descriptor(0);
    let hook = admitted.id();
    let invocation = HookInvocationId::new();
    let mut state = SessionState::default();
    state.apply(&fixture.admission(vec![admitted]));

    let reached_before_start = fixture.settle(EventId::new(), hook, invocation);
    state.apply(&reached_before_start);
    assert_eq!(
        state.refusals.last().map(|refusal| refusal.reason),
        Some(RefusalReason::HookInvocationUnknown)
    );
    assert!(state.live_hooks().is_empty());

    let key = HookOccurrenceKey::new(
        HookLifecycle::PreCompact,
        HookOccurrenceOwner::Item { item: fixture.item },
    );
    let reached = fixture.reached(
        key,
        ego_core::event::hook::HookSubject::Item { item: fixture.item },
        HookPlan::new(vec![hook]).expect("single-hook plan"),
    );
    let occurrence_event = reached.env.id;
    state.apply(&reached);
    let started = fixture.start(
        occurrence_event,
        hook,
        invocation,
        HookLifecycle::PreCompact,
    );
    let started_id = started.env.id;
    state.apply(&started);
    let live = state
        .live_hooks()
        .get(&invocation)
        .expect("started invocation is live");
    assert_eq!(live.replay_disposition(), HookReplayDisposition::FailClosed);

    fixture.parent = Some(started_id);
    state.apply(&fixture.settle(occurrence_event, hook, invocation));
    assert!(state.live_hooks().is_empty());
    assert_eq!(state.hook_settlements().len(), 1);

    fixture.parent = Some(started_id);
    let duplicate = fixture.settle(occurrence_event, hook, invocation);
    state.apply(&duplicate);
    assert_eq!(
        state.refusals.last().map(|refusal| refusal.reason),
        Some(RefusalReason::HookInvocationAlreadySettled)
    );
    assert_eq!(
        state.hook_settlements().len(),
        1,
        "settled occurrence is not replayed"
    );
}

#[test]
fn reducer_keeps_partial_pre_and_observer_occurrences_uncertain_by_actual_phase() {
    let mut fixture = ReducerFixture::new();
    let pre = descriptor(0);
    let observer = descriptor(1);
    let pre_id = pre.id();
    let observer_id = observer.id();
    let pre_invocation = HookInvocationId::new();
    let observer_invocation = HookInvocationId::new();
    let mut state = SessionState::default();
    state.apply(&fixture.admission(vec![pre, observer]));
    let pre_reached = fixture.reached(
        HookOccurrenceKey::new(
            HookLifecycle::PreToolUse,
            HookOccurrenceOwner::Item { item: fixture.item },
        ),
        ego_core::event::hook::HookSubject::Item { item: fixture.item },
        HookPlan::new(vec![pre_id]).expect("pre plan"),
    );
    let pre_occurrence = pre_reached.env.id;
    state.apply(&pre_reached);
    state.apply(&fixture.start(
        pre_occurrence,
        pre_id,
        pre_invocation,
        HookLifecycle::PreToolUse,
    ));
    let observer_reached = fixture.reached(
        HookOccurrenceKey::new(
            HookLifecycle::PostToolUse,
            HookOccurrenceOwner::Item { item: fixture.item },
        ),
        ego_core::event::hook::HookSubject::Item { item: fixture.item },
        HookPlan::new(vec![observer_id]).expect("observer plan"),
    );
    let observer_occurrence = observer_reached.env.id;
    state.apply(&observer_reached);
    state.apply(&fixture.start(
        observer_occurrence,
        observer_id,
        observer_invocation,
        HookLifecycle::PostToolUse,
    ));
    assert_eq!(
        state
            .live_hooks()
            .get(&pre_invocation)
            .expect("pre live")
            .replay_disposition(),
        HookReplayDisposition::FailClosed
    );
    let observer_live = state
        .live_hooks()
        .get(&observer_invocation)
        .expect("observer live");
    assert_eq!(
        observer_live.replay_disposition(),
        HookReplayDisposition::ObserverUncertain
    );
    assert_eq!(
        observer_live.delivery(),
        DeliveryCertainty::PossiblySent,
        "crash recovery cannot claim whether the external observer received the event"
    );
    assert_eq!(
        state.live_hooks().len(),
        2,
        "partial starts remain durable and independently replayable"
    );
}

#[test]
fn hook_dispatcher_rejects_hook_produced_reentry_before_owner_effect() {
    let location =
        ego_core::event::hook::HookSourceLocation::parse("hooks/project.toml").expect("location");
    let digest: ego_core::event::hook::HookContentDigest =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .parse()
            .expect("hook digest");
    let hook = ego_core::event::hook::HookId::derive(
        ego_core::event::hook::HookSourceAuthority::Project,
        &location,
        &digest,
        0,
    );
    let event = occurrence(
        HookLifecycle::UserPromptSubmit,
        HookOccurrenceOwner::Session {
            session: SessionId::new(),
        },
    );
    let port = RecordingPort::default();
    let runner =
        HookRunner::new_with_durable_started(port.clone(), Vec::<HookStartedEvidence>::new());
    let dispatcher = HookLifecycleDispatcher::new(runner, catalogue());
    let _ = block_on(dispatcher.dispatch(Producer::Hook(hook), event));
    assert!(
        port.calls().is_empty(),
        "hook-produced effects do not recursively invoke lifecycle handlers"
    );
}

#[test]
fn admitted_skill_instruction_crosses_real_hook_boundary_once_before_child_start() {
    let location = SkillSourceLocation::parse("skills/explain/SKILL.md").expect("skill location");
    let source_digest: SkillContentDigest =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .parse()
            .expect("skill digest");
    let descriptor = SkillDescriptorEvidence {
        id: SkillId::derive(
            SkillSourceAuthority::User,
            &location,
            "explain",
            &source_digest,
        ),
        name: "explain".to_owned(),
        description: "Explain a request".to_owned(),
        trigger: None,
        argument_hint: None,
        source: SkillSourceAuthority::User,
        location,
        source_digest,
        user_invocable: true,
        model_invocable: true,
    };
    let spec = SkillSpec::new(descriptor, b"Explain {{arguments}}".to_vec())
        .expect("admitted skill")
        .with_model(ModelRef("ordinary-model".to_owned()), "balanced")
        .with_hook_entries(vec![ego_core::hook::HookConfigEntry::new(
            HookLifecycle::InstructionsLoaded,
            None,
            ego_core::hook::HookHandlerConfig::Prompt {
                prompt: "continue".to_owned(),
                model: None,
                timeout_seconds: None,
            },
        )]);
    let invocation = SkillInvocation::admit_for_service(
        spec.clone(),
        SkillOperation::Invoke {
            name: "explain".to_owned(),
            arguments: "question".to_owned(),
        },
    )
    .expect("durable skill invocation");
    assert!(
        invocation.rendered_ref().is_some(),
        "skill admission publishes rendered instruction evidence"
    );
    let source = ego_core::hook::HookSource::skill_entries_with_authority(
        spec.descriptor().location.as_str(),
        invocation.id(),
        spec.descriptor().source,
        spec.hooks().to_vec(),
    );
    let catalogue =
        HookCatalogue::admit_typed(&[], &[source], Trust::Trusted).expect("skill hook catalogue");
    let port = RecordingPort::default();
    let runner =
        HookRunner::new_with_durable_started(port.clone(), Vec::<HookStartedEvidence>::new());
    let occurrence = HookOccurrence::new(
        ego_core::event::hook::HookOccurrenceKey::new(
            HookLifecycle::InstructionsLoaded,
            ego_core::event::hook::HookOccurrenceOwner::Session {
                session: SessionId::new(),
            },
        ),
        HookEvent::session(HookLifecycle::InstructionsLoaded),
    )
    .expect("instruction occurrence");
    block_on(runner.run_occurrence(&catalogue, Producer::Main, occurrence))
        .expect("instruction hook boundary");
    assert_eq!(port.calls(), vec![HookLifecycle::InstructionsLoaded]);
}
