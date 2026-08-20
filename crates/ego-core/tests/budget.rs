//! The budget gate: pre-request reservation, post-response reconciliation,
//! and failing closed on anything that cannot be bounded.
//!
//! See `ARCHITECTURE.md` §3.1 and `crates/ego-core/src/budget.rs`'s module
//! doc comment.
//!
//! The claim under test is that the gate is *load-bearing*, not advisory.
//! Three properties carry that:
//!
//! 1. **It runs before the money is spent.** `Generator::generate` takes a
//!    `&Reservation` whose constructor is sealed inside `ego_core::budget`,
//!    so `BudgetGate::reserve` is the only source of one anywhere in the
//!    program — not by convention, but because nothing else can name the
//!    private type its constructor needs. `budget_reservations_cannot_be_
//!    forged_outside_the_gate` documents the compile-time half, which no
//!    runtime test can express.
//! 2. **It reserves the worst case, not the likely case.** Output length is
//!    the model's choice, so anything less than a pessimistic bound lets a
//!    run of long generations walk through the limit.
//! 3. **It fails closed.** Unpriced category, unstated context window,
//!    wrong currency, overflow — every one denies rather than proceeding
//!    and hoping.

use ego_core::budget::{BudgetDenied, BudgetGate, PricingQuote};
use ego_core::cost::{
    Amount, CostDelta, CurrencyCode, Money, PricingRevision, TokenCategory, UnknownCost,
};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, Transport,
};
use ego_core::generator::types::{GenRequest, Message, Role, ThinkingLevel};
use ego_core::state::Usage;

// Wrapped in a module named `budget` so `cargo test -p ego-core budget`
// matches by substring on the full test path.
mod budget {
    use super::*;

    fn amount(decimal: &str) -> Amount {
        decimal.parse().expect("a valid test amount")
    }

    fn usd(decimal: &str) -> Money {
        Money::new(
            amount(decimal),
            CurrencyCode::new("USD").expect("USD is valid"),
        )
    }

    fn revision() -> PricingRevision {
        PricingRevision("test-2026-08".to_string())
    }

    /// Prices in the shape of a published table, converted once from the
    /// usual `$/MTok` quoting: $3/MTok input, $0.30/MTok cache read,
    /// $3.75/MTok cache write, $15/MTok output.
    fn quote() -> PricingQuote {
        PricingQuote {
            revision: revision(),
            currency: CurrencyCode::new("USD").expect("USD is valid"),
            uncached_input: Some(amount("0.000003")),
            cache_read: Some(amount("0.0000003")),
            cache_write: Some(amount("0.00000375")),
            output: Some(amount("0.000015")),
        }
    }

    fn caps(max_context_tokens: Option<u64>, max_output_tokens: Option<u64>) -> ModelCapabilities {
        ModelCapabilities {
            schema_version: 1,
            max_context_tokens,
            max_output_tokens,
            max_tools: Some(128),
            supports_parallel_tool_calls: true,
            reasoning_carryover: ReasoningCarryover::Portable,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
        }
    }

    fn request(max_output_tokens: Option<u64>) -> GenRequest {
        GenRequest {
            model: "claude-opus-5".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: "list the files".to_string(),
                tool_call_id: None,
            }],
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            max_output_tokens,
            parallel_tool_calls: false,
            reasoning_state: None,
        }
    }

    fn tokens(uncached_input: u64, cache_read: u64, cache_write: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            output_tokens: output,
        }
    }

    /// The bound for the standard fixture: a full 200k context of uncached
    /// input at $3/MTok is $0.60, plus 1024 output tokens at $15/MTok,
    /// $0.01536. Written out because every limit below is chosen relative
    /// to it.
    fn standard_bound() -> Money {
        usd("0.61536")
    }

    // --- pricing ----------------------------------------------------------

    #[test]
    fn pricing_a_generation_sums_every_category_exactly() {
        // 1000 x 0.000003 + 10000 x 0.0000003 + 500 x 0.00000375
        //   + 200 x 0.000015 = 0.003 + 0.003 + 0.001875 + 0.003 = 0.010875
        let priced = quote().price(&tokens(1_000, 10_000, 500, 200));
        assert_eq!(
            priced,
            CostDelta::Known {
                amount: usd("0.010875"),
                pricing_revision: revision(),
                fx: None,
            }
        );
    }

    /// Cache reads and writes are billed at materially different rates from
    /// uncached input. If the categories were folded together — which is
    /// exactly what the old two-field `Usage` forced — this would be
    /// impossible to compute, and a heavily cached conversation would be
    /// mispriced by an order of magnitude.
    #[test]
    fn the_cache_categories_are_priced_differently_from_uncached_input() {
        let same_count = 100_000;
        let uncached = quote().price(&tokens(same_count, 0, 0, 0));
        let cached = quote().price(&tokens(0, same_count, 0, 0));
        let (CostDelta::Known { amount: a, .. }, CostDelta::Known { amount: b, .. }) =
            (&uncached, &cached)
        else {
            panic!("both must be priced");
        };
        assert_eq!(*a, usd("0.3"));
        assert_eq!(*b, usd("0.03"));
        assert_ne!(
            a, b,
            "folding the categories together would mis-bill by 10x"
        );
    }

    /// A category a quote does not cover makes the *whole* figure unknown.
    /// Treating an unpriced category as free would produce an undercount
    /// presented as a fact — the single most expensive kind of wrong here.
    #[test]
    fn an_unpriced_category_makes_the_whole_cost_unknown_not_an_undercount() {
        let mut quote = quote();
        quote.cache_write = None;
        assert_eq!(
            quote.price(&tokens(1_000, 0, 500, 200)),
            CostDelta::Unknown(UnknownCost::UnpricedCategory {
                category: TokenCategory::CacheWrite
            })
        );
    }

    /// A category with a zero count is not consulted, so a quote that omits
    /// `cache_write` still prices an uncached generation exactly. Without
    /// this, any incomplete quote would poison every generation.
    #[test]
    fn a_category_with_no_tokens_does_not_need_a_price() {
        let mut quote = quote();
        quote.cache_write = None;
        assert_eq!(
            quote.price(&tokens(1_000, 0, 0, 200)),
            CostDelta::Known {
                amount: usd("0.006"),
                pricing_revision: revision(),
                fx: None,
            }
        );
    }

    // --- the upper bound --------------------------------------------------

    #[test]
    fn the_upper_bound_is_a_full_context_of_uncached_input_plus_max_output() {
        let bound = quote()
            .upper_bound(&request(Some(1_024)), &caps(Some(200_000), Some(64_000)))
            .unwrap();
        assert_eq!(bound, standard_bound());
    }

    /// The bound must exceed anything the generation can actually cost, or
    /// it is not a bound. A generation that filled the window and produced
    /// the full requested output is the worst case by construction.
    #[test]
    fn the_upper_bound_is_never_below_what_the_generation_actually_costs() {
        let quote = quote();
        let bound = quote
            .upper_bound(&request(Some(1_024)), &caps(Some(200_000), Some(64_000)))
            .unwrap();
        let worst_case = quote.price(&tokens(200_000, 0, 0, 1_024));
        let CostDelta::Known { amount: actual, .. } = worst_case else {
            panic!("the worst case must be priceable");
        };
        assert!(
            bound.amount >= actual.amount,
            "bound {} must not be below the worst case {}",
            bound.amount,
            actual.amount
        );
    }

    /// The request may cap output below the model's ceiling; the bound takes
    /// whichever is lower, because the model cannot exceed either.
    #[test]
    fn the_upper_bound_takes_the_lower_of_the_request_and_model_output_caps() {
        let quote = quote();
        let low_request = quote
            .upper_bound(&request(Some(100)), &caps(Some(200_000), Some(64_000)))
            .unwrap();
        let low_model = quote
            .upper_bound(&request(Some(64_000)), &caps(Some(200_000), Some(100)))
            .unwrap();
        assert_eq!(low_request, low_model);
        // 0.6 input + 100 x 0.000015 = 0.6015
        assert_eq!(low_request, usd("0.6015"));
    }

    /// An unbounded spend is exactly what a budget exists to prevent, so a
    /// model with no stated context window has no bound and no admission.
    #[test]
    fn a_model_with_no_stated_context_window_cannot_be_bounded() {
        assert_eq!(
            quote().upper_bound(&request(Some(1_024)), &caps(None, Some(64_000))),
            Err(UnknownCost::NoPricingQuote)
        );
    }

    #[test]
    fn a_request_with_no_output_ceiling_anywhere_cannot_be_bounded() {
        assert_eq!(
            quote().upper_bound(&request(None), &caps(Some(200_000), None)),
            Err(UnknownCost::NoPricingQuote)
        );
    }

    // --- reservation ------------------------------------------------------

    #[test]
    fn a_reservation_holds_its_bound_against_the_limit_until_reconciled() {
        let mut gate = BudgetGate::new(usd("10"));
        assert!(gate.outstanding().amount.is_zero());
        assert!(gate.committed().amount.is_zero());

        let reservation = gate
            .reserve(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("well within a $10 limit");
        assert_eq!(*reservation.upper_bound(), standard_bound());
        assert_eq!(*gate.outstanding(), standard_bound());
        assert!(gate.committed().amount.is_zero());

        let settled = gate.reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)));
        assert!(settled.exact);
        assert_eq!(settled.charged, usd("0.006"));
        assert!(
            gate.outstanding().amount.is_zero(),
            "the hold must be released"
        );
        assert_eq!(*gate.committed(), usd("0.006"));
    }

    /// The property that makes `outstanding` necessary at all. A gate that
    /// tracked only committed spend would authorise any number of concurrent
    /// generations that each fit the remaining budget alone — and then be
    /// billed for all of them.
    #[test]
    fn concurrent_reservations_accumulate_and_eventually_deny() {
        // Room for exactly two standard bounds ($1.23072), not three.
        let mut gate = BudgetGate::new(usd("1.5"));
        let quote = quote();
        let caps = caps(Some(200_000), Some(64_000));

        let first = gate.reserve(&quote, &request(Some(1_024)), &caps).unwrap();
        let _second = gate.reserve(&quote, &request(Some(1_024)), &caps).unwrap();
        assert_eq!(*gate.outstanding(), usd("1.23072"));

        let denied = gate
            .reserve(&quote, &request(Some(1_024)), &caps)
            .expect_err("a third concurrent generation must not fit");
        assert!(
            matches!(denied, BudgetDenied::WouldExceed { .. }),
            "got {denied:?}"
        );

        // Settling one cheaply frees its hold, and the third now fits.
        gate.reconcile(first, &quote.price(&tokens(10, 0, 0, 10)));
        assert!(gate.reserve(&quote, &request(Some(1_024)), &caps).is_ok());
    }

    /// `ARCHITECTURE.md` §3.1's rule, at the point where it actually bites:
    /// the denial happens before `generate` can be called, because there is
    /// no `Reservation` to call it with.
    #[test]
    fn an_unpriceable_request_fails_closed_before_any_generation() {
        let mut gate = BudgetGate::new(usd("1000000"));
        let mut quote = quote();
        quote.output = None;
        let denied = gate
            .reserve(
                &quote,
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect_err("an unpriced output category cannot be bounded");
        assert_eq!(
            denied,
            BudgetDenied::CostUnknown(UnknownCost::UnpricedCategory {
                category: TokenCategory::Output
            })
        );
        assert!(
            gate.outstanding().amount.is_zero(),
            "a denied reservation must not hold anything"
        );
    }

    /// A gate is the wrong place to invent an exchange rate, so a quote in
    /// another currency is refused rather than converted.
    #[test]
    fn a_quote_in_another_currency_is_refused_not_converted() {
        let mut gate = BudgetGate::new(usd("10"));
        let mut quote = quote();
        quote.currency = CurrencyCode::new("EUR").unwrap();
        let denied = gate
            .reserve(
                &quote,
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect_err("a EUR quote cannot be charged to a USD budget");
        assert!(matches!(denied, BudgetDenied::CurrencyMismatch { .. }));
    }

    /// A limit smaller than a single worst case admits nothing at all. That
    /// looks harsh, and is correct: the gate cannot know the generation will
    /// be cheap, and finding out costs exactly what it was trying to avoid.
    #[test]
    fn a_limit_below_one_worst_case_admits_nothing() {
        let mut gate = BudgetGate::new(usd("0.5"));
        let denied = gate
            .reserve(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect_err("$0.50 cannot cover a $0.61536 worst case");
        let BudgetDenied::WouldExceed(overrun) = denied else {
            panic!("expected WouldExceed, got {denied:?}");
        };
        // The denial names every quantity that produced it.
        assert!(overrun.committed.amount.is_zero());
        assert!(overrun.outstanding.amount.is_zero());
        assert_eq!(overrun.requested, standard_bound());
        assert_eq!(overrun.limit, usd("0.5"));
    }

    /// A bound exactly equal to the remaining budget is admitted: the limit
    /// is a ceiling, not an exclusive bound, and an off-by-one here would
    /// silently reject the last legitimate generation of every session.
    #[test]
    fn a_bound_exactly_equal_to_the_limit_is_admitted() {
        let mut gate = BudgetGate::new(standard_bound());
        assert!(
            gate.reserve(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000))
            )
            .is_ok()
        );
    }

    // --- reconciliation ---------------------------------------------------

    /// The pessimistic bound must not become the charge in the normal case,
    /// or a session would exhaust its budget at a fraction of real spend.
    #[test]
    fn reconciliation_charges_the_actual_cost_not_the_reserved_bound() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .unwrap();
        let settled = gate.reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)));
        assert_eq!(settled.charged, usd("0.006"));
        assert!(settled.exact);
        assert!(gate.committed().amount < standard_bound().amount);
    }

    /// An unknown actual cost still releases the hold — the generation *is*
    /// over — but charges the full reserved bound and says so. Leaving the
    /// hold would leak budget for the rest of the session; charging the
    /// bound can only make the gate stricter, never laxer.
    #[test]
    fn an_unknown_actual_cost_charges_the_bound_and_reports_it_as_inexact() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .unwrap();
        let settled = gate.reconcile(
            reservation,
            &CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage),
        );
        assert_eq!(settled.charged, standard_bound());
        assert!(
            !settled.exact,
            "the committed total is now an overestimate and must not claim to be the truth"
        );
        assert!(gate.outstanding().amount.is_zero());
        assert_eq!(*gate.committed(), standard_bound());
    }

    /// A cost computed under a different pricing revision cannot be matched
    /// to the authorisation that admitted it: the table changed mid-flight,
    /// so the figure is not the one that was approved. Treated exactly like
    /// an unknown cost.
    #[test]
    fn a_cost_priced_under_a_different_revision_is_not_accepted_as_exact() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .unwrap();
        let settled = gate.reconcile(
            reservation,
            &CostDelta::Known {
                amount: usd("0.006"),
                pricing_revision: PricingRevision("a-different-table".to_string()),
                fx: None,
            },
        );
        assert!(!settled.exact);
        assert_eq!(settled.charged, standard_bound());
    }

    /// Reconciliation consumes the `Reservation` by value, so one
    /// authorisation cannot be settled twice — which would double-count the
    /// spend and release twice the hold that was ever taken. This test
    /// documents the property; the compiler enforces it, and the commented
    /// line below does not compile.
    #[test]
    fn a_reservation_is_consumed_by_reconciliation_and_cannot_be_settled_twice() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .unwrap();
        gate.reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)));
        // gate.reconcile(reservation, ...);  // error[E0382]: use of moved value
        assert_eq!(*gate.committed(), usd("0.006"));
        assert!(gate.outstanding().amount.is_zero());
    }

    /// The enforcement mechanism itself, stated where a reader will look for
    /// it. `Reservation`'s fields are private and its constructor needs a
    /// type (`Sealed`) that nothing outside `ego_core::budget` can name, so
    /// neither this test nor any other code — including `ego-ai` and every
    /// future call site — can fabricate one. `BudgetGate::reserve` is the
    /// only source, and `Generator::generate` requires one, so a generation
    /// that skipped the budget does not compile.
    ///
    /// The compile-time half cannot be asserted at runtime; what this test
    /// pins is the runtime half, that the gate really is the source.
    #[test]
    fn budget_reservations_cannot_be_forged_outside_the_gate() {
        // Reservation { upper_bound: usd("0"), revision: revision() }
        //   error: cannot construct `Reservation` with struct literal
        //   syntax due to private fields
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("the gate is the only source of a Reservation");
        assert_eq!(*reservation.revision(), revision());
        assert_eq!(*reservation.upper_bound(), standard_bound());
    }

    #[test]
    fn the_limit_is_readable_for_reporting() {
        let gate = BudgetGate::new(usd("42.50"));
        assert_eq!(*gate.limit(), usd("42.50"));
    }
}
