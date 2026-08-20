//! `Generator` port, `ModelCapabilities` and `FakeGenerator`: the provider
//! seam and its fake/replay counterpart.
//!
//! See `ARCHITECTURE.md` §2/§3.1 and `plans/m0a-contracts.md` Step 10.

use ego_core::budget::{BudgetGate, PricingQuote};
use ego_core::cost::{Amount, CostDelta, CurrencyCode, Money, PricingRevision};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, GenerationId, ItemId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::{AttemptStream, GenerationAttempt, Settlement};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, CapabilityGap, ModelCapabilities, ReasoningCarryover,
    Transport,
};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::port::{AiEvent, Generator};
use ego_core::generator::types::{
    GenRequest, Message, ReasoningState, Role, StopReason, ThinkingLevel, ToolDescriptor,
};
use ego_core::state::{SessionState, Usage};
use futures::StreamExt;

// Wrapped in a module named `generator` so `cargo test -p ego-core generator`
// (the story's VERIFY command) matches these tests by substring: cargo's
// test filter matches on the full test path, not the source file name, and
// `tests/generator.rs`'s bare functions wouldn't otherwise contain
// "generator".
mod generator {
    use super::*;

    fn gen_request() -> GenRequest {
        GenRequest {
            model: "claude-opus-5".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: "list the files".to_string(),
                tool_call_id: None,
            }],
            tools: vec![ToolDescriptor {
                name: "bash".to_string(),
                description: "run a shell command".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(1024),
            parallel_tool_calls: false,
            reasoning_state: None,
        }
    }

    fn capabilities(reasoning_carryover: ReasoningCarryover) -> ModelCapabilities {
        ModelCapabilities {
            schema_version: 1,
            max_context_tokens: Some(200_000),
            max_output_tokens: Some(64_000),
            max_tools: Some(128),
            supports_parallel_tool_calls: true,
            reasoning_carryover,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
        }
    }

    fn usd(decimal: &str) -> Money {
        Money::new(
            decimal.parse::<Amount>().expect("a valid test amount"),
            CurrencyCode::new("USD").expect("USD is a valid code"),
        )
    }

    /// Per-token prices in the shape of a real published table, converted
    /// once from the usual `$/MTok` quoting.
    fn quote() -> PricingQuote {
        let rate = |d: &str| Some(d.parse::<Amount>().expect("a valid test rate"));
        PricingQuote {
            revision: PricingRevision("test-2026-08".to_string()),
            currency: CurrencyCode::new("USD").expect("USD is a valid code"),
            uncached_input: rate("0.000003"),
            cache_read: rate("0.0000003"),
            cache_write: rate("0.00000375"),
            output: rate("0.000015"),
        }
    }

    /// A reservation minted by a real [`BudgetGate`] against a generous
    /// limit.
    ///
    /// There is deliberately no shortcut: `Reservation`'s constructor is
    /// sealed inside `ego_core::budget`, so even a test cannot fabricate
    /// one, and going through `reserve` is the only way to call `generate`
    /// at all. That is the budget gate's whole enforcement mechanism, and
    /// the fact that this helper has to exist is the proof it works.
    fn gate_and_attempt() -> (BudgetGate, GenerationAttempt) {
        let mut gate = BudgetGate::new(usd("1000"));
        let caps = capabilities(ReasoningCarryover::Portable);
        let reservation = gate
            .reserve(&quote(), &gen_request(), &caps)
            .expect("a generous budget admits an ordinary request");
        let attempt = GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &quote(),
            &gen_request(),
            &caps,
        )
        .expect("the attempt is built from the request that was priced");
        (gate, attempt)
    }

    /// Drive a stream to exhaustion and settle it — what the turn loop does.
    ///
    /// `by_ref` rather than `collect` on the stream itself, because settling
    /// needs the stream back: the attempt rides inside it, and that is the
    /// point. Dropping it instead would leak the hold and log loudly.
    fn drain(mut stream: AttemptStream, gate: &mut BudgetGate) -> (Vec<AiEvent>, Settlement) {
        let events: Vec<AiEvent> = futures::executor::block_on(stream.by_ref().collect());
        let settlement = stream
            .settle(gate)
            .expect("the gate that minted the reservation settles it");
        (events, settlement)
    }

    /// One whole generation — reserve, generate, drain, settle — for the
    /// tests that care only about what the stream produced. They still go
    /// through the full lifecycle rather than leaking an attempt, because a
    /// test that leaks is a test that would not notice if everything did.
    fn drain_events(fake: &FakeGenerator) -> Vec<AiEvent> {
        let (mut gate, attempt) = gate_and_attempt();
        drain(fake.generate(gen_request(), attempt), &mut gate).0
    }

    /// A minimal, test-local turn assembler: it maps one `AiEvent` at a time
    /// onto the `DomainEvent`s the harness loop would append to the log.
    /// Proving this mapping is exactly what this step's acceptance
    /// criterion ("drives turn assembly into the correct DomainEvents")
    /// asks for; productizing a full assembler (item bookkeeping across
    /// tool-call results, causal parents, …) is the harness loop's job, out
    /// of scope for the `Generator` port itself.
    struct Assembler {
        turn: TurnId,
        producer: Producer,
        seq: Seq,
        message_item: Option<ItemId>,
        reasoning_item: Option<ItemId>,
    }

    impl Assembler {
        fn new(turn: TurnId) -> Self {
            Self {
                turn,
                producer: Producer::Main,
                seq: Seq::default(),
                message_item: None,
                reasoning_item: None,
            }
        }

        fn envelope(&mut self, item: Option<ItemId>) -> Envelope {
            Envelope {
                seq: self.seq.next(),
                id: EventId::new(),
                turn: self.turn,
                item,
                parent: None,
                ts: Timestamp::EPOCH,
                producer: self.producer.clone(),
            }
        }

        fn assemble(&mut self, event: &AiEvent) -> Vec<DomainEvent> {
            match event {
                AiEvent::TextDelta { text } => {
                    let item = *self.message_item.get_or_insert_with(ItemId::new);
                    let env = self.envelope(Some(item));
                    vec![DomainEvent {
                        env,
                        kind: EventKind::AssistantDelta { text: text.clone() },
                    }]
                }
                AiEvent::ReasoningDelta { text } => {
                    let item = *self.reasoning_item.get_or_insert_with(ItemId::new);
                    let env = self.envelope(Some(item));
                    vec![DomainEvent {
                        env,
                        kind: EventKind::ReasoningDelta { text: text.clone() },
                    }]
                }
                AiEvent::ToolCall { name, args, .. } => {
                    let item = ItemId::new();
                    let env = self.envelope(Some(item));
                    vec![DomainEvent {
                        env,
                        kind: EventKind::ToolCallStarted {
                            name: name.clone(),
                            args: args.clone(),
                        },
                    }]
                }
                AiEvent::Usage(usage) => {
                    let env = self.envelope(None);
                    vec![DomainEvent {
                        env,
                        // The assembler prices the generation with the same
                        // quote the budget gate reserved against, which is
                        // what makes the reservation reconcilable: a cost
                        // computed under a different revision cannot be
                        // matched to the authorisation that admitted it.
                        kind: EventKind::UsageUpdated {
                            usage: *usage,
                            cost_delta: quote().price(usage),
                        },
                    }]
                }
                AiEvent::Stop(reason) => {
                    let (ok, exit_reason) = match reason {
                        StopReason::Stop | StopReason::ToolUse => (true, ExitReason::Completed),
                        StopReason::Length => (false, ExitReason::Budget),
                        StopReason::Error => (false, ExitReason::Error),
                        StopReason::Aborted => (false, ExitReason::Cancelled),
                        // Not a terminal state: nothing to assemble yet.
                        StopReason::Pending | StopReason::Deferred => return vec![],
                    };
                    let env = self.envelope(None);
                    vec![DomainEvent {
                        env,
                        kind: EventKind::TurnCompleted { ok, exit_reason },
                    }]
                }
                // The transport's delivery reading is not conversation:
                // `AttemptStream` folds it into the attempt's `Outcome`, and
                // it reaches the log through `AttemptSettled` (Step 6), not
                // through turn assembly.
                AiEvent::DeliveryObserved(_) => Vec::new(),
                AiEvent::Error { message } => {
                    let env = self.envelope(None);
                    vec![DomainEvent {
                        env,
                        kind: EventKind::Error {
                            message: message.clone(),
                        },
                    }]
                }
            }
        }
    }

    // --- FakeGenerator drives turn assembly ------------------------------

    #[test]
    fn generator_fake_stream_assembles_into_domain_events() {
        let script = vec![
            AiEvent::TextDelta {
                text: "Sure, ".to_string(),
            },
            AiEvent::TextDelta {
                text: "let me check.".to_string(),
            },
            AiEvent::ToolCall {
                call_id: "call-1".to_string(),
                name: "bash".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            },
            AiEvent::Usage(Usage {
                uncached_input_tokens: 12,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 34,
            }),
            AiEvent::Stop(StopReason::ToolUse),
        ];
        let fake = FakeGenerator::new(vec![script]);
        let events: Vec<AiEvent> = drain_events(&fake);
        assert_eq!(events.len(), 5);

        let turn = TurnId::new();
        let mut assembler = Assembler::new(turn);
        let domain_events: Vec<DomainEvent> =
            events.iter().flat_map(|e| assembler.assemble(e)).collect();

        assert_eq!(domain_events.len(), 5);
        assert_eq!(
            domain_events[0].kind,
            EventKind::AssistantDelta {
                text: "Sure, ".to_string()
            }
        );
        assert_eq!(
            domain_events[1].kind,
            EventKind::AssistantDelta {
                text: "let me check.".to_string()
            }
        );
        // Both text deltas land on the same item.
        assert_eq!(domain_events[0].env.item, domain_events[1].env.item);
        assert_eq!(
            domain_events[2].kind,
            EventKind::ToolCallStarted {
                name: "bash".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            }
        );
        // The tool call gets its own item, distinct from the message item.
        assert_ne!(domain_events[0].env.item, domain_events[2].env.item);
        assert_eq!(
            domain_events[3].kind,
            EventKind::UsageUpdated {
                usage: Usage {
                    uncached_input_tokens: 12,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 34,
                },
                // 12 x 0.000003 + 34 x 0.000015 = 0.000546, exactly.
                cost_delta: CostDelta::Known {
                    amount: usd("0.000546"),
                    pricing_revision: PricingRevision("test-2026-08".to_string()),
                    fx: None,
                },
            }
        );
        assert_eq!(
            domain_events[4].kind,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            }
        );
        for pair in domain_events.windows(2) {
            assert!(pair[0].env.seq < pair[1].env.seq);
        }

        // Feeding the assembled events through the real reducer proves the
        // mapping is not just structurally right but semantically usable.
        let mut state = SessionState::default();
        for ev in &domain_events {
            state.apply(ev);
        }
        assert_eq!(state.usage.uncached_input_tokens, 12);
        assert_eq!(state.usage.output_tokens, 34);
        assert_eq!(
            state.cost,
            ego_core::cost::CumulativeCost::Known {
                total: usd("0.000546")
            },
            "the priced generation reduces into an exact cumulative total"
        );
        assert!(state.turns.get(&turn).unwrap().completed);
        assert!(state.turns.get(&turn).unwrap().ok);

        // The generator recorded exactly the request it was driven with.
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(fake.calls()[0].model, "claude-opus-5");
    }

    #[test]
    fn generator_fake_stop_length_completes_not_ok_with_budget_exit() {
        let fake = FakeGenerator::new(vec![vec![AiEvent::Stop(StopReason::Length)]]);
        let events: Vec<AiEvent> = drain_events(&fake);
        let mut assembler = Assembler::new(TurnId::new());
        let domain_events: Vec<DomainEvent> =
            events.iter().flat_map(|e| assembler.assemble(e)).collect();
        assert_eq!(
            domain_events[0].kind,
            EventKind::TurnCompleted {
                ok: false,
                exit_reason: ExitReason::Budget,
            }
        );
    }

    #[test]
    fn generator_fake_pending_stop_is_not_terminal() {
        let fake = FakeGenerator::new(vec![vec![AiEvent::Stop(StopReason::Pending)]]);
        let events: Vec<AiEvent> = drain_events(&fake);
        let mut assembler = Assembler::new(TurnId::new());
        let domain_events: Vec<DomainEvent> =
            events.iter().flat_map(|e| assembler.assemble(e)).collect();
        assert!(domain_events.is_empty());
    }

    // --- FakeGenerator as the replay seam ---------------------------------

    #[test]
    fn generator_fake_plays_one_script_per_call_in_order() {
        let fake = FakeGenerator::new(vec![
            vec![AiEvent::TextDelta {
                text: "first".to_string(),
            }],
            vec![AiEvent::TextDelta {
                text: "second".to_string(),
            }],
        ]);

        let first: Vec<AiEvent> = drain_events(&fake);
        let second: Vec<AiEvent> = drain_events(&fake);

        assert_eq!(
            first,
            vec![AiEvent::TextDelta {
                text: "first".to_string()
            }]
        );
        assert_eq!(
            second,
            vec![AiEvent::TextDelta {
                text: "second".to_string()
            }]
        );
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn generator_fake_call_past_scripted_count_is_empty_not_a_panic() {
        let fake = FakeGenerator::new(Vec::new());
        let events: Vec<AiEvent> = drain_events(&fake);
        assert!(events.is_empty());
        assert_eq!(fake.calls().len(), 1);
    }

    // --- ModelCapabilities --------------------------------------------------

    #[test]
    fn generator_model_capabilities_serde_roundtrip() {
        let caps = capabilities(ReasoningCarryover::SameProviderOnly);
        let json = serde_json::to_string(&caps).expect("serialize");
        let back: ModelCapabilities = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(caps, back);
    }

    // --- Incompatible fallback is detected and reported ---------------------

    #[test]
    fn generator_fallback_reasoning_carryover_gap_is_detected() {
        let current = capabilities(ReasoningCarryover::Portable);
        let target = capabilities(ReasoningCarryover::Unsupported);
        let req = GenRequest {
            reasoning_state: Some(ReasoningState("provider-opaque-token".to_string())),
            ..gen_request()
        };

        let gaps = current.fallback_gaps(&target, &req);

        assert!(gaps.contains(&CapabilityGap::ReasoningStateCarryoverUnsupported));
    }

    #[test]
    fn generator_fallback_without_reasoning_state_has_no_carryover_gap() {
        let current = capabilities(ReasoningCarryover::Portable);
        let target = capabilities(ReasoningCarryover::Unsupported);
        let req = gen_request(); // reasoning_state: None

        let gaps = current.fallback_gaps(&target, &req);

        assert!(!gaps.contains(&CapabilityGap::ReasoningStateCarryoverUnsupported));
    }

    #[test]
    fn generator_fallback_parallel_tool_calls_gap_is_detected() {
        let current = capabilities(ReasoningCarryover::Portable);
        let mut target = capabilities(ReasoningCarryover::Portable);
        target.supports_parallel_tool_calls = false;
        let req = GenRequest {
            parallel_tool_calls: true,
            ..gen_request()
        };

        let gaps = current.fallback_gaps(&target, &req);

        assert!(gaps.contains(&CapabilityGap::ParallelToolCallsUnsupported));
    }

    #[test]
    fn generator_fallback_too_many_tools_gap_is_detected() {
        let current = capabilities(ReasoningCarryover::Portable);
        let mut target = capabilities(ReasoningCarryover::Portable);
        target.max_tools = Some(0);
        let req = gen_request(); // carries one tool

        let gaps = current.fallback_gaps(&target, &req);

        assert!(gaps.contains(&CapabilityGap::TooManyTools {
            requested: 1,
            max: 0,
        }));
    }

    #[test]
    fn generator_fallback_output_too_large_gap_is_detected() {
        let current = capabilities(ReasoningCarryover::Portable);
        let mut target = capabilities(ReasoningCarryover::Portable);
        target.max_output_tokens = Some(100);
        let req = GenRequest {
            max_output_tokens: Some(1024),
            ..gen_request()
        };

        let gaps = current.fallback_gaps(&target, &req);

        assert!(gaps.contains(&CapabilityGap::OutputTooLarge {
            requested: 1024,
            max: 100,
        }));
    }

    #[test]
    fn generator_fallback_fully_compatible_has_no_gaps() {
        let current = capabilities(ReasoningCarryover::Portable);
        let target = capabilities(ReasoningCarryover::Portable);
        let req = gen_request();

        let gaps = current.fallback_gaps(&target, &req);

        assert!(gaps.is_empty());
    }

    // --- wire-format spelling of the pi-borrowed enums -----------------------

    #[test]
    fn generator_thinking_level_serialises_pi_scale() {
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::Off).unwrap(),
            "\"off\""
        );
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::XHigh).unwrap(),
            "\"xhigh\""
        );
    }

    #[test]
    fn generator_stop_reason_serialises_pi_scale() {
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"toolUse\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::Deferred).unwrap(),
            "\"deferred\""
        );
    }

    #[test]
    fn generator_gen_request_serde_roundtrip() {
        let req = gen_request();
        let json = serde_json::to_string(&req).expect("serialize");
        let back: GenRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, back);
    }
}
