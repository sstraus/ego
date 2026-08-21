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

use ego_core::budget::{BudgetBinding, BudgetDenied, BudgetGate, PricingQuote};
use ego_core::cost::{
    Amount, Ceiling, CostDelta, CurrencyCode, Money, PricingRevision, TokenCategory, UnknownCost,
};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
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
            reasoning_efforts: ReasoningEffortSupport::Any,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
        }
    }

    fn request(max_output_tokens: Option<u64>) -> GenRequest {
        GenRequest {
            model: "claude-opus-5".to_string(),
            messages: Conversation::new(vec![Message::text(
                Role::User,
                "list the files".to_string(),
            )])
            .expect("one user message is a conversation"),
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

    /// The bound for the standard fixture: a full 200k context at the
    /// dearest input rate the quote prices — cache write, $3.75/MTok, not
    /// uncached input's $3 — is $0.75, plus 1024 output tokens at $15/MTok,
    /// $0.01536. Written out because every limit below is chosen relative
    /// to it.
    fn standard_bound() -> Money {
        usd("0.76536")
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
    fn the_upper_bound_is_a_full_context_at_the_dearest_input_rate_plus_max_output() {
        let bound = quote()
            .upper_bound(&request(Some(1_024)), &caps(Some(200_000), Some(64_000)))
            .unwrap();
        assert_eq!(bound, standard_bound());
    }

    /// An input category this quote cannot price is one whose cost cannot be
    /// bounded, so the bound is unknown rather than quietly computed from
    /// the categories that happen to be priced. `None` means unpriced, never
    /// free — a provider that genuinely does not charge says `Some(zero)`.
    #[test]
    fn an_unpriced_input_category_leaves_the_bound_unknown() {
        let mut quote = quote();
        quote.cache_write = None;
        assert_eq!(
            quote.upper_bound(&request(Some(1_024)), &caps(Some(200_000), Some(64_000))),
            Err(UnknownCost::UnpricedCategory {
                category: TokenCategory::CacheWrite
            })
        );
    }

    /// The bound must exceed anything the generation can actually cost, or
    /// it is not a bound.
    ///
    /// Filling the window with *uncached input* is only the worst case if
    /// uncached input is the dearest input category, and it is not: in the
    /// fixture — which follows a real published table — cache writes cost
    /// $3.75/MTok against uncached input's $3. So the window is filled with
    /// each input category in turn and every one of them has to fit under
    /// the same bound. Checking only the uncached row is what let a bound
    /// that undershoots by 25% look correct.
    #[test]
    fn the_upper_bound_is_never_below_what_the_generation_actually_costs() {
        let quote = quote();
        let bound = quote
            .upper_bound(&request(Some(1_024)), &caps(Some(200_000), Some(64_000)))
            .unwrap();
        let full_window = [
            ("uncached input", tokens(200_000, 0, 0, 1_024)),
            ("cache read", tokens(0, 200_000, 0, 1_024)),
            ("cache write", tokens(0, 0, 200_000, 1_024)),
            ("a mix", tokens(50_000, 50_000, 100_000, 1_024)),
        ];
        for (label, usage) in full_window {
            let CostDelta::Known { amount: actual, .. } = quote.price(&usage) else {
                panic!("the {label} worst case must be priceable");
            };
            assert!(
                bound.amount >= actual.amount,
                "a window of {label} costs {}, which the {} bound does not cover",
                actual.amount,
                bound.amount
            );
        }
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
        // 0.75 input + 100 x 0.000015 = 0.7515
        assert_eq!(low_request, usd("0.7515"));
    }

    /// An unbounded spend is exactly what a budget exists to prevent, so a
    /// model with no stated context window has no bound and no admission.
    #[test]
    fn a_model_with_no_stated_context_window_cannot_be_bounded() {
        assert_eq!(
            quote().upper_bound(&request(Some(1_024)), &caps(None, Some(64_000))),
            Err(UnknownCost::UnstatedCeiling {
                ceiling: Ceiling::Context
            }),
            "the quote prices everything it was asked to; it is the model that \
             says nothing, and the denial has to name that and not the price"
        );
    }

    #[test]
    fn a_request_with_no_output_ceiling_anywhere_cannot_be_bounded() {
        assert_eq!(
            quote().upper_bound(&request(None), &caps(Some(200_000), None)),
            Err(UnknownCost::UnstatedCeiling {
                ceiling: Ceiling::Output
            })
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

        let settled = gate
            .reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)))
            .expect("the minting gate settles its own reservation");
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
        // Room for exactly two standard bounds ($1.53072), not three
        // ($2.29608).
        let mut gate = BudgetGate::new(usd("1.6"));
        let quote = quote();
        let caps = caps(Some(200_000), Some(64_000));

        let first = gate.reserve(&quote, &request(Some(1_024)), &caps).unwrap();
        let _second = gate.reserve(&quote, &request(Some(1_024)), &caps).unwrap();
        assert_eq!(*gate.outstanding(), usd("1.53072"));

        let denied = gate
            .reserve(&quote, &request(Some(1_024)), &caps)
            .expect_err("a third concurrent generation must not fit");
        assert!(
            matches!(denied, BudgetDenied::WouldExceed { .. }),
            "got {denied:?}"
        );

        // Settling one cheaply frees its hold, and the third now fits.
        gate.reconcile(first, &quote.price(&tokens(10, 0, 0, 10)))
            .expect("the minting gate settles its own reservation");
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
        let settled = gate
            .reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)))
            .expect("the minting gate settles its own reservation");
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
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage),
            )
            .expect("the minting gate settles its own reservation");
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
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Known {
                    amount: usd("0.006"),
                    pricing_revision: PricingRevision("a-different-table".to_string()),
                    fx: None,
                },
            )
            .expect("the minting gate settles its own reservation");
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
        gate.reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)))
            .expect("the minting gate settles its own reservation");
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

    // --- whose budget, and for what -----------------------------------------
    //
    // A `Reservation` used to prove only that *some* `reserve` ran. Since
    // `BudgetGate::new` is public, anyone could stand up a gate with a limit
    // of their choosing and obtain a genuine token from it. These tests pin
    // the two things that turn "genuine" into "authorised": which gate is
    // holding the bound, and which generation it was priced for.

    fn standard_caps() -> ModelCapabilities {
        caps(Some(200_000), Some(64_000))
    }

    /// The standard fixture request against a named model.
    fn request_for(model: &str, max_output_tokens: Option<u64>) -> GenRequest {
        GenRequest {
            model: model.to_string(),
            ..request(max_output_tokens)
        }
    }

    fn reserve_standard(gate: &mut BudgetGate) -> ego_core::budget::Reservation {
        gate.reserve(&quote(), &request(Some(1_024)), &standard_caps())
            .expect("well within the test limits")
    }

    #[test]
    fn every_gate_has_its_own_identity_and_stamps_it_on_what_it_mints() {
        let (mut a, mut b) = (BudgetGate::new(usd("10")), BudgetGate::new(usd("10")));
        assert_ne!(a.id(), b.id(), "two gates must not share an identity");
        assert_eq!(reserve_standard(&mut a).gate(), a.id());
        assert_eq!(reserve_standard(&mut b).gate(), b.id());
    }

    /// The hole this closes, and the reason the test makes gate B hold
    /// something first: `subtract_or_zero` floors at zero, so releasing a
    /// foreign bound against an *empty* gate is invisible. Against a gate
    /// with a real hold it is a silent gift of headroom — B would drop from
    /// $0.76536 outstanding to nothing and cheerfully admit another
    /// generation it never had room for.
    #[test]
    fn a_reservation_is_refused_by_a_gate_that_did_not_mint_it() {
        let mut a = BudgetGate::new(usd("10"));
        let mut b = BudgetGate::new(usd("10"));
        let from_a = reserve_standard(&mut a);
        let _b_own = reserve_standard(&mut b);
        assert_eq!(*b.outstanding(), standard_bound());

        let denied = b
            .reconcile(from_a, &quote().price(&tokens(1_000, 0, 0, 200)))
            .expect_err("gate B never minted this");
        assert_eq!(
            denied.reason,
            BudgetDenied::WrongGate {
                minted_by: a.id(),
                presented_to: b.id(),
            }
        );
        assert_eq!(
            *b.outstanding(),
            standard_bound(),
            "a refused reconciliation must not release a hold B never took"
        );
        assert!(b.committed().amount.is_zero(), "and must not charge B");

        // The authorisation is handed back intact, because B never released
        // it — dropping it here would strand A's hold for the rest of A's
        // life. It still settles, at the one gate that can settle it.
        let settled = a
            .reconcile(
                denied.reservation,
                &quote().price(&tokens(1_000, 0, 0, 200)),
            )
            .expect("A minted it, so A can settle it");
        assert!(settled.exact);
        assert!(a.outstanding().amount.is_zero());
    }

    #[test]
    fn a_reservation_authorises_the_generation_it_was_priced_for() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        assert_eq!(
            reservation.authorises(&quote(), &request(Some(1_024)), &standard_caps()),
            Ok(())
        );
        assert_eq!(reservation.binding().model(), "claude-opus-5");
        assert_eq!(reservation.binding().context_ceiling(), 200_000);
        assert_eq!(
            reservation.binding().output_ceiling(),
            1_024,
            "the request caps output below the model's 64k ceiling"
        );
    }

    #[test]
    fn a_reservation_priced_for_one_model_is_refused_for_another() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        let denied = reservation
            .authorises(
                &quote(),
                &request_for("some-other-model", Some(1_024)),
                &standard_caps(),
            )
            .expect_err("this authorised a generation against claude-opus-5");
        let BudgetDenied::WrongBinding(mismatch) = &denied else {
            panic!("expected WrongBinding, got {denied:?}");
        };
        assert_eq!(mismatch.authorised.model(), "claude-opus-5");
        assert_eq!(mismatch.presented.model(), "some-other-model");
        // ARCH §3.2: a denial names what was denied.
        let said = denied.to_string();
        assert!(
            said.contains("claude-opus-5") && said.contains("some-other-model"),
            "the denial must name the substitution: {said}"
        );
    }

    /// Moving a ceiling moves the bound, so the authorisation no longer
    /// covers what is being asked for.
    #[test]
    fn a_reservation_is_refused_when_a_ceiling_moved() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);

        let wider_window = reservation
            .authorises(
                &quote(),
                &request(Some(1_024)),
                &caps(Some(400_000), Some(64_000)),
            )
            .expect_err("a 400k window is not the 200k that was priced");
        assert!(matches!(wider_window, BudgetDenied::WrongBinding(_)));

        let more_output = reservation
            .authorises(&quote(), &request(Some(8_192)), &standard_caps())
            .expect_err("8192 output tokens is not the 1024 that was priced");
        assert!(matches!(more_output, BudgetDenied::WrongBinding(_)));
    }

    /// `PricingRevision` is an opaque public string, so a fabricated cheap
    /// table can wear a genuine label and the binding alone cannot tell.
    /// Recomputing the bound from the presented quote can — this is the
    /// check that stops a caller from having its generation authorised at
    /// real prices and then billed against pretend ones.
    #[test]
    fn a_cheaper_quote_wearing_the_authorised_revision_is_still_refused() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);

        let mut pretend = quote();
        pretend.cache_write = Some(amount("0.0000001"));
        assert_eq!(
            pretend.revision,
            revision(),
            "the fixture's whole point is a genuine label on a false table"
        );

        let denied = reservation
            .authorises(&pretend, &request(Some(1_024)), &standard_caps())
            .expect_err("the same revision does not make it the same table");
        assert_eq!(
            denied,
            BudgetDenied::QuoteChanged {
                authorised: standard_bound(),
                // 200_000 x 0.000003 (uncached input is now the dearest)
                //   + 1_024 x 0.000015
                presented: usd("0.61536"),
            }
        );
    }

    /// A deliberate *non*-property, recorded so it is not mistaken for an
    /// oversight. The bound already assumes a full context window at the
    /// dearest input rate, so no rewording of a prompt can cost more than
    /// what was authorised — and refusing one would be this type claiming an
    /// authority over content that it does not have and cannot enforce.
    #[test]
    fn rewording_the_prompt_does_not_change_what_was_authorised() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);

        let mut reworded = request(Some(1_024));
        reworded.messages = Conversation::new(vec![Message::text(
            Role::User,
            "something else entirely, and much longer".repeat(100),
        )])
        .expect("one user message is a conversation");
        assert_eq!(
            reservation.authorises(&quote(), &reworded, &standard_caps()),
            Ok(()),
            "a reservation is budget authority, not content authority"
        );
    }

    #[test]
    fn a_binding_cannot_be_built_for_a_request_with_no_stated_ceilings() {
        assert_eq!(
            BudgetBinding::of(&request(None), &caps(Some(200_000), None), &revision()),
            Err(UnknownCost::UnstatedCeiling {
                ceiling: Ceiling::Output
            })
        );
        assert_eq!(
            BudgetBinding::of(
                &request(Some(1_024)),
                &caps(None, Some(64_000)),
                &revision()
            ),
            Err(UnknownCost::UnstatedCeiling {
                ceiling: Ceiling::Context
            })
        );
    }

    // --- a bound that was not a bound ---------------------------------------

    /// 200k cache-write tokens ($0.75) plus 2048 output tokens ($0.03072) —
    /// twice the output the request asked for, which is the provider
    /// breaking its side of the contract. $0.78072 against a $0.76536 bound.
    fn over_bound_usage() -> Usage {
        tokens(0, 0, 200_000, 2_048)
    }

    /// The money is already spent, so the honest charge is the real one.
    /// `exact` stays true: the figure is perfectly well known — what is
    /// broken is the bound, and saying "inexact" would report the opposite
    /// of what happened.
    #[test]
    fn a_cost_above_the_bound_is_charged_in_full_and_reported() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        let settled = gate
            .reconcile(reservation, &quote().price(&over_bound_usage()))
            .expect("its own gate");

        assert_eq!(settled.charged, usd("0.78072"));
        assert!(
            settled.exact,
            "the actual cost is known, it is just too big"
        );
        let breach = settled
            .over_bound
            .expect("a charge above the bound must be reported, not absorbed");
        assert_eq!(breach.charged, usd("0.78072"));
        assert_eq!(breach.bound, standard_bound());
        assert_eq!(breach.excess(), usd("0.01536"));
        assert_eq!(*gate.committed(), usd("0.78072"));
    }

    /// Reporting alone would be "silently charged" one level up: the caller
    /// may ignore a field, and the gate would keep admitting generations on
    /// a premise it has just watched fail. `upper_bound` claims to be an
    /// absolute ceiling; once one is exceeded, none of them are trustworthy.
    #[test]
    fn a_breached_bound_closes_the_gate_for_good() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        gate.reconcile(reservation, &quote().price(&over_bound_usage()))
            .expect("its own gate");

        let breach = *gate.breached().expect("the gate records why it closed");
        assert_eq!(breach.excess(), usd("0.01536"));

        let denied = gate
            .reserve(&quote(), &request(Some(1_024)), &standard_caps())
            .expect_err("a gate whose bounds are not bounds admits nothing");
        assert_eq!(denied, BudgetDenied::BoundBreached(breach));
        assert!(
            denied.to_string().contains("0.01536"),
            "the denial names the breach that caused it: {denied}"
        );
        // Plenty of budget left — the refusal is about trust, not headroom.
        assert!(gate.committed().amount < gate.limit().amount);
    }

    /// A charge exactly equal to the bound is not a breach. The bound is a
    /// ceiling, and an off-by-one here would close a gate every time a
    /// generation ran to precisely its authorised worst case.
    #[test]
    fn a_cost_exactly_at_the_bound_is_not_a_breach() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Known {
                    amount: standard_bound(),
                    pricing_revision: revision(),
                    fx: None,
                },
            )
            .expect("its own gate");
        assert!(settled.over_bound.is_none());
        assert!(gate.breached().is_none());
        assert!(
            gate.reserve(&quote(), &request(Some(1_024)), &standard_caps())
                .is_ok()
        );
    }

    /// The inexact path charges the reserved bound itself, which by
    /// definition cannot exceed it. Without this, every provider that failed
    /// to report usage would look like a breached bound and close the gate.
    #[test]
    fn falling_back_to_the_bound_is_not_a_breach() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage),
            )
            .expect("its own gate");
        assert!(!settled.exact);
        assert!(settled.over_bound.is_none());
        assert!(gate.breached().is_none());
    }
}
