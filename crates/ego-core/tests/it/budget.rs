//! Actual-spend admission and exact response pricing.
use ego_core::budget::{
    BudgetDenied, BudgetGate, Ledger, Metering, PricingQuote, SettlementSource,
};
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money, PricingRevision,
    ProviderCostRevision, TokenCategory, UnknownCost,
};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
use ego_core::state::Usage;
// The module keeps the test names `budget::<test>` that `--test budget` selected
// before the binaries were consolidated.
#[allow(clippy::module_inception)]
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

    fn caps(max_context_tokens: Option<u64>, max_output_tokens: Option<u64>) -> ModelCapabilities {
        ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
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

    /// Catches: a worst-case hold denying a cheap response under a small cap.
    #[test]
    fn real_spend_admits_below_worst_case_and_stops_only_after_cap_is_exceeded() {
        let mut gate = BudgetGate::new(usd("0.01"));
        let rates = quote();
        let req = request(Some(1_024));
        let caps = caps(Some(200_000), Some(64_000));
        for expected in ["0.006", "0.012"] {
            let admission = gate
                .admit(&metered(), Some(&rates), &req, &caps)
                .expect("recorded spend remains below the cap before this request");
            let response = rates.price(&tokens(1_000, 0, 0, 200));
            let reconciled = gate.reconcile(admission, &response).expect("same gate");
            assert_eq!(reconciled.charged, Some(usd("0.006")));
            assert_eq!(gate.committed(), Some(&usd(expected)));
        }
        let denied = gate
            .admit(&metered(), Some(&rates), &req, &caps)
            .expect_err("the request after spend crosses the cap must be refused");
        let diagnostic = denied.to_string();
        assert!(diagnostic.contains("0.012"), "{diagnostic}");
        assert!(diagnostic.contains("0.01"), "{diagnostic}");
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

    // Catches: user-selected long caching spends above the admitted admission.

    fn admit(gate: &mut BudgetGate) -> ego_core::budget::GenerationAdmission {
        gate.admit(
            &metered(),
            Some(&quote()),
            &request(None),
            &caps(None, None),
        )
        .unwrap()
    }

    /// Catches: comparing with >= denies equality or the first request at zero.
    #[test]
    fn equality_and_zero_caps_admit_until_a_response_exceeds_the_cap() {
        for cap in ["0", "0.006"] {
            let mut gate = BudgetGate::new(usd(cap));
            let first = admit(&mut gate);
            gate.reconcile(first, &quote().price(&tokens(1000, 0, 0, 200)))
                .unwrap();
            if cap == "0" {
                assert!(
                    gate.admit(
                        &metered(),
                        Some(&quote()),
                        &request(None),
                        &caps(None, None)
                    )
                    .is_err()
                );
            } else {
                let next = admit(&mut gate);
                gate.reconcile(next, &quote().price(&tokens(1000, 0, 0, 200)))
                    .unwrap();
                assert!(
                    gate.admit(&metered(), None, &request(None), &caps(None, None))
                        .is_err()
                );
            }
        }
    }

    /// Catches: a hidden monetary hold or serialized gate denies concurrent work.
    #[test]
    fn in_flight_requests_settle_after_cap_crossing_but_new_admission_stops() {
        let mut gate = BudgetGate::new(usd("0.001"));
        let first = admit(&mut gate);
        let second = admit(&mut gate);
        let cost = quote().price(&tokens(1000, 0, 0, 200));
        gate.reconcile(first, &cost).unwrap();
        assert!(
            gate.admit(&metered(), None, &request(None), &caps(None, None))
                .is_err()
        );
        gate.reconcile(second, &cost).unwrap();
        assert_eq!(gate.committed(), Some(&usd("0.012")));
    }

    /// Catches: unknown response charges an invented bound or drops the next known bill.
    #[test]
    fn unknown_cost_is_not_fabricated_and_later_known_cost_still_counts() {
        let mut gate = BudgetGate::new(usd("0.001"));
        let admission = admit(&mut gate);
        let result = gate
            .reconcile(
                admission,
                &CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage),
            )
            .unwrap();
        assert_eq!(result.charged, None);
        assert!(!result.exact);
        assert_eq!(gate.committed(), Some(&usd("0")));
        let admission = admit(&mut gate);
        gate.reconcile(admission, &quote().price(&tokens(1000, 0, 0, 200)))
            .unwrap();
        assert_eq!(gate.committed(), Some(&usd("0.006")));
    }

    /// Catches: another gate consuming an admission and misattributing its bill.
    #[test]
    fn another_gate_returns_the_admission_without_charging() {
        let mut owner = BudgetGate::new(usd("1"));
        let mut other = BudgetGate::new(usd("1"));
        let admission = admit(&mut owner);
        let cost = quote().price(&tokens(1000, 0, 0, 200));
        let refused = other.reconcile(admission, &cost).unwrap_err();
        assert!(matches!(refused.reason, BudgetDenied::WrongGate { .. }));
        assert_eq!(other.committed(), Some(&usd("0")));
        owner.reconcile(refused.admission, &cost).unwrap();
        assert_eq!(owner.committed(), Some(&usd("0.006")));
    }

    /// Catches: substituting model, billing arrangement, or same-revision rates at use.
    #[test]
    fn admission_binds_model_metering_and_exact_settlement_quote() {
        let mut gate = BudgetGate::new(usd("1"));
        let admission = admit(&mut gate);
        let mut req = request(None);
        req.model = "other".into();
        assert!(matches!(
            admission.authorises(&metered(), Some(&quote()), &req, &caps(None, None)),
            Err(BudgetDenied::WrongModel { .. })
        ));
        req.model = "claude-opus-5".into();
        let policy = Metering::Unmetered {
            revision: MeteringRevision("local".into()),
        };
        assert!(matches!(
            admission.authorises(&policy, Some(&quote()), &req, &caps(None, None)),
            Err(BudgetDenied::WrongMetering { .. })
        ));
        let mut cheaper = quote();
        cheaper.output = Some(Amount::ZERO);
        assert!(matches!(
            admission.authorises(&metered(), Some(&cheaper), &req, &caps(None, None)),
            Err(BudgetDenied::QuoteChanged)
        ));
        assert!(
            admission
                .authorises(&metered(), Some(&quote()), &req, &caps(None, None))
                .is_ok()
        );
    }

    /// Catches: a different currency or table revision being counted as a real charge.
    #[test]
    fn unusable_actual_cost_stays_unknown_without_conversion_or_estimates() {
        for (currency, revision) in [("EUR", "test-2026-08"), ("USD", "other")] {
            let mut gate = BudgetGate::new(usd("1"));
            let admission = admit(&mut gate);
            let cost = CostDelta::Known {
                amount: Money::new("0.2".parse().unwrap(), CurrencyCode::new(currency).unwrap()),
                attribution: CostAttribution::Table {
                    revision: PricingRevision(revision.into()),
                },
                fx: None,
            };
            assert_eq!(gate.reconcile(admission, &cost).unwrap().charged, None);
            assert_eq!(gate.committed(), Some(&usd("0")));
        }
    }

    /// Catches: requiring an admission quote to record a provider-stated bill.
    #[test]
    fn provider_cost_counts_without_rates_or_token_ceilings() {
        let mut gate = BudgetGate::new(usd("0.01"));
        let revision = ProviderCostRevision {
            provider: "provider".into(),
            field: "usage.cost".into(),
            schema_revision: "1".into(),
        };
        let metering = Metering::Currency {
            settlement: SettlementSource::ProviderReported {
                revision: revision.clone(),
            },
        };
        let admission = gate
            .admit(&metering, None, &request(None), &caps(None, None))
            .unwrap();
        let cost = CostDelta::Known {
            amount: usd("0.02"),
            attribution: CostAttribution::ProviderReported(revision),
            fx: None,
        };
        assert_eq!(
            gate.reconcile(admission, &cost).unwrap().charged,
            Some(usd("0.02"))
        );
        assert_eq!(gate.committed(), Some(&usd("0.02")));
        assert!(
            gate.admit(&metering, None, &request(None), &caps(None, None))
                .is_err()
        );
    }

    /// Catches: numeric overflow erasing spent money and reopening admission.
    #[test]
    fn spend_overflow_refuses_later_requests_without_fabricating_a_total() {
        let maximum = Money::new(Amount::new(u128::MAX, 0).unwrap(), usd("0").currency);
        let mut gate = BudgetGate::new(maximum);
        for cost in [maximum, usd("1")] {
            let admission = admit(&mut gate);
            gate.reconcile(
                admission,
                &CostDelta::Known {
                    amount: cost,
                    attribution: CostAttribution::Table {
                        revision: revision(),
                    },
                    fx: None,
                },
            )
            .unwrap();
        }
        assert!(matches!(
            gate.admit(&metered(), None, &request(None), &caps(None, None)),
            Err(BudgetDenied::CostUnknown(UnknownCost::Overflow))
        ));
    }

    /// Catches: losing administrative clamp reporting or inventing an unlimited cap.
    #[test]
    fn configured_clamp_and_unlimited_ledger_keep_their_meaning() {
        let gate = BudgetGate::clamped(usd("1"), usd("2"));
        assert!(
            matches!(gate.ledger(), Ledger::Limited { limit, clamped_from: Some(requested), committed } if *limit == usd("1") && *requested == usd("2") && *committed == usd("0"))
        );
        let mut gate = BudgetGate::unlimited(usd("0").currency);
        assert_eq!(gate.committed(), None);
        assert_eq!(gate.limit(), None);
        let admission = admit(&mut gate);
        assert!(
            gate.reconcile(admission, &quote().price(&tokens(1000, 0, 0, 200)))
                .unwrap()
                .exact
        );
        assert_eq!(gate.committed(), None);
    }
}
