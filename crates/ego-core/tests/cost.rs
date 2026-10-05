//! The accounting contract: exact decimal money, the `Known`/`Unknown` cost
//! delta, and the rules that fold deltas into a cumulative session total.
//!
//! See `ARCHITECTURE.md` §3.1 and the mdkb decision `ego-acp-usage-verdict`.
//!
//! What these tests are really defending is a single claim: **a cumulative
//! cost is either exactly right or explicitly unknown, and never quietly
//! wrong**. Every case below is one route by which a plausible-looking
//! number could reach a budget gate or a bill — a rounding error, a mixed
//! currency, an unpriced generation read as free, an ordering comparison
//! that puts `1.5` above `2`.

use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CumulativeCost, CurrencyCode, FxQuote, MAX_SCALE,
    MeteringRevision, Money, MoneyError, PricingRevision, TokenCategory, UnknownCost,
};
use ego_core::event::time::Timestamp;

// Wrapped in a module named `cost` so `cargo test -p ego-core cost` matches
// by substring: cargo filters on the full test path, not the file name.
mod cost {
    use super::*;

    fn amount(decimal: &str) -> Amount {
        decimal.parse().expect("a valid test amount")
    }

    fn currency(code: &str) -> CurrencyCode {
        CurrencyCode::new(code).expect("a valid test currency")
    }

    fn money(decimal: &str, code: &str) -> Money {
        Money::new(amount(decimal), currency(code))
    }

    fn revision() -> PricingRevision {
        PricingRevision("test-2026-08".to_string())
    }

    fn known(decimal: &str, code: &str) -> CostDelta {
        CostDelta::Known {
            amount: money(decimal, code),
            attribution: CostAttribution::Table {
                revision: revision(),
            },
            fx: None,
        }
    }

    // --- currency codes ---------------------------------------------------

    #[test]
    fn currency_codes_must_be_three_uppercase_letters() {
        assert!(CurrencyCode::new("USD").is_ok());
        assert!(CurrencyCode::new("JPY").is_ok());
        for bad in ["usd", "US", "USDD", "", "US1", "US$", "ＵＳＤ"] {
            assert!(
                CurrencyCode::new(bad).is_err(),
                "{bad:?} must not be accepted as a currency code"
            );
        }
    }

    #[test]
    fn currency_codes_round_trip_as_bare_strings() {
        let json = serde_json::to_string(&currency("EUR")).unwrap();
        assert_eq!(json, "\"EUR\"");
        assert_eq!(
            serde_json::from_str::<CurrencyCode>(&json).unwrap(),
            currency("EUR")
        );
    }

    /// Validation has to survive the round trip, or a hand-edited log could
    /// smuggle in a currency that was never checked.
    #[test]
    fn deserialising_an_invalid_currency_code_fails() {
        assert!(serde_json::from_str::<CurrencyCode>("\"dollars\"").is_err());
    }

    // --- exact decimals ---------------------------------------------------

    #[test]
    fn amounts_parse_and_print_their_canonical_decimal() {
        for (input, expected) in [
            ("0", "0"),
            ("1", "1"),
            ("1.5", "1.5"),
            ("0.000003", "0.000003"),
            ("1234567.891", "1234567.891"),
        ] {
            assert_eq!(amount(input).to_string(), expected);
        }
    }

    /// Canonical form is what makes equality mean "the same value" and
    /// makes the durable spelling unique — the bytes are checksummed into
    /// the log, so two spellings of one amount would be two different
    /// records of the same fact.
    #[test]
    fn trailing_zeros_are_stripped_so_equal_values_are_equal() {
        assert_eq!(amount("1.50"), amount("1.5"));
        assert_eq!(amount("1.500000"), amount("1.5"));
        assert_eq!(amount("0.00"), amount("0"));
        assert_eq!(amount("2.0").to_string(), "2");
        assert_eq!(
            serde_json::to_string(&amount("1.50")).unwrap(),
            serde_json::to_string(&amount("1.5")).unwrap()
        );
    }

    /// A JSON *string*, not a JSON number: most readers turn a JSON number
    /// into an `f64` on sight, which destroys the exactness this whole type
    /// exists for before the value is ever used.
    #[test]
    fn amounts_serialise_as_strings_not_json_numbers() {
        assert_eq!(
            serde_json::to_string(&amount("0.001035")).unwrap(),
            "\"0.001035\""
        );
        assert_eq!(
            serde_json::from_str::<Amount>("\"0.001035\"").unwrap(),
            amount("0.001035")
        );
        // A bare number is not accepted: allowing it would let a producer
        // that skipped this type write a lossy value into the log.
        assert!(serde_json::from_str::<Amount>("0.001035").is_err());
    }

    #[test]
    fn amounts_reject_anything_that_is_not_a_plain_non_negative_decimal() {
        for bad in [
            "", ".", "1.", ".5", "-1", "+1", "1e5", "1_000", "abc", "1.2.3", " 1",
        ] {
            assert!(
                bad.parse::<Amount>().is_err(),
                "{bad:?} must not parse as an amount"
            );
        }
    }

    #[test]
    fn amounts_reject_more_precision_than_the_contract_allows() {
        let too_precise = format!("0.{}", "1".repeat(MAX_SCALE as usize + 1));
        assert_eq!(
            too_precise.parse::<Amount>(),
            Err(MoneyError::ScaleTooLarge {
                scale: MAX_SCALE + 1
            })
        );
        let at_the_limit = format!("0.{}", "1".repeat(MAX_SCALE as usize));
        assert!(at_the_limit.parse::<Amount>().is_ok());
    }

    /// Addition across different scales must be exact. `0.1 + 0.2` is the
    /// canonical demonstration that `f64` cannot do this: in binary
    /// floating point it is `0.30000000000000004`.
    #[test]
    fn addition_across_scales_is_exact() {
        assert_eq!(
            amount("0.1").checked_add(&amount("0.2")),
            Some(amount("0.3"))
        );
        assert_eq!(
            amount("1").checked_add(&amount("0.000003")),
            Some(amount("1.000003"))
        );
        assert_ne!(0.1f64 + 0.2f64, 0.3f64, "the f64 this type exists to avoid");
    }

    /// Thousands of tiny deltas are exactly the shape of a real session, and
    /// exactly where `f64` drifts. Ten thousand additions of `0.0001` must
    /// be `1`, not `0.9999999999999062`.
    #[test]
    fn accumulating_many_small_deltas_does_not_drift() {
        let delta = amount("0.0001");
        let mut total = Amount::ZERO;
        let mut float_total = 0.0f64;
        for _ in 0..10_000 {
            total = total.checked_add(&delta).expect("no overflow");
            float_total += 0.0001;
        }
        assert_eq!(total, amount("1"));
        assert_ne!(float_total, 1.0, "the f64 comparison drifts, as expected");
    }

    /// A derived `Ord` would compare `(coefficient, scale)` lexicographically
    /// and rank `1.5` — stored as `(15, 1)` — above `2`, stored as `(2, 0)`.
    /// The budget gate admits with `>`, so that is a spend hole, not a
    /// cosmetic wrong answer.
    #[test]
    fn ordering_compares_values_not_the_underlying_fields() {
        assert!(amount("1.5") < amount("2"));
        assert!(amount("0.9") < amount("1"));
        assert!(amount("10") > amount("9.999999"));
        assert_eq!(
            amount("1.50").cmp(&amount("1.5")),
            std::cmp::Ordering::Equal
        );

        let mut sorted = vec![amount("2"), amount("0.5"), amount("1.5"), amount("10")];
        sorted.sort();
        assert_eq!(
            sorted,
            vec![amount("0.5"), amount("1.5"), amount("2"), amount("10")]
        );
    }

    /// A huge coefficient at scale zero next to a small one at full scale is
    /// the case where the obvious "restate both at the wider scale"
    /// comparison overflows — and an overflow inside `cmp` has nowhere to
    /// report itself.
    #[test]
    fn ordering_does_not_overflow_on_extreme_amounts() {
        let huge = Amount::new(u128::MAX, 0).unwrap();
        let tiny = amount("0.000000000001");
        assert!(huge > tiny);
        assert!(tiny < huge);
    }

    #[test]
    fn addition_reports_overflow_rather_than_wrapping_or_saturating() {
        let huge = Amount::new(u128::MAX, 0).unwrap();
        assert_eq!(huge.checked_add(&amount("1")), None);
    }

    #[test]
    fn conversion_to_f64_happens_only_at_the_edge_and_is_named_lossy() {
        assert_eq!(amount("1.5").to_f64_lossy(), 1.5);
        assert_eq!(amount("0").to_f64_lossy(), 0.0);
        assert!((amount("0.001035").to_f64_lossy() - 0.001035).abs() < 1e-15);
    }

    // --- money ------------------------------------------------------------

    #[test]
    fn money_adds_within_a_currency_and_refuses_across_currencies() {
        assert_eq!(
            money("1.5", "USD").checked_add(&money("2.25", "USD")),
            Ok(money("3.75", "USD"))
        );
        assert_eq!(
            money("1.5", "USD").checked_add(&money("1", "EUR")),
            Err(MoneyError::CurrencyMismatch {
                left: currency("USD"),
                right: currency("EUR"),
            })
        );
    }

    // --- fx ---------------------------------------------------------------

    fn eur_to_usd(rate: &str) -> FxQuote {
        FxQuote {
            from: currency("EUR"),
            to: currency("USD"),
            rate: amount(rate),
            quoted_at: Timestamp::from_millis(1_787_000_000_000),
        }
    }

    #[test]
    fn an_fx_quote_converts_into_its_target_currency() {
        let converted = eur_to_usd("1.1").convert(&money("10", "EUR")).unwrap();
        assert_eq!(converted, money("11", "USD"));
    }

    /// A quote is not a general-purpose converter. Applying `EUR→USD` to a
    /// `GBP` amount would produce a plausible number that means nothing.
    #[test]
    fn an_fx_quote_refuses_an_amount_in_the_wrong_source_currency() {
        assert!(eur_to_usd("1.1").convert(&money("10", "GBP")).is_err());
    }

    // --- cost deltas ------------------------------------------------------

    /// Zero is a price. A local model that costs nothing must report
    /// `Known(0)`, because reporting `Unknown` would poison the session
    /// total for no reason.
    ///
    /// And the zero is attributed to the *policy* that produced it, not to a
    /// price list: there is no table saying "zero per token" here, and
    /// spelling one would be the confusion
    /// `ARCHITECTURE.md` §4 exists to
    /// prevent.
    #[test]
    fn a_free_generation_is_known_zero_not_unknown() {
        let policy = MeteringRevision("local-unmetered".to_string());
        let free = CostDelta::free(
            currency("USD"),
            CostAttribution::MeteringPolicy {
                revision: policy.clone(),
            },
        );
        let CostDelta::Known {
            amount,
            attribution,
            ..
        } = &free
        else {
            panic!("a free generation must be Known");
        };
        assert!(amount.amount.is_zero());
        assert_eq!(
            *attribution,
            CostAttribution::MeteringPolicy { revision: policy },
            "a zero has to name the policy that produced it, not a price list"
        );

        let mut total = CumulativeCost::default();
        total.add_delta(&free);
        assert_eq!(
            total,
            CumulativeCost::Known {
                total: money("0", "USD")
            }
        );
    }

    // --- the cumulative fold ---------------------------------------------

    #[test]
    fn an_untouched_session_has_no_total_and_no_settlement_currency() {
        assert_eq!(CumulativeCost::default(), CumulativeCost::Unstarted);
        assert_eq!(CumulativeCost::default().known(), None);
    }

    #[test]
    fn known_deltas_sum_exactly_in_one_currency() {
        let mut total = CumulativeCost::default();
        total.add_delta(&known("0.001035", "USD"));
        total.add_delta(&known("0.000105", "USD"));
        assert_eq!(
            total,
            CumulativeCost::Known {
                total: money("0.00114", "USD")
            }
        );
    }

    /// The headline rule of criterion 4. One unpriced generation makes the
    /// *session* total unknown — not zero, not "the sum of the ones we could
    /// price", both of which would understate a real bill.
    #[test]
    fn one_unknown_delta_poisons_the_cumulative_total() {
        let mut total = CumulativeCost::default();
        total.add_delta(&known("1.50", "USD"));
        total.add_delta(&CostDelta::Unknown(UnknownCost::NoPricingQuote));
        assert_eq!(
            total,
            CumulativeCost::Unknown(UnknownCost::NoPricingQuote),
            "a gap in the sum must be visible, not absorbed"
        );
        assert_eq!(total.known(), None);
    }

    /// The reason kept is the one that broke the total, not the most recent
    /// one to arrive. An operator asking "why is this session's cost
    /// unknown?" needs the root cause: "we have no pricing table for this
    /// model" is actionable, while a later "the provider reported no usage"
    /// is just the next symptom, and overwriting hides the first behind it.
    #[test]
    fn the_reason_a_total_went_unknown_is_the_first_one_not_the_last() {
        let mut total = CumulativeCost::default();
        total.add_delta(&known("1.50", "USD"));
        total.add_delta(&CostDelta::Unknown(UnknownCost::NoPricingQuote));
        total.add_delta(&CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage));
        assert_eq!(
            total,
            CumulativeCost::Unknown(UnknownCost::NoPricingQuote),
            "the root cause must survive later symptoms"
        );
    }

    /// And it stays unknown: adding a later *known* delta cannot repair a
    /// sum with a hole in it, because nobody knows how big the hole was.
    #[test]
    fn an_unknown_total_stays_unknown_under_later_known_deltas() {
        let mut total = CumulativeCost::default();
        total.add_delta(&CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage));
        for _ in 0..5 {
            total.add_delta(&known("1.00", "USD"));
        }
        assert_eq!(
            total,
            CumulativeCost::Unknown(UnknownCost::ProviderReportedNoUsage)
        );
    }

    /// The only way out, and deliberately a replacement rather than an
    /// addition: something authoritative states the total outright.
    #[test]
    fn reconciliation_is_the_only_escape_from_unknown() {
        let mut total = CumulativeCost::default();
        total.add_delta(&CostDelta::Unknown(UnknownCost::NoPricingQuote));
        total.reconcile(money("7.25", "USD"));
        assert_eq!(
            total,
            CumulativeCost::Known {
                total: money("7.25", "USD")
            }
        );
        // And accounting resumes normally from the reconciled figure.
        total.add_delta(&known("0.75", "USD"));
        assert_eq!(
            total,
            CumulativeCost::Known {
                total: money("8", "USD")
            }
        );
    }

    /// Criterion 6. Two currencies with no quote must never be added — the
    /// numbers are not commensurable, and a sum of them is meaningless
    /// while looking entirely ordinary.
    #[test]
    fn mixed_currencies_without_a_quote_mark_the_total_unknown_never_add() {
        let mut total = CumulativeCost::default();
        total.add_delta(&known("10.00", "USD"));
        total.add_delta(&known("10.00", "EUR"));
        assert_eq!(
            total,
            CumulativeCost::Unknown(UnknownCost::UnconvertibleCurrency {
                found: currency("EUR")
            })
        );
        assert_ne!(
            total,
            CumulativeCost::Known {
                total: money("20.00", "USD")
            },
            "20 of nothing in particular is the answer this rule exists to prevent"
        );
    }

    /// With a persisted quote, the same delta settles exactly. The quote
    /// rides the event, so replaying the log a year later produces the same
    /// total — which is the reason it is persisted rather than looked up.
    #[test]
    fn a_persisted_fx_quote_resolves_a_foreign_currency_delta() {
        let mut total = CumulativeCost::default();
        total.add_delta(&known("10.00", "USD"));
        total.add_delta(&CostDelta::Known {
            amount: money("10.00", "EUR"),
            attribution: CostAttribution::Table {
                revision: revision(),
            },
            fx: Some(eur_to_usd("1.1")),
        });
        assert_eq!(
            total,
            CumulativeCost::Known {
                total: money("21", "USD")
            }
        );
    }

    /// A quote that converts into some third currency does not help: the
    /// result still cannot be added to the session's settlement currency,
    /// and applying it anyway would be the silent mixing this forbids.
    #[test]
    fn an_fx_quote_into_the_wrong_currency_still_marks_the_total_unknown() {
        let mut total = CumulativeCost::default();
        total.add_delta(&known("10.00", "USD"));
        total.add_delta(&CostDelta::Known {
            amount: money("10.00", "EUR"),
            attribution: CostAttribution::Table {
                revision: revision(),
            },
            fx: Some(FxQuote {
                from: currency("EUR"),
                to: currency("GBP"),
                rate: amount("0.85"),
                quoted_at: Timestamp::from_millis(1_787_000_000_000),
            }),
        });
        assert!(matches!(total, CumulativeCost::Unknown(_)));
    }

    /// The first priced generation establishes the settlement currency —
    /// there is nothing to convert *to* before it, so a session that begins
    /// in EUR simply settles in EUR.
    #[test]
    fn the_first_known_delta_fixes_the_settlement_currency() {
        let mut total = CumulativeCost::default();
        total.add_delta(&known("5.00", "EUR"));
        total.add_delta(&known("2.50", "EUR"));
        assert_eq!(
            total,
            CumulativeCost::Known {
                total: money("7.50", "EUR")
            }
        );
    }

    /// An unrepresentable total is reported as unknown rather than
    /// saturating: a total pinned at the maximum would read to a budget gate
    /// as a real, enormous number.
    #[test]
    fn an_overflowing_total_becomes_unknown_rather_than_saturating() {
        let huge = Money::new(Amount::new(u128::MAX, 0).unwrap(), currency("USD"));
        let mut total = CumulativeCost::default();
        total.add_delta(&CostDelta::Known {
            amount: huge,
            attribution: CostAttribution::Table {
                revision: revision(),
            },
            fx: None,
        });
        total.add_delta(&known("1", "USD"));
        assert_eq!(total, CumulativeCost::Unknown(UnknownCost::Overflow));
    }

    // --- serde ------------------------------------------------------------

    /// Every one of these types is written into a checksummed log line, so
    /// each must survive a round trip unchanged.
    #[test]
    fn the_accounting_types_round_trip_through_json() {
        let deltas = [
            known("0.001035", "USD"),
            CostDelta::Known {
                amount: money("10.00", "EUR"),
                attribution: CostAttribution::Table {
                    revision: revision(),
                },
                fx: Some(eur_to_usd("1.1")),
            },
            CostDelta::Unknown(UnknownCost::NoPricingQuote),
            CostDelta::Unknown(UnknownCost::UnpricedCategory {
                category: TokenCategory::CacheWrite,
            }),
            CostDelta::Unknown(UnknownCost::UnconvertibleCurrency {
                found: currency("JPY"),
            }),
            CostDelta::Unknown(UnknownCost::Overflow),
        ];
        for delta in deltas {
            let json = serde_json::to_string(&delta).unwrap();
            assert_eq!(serde_json::from_str::<CostDelta>(&json).unwrap(), delta);
        }

        for total in [
            CumulativeCost::Unstarted,
            CumulativeCost::Known {
                total: money("1.25", "USD"),
            },
            CumulativeCost::Unknown(UnknownCost::NoPricingQuote),
        ] {
            let json = serde_json::to_string(&total).unwrap();
            assert_eq!(
                serde_json::from_str::<CumulativeCost>(&json).unwrap(),
                total
            );
        }
    }

    /// `fx` is absent from the wire for the overwhelmingly common
    /// single-currency case, so the log does not carry a null on every
    /// priced generation.
    #[test]
    fn a_delta_without_an_fx_quote_omits_the_field_entirely() {
        let json = serde_json::to_string(&known("0.5", "USD")).unwrap();
        assert!(!json.contains("fx"), "{json}");
        assert!(json.contains("\"cost\":\"known\""), "{json}");
    }
}
