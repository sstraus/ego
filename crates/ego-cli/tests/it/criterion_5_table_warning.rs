//! 034 criterion 5: a third-party table binding must be visible in text mode.

use std::sync::{Arc, Mutex};

use ego_cli::output::{Diagnostics, TextOutput, TextSink};
use ego_cli::render::caps::{Capabilities, Depth, OscProfile};
use ego_core::cost::{CurrencyCode, Money, PricingRevision, RateSource};
use ego_core::event::id::TurnId;
use ego_core::public::{CeilingEffect, MeteringKind, PublicEvent};
use ego_core::sink::EventSink;
use ego_core::state::SessionState;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>, Arc<Mutex<String>>);

impl Diagnostics for Capture {
    fn note(&self, message: &str) {
        self.0.lock().unwrap().push(message.to_owned());
    }
}

impl TextOutput for Capture {
    fn write(&self, text: &str) {
        self.1.lock().unwrap().push_str(text);
    }
    fn line(&self, text: &str) {
        let mut answer = self.1.lock().unwrap();
        answer.push_str(text);
        answer.push('\n');
    }
}

fn money(value: &str) -> Money {
    Money::new(value.parse().unwrap(), CurrencyCode::new("USD").unwrap())
}

fn table_event(effect: CeilingEffect) -> PublicEvent {
    PublicEvent::MeteringSelected {
        turn: TurnId::new(),
        metering: MeteringKind::Currency,
        ceiling: effect,
        rates: RateSource::Table {
            revision: PricingRevision("litellm/2026-08-21#abc123".to_owned()),
        },
    }
}

fn render(events: &[PublicEvent], quiet: bool) -> Vec<String> {
    let capture = Capture::default();
    let sink = TextSink::new(
        &capture,
        quiet,
        Capabilities {
            depth: Depth::None,
            width: 80,
            hyperlinks: false,
            progress: false,
            osc: OscProfile::None,
        },
        None,
    );
    let state = SessionState::default();
    for event in events {
        sink.publish(event, &state);
    }
    assert!(
        capture.1.lock().unwrap().is_empty(),
        "status diagnostics must not become answer text"
    );
    capture.0.lock().unwrap().clone()
}

#[test]
fn table_binding_warns_with_risk_and_attributable_revision_once_per_generation_series() {
    let event = table_event(CeilingEffect::Binds {
        limit: money("5"),
        clamped_from: None,
    });
    let notes = render(&[event.clone(), event], false);
    assert_eq!(
        notes.len(),
        1,
        "identical per-generation events deduplicate"
    );
    let warning = &notes[0];
    assert!(warning.contains("third-party"), "{warning}");
    assert!(
        warning.contains("stale") && warning.contains("charge"),
        "{warning}"
    );
    assert!(warning.contains("litellm/2026-08-21#abc123"), "{warning}");
}

#[test]
fn table_binding_warning_survives_quiet_mode() {
    let notes = render(
        &[table_event(CeilingEffect::Binds {
            limit: money("5"),
            clamped_from: None,
        })],
        true,
    );
    assert_eq!(notes.len(), 1);
}

#[test]
fn a_clamped_table_warning_preserves_the_clamp_and_table_risk() {
    let notes = render(
        &[table_event(CeilingEffect::Binds {
            limit: money("5"),
            clamped_from: Some(money("10")),
        })],
        false,
    );
    assert_eq!(notes.len(), 1);
    let warning = &notes[0];
    assert!(
        warning.contains("10USD") && warning.contains("5USD"),
        "{warning}"
    );
    assert!(warning.contains("third-party"), "{warning}");
    assert!(
        warning.contains("stale") && warning.contains("charge"),
        "{warning}"
    );
    assert!(warning.contains("litellm/2026-08-21#abc123"), "{warning}");
}

#[test]
fn nonbinding_or_non_table_rates_are_silent() {
    let cases = [
        CeilingEffect::None,
        CeilingEffect::Binds {
            limit: money("5"),
            clamped_from: None,
        },
    ];
    for ceiling in cases {
        let event = PublicEvent::MeteringSelected {
            turn: TurnId::new(),
            metering: MeteringKind::Currency,
            ceiling,
            rates: RateSource::Operator,
        };
        assert!(render(&[event], false).is_empty());
    }
    assert!(render(&[table_event(CeilingEffect::None)], false).is_empty());
}

#[test]
fn interactive_retry_notice_is_visible_without_becoming_answer_text() {
    // Catches: interactive seats ignore the coordinator's retry status and appear idle.
    let event = PublicEvent::ProviderRetry {
        turn: TurnId::new(),
        attempt: 1,
        limit: 6,
        recovery_remaining_ms: None,
        delay_ms: 750,
        kind: ego_core::generator::types::ErrorKind::Transient,
        capacity: false,
        cause: "the network failed".to_owned(),
    };
    assert_eq!(
        render(std::slice::from_ref(&event), false),
        ["the network failed — connection problem, retrying in 0.8s (attempt 1/6)"]
    );
    assert!(render(&[event], true).is_empty());
}

#[test]
fn capacity_retry_notice_names_capacity_the_wait_and_the_next_attempt_time() {
    // Catches: a capacity refusal shown as a connection problem, or a notice
    // without the clock time of the next attempt.
    let event = PublicEvent::ProviderRetry {
        turn: TurnId::new(),
        attempt: 1,
        limit: 3,
        recovery_remaining_ms: None,
        delay_ms: 60_000,
        kind: ego_core::generator::types::ErrorKind::Transient,
        capacity: true,
        cause: "At capacity".to_owned(),
    };
    let rendered = render(std::slice::from_ref(&event), false);
    assert_eq!(rendered.len(), 1, "one notice per retry: {rendered:?}");
    let notice = &rendered[0];
    let (head, tail) = notice
        .rsplit_once(" at ")
        .unwrap_or_else(|| panic!("no next attempt time: {notice}"));
    assert_eq!(
        head,
        "At capacity — provider at capacity, retrying in 60.0s"
    );
    let (clock, counter) = tail.split_once(" (").expect("attempt counter");
    assert_eq!(counter, "attempt 1/3)");
    assert!(
        chrono::NaiveTime::parse_from_str(clock, "%I:%M:%S %p").is_ok(),
        "next attempt time: {clock}"
    );
}
