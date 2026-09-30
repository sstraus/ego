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
fn context_reads_as_a_share_of_the_window_never_as_a_raw_count() {
    let names = [StatusSegmentName::Context];
    // 3_000 of 1_000_000 is 0.3%; 42_000 of 200_000 is 21%; 500 of 128_000
    // is well under a tenth of a percent.
    for (used, window, expected) in [
        (3_000, 1_000_000, "ctx 0.3%/1M"),
        (42_000, 200_000, "ctx 21.0%/200k"),
        (500, 128_000, "ctx 0.4%/128k"),
        (1_000_000, 1_000_000, "ctx 100.0%/1M"),
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
    assert_eq!(unbounded, "ctx 2.5k");
    let nothing_yet = row(&state(None, None, CumulativeCost::Unstarted), &names, 120);
    assert_eq!(nothing_yet, "ctx ?");
    let window_only = row(
        &state(None, Some(1_000_000), CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert_eq!(window_only, "ctx ?/1M");
}

#[test]
fn recorded_cost_shows_with_a_currency_mark() {
    let names = [StatusSegmentName::Cost];
    for (currency, amount, expected) in [
        ("USD", "0.00156247", "$0.002"),
        ("USD", "2.5", "$2.50"),
        ("USD", "0", "$0.000"),
        ("USD", "0.0000164", "<$0.001"),
        ("EUR", "0.5", "0.500 EUR"),
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
        let shown = row(&state(None, None, cost), &names, 120);
        assert_eq!(shown, "glm-5.3-flash");
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
            "glm-5.3-flash · low · default · ctx 0.3%/1M · $0.002 · cwd=/workspace/ego",
        ),
        (
            80,
            "glm-5.3-flash · low · default · ctx 0.3%/1M · $0.002 · cwd=/workspace/ego",
        ),
        (60, "glm-5.3-flash · low · default · ctx 0.3%/1M · $0.002"),
        (40, "glm-5.3-flash · low · ctx 0.3%/1M"),
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
    assert_eq!(row(&replayed, &names, 120), "ctx 0.7%/1M · $0.012");
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
    // model 13 + ctx 11 + trust=untrusted 15 + two separators 6 = 45 cells:
    // at 40, one of the two optional fields has to go, and it is trust.
    let names = [
        StatusSegmentName::Model,
        StatusSegmentName::Context,
        StatusSegmentName::Trust,
    ];
    let facts = state(Some(3_000), Some(1_000_000), CumulativeCost::Unstarted);
    let surface = snapshot_for(&facts, Trust::Untrusted);

    let narrow = StatusLine::render(&surface, &names, 40);
    assert_eq!(narrow.text(), "glm-5.3-flash · ctx 0.3%/1M");
    assert!(narrow.omitted(StatusSegmentName::Trust));

    let wide = StatusLine::render(&surface, &names, 60);
    assert_eq!(wide.text(), "glm-5.3-flash · ctx 0.3%/1M · trust=untrusted");
}

#[test]
fn a_zero_window_never_divides_and_falls_back_to_the_count() {
    let names = [StatusSegmentName::Context];
    let shown = row(
        &state(Some(2_482), Some(0), CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert_eq!(shown, "ctx 2.5k");
}

#[test]
fn context_beyond_the_window_is_reported_as_over_one_hundred_percent() {
    let names = [StatusSegmentName::Context];
    let over = row(
        &state(Some(150_000), Some(100_000), CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert_eq!(over, "ctx 150.0%/100k");
    let extreme = row(
        &state(Some(u64::MAX), Some(1), CumulativeCost::Unstarted),
        &names,
        120,
    );
    assert!(
        extreme.starts_with("ctx ") && extreme.ends_with("/1"),
        "{extreme}"
    );
}
