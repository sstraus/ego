//! The turn loop: what a turn writes down, and what it does when something
//! goes wrong.
//!
//! See `docs/decisions/2026-08-21-the-turn-loop.md` and
//! `plans/m0b-vertical-slice.md` Step 10.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ego_core::budget::{BudgetGate, Metering, PricingQuote, ResolvedRates, SettlementSource};
use ego_core::config::SystemPrompt;
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money, PricingRevision,
    ProviderCostRevision, RateSource, UnknownCost,
};
use ego_core::event::domain::{CancelCause, DomainEvent, EventKind, ExitReason};
use ego_core::event::id::{Seq, SessionId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::attempt::{AttemptStream, GenerationAttempt};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::port::{AiEvent, Generator};
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, Role, StopReason, ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::{
    CanonPath, GuardedIo, Op, Permit, PolicyEngine, PrepareCtx, PreparedCall, ToolError,
    ToolResult, Typed, TypedTool,
};
use ego_core::public::PublicEvent;
use ego_core::run::RunTermination;
use ego_core::sink::{CollectingSink, EventSink};
use ego_core::state::{ItemKind, SessionState, Usage};
use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
use ego_core::turn::{CancelToken, Model, Session, Tools, TurnConfig, TurnLoop};
use futures::StreamExt;
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use schemars::JsonSchema;
use serde::Deserialize;

mod common;

mod turn {
    use super::*;
    use ego_core::permit::fake::FakeFs;

    /// A `SessionStore` that keeps the log in memory, and can be told to fail
    /// on the nth append — which is the only way to test what the loop does
    /// when the log cannot be written.
    #[derive(Default)]
    struct MemStore {
        events: Mutex<Vec<DomainEvent>>,
        appends: AtomicUsize,
        /// Fail the append with this 1-based index. `0` never fails.
        fail_at: AtomicUsize,
    }

    impl MemStore {
        fn fail_at(&self, nth: usize) {
            self.fail_at.store(nth, Ordering::SeqCst);
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
            if nth == self.fail_at.load(Ordering::SeqCst) {
                return Err(StoreError::Io(std::io::Error::other("the disk said no")));
            }
            self.events.lock().unwrap().push(ev.clone());
            Ok(())
        }

        fn read(
            &self,
            _id: &SessionId,
        ) -> Result<BoxStream<'static, Result<DomainEvent, StoreError>>, StoreError> {
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

    impl Generator for CancellingGenerator {
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
        fn generate(&self, _req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
            let events = std::mem::take(&mut *self.events.lock().unwrap());
            let stream = futures::stream::iter(events).chain(futures::stream::pending());
            AttemptStream::new(attempt, Box::pin(stream))
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
            _permit: &'a Permit,
            io: &'a dyn GuardedIo,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
            Box::pin(async move {
                let Some(Op::Read { path }) = call.ops.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Read op".into()));
                };
                let bytes = io.read(&path)?;
                Ok(ToolResult {
                    content: String::from_utf8_lossy(&bytes).into_owned(),
                })
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
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
        }
    }

    fn config() -> TurnConfig {
        TurnConfig {
            model: "test-model".to_string(),
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(1_024),
            parallel_tool_calls: false,
            system: None,
        }
    }

    fn gate() -> BudgetGate {
        BudgetGate::new(usd("1000"))
    }

    /// Everything a `TurnLoop` borrows, kept alive in one place so a test can
    /// build a loop in one line without fighting the borrow checker.
    struct Fixture {
        store: MemStore,
        clock: FixedClock,
        generator: FakeGenerator,
        policy: PolicyEngine,
        fs: FakeFs,
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
                generator: FakeGenerator::new(script),
                policy: PolicyEngine::new(CanonPath::new("/ws")),
                fs: FakeFs::default(),
                registry: Vec::new(),
                gate: RefCell::new(gate()),
                sink: CollectingSink::new(),
            }
        }

        fn with_read_tool(mut self) -> Self {
            self.registry = vec![std::sync::Arc::new(Typed(FakeReadTool))];
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

    fn model<'a>(generator: &'a dyn Generator, gate: &'a mut BudgetGate) -> Model<'a> {
        model_with(generator, gate, metering(), Some(rates()), None, config())
    }

    fn model_with<'a>(
        generator: &'a dyn Generator,
        gate: &'a mut BudgetGate,
        metering: Metering,
        rates: Option<ResolvedRates>,
        token_ceiling: Option<u64>,
        config: TurnConfig,
    ) -> Model<'a> {
        Model {
            generator,
            gate,
            metering,
            rates,
            token_ceiling,
            caps: caps(),
            config,
        }
    }

    /// Run one turn against `fx`, starting from `state`, and hand back where
    /// the session got to.
    fn run(fx: &Fixture, state: SessionState, prompt: &str) -> (ExitReason, bool, SessionState) {
        run_with(fx, state, prompt, metering(), Some(rates()), None, config())
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
        config: TurnConfig,
    ) -> (ExitReason, bool, SessionState) {
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
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
        assert_eq!(call.text, "hello from the file");
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
        assert!(
            call.text.contains("delete_everything"),
            "the refusal must name the tool: {}",
            call.text
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
        assert!(
            call.text.contains("not JSON"),
            "the model has to learn its arguments were malformed: {}",
            call.text
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
        assert!(
            call.text.contains("/etc/passwd") && call.text.contains("outside the workspace"),
            "a denial must name what it denied: {}",
            call.text
        );
        assert!(
            !call.text.contains("root:x"),
            "the file must not have been read at all: {}",
            call.text
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
        let generator = CancellingGenerator {
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
        };

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
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
        let generator = CancellingGenerator {
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
        };

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
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
            !kinds.contains(&"tool_call_completed".to_string()),
            "a cancelled turn must not run the call it had queued: {kinds:?}"
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
            call.ok, None,
            "the call was neither run nor answered — it was abandoned, and the log says so"
        );
    }

    #[test]
    fn a_refused_budget_stops_the_turn_without_starting_a_generation() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        // A gate too small to admit the upper bound of any request.
        let mut gate = BudgetGate::new(usd("0.0000001"));
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
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
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
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
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
        let mut refusing = model(&fx.generator, &mut gate);
        // The model accepts three levels; `config()` asks for `Off`.
        refusing.caps.reasoning_efforts = ReasoningEffortSupport::Only(BTreeSet::from([
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ]));
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
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
        let mut refusing = model(&fx.generator, &mut gate);
        refusing.caps.reasoning_efforts =
            ReasoningEffortSupport::Only(BTreeSet::from([ThinkingLevel::High]));
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
            RunTermination::after_turn(outcome.exit_reason, outcome.denied.as_deref())
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
                EventKind::Error { message } => Some(message.clone()),
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
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
        let mut silent = model(&fx.generator, &mut gate);
        silent.caps.reasoning_efforts = ReasoningEffortSupport::Unknown;
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
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
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
        let generator = StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "thinking out loud".to_string(),
            }]),
        };
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
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
        let tools = Tools::new(&fx.registry, &fx.policy, &fx.fs, CanonPath::new("/ws"));
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
}
