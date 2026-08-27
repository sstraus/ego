//! The turn loop: what a turn writes down, and what it does when something
//! goes wrong.
//!
//! See `docs/decisions/2026-08-21-the-turn-loop.md` and
//! `plans/m0b-vertical-slice.md` Step 10.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsString;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use ego_core::budget::{BudgetGate, Metering, PricingQuote, ResolvedRates, SettlementSource};
use ego_core::config::{Mode, PermCfg, Ruleset, Source, SystemPrompt};
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money, PricingRevision,
    ProviderCostRevision, RateSource, UnknownCost,
};
use ego_core::event::domain::{CancelCause, DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, Producer, QueuedPromptId, Seq, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::attempt::{AttemptStream, GenerationAttempt};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::history::conversation_from;
use ego_core::generator::port::{AiEvent, Generator, RequestExpressibilityError};
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, ReasoningState, Role, StopReason, ThinkingLevel,
    ToolDescriptor,
};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    Authority, CanonPath, Confinement, EnvPolicy, ExecCaps, Fs, GuardedIo, HumanApprover, OneHop,
    OneHopResponse, Op, PermissionDecision, PermissionRequest, PolicyEngine, PrepareCtx,
    PreparedCall, ProcessError, ProcessExecutor, ResolvedConnect, Resolver, RunConfinement,
    SpawnSpec, ToolError, ToolResult, TransportError, Typed, TypedTool,
};
use ego_core::public::PublicEvent;
use ego_core::run::RunTermination;
use ego_core::run::{RunAppend, RunRevision, RunTransaction};
use ego_core::sink::{CollectingSink, EventSink};
use ego_core::state::{ItemKind, SessionState, Usage};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::{
    CancelToken, ControlError, GenerationRuntime, JudgeContext, Session, ToolEffectPorts, Tools,
    TurnConfig, TurnLoop,
};
use futures::StreamExt;
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

mod common;

mod turn {
    use super::*;
    use ego_core::permit::fake::{FakeFs, FakeWriteTool};

    struct NoResolver;

    impl Resolver for NoResolver {
        fn resolve(
            &self,
            _target: ego_core::permit::NetworkTarget,
            _deadline: Option<std::time::Instant>,
        ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, TransportError>> {
            panic!("turn test unexpectedly resolved an HTTP target")
        }
    }

    struct NoHop;

    impl OneHop for NoHop {
        fn execute(
            &self,
            _connect: ResolvedConnect,
            _request: ego_core::permit::HttpRequest,
            _deadline: Option<std::time::Instant>,
        ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
            panic!("turn test unexpectedly executed an HTTP hop")
        }
    }

    struct NoDuplex;

    impl ego_core::permit::DuplexSpawner for NoDuplex {
        fn spawn(
            &self,
            _spec: ego_core::permit::SpawnSpec,
        ) -> BoxFuture<'static, Result<ego_core::permit::DuplexHandle, TransportError>> {
            panic!("turn test unexpectedly spawned a duplex")
        }
    }

    static NO_RESOLVER: NoResolver = NoResolver;
    static NO_HOP: NoHop = NoHop;
    static NO_DUPLEX: NoDuplex = NoDuplex;

    /// A `SessionStore` that keeps the log in memory, and can be told to fail
    /// on the nth append — which is the only way to test what the loop does
    /// when the log cannot be written.
    #[derive(Default)]
    struct MemStore {
        events: Mutex<Vec<DomainEvent>>,
        appends: AtomicUsize,
        /// Fail the append with this 1-based index. `0` never fails.
        fail_at: AtomicUsize,
        fail_prompt_queued: AtomicBool,
        permission_requested: Arc<AtomicBool>,
    }

    impl MemStore {
        fn fail_at(&self, nth: usize) {
            self.fail_at.store(nth, Ordering::SeqCst);
        }

        fn fail_prompt_queued(&self) {
            self.fail_prompt_queued.store(true, Ordering::SeqCst);
        }

        fn kinds(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|ev| ev.kind.name().to_string())
                .collect()
        }

        fn events(&self) -> Vec<DomainEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl SessionStore for MemStore {
        fn append(&self, _id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
            let nth = self.appends.fetch_add(1, Ordering::SeqCst) + 1;
            if nth == self.fail_at.load(Ordering::SeqCst)
                || (self.fail_prompt_queued.load(Ordering::SeqCst)
                    && matches!(ev.kind, EventKind::PromptQueued { .. }))
            {
                return Err(StoreError::Io(std::io::Error::other("the disk said no")));
            }
            self.events.lock().unwrap().push(ev.clone());
            if matches!(ev.kind, EventKind::PermissionRequested { .. }) {
                self.permission_requested.store(true, Ordering::SeqCst);
            }
            Ok(())
        }

        fn compare_and_append_run(
            &self,
            _session: &SessionId,
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
            _id: &SessionId,
        ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
            unimplemented!("the turn loop only appends")
        }

        fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
            unimplemented!("the turn loop only appends")
        }
    }

    /// A clock that does not move. Envelope timestamps are observation time,
    /// not ordering (`event/time.rs`), so a turn that runs in an instant is a
    /// perfectly valid thing to log.
    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(self.0)
        }
    }

    /// A generator that cancels the turn part-way through its own stream.
    ///
    /// [`FakeGenerator`] covers every other test here — it is the scripted
    /// `Generator` this crate already ships. What it cannot express is a
    /// cancellation arriving *during* a generation, which is the one case
    /// where settling the attempt is hard, so that case gets a fake of its
    /// own rather than a hook bolted onto the general one.
    struct CancellingGenerator {
        events: Mutex<Vec<AiEvent>>,
        /// Cancel once the stream has yielded this many events.
        after: usize,
        token: CancelToken,
    }

    /// A human seat whose answers are explicit and finite. Exhausting the
    /// script is a test defect: production EOF is represented by an explicit
    /// fail-closed `Deny`, never by the approver inventing an answer.
    #[derive(Debug)]
    struct ScriptedHuman {
        answers: Mutex<VecDeque<PermissionDecision>>,
    }

    #[derive(Debug)]
    struct RequestObservingHuman {
        permission_requested: Arc<AtomicBool>,
    }

    impl HumanApprover for RequestObservingHuman {
        fn decide<'a>(
            &'a self,
            _request: &'a PermissionRequest,
        ) -> BoxFuture<'a, PermissionDecision> {
            assert!(
                self.permission_requested.load(Ordering::SeqCst),
                "PermissionRequested must be durable before the human is called"
            );
            Box::pin(futures::future::ready(PermissionDecision::Allow))
        }
    }

    #[derive(Debug)]
    struct PendingHuman;

    impl HumanApprover for PendingHuman {
        fn decide<'a>(
            &'a self,
            _request: &'a PermissionRequest,
        ) -> BoxFuture<'a, PermissionDecision> {
            Box::pin(futures::future::pending())
        }
    }

    impl ScriptedHuman {
        fn new(answers: impl IntoIterator<Item = PermissionDecision>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().collect()),
            }
        }
    }

    impl HumanApprover for ScriptedHuman {
        fn decide<'a>(
            &'a self,
            _request: &'a PermissionRequest,
        ) -> BoxFuture<'a, PermissionDecision> {
            let answer = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("the test supplied one human answer per question");
            Box::pin(futures::future::ready(answer))
        }
    }

    impl Generator for CancellingGenerator {
        fn ensure_expressible(&self, _req: &GenRequest) -> Result<(), RequestExpressibilityError> {
            Ok(())
        }

        fn generate(&self, _req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
            let events = std::mem::take(&mut *self.events.lock().unwrap());
            let after = self.after;
            let token = self.token.clone();
            let stream =
                futures::stream::iter(events.into_iter().enumerate()).map(move |(at, event)| {
                    if at + 1 == after {
                        token.cancel(CancelCause::User);
                    }
                    event
                });
            AttemptStream::new(attempt, Box::pin(stream))
        }
    }

    /// A generator that says something and then never finishes.
    ///
    /// The only way to prove *live* rather than *eventually*: a batch handed
    /// over at turn end and a sink fed as events happen are indistinguishable
    /// once the turn is over, so the turn must be caught while it is still
    /// running. This one yields its events and then pends forever, so a
    /// single poll of `run` provably cannot have returned.
    struct StallingGenerator {
        events: Mutex<Vec<AiEvent>>,
    }

    impl Generator for StallingGenerator {
        fn ensure_expressible(&self, _req: &GenRequest) -> Result<(), RequestExpressibilityError> {
            Ok(())
        }

        fn generate(&self, _req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
            let events = std::mem::take(&mut *self.events.lock().unwrap());
            let stream = futures::stream::iter(events).chain(futures::stream::pending());
            AttemptStream::new(attempt, Box::pin(stream))
        }
    }

    struct ContinuouslyReadyGenerator;

    impl Generator for ContinuouslyReadyGenerator {
        fn ensure_expressible(&self, _req: &GenRequest) -> Result<(), RequestExpressibilityError> {
            Ok(())
        }

        fn generate(&self, _req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
            AttemptStream::new(
                attempt,
                Box::pin(
                    futures::stream::repeat(AiEvent::TextDelta {
                        text: "ready delta".to_owned(),
                    })
                    .take(32),
                ),
            )
        }
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct ReadParams {
        path: String,
    }

    /// A read-only tool, the shape Step 11 ships for real: it resolves its
    /// path at `prepare` and reads it through `GuardedIo`, never through `Fs`.
    struct FakeReadTool;

    impl TypedTool for FakeReadTool {
        type Params = ReadParams;

        fn name(&self) -> &str {
            "read"
        }

        fn description(&self) -> &str {
            "reads a file"
        }

        fn prepare_typed(
            &self,
            params: ReadParams,
            ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            Ok(PreparedCall {
                tool: self.name().to_string(),
                ops: vec![Op::Read {
                    path: ctx.resolver.canonicalize(Path::new(&params.path))?,
                }],
            })
        }

        fn execute<'a>(
            &'a self,
            call: PreparedCall,
            io: &'a dyn GuardedIo,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
            Box::pin(async move {
                let Some(Op::Read { path }) = call.ops.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Read op".into()));
                };
                let bytes = io.read(&path)?;
                Ok(ToolResult::builtin(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            })
        }
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct ExecParams {
        argv: Vec<String>,
    }

    struct FakeExecTool;

    impl TypedTool for FakeExecTool {
        type Params = ExecParams;

        fn name(&self) -> &str {
            "exec"
        }

        fn description(&self) -> &str {
            "executes a process"
        }

        fn prepare_typed(
            &self,
            params: ExecParams,
            _ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            let spec = SpawnSpec::new(
                params.argv.into_iter().map(OsString::from).collect(),
                None,
                None,
                EnvPolicy::Inherited,
            )
            .map_err(|err| ToolError::InvalidParams(err.to_string()))?;
            Ok(PreparedCall {
                tool: self.name().to_string(),
                ops: vec![Op::Spawn { spec }],
            })
        }

        fn execute<'a>(
            &'a self,
            call: PreparedCall,
            io: &'a dyn GuardedIo,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
            Box::pin(async move {
                let Some(Op::Spawn { spec }) = call.ops.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Spawn op".into()));
                };
                io.exec(&spec).await.map_err(ToolError::from)
            })
        }
    }

    struct GatedCancellableProcess {
        calls: Arc<AtomicUsize>,
        released: Arc<AtomicBool>,
        release_seen: Arc<AtomicBool>,
    }

    impl ProcessExecutor for GatedCancellableProcess {
        fn execute<'a>(
            &'a self,
            _spec: &'a ego_core::permit::SpawnSpec,
        ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let released = Arc::clone(&self.released);
            let release_seen = Arc::clone(&self.release_seen);
            Box::pin(async move {
                futures::future::poll_fn(move |cx| {
                    if released.load(Ordering::SeqCst) && release_seen.swap(true, Ordering::SeqCst)
                    {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                Err(ProcessError::Cancelled)
            })
        }
    }

    struct GatedReadTool {
        released: Arc<AtomicBool>,
        release_seen: Arc<AtomicBool>,
    }

    impl TypedTool for GatedReadTool {
        type Params = ReadParams;

        fn name(&self) -> &str {
            "builtin_read"
        }

        fn description(&self) -> &str {
            "reads through a non-cancellable builtin"
        }

        fn prepare_typed(
            &self,
            params: ReadParams,
            ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            Ok(PreparedCall {
                tool: self.name().to_string(),
                ops: vec![Op::Read {
                    path: ctx.resolver.canonicalize(Path::new(&params.path))?,
                }],
            })
        }

        fn execute<'a>(
            &'a self,
            call: PreparedCall,
            io: &'a dyn GuardedIo,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
            let released = Arc::clone(&self.released);
            let release_seen = Arc::clone(&self.release_seen);
            Box::pin(async move {
                futures::future::poll_fn(move |cx| {
                    if released.load(Ordering::SeqCst) && release_seen.swap(true, Ordering::SeqCst)
                    {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                let Some(Op::Read { path }) = call.ops.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Read op".into()));
                };
                let bytes = io.read(&path)?;
                Ok(ToolResult::builtin(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            })
        }
    }

    struct CancellingReadTool {
        started: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        released: Arc<AtomicBool>,
        release_seen: Arc<AtomicBool>,
    }

    impl TypedTool for CancellingReadTool {
        type Params = ReadParams;

        fn name(&self) -> &str {
            "cancel_after_read"
        }

        fn description(&self) -> &str {
            "reads once and interrupts the remaining batch"
        }

        fn prepare_typed(
            &self,
            params: ReadParams,
            ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            Ok(PreparedCall {
                tool: self.name().to_string(),
                ops: vec![Op::Read {
                    path: ctx.resolver.canonicalize(Path::new(&params.path))?,
                }],
            })
        }

        fn execute<'a>(
            &'a self,
            call: PreparedCall,
            io: &'a dyn GuardedIo,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let calls = Arc::clone(&self.calls);
            let released = Arc::clone(&self.released);
            let release_seen = Arc::clone(&self.release_seen);
            Box::pin(async move {
                futures::future::poll_fn(move |cx| {
                    if released.load(Ordering::SeqCst) && release_seen.swap(true, Ordering::SeqCst)
                    {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                let Some(Op::Read { path }) = call.ops.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Read op".into()));
                };
                let bytes = io.read(&path)?;
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(ToolResult::builtin(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            })
        }
    }

    fn usd(decimal: &str) -> Money {
        Money::new(
            decimal.parse::<Amount>().expect("a valid test amount"),
            CurrencyCode::new("USD").expect("USD is a valid code"),
        )
    }

    fn quote() -> PricingQuote {
        let rate = |d: &str| Some(d.parse::<Amount>().expect("a valid test rate"));
        PricingQuote {
            revision: PricingRevision("test-2026-08".to_string()),
            currency: CurrencyCode::new("USD").expect("USD is a valid code"),
            uncached_input: rate("0.000003"),
            cache_read: rate("0.0000003"),
            cache_write: rate("0.00000375"),
            cache_write_extended: None,
            output: rate("0.000015"),
        }
    }

    /// The fixture quote as a metering: a table that both bounds and
    /// settles. Every turn test here was written against that arrangement;
    /// the other three arrangements are exercised in `tests/metering.rs`.
    fn metering() -> Metering {
        Metering::Currency {
            settlement: SettlementSource::Quote,
        }
    }

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            schema_version: 1,
            max_context_tokens: Some(200_000),
            max_output_tokens: Some(64_000),
            max_tools: Some(128),
            supports_parallel_tool_calls: true,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: ReasoningEffortSupport::Any,
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
        }
    }

    struct TestGenerationSettings {
        model: String,
        thinking: ThinkingLevel,
        max_output_tokens: Option<u64>,
        parallel_tool_calls: bool,
        system: Option<SystemPrompt>,
        reasoning_efforts: ReasoningEffortSupport,
    }

    fn config() -> TestGenerationSettings {
        TestGenerationSettings {
            model: "test-model".to_string(),
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(1_024),
            parallel_tool_calls: false,
            system: None,
            reasoning_efforts: ReasoningEffortSupport::Any,
        }
    }

    fn gate() -> BudgetGate {
        BudgetGate::new(usd("1000"))
    }

    struct NoProcess;

    impl ProcessExecutor for NoProcess {
        fn execute<'a>(
            &'a self,
            _spec: &'a ego_core::permit::SpawnSpec,
        ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
            Box::pin(futures::future::ready(Err(ProcessError::Invariant(
                "process execution is not used by core turn tests".to_string(),
            ))))
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

    /// Everything a `TurnLoop` borrows, kept alive in one place so a test can
    /// build a loop in one line without fighting the borrow checker.
    struct Fixture {
        store: MemStore,
        clock: FixedClock,
        generator: Arc<FakeGenerator>,
        policy: PolicyEngine,
        fs: FakeFs,
        process: Arc<dyn ProcessExecutor>,
        evidence: NoEvidence,
        registry: Vec<std::sync::Arc<dyn ego_core::permit::Tool>>,
        gate: RefCell<BudgetGate>,
        /// Where the public projection goes. Collecting rather than null, so
        /// a test can assert on what a host would have seen — which is now
        /// the only way to see it.
        sink: CollectingSink,
    }

    impl Fixture {
        fn new(script: Vec<Vec<AiEvent>>) -> Self {
            Self {
                store: MemStore::default(),
                clock: FixedClock(1_787_000_000_000),
                generator: Arc::new(FakeGenerator::new(script)),
                policy: PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default()),
                fs: FakeFs::default(),
                process: Arc::new(NoProcess),
                evidence: NoEvidence,
                registry: Vec::new(),
                gate: RefCell::new(gate()),
                sink: CollectingSink::new(),
            }
        }

        fn with_read_tool(mut self) -> Self {
            self.registry = vec![std::sync::Arc::new(Typed(FakeReadTool))];
            self
        }

        fn with_exec_tool(mut self) -> Self {
            self.registry = vec![std::sync::Arc::new(Typed(FakeExecTool))];
            self
        }

        fn with_gated_read_tool(&mut self, released: Arc<AtomicBool>) -> Arc<AtomicBool> {
            let release_seen = Arc::new(AtomicBool::new(false));
            self.registry = vec![std::sync::Arc::new(Typed(GatedReadTool {
                released,
                release_seen: Arc::clone(&release_seen),
            }))];
            release_seen
        }

        fn with_cancelling_read_tool(
            &mut self,
            calls: Arc<AtomicUsize>,
            released: Arc<AtomicBool>,
        ) -> Arc<AtomicUsize> {
            let started = Arc::new(AtomicUsize::new(0));
            self.registry = vec![std::sync::Arc::new(Typed(CancellingReadTool {
                started: Arc::clone(&started),
                calls,
                released,
                release_seen: Arc::new(AtomicBool::new(false)),
            }))];
            started
        }

        fn with_process(&mut self, process: Arc<dyn ProcessExecutor>) {
            self.process = process;
        }

        /// A tool that mutates, which is what it takes to reach the third
        /// classification: under the default mode a read inside the workspace
        /// is settled and only a write has nobody to answer for it.
        fn with_write_tool(mut self) -> Self {
            self.registry = vec![std::sync::Arc::new(Typed(FakeWriteTool))];
            self
        }

        /// Rules an operator wrote, so a call can be settled at §6 step 3 —
        /// by a rule with an ID and a layer — rather than at step 5 by the
        /// mode. `Project` because that is the layer an operator edits.
        fn with_rules(mut self, allow: &[&str], deny: &[&str]) -> Self {
            let load = |raws: &[&str]| {
                raws.iter()
                    .map(|raw| Rule::load(raw).expect("a rule this build enforces"))
                    .collect()
            };
            self.policy = PolicyEngine::new(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::from_layer(
                    PermCfg {
                        judge: None,
                        allow: load(allow),
                        deny: load(deny),
                    },
                    Source::Project,
                ),
            );
            self
        }

        fn with_human(mut self, answers: impl IntoIterator<Item = PermissionDecision>) -> Self {
            self.policy = PolicyEngine::with_human(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::default(),
                Arc::new(ScriptedHuman::new(answers)),
            );
            self
        }

        fn with_request_observing_human(mut self) -> Self {
            self.policy = PolicyEngine::with_human(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::default(),
                Arc::new(RequestObservingHuman {
                    permission_requested: Arc::clone(&self.store.permission_requested),
                }),
            );
            self
        }

        fn with_pending_human(mut self) -> Self {
            self.policy = PolicyEngine::with_human(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::default(),
                Arc::new(PendingHuman),
            );
            self
        }

        fn with_human_and_rules(
            mut self,
            answers: impl IntoIterator<Item = PermissionDecision>,
            deny: &[&str],
        ) -> Self {
            let denied = deny
                .iter()
                .map(|raw| Rule::load(raw).expect("a rule this build enforces"))
                .collect();
            self.policy = PolicyEngine::with_human(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::from_layer(
                    PermCfg {
                        judge: None,
                        allow: Vec::new(),
                        deny: denied,
                    },
                    Source::Project,
                ),
                Arc::new(ScriptedHuman::new(answers)),
            );
            self
        }
    }

    fn session<'a>(
        state: SessionState,
        store: &'a MemStore,
        clock: &'a FixedClock,
        sink: &'a dyn EventSink,
        cancel: CancelToken,
    ) -> Session<'a> {
        Session {
            id: SessionId::new(),
            state,
            store,
            clock,
            sink,
            cancel,
        }
    }

    /// The fixture quote as the run resolved it: operator-supplied, which is
    /// the only source `models.toml` has and the first of the three §5
    /// consults.
    fn rates() -> ResolvedRates {
        ResolvedRates {
            quote: quote(),
            source: RateSource::Operator,
        }
    }

    struct FixedGenerationSource {
        generator: Arc<dyn Generator>,
        settings: TestGenerationSettings,
        metering: Metering,
        rates: Option<ResolvedRates>,
    }

    impl ego_core::generation_model_source::GenerationModelSource for FixedGenerationSource {
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
            let mut capabilities = caps();
            capabilities.max_output_tokens = self.settings.max_output_tokens;
            capabilities.supports_parallel_tool_calls = self.settings.parallel_tool_calls;
            capabilities.reasoning_efforts = self.settings.reasoning_efforts.clone();
            Box::pin(futures::future::ready(Ok(
                ego_core::generation_model_source::GenerationProfile {
                    identity: ego_core::config::ModelRef(self.settings.model.clone()),
                    request_model: self.settings.model.clone(),
                    thinking: self.settings.thinking,
                    generator: self.generator.clone(),
                    capabilities,
                    metering: self.metering.clone(),
                    rates: self.rates.clone(),
                },
            )))
        }
    }

    fn model<'a, G: Generator + 'static>(
        generator: &Arc<G>,
        gate: &'a mut BudgetGate,
    ) -> GenerationRuntime<'a> {
        model_with(generator, gate, metering(), Some(rates()), None, config())
    }

    fn model_with_reasoning<'a, G: Generator + 'static>(
        generator: &Arc<G>,
        gate: &'a mut BudgetGate,
        reasoning_efforts: ReasoningEffortSupport,
    ) -> GenerationRuntime<'a> {
        let mut settings = config();
        settings.reasoning_efforts = reasoning_efforts;
        model_with(generator, gate, metering(), Some(rates()), None, settings)
    }

    fn model_with<'a, G: Generator + 'static>(
        generator: &Arc<G>,
        gate: &'a mut BudgetGate,
        metering: Metering,
        rates: Option<ResolvedRates>,
        token_ceiling: Option<u64>,
        settings: TestGenerationSettings,
    ) -> GenerationRuntime<'a> {
        let system = settings.system.clone();
        let source = FixedGenerationSource {
            generator: generator.clone(),
            settings,
            metering,
            rates,
        };
        GenerationRuntime {
            source: Arc::new(source),
            gate,
            config: TurnConfig {
                system,
                token_ceiling,
            },
        }
    }

    /// Run one turn against `fx`, starting from `state`, and hand back where
    /// the session got to.
    fn run(fx: &Fixture, state: SessionState, prompt: &str) -> (ExitReason, bool, SessionState) {
        run_with(fx, state, prompt, metering(), Some(rates()), None, config())
    }

    fn tool_result_text(call: &ego_core::state::ItemState) -> String {
        call.tool_result
            .as_ref()
            .expect("a completed tool call has a semantic result")
            .model_text()
    }

    /// The same, under a metering of the caller's choosing.
    fn run_metered(
        fx: &Fixture,
        state: SessionState,
        prompt: &str,
        metering: Metering,
    ) -> (ExitReason, bool, SessionState) {
        run_with(fx, state, prompt, metering, Some(rates()), None, config())
    }

    /// The same, under a metering, rates, token ceiling and config the
    /// caller chose.
    fn run_with(
        fx: &Fixture,
        state: SessionState,
        prompt: &str,
        metering: Metering,
        rates: Option<ResolvedRates>,
        token_ceiling: Option<u64>,
        config: TestGenerationSettings,
    ) -> (ExitReason, bool, SessionState) {
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model_with(
                &fx.generator,
                &mut gate,
                metering,
                rates,
                token_ceiling,
                config,
            ),
            tools,
        );
        let outcome = block_on(turn.run(prompt)).expect("the store accepted every append");
        (outcome.exit_reason, outcome.ok, turn.into_state())
    }

    /// Every `seq` in the log, in the order it was written.
    fn seqs_of(fx: &Fixture) -> Vec<Seq> {
        fx.store.events().iter().map(|ev| ev.env.seq).collect()
    }

    /// `n` sequence values counted from the first one. `Seq` has no accessor
    /// on purpose — it is an ordering, not a number — so the expectation is
    /// built the same way the journal builds it.
    fn counting_from_one(n: usize) -> Vec<Seq> {
        let mut seq = Seq::ZERO;
        (0..n).map(|_| seq.next()).collect()
    }

    fn usage() -> Usage {
        tokens(100, 0, 0, 20)
    }

    fn tokens(uncached_input: u64, cache_read: u64, cache_write: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            cache_write_extended_tokens: 0,
            output_tokens: output,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn a_plain_turn_writes_the_log_it_promised() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "the answer".to_string(),
            },
            AiEvent::Usage(usage()),
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "what is the answer?");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert_eq!(
            fx.store.kinds(),
            vec![
                "user_prompt",
                "generation_started",
                "assistant_delta",
                "usage_updated",
                "assistant_message",
                "attempt_settled",
                // The loop is the only place that sees a generation boundary,
                // so it is the only place that can say how full the context
                // was at one.
                "context_window_updated",
                "turn_completed",
            ]
        );
    }

    #[test]
    fn the_log_is_numbered_in_the_order_it_was_written() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "hi".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        run(&fx, SessionState::default(), "hello");

        assert_eq!(seqs_of(&fx), counting_from_one(fx.store.events().len()));
    }

    #[test]
    fn a_second_turn_keeps_counting_where_the_first_stopped() {
        let fx = Fixture::new(vec![
            vec![AiEvent::Stop(StopReason::Stop)],
            vec![AiEvent::Stop(StopReason::Stop)],
        ]);

        let (_, _, state) = run(&fx, SessionState::default(), "first");
        let first_last = state.last_seq;
        let (_, _, state) = run(&fx, state, "second");

        assert!(
            state.last_seq > first_last,
            "a resumed session must not restart its numbering: {:?} then {:?}",
            first_last,
            state.last_seq
        );
        assert_eq!(seqs_of(&fx), counting_from_one(fx.store.events().len()));
    }

    #[test]
    fn every_timestamp_comes_from_the_injected_clock() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        run(&fx, SessionState::default(), "hello");

        for event in fx.store.events() {
            assert_eq!(
                event.env.ts,
                Timestamp::from_millis(1_787_000_000_000),
                "an envelope read a clock this test did not inject"
            );
        }
    }

    #[test]
    fn a_tool_call_and_its_result_reach_the_next_request() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "it says hello".to_string(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_read_tool();
        fx.fs.file("/ws/note.txt", "hello from the file");

        let (reason, ok, state) = run(&fx, SessionState::default(), "what is in note.txt?");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);

        // The call ran, and its output was logged against the item that
        // started it.
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the tool call became an item");
        assert_eq!(call.call_id.as_deref(), Some("call_abc"));
        assert_eq!(call.args, r#"{"path":"/ws/note.txt"}"#);
        assert_eq!(tool_result_text(call), "hello from the file");
        assert_eq!(call.ok, Some(true));

        // And the second generation was handed a history containing both
        // halves — the round trip this whole step exists for.
        let second = &fx.generator.calls()[1];
        let blocks: Vec<&ContentBlock> = second
            .messages
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::ToolUse { call_id, name, .. } if call_id == "call_abc" && name == "read"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::ToolResult { call_id, output, is_error }
                if call_id == "call_abc" && output == "hello from the file" && !is_error
        )));
    }

    #[test]
    fn a_queued_prompt_forces_a_follow_up_after_a_no_tool_final() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![
            vec![
                AiEvent::TextDelta {
                    text: "the final answer".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "the queued follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let mut submit = Box::pin(ingress.submit("deliver this after the final".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(submit.as_mut().poll(&mut cx), Poll::Pending));
        let (outcome, accepted) =
            block_on(async { futures::join!(turn.run("initial prompt"), submit) });
        let outcome = outcome.expect("the first no-tool turn closes normally");
        let accepted = accepted.expect("the queue acceptance is durable before acknowledgement");
        assert_eq!(accepted.pending_steers, 1);
        assert_eq!(accepted.queued, 0);
        assert!(!accepted.interrupt_pending);
        let events = fx.store.events();
        let first_assistant = events
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    EventKind::AssistantMessage { text } if text == "the final answer"
                )
            })
            .expect("the active turn records its first final answer");
        assert_eq!(fx.generator.calls().len(), 2);
        let correlated = events
            .iter()
            .enumerate()
            .find_map(|(index, event)| match &event.kind {
                EventKind::UserPrompt {
                    text,
                    queued_prompt: Some(_),
                    ..
                } if text == "deliver this after the final" => Some(index),
                _ => None,
            })
            .expect("the normal steer is durably correlated in this turn");
        let second_assistant = events
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    EventKind::AssistantMessage { text } if text == "the queued follow-up"
                )
            })
            .expect("the correlated follow-up is generated in this turn");
        let completed = events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1, "normal delivery remains one turn");
        let completed_index = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .expect("the same turn closes after the follow-up");
        assert!(first_assistant < correlated);
        assert!(correlated < second_assistant);
        assert!(second_assistant < completed_index);
        assert_eq!(events[completed_index].env.turn, outcome.turn);
        let generations = fx.generator.calls();
        let blocks: Vec<&ContentBlock> = generations[1]
            .messages
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Text { text } if text == "deliver this after the final"
        )));
    }

    #[test]
    fn an_ordinary_sibling_batch_delivers_a_pending_steer_at_the_safe_boundary() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "ordinary-first".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/first.txt"}"#.to_owned(),
                },
                AiEvent::ToolCall {
                    call_id: "ordinary-second".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/second.txt"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "ordinary sibling follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_rules(&["read(**)"], &[]);
        let started = fx.with_cancelling_read_tool(Arc::clone(&calls), Arc::clone(&released));
        fx.fs.file("/ws/first.txt", "first");
        fx.fs.file("/ws/second.txt", "second");

        let mut turn_config = config();
        turn_config.parallel_tool_calls = true;
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_with(
                &fx.generator,
                &mut budget,
                metering(),
                Some(rates()),
                None,
                turn_config,
            ),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("run ordinary siblings");
        let drive = async {
            futures::future::poll_fn(|cx| {
                if started.load(Ordering::SeqCst) >= 1 {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            let accepted = ingress
                .submit("ordinary sibling follow-up".to_owned())
                .await
                .expect("the ordinary steer is durably accepted while the first sibling runs");
            assert_eq!(accepted.pending_steers, 1);
            assert_eq!(accepted.queued, 0);
            assert!(!accepted.interrupt_pending);
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("ordinary sibling delivery completes");
        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(fx.generator.calls().len(), 2);
        let events = fx.store.events();
        let completions: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(event.kind, EventKind::ToolCallCompleted { .. }).then_some(index)
            })
            .collect();
        assert_eq!(completions.len(), 2, "both sibling calls complete");
        let correlated = events
            .iter()
            .enumerate()
            .find_map(|(index, event)| match &event.kind {
                EventKind::UserPrompt {
                    text,
                    queued_prompt: Some(_),
                    ..
                } if text == "ordinary sibling follow-up" => Some(index),
                _ => None,
            })
            .expect("the ordinary follow-up is correlated at the safe boundary");
        assert!(completions.into_iter().all(|index| index < correlated));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
                .count(),
            1,
            "ordinary delivery stays in one turn"
        );
        let generations = fx.generator.calls();
        let blocks: Vec<&ContentBlock> = generations[1]
            .messages
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Text { text } if text == "ordinary sibling follow-up"
        )));
    }

    #[test]
    fn partial_correlated_delivery_refreshes_status_after_each_durable_append() {
        let (first, second) = (QueuedPromptId::new(), QueuedPromptId::new());
        let seeded_turn = TurnId::new();
        let mut seq = Seq::default();
        let mut seeded = |kind| DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn: seeded_turn,
                item: None,
                parent: None,
                ts: Timestamp::from_millis(1_787_000_000_000),
                producer: Producer::Main,
            },
            kind,
        };
        let durable = vec![
            seeded(EventKind::PromptQueued {
                id: first,
                text: "first recovery prompt".to_owned(),
            }),
            seeded(EventKind::PromptQueued {
                id: second,
                text: "second recovery prompt".to_owned(),
            }),
        ];
        let fx = Fixture::new(vec![]);
        fx.store
            .events
            .lock()
            .unwrap()
            .extend(durable.iter().cloned());
        fx.store.fail_at(2);
        let mut state = SessionState::default();
        for event in &durable {
            state.apply(event);
        }
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model(&fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);

        let abandoned = block_on(turn.run_queued_turn())
            .expect_err("the second correlated UserPrompt append fails");
        drop(turn);
        assert_eq!(abandoned.state.queue_status().pending_steers, 0);
        assert_eq!(abandoned.state.queue_status().queued, 1);
        assert!(
            fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::UserPrompt {
                    queued_prompt: Some(id),
                    ..
                } if id == first
            )),
            "the first correlation remains durable before the second append fails"
        );
        assert!(!fx.store.events().iter().any(|event| matches!(
            event.kind,
            EventKind::UserPrompt {
                queued_prompt: Some(id),
                ..
            } if id == second
        )));
        let mut closed_submit = Box::pin(ingress.submit("after partial failure".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            closed_submit.as_mut().poll(&mut cx),
            Poll::Ready(Err(ControlError::Closed))
        ));
    }

    #[test]
    fn an_attached_empty_queue_run_closes_admission_without_leaking_a_pending_submit() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![]);
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        assert!(
            block_on(turn.run_queued_turn())
                .expect("empty queue run is not a store failure")
                .is_none()
        );
        let _state = turn.into_state();
        drop(budget);

        let mut closed_submit = Box::pin(ingress.submit("after empty queue run".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            closed_submit.as_mut().poll(&mut cx),
            Poll::Ready(Err(ControlError::Closed))
        ));
    }

    #[test]
    fn admission_barrier_keeps_prompts_submitted_before_and_after_turn_completion() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![
            vec![
                AiEvent::TextDelta {
                    text: "first final".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "first follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "reopened final".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "reopened follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let mut submit_before = Box::pin(ingress.submit("before completion".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            submit_before.as_mut().poll(&mut cx),
            Poll::Pending
        ));

        let state = {
            let mut budget = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                model(&fx.generator, &mut budget),
                tools,
            )
            .with_interactive_control(&mut control);
            let (outcome, accepted) =
                block_on(async { futures::join!(turn.run("initial prompt"), submit_before) });
            assert_eq!(
                outcome
                    .expect("the before-boundary prompt does not interrupt the turn")
                    .exit_reason,
                ExitReason::Completed
            );
            assert_eq!(
                accepted
                    .expect("the before-boundary prompt is durably admitted")
                    .pending_steers,
                1
            );
            let state = turn.into_state();
            assert_eq!(state.queue_status().queued, 0);
            state
        };

        let mut closed_submit = Box::pin(ingress.submit("after close".to_owned()));
        assert!(matches!(
            closed_submit.as_mut().poll(&mut cx),
            Poll::Ready(Err(ControlError::Closed))
        ));
        let (outcome, accepted) = {
            let mut budget = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
                model(&fx.generator, &mut budget),
                tools,
            )
            .with_interactive_control(&mut control);
            let mut reopened_submit = Box::pin(ingress.submit("after reopen".to_owned()));
            assert!(matches!(
                reopened_submit.as_mut().poll(&mut cx),
                Poll::Pending
            ));
            block_on(async { futures::join!(turn.run("reopened prompt"), reopened_submit) })
        };
        let outcome = outcome.expect("the reopened prompt does not interrupt the turn");
        let accepted = accepted.expect("the reopened prompt is durably admitted");
        assert_eq!(accepted.pending_steers, 1);
        assert_eq!(fx.generator.calls().len(), 4);
        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        let events = fx.store.events();
        for text in [
            "before completion",
            "first follow-up",
            "after reopen",
            "reopened follow-up",
        ] {
            assert!(
                events.iter().any(|event| matches!(
                    &event.kind,
                    EventKind::UserPrompt { text: actual, .. } if actual == text
                ) || matches!(
                    &event.kind,
                    EventKind::AssistantMessage { text: actual } if actual == text
                )),
                "the admission barrier lost {text:?}"
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
                .count(),
            2,
            "before and after admissions remain two safe-boundary turns"
        );
    }

    #[test]
    fn all_sibling_tool_results_close_before_the_queued_follow_up() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let cancel = CancelToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call-first".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/first.txt"}"#.to_owned(),
                },
                AiEvent::ToolCall {
                    call_id: "call-second".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/second.txt"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_rules(&["read(**)"], &[]);
        fx.with_cancelling_read_tool(Arc::clone(&calls), Arc::clone(&released));
        fx.fs.file("/ws/first.txt", "first");
        fx.fs.file("/ws/second.txt", "second");

        let mut turn_config = config();
        turn_config.parallel_tool_calls = true;
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                cancel,
            ),
            model_with(
                &fx.generator,
                &mut budget,
                metering(),
                Some(rates()),
                None,
                turn_config,
            ),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("run both reads");
        let drive = async {
            let accepted = ingress
                .submit("after both siblings".to_owned())
                .await
                .expect("sibling follow-up acceptance is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            let expedited = ingress
                .interrupt()
                .await
                .expect("sibling follow-up interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("sibling results were durably closed");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let events = fx.store.events();
        let completed: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(event.kind, EventKind::ToolCallCompleted { .. }).then_some(index)
            })
            .collect();
        assert_eq!(completed.len(), 2, "both sibling calls must close");
        assert_live_interrupt_order(&fx, |kind| {
            matches!(kind, EventKind::ToolCallCompleted { .. })
        });
        let first_completed = events
            .iter()
            .position(|event| {
                event.env.turn == outcome.turn
                    && matches!(event.kind, EventKind::TurnCompleted { .. })
            })
            .expect("the active sibling batch closes before queue delivery");
        assert!(!events[..first_completed].iter().any(|event| {
            matches!(
                event.kind,
                EventKind::UserPrompt {
                    queued_prompt: Some(_),
                    ..
                }
            )
        }));
        let state = turn.into_state();
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::new(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model(&fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let recovered = block_on(recovery.run_queued_turn())
            .expect("the sibling follow-up remains replayable")
            .expect("the sibling follow-up runs on a fresh turn");
        assert_ne!(recovered.turn, outcome.turn);
        let events = fx.store.events();
        let correlated = events
            .iter()
            .enumerate()
            .find_map(|(index, event)| {
                matches!(
                    event.kind,
                    EventKind::UserPrompt {
                        queued_prompt: Some(_),
                        ..
                    }
                )
                .then_some(index)
            })
            .expect("the follow-up is correlated");
        assert!(first_completed < correlated);
        let calls = fx.generator.calls();
        let blocks: Vec<&ContentBlock> = calls[1]
            .messages
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Text { text } if text == "after both siblings"
        )));
        assert!(
            completed.into_iter().all(|index| index < correlated),
            "queue delivery must not split the sibling call/result batch"
        );
    }

    #[test]
    fn busy_ingress_is_durable_before_ack_and_projects_pending_steer() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![]);
        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "streaming".to_owned(),
            }]),
        });
        let mut budget = gate();
        let sink = CollectingSink::new();
        let policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());
        let fs = FakeFs::default();
        let process = NoProcess;
        let evidence = NoEvidence;
        let cancel = CancelToken::new();
        let mut turn = TurnLoop::new(
            session(SessionState::default(), &fx.store, &fx.clock, &sink, cancel),
            model(&generator, &mut budget),
            Tools::new(
                &[],
                &policy,
                ToolEffectPorts::new(&fs, &process, &evidence, &NO_RESOLVER, &NO_HOP, &NO_DUPLEX),
                CanonPath::new("/ws"),
            ),
        )
        .with_interactive_control(&mut control);

        let active = turn.run("initial prompt");
        let drive = async {
            let accepted = ingress
                .submit("durably accepted while the model is busy".to_owned())
                .await
                .expect("acknowledgement follows PromptQueued durability");
            assert_eq!(accepted.pending_steers, 1);
            assert_eq!(accepted.queued, 0);
            assert!(!accepted.interrupt_pending);
            let expedited = ingress
                .interrupt()
                .await
                .expect("expedited intent is durable before interruption");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("interruption settles the active turn");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
    }

    #[test]
    fn a_ready_provider_stream_cannot_starve_a_durable_control_intent() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![]);
        let generator = Arc::new(ContinuouslyReadyGenerator);
        let mut budget = gate();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);

        let mut submit = Box::pin(ingress.submit("ready-stream steer".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(submit.as_mut().poll(&mut cx), Poll::Pending));
        let active = turn.run("initial prompt");
        let (outcome, accepted) = block_on(async { futures::join!(active, submit) });
        let outcome = outcome.expect("ready-stream control remains durably settled");
        let accepted = accepted.expect("a ready provider must still acknowledge control");
        assert_eq!(accepted.pending_steers, 1);
        let events = fx.store.events();
        let queued = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::PromptQueued { .. }))
            .expect("the ready-stream steer was persisted");
        let delta = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::AssistantDelta { .. }))
            .expect("the ready-stream delta is persisted");
        let completed = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .expect("the ready-stream turn closes");
        assert!(queued < delta, "the ack precedes the ready stream");
        assert!(queued < completed, "the ack precedes turn completion");
        assert_eq!(outcome.turn, events[completed].env.turn);
    }

    #[test]
    fn concurrent_ingress_submits_share_global_capacity() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let first_ingress = ingress.clone();
        let second_ingress = ingress.clone();
        let mut first = Box::pin(first_ingress.submit("first concurrent steer".to_owned()));
        let mut second = Box::pin(second_ingress.submit("second concurrent steer".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Pending));
        assert!(
            matches!(second.as_mut().poll(&mut cx), Poll::Pending),
            "the second clone must remain pending while the sole slot is occupied"
        );

        let fx = Fixture::new(vec![]);
        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "coordinator consumes one intent at a time".to_owned(),
            }]),
        });
        let mut budget = gate();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("initial prompt");
        let drive = async {
            let first_status = first.await.expect("the first submit is acknowledged");
            assert_eq!(first_status.pending_steers, 1);
            let second_status = second.await.expect("the second submit follows the first");
            assert_eq!(second_status.pending_steers, 2);
            ingress
                .interrupt()
                .await
                .expect("the shared coordinator accepts the final interrupt");
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        assert_eq!(
            outcome
                .expect("the shared coordinator remains live")
                .exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
    }

    #[test]
    fn interrupt_does_not_append_a_receding_marker_after_an_adversarial_ack() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let first = QueuedPromptId::new();
        let second = QueuedPromptId::new();
        let seeded_turn = TurnId::new();
        let mut seq = Seq::default();
        let mut seeded = |kind| DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn: seeded_turn,
                item: None,
                parent: None,
                ts: Timestamp::from_millis(1_787_000_000_000),
                producer: Producer::Main,
            },
            kind,
        };
        let durable = vec![
            seeded(EventKind::PromptQueued {
                id: first,
                text: "first remains pending".to_owned(),
            }),
            seeded(EventKind::PromptQueued {
                id: second,
                text: "second was delivered".to_owned(),
            }),
            seeded(EventKind::PromptDeliveryExpedited { through: second }),
            seeded(EventKind::UserPrompt {
                text: "second was delivered".to_owned(),
                queued_prompt: Some(second),
            }),
        ];
        let fx = Fixture::new(vec![]);
        fx.store.events.lock().unwrap().extend(durable.clone());
        let mut state = SessionState::default();
        for event in &durable {
            state.apply(event);
        }
        assert_eq!(state.unacknowledged_prompts().len(), 1);
        assert_eq!(state.unacknowledged_prompts()[0].id(), first);
        assert_eq!(state.expedited_through(), Some(second));

        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "busy after adversarial ack".to_owned(),
            }]),
        });
        let mut budget = gate();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model(&generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("resume the live turn");
        let interrupt = async {
            ingress
                .interrupt()
                .await
                .expect("the receding interrupt is still acknowledged durably");
        };
        let (outcome, _) = block_on(async { futures::join!(active, interrupt) });
        assert_eq!(
            outcome
                .expect("the seeded durable state remains writable")
                .exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        let markers: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::PromptDeliveryExpedited { through } => Some(through),
                _ => None,
            })
            .collect();
        assert_eq!(
            markers,
            vec![second],
            "interrupt must not append through first"
        );
    }

    #[test]
    fn store_failure_before_ack_returns_control_error_and_leaves_queue_unchanged() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![]);
        // The stalled provider leaves the ingress window open before the
        // coordinator appends PromptQueued. Fail that event by kind so this
        // test cannot accidentally target an unrelated lifecycle append.
        fx.store.fail_prompt_queued();
        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(Vec::new()),
        });
        let mut budget = gate();
        let sink = CollectingSink::new();
        let policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());
        let fs = FakeFs::default();
        let process = NoProcess;
        let evidence = NoEvidence;
        let cancel = CancelToken::new();
        let mut turn = TurnLoop::new(
            session(SessionState::default(), &fx.store, &fx.clock, &sink, cancel),
            model(&generator, &mut budget),
            Tools::new(
                &[],
                &policy,
                ToolEffectPorts::new(&fs, &process, &evidence, &NO_RESOLVER, &NO_HOP, &NO_DUPLEX),
                CanonPath::new("/ws"),
            ),
        )
        .with_interactive_control(&mut control);

        let active = turn.run("initial prompt");
        let submit = ingress.submit("must not be acknowledged".to_owned());
        let (outcome, result) = block_on(async { futures::join!(active, submit) });
        let abandoned = outcome.expect_err("a failed queue append abandons the turn");
        assert_eq!(abandoned.state.queue_status().pending_steers, 0);
        assert_eq!(abandoned.state.queue_status().queued, 0);
        let Err(ControlError::Store(message)) = result else {
            panic!("failed queue append must return ControlError::Store: {result:?}");
        };
        assert!(
            !message.is_empty(),
            "the terminal receives the store failure"
        );
        assert!(
            fx.store
                .events()
                .iter()
                .all(|event| !matches!(event.kind, EventKind::PromptQueued { .. }))
        );
        drop(turn);
    }

    #[test]
    fn pending_human_approval_interrupt_decides_false_and_never_executes() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "permission-interrupt".to_owned(),
                    name: "fake_write".to_owned(),
                    args: r#"{"path":"/ws/out.txt","contents":"must not land"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued recovery".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_write_tool()
        .with_pending_human();
        fx.fs.file("/ws/out.txt", "original");
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);

        let active = turn.run("write it");
        let drive = async {
            let accepted = ingress
                .submit("permission follow-up".to_owned())
                .await
                .expect("the permission-phase prompt is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            let expedited = ingress
                .interrupt()
                .await
                .expect("the permission-phase interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("the store accepted the cancellation ledger");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(),
            b"original"
        );
        assert!(fx.store.events().iter().any(|event| {
            matches!(
                event.kind,
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    ..
                }
            )
        }));
        let call = turn
            .state()
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the pending call remains visible");
        assert_eq!(call.ok, Some(false));
        assert_live_interrupt_order(&fx, |kind| {
            matches!(
                kind,
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    ..
                } | EventKind::ToolCallCompleted { ok: false, .. }
            )
        });
        let state = turn.into_state();
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::new(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model(&fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let queued = block_on(recovery.run_queued_turn())
            .expect("queued recovery is durable")
            .expect("the accepted permission follow-up runs");
        assert_ne!(queued.turn, outcome.turn);
    }

    #[test]
    fn running_cancellable_process_records_actual_failed_cancellation_result() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "exec-cancel".to_owned(),
                    name: "exec".to_owned(),
                    args: r#"{"argv":["long-running"]}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued process recovery".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_exec_tool()
        .with_rules(&["exec(*)"], &[]);
        fx.with_process(Arc::new(GatedCancellableProcess {
            calls: Arc::clone(&calls),
            released: Arc::clone(&released),
            release_seen: Arc::new(AtomicBool::new(false)),
        }));

        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("run it");
        let drive = async {
            let accepted = ingress
                .submit("process follow-up".to_owned())
                .await
                .expect("process-phase acceptance is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the steer was admitted while the process was running"
            );
            let expedited = ingress
                .interrupt()
                .await
                .expect("process-phase interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("the store accepted the process cancellation ledger");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let call = turn
            .state()
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the process call remains visible");
        assert_eq!(call.ok, Some(false));
        assert!(tool_result_text(call).contains("process was cancelled"));
        assert_live_interrupt_order(&fx, |kind| {
            matches!(kind, EventKind::ToolCallCompleted { ok: false, .. })
        });
        let state = turn.into_state();
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::new(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model(&fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let recovered = block_on(recovery.run_queued_turn())
            .expect("the process follow-up remains replayable")
            .expect("the process follow-up runs on a fresh turn");
        assert_ne!(recovered.turn, outcome.turn);
    }

    #[test]
    fn non_cancellable_builtin_waits_for_release_then_records_actual_success() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "builtin-read".to_owned(),
                    name: "builtin_read".to_owned(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued builtin recovery".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_rules(&["read(**)"], &[]);
        let release_seen = fx.with_gated_read_tool(Arc::clone(&released));
        fx.fs.file("/ws/note.txt", "builtin succeeded");
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("read it");
        let drive = async {
            let accepted = ingress
                .submit("builtin follow-up".to_owned())
                .await
                .expect("builtin-phase acceptance is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            let expedited = ingress
                .interrupt()
                .await
                .expect("builtin-phase interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
            assert!(
                !fx.store
                    .events()
                    .iter()
                    .any(|event| { matches!(event.kind, EventKind::ToolCallCompleted { .. }) }),
                "the active builtin remains incomplete until its owner releases it"
            );
            assert!(
                !release_seen.load(Ordering::SeqCst),
                "the non-cancellable effect remains active until its owner releases it"
            );
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("the released builtin completes");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        let call = turn
            .state()
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the builtin call remains visible");
        assert_eq!(call.ok, Some(true));
        assert!(tool_result_text(call).contains("builtin succeeded"));
        assert_live_interrupt_order(&fx, |kind| {
            matches!(kind, EventKind::ToolCallCompleted { ok: true, .. })
        });
        let state = turn.into_state();
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::new(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model(&fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let recovered = block_on(recovery.run_queued_turn())
            .expect("the builtin follow-up remains replayable")
            .expect("the builtin follow-up runs on a fresh turn");
        assert_ne!(recovered.turn, outcome.turn);
    }

    #[test]
    fn three_siblings_keep_current_result_and_close_unstarted_calls_explicitly() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let cancel = CancelToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "sibling-1".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/one.txt"}"#.to_owned(),
                },
                AiEvent::ToolCall {
                    call_id: "sibling-2".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/two.txt"}"#.to_owned(),
                },
                AiEvent::ToolCall {
                    call_id: "sibling-3".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/three.txt"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued sibling recovery".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_rules(&["read(**)"], &[]);
        fx.with_cancelling_read_tool(Arc::clone(&calls), Arc::clone(&released));
        fx.fs.file("/ws/one.txt", "one");
        fx.fs.file("/ws/two.txt", "two");
        fx.fs.file("/ws/three.txt", "three");
        let mut turn_config = config();
        turn_config.parallel_tool_calls = true;
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                cancel,
            ),
            model_with(
                &fx.generator,
                &mut budget,
                metering(),
                Some(rates()),
                None,
                turn_config,
            ),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("run siblings");
        let drive = async {
            let accepted = ingress
                .submit("sibling follow-up".to_owned())
                .await
                .expect("sibling-batch acceptance is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            let expedited = ingress
                .interrupt()
                .await
                .expect("sibling-batch interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("the store accepted sibling closure");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut calls: Vec<_> = turn
            .state()
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall)
            .collect();
        calls.sort_by_key(|item| item.call_id.clone());
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].ok, Some(true));
        assert_eq!(calls[1].ok, Some(false));
        assert_eq!(calls[2].ok, Some(false));
        assert!(tool_result_text(calls[1]).contains("interrupted"));
        assert!(tool_result_text(calls[2]).contains("interrupted"));
        assert_live_interrupt_order(&fx, |kind| {
            matches!(kind, EventKind::ToolCallCompleted { .. })
        });
        let state = turn.into_state();
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::new(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model(&fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let recovered = block_on(recovery.run_queued_turn())
            .expect("the sibling follow-up remains replayable")
            .expect("the sibling follow-up runs on a fresh turn");
        assert_ne!(recovered.turn, outcome.turn);
    }

    /// A call nobody in this run can answer ends the run, and says so in the
    /// words of a question rather than a refusal.
    ///
    /// `CLI.md` §4.4 gives both endings exit 3, which is exactly why the text
    /// has to differ. "Denied by r1:…" tells an operator which line of their
    /// config to go and look at; "no rule covers write /ws/note.txt" tells
    /// them there is no line to look at and they have to write one. A loop
    /// that collapsed the two would leave the commonest case reading like a
    /// refusal somebody configured on purpose.
    #[test]
    fn a_call_nobody_can_answer_stops_the_run_and_reads_as_a_question() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "fake_write".to_string(),
                    args: r#"{"path":"/ws/note.txt","contents":"pwned"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_write_tool();
        fx.fs.file("/ws/note.txt", "the original");

        let (reason, ok, state) = run(&fx, SessionState::default(), "overwrite note.txt");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);

        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the tool call became an item");
        assert_eq!(call.ok, Some(false));
        // "does not approve ... on its own" and not "no rule covers": both are
        // true, and the second is the reason the run stopped. The rules did
        // not cover it, so it went to the mode at §6 step 5, and the mode
        // declined too. The last party to decline is the one whose reason an
        // operator can act on — it is what tells them `--yolo` and `--allow`
        // are both available, where "no rule covers" names only the second.
        let result = tool_result_text(call);
        assert!(
            result.contains("needs permission") && result.contains("does not approve"),
            "an unanswerable call must read as a question: {}",
            result
        );
        assert!(
            result.contains("/ws/note.txt"),
            "and must name what it is a question about: {}",
            result
        );

        assert_eq!(
            fx.generator.calls().len(),
            1,
            "the run stopped, so there is no second generation"
        );
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/note.txt")).unwrap(),
            b"the original",
            "a call that was never authorised must not have run"
        );
    }

    /// A system prompt is a setting the whole turn runs under, so it leads
    /// *every* generation — not just the first — and it is never written to
    /// the log. Writing it there would make a resumed session rebuild it as a
    /// message someone sent, and then send it twice.
    #[test]
    fn a_system_prompt_leads_every_generation_and_is_never_logged() {
        const HOUSE_STYLE: &str = "the house style: no semicolons";

        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();
        fx.fs.file("/ws/note.txt", "hello from the file");

        let mut config = config();
        config.system = Some(SystemPrompt::verbatim(HOUSE_STYLE));
        run_with(
            &fx,
            SessionState::default(),
            "what is in note.txt?",
            metering(),
            Some(rates()),
            None,
            config,
        );

        let calls = fx.generator.calls();
        assert_eq!(calls.len(), 2, "the tool call should force a second pass");
        for (nth, call) in calls.iter().enumerate() {
            let lead = &call.messages.messages()[0];
            assert_eq!(
                lead.role,
                Role::System,
                "generation {nth} led with a {lead:?}"
            );
            assert!(
                matches!(&lead.content[0], ContentBlock::Text { text } if text == HOUSE_STYLE),
                "generation {nth} did not carry the system prompt: {lead:?}"
            );
        }

        // Debug over the whole event rather than a field: the claim is that
        // *no* kind carries it, and naming the kinds that must not would go
        // stale the moment `EventKind` grows one.
        for event in fx.store.events() {
            assert!(
                !format!("{:?}", event.kind).contains(HOUSE_STYLE),
                "the system prompt was written to the log as {:?}",
                event.kind
            );
        }
    }

    /// No system prompt means no system message — not an empty one. A
    /// provider that receives `""` in the system role has been told something.
    #[test]
    fn without_a_system_prompt_a_request_carries_no_system_message() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        run(&fx, SessionState::default(), "hello");

        let roles: Vec<Role> = fx.generator.calls()[0]
            .messages
            .messages()
            .iter()
            .map(|message| message.role)
            .collect();
        assert_eq!(roles, vec![Role::User]);
    }

    #[test]
    fn the_model_is_told_which_tools_it_may_call() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]).with_read_tool();

        run(&fx, SessionState::default(), "hello");

        let names: Vec<String> = fx.generator.calls()[0]
            .tools
            .iter()
            .map(|tool: &ToolDescriptor| tool.name.clone())
            .collect();
        assert_eq!(names, vec!["read".to_string()]);
    }

    #[test]
    fn a_tool_nobody_registered_is_answered_not_raised() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "delete_everything".to_string(),
                    args: "{}".to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (reason, _, state) = run(&fx, SessionState::default(), "delete everything");

        assert_eq!(reason, ExitReason::Completed);
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call was logged even though nothing could run it");
        assert_eq!(call.ok, Some(false));
        let result = tool_result_text(call);
        assert!(
            result.contains("delete_everything"),
            "the refusal must name the tool: {}",
            result
        );
    }

    #[test]
    fn arguments_that_are_not_json_are_answered_not_raised() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    // What a tool call truncated mid-stream looks like.
                    args: r#"{"path":"/ws/no"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (_, _, state) = run(&fx, SessionState::default(), "read it");

        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call was logged");
        assert_eq!(call.ok, Some(false));
        let result = tool_result_text(call);
        assert!(
            result.contains("not JSON"),
            "the model has to learn its arguments were malformed: {}",
            result
        );
    }

    #[test]
    fn a_read_outside_the_workspace_is_refused_by_policy() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/etc/passwd"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();
        fx.fs.file("/etc/passwd", "root:x:0:0");

        let (_, _, state) = run(&fx, SessionState::default(), "read /etc/passwd");

        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call was logged");
        assert_eq!(call.ok, Some(false));
        let result = tool_result_text(call);
        assert!(
            result.contains("/etc/passwd") && result.contains("outside the workspace"),
            "a denial must name what it denied: {}",
            result
        );
        assert!(
            !result.contains("root:x"),
            "the file must not have been read at all: {}",
            result
        );
    }

    /// Everything the log recorded about permissions, in order.
    fn permission_events(fx: &Fixture) -> Vec<EventKind> {
        fx.store
            .events()
            .into_iter()
            .map(|ev| ev.kind)
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::PermissionRequested { .. }
                        | EventKind::PermissionAmended { .. }
                        | EventKind::PermissionDecided { .. }
                )
            })
            .collect()
    }

    fn assert_live_interrupt_order(fx: &Fixture, terminal: impl Fn(&EventKind) -> bool) {
        let events = fx.store.events();
        let queued = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::PromptQueued { .. }))
            .expect("live submit is durable");
        let expedited = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::PromptDeliveryExpedited { .. }))
            .expect("live interrupt is durable");
        let truthful = events
            .iter()
            .position(|event| terminal(&event.kind))
            .expect("the interrupted phase has a truthful terminal event");
        let completed = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .expect("the interrupted turn closes durably");
        assert!(queued < expedited, "acceptance precedes expedition");
        assert!(expedited < truthful, "expedition precedes truthful closure");
        assert!(
            truthful < completed,
            "truthful closure precedes turn closure"
        );
    }

    fn reading(path: &str) -> Vec<Vec<AiEvent>> {
        vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: format!(r#"{{"path":"{path}"}}"#),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ]
    }

    /// §7: *every decision is appended, asked or not.*
    ///
    /// This is the case the audit event exists for. Nobody was asked, nothing
    /// stopped, the file was read — and the log still says which rule, in
    /// which layer, let it happen. Before §7 the only trace was a successful
    /// `ToolCallCompleted`, which records that a read happened and not that
    /// anybody authorised it.
    #[test]
    fn a_call_the_rules_allow_records_the_rule_that_allowed_it() {
        let fx = Fixture::new(reading("/ws/notes.md"))
            .with_read_tool()
            .with_rules(&["read(**)"], &[]);
        fx.fs.file("/ws/notes.md", "hello");

        let (reason, _, _) = run(&fx, SessionState::default(), "read the notes");
        assert_eq!(reason, ExitReason::Completed, "the read was allowed");

        let recorded = permission_events(&fx);
        let [EventKind::PermissionDecided { allowed, by, .. }] = recorded.as_slice() else {
            panic!("one decision, and no question: {recorded:?}");
        };
        assert_eq!(*allowed, Some(true));
        let Some(Authority::Rules { rules }) = by else {
            panic!("a rule settled it at step 3, not the mode at step 5: {by:?}");
        };
        let [rule] = rules.as_slice() else {
            panic!("one op, so one rule: {rules:?}");
        };
        assert_eq!(rule.from, Source::Project, "the layer an operator can edit");
        assert!(
            rule.also.is_empty(),
            "one layer wrote it, so there is no other copy to mention"
        );
    }

    /// The same event, from the other end of §6: no rule matched, so the mode
    /// answered at step 5. `by` says so, and it names the mode rather than
    /// inventing a rule ID for a decision no rule made.
    #[test]
    fn a_call_only_the_mode_allows_names_the_mode_and_not_a_rule() {
        let fx = Fixture::new(reading("/ws/notes.md")).with_read_tool();
        fx.fs.file("/ws/notes.md", "hello");

        run(&fx, SessionState::default(), "read the notes");

        let recorded = permission_events(&fx);
        let [EventKind::PermissionDecided { allowed, by, .. }] = recorded.as_slice() else {
            panic!("one decision, and no question: {recorded:?}");
        };
        assert_eq!(*allowed, Some(true));
        assert_eq!(
            by.as_ref(),
            Some(&Authority::Mode {
                mode: Mode::Default
            })
        );
    }

    /// A refusal at §6 step 0. The boundary is nobody's rule — no `--allow`
    /// widens it and no layer carries it — so the authority has no ID to
    /// record, and recording one would send an operator looking for a line of
    /// config that does not exist.
    #[test]
    fn a_refusal_at_the_boundary_records_an_authority_with_no_rule_to_revoke() {
        let fx = Fixture::new(reading("/etc/passwd")).with_read_tool();
        fx.fs.file("/etc/passwd", "root:x:0:0");

        let (reason, _, _) = run(&fx, SessionState::default(), "read /etc/passwd");
        assert_eq!(reason, ExitReason::PermissionRequired);

        let recorded = permission_events(&fx);
        let [EventKind::PermissionDecided { allowed, by, .. }] = recorded.as_slice() else {
            panic!("one decision, and no question: {recorded:?}");
        };
        assert_eq!(*allowed, Some(false));
        assert_eq!(by.as_ref(), Some(&Authority::Boundary));
    }

    /// The mode raised a canonical question and the headless run had no final
    /// seat. The request is still durable even though no decision follows.
    /// Silent rule/mode decisions above remain decisions without requests.
    #[test]
    fn a_question_nobody_can_answer_is_recorded_as_a_question_and_not_a_decision() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "fake_write".to_string(),
                    args: r#"{"path":"/ws/out.txt","contents":"x"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_write_tool();

        let (reason, _, _) = run(&fx, SessionState::default(), "write it");
        assert_eq!(reason, ExitReason::PermissionRequired);

        let recorded = permission_events(&fx);
        let [EventKind::PermissionRequested { tool, escalation }] = recorded.as_slice() else {
            panic!("one question, and no decision: {recorded:?}");
        };
        assert_eq!(tool, "fake_write");
        assert_eq!(
            escalation.to_string(),
            "default mode does not approve write /ws/out.txt on its own",
            "the escalation names the step that asked and the op it asked about"
        );
    }

    fn writing(path: &str, contents: &str) -> Vec<Vec<AiEvent>> {
        vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "fake_write".to_string(),
                    args: format!(r#"{{"path":"{path}","contents":"{contents}"}}"#),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ]
    }

    #[test]
    fn a_human_allow_mints_for_and_executes_the_exact_prepared_call() {
        let fx = Fixture::new(writing("/ws/out.txt", "allowed"))
            .with_write_tool()
            .with_human([PermissionDecision::Allow]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(),
            b"allowed"
        );
        let recorded = permission_events(&fx);
        assert!(matches!(
            recorded.as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionDecided {
                    allowed: Some(true),
                    by: Some(Authority::Party),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn a_permission_request_is_durable_before_the_human_is_called() {
        let fx = Fixture::new(writing("/ws/out.txt", "allowed"))
            .with_write_tool()
            .with_request_observing_human();

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert!(matches!(
            permission_events(&fx).as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionDecided {
                    allowed: Some(true),
                    by: Some(Authority::Party),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn a_human_deny_is_a_settled_refusal_and_executes_nothing() {
        let fx = Fixture::new(writing("/ws/out.txt", "denied"))
            .with_write_tool()
            .with_human([PermissionDecision::Deny]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        let recorded = permission_events(&fx);
        assert!(matches!(
            recorded.as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: Some(Authority::Party),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn an_amendment_restarts_at_prepare_and_cannot_cross_the_workspace_boundary() {
        let fx = Fixture::new(writing("/ws/out.txt", "original"))
            .with_write_tool()
            .with_human([PermissionDecision::Amend {
                params: json!({"path": "/etc/ego-owned", "contents": "escaped"}),
            }]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        assert!(fx.fs.read(&CanonPath::new("/etc/ego-owned")).is_err());
        let recorded = permission_events(&fx);
        assert!(matches!(
            recorded.as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionAmended { .. },
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: Some(Authority::Boundary),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn an_amendment_restarts_at_classification_and_cannot_cross_a_deny_rule() {
        let fx = Fixture::new(writing("/ws/out.txt", "original"))
            .with_write_tool()
            .with_human_and_rules(
                [PermissionDecision::Amend {
                    params: json!({"path": "/ws/blocked.txt", "contents": "blocked"}),
                }],
                &["write(blocked.txt)"],
            );

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        assert!(fx.fs.read(&CanonPath::new("/ws/blocked.txt")).is_err());
        let recorded = permission_events(&fx);
        assert!(matches!(
            recorded.as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionAmended { .. },
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: Some(Authority::Rules { .. }),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn malformed_amended_params_are_tool_feedback_and_execute_nothing() {
        let fx = Fixture::new(writing("/ws/out.txt", "original"))
            .with_write_tool()
            .with_human([PermissionDecision::Amend {
                params: json!({"path": "/ws/out.txt"}),
            }]);

        let (reason, _, state) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::Completed);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the amended call still completes its tool item");
        assert_eq!(call.ok, Some(false));
        let result = tool_result_text(call);
        assert!(result.contains("contents"), "{}", result);
        assert!(matches!(
            permission_events(&fx).as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionAmended { .. }
            ]
        ));
    }

    /// The permission event gets an item of its own, and the tool call keeps
    /// its own. One id per item is what lets a consumer say which lifecycle
    /// it is watching; the parent link is what still ties the two together.
    #[test]
    fn a_permission_event_is_its_own_item_and_names_the_call_as_its_cause() {
        let fx = Fixture::new(reading("/ws/notes.md")).with_read_tool();
        fx.fs.file("/ws/notes.md", "hello");

        run(&fx, SessionState::default(), "read the notes");

        let log = fx.store.events();
        let started = log
            .iter()
            .find(|ev| matches!(ev.kind, EventKind::ToolCallStarted { .. }))
            .expect("the call was logged");
        let decided = log
            .iter()
            .find(|ev| matches!(ev.kind, EventKind::PermissionDecided { .. }))
            .expect("the decision was logged");

        assert_ne!(
            decided.env.item, started.env.item,
            "a permission is not the tool item wearing a second kind"
        );
        assert_eq!(
            decided.env.parent,
            Some(started.env.id),
            "the decision is caused by the call it authorised"
        );
    }

    #[test]
    fn a_generation_that_calls_nothing_ends_the_turn_even_if_it_says_tool_use() {
        // A provider that keeps reporting `ToolUse` with no calls would spin
        // forever if the loop believed the stop reason over the evidence.
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::ToolUse)]]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert_eq!(fx.generator.calls().len(), 1);
    }

    #[test]
    fn a_cancelled_turn_still_settles_its_attempt() {
        let fx = Fixture::new(vec![]);
        let token = CancelToken::new();
        let generator = Arc::new(CancellingGenerator {
            events: Mutex::new(vec![
                AiEvent::TextDelta {
                    text: "I have started".to_string(),
                },
                AiEvent::TextDelta {
                    text: " and will not finish".to_string(),
                },
                AiEvent::Stop(StopReason::Stop),
            ]),
            after: 1,
            token: token.clone(),
        });

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                token.clone(),
            ),
            model(&generator, &mut gate),
            tools,
        );
        assert!(!token.is_cancelled(), "nothing has asked to stop yet");

        let outcome = block_on(turn.run("start something long")).expect("the store accepted");

        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert!(!outcome.ok);
        let kinds = fx.store.kinds();
        assert!(
            kinds.contains(&"attempt_settled".to_string()),
            "a cancelled turn must still settle: {kinds:?}"
        );
        let settled = fx
            .store
            .events()
            .into_iter()
            .find_map(|ev| match ev.kind {
                EventKind::AttemptSettled { outcome, .. } => Some(outcome),
                _ => None,
            })
            .expect("the settlement was logged");
        assert_eq!(
            settled.stop(),
            StopReason::Aborted,
            "a stream nobody finished draining aborted, whatever it was about to say"
        );
        drop(turn);
        assert_eq!(
            gate.outstanding(),
            Some(&usd("0")),
            "the reservation was released, not leaked"
        );
    }

    #[test]
    fn the_first_reason_a_turn_was_asked_to_stop_is_the_one_it_reports() {
        // A `SIGINT` and a deadline can both land, in either order, on a run
        // that is already shutting down — the second is often *caused* by the
        // first, since a cancelled turn still has to unwind. Reporting
        // whichever stored last would exit 124 for a run the user stopped,
        // and a scheduler that retries on timeout would then retry it.
        let token = CancelToken::new();
        assert_eq!(token.cause(), None, "nothing has asked to stop yet");

        token.cancel(CancelCause::User);
        token.cancel(CancelCause::Timeout);
        token.cancel(CancelCause::Host);

        assert!(token.is_cancelled());
        assert_eq!(
            token.cause(),
            Some(CancelCause::User),
            "the user asked first, so the run stopped because the user asked"
        );

        // And the same property from the other end: a deadline that arrives
        // first is not overwritten by the signal the shutdown then raises.
        let token = CancelToken::new();
        token.cancel(CancelCause::Timeout);
        token.cancel(CancelCause::User);
        assert_eq!(token.cause(), Some(CancelCause::Timeout));
    }

    #[test]
    fn a_cancelled_turn_does_not_run_the_tools_it_was_about_to() {
        // The hard case, and the reason cancellation is checked on the
        // generation's result and not only at the top of the loop: here the
        // cancel lands on the stream's *last* event, so the attempt settles
        // cleanly as `ToolUse` with a call already logged. A loop that only
        // looked at the stop reason would run that call — a side effect the
        // user asked not to happen — and only notice the cancellation on the
        // next iteration, once it was too late to matter.
        let fx = Fixture::new(vec![]).with_read_tool();
        fx.fs.file("/ws/note.txt", "hello from the file");
        let token = CancelToken::new();
        let generator = Arc::new(CancellingGenerator {
            events: Mutex::new(vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ]),
            after: 2,
            token: token.clone(),
        });

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                token,
            ),
            model(&generator, &mut gate),
            tools,
        );

        let outcome = block_on(turn.run("read note.txt")).expect("the store accepted");

        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        let kinds = fx.store.kinds();
        assert!(
            kinds.contains(&"tool_call_completed".to_string()),
            "a cancelled turn must close the queued call explicitly: {kinds:?}"
        );
        assert!(
            kinds.contains(&"attempt_settled".to_string()),
            "and it must still settle its attempt: {kinds:?}"
        );
        let state = turn.into_state();
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call the model asked for was still logged");
        assert_eq!(
            call.ok,
            Some(false),
            "the call was not run and was explicitly answered as interrupted"
        );
        assert!(
            tool_result_text(call).contains("interrupted"),
            "the failed sibling result must say it was interrupted before execution"
        );
        assert_eq!(
            fx.store
                .events()
                .iter()
                .filter(|event| matches!(event.kind, EventKind::ToolCallCompleted { .. }))
                .count(),
            1,
            "the queued call receives exactly one explicit completion"
        );
    }

    #[test]
    fn a_refused_budget_stops_the_turn_without_starting_a_generation() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        // A gate too small to admit the upper bound of any request.
        let mut gate = BudgetGate::new(usd("0.0000001"));
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut gate),
            tools,
        );

        let outcome = block_on(turn.run("something expensive")).expect("the store accepted");

        assert_eq!(outcome.exit_reason, ExitReason::Budget);
        assert!(!outcome.ok);
        let kinds = fx.store.kinds();
        assert!(
            !kinds.contains(&"generation_started".to_string()),
            "nothing may reach the provider once the gate refuses: {kinds:?}"
        );
        assert_eq!(kinds, vec!["user_prompt", "error", "turn_completed"]);
        assert!(fx.generator.calls().is_empty());
    }

    #[test]
    fn a_provider_error_is_logged_and_the_attempt_still_settles() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Error {
                message: "the provider fell over".to_string(),
                kind: ErrorKind::Unclassified,
            },
            AiEvent::Stop(StopReason::Error),
        ]]);

        let (reason, ok, state) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Provider);
        assert!(!ok);
        assert_eq!(state.errors, vec!["the provider fell over".to_string()]);
        assert!(fx.store.kinds().contains(&"attempt_settled".to_string()));
    }

    /// Two calls in one generation claiming the same provider id.
    ///
    /// See the tool-use-history ADR §6. Nothing rejected this before: the
    /// openai decoder's `settle_identity` compares an incoming id only with
    /// the id held at the *same* index, and the loop mints a fresh `ItemId`
    /// per call while `SessionState` keys on `ItemId`, so two items carried
    /// one `call_id` without complaint. It surfaced two generations later as
    /// a **panic** — `Conversation::new` returning `DuplicateCallId` into an
    /// `expect` whose stated reason was about unanswered calls — by which
    /// point both tools had already run.
    ///
    /// So the assertion that matters is not the exit code, it is
    /// **`executions == 0`**. A check that merely stopped the panic would
    /// leave the double effect, which is survivable only while every tool is
    /// read-only.
    /// The loop's half of `POTENTIALS.md` P22, and the only place it can be
    /// proved: a hand-built log can assert what a *rebuild* does with a
    /// parent edge, but only a run can assert that the loop writes one. The
    /// history is rebuilt from the state the run produced, exactly as the
    /// next generation would rebuild it.
    ///
    /// Without the edge this is five turns: use, answer, use, answer — a
    /// conversation in which the model saw `/ws/a`'s contents before asking
    /// for `/ws/b`, which is not what it did.
    #[test]
    fn two_calls_from_one_generation_rebuild_as_the_turn_the_model_took() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/a"}"#.to_string(),
                },
                AiEvent::ToolCall {
                    call_id: "call_2".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/b"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (reason, ok, state) = run(&fx, SessionState::default(), "read both");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        let (conversation, repairs) =
            conversation_from(&state).expect("a history the loop just wrote rebuilds");
        assert!(repairs.is_empty(), "{repairs:?}");
        let shape: Vec<(Role, usize)> = conversation
            .messages()
            .iter()
            .map(|message| (message.role, message.content.len()))
            .collect();
        assert_eq!(
            shape,
            [(Role::User, 1), (Role::Assistant, 2), (Role::User, 2)],
            "one assistant turn made both calls, and one user turn answers both"
        );
    }

    #[test]
    fn duplicate_provider_call_ids_end_the_turn_before_any_tool_runs() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/a"}"#.to_string(),
                },
                AiEvent::ToolCall {
                    // The same id, a different call. Neither may be dispatched:
                    // once two calls share an id, a result cannot be attributed
                    // back to the call that earned it.
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/b"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            // Present and must go unused: reaching it would mean the loop
            // carried on past corruption.
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (reason, ok, state) = run(&fx, SessionState::default(), "read both");

        assert_eq!(
            reason,
            ExitReason::Provider,
            "a malformed response is the provider's fault, not ours: CLI.md §4.3 exit 4"
        );
        assert!(!ok);
        assert_eq!(
            fx.generator.calls().len(),
            1,
            "the turn must not generate again on a corrupt response"
        );
        let executions = state
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall && item.ok.is_some())
            .count();
        assert_eq!(
            executions, 0,
            "neither call may run: the panic this replaces fired only after both had"
        );
        assert!(
            state.errors.iter().any(|error| error.contains("call_1")),
            "the duplicated id has to be named, or nobody can diagnose it: {:?}",
            state.errors
        );
        assert!(
            fx.store.kinds().contains(&"attempt_settled".to_string()),
            "the attempt is already paid for and still has to settle: {:?}",
            fx.store.kinds()
        );
    }

    /// The case that decides *where* the guard lives. A decoder sees one
    /// response and can never catch this; only a check seeded from the
    /// reduction can. Emptying that seed leaves the test above passing, which
    /// is how this one came to be written.
    #[test]
    fn a_call_id_reused_by_a_later_generation_is_refused_too() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/a"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::ToolCall {
                    // Answered in the generation before this one, so a result
                    // naming it now would answer that call as readily as this
                    // one.
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/b"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (reason, ok, state) = run(&fx, SessionState::default(), "read a, then b");

        assert_eq!(reason, ExitReason::Provider);
        assert!(!ok);
        assert_eq!(
            fx.generator.calls().len(),
            2,
            "the third generation is never reached"
        );
        let executions = state
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall && item.ok.is_some())
            .count();
        assert_eq!(
            executions, 1,
            "the first generation's call had a valid id and stands; only the reuse is refused"
        );
        assert!(
            state.errors.iter().any(|error| error.contains("call_1")),
            "{:?}",
            state.errors
        );
    }

    /// The one classification that changes where the run lands. `CLI.md:215`
    /// tells a script to retry on exit 4; a request that will be wrong again
    /// is precisely what it must not retry, so an `Invalid` is exit 2.
    #[test]
    fn a_provider_error_the_request_caused_exits_as_usage_and_not_as_provider() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Error {
                message: "context length exceeded".to_string(),
                kind: ErrorKind::Invalid,
            },
            AiEvent::Stop(StopReason::Error),
        ]]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Usage);
        assert!(!ok);
    }

    /// The other three classifications land on exit 4, and this pins that as
    /// a decision rather than an accident — including `Unclassified`, which
    /// gets there by failing closed rather than by being read from anything.
    #[test]
    fn every_other_classification_still_exits_as_provider() {
        for kind in [
            ErrorKind::Transient,
            ErrorKind::Refused,
            ErrorKind::Unclassified,
        ] {
            let fx = Fixture::new(vec![vec![
                AiEvent::Error {
                    message: "upstream said no".to_string(),
                    kind,
                },
                AiEvent::Stop(StopReason::Error),
            ]]);

            let (reason, _, _) = run(&fx, SessionState::default(), "hello");
            assert_eq!(reason, ExitReason::Provider, "for {kind:?}");
        }
    }

    /// The last classification decides, because the terminal frame follows
    /// the error that caused it. An earlier error the stream recovered from
    /// does not describe how it ended.
    #[test]
    fn the_last_error_classification_is_the_one_that_decides_the_exit() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Error {
                message: "a blip".to_string(),
                kind: ErrorKind::Transient,
            },
            AiEvent::Error {
                message: "context length exceeded".to_string(),
                kind: ErrorKind::Invalid,
            },
            AiEvent::Stop(StopReason::Error),
        ]]);

        let (reason, _, state) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Usage);
        assert_eq!(
            state.errors.len(),
            2,
            "both are still journalled; only the exit is decided by the last"
        );
    }

    /// A classification is never consulted for a generation that ended some
    /// other way — otherwise a recovered-from error would rewrite the exit of
    /// a turn that finished cleanly.
    #[test]
    fn an_error_the_stream_recovered_from_does_not_change_a_clean_exit() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Error {
                message: "a blip".to_string(),
                kind: ErrorKind::Invalid,
            },
            AiEvent::TextDelta {
                text: "recovered".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
    }

    // --- reasoning state closes its block ----------------------------------

    /// The state is the block's close, so the next delta opens a new item.
    /// Anthropic documents a `400` when blocks are not replayed exactly, and
    /// two blocks merged into one is not an exact replay.
    #[test]
    fn reasoning_state_closes_its_block_so_the_next_delta_opens_another() {
        let fx = Fixture::new(vec![vec![
            AiEvent::ReasoningDelta {
                text: "first".to_string(),
            },
            AiEvent::ReasoningState(ReasoningState {
                blob: "sealed-one".to_string(),
                dialect: "anthropic-messages".to_string(),
            }),
            AiEvent::ReasoningDelta {
                text: "second".to_string(),
            },
            AiEvent::ReasoningState(ReasoningState {
                blob: "sealed-two".to_string(),
                dialect: "anthropic-messages".to_string(),
            }),
            AiEvent::Stop(StopReason::Stop),
        ]]);

        run(&fx, SessionState::default(), "hello");

        let items: Vec<_> = fx
            .store
            .events()
            .iter()
            .filter(|ev| {
                matches!(
                    ev.kind,
                    EventKind::ReasoningDelta { .. } | EventKind::ReasoningState { .. }
                )
            })
            .map(|ev| ev.env.item.expect("reasoning events carry an item"))
            .collect();

        assert_eq!(items.len(), 4);
        assert_eq!(items[0], items[1], "the state seals the block it closes");
        assert_eq!(items[2], items[3]);
        assert_ne!(
            items[0], items[2],
            "the second block must be a new item, not a continuation of the sealed one"
        );
    }

    /// A block whose `thinking` field is empty is the case Anthropic's `400`
    /// names explicitly, so state arriving with no delta before it still
    /// needs an item to hang on.
    #[test]
    fn reasoning_state_with_no_preceding_delta_still_gets_an_item() {
        let fx = Fixture::new(vec![vec![
            AiEvent::ReasoningState(ReasoningState {
                blob: "sealed-and-silent".to_string(),
                dialect: "anthropic-messages".to_string(),
            }),
            AiEvent::Stop(StopReason::Stop),
        ]]);

        run(&fx, SessionState::default(), "hello");

        let events = fx.store.events();
        let logged = events
            .iter()
            .find(|ev| matches!(ev.kind, EventKind::ReasoningState { .. }))
            .expect("the state is journalled even with no text before it");
        assert!(logged.env.item.is_some());
    }

    #[test]
    fn a_turn_whose_log_cannot_be_written_is_abandoned_with_the_state_intact() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "hi".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        // The third append is `assistant_delta`, in the middle of a live
        // generation — the case where an early return would drop an
        // unsettled attempt.
        fx.store.fail_at(3);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut gate),
            tools,
        );

        let abandoned = block_on(turn.run("hello")).expect_err("the store refused an append");

        assert_eq!(
            Some(abandoned.state.last_seq),
            counting_from_one(fx.store.events().len()).last().copied(),
            "the in-memory state must describe exactly what reached the log"
        );
        drop(turn);
        assert_eq!(
            gate.outstanding(),
            Some(&usd("0")),
            "an abandoned turn still settles the attempt it had in flight"
        );
    }

    /// The decisive property of the preflight, and the reason it sits where it
    /// sits: a reservation taken for a request the model will reject has
    /// authorised spending on something that was never going to be sent. So
    /// the gate must be untouched — `outstanding` and `committed` both zero —
    /// and the generator must never have been called.
    #[test]
    fn a_request_the_model_will_not_take_never_reaches_the_budget_gate() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "never sent".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let refusing = model_with_reasoning(
            &fx.generator,
            &mut gate,
            ReasoningEffortSupport::Only(BTreeSet::from([
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ])),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            refusing,
            tools,
        );

        let outcome = block_on(turn.run("hi")).expect("the store accepted every append");

        assert_eq!(outcome.exit_reason, ExitReason::Usage);
        assert!(!outcome.ok);
        drop(turn);
        assert_eq!(
            gate.outstanding(),
            Some(&usd("0")),
            "the gate reserved anyway"
        );
        assert_eq!(gate.committed(), Some(&usd("0")), "the gate settled anyway");
        assert!(
            fx.generator.calls().is_empty(),
            "the request was sent to the provider anyway"
        );
    }

    /// A usage error is exit 2, and the log says which levels would have
    /// worked — the summary line in `RunTermination` deliberately does not.
    #[test]
    fn a_refused_level_is_named_in_the_log_with_the_levels_that_would_have_worked() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let refusing = model_with_reasoning(
            &fx.generator,
            &mut gate,
            ReasoningEffortSupport::Only(BTreeSet::from([ThinkingLevel::High])),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            refusing,
            tools,
        );

        let outcome = block_on(turn.run("hi")).expect("the store accepted every append");

        assert_eq!(
            RunTermination::after_turn(outcome.exit_reason, outcome.denied)
                .expect("a usage error ends the run")
                .exit_code(),
            2
        );
        drop(turn);
        let message = fx
            .store
            .events()
            .iter()
            .find_map(|ev| match &ev.kind {
                EventKind::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("the refusal is durable, or nobody can diagnose the run from the log");
        assert!(message.contains("Off"), "{message}");
        assert!(message.contains("High"), "{message}");
    }

    /// A model that states nothing is refused for a different reason, and the
    /// message says so: re-resolve the model, do not pick another level.
    #[test]
    fn a_model_that_stated_nothing_is_refused_before_it_is_sent_anything() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let silent =
            model_with_reasoning(&fx.generator, &mut gate, ReasoningEffortSupport::Unknown);
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            silent,
            tools,
        );

        let outcome = block_on(turn.run("hi")).expect("the store accepted every append");

        assert_eq!(outcome.exit_reason, ExitReason::Usage);
        drop(turn);
        assert!(fx.generator.calls().is_empty());
    }

    #[test]
    fn the_public_stream_is_what_the_caller_gets_back() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "hello".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut gate),
            tools,
        );

        block_on(turn.run("hi")).expect("the store accepted");

        let published = fx.sink.events();
        assert!(
            !published.is_empty(),
            "a turn that wrote a log published something"
        );
        // `GenerationStarted` and `AttemptSettled` have no public spelling:
        // the attempt lifecycle is internal accounting, not a host's business.
        // `TurnStarted` and `MeteringSelected` go the other way — both are
        // published and neither is appended, so the sink carries two more
        // events than the log does public ones.
        assert_eq!(
            published.len(),
            fx.store.events().len() - 2 + 2,
            "exactly the two attempt-lifecycle events stay internal, and \
             turn.started and metering.selected are published without being \
             logged"
        );
    }

    #[test]
    fn the_first_request_carries_the_prompt_and_nothing_else() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        run(&fx, SessionState::default(), "the only thing said so far");

        let first = &fx.generator.calls()[0];
        let messages = first.messages.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(
            messages[0].content,
            vec![ContentBlock::Text {
                text: "the only thing said so far".to_string()
            }]
        );
    }

    // --- the sink is live, and it is fed only what is durable ---

    #[test]
    fn an_event_reaches_the_sink_before_the_turn_that_produced_it_ends() {
        // `CLI.md` §4.2: "one event per line, **live**". The turn below never
        // ends, so anything the sink holds got there while it was running.
        let fx = Fixture::new(Vec::new());
        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "thinking out loud".to_string(),
            }]),
        });
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&generator, &mut gate),
            tools,
        );

        let mut running = Box::pin(turn.run("say something"));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            running.as_mut().poll(&mut cx).is_pending(),
            "the generator never finishes, so neither does the turn"
        );

        let seen = fx.sink.events();
        assert!(
            seen.iter().any(|event| matches!(
                event,
                PublicEvent::ItemDelta { text, .. } if text == "thinking out loud"
            )),
            "the delta was published while the turn was still running, got {seen:?}"
        );
    }

    #[test]
    fn a_turn_announces_itself_before_it_says_anything_else() {
        // `turn.started` has no `EventKind`, so before the sink existed
        // nothing in the workspace could emit it and §4.2's envelope was
        // missing a level.
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "hello".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let (_, _, _) = run(&fx, SessionState::default(), "hi");

        let seen = fx.sink.events();
        match seen.first() {
            Some(PublicEvent::TurnStarted { turn }) => assert!(
                seen.iter().skip(1).any(|event| matches!(
                    event,
                    PublicEvent::TurnCompleted { turn: ended, .. } if ended == turn
                )),
                "the turn that announced itself is the one that finished"
            ),
            other => panic!("a turn announces itself first, got {other:?}"),
        }
    }

    #[test]
    fn an_event_the_store_refused_never_reaches_the_sink() {
        // The ordering is the guarantee: a host told about an event no reader
        // will find is being told about a turn nobody can resume into.
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "never written".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        fx.store.fail_at(1);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            &fx.policy,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                &NO_RESOLVER,
                &NO_HOP,
                &NO_DUPLEX,
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::new(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&fx.generator, &mut gate),
            tools,
        );
        block_on(turn.run("hello")).expect_err("the store refused the first append");

        // `TurnStarted` is published and never appended — it has no log entry
        // to be refused — so it is the only thing a sink may hold here.
        assert!(
            fx.sink
                .events()
                .iter()
                .all(|event| matches!(event, PublicEvent::TurnStarted { .. })),
            "nothing the store refused was published: {:?}",
            fx.sink.events()
        );
    }

    // --- how this generation is metered, said out loud ---
    //
    // `docs/decisions/2026-08-21-how-a-model-is-metered.md` §6. Three of the
    // four arrangements make a monetary `--budget` inert, and an operator who
    // set one has to be told that before the run rather than inferring it
    // from a limit that never fired.

    fn metering_events(fx: &Fixture) -> Vec<PublicEvent> {
        fx.sink
            .events()
            .into_iter()
            .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
            .collect()
    }

    fn subscription() -> Metering {
        Metering::Subscription {
            revision: MeteringRevision("acme/pro-plan/2026-08".to_string()),
        }
    }

    fn provider_reported() -> Metering {
        Metering::Currency {
            settlement: SettlementSource::ProviderReported {
                revision: ProviderCostRevision {
                    provider: "xai".to_string(),
                    field: "usage.cost_in_usd_ticks".to_string(),
                    schema_revision: "2026-08-21".to_string(),
                },
            },
        }
    }

    #[test]
    fn every_generation_says_how_it_is_metered_before_the_gate_is_asked() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        run(&fx, SessionState::default(), "hi");

        assert_eq!(
            metering_events(&fx),
            vec![PublicEvent::MeteringSelected {
                turn: match fx.sink.events().first() {
                    Some(PublicEvent::TurnStarted { turn }) => *turn,
                    other => panic!("a turn starts before it is metered: {other:?}"),
                },
                metering: ego_core::public::MeteringKind::Currency,
                ceiling: ego_core::public::CeilingEffect::Binds {
                    limit: usd("1000"),
                    clamped_from: None,
                },
                rates: RateSource::Operator,
            }]
        );

        // Before the gate is asked, and the falsifiable form of that is a
        // gate that refuses: an operator whose `--budget` denies the very
        // first generation still has to be told which metering was about to
        // be charged, or the denial is unreadable.
        let denied = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        *denied.gate.borrow_mut() = BudgetGate::new(usd("0.01"));
        let (exit, ok, _) = run(&denied, SessionState::default(), "hi");
        assert_eq!(exit, ExitReason::Budget);
        assert!(!ok, "a cent cannot admit this generation");
        assert_eq!(
            metering_events(&denied).len(),
            1,
            "a refused generation was still metered: {:?}",
            denied.sink.events()
        );
        assert!(
            denied.generator.calls().is_empty(),
            "and the provider was never reached, so this is genuinely pre-gate"
        );
    }

    /// The token ceiling, at the boundary §9 puts it on: between two
    /// generations of the *same* turn, not between turns.
    ///
    /// The tool call is what makes the distinction observable. A turn that
    /// calls a tool generates twice; a ceiling checked only between turns
    /// would let the second generation run and stop the run afterwards,
    /// having spent the tokens it was there to stop. Here the first
    /// generation's usage crosses the ceiling and the second never happens.
    #[test]
    fn a_token_ceiling_stops_the_next_generation_of_the_turn_it_is_in() {
        let script = || {
            vec![
                vec![
                    AiEvent::ToolCall {
                        call_id: "call-1".to_string(),
                        name: "read".to_string(),
                        args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                    },
                    AiEvent::Usage(tokens(900, 0, 0, 200)),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![AiEvent::Stop(StopReason::Stop)],
            ]
        };

        // 1_100 billed tokens against a 1_000 ceiling: reached after the
        // first generation, so the second is refused.
        let capped = Fixture::new(script()).with_read_tool();
        capped.fs.file("/ws/note.txt", "hello from the file");
        let (exit, ok, _) = run_with(
            &capped,
            SessionState::default(),
            "what is in note.txt?",
            metering(),
            Some(rates()),
            Some(1_000),
            config(),
        );

        assert_eq!(exit, ExitReason::Budget);
        assert!(!ok);
        assert_eq!(
            capped.generator.calls().len(),
            1,
            "the second generation ran despite the ceiling"
        );

        // The same script with no ceiling generates twice, so the assertion
        // above is about the ceiling and not about the script.
        let uncapped = Fixture::new(script()).with_read_tool();
        uncapped.fs.file("/ws/note.txt", "hello from the file");
        let (exit, ok, _) = run(&uncapped, SessionState::default(), "what is in note.txt?");
        assert_eq!(exit, ExitReason::Completed);
        assert!(ok);
        assert_eq!(uncapped.generator.calls().len(), 2);
    }

    /// It binds a subscription row exactly as it binds a metered one. That is
    /// §9's second property: the token ceiling needs no rates, no fetch and
    /// no network, so it is the one instrument that works on the two
    /// arrangements a monetary ceiling cannot touch.
    #[test]
    fn a_token_ceiling_binds_a_row_no_money_ceiling_could() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(900, 0, 0, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]]);
        *fx.gate.borrow_mut() =
            BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));

        // The first turn spends 1_100 tokens against a 1_000 ceiling, and
        // completes: the ceiling is checked before a generation, not after.
        let (exit, ok, state) = run_with(
            &fx,
            SessionState::default(),
            "hi",
            subscription(),
            None,
            Some(1_000),
            config(),
        );
        assert_eq!(exit, ExitReason::Completed);
        assert!(ok);

        // The next one is refused before it reaches the provider, on a row
        // that reserves zero and could never be stopped by `--budget 5$`.
        let (exit, ok, _) = run_with(
            &fx,
            state,
            "and again",
            subscription(),
            None,
            Some(1_000),
            config(),
        );
        assert_eq!(exit, ExitReason::Budget);
        assert!(!ok);
        assert_eq!(
            fx.generator.calls().len(),
            1,
            "a flat-fee row was generated past its token ceiling"
        );
        assert_eq!(
            RunTermination::after_turn(exit, None),
            Some(RunTermination::BudgetExceeded),
            "and it is exit 5, the same code the money ceiling ends on"
        );
    }

    /// The ordinary case, end to end: nobody configured a ceiling, so the
    /// run says so and runs anyway.
    ///
    /// This is §1's whole claim reduced to something falsifiable. The turn
    /// has no rates at all — the state that used to be a refusal — and it
    /// completes, publishing `ceiling: none` and `rates: none` rather than a
    /// limit that is not there or a provenance for figures nobody has.
    #[test]
    fn a_run_with_no_ceiling_and_no_rates_completes_and_says_so() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(1_000, 0, 0, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]]);
        *fx.gate.borrow_mut() =
            BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));

        let (exit, ok, _) = run_with(
            &fx,
            SessionState::default(),
            "hi",
            metering(),
            None,
            None,
            config(),
        );

        assert_eq!(exit, ExitReason::Completed);
        assert!(ok, "a run nobody asked to bound is not refused for a price");
        assert_eq!(
            metering_events(&fx),
            vec![PublicEvent::MeteringSelected {
                turn: match fx.sink.events().first() {
                    Some(PublicEvent::TurnStarted { turn }) => *turn,
                    other => panic!("a turn starts before it is metered: {other:?}"),
                },
                metering: ego_core::public::MeteringKind::Currency,
                ceiling: ego_core::public::CeilingEffect::None,
                rates: RateSource::None,
            }],
            "no ceiling and no rates are each announced as the absences they are"
        );
        assert_eq!(
            fx.generator.calls().len(),
            1,
            "and the provider was actually reached"
        );
    }

    /// The three arrangements a monetary budget cannot bind, each said in its
    /// own words. Without this the variants would be constructible and
    /// invisible — an operator running a subscription model under `--budget
    /// $5` would see a limit that silently never applies. §10 says that
    /// outright: the ceiling on a flat-fee row is announced `inapplicable`,
    /// naming the limit that will never fire and the metering that defeats
    /// it.
    #[test]
    fn each_metering_is_announced_as_the_thing_it_actually_is() {
        use ego_core::public::{CeilingEffect, MeteringKind};

        for (metering, expected_kind, expected_ceiling) in [
            (
                subscription(),
                MeteringKind::Subscription,
                CeilingEffect::Inapplicable {
                    limit: usd("1000"),
                    metering: MeteringKind::Subscription,
                },
            ),
            (
                Metering::Unmetered {
                    revision: MeteringRevision("ego/builtin/local/v1".to_string()),
                },
                MeteringKind::Unmetered,
                CeilingEffect::Inapplicable {
                    limit: usd("1000"),
                    metering: MeteringKind::Unmetered,
                },
            ),
            (
                provider_reported(),
                MeteringKind::Currency,
                CeilingEffect::Binds {
                    limit: usd("1000"),
                    clamped_from: None,
                },
            ),
        ] {
            let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
            run_metered(&fx, SessionState::default(), "hi", metering.clone());

            let announced = metering_events(&fx);
            let [
                PublicEvent::MeteringSelected {
                    metering: kind,
                    ceiling,
                    rates,
                    ..
                },
            ] = announced.as_slice()
            else {
                panic!("exactly one metering announcement per generation: {announced:?}");
            };
            assert_eq!(*kind, expected_kind, "for {metering:?}");
            assert_eq!(*ceiling, expected_ceiling, "for {metering:?}");
            assert_eq!(
                *rates,
                RateSource::Operator,
                "every fixture run resolves its rates from the operator's file, \
                 whatever the arrangement: for {metering:?}"
            );
        }
    }

    /// A subscription reserves nothing, so a monetary limit smaller than any
    /// currency bound still admits the generation. This is the gate actually
    /// behaving differently, not just a label on a log line.
    #[test]
    fn a_subscription_generation_is_admitted_by_a_budget_no_currency_row_could_pass() {
        let script = || {
            vec![vec![
                AiEvent::Usage(tokens(1_000, 0, 0, 200)),
                AiEvent::Stop(StopReason::Stop),
            ]]
        };

        // A cent could not cover the $0.76536 worst case of the currency row.
        let currency = Fixture::new(script());
        *currency.gate.borrow_mut() = BudgetGate::new(usd("0.01"));
        let (exit, ok, _) = run(&currency, SessionState::default(), "hi");
        assert_eq!(exit, ExitReason::Budget);
        assert!(!ok, "a cent cannot admit a currency-metered generation");

        let flat = Fixture::new(script());
        *flat.gate.borrow_mut() = BudgetGate::new(usd("0.01"));
        let (exit, ok, _) = run_metered(&flat, SessionState::default(), "hi", subscription());
        assert_eq!(exit, ExitReason::Completed);
        assert!(ok, "a flat fee is not spent per request");
        assert!(
            flat.gate
                .borrow()
                .committed()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );
    }

    /// And the zero it settles at is `Known`, attributed to the policy.
    /// `Unknown` would poison the session's cumulative total for a figure
    /// nobody doubts, and a `PricingRevision` here would be a price list that
    /// does not exist.
    #[test]
    fn a_subscription_settles_at_a_known_zero_attributed_to_its_policy() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(1_000, 0, 0, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]]);
        run_metered(&fx, SessionState::default(), "hi", subscription());

        let booked: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                EventKind::UsageUpdated { cost_delta, .. } => Some(cost_delta),
                _ => None,
            })
            .collect();
        assert_eq!(
            booked,
            vec![CostDelta::Known {
                amount: usd("0"),
                attribution: CostAttribution::MeteringPolicy {
                    revision: MeteringRevision("acme/pro-plan/2026-08".to_string()),
                },
                fx: None,
            }]
        );
    }

    /// Under provider-reported settlement the log books the provider's own
    /// figure, and the usage frame no longer decides the money.
    #[test]
    fn a_stated_cost_settles_the_generation_and_the_table_does_not() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(1_000, 0, 0, 200)),
            AiEvent::ProviderCost {
                cost: usd("0.0012345678"),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        run_metered(&fx, SessionState::default(), "hi", provider_reported());

        let booked: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                EventKind::UsageUpdated {
                    usage, cost_delta, ..
                } => Some((usage, cost_delta)),
                _ => None,
            })
            .collect();
        let [(usage, cost_delta)] = booked.as_slice() else {
            panic!("one usage update, holding both halves: {booked:?}");
        };
        assert_eq!(
            *usage,
            tokens(1_000, 0, 0, 200),
            "the tokens are still recorded; it is the money that changed hands"
        );
        assert_eq!(
            *cost_delta,
            CostDelta::Known {
                amount: usd("0.0012345678"),
                attribution: CostAttribution::ProviderReported(ProviderCostRevision {
                    provider: "xai".to_string(),
                    field: "usage.cost_in_usd_ticks".to_string(),
                    schema_revision: "2026-08-21".to_string(),
                }),
                fx: None,
            }
        );
        // 1000 x 0.000003 + 200 x 0.000015 = 0.006, which is what the table
        // would have said. It is three times the stated figure, so a silent
        // fallback to the quote could not hide here.
        assert_ne!(
            *cost_delta,
            quote().price(&tokens(1_000, 0, 0, 200)),
            "a stated cost is not the table's answer"
        );
        assert_eq!(fx.gate.borrow().committed(), Some(&usd("0.0012345678")));
    }

    /// The cost may arrive before the usage or after it — SSE gives no
    /// guarantee — and one `UsageUpdated` has to come out either way. Booking
    /// per event would produce two, double-counting the tokens.
    #[test]
    fn a_stated_cost_and_a_usage_frame_are_booked_once_in_either_order() {
        let cost = AiEvent::ProviderCost {
            cost: usd("0.0012345678"),
        };
        let usage = AiEvent::Usage(tokens(1_000, 0, 0, 200));
        let mut booked = Vec::new();
        for script in [
            vec![usage.clone(), cost.clone(), AiEvent::Stop(StopReason::Stop)],
            vec![cost.clone(), usage.clone(), AiEvent::Stop(StopReason::Stop)],
        ] {
            let fx = Fixture::new(vec![script]);
            run_metered(&fx, SessionState::default(), "hi", provider_reported());
            booked.push(
                fx.store
                    .events()
                    .into_iter()
                    .filter_map(|ev| match ev.kind {
                        EventKind::UsageUpdated {
                            usage, cost_delta, ..
                        } => Some((usage, cost_delta)),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(booked[0].len(), 1, "one booking, not one per event");
        assert_eq!(booked[0], booked[1], "the wire order is not the record");
    }

    /// A provider that declared it states a cost and then did not: the money
    /// is unknown, named as *that* silence rather than the usage one, and the
    /// gate charges the full reserved bound.
    #[test]
    fn a_missing_stated_cost_settles_at_the_bound_and_not_at_zero() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(1_000, 0, 0, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]]);
        run_metered(&fx, SessionState::default(), "hi", provider_reported());

        let booked: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                EventKind::UsageUpdated { cost_delta, .. } => Some(cost_delta),
                _ => None,
            })
            .collect();
        assert_eq!(
            booked,
            vec![CostDelta::Unknown(UnknownCost::ProviderReportedCostMissing)],
            "the tokens arrived, so `ProviderReportedNoUsage` would name the wrong silence"
        );
        assert_eq!(
            fx.gate.borrow().committed(),
            Some(&usd("0.76536")),
            "an unknown cost charges the bound, never zero"
        );
    }

    mod generation_boundary {
        use super::*;
        use ego_core::generation_model_source::{
            GenerationModelError, GenerationModelErrorClass, GenerationModelSource,
            GenerationProfile, GenerationRole,
        };

        enum ResolutionStep {
            Ready(Box<Result<GenerationProfile, GenerationModelError>>),
            Pending,
        }

        struct ScriptedGenerationSource {
            steps: Mutex<VecDeque<ResolutionStep>>,
            resolved: Mutex<Vec<GenerationRole>>,
            ready: Option<Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
            trace: Option<GenerationTrace>,
        }

        #[derive(Clone, Default)]
        struct GenerationTrace {
            requests: Arc<Mutex<Vec<GenRequest>>>,
            steps: Arc<Mutex<Vec<String>>>,
        }

        struct RecordingGenerator {
            inner: FakeGenerator,
            trace: GenerationTrace,
            expressibility: Result<(), String>,
        }

        impl Generator for RecordingGenerator {
            fn ensure_expressible(
                &self,
                _request: &GenRequest,
            ) -> Result<(), RequestExpressibilityError> {
                match &self.expressibility {
                    Ok(()) => Ok(()),
                    Err(message) => Err(RequestExpressibilityError::new(message.clone())),
                }
            }

            fn generate(&self, request: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
                self.trace.requests.lock().unwrap().push(request.clone());
                self.trace
                    .steps
                    .lock()
                    .unwrap()
                    .push(format!("generate:{}", request.model));
                self.inner.generate(request, attempt)
            }
        }

        impl ScriptedGenerationSource {
            fn new(
                profiles: impl IntoIterator<Item = Result<GenerationProfile, GenerationModelError>>,
            ) -> Self {
                Self {
                    steps: Mutex::new(
                        profiles
                            .into_iter()
                            .map(|profile| ResolutionStep::Ready(Box::new(profile)))
                            .collect(),
                    ),
                    resolved: Mutex::new(Vec::new()),
                    ready: None,
                    trace: None,
                }
            }

            fn pending_with_signal_at(
                mut self,
                index: usize,
            ) -> (Self, futures::channel::oneshot::Receiver<()>) {
                let (sender, receiver) = futures::channel::oneshot::channel();
                let mut steps = self.steps.lock().unwrap();
                assert!(index <= steps.len());
                steps.insert(index, ResolutionStep::Pending);
                drop(steps);
                self.ready = Some(Mutex::new(Some(sender)));
                (self, receiver)
            }

            fn pending_with_signal(self) -> (Self, futures::channel::oneshot::Receiver<()>) {
                self.pending_with_signal_at(0)
            }

            fn with_trace(mut self, trace: GenerationTrace) -> Self {
                self.trace = Some(trace);
                self
            }

            fn roles(&self) -> Vec<GenerationRole> {
                self.resolved.lock().unwrap().clone()
            }
        }

        fn profile_with_settings(
            slug: &str,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            output_ceiling: Option<u64>,
            context_ceiling: Option<u64>,
            thinking: ThinkingLevel,
            parallel_tool_calls: bool,
        ) -> GenerationProfile {
            profile_with_expressibility(
                slug,
                script,
                trace,
                ProfileSettings {
                    output_ceiling,
                    context_ceiling,
                    thinking,
                    parallel_tool_calls,
                },
                Ok(()),
            )
        }

        struct ProfileSettings {
            output_ceiling: Option<u64>,
            context_ceiling: Option<u64>,
            thinking: ThinkingLevel,
            parallel_tool_calls: bool,
        }

        fn profile_with_expressibility(
            slug: &str,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            settings: ProfileSettings,
            expressibility: Result<(), String>,
        ) -> GenerationProfile {
            let mut capabilities = caps();
            capabilities.max_context_tokens = settings.context_ceiling;
            capabilities.max_output_tokens = settings.output_ceiling;
            capabilities.supports_parallel_tool_calls = settings.parallel_tool_calls;
            let profile = GenerationProfile {
                identity: ego_core::config::ModelRef(slug.to_string()),
                request_model: slug.split_once('/').unwrap().1.to_string(),
                thinking: settings.thinking,
                generator: Arc::new(RecordingGenerator {
                    inner: FakeGenerator::new(vec![script]),
                    trace: trace.clone(),
                    expressibility,
                }),
                capabilities,
                metering: metering(),
                rates: Some(rates()),
            };
            assert_eq!(
                profile.output_ceiling(),
                settings.output_ceiling.or(settings.context_ceiling)
            );
            profile
        }

        fn refusing_profile(
            slug: &str,
            judge: bool,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            output_ceiling: Option<u64>,
        ) -> GenerationProfile {
            profile_with_expressibility(
                slug,
                script,
                trace,
                ProfileSettings {
                    output_ceiling,
                    context_ceiling: Some(8_192),
                    thinking: if judge {
                        ThinkingLevel::Off
                    } else {
                        ThinkingLevel::High
                    },
                    parallel_tool_calls: false,
                },
                Err("the selected wire cannot encode this request".into()),
            )
        }

        fn profile(
            slug: &str,
            judge: bool,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            output_ceiling: Option<u64>,
        ) -> GenerationProfile {
            profile_with_settings(
                slug,
                script,
                trace,
                output_ceiling,
                Some(8_192),
                if judge || output_ceiling == Some(1_024) {
                    ThinkingLevel::Off
                } else {
                    ThinkingLevel::High
                },
                output_ceiling == Some(2_048),
            )
        }

        fn judge_context() -> JudgeContext {
            JudgeContext {
                confinement: RunConfinement::new(
                    Confinement::Workspace,
                    false,
                    CanonPath::new("/ws"),
                )
                .unwrap(),
                exec_caps: ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
            }
        }

        impl GenerationModelSource for ScriptedGenerationSource {
            fn resolve<'a>(
                &'a self,
                role: GenerationRole,
            ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
                self.resolved.lock().unwrap().push(role);
                if let Some(trace) = &self.trace {
                    trace
                        .steps
                        .lock()
                        .unwrap()
                        .push(format!("resolve:{role:?}"));
                }
                let step = self
                    .steps
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("the scripted source has one step per boundary");
                if matches!(step, ResolutionStep::Pending) {
                    if let Some(ready) = &self.ready
                        && let Some(sender) = ready.lock().unwrap().take()
                    {
                        let _ = sender.send(());
                    }
                    return Box::pin(futures::future::pending());
                }
                let ResolutionStep::Ready(result) = step else {
                    unreachable!("pending steps return above");
                };
                Box::pin(futures::future::ready(*result))
            }
        }

        #[test]
        fn main_read_tool_main_resolves_fresh_profiles_with_current_caps() {
            let fx = Fixture::new(vec![
                vec![
                    AiEvent::TextDelta {
                        text: "before".into(),
                    },
                    AiEvent::ToolCall {
                        call_id: "call-1".into(),
                        name: "read".into(),
                        args: r#"{"path":"/ws/notes.md"}"#.into(),
                    },
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![
                    AiEvent::TextDelta {
                        text: "after".into(),
                    },
                    AiEvent::Stop(StopReason::Stop),
                ],
            ])
            .with_read_tool();
            fx.fs.file("/ws/notes.md", "seeded notes");
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        "openai/first",
                        false,
                        vec![
                            AiEvent::TextDelta {
                                text: "before".into(),
                            },
                            AiEvent::ToolCall {
                                call_id: "call-1".into(),
                                name: "read".into(),
                                args: r#"{"path":"/ws/notes.md"}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ],
                        &trace,
                        Some(2_048),
                    )),
                    Ok(profile_with_settings(
                        "openai/second",
                        vec![
                            AiEvent::TextDelta {
                                text: "after".into(),
                            },
                            AiEvent::Stop(StopReason::Stop),
                        ],
                        &trace,
                        None,
                        Some(4_096),
                        ThinkingLevel::Off,
                        false,
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let mut gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            );
            block_on(turn.run("read this")).expect("the scripted turn succeeds");
            let names = fx.store.kinds();
            assert_eq!(
                names
                    .iter()
                    .filter(|name| *name == "generation_started")
                    .count(),
                2
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                2
            );
            assert_eq!(
                source.roles(),
                vec![GenerationRole::Main, GenerationRole::Main]
            );
            let requests = trace.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.model.as_str())
                    .collect::<Vec<_>>(),
                ["first", "second"]
            );
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.max_output_tokens)
                    .collect::<Vec<_>>(),
                [Some(2_048), Some(4_096)]
            );
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.thinking)
                    .collect::<Vec<_>>(),
                [ThinkingLevel::High, ThinkingLevel::Off]
            );
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.parallel_tool_calls)
                    .collect::<Vec<_>>(),
                [true, false]
            );
            assert_eq!(
                trace.steps.lock().unwrap().as_slice(),
                [
                    "resolve:Main",
                    "generate:first",
                    "resolve:Main",
                    "generate:second"
                ]
            );
        }

        #[test]
        fn auto_write_judge_allow_resolves_fresh_judge_and_executes_the_prepared_write() {
            let mut fx = Fixture::new(vec![]).with_write_tool();
            fx.policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Auto, Ruleset::default());
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        "openai/main",
                        false,
                        vec![
                            AiEvent::ToolCall {
                                call_id: "call-write".into(),
                                name: "fake_write".into(),
                                args: r#"{"path":"/ws/out","contents":"ok"}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ],
                        &trace,
                        Some(2_048),
                    )),
                    Ok(profile(
                        "openai/judge",
                        true,
                        vec![
                            AiEvent::TextDelta {
                                text: r#"{"verdict":"allow","reason":"bounded"}"#.into(),
                            },
                            AiEvent::Stop(StopReason::Stop),
                        ],
                        &trace,
                        None,
                    )),
                    Ok(profile(
                        "openai/main",
                        false,
                        vec![AiEvent::Stop(StopReason::Stop)],
                        &trace,
                        Some(1_024),
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let confinement =
                RunConfinement::new(Confinement::Workspace, false, CanonPath::new("/ws")).unwrap();
            let context = JudgeContext {
                confinement,
                exec_caps: ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
            };
            let mut gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            )
            .with_judge(context);
            block_on(turn.run("write this")).expect("auto judge allow completes the call");
            assert_eq!(
                source.roles(),
                vec![
                    GenerationRole::Main,
                    GenerationRole::Judge,
                    GenerationRole::Main
                ]
            );
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                3
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                3
            );
            let requests = trace.requests.lock().unwrap();
            assert_eq!(requests[1].model, "judge");
            assert_eq!(requests[1].thinking, ThinkingLevel::Off);
            assert!(requests[1].tools.is_empty());
            assert!(!requests[1].parallel_tool_calls);
            assert_eq!(requests[1].max_output_tokens, Some(8_192));
            assert_eq!(
                trace.steps.lock().unwrap().as_slice(),
                [
                    "resolve:Main",
                    "generate:main",
                    "resolve:Judge",
                    "generate:judge",
                    "resolve:Main",
                    "generate:main",
                ]
            );
            assert_eq!(fx.fs.read(&CanonPath::new("/ws/out")).unwrap(), b"ok");
            let events = fx.store.events();
            let judge_attempt = events
                .iter()
                .find_map(|event| match &event.kind {
                    EventKind::GenerationStarted {
                        attempt,
                        role: GenerationRole::Judge,
                    } => Some(*attempt),
                    _ => None,
                })
                .expect("Judge generation is durable");
            assert!(events.iter().any(|event| matches!(
                &event.kind,
                EventKind::PermissionDecided {
                    allowed: Some(true),
                    by: Some(Authority::Judge { model, attempt }),
                    ..
                } if model == &ego_core::config::ModelRef("openai/judge".into()) && attempt == &judge_attempt
            )));
            assert!(fx.store.kinds().iter().any(|kind| kind.contains("tool")));
        }

        #[test]
        fn judge_context_without_unresolved_call_does_not_resolve_or_reserve() {
            let fx = Fixture::new(vec![vec![
                AiEvent::TextDelta {
                    text: "done".into(),
                },
                AiEvent::Stop(StopReason::Stop),
            ]]);
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([Ok(profile(
                    "openai/main",
                    false,
                    vec![
                        AiEvent::TextDelta {
                            text: "done".into(),
                        },
                        AiEvent::Stop(StopReason::Stop),
                    ],
                    &trace,
                    Some(2_048),
                ))])
                .with_trace(trace.clone()),
            );
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut gate = fx.gate.borrow_mut();
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            )
            .with_judge(judge_context());
            block_on(turn.run("hello")).expect("the scripted turn succeeds");
            drop(turn);
            assert_eq!(source.roles(), vec![GenerationRole::Main]);
            assert!(!fx.store.kinds().iter().any(|kind| kind == "judge_resolved"));
            assert!(!fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    ..
                }
            )));
        }

        #[test]
        fn main_resolution_classes_refuse_without_start_or_reservation() {
            for (class, expected) in [
                (GenerationModelErrorClass::Usage, ExitReason::Usage),
                (GenerationModelErrorClass::Provider, ExitReason::Provider),
                (GenerationModelErrorClass::Internal, ExitReason::Internal),
            ] {
                let fx = Fixture::new(vec![]);
                let source = Arc::new(ScriptedGenerationSource::new([Err(
                    GenerationModelError::new(class, "resolution failed"),
                )]));
                let tools = Tools::new(
                    &fx.registry,
                    &fx.policy,
                    ToolEffectPorts::new(
                        &fx.fs,
                        fx.process.as_ref(),
                        &fx.evidence,
                        &NO_RESOLVER,
                        &NO_HOP,
                        &NO_DUPLEX,
                    ),
                    CanonPath::new("/ws"),
                );
                let mut gate = fx.gate.borrow_mut();
                let mut turn = TurnLoop::new(
                    session(
                        SessionState::default(),
                        &fx.store,
                        &fx.clock,
                        &fx.sink,
                        CancelToken::new(),
                    ),
                    GenerationRuntime {
                        source: source.clone(),
                        gate: &mut gate,
                        config: TurnConfig {
                            system: None,
                            token_ceiling: None,
                        },
                    },
                    tools,
                );
                let outcome = block_on(turn.run("hello")).expect("refusal is durable");
                assert_eq!(outcome.exit_reason, expected);
                assert!(
                    !fx.store
                        .kinds()
                        .iter()
                        .any(|kind| kind == "generation_started")
                );
                assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
                assert_eq!(gate.outstanding(), Some(&usd("0")));
                assert_eq!(gate.committed(), Some(&usd("0")));
            }
        }

        #[test]
        fn main_expressibility_refusal_is_durable_before_metering_or_attempt() {
            let fx = Fixture::new(vec![]);
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([Ok(refusing_profile(
                    "openai/main",
                    false,
                    Vec::new(),
                    &trace,
                    Some(2_048),
                ))])
                .with_trace(trace.clone()),
            );
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut gate = fx.gate.borrow_mut();
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            );
            let outcome = block_on(turn.run("hello")).expect("refusal is durable");
            assert_eq!(outcome.exit_reason, ExitReason::Usage);
            drop(turn);
            assert_eq!(source.roles(), vec![GenerationRole::Main]);
            assert!(trace.requests.lock().unwrap().is_empty());
            assert!(
                !fx.store
                    .kinds()
                    .iter()
                    .any(|kind| kind == "generation_started")
            );
            assert!(
                !fx.sink
                    .events()
                    .iter()
                    .any(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
            );
            assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
            assert_eq!(gate.outstanding(), Some(&usd("0")));
            assert_eq!(gate.committed(), Some(&usd("0")));
        }

        #[test]
        fn judge_expressibility_refusal_escalates_after_only_main_attempt() {
            let mut fx = Fixture::new(vec![]).with_write_tool();
            fx.policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Auto, Ruleset::default());
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        "openai/main",
                        false,
                        vec![
                            AiEvent::ToolCall {
                                call_id: "write".into(),
                                name: "fake_write".into(),
                                args: r#"{"path":"/ws/out","contents":"ok"}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ],
                        &trace,
                        Some(2_048),
                    )),
                    Ok(refusing_profile(
                        "openai/judge",
                        true,
                        Vec::new(),
                        &trace,
                        None,
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let mut gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            )
            .with_judge(judge_context());
            let outcome = block_on(turn.run("write")).expect("judge refusal is durable");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            drop(turn);
            assert_eq!(
                source.roles(),
                vec![GenerationRole::Main, GenerationRole::Judge]
            );
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                1
            );
            assert_eq!(
                trace
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|request| request.model.as_str())
                    .collect::<Vec<_>>(),
                ["main"]
            );
            assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
            assert!(!fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    ..
                }
            )));
            assert_eq!(gate.outstanding(), Some(&usd("0")));
            assert_eq!(gate.committed(), Some(&usd("0.06144")));
        }

        #[test]
        fn controlled_cancellation_before_resolution_creates_no_attempt() {
            let fx = Fixture::new(vec![]);
            let trace = GenerationTrace::default();
            let (source, ready) = ScriptedGenerationSource::new([Ok(profile(
                "openai/main",
                false,
                Vec::new(),
                &trace,
                Some(2_048),
            ))])
            .pending_with_signal();
            let source = Arc::new(source);
            let (ingress, mut control) = ego_core::turn::interactive_control();
            let cancel = CancelToken::new();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut gate = fx.gate.borrow_mut();
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    cancel,
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            )
            .with_interactive_control(&mut control);
            let active = turn.run("hello");
            let drive = async move {
                ready.await.expect("resolution reached the controlled wait");
                ingress.interrupt().await.expect("interrupt is accepted");
            };
            let _ = block_on(async { futures::join!(active, drive) });
            drop(turn);
            assert!(
                !fx.store
                    .kinds()
                    .iter()
                    .any(|kind| kind == "generation_started")
            );
            assert_eq!(gate.outstanding(), Some(&usd("0")));
            assert_eq!(gate.committed(), Some(&usd("0")));
        }

        #[test]
        fn unresolved_write_without_judge_context_is_headless_and_does_not_resolve_judge() {
            let fx = Fixture::new(vec![]).with_write_tool();
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([Ok(profile(
                    "openai/main",
                    false,
                    vec![
                        AiEvent::ToolCall {
                            call_id: "write".into(),
                            name: "fake_write".into(),
                            args: r#"{"path":"/ws/out","contents":"ok"}"#.into(),
                        },
                        AiEvent::Stop(StopReason::ToolUse),
                    ],
                    &trace,
                    Some(2_048),
                ))])
                .with_trace(trace),
            );
            let mut gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            );
            let outcome = block_on(turn.run("write")).expect("headless denial is durable");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            assert_eq!(source.roles(), vec![GenerationRole::Main]);
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
        }

        #[test]
        fn explicitly_denied_write_skips_judge_even_with_context() {
            let fx = Fixture::new(vec![])
                .with_write_tool()
                .with_rules(&[], &["write(**)"]);
            let trace = GenerationTrace::default();
            let source = Arc::new(ScriptedGenerationSource::new([Ok(profile(
                "openai/main",
                false,
                vec![
                    AiEvent::ToolCall {
                        call_id: "write".into(),
                        name: "fake_write".into(),
                        args: r#"{"path":"/ws/out","contents":"ok"}"#.into(),
                    },
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                &trace,
                Some(2_048),
            ))]));
            let mut gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            )
            .with_judge(judge_context());
            let outcome = block_on(turn.run("write")).expect("denial is durable");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            assert_eq!(source.roles(), vec![GenerationRole::Main]);
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
        }

        #[test]
        fn judge_resolution_errors_are_headless_without_judge_attempt() {
            for class in [
                GenerationModelErrorClass::Usage,
                GenerationModelErrorClass::Provider,
                GenerationModelErrorClass::Internal,
            ] {
                let fx = Fixture::new(vec![]).with_write_tool();
                let trace = GenerationTrace::default();
                let source = Arc::new(
                    ScriptedGenerationSource::new([
                        Ok(profile(
                            "openai/main",
                            false,
                            vec![
                                AiEvent::ToolCall {
                                    call_id: "write".into(),
                                    name: "fake_write".into(),
                                    args: r#"{"path":"/ws/out","contents":"ok"}"#.into(),
                                },
                                AiEvent::Stop(StopReason::ToolUse),
                            ],
                            &trace,
                            Some(2_048),
                        )),
                        Err(GenerationModelError::new(class, "judge unavailable")),
                    ])
                    .with_trace(trace.clone()),
                );
                let mut gate = fx.gate.borrow_mut();
                let tools = Tools::new(
                    &fx.registry,
                    &fx.policy,
                    ToolEffectPorts::new(
                        &fx.fs,
                        fx.process.as_ref(),
                        &fx.evidence,
                        &NO_RESOLVER,
                        &NO_HOP,
                        &NO_DUPLEX,
                    ),
                    CanonPath::new("/ws"),
                );
                let mut turn = TurnLoop::new(
                    session(
                        SessionState::default(),
                        &fx.store,
                        &fx.clock,
                        &fx.sink,
                        CancelToken::new(),
                    ),
                    GenerationRuntime {
                        source: source.clone(),
                        gate: &mut gate,
                        config: TurnConfig {
                            system: None,
                            token_ceiling: None,
                        },
                    },
                    tools,
                )
                .with_judge(judge_context());
                let outcome = block_on(turn.run("write")).expect("judge error is durable");
                assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
                assert_eq!(
                    source.roles(),
                    vec![GenerationRole::Main, GenerationRole::Judge]
                );
                assert_eq!(
                    fx.store
                        .kinds()
                        .iter()
                        .filter(|kind| *kind == "generation_started")
                        .count(),
                    1
                );
                assert_eq!(
                    fx.sink
                        .events()
                        .iter()
                        .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                        .count(),
                    1
                );
                assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
                assert_eq!(
                    trace.steps.lock().unwrap().as_slice(),
                    ["resolve:Main", "generate:main", "resolve:Judge",]
                );
                assert!(!fx.store.events().iter().any(|event| matches!(
                    event.kind,
                    EventKind::PermissionDecided {
                        by: Some(Authority::Judge { .. }),
                        ..
                    }
                )));
                drop(turn);
                assert_eq!(gate.outstanding(), Some(&usd("0")));
                assert_eq!(gate.committed(), Some(&usd("0.06144")));
            }
        }

        #[test]
        fn judge_pending_cancellation_has_no_judge_attempt_or_reservation() {
            let fx = Fixture::new(vec![]).with_write_tool();
            let trace = GenerationTrace::default();
            let (source, ready) = ScriptedGenerationSource::new([
                Ok(profile(
                    "openai/main",
                    false,
                    vec![
                        AiEvent::ToolCall {
                            call_id: "write".into(),
                            name: "fake_write".into(),
                            args: r#"{"path":"/ws/out","contents":"ok"}"#.into(),
                        },
                        AiEvent::Stop(StopReason::ToolUse),
                    ],
                    &trace,
                    Some(2_048),
                )),
                Ok(profile("openai/judge", true, vec![], &trace, None)),
            ])
            .pending_with_signal_at(1);
            let source = Arc::new(source);
            let (ingress, mut control) = ego_core::turn::interactive_control();
            let cancel = CancelToken::new();
            let mut gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    cancel,
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            )
            .with_judge(judge_context())
            .with_interactive_control(&mut control);
            let active = turn.run("write");
            let drive = async move {
                ready.await.expect("Judge resolution reached pending");
                ingress.interrupt().await.expect("interrupt accepted");
            };
            let (outcome, ()) = block_on(async { futures::join!(active, drive) });
            assert_eq!(
                outcome.expect("cancellation is durable").exit_reason,
                ExitReason::Cancelled(CancelCause::User)
            );
            drop(turn);
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                1
            );
            assert!(!fx.store.kinds().iter().any(|kind| kind == "error"));
            assert!(!fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    ..
                }
            )));
            assert_eq!(gate.outstanding(), Some(&usd("0")));
            assert_eq!(gate.committed(), Some(&usd("0.06144")));
        }

        #[test]
        fn judge_capability_gap_is_headless_without_judge_attempt() {
            let mut fx = Fixture::new(vec![]).with_write_tool();
            fx.policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Auto, Ruleset::default());
            let trace = GenerationTrace::default();
            let mut judge = profile_with_settings(
                "openai/judge",
                vec![],
                &trace,
                Some(8_192),
                Some(8_192),
                ThinkingLevel::Off,
                false,
            );
            judge.capabilities.reasoning_efforts =
                ReasoningEffortSupport::Only(BTreeSet::from([ThinkingLevel::High]));
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        "openai/main",
                        false,
                        vec![
                            AiEvent::ToolCall {
                                call_id: "write".into(),
                                name: "fake_write".into(),
                                args: r#"{"path":"/ws/out","contents":"ok"}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ],
                        &trace,
                        Some(2_048),
                    )),
                    Ok(judge),
                ])
                .with_trace(trace.clone()),
            );
            let mut gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                &fx.policy,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    &NO_RESOLVER,
                    &NO_HOP,
                    &NO_DUPLEX,
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    source: source.clone(),
                    gate: &mut gate,
                    config: TurnConfig {
                        system: None,
                        token_ceiling: None,
                    },
                },
                tools,
            )
            .with_judge(judge_context());
            let outcome = block_on(turn.run("write")).expect("capability gap is durable");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            assert_eq!(
                source.roles(),
                vec![GenerationRole::Main, GenerationRole::Judge]
            );
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                1
            );
            assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
            assert_eq!(
                trace.steps.lock().unwrap().as_slice(),
                ["resolve:Main", "generate:main", "resolve:Judge",]
            );
            assert!(!fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    ..
                }
            )));
            drop(turn);
            assert_eq!(gate.outstanding(), Some(&usd("0")));
            assert_eq!(gate.committed(), Some(&usd("0.06144")));
        }
    }
}
