//! RED contract for lifecycle production at the real core owners (M4 S7).

mod common;

use std::ffi::OsString;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use ego_core::budget::{BudgetGate, Metering, SharedBudgetGate};
use ego_core::config::{ConfigLayer, Mode, ModelRef, PermCfg, Ruleset, Source, Trust};
use ego_core::cost::MeteringRevision;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::hook::{
    HookActivation, HookCatalogueEntryEvidence, HookCatalogueEvidence, HookDecision,
    HookDescriptorEvidence, HookFailure, HookFailureKind, HookInvocationId, HookLifecycle,
    HookOccurrenceKey, HookOccurrenceOwner, HookOutcome, HookPlan, HookResult, HookSourceAuthority,
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
    HookCatalogue, HookCausality, HookEvent, HookFuture, HookInvocationBinding, HookInvocationPort,
    HookInvocationRequest, HookJournalPort, HookLifecycleDispatcher, HookOccurrence,
    HookReachError, HookReachOutcome, HookReachedEvidence, HookRunError,
    HookRunner as CoreHookRunner, HookSource, HookSpec, HookStartError, HookStartedEvidence,
    SessionHookState,
};
use ego_core::mcp::McpRuntimeBinding;
use ego_core::mcp::ToolExecution;
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, DnsResolution, DuplexHandle, DuplexSpawner, EnvPolicy, GuardedIo, OneHop,
    OneHopResponse, Op, PermissionDecision, PolicyEngine, PrepareCtx, PreparedCall, ProcessError,
    ProcessExecutor, Resolver, SpawnSpec, Tool, ToolDescriptor, ToolError, ToolResult,
    TransportError, TransportFailure,
};
use ego_core::permit::{HumanApprover, PermissionRequest};
use ego_core::sink::CollectingSink;
use ego_core::skill::{SkillInvocation, SkillOperation, SkillSpec};
use ego_core::state::{HookReplayDisposition, RefusalReason, SessionState};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::{
    CancelToken, GenerationPolicyCapture, GenerationPolicySnapshot, GenerationPolicySource,
    GenerationRuntime, Session, SharedJournal, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
};
use futures::channel::oneshot;
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

fn catalogue_evidence(catalogue: &HookCatalogue) -> HookCatalogueEvidence {
    let descriptors = catalogue.specs().iter().map(|spec| {
        let authority = match spec.source() {
            HookSource::System => HookSourceAuthority::System,
            HookSource::User => HookSourceAuthority::User,
            HookSource::Project => HookSourceAuthority::Project,
            HookSource::Skill { authority, .. } => match authority {
                SkillSourceAuthority::System => HookSourceAuthority::System,
                SkillSourceAuthority::User => HookSourceAuthority::User,
                SkillSourceAuthority::Project => HookSourceAuthority::Project,
            },
        };
        HookDescriptorEvidence::new(
            authority,
            spec.location().clone(),
            spec.content_digest(),
            spec.ordinal().expect("hook ordinal"),
        )
    });
    HookCatalogueEvidence::new(
        descriptors
            .map(|descriptor| {
                HookCatalogueEntryEvidence::new(descriptor, HookActivation::Persistent)
            })
            .collect(),
    )
    .expect("catalogue evidence")
}

fn occurrence(lifecycle: HookLifecycle, session: SessionId) -> HookOccurrence {
    let turn = TurnId::new();
    HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Session { session }),
        HookEvent::session(lifecycle),
        HookCausality::session(session, turn),
    )
    .expect("matching lifecycle occurrence")
}

#[derive(Clone)]
struct HookRunner<P> {
    inner: CoreHookRunner,
    port: Arc<P>,
}

impl<P> HookRunner<P>
where
    P: HookJournalPort + HookInvocationPort,
{
    fn new(port: P) -> Self {
        Self {
            inner: CoreHookRunner::new(),
            port: Arc::new(port),
        }
    }

    fn new_with_durable_started<I>(port: P, started: I) -> Self
    where
        I: IntoIterator<Item = HookStartedEvidence>,
    {
        Self {
            inner: CoreHookRunner::new_with_durable_started(started),
            port: Arc::new(port),
        }
    }

    async fn run_occurrence(
        &self,
        catalogue: &HookCatalogue,
        producer: Producer,
        occurrence: HookOccurrence,
    ) -> Result<ego_core::hook::HookRunResult, HookRunError> {
        self.inner
            .run_occurrence(
                self.port.as_ref(),
                self.port.as_ref(),
                CancelToken::new(),
                Instant::now() + Duration::from_secs(30),
                catalogue,
                producer,
                occurrence,
            )
            .await
    }
}

#[derive(Clone, Default)]
struct RecordingPort {
    starts: Arc<Mutex<Vec<HookStartedEvidence>>>,
    calls: Arc<Mutex<Vec<HookLifecycle>>>,
    requests: Arc<Mutex<Vec<HookInvocationRequest>>>,
}

#[derive(Clone, Default)]
struct BlockingPort(RecordingPort);

#[derive(Clone, Default)]
struct FailingPort(RecordingPort);

impl RecordingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.calls.lock().expect("hook calls").clone()
    }

    fn requests(&self) -> Vec<HookInvocationRequest> {
        self.requests.lock().expect("hook requests").clone()
    }
}

impl FailingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.0.calls()
    }
}

impl BlockingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.0.calls()
    }
}

macro_rules! delegate_hook_journal_port {
    ($name:ty) => {
        impl HookJournalPort for $name {
            fn reached(
                &self,
                producer: &Producer,
                occurrence: &HookOccurrence,
                plan: &HookPlan,
            ) -> Result<HookReachOutcome, HookReachError> {
                self.0.reached(producer, occurrence, plan)
            }
            fn started(
                &self,
                spec: &HookSpec,
                occurrence: &HookOccurrence,
                reached: &HookReachedEvidence,
                activation: HookActivation,
                invocation: HookInvocationId,
            ) -> Result<HookStartedEvidence, HookStartError> {
                self.0
                    .started(spec, occurrence, reached, activation, invocation)
            }
            fn settled(
                &self,
                occurrence: &HookOccurrence,
                started: HookStartedEvidence,
                result: &HookResult,
            ) -> Result<EventId, HookRunError> {
                self.0.settled(occurrence, started, result)
            }
        }
    };
}

delegate_hook_journal_port!(BlockingPort);

impl HookInvocationPort for BlockingPort {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        self.0
            .calls
            .lock()
            .expect("hook calls")
            .push(request.lifecycle());
        let decision = if request.lifecycle() == HookLifecycle::PreToolUse {
            HookDecision::Block
        } else {
            HookDecision::Continue
        };
        Box::pin(std::future::ready(Ok(HookOutcome::new(decision, None))))
    }
}

delegate_hook_journal_port!(FailingPort);

impl HookInvocationPort for FailingPort {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        self.0
            .calls
            .lock()
            .expect("hook calls")
            .push(request.lifecycle());
        if request.lifecycle() == HookLifecycle::PostToolUseFailure {
            Box::pin(std::future::ready(Err(HookFailure::new(
                HookFailureKind::HandlerFailed,
                None,
            ))))
        } else {
            Box::pin(std::future::ready(Ok(HookOutcome::new(
                HookDecision::Continue,
                None,
            ))))
        }
    }
}

impl HookJournalPort for RecordingPort {
    fn reached(
        &self,
        producer: &Producer,
        occurrence: &HookOccurrence,
        plan: &HookPlan,
    ) -> Result<HookReachOutcome, HookReachError> {
        let evidence = HookReachedEvidence::new(
            EventId::new(),
            occurrence.key().clone(),
            occurrence.event().subject(),
            producer.clone(),
            plan.clone(),
        );
        Ok(HookReachOutcome::Fresh(evidence))
    }
    fn started(
        &self,
        spec: &HookSpec,
        _occurrence: &HookOccurrence,
        reached: &HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> Result<HookStartedEvidence, HookStartError> {
        let evidence = HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        );
        self.starts.lock().expect("hook starts").push(evidence);
        Ok(evidence)
    }

    fn settled(
        &self,
        _occurrence: &HookOccurrence,
        _started: HookStartedEvidence,
        _result: &HookResult,
    ) -> Result<EventId, HookRunError> {
        Ok(EventId::new())
    }
}

impl HookInvocationPort for RecordingPort {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        self.calls
            .lock()
            .expect("hook calls")
            .push(request.lifecycle());
        self.requests.lock().expect("hook requests").push(request);
        Box::pin(std::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
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

#[derive(Clone, Default)]
struct NoProcess {
    calls: Arc<AtomicUsize>,
}

impl NoProcess {
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Err(ProcessError::Invariant(
            "unused process".to_owned(),
        ))))
    }
}

#[derive(Debug)]
struct DenyApprover;

impl HumanApprover for DenyApprover {
    fn decide<'a>(&'a self, _request: &'a PermissionRequest) -> BoxFuture<'a, PermissionDecision> {
        Box::pin(std::future::ready(PermissionDecision::Deny))
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
            Op::Spawn { spec },
        ))
    }

    fn execute<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let Some(Op::Spawn { spec }) = call.into_parts().1.into_iter().next() else {
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

fn run_turn<P: HookInvocationPort + Clone + 'static>(
    script: Vec<AiEvent>,
    port: &P,
) -> (Vec<DomainEvent>, SessionState, usize) {
    run_turn_scripts(vec![script], port)
}

struct TestHookBinding<P>(P);

struct FixedPolicySource {
    policy: Arc<PolicyEngine>,
    snapshot: Arc<GenerationPolicySnapshot>,
    human: Option<Arc<dyn HumanApprover>>,
}

impl GenerationPolicySource for FixedPolicySource {
    fn current(&self) -> Result<Arc<PolicyEngine>, String> {
        Ok(Arc::clone(&self.policy))
    }

    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(Arc::clone(&self.snapshot), self.human.clone())
            .map_err(|error| error.to_string())
    }

    fn restore(
        &self,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(snapshot, self.human.clone())
            .map_err(|error| error.to_string())
    }
}

impl<P: HookInvocationPort + Clone + 'static> HookInvocationBinding for TestHookBinding<P> {
    fn bind<'a>(
        &self,
        _policy: Arc<PolicyEngine>,
        _mcp: Option<&dyn McpRuntimeBinding>,
    ) -> Result<Arc<dyn HookInvocationPort + 'a>, String>
    where
        Self: 'a,
    {
        Ok(Arc::new(self.0.clone()))
    }
}

fn run_turn_scripts<P: HookInvocationPort + Clone + 'static>(
    scripts: Vec<Vec<AiEvent>>,
    port: &P,
) -> (Vec<DomainEvent>, SessionState, usize) {
    run_turn_scripts_with_policy(scripts, port, false)
}

fn run_turn_scripts_with_policy<P: HookInvocationPort + Clone + 'static>(
    scripts: Vec<Vec<AiEvent>>,
    port: &P,
    deny_exec: bool,
) -> (Vec<DomainEvent>, SessionState, usize) {
    let store = Store::default();
    let clock = FixedClock;
    let sink = CollectingSink::new();
    let generator = Arc::new(FakeGenerator::new(scripts));
    let gate = SharedBudgetGate::new(BudgetGate::new(ego_core::cost::Money::new(
        ego_core::cost::Amount::new(1000, 0).expect("amount"),
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    )));
    let permissions = Ruleset::from_layer(
        PermCfg {
            judge: None,
            allow: if deny_exec {
                Vec::new()
            } else {
                vec![Rule::load("exec(*)").expect("exec rule")]
            },
            deny: Vec::new(),
        },
        Source::Project,
    );
    let human: Option<Arc<dyn HumanApprover>> = if deny_exec {
        Some(Arc::new(DenyApprover))
    } else {
        None
    };
    let policy = GenerationPolicyCapture::new(
        CanonPath::new("/ws"),
        Mode::Default,
        permissions,
        human.clone(),
    )
    .expect("valid generation policy capture");
    let process = NoProcess::default();
    let evidence = NoEvidence;
    let fs = ego_core::permit::fake::FakeFs::default();
    let registry: [Arc<dyn Tool>; 1] = [Arc::new(FailingExecTool)];
    let tools = Tools::new(
        &registry,
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
    let session = SessionId::new();
    let cancel = CancelToken::new();
    let mut turn = TurnLoop::new(
        Session {
            id: session,
            state: SessionState::default(),
            store: &store,
            evidence: &NO_ADMISSION,
            clock: &clock,
            sink: &sink,
            cancel: cancel.clone(),
        },
        GenerationRuntime {
            policy_source: Arc::new(FixedPolicySource {
                policy: policy.policy(),
                snapshot: policy.snapshot(),
                human,
            }),
            source: Arc::new(GeneratorSource { generator }),
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
    .expect("synchronized turn loop");
    let catalogue = catalogue();
    turn.journal()
        .append(
            TurnId::new(),
            None,
            None,
            EventKind::HookCatalogueAdmitted {
                descriptors: catalogue_evidence(&catalogue),
            },
        )
        .expect("hook catalogue admission");
    let hook_state = SessionHookState::new(
        HookLifecycleDispatcher::new(CoreHookRunner::new(), catalogue),
        turn.journal(),
        cancel.clone(),
        Instant::now() + Duration::from_secs(30),
    );
    turn = turn
        .with_hook_binding(hook_state, Arc::new(TestHookBinding(port.clone())), None)
        .expect("shared turn hook runtime");
    let _outcome = block_on(turn.run("prompt")).expect("TurnLoop owner");
    (
        store.events(),
        turn.into_state().expect("unique turn journal state"),
        process.call_count(),
    )
}

#[test]
fn turn_loop_real_owner_records_prompt_and_stop_once_in_order() {
    let port = RecordingPort::default();
    let (_events, _state, _process_calls) = run_turn(
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
    assert_eq!(port.requests().len(), 2);
}

#[test]
fn session_hook_state_binds_distinct_invokers_over_one_shared_catalogue_and_journal() {
    let store = Store::default();
    let clock = FixedClock;
    let sink = CollectingSink::new();
    let session = SessionId::new();
    let journal = SharedJournal::new(ego_core::turn::Journal::new(
        session,
        &store,
        &clock,
        &sink,
        Producer::Main,
        SessionState::default(),
    ));
    let state = SessionHookState::new(
        HookLifecycleDispatcher::new(CoreHookRunner::new(), catalogue()),
        journal,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(30),
    );
    let a = RecordingPort::default();
    let b = RecordingPort::default();
    let binding_a = state.bind(Arc::new(a.clone()));
    let binding_b = state.bind(Arc::new(b.clone()));
    let retained_a = binding_a.clone();

    block_on(binding_a.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::UserPromptSubmit, session),
    ))
    .expect("binding A dispatch");
    block_on(binding_b.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::UserPromptSubmit, session),
    ))
    .expect("binding B dispatch");
    block_on(retained_a.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::UserPromptSubmit, session),
    ))
    .expect("retained A dispatch");

    assert_eq!(a.calls(), [HookLifecycle::UserPromptSubmit; 2]);
    assert_eq!(b.calls(), [HookLifecycle::UserPromptSubmit]);
    assert_eq!(
        binding_a.hook_state().catalogue_evidence(),
        binding_b.hook_state().catalogue_evidence()
    );
    assert_eq!(
        retained_a.hook_state().catalogue_evidence(),
        binding_a.hook_state().catalogue_evidence()
    );
    assert_eq!(
        store
            .events()
            .iter()
            .filter(|event| matches!(event.kind, EventKind::HookCatalogueAdmitted { .. }))
            .count(),
        0,
        "binding does not re-admit or recreate SessionStart state"
    );
}

#[test]
fn dispatcher_accepts_every_admitted_lifecycle_once_in_deterministic_order() {
    let port = RecordingPort::default();
    let dispatcher = HookLifecycleDispatcher::new(
        CoreHookRunner::new_with_durable_started(Vec::<HookStartedEvidence>::new()),
        catalogue(),
    );
    let session = SessionId::new();
    for lifecycle in LIFECYCLES {
        block_on(dispatcher.dispatch(
            &port,
            &port,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(30),
            Producer::Main,
            occurrence(lifecycle, session),
        ))
        .expect("admitted lifecycle dispatch");
    }
    assert_eq!(port.calls(), LIFECYCLES);
}

#[test]
fn turn_loop_pre_block_stops_before_tool_effect_and_post_failure_observer_does_not_revert() {
    let blocking = BlockingPort::default();
    let (blocked_events, blocked_state, process_calls) = run_turn(
        vec![
            AiEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: "failing_exec".to_owned(),
                args: "{}".to_owned(),
            },
            AiEvent::Stop(StopReason::ToolUse),
        ],
        &blocking,
    );
    assert!(
        blocked_state.unacknowledged_prompts().is_empty(),
        "a blocked pre-effect prompt cannot alter session state"
    );
    assert_eq!(
        blocking.calls(),
        vec![
            HookLifecycle::UserPromptSubmit,
            HookLifecycle::PreToolUse,
            HookLifecycle::PostToolBatch,
            HookLifecycle::StopFailure,
        ],
        "the earlier lifecycle must continue so the tool is blocked at PreToolUse"
    );
    assert!(
        !blocking.calls().iter().any(|lifecycle| matches!(
            lifecycle,
            HookLifecycle::PermissionRequest
                | HookLifecycle::PermissionDenied
                | HookLifecycle::PostToolUse
                | HookLifecycle::PostToolUseFailure
        )),
        "a blocked tool must not reach permission or post-tool effect lifecycles"
    );
    assert_eq!(
        process_calls, 0,
        "a PreToolUse block must not cross the prepared tool's process port"
    );
    assert!(
        blocked_events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolCallCompleted { ok: false, .. })),
        "the PreToolUse block must produce the durable blocked result"
    );
    let failing = FailingPort::default();
    let (events, _state, _process_calls) = run_turn_scripts(
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
    let (_events, _state, _process_calls) = run_turn_scripts(
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

#[test]
fn turn_loop_permission_denial_reaches_request_and_denied_hooks_once_before_exit() {
    let port = RecordingPort::default();
    let (_events, _state, _process_calls) = run_turn_scripts_with_policy(
        vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call-denied".to_owned(),
                    name: "failing_exec".to_owned(),
                    args: "{}".to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ],
        &port,
        true,
    );
    assert_eq!(
        port.calls(),
        vec![
            HookLifecycle::UserPromptSubmit,
            HookLifecycle::PreToolUse,
            HookLifecycle::PermissionRequest,
            HookLifecycle::PermissionDenied,
        ],
        "a settled policy denial has one request/denied owner path and exits before batch/stop",
    );
}

#[test]
fn runner_passes_one_complete_request_bound_to_the_durable_start() {
    let catalogue = catalogue();
    let spec = catalogue
        .specs()
        .first()
        .expect("one admitted hook")
        .clone();
    let event = HookEvent::session(HookLifecycle::SessionStart);
    let session = SessionId::new();
    let turn = TurnId::new();
    let occurrence = HookOccurrence::new(
        HookOccurrenceKey::new(event.lifecycle(), HookOccurrenceOwner::Session { session }),
        event.clone(),
        HookCausality::session(session, turn),
    )
    .expect("matching occurrence");
    let port = RecordingPort::default();
    let dispatcher = HookLifecycleDispatcher::new(
        CoreHookRunner::new_with_durable_started(Vec::<HookStartedEvidence>::new()),
        catalogue,
    );
    block_on(dispatcher.dispatch(
        &port,
        &port,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(30),
        Producer::Main,
        occurrence.clone(),
    ))
    .expect("hook dispatch");

    let started = port.starts.lock().expect("hook starts")[0];
    let request = port.requests().pop().expect("one invocation request");
    assert_eq!(request.hook(), started.hook());
    assert_eq!(request.invocation(), started.invocation());
    assert_eq!(request.activation(), started.activation());
    assert_eq!(request.lifecycle(), occurrence.event().lifecycle());
    assert_eq!(request.subject(), occurrence.event().subject());
    assert_eq!(request.handler(), spec.handler());
    assert_eq!(
        request.effect_scope().timeout(),
        std::time::Duration::from_secs(30)
    );
}

#[derive(Clone, Default)]
struct PendingInvocationPort {
    started: Arc<Mutex<Vec<HookStartedEvidence>>>,
    releases: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
}

impl PendingInvocationPort {
    fn release_pending(&self) {
        let releases = std::mem::take(&mut *self.releases.lock().expect("hook releases"));
        for release in releases {
            let _ = release.send(());
        }
    }
}

impl HookJournalPort for PendingInvocationPort {
    fn reached(
        &self,
        producer: &Producer,
        occurrence: &HookOccurrence,
        plan: &HookPlan,
    ) -> Result<HookReachOutcome, HookReachError> {
        Ok(HookReachOutcome::Fresh(HookReachedEvidence::new(
            EventId::new(),
            occurrence.key().clone(),
            occurrence.event().subject(),
            producer.clone(),
            plan.clone(),
        )))
    }

    fn started(
        &self,
        spec: &HookSpec,
        _occurrence: &HookOccurrence,
        reached: &HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> Result<HookStartedEvidence, HookStartError> {
        let evidence = HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        );
        self.started.lock().expect("hook starts").push(evidence);
        Ok(evidence)
    }

    fn settled(
        &self,
        _occurrence: &HookOccurrence,
        _started: HookStartedEvidence,
        _result: &HookResult,
    ) -> Result<EventId, HookRunError> {
        Ok(EventId::new())
    }
}

impl HookInvocationPort for PendingInvocationPort {
    fn invoke<'a>(&'a self, _request: HookInvocationRequest) -> HookFuture<'a> {
        let (sender, receiver) = oneshot::channel();
        self.releases.lock().expect("hook releases").push(sender);
        Box::pin(async move {
            receiver
                .await
                .map_err(|_| HookFailure::new(HookFailureKind::HandlerFailed, None))?;
            Ok(HookOutcome::new(HookDecision::Continue, None))
        })
    }
}

#[test]
fn concurrent_occurrences_release_admission_before_external_handler_await() {
    use futures::task::LocalSpawnExt;

    let port = PendingInvocationPort::default();
    let runner = HookRunner::new(port.clone());
    let shared_catalogue = Arc::new(catalogue());
    let first = occurrence(HookLifecycle::SessionStart, SessionId::new());
    let second = occurrence(HookLifecycle::SessionStart, SessionId::new());
    let mut pool = futures::executor::LocalPool::new();
    let first_runner = runner.clone();
    let first_catalogue = Arc::clone(&shared_catalogue);
    pool.spawner()
        .spawn_local(async move {
            first_runner
                .run_occurrence(&first_catalogue, Producer::Main, first)
                .await
                .expect("first occurrence");
        })
        .expect("first occurrence task");
    let second_catalogue = Arc::clone(&shared_catalogue);
    pool.spawner()
        .spawn_local(async move {
            runner
                .run_occurrence(&second_catalogue, Producer::Main, second)
                .await
                .expect("second occurrence");
        })
        .expect("second occurrence task");
    pool.run_until_stalled();
    assert_eq!(
        port.started.lock().expect("hook starts").len(),
        2,
        "the journal/admission lock must not span external handler await"
    );
    port.release_pending();
    pool.run_until_stalled();
    port.release_pending();
    pool.run_until_stalled();
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
                descriptors: ego_core::event::hook::HookCatalogueEvidence::new(
                    descriptors
                        .into_iter()
                        .map(|descriptor| {
                            ego_core::event::hook::HookCatalogueEntryEvidence::new(
                                descriptor,
                                HookActivation::Persistent,
                            )
                        })
                        .collect(),
                )
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
        HookPlan::new(vec![ego_core::event::hook::HookPlanEntry::new(
            hook,
            HookActivation::Persistent,
        )])
        .expect("single-hook plan"),
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
        HookPlan::new(vec![ego_core::event::hook::HookPlanEntry::new(
            pre_id,
            HookActivation::Persistent,
        )])
        .expect("pre plan"),
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
        HookPlan::new(vec![ego_core::event::hook::HookPlanEntry::new(
            observer_id,
            HookActivation::Persistent,
        )])
        .expect("observer plan"),
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
fn every_accepted_lifecycle_replays_uncertainty_from_its_declared_phase() {
    let mut fixture = ReducerFixture::new();
    let admitted = descriptor(0);
    let hook = admitted.id();
    let mut state = SessionState::default();
    state.apply(&fixture.admission(vec![admitted]));

    let mut dispositions = Vec::new();
    for lifecycle in LIFECYCLES {
        let reached = fixture.reached(
            HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Item { item: fixture.item }),
            ego_core::event::hook::HookSubject::Item { item: fixture.item },
            HookPlan::new(vec![ego_core::event::hook::HookPlanEntry::new(
                hook,
                HookActivation::Persistent,
            )])
            .expect("single-hook plan"),
        );
        let occurrence = reached.env.id;
        state.apply(&reached);
        let started = fixture.start(occurrence, hook, HookInvocationId::new(), lifecycle);
        let invocation = match &started.kind {
            EventKind::HookInvocationStarted { invocation, .. } => *invocation,
            _ => unreachable!("fixture creates a hook start"),
        };
        state.apply(&started);
        let live = state
            .live_hooks()
            .get(&invocation)
            .expect("every reached lifecycle starts one hook");
        dispositions.push((lifecycle, live.replay_disposition(), live.delivery()));
    }

    assert_eq!(dispositions.len(), LIFECYCLES.len());
    for (lifecycle, disposition, delivery) in dispositions {
        match lifecycle.phase() {
            ego_core::event::hook::HookPhase::PreEffect => {
                assert_eq!(
                    disposition,
                    HookReplayDisposition::FailClosed,
                    "pre-effect {lifecycle:?} must fail closed after a crash",
                );
            }
            ego_core::event::hook::HookPhase::Observer => {
                assert_eq!(
                    disposition,
                    HookReplayDisposition::ObserverUncertain,
                    "observer {lifecycle:?} must remain truthful after a crash",
                );
                assert_eq!(delivery, DeliveryCertainty::PossiblySent);
            }
        }
    }
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
    let event = occurrence(HookLifecycle::UserPromptSubmit, SessionId::new());
    let port = RecordingPort::default();
    let dispatcher = HookLifecycleDispatcher::new(
        CoreHookRunner::new_with_durable_started(Vec::<HookStartedEvidence>::new()),
        catalogue(),
    );
    let result = block_on(dispatcher.dispatch(
        &port,
        &port,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(30),
        Producer::Hook(hook),
        event,
    ))
    .expect("hook-produced effects are rejected as a no-op, not rerun");
    assert_eq!(result.decision(), HookDecision::Continue);
    assert!(
        !result.is_durable(),
        "reentry must not create hook evidence"
    );
    assert!(
        !result.started_invocation(),
        "reentry must not start a nested invocation"
    );
    assert!(result.closure_event().is_none());
    assert!(
        port.calls().is_empty(),
        "reentry never reaches the invocation port"
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
    let session = SessionId::new();
    let occurrence = HookOccurrence::new(
        ego_core::event::hook::HookOccurrenceKey::new(
            HookLifecycle::InstructionsLoaded,
            ego_core::event::hook::HookOccurrenceOwner::Session { session },
        ),
        HookEvent::session(HookLifecycle::InstructionsLoaded),
        HookCausality::session(session, TurnId::new()),
    )
    .expect("instruction occurrence");
    block_on(runner.run_occurrence(&catalogue, Producer::Main, occurrence))
        .expect("instruction hook boundary");
    assert_eq!(port.calls(), vec![HookLifecycle::InstructionsLoaded]);
}
