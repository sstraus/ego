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

use ego_core::budget::{
    BudgetBinding, BudgetDenied, BudgetGate, Ledger, Metering, MeteringIdentity, PricingQuote,
    Reserved, SettlementSource,
};
use ego_core::cost::{
    Amount, Ceiling, CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money,
    PricingRevision, ProviderCostRevision, TokenCategory, UnknownCost,
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
            web_search: None,
            revision: revision(),
            currency: CurrencyCode::new("USD").expect("USD is valid"),
            uncached_input: Some(amount("0.000003")),
            cache_read: Some(amount("0.0000003")),
            cache_write: Some(amount("0.00000375")),
            cache_write_extended: None,
            output: Some(amount("0.000015")),
        }
    }

    /// The fixture quote as a metering: priced from a table, settled from
    /// the same table. Every existing test in this file was written against
    /// that arrangement, and it stays the default here.
    fn metered() -> Metering {
        Metering::Currency {
            settlement: SettlementSource::Quote,
        }
    }

    /// The identity `metered()` binds itself to. It no longer names a
    /// revision: which table priced a bound is recorded beside the metering
    /// on the binding, not inside it.
    fn metered_identity() -> MeteringIdentity {
        MeteringIdentity::Currency {
            settlement: SettlementSource::Quote,
        }
    }

    fn caps(max_context_tokens: Option<u64>, max_output_tokens: Option<u64>) -> ModelCapabilities {
        ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens,
            max_output_tokens,
            max_tools: Some(128),
            supports_parallel_tool_calls: true,
            reasoning_carryover: ReasoningCarryover::Portable,
            reasoning_efforts: ReasoningEffortSupport::Any,
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
            image_input: ego_core::generator::capabilities::ImageInputSupport::Unknown,
            default_reasoning_effort: None,
        }
    }

    fn request(max_output_tokens: Option<u64>) -> GenRequest {
        GenRequest {
            web_search: None,
            cache: None,
            model: "claude-opus-5".to_string(),
            messages: Conversation::new(vec![Message::text(Role::User, "x".repeat(200_000))])
                .expect("one user message is a conversation"),
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            max_output_tokens,
            parallel_tool_calls: false,
        }
    }

    fn tokens(uncached_input: u64, cache_read: u64, cache_write: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            cache_write_extended_tokens: 0,
            output_tokens: output,
            web_search_requests: 0,
            reasoning_tokens: None,
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
                attribution: CostAttribution::Table {
                    revision: revision()
                },
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
                attribution: CostAttribution::Table {
                    revision: revision()
                },
                fx: None,
            }
        );
    }

    /// An extended cache write is its own rate, not a multiplier on the
    /// standard one. Anthropic prices a 1-hour write at 2x base against a
    /// 5-minute write's 1.25x, so deriving one from the other would be a
    /// guess presented as a measurement.
    #[test]
    fn an_extended_cache_write_is_priced_at_its_own_rate() {
        let mut quote = quote();
        quote.cache_write_extended = Some(amount("0.000006"));
        let usage = Usage {
            cache_write_extended_tokens: 100_000,
            ..tokens(0, 0, 0, 0)
        };
        assert_eq!(
            quote.price(&usage),
            CostDelta::Known {
                amount: usd("0.6"),
                attribution: CostAttribution::Table {
                    revision: revision()
                },
                fx: None,
            },
            "100k extended writes at $6/MTok is $0.60, not the $0.375 the \
             standard cache-write rate would give"
        );
    }

    /// The category obeys the same rule as every other: a count that was
    /// actually reported and cannot be priced makes the figure unknown. No
    /// quote prices this category today, so a provider that started
    /// reporting extended writes would make ego say so rather than undercount.
    #[test]
    fn a_reported_extended_write_with_no_rate_makes_the_cost_unknown() {
        let usage = Usage {
            cache_write_extended_tokens: 500,
            ..tokens(1_000, 0, 0, 200)
        };
        assert_eq!(
            quote().price(&usage),
            CostDelta::Unknown(UnknownCost::UnpricedCategory {
                category: TokenCategory::CacheWriteExtended
            })
        );
    }

    // --- the upper bound --------------------------------------------------

    // Catches: user-selected long caching spends above the admitted reservation.
    #[test]
    fn long_cache_reservation_covers_extended_write_spend() {
        use ego_core::event::id::SessionId;
        use ego_core::generator::types::PromptCache;
        let mut req = request(Some(1_024));
        req.messages =
            Conversation::new(vec![Message::text(Role::User, "a ".repeat(190_000))]).unwrap();
        req.cache = Some(PromptCache {
            session_id: SessionId::new(),
            lineage_root: SessionId::new(),
            retention: CacheRetention::Long,
            stable_messages: 1,
        });
        assert_eq!(
            quote().upper_bound(&req, &standard_caps()),
            Err(UnknownCost::UnpricedCategory {
                category: TokenCategory::CacheWriteExtended,
            }),
            "long retention must fail closed without its reachable rate"
        );
        let mut prices = quote();
        prices.cache_write_extended = Some(amount("0.000006"));
        let bound = prices.upper_bound(&req, &standard_caps()).unwrap();
        let CostDelta::Known { amount: actual, .. } = prices.price(&Usage {
            cache_write_extended_tokens: 180_000,
            ..tokens(0, 0, 0, 1_024)
        }) else {
            panic!("the extended write is fully priced");
        };
        assert!(
            bound.amount >= actual.amount,
            "long-cache reservation {bound:?} is below actual spend {actual:?}"
        );
    }

    // Catches: a tiny capped request reserves the model's entire context.
    #[test]
    fn short_capped_prompt_fits_a_small_currency_budget() {
        let mut short = request(Some(100));
        short.messages = Conversation::new(vec![Message::text(Role::User, "hello")]).unwrap();
        let bound = quote()
            .upper_bound(&short, &caps(Some(200_000), Some(64_000)))
            .expect("the fully priced request has a bound");
        assert!(
            bound.amount < usd("0.01").amount,
            "short prompt reserved {bound:?}"
        );
    }

    // Catches: byte-counting opaque reasoning understates provider context.
    #[test]
    fn opaque_reasoning_preserves_the_full_window_bound() {
        use ego_core::generator::types::{ContentBlock, ReasoningState};
        let mut req = request(Some(1_024));
        req.messages = Conversation::new(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning {
                state: ReasoningState {
                    blob: "encrypted".into(),
                    dialect: "provider".into(),
                },
            }],
        }])
        .unwrap();
        let bound = quote()
            .upper_bound(&req, &caps(Some(200_000), Some(64_000)))
            .unwrap();
        let CostDelta::Known { amount: actual, .. } = quote().price(&tokens(0, 0, 200_000, 1_024))
        else {
            panic!("priced usage")
        };
        assert_eq!(bound, actual);
    }

    // Catches: small image byte counts under-reserve large visual contexts.
    #[test]
    fn image_request_preserves_the_full_window_bound() {
        use ego_core::generator::types::{ContentBlock, Image, ImageMediaType};
        let image = Image::new(
            vec![
                71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 255, 255, 255, 0, 0, 0, 33, 249, 4,
                1, 0, 0, 0, 0, 44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59,
            ],
            ImageMediaType::Gif,
        )
        .unwrap();
        let mut req = request(Some(1_024));
        req.messages = Conversation::new(vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image { image }],
        }])
        .unwrap();
        let bound = quote()
            .upper_bound(&req, &caps(Some(200_000), Some(64_000)))
            .unwrap();
        let CostDelta::Known { amount: actual, .. } = quote().price(&tokens(200_000, 0, 0, 1_024))
        else {
            panic!("priced usage")
        };
        assert!(bound.amount >= actual.amount);
    }

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

    /// Short retention cannot incur an extended write, so its missing rate
    /// must not block otherwise priced requests.
    #[test]
    fn an_unpriced_extended_write_does_not_make_every_model_unboundable() {
        assert_eq!(
            quote().cache_write_extended,
            None,
            "the fixture is unpriced"
        );
        assert_eq!(
            quote()
                .upper_bound(&request(Some(1_024)), &caps(Some(200_000), Some(64_000)))
                .unwrap(),
            standard_bound(),
            "the bound must still be the dearest of the three reachable rates"
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
        assert!(
            gate.outstanding()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );
        assert!(
            gate.committed()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );

        let reservation = gate
            .reserve(
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("well within a $10 limit");
        assert_eq!(
            *reservation.upper_bound().expect("a bounded reservation"),
            standard_bound()
        );
        assert_eq!(
            *gate.outstanding().expect("a limited gate keeps totals"),
            standard_bound()
        );
        assert!(
            gate.committed()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );

        let settled = gate
            .reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)))
            .expect("the minting gate settles its own reservation");
        assert!(settled.exact);
        assert_eq!(settled.charged, Some(usd("0.006")));
        assert!(
            gate.outstanding()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero(),
            "the hold must be released"
        );
        assert_eq!(
            *gate.committed().expect("a limited gate keeps totals"),
            usd("0.006")
        );
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

        let first = gate
            .reserve(
                &metered(),
                Some(&quote.clone()),
                &request(Some(1_024)),
                &caps,
            )
            .unwrap();
        let _second = gate
            .reserve(
                &metered(),
                Some(&quote.clone()),
                &request(Some(1_024)),
                &caps,
            )
            .unwrap();
        assert_eq!(
            *gate.outstanding().expect("a limited gate keeps totals"),
            usd("1.53072")
        );

        let denied = gate
            .reserve(
                &metered(),
                Some(&quote.clone()),
                &request(Some(1_024)),
                &caps,
            )
            .expect_err("a third concurrent generation must not fit");
        assert!(
            matches!(denied, BudgetDenied::WouldExceed { .. }),
            "got {denied:?}"
        );

        // Settling one cheaply frees its hold, and the third now fits.
        gate.reconcile(first, &quote.price(&tokens(10, 0, 0, 10)))
            .expect("the minting gate settles its own reservation");
        assert!(
            gate.reserve(
                &metered(),
                Some(&quote.clone()),
                &request(Some(1_024)),
                &caps
            )
            .is_ok()
        );
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
                &metered(),
                Some(&quote.clone()),
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
            gate.outstanding()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero(),
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
                &metered(),
                Some(&quote.clone()),
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
                &metered(),
                Some(&quote()),
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
                &metered(),
                Some(&quote()),
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
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .unwrap();
        let settled = gate
            .reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)))
            .expect("the minting gate settles its own reservation");
        assert_eq!(settled.charged, Some(usd("0.006")));
        assert!(settled.exact);
        assert!(
            gate.committed()
                .expect("a limited gate keeps totals")
                .amount
                < standard_bound().amount
        );
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
                &metered(),
                Some(&quote()),
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
        assert_eq!(settled.charged, Some(standard_bound()));
        assert!(
            !settled.exact,
            "the committed total is now an overestimate and must not claim to be the truth"
        );
        assert!(
            gate.outstanding()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );
        assert_eq!(
            *gate.committed().expect("a limited gate keeps totals"),
            standard_bound()
        );
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
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .unwrap();
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Known {
                    amount: usd("0.006"),
                    attribution: CostAttribution::Table {
                        revision: PricingRevision("a-different-table".to_string()),
                    },
                    fx: None,
                },
            )
            .expect("the minting gate settles its own reservation");
        assert!(!settled.exact);
        assert_eq!(settled.charged, Some(standard_bound()));
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
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .unwrap();
        gate.reconcile(reservation, &quote().price(&tokens(1_000, 0, 0, 200)))
            .expect("the minting gate settles its own reservation");
        // gate.reconcile(reservation, ...);  // error[E0382]: use of moved value
        assert_eq!(
            *gate.committed().expect("a limited gate keeps totals"),
            usd("0.006")
        );
        assert!(
            gate.outstanding()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );
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
        // Reservation { upper_bound: usd("0"), metering: metered_identity() }
        //   error: cannot construct `Reservation` with struct literal
        //   syntax due to private fields
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("the gate is the only source of a Reservation");
        assert_eq!(
            *reservation.metering().expect("a bounded reservation"),
            metered_identity()
        );
        assert_eq!(
            *reservation.upper_bound().expect("a bounded reservation"),
            standard_bound()
        );
    }

    #[test]
    fn the_limit_is_readable_for_reporting() {
        let gate = BudgetGate::new(usd("42.50"));
        assert_eq!(
            *gate.limit().expect("a limited gate has a limit"),
            usd("42.50")
        );
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
            web_search: None,
            cache: None,
            model: model.to_string(),
            ..request(max_output_tokens)
        }
    }

    fn reserve_standard(gate: &mut BudgetGate) -> ego_core::budget::Reservation {
        gate.reserve(
            &metered(),
            Some(&quote()),
            &request(Some(1_024)),
            &standard_caps(),
        )
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
        assert_eq!(
            *b.outstanding().expect("a limited gate keeps totals"),
            standard_bound()
        );

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
            *b.outstanding().expect("a limited gate keeps totals"),
            standard_bound(),
            "a refused reconciliation must not release a hold B never took"
        );
        assert!(
            b.committed()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero(),
            "and must not charge B"
        );

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
        assert!(
            a.outstanding()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );
    }

    #[test]
    fn a_reservation_authorises_the_generation_it_was_priced_for() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        assert_eq!(
            reservation.authorises(
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &standard_caps()
            ),
            Ok(())
        );
        assert_eq!(
            reservation
                .binding()
                .expect("a bounded reservation")
                .model(),
            "claude-opus-5"
        );
        assert_eq!(
            reservation
                .binding()
                .expect("a bounded reservation")
                .context_ceiling(),
            200_000
        );
        assert_eq!(
            reservation
                .binding()
                .expect("a bounded reservation")
                .output_ceiling(),
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
                &metered(),
                Some(&quote()),
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
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &caps(Some(400_000), Some(64_000)),
            )
            .expect_err("a 400k window is not the 200k that was priced");
        assert!(matches!(wider_window, BudgetDenied::WrongBinding(_)));

        let more_output = reservation
            .authorises(
                &metered(),
                Some(&quote()),
                &request(Some(8_192)),
                &standard_caps(),
            )
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
            .authorises(
                &metered(),
                Some(&pretend),
                &request(Some(1_024)),
                &standard_caps(),
            )
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

    // Catches: a changed request reuses a reservation with another byte bound.
    #[test]
    fn changing_the_prompt_size_requires_a_fresh_reservation() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        let mut reworded = request(Some(1_024));
        reworded.messages = Conversation::new(vec![Message::text(Role::User, "shorter")]).unwrap();
        assert!(matches!(
            reservation.authorises(&metered(), Some(&quote()), &reworded, &standard_caps()),
            Err(BudgetDenied::QuoteChanged { .. })
        ));
    }

    #[test]
    fn a_binding_cannot_be_built_for_a_request_with_no_stated_ceilings() {
        assert_eq!(
            BudgetBinding::of(
                &request(None),
                &caps(Some(200_000), None),
                &metered(),
                Some(&quote())
            ),
            Err(UnknownCost::UnstatedCeiling {
                ceiling: Ceiling::Output
            })
        );
        assert_eq!(
            BudgetBinding::of(
                &request(Some(1_024)),
                &caps(None, Some(64_000)),
                &metered(),
                Some(&quote())
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

        assert_eq!(settled.charged, Some(usd("0.78072")));
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
        assert_eq!(
            *gate.committed().expect("a limited gate keeps totals"),
            usd("0.78072")
        );
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
            .reserve(
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &standard_caps(),
            )
            .expect_err("a gate whose bounds are not bounds admits nothing");
        assert_eq!(denied, BudgetDenied::BoundBreached(breach));
        assert!(
            denied.to_string().contains("0.01536"),
            "the denial names the breach that caused it: {denied}"
        );
        // Plenty of budget left — the refusal is about trust, not headroom.
        assert!(
            gate.committed()
                .expect("a limited gate keeps totals")
                .amount
                < gate.limit().expect("a limited gate has a limit").amount
        );
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
                    attribution: CostAttribution::Table {
                        revision: revision(),
                    },
                    fx: None,
                },
            )
            .expect("its own gate");
        assert!(settled.over_bound.is_none());
        assert!(gate.breached().is_none());
        assert!(
            gate.reserve(
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &standard_caps()
            )
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

    // --- the other three meterings -----------------------------------------
    //
    // `ARCHITECTURE.md` §2. Everything
    // above this line is the currency-table arrangement the gate was born
    // with. These are the three the sum-product added, and each is here
    // because it makes the gate *behave* differently — not because a variant
    // exists.

    fn policy() -> MeteringRevision {
        MeteringRevision("acme/pro-plan/2026-08".to_string())
    }

    fn provider() -> ProviderCostRevision {
        ProviderCostRevision {
            provider: "xai".to_string(),
            field: "usage.cost_in_usd_ticks".to_string(),
            schema_revision: "2026-08-21".to_string(),
        }
    }

    fn provider_reported() -> Metering {
        Metering::Currency {
            settlement: SettlementSource::ProviderReported {
                revision: provider(),
            },
        }
    }

    /// A flat fee is not spent per request, so there is nothing to hold —
    /// and a limit that could not admit one currency-metered generation
    /// admits any number of these.
    #[test]
    fn a_row_that_is_not_metered_in_currency_reserves_nothing() {
        for metering in [
            Metering::Subscription {
                revision: policy(),
                equivalent: None,
            },
            Metering::Unmetered { revision: policy() },
        ] {
            // A tenth of a cent: three orders of magnitude below the
            // $0.76536 bound the same request takes under the quote.
            let mut gate = BudgetGate::new(usd("0.001"));
            for _ in 0..100 {
                let reservation = gate
                    .reserve(&metering, None, &request(Some(1_024)), &standard_caps())
                    .expect("nothing is being charged, so nothing is being reserved");
                assert!(
                    reservation
                        .upper_bound()
                        .expect("a bounded reservation")
                        .amount
                        .is_zero()
                );
                gate.reconcile(
                    reservation,
                    &CostDelta::free(usd("0").currency, {
                        let Some(attribution) = metering.policy_attribution() else {
                            panic!("a policy row attributes its own zero");
                        };
                        attribution
                    }),
                )
                .expect("its own gate");
            }
            assert!(
                gate.outstanding()
                    .expect("a limited gate keeps totals")
                    .amount
                    .is_zero()
            );
            assert!(
                gate.committed()
                    .expect("a limited gate keeps totals")
                    .amount
                    .is_zero(),
                "{metering:?}"
            );
        }
    }

    /// The zero is minted in the *gate's* currency, because neither variant
    /// states one. A zero in some other currency would be refused by
    /// `reconcile`'s currency check and quietly charge the bound instead.
    #[test]
    fn a_policy_zero_is_minted_in_the_currency_it_will_be_compared_against() {
        let mut gate = BudgetGate::new(Money::new(
            amount("10"),
            CurrencyCode::new("JPY").expect("JPY is valid"),
        ));
        let metering = Metering::Subscription {
            revision: policy(),
            equivalent: None,
        };
        let reservation = gate
            .reserve(&metering, None, &request(Some(1_024)), &standard_caps())
            .expect("a policy row states no currency of its own to clash");
        assert_eq!(
            reservation
                .upper_bound()
                .expect("a bounded reservation")
                .currency,
            CurrencyCode::new("JPY").expect("JPY is valid")
        );

        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::free(
                    CurrencyCode::new("JPY").expect("JPY is valid"),
                    CostAttribution::MeteringPolicy {
                        revision: policy(),
                        equivalent: None,
                    },
                ),
            )
            .expect("its own gate");
        assert!(settled.exact, "a policy zero is not a fallback");
        assert!(
            gate.committed()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );
    }

    /// A row nobody charges per token is not a row whose shape may go
    /// unstated. The binding says what was authorised, and "against a model
    /// with an unknown context window" is not something anyone can check
    /// afterwards — so the ceiling refusal survives the metering change.
    #[test]
    fn a_policy_row_is_still_refused_when_a_ceiling_is_unstated() {
        let mut gate = BudgetGate::new(usd("10"));
        let denied = gate
            .reserve(
                &Metering::Unmetered { revision: policy() },
                None,
                &request(Some(1_024)),
                &caps(None, Some(64_000)),
            )
            .expect_err("a model that states no window cannot be bound to");
        assert_eq!(
            denied,
            BudgetDenied::CostUnknown(UnknownCost::UnstatedCeiling {
                ceiling: Ceiling::Context
            })
        );
    }

    /// A zero attributed to the wrong policy does not settle. The check is
    /// the same one that stops a mid-session table change from pricing one
    /// generation two ways; without it, "free" would be a universal solvent
    /// that settled any reservation exactly.
    #[test]
    fn a_zero_attributed_to_another_policy_falls_back_to_the_bound() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &Metering::Subscription {
                    revision: policy(),
                    equivalent: None,
                },
                None,
                &request(Some(1_024)),
                &standard_caps(),
            )
            .unwrap();
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::free(
                    usd("0").currency,
                    CostAttribution::MeteringPolicy {
                        revision: MeteringRevision("someone/elses-plan".to_string()),
                        equivalent: None,
                    },
                ),
            )
            .expect("its own gate");
        assert!(
            !settled.exact,
            "a figure from another policy is not this generation's cost"
        );
    }

    /// A provider-reported row still reserves from its table, because the
    /// stated figure does not exist until after the tokens are spent. This is
    /// the ADR's §1 invariant at the one place it can be observed.
    #[test]
    fn a_provider_reported_row_reserves_from_its_table_exactly_as_a_quoted_one_does() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &provider_reported(),
                Some(&quote()),
                &request(Some(1_024)),
                &standard_caps(),
            )
            .expect("the reservation quote bounds it");
        assert_eq!(
            *reservation.upper_bound().expect("a bounded reservation"),
            standard_bound()
        );
        assert_eq!(
            *gate.outstanding().expect("a limited gate keeps totals"),
            standard_bound()
        );
    }

    /// And it settles from the provider's figure whatever table minted the
    /// reservation. A provider-stated total carries no table revision at all,
    /// so applying the revision check to it would discard the best figure the
    /// gate will ever get, on every single generation.
    #[test]
    fn a_provider_stated_figure_settles_without_matching_any_table_revision() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &provider_reported(),
                Some(&quote()),
                &request(Some(1_024)),
                &standard_caps(),
            )
            .unwrap();
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Known {
                    amount: usd("0.0012345678"),
                    attribution: CostAttribution::ProviderReported(provider()),
                    fx: None,
                },
            )
            .expect("its own gate");
        assert!(settled.exact, "an invoice figure is not a guess");
        assert_eq!(settled.charged, Some(usd("0.0012345678")));
    }

    /// The exception is narrow: it is provider-*reported* figures that skip
    /// the revision check, not every figure presented to a provider-reported
    /// reservation. A table figure computed under another revision is refused
    /// here exactly as it is anywhere else.
    #[test]
    fn a_provider_reported_reservation_still_checks_a_table_figures_revision() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &provider_reported(),
                Some(&quote()),
                &request(Some(1_024)),
                &standard_caps(),
            )
            .unwrap();
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Known {
                    amount: usd("0.006"),
                    attribution: CostAttribution::Table {
                        revision: PricingRevision("a-different-table".to_string()),
                    },
                    fx: None,
                },
            )
            .expect("its own gate");
        assert!(!settled.exact);
        assert_eq!(settled.charged, Some(standard_bound()));
    }

    /// A stated cost that never arrived charges the bound and is reported
    /// inexact — the same treatment every unknown cost gets, reached through
    /// a distinct name so the log says which silence it was.
    #[test]
    fn a_missing_stated_cost_charges_the_bound_like_any_other_unknown() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = gate
            .reserve(
                &provider_reported(),
                Some(&quote()),
                &request(Some(1_024)),
                &standard_caps(),
            )
            .unwrap();
        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Unknown(UnknownCost::ProviderReportedCostMissing),
            )
            .expect("its own gate");
        assert!(!settled.exact);
        assert_eq!(settled.charged, Some(standard_bound()));
        assert!(
            settled.over_bound.is_none(),
            "the bound charged is not a breach"
        );
        assert!(
            gate.outstanding()
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );
    }

    /// A reservation binds the settlement source, not only the table. Two
    /// rows on the same price list that settle differently are two different
    /// authorisations, and swapping one for the other after admission would
    /// change which figure the gate accepts as exact.
    #[test]
    fn changing_where_the_settlement_comes_from_moves_the_binding() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        let denied = reservation
            .authorises(
                &provider_reported(),
                Some(&quote()),
                &request(Some(1_024)),
                &standard_caps(),
            )
            .expect_err("this authorised a quote-settled generation");
        let BudgetDenied::WrongBinding(mismatch) = &denied else {
            panic!("expected WrongBinding, got {denied:?}");
        };
        assert_eq!(*mismatch.authorised.metering(), metered_identity());
        // ARCH §3.2: a denial names what was denied.
        let said = denied.to_string();
        assert!(
            said.contains("the quote") && said.contains("usage.cost_in_usd_ticks"),
            "the denial must name both settlement sources: {said}"
        );
    }

    // --- a run with no ceiling -------------------------------------------
    //
    // `ARCHITECTURE.md` §§1-2. Nobody has
    // to configure a ceiling, and a run without one still reserves, settles
    // and reconciles — it simply has nothing to enforce. The four cases of
    // §2's table are exercised below, plus what the shape it mints costs the
    // caller.

    /// An unlimited ledger holds no totals, and says so by having none
    /// rather than by reporting zeroes. A zero `outstanding` on a gate with
    /// no limit is a number nothing consults and nothing checks; reporting
    /// it would invite a consumer to compare it against a limit that is not
    /// there.
    #[test]
    fn a_gate_with_no_ceiling_reports_no_limit_and_no_totals() {
        let gate = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));

        assert_eq!(gate.ledger(), &Ledger::Unlimited);
        assert_eq!(gate.limit(), None);
        assert_eq!(gate.committed(), None);
        assert_eq!(gate.outstanding(), None);
        assert_eq!(
            gate.currency(),
            CurrencyCode::new("USD").expect("USD is valid"),
            "the settlement currency survives the ceiling's absence: a flat-fee \
             row still costs an attributable zero, and a zero is denominated"
        );
    }

    /// §2, row three: `Unlimited` and a computable bound still bounds. The
    /// bound is not wasted work with nothing to compare against — it is what
    /// `reconcile` charges when the real figure is unusable, and it is a
    /// falsifiable claim about the quote and the capability record.
    #[test]
    fn a_run_with_no_ceiling_still_bounds_what_it_can_bound() {
        let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));

        let reservation = gate
            .reserve(
                &metered(),
                Some(&quote()),
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("no ceiling refuses nothing");

        assert_eq!(reservation.upper_bound(), Some(&standard_bound()));
        assert_eq!(
            gate.ledger(),
            &Ledger::Unlimited,
            "and the bound is held against nothing, because there is nothing to hold it against"
        );
    }

    /// The invariant §2 turns on, in one test: the *same* generation, with
    /// the same metering and the same missing rates, is admitted unbounded
    /// where no ceiling was configured and refused by name where one was.
    ///
    /// Neither half means anything alone. Admitting without the refusal would
    /// be a gate that never bounds; refusing without the admission is exactly
    /// the "no price, no run" rule §1 deleted. The pair is what says the
    /// operator's intent — not the state of our price tables — is what
    /// decides.
    #[test]
    fn an_unbounded_reservation_is_admitted_where_a_limited_ledger_with_no_rates_refuses() {
        let no_rates: Option<&PricingQuote> = None;

        let mut unlimited = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));
        let admitted = unlimited
            .reserve(
                &metered(),
                no_rates,
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("a run nobody asked to bound is not refused for a price nobody asked for");
        assert_eq!(
            admitted.reserved(),
            &Reserved::Unbounded {
                model: "claude-opus-5".to_string()
            },
            "and what it gets is a named absence, not a zero standing in for one"
        );

        let mut limited = BudgetGate::new(usd("10"));
        let refused = limited
            .reserve(
                &metered(),
                no_rates,
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect_err("a ceiling was asked for and cannot be honoured");
        assert_eq!(
            refused,
            BudgetDenied::CostUnknown(UnknownCost::NoPricingQuote),
            "the refusal names the missing fact"
        );
    }

    /// The same asymmetry for the other reason a bound fails: a model whose
    /// context window nobody stated. §1 says this degrades under `Unlimited`
    /// and stays fatal under `Limited`, and the two must not be allowed to
    /// converge on either answer.
    #[test]
    fn an_unstated_ceiling_is_fatal_only_where_a_ceiling_was_asked_for() {
        let no_window = caps(None, Some(64_000));

        let mut unlimited = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));
        assert!(matches!(
            unlimited
                .reserve(
                    &metered(),
                    Some(&quote()),
                    &request(Some(1_024)),
                    &no_window
                )
                .expect("nothing to enforce, so nothing to refuse")
                .reserved(),
            Reserved::Unbounded { .. }
        ));

        let mut limited = BudgetGate::new(usd("10"));
        assert_eq!(
            limited
                .reserve(
                    &metered(),
                    Some(&quote()),
                    &request(Some(1_024)),
                    &no_window
                )
                .expect_err("a ceiling cannot be honoured against an unknown context window"),
            BudgetDenied::CostUnknown(UnknownCost::UnstatedCeiling {
                ceiling: Ceiling::Context
            })
        );
    }

    /// An unbounded reservation is checked on the model, which is every fact
    /// it holds. Nothing weaker than the bounded check in what it protects:
    /// it authorised no figure, so no substitution can overspend one — but a
    /// reservation for one model must still not run another.
    #[test]
    fn an_unbounded_reservation_still_refuses_a_different_model() {
        let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));
        let reservation = gate
            .reserve(
                &metered(),
                None,
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("no ceiling refuses nothing");

        assert_eq!(reservation.model(), "claude-opus-5");
        assert_eq!(reservation.binding(), None, "there is no bound to bind");
        assert_eq!(reservation.metering(), None);
        assert!(
            reservation
                .authorises(
                    &metered(),
                    None,
                    &request(Some(1_024)),
                    &caps(Some(200_000), Some(64_000))
                )
                .is_ok(),
            "the generation it was minted for is the one it authorises"
        );

        let elsewhere = GenRequest {
            web_search: None,
            cache: None,
            model: "claude-haiku-5".to_string(),
            ..request(Some(1_024))
        };
        let denied = reservation
            .authorises(
                &metered(),
                None,
                &elsewhere,
                &caps(Some(200_000), Some(64_000)),
            )
            .expect_err("a different model is a different generation");
        assert_eq!(
            denied,
            BudgetDenied::WrongModel {
                authorised: "claude-opus-5".to_string(),
                presented: "claude-haiku-5".to_string()
            }
        );
        let said = denied.to_string();
        assert!(
            said.contains("claude-opus-5") && said.contains("claude-haiku-5"),
            "the denial names both models: {said}"
        );
    }

    /// Reconciling an unbounded reservation releases nothing — there was no
    /// hold — and charges the figure that actually arrived.
    #[test]
    fn an_unbounded_reservation_charges_what_was_stated_and_releases_nothing() {
        let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));
        let reservation = gate
            .reserve(
                &metered(),
                None,
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("no ceiling refuses nothing");

        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Known {
                    amount: usd("0.004"),
                    attribution: CostAttribution::Table {
                        revision: revision(),
                    },
                    fx: None,
                },
            )
            .expect("its own gate settles it");

        assert_eq!(settled.charged, Some(usd("0.004")));
        assert!(settled.exact, "a stated figure is not a guess");
        assert!(settled.over_bound.is_none(), "there was no bound to breach");
        assert_eq!(
            gate.ledger(),
            &Ledger::Unlimited,
            "and no total moved, because there is no total"
        );
    }

    /// And when the figure does not arrive, nothing is charged. There is no
    /// bound to fall back to, and a `Some(zero)` here would read as *free* —
    /// which is the one thing an unpriced hosted generation certainly was
    /// not. The unknown propagates instead, and poisons the run's cumulative
    /// total where a fabricated zero would have quietly completed it.
    #[test]
    fn an_unbounded_reservation_with_no_usable_cost_charges_nothing_rather_than_zero() {
        let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));
        let reservation = gate
            .reserve(
                &metered(),
                None,
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("no ceiling refuses nothing");

        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Unknown(UnknownCost::NoPricingQuote),
            )
            .expect("its own gate settles it");

        assert_eq!(settled.charged, None);
        assert!(!settled.exact);
        assert!(settled.over_bound.is_none());
    }

    /// The one check that survives on an unbounded reservation: the currency.
    /// It has no binding, so there is no authorised revision for an
    /// attribution to contradict — but a figure denominated in something the
    /// session does not settle in is still not this session's cost, and
    /// adding it to a run total would be adding euros to dollars.
    #[test]
    fn an_unbounded_reservation_still_refuses_a_cost_in_another_currency() {
        let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));
        let reservation = gate
            .reserve(
                &metered(),
                None,
                &request(Some(1_024)),
                &caps(Some(200_000), Some(64_000)),
            )
            .expect("no ceiling refuses nothing");

        let settled = gate
            .reconcile(
                reservation,
                &CostDelta::Known {
                    amount: Money::new(
                        amount("0.004"),
                        CurrencyCode::new("EUR").expect("EUR is valid"),
                    ),
                    attribution: CostAttribution::Table {
                        revision: revision(),
                    },
                    fx: None,
                },
            )
            .expect("its own gate settles it");

        assert_eq!(
            settled.charged, None,
            "euros are not this session's dollars, and there is no bound to fall back to"
        );
        assert!(!settled.exact);
    }

    /// A gate whose limit an administrator reduced remembers both figures.
    /// §4 requires the clamp be *stated, not silent*: a `--budget $20` that
    /// silently became $5 reads as binding at a figure it does not, and the
    /// only thing that knows both numbers is the ledger they live in.
    #[test]
    fn a_clamped_ceiling_remembers_the_figure_that_was_asked_for() {
        let gate = BudgetGate::clamped(usd("5"), usd("20"));

        assert_eq!(
            gate.ledger(),
            &Ledger::Limited {
                limit: usd("5"),
                clamped_from: Some(usd("20")),
                committed: usd("0"),
                outstanding: usd("0"),
            }
        );
        assert_eq!(
            BudgetGate::new(usd("5")).ledger(),
            &Ledger::Limited {
                limit: usd("5"),
                clamped_from: None,
                committed: usd("0"),
                outstanding: usd("0"),
            },
            "and an unclamped ceiling does not accidentally claim a clamp"
        );
    }

    /// And the metering variant itself is part of the binding: a currency
    /// authorisation is not a subscription one, however the ceilings line up.
    #[test]
    fn a_currency_authorisation_does_not_cover_a_policy_metered_generation() {
        let mut gate = BudgetGate::new(usd("10"));
        let reservation = reserve_standard(&mut gate);
        assert!(matches!(
            reservation.authorises(
                &Metering::Subscription {
                    revision: policy(),
                    equivalent: None
                },
                None,
                &request(Some(1_024)),
                &standard_caps()
            ),
            Err(BudgetDenied::WrongBinding(_) | BudgetDenied::QuoteChanged { .. })
        ));
    }

    /// Whether it is worth going and *finding* rates at all.
    ///
    /// `ARCHITECTURE.md` §5 says the three
    /// sources are consulted "only when a ceiling is configured", and §10
    /// says a monetary ceiling on a row with no per-token charge binds
    /// nothing. Those are not two rules: they are the single question "will
    /// a ceiling rest on rates", and it is asked here so that no caller can
    /// answer it a second way — reading a cached table, or reaching for the
    /// network, on behalf of a run that will never consult the answer.
    ///
    /// The predicate is exactly `CeilingEffect::Binds`, which is why
    /// `PublicEvent::metering_selected` is made to ask it too rather than
    /// re-deriving the same condition beside it.
    #[test]
    fn only_a_binding_money_ceiling_is_worth_looking_rates_up_for() {
        let unlimited = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is a code"));
        let limited = BudgetGate::new(usd("5"));
        let currency = Metering::Currency {
            settlement: SettlementSource::Quote,
        };

        assert!(currency.ceiling_needs_rates(limited.ledger()));
        assert!(
            !currency.ceiling_needs_rates(unlimited.ledger()),
            "a run that asked for no ceiling contacts no third party"
        );
        // A money ceiling on a row nobody charges per token for mints a zero
        // reservation, so the limit admits every generation whatever a table
        // would have said. Fetching one would be a network call whose result
        // cannot change a single decision.
        for metered_by_policy in [
            Metering::Unmetered { revision: policy() },
            Metering::Subscription {
                revision: policy(),
                equivalent: None,
            },
        ] {
            assert!(
                !metered_by_policy.ceiling_needs_rates(limited.ledger()),
                "{metered_by_policy:?} takes no rates from any source"
            );
        }
    }

    #[test]
    fn hosted_search_bound_catches_fees_and_provider_side_context_reentry() {
        let mut quote = quote();
        quote.web_search = Some(amount("0.01"));
        let mut req = request(Some(10));
        req.web_search = Some(ego_core::generator::types::HostedSearch {
            max_requests: std::num::NonZeroU32::new(5).unwrap(),
        });
        // Six model inputs of at most 100 tokens, ten output tokens, five search fees.
        assert_eq!(
            quote.upper_bound(&req, &caps(Some(100), Some(10))).unwrap(),
            usd("0.0524")
        );
        let usage = Usage {
            web_search_requests: 2,
            ..Usage::IDENTITY
        };
        assert!(
            matches!(quote.price(&usage), CostDelta::Known { amount, .. } if amount == usd("0.02"))
        );
        quote.web_search = None;
        assert_eq!(
            quote.upper_bound(&req, &caps(Some(100), Some(10))),
            Err(UnknownCost::UnpricedCategory {
                category: TokenCategory::WebSearch
            })
        );
    }
}
