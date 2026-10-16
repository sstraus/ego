//! contract for regression case: the status row reads as a sentence for an operator —
//! model, effort, mode, context as a share of the window, and money — not as
//! `key=value` pairs, and it says nothing about trust unless trust needs
//! attention.

use ego_core::config::{Mode, ModelRef, Trust};
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CumulativeCost, CurrencyCode, Money, PricingRevision,
    UnknownCost,
};
use ego_core::event::domain::{
    DomainEvent, Envelope, EventKind, GenerationRole, OccupancyProvenance,
};
use ego_core::event::id::{EventId, GenerationId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::types::ThinkingLevel;
use ego_core::state::{QueueStatus, SessionState, Usage};

use ego_cli::repl::status::{
    SeatFacts, SelectionFacts, StatusLine, StatusSegmentName, SurfaceSnapshot,
};

fn money(currency: &str, amount: &str) -> Money {
    Money::new(
        amount.parse::<Amount>().expect("valid amount"),
        CurrencyCode::new(currency).expect("valid currency"),
    )
}

fn state(used: Option<u64>, window: Option<u64>, cost: CumulativeCost) -> SessionState {
    let mut state = SessionState::default();
    state.context.used_tokens = used;
    state.context.window = window;
    state.cost = cost;
    state
}

fn snapshot_for(state: &SessionState, trust: Trust) -> SurfaceSnapshot {
    SurfaceSnapshot::from_state(
        SessionId::new(),
        state,
        SelectionFacts::new(
            ModelRef("openrouter/z-ai/glm-5.3-flash".into()),
            ThinkingLevel::Low,
            Mode::Default,
        ),
        SeatFacts::new(QueueStatus::default(), 0, "/workspace/ego", trust),
        None,
    )
}

fn row(state: &SessionState, names: &[StatusSegmentName], width: usize) -> String {
    StatusLine::render(&snapshot_for(state, Trust::Trusted), names, width)
        .text()
        .to_owned()
}

#[test]
fn context_bar_and_counts_show_occupancy_without_hiding_the_window() {
    let names = [StatusSegmentName::Context];
    // 3_000 of 1_000_000 is 0.3%; 42_000 of 200_000 is 21%; 500 of 128_000
    // is well under a tenth of a percent.
    for (used, window, expected) in [
        (3_000, 1_000_000, "▱▱▱▱▱▱▱▱▱▱ 3k/1M"),
        (42_000, 200_000, "▰▰▱▱▱▱▱▱▱▱ 42k/200k"),
        (500, 128_000, "▱▱▱▱▱▱▱▱▱▱ 500/128k"),
        (1_000_000, 1_000_000, "▰▰▰▰▰▰▰▰▰▰ 1M/1M"),
    ] {
        let shown = row(
            &state(Some(used), Some(window), CumulativeCost::Unstarted),
            &names,
            120,
        );
        assert_eq!(shown, expected, "used={used} window={window}");
        assert!(!shown.contains("context="), "{shown}");
    }
}

#[test]
fn context_without_a_stated_window_shows_the_count_and_never_a_percentage() {
    let names = [StatusSegmentName::Context];
    let unbounded = row(
        &state(Some(2_482), None, CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert_eq!(unbounded, "▱▱▱▱▱▱▱▱▱▱ 2.5k/?");
    let nothing_yet = row(&state(None, None, CumulativeCost::Unstarted), &names, 120);
    assert_eq!(nothing_yet, "▱▱▱▱▱▱▱▱▱▱ ?/?");
    let window_only = row(
        &state(None, Some(1_000_000), CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert_eq!(window_only, "▱▱▱▱▱▱▱▱▱▱ ?/1M");
}

#[test]
fn recorded_cost_shows_with_a_currency_mark() {
    let names = [StatusSegmentName::Cost];
    for (currency, amount, expected) in [
        ("USD", "0.00156247", "$0.002"),
        ("USD", "2.5", "$2.50"),
        ("USD", "0", "$0.000"),
        ("USD", "0.0000164", "<$0.001"),
        ("EUR", "0.5", "0.50 EUR"),
    ] {
        let known = CumulativeCost::Known {
            total: money(currency, amount),
        };
        assert_eq!(
            row(&state(None, None, known), &names, 120),
            expected,
            "{currency} {amount}"
        );
    }
}

#[test]
fn an_unpriced_or_unstarted_cost_is_absent_rather_than_a_false_zero() {
    let names = [StatusSegmentName::Model, StatusSegmentName::Cost];
    for cost in [
        CumulativeCost::Unknown(UnknownCost::ProviderReportedNoUsage),
        CumulativeCost::Unstarted,
    ] {
        let shown = row(&state(None, None, cost.clone()), &names, 120);
        let expected = if matches!(cost, CumulativeCost::Unknown(_)) {
            "glm-5.3-flash unpriced"
        } else {
            "glm-5.3-flash"
        };
        assert_eq!(shown, expected);
    }
}

#[test]
fn trust_is_silent_for_a_trusted_workspace_and_named_when_it_needs_attention() {
    let names = [StatusSegmentName::Model, StatusSegmentName::Trust];
    let facts = state(None, None, CumulativeCost::Unstarted);

    let trusted = StatusLine::render(&snapshot_for(&facts, Trust::Trusted), &names, 240);
    assert_eq!(trusted.text(), "glm-5.3-flash");

    let untrusted = StatusLine::render(&snapshot_for(&facts, Trust::Untrusted), &names, 240);
    assert_eq!(untrusted.text(), "glm-5.3-flash · trust=untrusted");
}

#[test]
fn the_row_degrades_by_dropping_lower_priority_fields_and_keeps_context() {
    let names = [
        StatusSegmentName::Model,
        StatusSegmentName::Effort,
        StatusSegmentName::Mode,
        StatusSegmentName::Context,
        StatusSegmentName::Cost,
        StatusSegmentName::Cwd,
    ];
    let facts = state(
        Some(3_000),
        Some(1_000_000),
        CumulativeCost::Known {
            total: money("USD", "0.00156247"),
        },
    );
    let surface = snapshot_for(&facts, Trust::Trusted);
    for (width, expected) in [
        (
            120,
            "glm-5.3-flash · low · default ▱▱▱▱▱▱▱▱▱▱ 3k/1M $0.002 /w/ego",
        ),
        (
            80,
            "glm-5.3-flash · low · default ▱▱▱▱▱▱▱▱▱▱ 3k/1M $0.002 /w/ego",
        ),
        (
            60,
            "glm-5.3-flash · low · default ▱▱▱▱▱▱▱▱▱▱ 3k/1M $0.002 /w/ego",
        ),
        (40, "glm-5.3-flash · low ▱▱▱▱▱▱▱▱▱▱ 3k/1M"),
    ] {
        let line = StatusLine::render(&surface, &names, width);
        assert_eq!(line.text(), expected, "width {width}");
        assert!(line.width() <= width);
        assert!(!line.text().contains('\n'));
    }
}

#[test]
fn context_and_cost_follow_the_durable_usage_events_of_the_session() {
    let mut seq = Seq::default();
    let attempt = AttemptRef::first(GenerationId::new());
    let mut log = Vec::new();
    let mut push = |ts: u64, kind: EventKind| {
        let current = seq;
        seq.next();
        log.push(DomainEvent {
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
        });
    };
    push(
        1,
        EventKind::GenerationStarted {
            attempt,
            role: GenerationRole::Main,
            snapshot: None,
        },
    );
    push(
        2,
        EventKind::UsageUpdated {
            usage: Usage {
                uncached_input_tokens: 7_000,
                output_tokens: 10,
                ..Usage::IDENTITY
            },
            cost_delta: CostDelta::Known {
                amount: money("USD", "0.0124"),
                attribution: CostAttribution::Table {
                    revision: PricingRevision("regression_case-fixture".to_owned()),
                },
                fx: None,
            },
            attempt: Some(attempt),
        },
    );
    push(
        3,
        EventKind::ContextWindowUpdated {
            used_tokens: 7_010,
            provenance: OccupancyProvenance::AuthoritativeQuery,
            window: Some(1_000_000),
        },
    );

    let replayed = SessionState::replay(log);
    let names = [StatusSegmentName::Context, StatusSegmentName::Cost];
    assert_eq!(row(&replayed, &names, 120), "▱▱▱▱▱▱▱▱▱▱ 7k/1M $0.012");
}

#[test]
fn idle_queue_drafts_and_interrupt_do_not_occupy_the_row() {
    let names = [
        StatusSegmentName::Model,
        StatusSegmentName::Queue,
        StatusSegmentName::Drafts,
        StatusSegmentName::Interrupt,
    ];
    let facts = state(None, None, CumulativeCost::Unstarted);
    let mut surface = snapshot_for(&facts, Trust::Trusted);
    assert_eq!(
        StatusLine::render(&surface, &names, 120).text(),
        "glm-5.3-flash"
    );

    surface.set_drafts(2);
    assert_eq!(
        StatusLine::render(&surface, &names, 120).text(),
        "glm-5.3-flash · drafts=2"
    );
}

#[test]
fn context_outlives_an_untrusted_posture_under_width_pressure() {
    // At 40 cells, trust cannot fit beside model and the complete context meter.
    let names = [
        StatusSegmentName::Model,
        StatusSegmentName::Context,
        StatusSegmentName::Trust,
    ];
    let facts = state(Some(3_000), Some(1_000_000), CumulativeCost::Unstarted);
    let surface = snapshot_for(&facts, Trust::Untrusted);

    let narrow = StatusLine::render(&surface, &names, 40);
    assert_eq!(narrow.text(), "glm-5.3-flash ▱▱▱▱▱▱▱▱▱▱ 3k/1M");
    assert!(narrow.omitted(StatusSegmentName::Trust));

    let wide = StatusLine::render(&surface, &names, 60);
    assert_eq!(
        wide.text(),
        "glm-5.3-flash ▱▱▱▱▱▱▱▱▱▱ 3k/1M trust=untrusted"
    );
}

#[test]
fn a_zero_window_never_divides_and_falls_back_to_the_count() {
    let names = [StatusSegmentName::Context];
    let shown = row(
        &state(Some(2_482), Some(0), CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert_eq!(shown, "▱▱▱▱▱▱▱▱▱▱ 2.5k/?");
}

#[test]
fn context_beyond_the_window_saturates_the_bar_but_preserves_the_count() {
    let names = [StatusSegmentName::Context];
    let over = row(
        &state(Some(150_000), Some(100_000), CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert_eq!(over, "▰▰▰▰▰▰▰▰▰▰ 150k/100k");
    let extreme = row(
        &state(Some(u64::MAX), Some(1), CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert!(
        extreme.starts_with("▰▰▰▰▰▰▰▰▰▰ ") && extreme.ends_with("/1"),
        "{extreme}"
    );
}

// Replay the same settlement facts consumed by the live status snapshot.
fn replay_costs(costs: impl IntoIterator<Item = CostDelta>) -> SessionState {
    let mut seq = Seq::default();
    SessionState::replay(costs.into_iter().map(|cost_delta| {
        let current = seq;
        seq.next();
        DomainEvent {
            env: Envelope {
                seq: current,
                id: EventId::new(),
                turn: TurnId::new(),
                item: None,
                parent: None,
                ts: Timestamp::from_millis(1),
                producer: Producer::Main,
            },
            kind: EventKind::UsageUpdated {
                usage: Usage {
                    uncached_input_tokens: 1000,
                    ..Usage::IDENTITY
                },
                cost_delta,
                attempt: None,
            },
        }
    }))
}

fn subscription_cost() -> CostDelta {
    ego_core::generator::attempt::settled_cost(
        &ego_core::budget::Metering::Subscription {
            revision: ego_core::cost::MeteringRevision(
                "ego/builtin/openai-codex-subscription/v1".into(),
            ),
            equivalent: None,
        },
        None,
        ego_core::generator::attempt::UsageReport::Reported(Usage {
            uncached_input_tokens: 1000,
            ..Usage::IDENTITY
        }),
        None,
        CurrencyCode::new("USD").unwrap(),
    )
}

#[test]
fn subscription_policy_zero_is_not_displayed_as_currency() {
    let state = replay_costs([subscription_cost()]);
    assert_eq!(row(&state, &[StatusSegmentName::Cost], 120), "unpriced");
}

fn estimated_cost(amount: &str) -> CostDelta {
    let mut cost = subscription_cost();
    if let CostDelta::Known {
        attribution: CostAttribution::MeteringPolicy { equivalent, .. },
        ..
    } = &mut cost
    {
        *equivalent = Some(Box::new(ego_core::cost::SubscriptionEquivalent::V1 {
            model: "openai-codex/gpt-6.1-sol".into(),
            source: PricingRevision("official-model-page-2026-10-16".into()),
            service_tier: "standard".into(),
            amount: money("USD", amount),
        }));
    }
    cost
}

#[test]
fn subscription_equivalent_is_labelled_and_does_not_become_budget_spend() {
    let state = replay_costs([estimated_cost("0.002"), estimated_cost("0.0001")]);
    assert!(state.cost.known().unwrap().amount.is_zero());
    assert_eq!(row(&state, &[StatusSegmentName::Cost], 120), "≈$0.002");
    assert_eq!(
        row(
            &replay_costs([estimated_cost("0.661")]),
            &[StatusSegmentName::Cost],
            120
        ),
        "≈$0.66"
    );
}

#[test]
fn mixed_or_unpriced_generations_cannot_look_like_a_complete_estimate() {
    for costs in [
        vec![estimated_cost("0.002"), subscription_cost()],
        vec![subscription_cost(), estimated_cost("0.002")],
        vec![
            estimated_cost("0.002"),
            CostDelta::Unknown(UnknownCost::NoPricingQuote),
        ],
        vec![
            estimated_cost("0.002"),
            CostDelta::Known {
                amount: money("USD", "1"),
                attribution: CostAttribution::Table {
                    revision: PricingRevision("test".into()),
                },
                fx: None,
            },
        ],
    ] {
        assert_eq!(
            row(&replay_costs(costs), &[StatusSegmentName::Cost], 120),
            "≈$0.002 (partial)"
        );
    }
}

#[test]
fn legacy_policy_attribution_decodes_without_an_equivalent_price() {
    let legacy =
        r#"{"source":"metering_policy","revision":"ego/builtin/openai-codex-subscription/v1"}"#;
    let attribution: CostAttribution = serde_json::from_str(legacy).unwrap();
    assert!(matches!(
        attribution,
        CostAttribution::MeteringPolicy {
            equivalent: None,
            ..
        }
    ));
    let cost = estimated_cost("0.002");
    let bytes = serde_json::to_string(&cost).unwrap();
    assert!(bytes.contains("v1"));
    assert_eq!(serde_json::from_str::<CostDelta>(&bytes).unwrap(), cost);
}

// Catches: missing occupancy renders unexplained punctuation or a fabricated zero.
#[test]
fn rendered_status_explains_unmeasured_context() {
    let names = [StatusSegmentName::Context, StatusSegmentName::Cost];
    for (used, window, expected) in [
        (None, None, "▱▱▱▱▱▱▱▱▱▱ ?/?"),
        (None, Some(1_000_000), "▱▱▱▱▱▱▱▱▱▱ ?/1M"),
        (Some(7_010), Some(1_000_000), "▱▱▱▱▱▱▱▱▱▱ 7k/1M"),
    ] {
        let facts = state(used, window, CumulativeCost::Unstarted);
        for width in [80, 120, 200] {
            let shown = row(&facts, &names, width);
            let mut terminal = vt100::Parser::new(4, width as u16, 0);
            terminal.process(shown.as_bytes());
            assert_eq!(terminal.screen().contents(), expected, "width={width}");
        }
        for width in 1..25 {
            let shown = row(&facts, &names, width);
            assert!(
                shown.is_empty() || shown == expected,
                "width={width}: {shown}"
            );
        }
    }
    // A resumed log without any occupancy event has no measurement to invent.
    let resumed = SessionState::replay(Vec::<DomainEvent>::new());
    assert_eq!(row(&resumed, &names, 80), "▱▱▱▱▱▱▱▱▱▱ ?/?");
    // Compaction creates a checkpoint-seeded session, not a zero occupancy event.
    let compacted = SessionState::replay([DomainEvent {
        env: Envelope {
            seq: Seq::ZERO,
            id: EventId::new(),
            turn: TurnId::new(),
            item: Some(ego_core::event::id::ItemId::new()),
            parent: None,
            ts: Timestamp::from_millis(1),
            producer: Producer::Main,
        },
        kind: EventKind::CompactionSeeded {
            source: SessionId::new(),
            source_seq: Seq::ZERO,
            prompt_revision: 1,
            checkpoint: "Retained facts".into(),
        },
    }]);
    assert_eq!(row(&compacted, &names, 80), "▱▱▱▱▱▱▱▱▱▱ ?/?");
}
