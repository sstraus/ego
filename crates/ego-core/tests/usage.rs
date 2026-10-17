//! contract for regression case: durable usage retains role and time.

use ego_core::cost::{Amount, CostAttribution, CostDelta, CurrencyCode, Money, PricingRevision};
use ego_core::event::domain::{
    DomainEvent, Envelope, EventKind, GenerationRole, OccupancyProvenance,
};
use ego_core::event::id::{EventId, GenerationId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::{AttemptRef, DeliveryCertainty, Outcome, UsageReport};
use ego_core::generator::types::StopReason;
use ego_core::state::{ChargeAttribution, RefusalReason, SessionState, Usage};

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        uncached_input_tokens: input,
        output_tokens: output,
        ..Usage::IDENTITY
    }
}

fn cost(currency: &str, amount: &str) -> CostDelta {
    CostDelta::Known {
        amount: Money::new(
            amount.parse::<Amount>().expect("valid amount"),
            CurrencyCode::new(currency).expect("valid currency"),
        ),
        attribution: CostAttribution::Table {
            revision: PricingRevision("regression_case-fixture".to_owned()),
        },
        fx: None,
    }
}

fn event(seq: &mut Seq, ts: u64, kind: EventKind) -> DomainEvent {
    let current = *seq;
    seq.next();
    DomainEvent {
        env: Envelope {
            seq: current,
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(ts),
            producer: Producer::Main,
        },
        kind,
    }
}

fn charged_log() -> (Vec<DomainEvent>, [AttemptRef; 3]) {
    let mut seq = Seq::default();
    let main = AttemptRef::first(GenerationId::new());
    let judge = AttemptRef::first(GenerationId::new());
    let compaction = AttemptRef::first(GenerationId::new());
    let mut log = Vec::new();
    for (attempt, role, at, input, output, currency) in [
        (main, GenerationRole::Main, 1_000, 10, 2, "USD"),
        (judge, GenerationRole::Judge, 2_000, 20, 3, "EUR"),
        (compaction, GenerationRole::Compaction, 3_000, 30, 4, "USD"),
    ] {
        let observed = usage(input, output);
        log.push(event(
            &mut seq,
            at,
            EventKind::GenerationStarted {
                attempt,
                role,
                snapshot: None,
            },
        ));
        log.push(event(
            &mut seq,
            at + 1,
            EventKind::UsageUpdated {
                usage: observed,
                cost_delta: cost(currency, "0.10"),
                attempt: Some(attempt),
            },
        ));
        log.push(event(
            &mut seq,
            at + 2,
            EventKind::AttemptSettled {
                attempt,
                outcome: Outcome::new(
                    StopReason::Stop,
                    DeliveryCertainty::DefinitelyUnsent,
                    UsageReport::Reported(observed),
                ),
            },
        ));
    }
    log.push(event(
        &mut seq,
        4_000,
        EventKind::UsageCorrected {
            attempt: judge,
            usage: usage(25, 5),
            cost_delta: cost("EUR", "0.25"),
        },
    ));
    (log, [main, judge, compaction])
}

#[test]
fn durable_charges_retain_each_generation_role_and_original_timestamp_through_correction() {
    let (log, [main, judge, compaction]) = charged_log();
    let state = SessionState::replay(log);
    assert_eq!(
        state.charges.len(),
        3,
        "correction replaces one contribution"
    );
    let expected = [
        (main, GenerationRole::Main, 1_000, 10, 2, "USD"),
        (judge, GenerationRole::Judge, 2_000, 25, 5, "EUR"),
        (compaction, GenerationRole::Compaction, 3_000, 30, 4, "USD"),
    ];
    for (charge, (attempt, role, started_at, input, output, currency)) in
        state.charges.iter().zip(expected)
    {
        assert_eq!(
            &charge.attribution,
            &ChargeAttribution::Generation {
                attempt,
                role,
                started_at: Timestamp::from_millis(started_at),
            }
        );
        assert_eq!(charge.usage, usage(input, output));
        assert_eq!(
            charge.cost,
            cost(currency, if output == 5 { "0.25" } else { "0.10" })
        );
    }
}

#[test]
fn replaying_the_same_durable_usage_log_is_deterministic() {
    let (log, _) = charged_log();
    assert_eq!(SessionState::replay(log.clone()), SessionState::replay(log));
}

#[test]
fn one_reconciled_report_has_main_judge_compaction_and_total_lines_under_one_ledger() {
    let (log, _) = charged_log();
    let state = SessionState::replay(log);
    let roles: Vec<_> = state
        .charges
        .iter()
        .filter_map(|charge| match &charge.attribution {
            ChargeAttribution::Generation { role, .. } => Some(*role),
            ChargeAttribution::LegacyUnattributed => None,
        })
        .collect();
    assert_eq!(
        roles,
        [
            GenerationRole::Main,
            GenerationRole::Judge,
            GenerationRole::Compaction
        ]
    );
    assert_eq!(state.charges.len(), 3, "one gate, one corrected ledger");
}

#[test]
fn fork_spend_is_local_and_does_not_replay_source_charges_into_the_target() {
    let source = SessionId::new();
    let (source_log, _) = charged_log();
    let source_state = SessionState::replay(source_log);
    let target = SessionState::replay(vec![event(
        &mut Seq::default(),
        5_000,
        EventKind::Forked {
            source,
            at_seq: Seq::default(),
            snapshot: None,
        },
    )]);
    assert!(!source_state.charges.is_empty());
    assert!(target.charges.is_empty(), "fork copied source spend");
    assert_eq!(target.usage, Usage::IDENTITY);
}

#[test]
fn unpriced_cost_stays_unknown_and_context_occupancy_is_not_a_billed_total() {
    let mut seq = Seq::default();
    let attempt = AttemptRef::first(GenerationId::new());
    let observed = usage(100, 10);
    let events = vec![
        event(
            &mut seq,
            6_000,
            EventKind::GenerationStarted {
                attempt,
                role: GenerationRole::Main,
                snapshot: None,
            },
        ),
        event(
            &mut seq,
            6_001,
            EventKind::UsageUpdated {
                usage: observed,
                cost_delta: CostDelta::Unknown(
                    ego_core::cost::UnknownCost::ProviderReportedNoUsage,
                ),
                attempt: Some(attempt),
            },
        ),
        event(
            &mut seq,
            6_002,
            EventKind::ContextWindowUpdated {
                used_tokens: 37,
                provenance: OccupancyProvenance::AuthoritativeQuery,
                window: None,
            },
        ),
    ];
    let state = SessionState::replay(events);
    assert_eq!(state.context.used_tokens, Some(37));
    assert_eq!(
        state.context.provenance,
        Some(OccupancyProvenance::AuthoritativeQuery)
    );
    assert_eq!(state.usage, observed);
    assert_eq!(
        serde_json::to_value(&state.cost).expect("cost serializes")["state"],
        "unknown"
    );
}

#[test]
fn pre_attribution_usage_is_explicitly_legacy_but_later_unattributed_usage_is_refused() {
    let mut seq = Seq::default();
    let first = event(
        &mut seq,
        900,
        EventKind::UsageUpdated {
            usage: usage(1, 0),
            cost_delta: CostDelta::Unknown(ego_core::cost::UnknownCost::ProviderReportedNoUsage),
            attempt: None,
        },
    );
    let attempt = AttemptRef::first(GenerationId::new());
    let started = event(
        &mut seq,
        1_000,
        EventKind::GenerationStarted {
            attempt,
            role: GenerationRole::Main,
            snapshot: None,
        },
    );
    let after_attribution = event(
        &mut seq,
        1_001,
        EventKind::UsageUpdated {
            usage: usage(2, 0),
            cost_delta: CostDelta::Unknown(ego_core::cost::UnknownCost::ProviderReportedNoUsage),
            attempt: None,
        },
    );
    let state = SessionState::replay([first, started, after_attribution]);
    assert!(matches!(
        state.charges.first().map(|charge| &charge.attribution),
        Some(ChargeAttribution::LegacyUnattributed)
    ));
    assert_eq!(state.charges.len(), 1);
    assert!(
        state
            .refusals
            .iter()
            .any(|refusal| refusal.reason == RefusalReason::UnattributedUsage)
    );
}

mod report_service {
    use super::*;
    use ego_core::event::time::Clock;
    use ego_core::run::{RunAppend, RunRevision, RunTransaction};
    use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
    use ego_core::usage::{
        USAGE_REPORT_VERSION, UsagePeriod, UsageReportError, UsageReportService,
    };
    use futures::executor::block_on;
    use futures::stream::{self, BoxStream};
    use std::collections::BTreeMap;
    use std::num::NonZeroU64;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingClock {
        now: Timestamp,
        reads: Arc<AtomicUsize>,
    }

    impl Clock for CountingClock {
        fn now(&self) -> Timestamp {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.now
        }
    }

    struct FixtureStore {
        entries: BTreeMap<SessionId, Vec<SessionEntry>>,
        metadata: Vec<SessionMeta>,
        reads: Arc<AtomicUsize>,
    }

    impl SessionStore for FixtureStore {
        fn append(&self, _id: &SessionId, _event: &DomainEvent) -> Result<(), StoreError> {
            Err(StoreError::Io(std::io::Error::other(
                "fixture is read-only",
            )))
        }

        fn read(
            &self,
            id: &SessionId,
        ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let entries = self.entries.get(id).cloned().ok_or(StoreError::NotFound)?;
            Ok(Box::pin(stream::iter(entries.into_iter().map(Ok))))
        }

        fn compare_and_append_run(
            &self,
            _session: &SessionId,
            _expected_revision: Option<RunRevision>,
            _transaction: RunTransaction,
        ) -> Result<RunAppend, StoreError> {
            Err(StoreError::Io(std::io::Error::other(
                "fixture is read-only",
            )))
        }

        fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
            Ok(self.metadata.clone())
        }
    }

    fn meta(id: SessionId, model: Option<&str>, updated: u64) -> SessionMeta {
        SessionMeta {
            profiles: Default::default(),
            id,
            workspace_root: PathBuf::from("/workspace"),
            model: model.map(str::to_owned),
            created: Timestamp::from_millis(1_000),
            updated: Timestamp::from_millis(updated),
            title: None,
            summary: None,
            summary_failures: 0,
            lineage: None,
        }
    }

    #[test]
    fn service_reads_one_clock_value_and_returns_the_frozen_report_shape() {
        let session = SessionId::new();
        let reads = Arc::new(AtomicUsize::new(0));
        let clock_reads = Arc::new(AtomicUsize::new(0));
        let store = FixtureStore {
            entries: BTreeMap::from([(session, Vec::new())]),
            metadata: vec![meta(session, Some("provider/model"), 2_000)],
            reads: Arc::clone(&reads),
        };
        let clock = CountingClock {
            now: Timestamp::from_millis(2_000),
            reads: Arc::clone(&clock_reads),
        };
        let service = UsageReportService::new(&store, &clock);
        let report = block_on(service.report(&store.metadata, UsagePeriod::All))
            .expect("empty durable report");
        let json = serde_json::to_value(&report).expect("report serializes");
        assert_eq!(json["v"], USAGE_REPORT_VERSION);
        assert_eq!(json["as_of"], 2_000);
        assert_eq!(json["period"], serde_json::json!({"kind": "all"}));
        assert!(json["sessions"].is_array());
        assert!(json["totals"].is_object());
        assert_eq!(clock_reads.load(Ordering::SeqCst), 1);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    // Catches: subscription accounting zero leaking into known monetary spend.
    #[test]
    fn subscription_equivalents_are_not_known_zero_monetary_spend() {
        use ego_core::cost::{MeteringRevision, SubscriptionEquivalent};
        for metered in [None, Some("0"), Some("0.25")] {
            let session = SessionId::new();
            let mut seq = Seq::default();
            let mut entries = Vec::new();
            let subscription = CostDelta::Known {
                amount: Money::new("0".parse().unwrap(), CurrencyCode::new("USD").unwrap()),
                attribution: CostAttribution::MeteringPolicy {
                    revision: MeteringRevision("ego/builtin/openai-codex-subscription/v1".into()),
                    equivalent: Some(Box::new(SubscriptionEquivalent::V1 {
                        model: "openai-codex/gpt-6-sol".into(),
                        source: PricingRevision("fixture-standard-api".into()),
                        service_tier: "standard".into(),
                        amount: Money::new(
                            "0.011458".parse().unwrap(),
                            CurrencyCode::new("USD").unwrap(),
                        ),
                    })),
                },
                fx: None,
            };
            let mut costs = vec![subscription];
            if let Some(amount) = metered {
                costs.push(cost("USD", amount));
            }
            for delta in costs {
                let attempt = AttemptRef::first(GenerationId::new());
                entries.push(SessionEntry::Event(event(
                    &mut seq,
                    1_000,
                    EventKind::GenerationStarted {
                        attempt,
                        role: GenerationRole::Main,
                        snapshot: None,
                    },
                )));
                entries.push(SessionEntry::Event(event(
                    &mut seq,
                    1_001,
                    EventKind::UsageUpdated {
                        usage: usage(10, 2),
                        cost_delta: delta,
                        attempt: Some(attempt),
                    },
                )));
            }
            let store = FixtureStore {
                entries: BTreeMap::from([(session, entries)]),
                metadata: vec![meta(session, None, 2_000)],
                reads: Arc::new(AtomicUsize::new(0)),
            };
            let clock = CountingClock {
                now: Timestamp::from_millis(2_000),
                reads: Arc::new(AtomicUsize::new(0)),
            };
            let report = block_on(
                UsageReportService::new(&store, &clock).report(&store.metadata, UsagePeriod::All),
            )
            .unwrap();
            let json = serde_json::to_value(&report).unwrap();
            let expected_known = metered.map_or_else(
                || serde_json::json!([]),
                |amount| serde_json::json!([{"amount":amount,"currency":"USD"}]),
            );
            for line in [
                &json["totals"]["total"],
                &json["sessions"][0]["lines"]["main"],
            ] {
                assert_eq!(
                    line["cost"]["known"], expected_known,
                    "subscription zero is not monetary spend"
                );
                assert_eq!(
                    line["cost"]["subscription_equivalent"],
                    serde_json::json!([{"amount":"0.011458","currency":"USD"}])
                );
                assert_eq!(
                    line["usage"]["uncached_input_tokens"],
                    if metered.is_some() { 20 } else { 10 }
                );
            }
        }
    }

    // Catches: missing/overflowing estimates looking complete, or losing real metered zero.
    #[test]
    fn subscription_missing_and_overflow_do_not_poison_monetary_currency_totals() {
        use ego_core::cost::{MeteringRevision, SubscriptionEquivalent};
        let policy = |estimate: Option<Money>| CostDelta::Known {
            amount: Money::new("0".parse().unwrap(), CurrencyCode::new("USD").unwrap()),
            attribution: CostAttribution::MeteringPolicy {
                revision: MeteringRevision("fixture-subscription".into()),
                equivalent: estimate.map(|amount| {
                    Box::new(SubscriptionEquivalent::V1 {
                        model: "fixture-model".into(),
                        source: PricingRevision("fixture-standard".into()),
                        service_tier: "standard".into(),
                        amount,
                    })
                }),
            },
            fx: None,
        };
        let money = |currency: &str, coefficient| {
            Money::new(
                Amount::new(coefficient, 0).unwrap(),
                CurrencyCode::new(currency).unwrap(),
            )
        };
        for (costs, equivalent, unknown) in [
            (
                vec![cost("USD", "0")],
                serde_json::json!([]),
                serde_json::json!([]),
            ),
            (
                vec![cost("USD", "0"), policy(None)],
                serde_json::json!([]),
                serde_json::json!([{"reason":"no_pricing_quote"}]),
            ),
            (
                vec![
                    cost("USD", "0"),
                    policy(Some(money("USD", u128::MAX))),
                    policy(Some(money("USD", 1))),
                    policy(Some(money("USD", 2))),
                    policy(Some(money("EUR", 3))),
                ],
                serde_json::json!([{"amount":"3","currency":"EUR"}]),
                serde_json::json!([{"reason":"overflow"}]),
            ),
        ] {
            let session = SessionId::new();
            let mut seq = Seq::default();
            let mut entries = Vec::new();
            for delta in costs {
                let attempt = AttemptRef::first(GenerationId::new());
                entries.push(SessionEntry::Event(event(
                    &mut seq,
                    1_000,
                    EventKind::GenerationStarted {
                        attempt,
                        role: GenerationRole::Main,
                        snapshot: None,
                    },
                )));
                entries.push(SessionEntry::Event(event(
                    &mut seq,
                    1_001,
                    EventKind::UsageUpdated {
                        usage: usage(10, 2),
                        cost_delta: delta,
                        attempt: Some(attempt),
                    },
                )));
            }
            let store = FixtureStore {
                entries: BTreeMap::from([(session, entries)]),
                metadata: vec![meta(session, None, 2_000)],
                reads: Arc::new(AtomicUsize::new(0)),
            };
            let clock = CountingClock {
                now: Timestamp::from_millis(2_000),
                reads: Arc::new(AtomicUsize::new(0)),
            };
            let report = block_on(
                UsageReportService::new(&store, &clock).report(&store.metadata, UsagePeriod::All),
            )
            .unwrap();
            let json = serde_json::to_value(&report).unwrap();
            let actual = &json["totals"]["total"]["cost"];
            assert_eq!(
                actual["known"],
                serde_json::json!([{"amount":"0","currency":"USD"}])
            );
            assert_eq!(actual["unknown"], serde_json::json!([]));
            assert_eq!(actual["subscription_equivalent"], equivalent);
            assert_eq!(actual["subscription_unknown"], unknown);
        }
    }

    #[test]
    fn report_period_is_tagged_and_rejects_zero_seconds_at_the_type_boundary() {
        let period = UsagePeriod::Last {
            seconds: NonZeroU64::new(60).expect("positive period"),
        };
        assert_eq!(
            serde_json::to_value(period).expect("period serializes"),
            serde_json::json!({"kind": "last", "seconds": 60})
        );
        assert!(NonZeroU64::new(0).is_none());
    }

    #[test]
    fn legacy_charge_is_a_typed_service_error_before_period_projection() {
        let session = SessionId::new();
        let mut sequence = Seq::default();
        let store = FixtureStore {
            entries: BTreeMap::from([(
                session,
                vec![SessionEntry::Event(event(
                    &mut sequence,
                    1_000,
                    EventKind::UsageUpdated {
                        usage: usage(1, 0),
                        cost_delta: cost("USD", "0.01"),
                        attempt: None,
                    },
                ))],
            )]),
            metadata: vec![meta(session, None, 1_000)],
            reads: Arc::new(AtomicUsize::new(0)),
        };
        let clock = CountingClock {
            now: Timestamp::from_millis(1_000),
            reads: Arc::new(AtomicUsize::new(0)),
        };
        let service = UsageReportService::new(&store, &clock);
        assert!(matches!(
            block_on(service.report(&store.metadata, UsagePeriod::All)),
            Err(UsageReportError::UnattributedCharge { session: actual, charge_index: 0 })
                if actual == session
        ));
    }
    // Catches: cache-hit reporting double-counting corrected usage or treating cache writes as reads.
    #[test]
    fn cache_hit_uses_corrected_input_and_both_write_categories() {
        let session = SessionId::new();
        let (mut log, [attempt, _, _]) = charged_log();
        let mut seq = log.last().expect("last event").env.seq;
        seq.next();
        log.push(event(
            &mut seq,
            4_000,
            EventKind::UsageCorrected {
                attempt,
                usage: Usage {
                    uncached_input_tokens: 10,
                    cache_read_tokens: 50,
                    cache_write_tokens: 20,
                    cache_write_extended_tokens: 20,
                    output_tokens: 4,
                    ..Usage::IDENTITY
                },
                cost_delta: cost("USD", "0.01"),
            },
        ));
        let store = FixtureStore {
            entries: BTreeMap::from([(
                session,
                log.into_iter().map(SessionEntry::Event).collect(),
            )]),
            metadata: vec![meta(session, Some("provider/model"), 4_000)],
            reads: Arc::new(AtomicUsize::new(0)),
        };
        let clock = CountingClock {
            now: Timestamp::from_millis(4_000),
            reads: Arc::new(AtomicUsize::new(0)),
        };
        let report = block_on(
            UsageReportService::new(&store, &clock).report(&store.metadata, UsagePeriod::All),
        )
        .expect("report");
        assert_eq!(report.sessions[0].lines.main.cache_hit(), Some(0.5));
        let json = serde_json::to_value(&report).expect("json");
        assert_eq!(json["sessions"][0]["lines"]["main"]["cache_hit"], 0.5);
    }
}
