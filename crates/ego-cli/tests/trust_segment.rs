//! contract for regression case: the status row states the workspace posture.
//!
//! A session that starts untrusted in silence — because nothing in the
//! workspace was worth asking about, or because the answer was remembered —
//! needs somewhere to say so. See
//! `docs/decisions/2026-09-05-a-trust-answer-is-remembered-per-root.md`.

use ego_cli::repl::status::{
    SeatFacts, SelectionFacts, StatusLine, StatusSegmentName, SurfaceSnapshot,
};
use ego_core::config::{Mode, ModelRef, Trust};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::state::{QueueStatus, SessionState};

fn snapshot(trust: Trust) -> SurfaceSnapshot {
    SurfaceSnapshot::from_state(
        SessionId::new(),
        &SessionState::default(),
        SelectionFacts::new(
            ModelRef("openai/gpt-5.6".into()),
            ThinkingLevel::High,
            Mode::Default,
        ),
        SeatFacts::new(QueueStatus::default(), 0, "/workspace/ego", trust),
        None,
    )
}

#[test]
fn trust_is_a_persistable_segment_name() {
    use ego_core::config::StatusSegmentName as Persisted;

    assert_eq!(Persisted::from_name("trust"), Some(Persisted::Trust));
    assert_eq!(Persisted::Trust.as_str(), "trust");
    assert!(
        Persisted::NAMES.contains(&"trust"),
        "a name nobody can spell is a name nobody can configure"
    );
}

#[test]
fn the_row_names_the_posture_it_is_rendered_from() {
    let untrusted = StatusLine::render(
        &snapshot(Trust::Untrusted),
        &[StatusSegmentName::Trust],
        240,
    );
    assert_eq!(untrusted.text(), "trust=untrusted");

    let trusted = StatusLine::render(&snapshot(Trust::Trusted), &[StatusSegmentName::Trust], 240);
    assert_eq!(trusted.text(), "", "a trusted posture is not worth a row");
}

#[test]
fn a_trusted_posture_is_dropped_before_an_untrusted_one() {
    // Trusted is the uninteresting answer, so it collapses with the other
    // inactive segments; untrusted is the last optional segment width pressure
    // reaches, because a posture nobody can see is a posture nobody checks.
    let names = [
        StatusSegmentName::Trust,
        StatusSegmentName::Model,
        StatusSegmentName::Cwd,
    ];

    let trusted = StatusLine::render(&snapshot(Trust::Trusted), &names, 40);
    assert_eq!(trusted.text(), "gpt-5.6 · cwd=/workspace/ego");
    assert!(trusted.omitted(StatusSegmentName::Trust));

    let untrusted = StatusLine::render(&snapshot(Trust::Untrusted), &names, 40);
    assert_eq!(untrusted.text(), "trust=untrusted · gpt-5.6");
    assert!(untrusted.omitted(StatusSegmentName::Cwd));
}
