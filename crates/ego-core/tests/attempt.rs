//! Generation attempts: the identity a retry keeps, the identity it does not,
//! and the delivery certainty that decides whether a retry is safe at all.
//!
//! See `docs/decisions/2026-08-20-generation-attempt-contract.md` and
//! `plans/m0b-vertical-slice.md` Steps 1-2.
//!
//! What these tests defend is one claim: **a generation and a wire attempt are
//! not the same thing, and confusing them double-bills**. Prior history is
//! re-sent on every retry, so a provider that already charged for attempt 1
//! charges again for attempt 2 — the only defence is knowing which is which
//! and refusing to retry when the first attempt may have landed.

use ego_core::event::id::{AttemptId, GenerationId};
use ego_core::generator::attempt::{AttemptRef, DeliveryCertainty};

// Wrapped in a module named `attempt` so `cargo test -p ego-core attempt`
// matches by substring: cargo filters on the full test path, not the file
// name — the same trick `tests/reducer.rs` and `tests/cost.rs` use.
mod attempt {
    use super::*;

    // --- identity ---------------------------------------------------------

    #[test]
    fn generation_and_attempt_ids_sort_by_creation() {
        let (a, b) = (GenerationId::new(), GenerationId::new());
        assert!(a < b, "UUIDv7 generations must order by creation");
        let (a, b) = (AttemptId::new(), AttemptId::new());
        assert!(a < b, "UUIDv7 attempts must order by creation");
    }

    #[test]
    fn ids_round_trip_through_serde() {
        let generation = GenerationId::new();
        let json = serde_json::to_string(&generation).unwrap();
        assert_eq!(
            serde_json::from_str::<GenerationId>(&json).unwrap(),
            generation
        );

        let attempt = AttemptId::new();
        let json = serde_json::to_string(&attempt).unwrap();
        assert_eq!(serde_json::from_str::<AttemptId>(&json).unwrap(), attempt);
    }

    /// The whole point of two identifiers. A retry is the *same* logical
    /// generation — the budget settles once for it — reaching the wire a
    /// second time, and usage figures key to the attempt so the two are not
    /// summed.
    #[test]
    fn a_retry_keeps_the_generation_and_mints_a_new_attempt() {
        let first = AttemptRef::first(GenerationId::new());
        let second = first.retry();
        let third = second.retry();

        assert_eq!(second.generation(), first.generation());
        assert_eq!(third.generation(), first.generation());

        assert_ne!(second.attempt(), first.attempt());
        assert_ne!(third.attempt(), second.attempt());
        assert_ne!(third.attempt(), first.attempt());
    }

    #[test]
    fn two_generations_never_share_an_identity() {
        let a = AttemptRef::first(GenerationId::new());
        let b = AttemptRef::first(GenerationId::new());
        assert_ne!(a.generation(), b.generation());
        assert_ne!(a.attempt(), b.attempt());
    }

    // --- delivery certainty ------------------------------------------------

    /// A fresh attempt has not touched the wire, so it is safe to retry: that
    /// is the only state in which a retry cannot double-bill.
    #[test]
    fn a_fresh_attempt_is_definitely_unsent_and_safe_to_retry() {
        let certainty = DeliveryCertainty::default();
        assert_eq!(certainty, DeliveryCertainty::DefinitelyUnsent);
        assert!(certainty.retry_is_safe());
    }

    #[test]
    fn once_the_request_may_have_landed_a_retry_is_not_safe() {
        let mut certainty = DeliveryCertainty::default();
        certainty.observe(DeliveryCertainty::PossiblySent);
        assert!(!certainty.retry_is_safe());
    }

    /// The invariant. Certainty is monotonic — it only ever gets worse — and
    /// this is the test that matters: a downgrade back to `DefinitelyUnsent`
    /// would licence exactly the retry that bills twice. A connection reset
    /// *after* the bytes went out looks locally like "nothing happened", and
    /// that is precisely when the lie is most tempting.
    #[test]
    fn certainty_never_improves_back_to_definitely_unsent() {
        let mut certainty = DeliveryCertainty::default();
        certainty.observe(DeliveryCertainty::PossiblySent);

        for _ in 0..3 {
            certainty.observe(DeliveryCertainty::DefinitelyUnsent);
            assert_eq!(
                certainty,
                DeliveryCertainty::PossiblySent,
                "a later observation must not restore certainty we lost"
            );
        }
    }

    #[test]
    fn observing_the_same_certainty_twice_changes_nothing() {
        let mut certainty = DeliveryCertainty::default();
        certainty.observe(DeliveryCertainty::DefinitelyUnsent);
        assert_eq!(certainty, DeliveryCertainty::DefinitelyUnsent);

        certainty.observe(DeliveryCertainty::PossiblySent);
        certainty.observe(DeliveryCertainty::PossiblySent);
        assert_eq!(certainty, DeliveryCertainty::PossiblySent);
    }

    /// It travels in a durable event (`AttemptSettled`), so it has to survive
    /// the round trip with its meaning intact.
    #[test]
    fn certainty_round_trips_through_serde() {
        for certainty in [
            DeliveryCertainty::DefinitelyUnsent,
            DeliveryCertainty::PossiblySent,
        ] {
            let json = serde_json::to_string(&certainty).unwrap();
            assert_eq!(
                serde_json::from_str::<DeliveryCertainty>(&json).unwrap(),
                certainty
            );
        }
    }
}

/// The attempt lifecycle: who owns the authorisation between admission and
/// settlement, and what happens when nobody does.
///
/// See `plans/m0b-vertical-slice.md` Steps 4-5. The claim under test is that
/// a `Reservation` can no longer be borrowed twice, settled by the wrong
/// gate, or quietly abandoned — and that the hold it represents is released
/// exactly once, against the gate that took it.
mod lifecycle {
    use std::sync::{Arc, Mutex};

    use ego_core::budget::{BudgetGate, PricingQuote};
    use ego_core::cost::{Amount, CostDelta, CurrencyCode, Money, PricingRevision};
    use ego_core::event::id::GenerationId;
    use ego_core::generator::attempt::{
        DeliveryCertainty, GenerationAttempt, Outcome, Settlement, UsageReport,
    };
    use ego_core::generator::capabilities::{
        CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, Transport,
    };
    use ego_core::generator::fake::FakeGenerator;
    use ego_core::generator::port::{AiEvent, Generator};
    use ego_core::generator::types::{GenRequest, Message, Role, StopReason, ThinkingLevel};
    use ego_core::state::Usage;
    use futures::StreamExt;

    // --- fixtures ---------------------------------------------------------

    fn usd(decimal: &str) -> Money {
        Money::new(
            decimal.parse::<Amount>().expect("a valid amount"),
            CurrencyCode::new("USD").expect("USD is valid"),
        )
    }

    fn revision() -> PricingRevision {
        PricingRevision("test-2026-08".to_string())
    }

    fn quote() -> PricingQuote {
        PricingQuote {
            revision: revision(),
            currency: CurrencyCode::new("USD").expect("USD is valid"),
            uncached_input: Some("0.000003".parse().unwrap()),
            cache_read: Some("0.0000003".parse().unwrap()),
            cache_write: Some("0.00000375".parse().unwrap()),
            output: Some("0.000015".parse().unwrap()),
        }
    }

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            schema_version: 1,
            max_context_tokens: Some(200_000),
            max_output_tokens: Some(64_000),
            max_tools: Some(128),
            supports_parallel_tool_calls: true,
            reasoning_carryover: ReasoningCarryover::Portable,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
        }
    }

    fn request_for(model: &str) -> GenRequest {
        GenRequest {
            model: model.to_string(),
            messages: vec![Message {
                role: Role::User,
                content: "list the files".to_string(),
                tool_call_id: None,
            }],
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(1_024),
            parallel_tool_calls: false,
            reasoning_state: None,
        }
    }

    fn request() -> GenRequest {
        request_for("claude-opus-5")
    }

    /// The bound the standard fixture reserves: a 200k context at the
    /// dearest input rate ($3.75/MTok cache write) plus 1024 output tokens.
    fn bound() -> Money {
        usd("0.76536")
    }

    fn tokens(uncached_input: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: output,
        }
    }

    /// A gate holding exactly one attempt's worth of budget.
    ///
    /// There is no shortcut here and that is the point: `Reservation`'s
    /// constructor is sealed, `GenerationAttempt` is the only thing that can
    /// hold one, and `generate` will not accept anything else. Reaching this
    /// call at all is the proof the gate cannot be routed around.
    fn gate_and_attempt() -> (BudgetGate, GenerationAttempt) {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(&quote(), &request(), &caps())
            .expect("a $10 budget admits an ordinary request");
        let attempt = GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &quote(),
            &request(),
            &caps(),
        )
        .expect("built from the request that was priced");
        (gate, attempt)
    }

    /// Run one whole generation: reserve, generate, drain, settle.
    fn run(script: Vec<AiEvent>) -> (BudgetGate, Settlement) {
        let fake = FakeGenerator::new(vec![script]);
        let (mut gate, attempt) = gate_and_attempt();
        let mut stream = fake.generate(request(), attempt);
        let _: Vec<AiEvent> = futures::executor::block_on(stream.by_ref().collect());
        let settlement = stream.settle(&mut gate).expect("its own gate settles it");
        (gate, settlement)
    }

    fn outcome_of(script: Vec<AiEvent>) -> Outcome {
        run(script).1.outcome
    }

    // --- admission --------------------------------------------------------

    /// `Reservation::authorises` finally has a caller, and this is the right
    /// place for it: the request, the capabilities and the quote are all
    /// still in hand here, and none of them are once the stream is running.
    #[test]
    fn an_attempt_refuses_a_reservation_that_does_not_authorise_the_request() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate.reserve(&quote(), &request(), &caps()).unwrap();
        let denied = GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &quote(),
            &request_for("some-other-model"),
            &caps(),
        )
        .expect_err("this reservation authorised claude-opus-5");
        assert!(
            denied.to_string().contains("some-other-model"),
            "the denial names the substitution: {denied}"
        );
    }

    #[test]
    fn generating_takes_the_attempt_by_value_and_hands_it_back_in_the_stream() {
        let fake = FakeGenerator::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        let (mut gate, attempt) = gate_and_attempt();
        let expected = attempt.attempt();

        // `attempt` is moved here. The line below does not compile:
        //   let _ = attempt.attempt();
        //   error[E0382]: borrow of moved value: `attempt`
        let mut stream = fake.generate(request(), attempt);
        assert_eq!(
            stream.attempt(),
            expected,
            "the identity ego-core stamped must survive the round trip, \
             rather than being re-supplied by the provider layer"
        );
        let _: Vec<AiEvent> = futures::executor::block_on(stream.by_ref().collect());
        assert!(stream.settle(&mut gate).is_ok());
    }

    // --- the hold ---------------------------------------------------------

    /// The property the wrapper exists for. The hold is still held while the
    /// stream is mid-flight, and released only by `settle` — and it cannot
    /// be otherwise, because the attempt is inside the stream and there is
    /// no way to reach it except by consuming both.
    #[test]
    fn the_hold_survives_until_the_stream_ends() {
        let fake = FakeGenerator::new(vec![vec![
            AiEvent::TextDelta {
                text: "one".to_string(),
            },
            AiEvent::TextDelta {
                text: "two".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        let (mut gate, attempt) = gate_and_attempt();
        assert_eq!(*gate.outstanding(), bound());

        let mut stream = fake.generate(request(), attempt);
        for _ in 0..3 {
            let event = futures::executor::block_on(stream.next());
            assert!(event.is_some());
            assert_eq!(
                *gate.outstanding(),
                bound(),
                "the hold must not be released part-way through a stream"
            );
        }

        let settled = stream.settle(&mut gate).expect("its own gate");
        assert!(
            gate.outstanding().amount.is_zero(),
            "and must be released exactly once, at settlement"
        );
        assert_eq!(settled.outcome.stop(), StopReason::Stop);
    }

    /// Settling twice is a compile error, not a runtime check: `settle`
    /// consumes the stream, which owns the attempt, which owns the sole
    /// non-`Clone` `Reservation`. This test pins the runtime half — that one
    /// settlement really does release the whole hold and charge once.
    #[test]
    fn an_attempt_settles_exactly_once() {
        let (gate, settled) = run(vec![
            AiEvent::Usage(tokens(1_000, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]);
        // stream.settle(&mut gate);   // error[E0382]: use of moved value
        assert_eq!(settled.reconciled.charged, usd("0.006"));
        assert_eq!(*gate.committed(), usd("0.006"));
        assert!(gate.outstanding().amount.is_zero());
    }

    /// Dropping an unsettled attempt cannot release the hold — `Drop` has no
    /// way to reach a `&mut BudgetGate` — so the two things that must be
    /// true are that it is loud, and that what it leaves behind fails
    /// *closed*: the budget stays narrowed, never widened.
    #[test]
    fn dropping_an_unsettled_attempt_is_loud_and_leaves_the_hold_in_place() {
        let (gate, logged) = {
            let (gate, attempt) = gate_and_attempt();
            let captured = capture_logs(|| drop(attempt));
            (gate, captured)
        };

        assert!(
            gate.outstanding().amount == bound().amount,
            "a leaked hold must stay held: narrowing the budget is safe, \
             releasing it silently is not"
        );
        let complaint = logged
            .iter()
            .find(|line| line.contains("dropped without settling"))
            .unwrap_or_else(|| panic!("a leaked attempt must say so; got {logged:?}"));
        assert!(complaint.starts_with("ERROR"), "and at ERROR: {complaint}");
        for field in ["generation", "attempt", "gate", "bound"] {
            assert!(
                complaint.contains(field),
                "the complaint must name {field} so the leak is traceable: {complaint}"
            );
        }
    }

    #[test]
    fn a_settled_attempt_says_nothing_on_drop() {
        let logged = capture_logs(|| {
            let _ = run(vec![AiEvent::Stop(StopReason::Stop)]);
        });
        assert!(
            !logged.iter().any(|line| line.contains("without settling")),
            "settling is the normal path and must be silent: {logged:?}"
        );
    }

    // --- exactly one outcome ----------------------------------------------

    #[test]
    fn only_the_first_terminal_stop_is_believed() {
        let outcome = outcome_of(vec![
            AiEvent::Stop(StopReason::Stop),
            AiEvent::Stop(StopReason::Error),
        ]);
        assert_eq!(
            outcome.stop(),
            StopReason::Stop,
            "a provider that reports two terminals is not believed twice"
        );
    }

    /// Every terminal stop reason survives the round trip to a settlement.
    /// `Deferred` included: the provider handing back a handle instead of a
    /// stream is still the end of *this* attempt, and its hold must be
    /// released like any other.
    #[test]
    fn every_terminal_stop_reason_reaches_a_settlement() {
        for stop in [
            StopReason::Stop,
            StopReason::Length,
            StopReason::ToolUse,
            StopReason::Error,
            StopReason::Aborted,
            StopReason::Deferred,
        ] {
            let (gate, settled) = run(vec![AiEvent::Stop(stop)]);
            assert_eq!(settled.outcome.stop(), stop);
            assert!(
                gate.outstanding().amount.is_zero(),
                "{stop:?} must still release its hold"
            );
        }
    }

    /// `Pending` means "no terminal state yet", so a provider that sends it
    /// has said nothing terminal — and an attempt that ends without one was
    /// cancelled, whoever cancelled it.
    #[test]
    fn pending_is_not_a_terminal_state() {
        assert_eq!(
            outcome_of(vec![AiEvent::Stop(StopReason::Pending)]).stop(),
            StopReason::Aborted
        );
        assert_eq!(outcome_of(Vec::new()).stop(), StopReason::Aborted);
        assert!(
            Outcome::new(
                StopReason::Pending,
                DeliveryCertainty::DefinitelyUnsent,
                UsageReport::Missing
            )
            .is_err(),
            "and it cannot be built directly either"
        );
    }

    /// Settling before the stream terminates *is* cancellation — it consumes
    /// the stream, so the generation stops — and the outcome says so rather
    /// than pretending the provider finished.
    #[test]
    fn settling_a_stream_early_is_a_cancellation_and_still_releases_the_hold() {
        let fake = FakeGenerator::new(vec![vec![
            AiEvent::TextDelta {
                text: "half a sen".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        let (mut gate, attempt) = gate_and_attempt();
        let mut stream = fake.generate(request(), attempt);
        let _ = futures::executor::block_on(stream.next());

        let settled = stream.settle(&mut gate).expect("its own gate");
        assert_eq!(settled.outcome.stop(), StopReason::Aborted);
        assert!(gate.outstanding().amount.is_zero());
        assert!(
            !settled.outcome.retry_is_safe(),
            "the provider already answered, so a retry would be billed twice"
        );
    }

    // --- what it charges --------------------------------------------------

    #[test]
    fn usage_increments_accumulate_across_the_stream() {
        let outcome = outcome_of(vec![
            AiEvent::Usage(tokens(1_000, 100)),
            AiEvent::Usage(tokens(0, 100)),
            AiEvent::Stop(StopReason::Stop),
        ]);
        assert_eq!(
            outcome.usage(),
            UsageReport::Reported(tokens(1_000, 200)),
            "usage arrives as increments, so it must add, not replace"
        );
    }

    /// The quote that authorised the generation is the quote that prices it,
    /// because the attempt holds it. A caller cannot hand `reconcile` a cost
    /// computed under a different table.
    #[test]
    fn a_reported_usage_is_charged_exactly_under_the_authorising_quote() {
        let (gate, settled) = run(vec![
            AiEvent::Usage(tokens(1_000, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]);
        // 1000 x $0.000003 + 200 x $0.000015
        assert_eq!(settled.reconciled.charged, usd("0.006"));
        assert!(settled.reconciled.exact);
        assert!(settled.reconciled.over_bound.is_none());
        assert_eq!(*gate.committed(), usd("0.006"));
    }

    /// "Reported nothing" and "reported zero" settle differently, which is
    /// exactly why `UsageReport` is an enum rather than an `Option`.
    #[test]
    fn a_missing_usage_report_charges_the_bound_while_a_reported_zero_charges_nothing() {
        let (_, missing) = run(vec![AiEvent::Stop(StopReason::Stop)]);
        assert_eq!(missing.outcome.usage(), UsageReport::Missing);
        assert_eq!(missing.reconciled.charged, bound());
        assert!(!missing.reconciled.exact);

        let (_, zero) = run(vec![
            AiEvent::Usage(Usage::default()),
            AiEvent::Stop(StopReason::Stop),
        ]);
        assert_eq!(
            zero.outcome.usage(),
            UsageReport::Reported(Usage::default())
        );
        assert_eq!(zero.reconciled.charged, usd("0"));
        assert!(zero.reconciled.exact);
    }

    #[test]
    fn settling_at_a_gate_that_did_not_mint_the_reservation_hands_it_back() {
        let fake = FakeGenerator::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        let (mut mine, attempt) = gate_and_attempt();
        let mut theirs = BudgetGate::new(usd("10"));

        let mut stream = fake.generate(request(), attempt);
        let _: Vec<AiEvent> = futures::executor::block_on(stream.by_ref().collect());
        let denied = stream
            .settle(&mut theirs)
            .expect_err("this attempt belongs to another gate");

        assert!(theirs.outstanding().amount.is_zero());
        assert_eq!(*mine.outstanding(), bound(), "still held by the real gate");
        // The authorisation is not lost — it travels inside the refusal, so
        // the caller can settle it where it belongs.
        assert!(
            mine.reconcile(
                denied.reservation,
                &CostDelta::Unknown(ego_core::cost::UnknownCost::ProviderReportedNoUsage)
            )
            .is_ok()
        );
        assert!(mine.outstanding().amount.is_zero());
    }

    // --- delivery certainty across a real stream ---------------------------

    #[test]
    fn delivery_certainty_only_ever_worsens_across_a_stream() {
        let outcome = outcome_of(vec![
            AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent),
            AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
            AiEvent::Stop(StopReason::Error),
        ]);
        assert_eq!(outcome.delivery(), DeliveryCertainty::PossiblySent);
        assert!(!outcome.retry_is_safe());
    }

    /// The backstop. Anything that is provider *content* proves the request
    /// arrived, whatever the transport did or did not bother to report — so
    /// a provider adapter that never emits `DeliveryObserved` still cannot
    /// licence a double-billing retry.
    #[test]
    fn provider_content_proves_the_request_arrived_even_with_no_transport_report() {
        let outcome = outcome_of(vec![AiEvent::TextDelta {
            text: "hello".to_string(),
        }]);
        assert_eq!(outcome.delivery(), DeliveryCertainty::PossiblySent);
        assert!(!outcome.retry_is_safe());
    }

    /// And the other direction, which matters just as much: an `Error` may
    /// be the provider answering *or* the transport failing before it sent
    /// anything, and those are opposite readings of retry safety. Guessing
    /// the pessimistic one would make every local connection failure
    /// unretryable, so the transport has to say which with
    /// `DeliveryObserved`.
    #[test]
    fn an_error_alone_is_not_read_as_proof_the_request_arrived() {
        let outcome = outcome_of(vec![AiEvent::Error {
            message: "connection refused".to_string(),
        }]);
        assert_eq!(outcome.delivery(), DeliveryCertainty::DefinitelyUnsent);
        assert!(
            outcome.retry_is_safe(),
            "a request that never left is safe to send again"
        );
    }

    // --- test-local tracing capture ----------------------------------------

    /// Collect the `tracing` events emitted while `body` runs, as
    /// `"LEVEL field=value …"` lines.
    ///
    /// Hand-rolled rather than pulling in `tracing-subscriber`: `ego-core`'s
    /// manifest is deliberate about what it depends on, and the whole need
    /// here is "did an ERROR come out, and did it name the ids".
    fn capture_logs(body: impl FnOnce()) -> Vec<String> {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let subscriber = Capture {
            lines: Arc::clone(&lines),
        };
        tracing::subscriber::with_default(subscriber, body);
        let captured = lines.lock().expect("capture poisoned");
        captured.clone()
    }

    struct Capture {
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut rendered = format!("{} ", event.metadata().level());
            event.record(&mut Fields(&mut rendered));
            self.lines.lock().expect("capture poisoned").push(rendered);
        }
    }

    struct Fields<'a>(&'a mut String);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!("{}={:?} ", field.name(), value));
        }
    }
}
