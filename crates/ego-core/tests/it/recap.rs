//! When the away recap is due, and who may turn it on.

use ego_core::config::{ConfigLayer, Source, Trust, resolve};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, Producer, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::state::SessionState;

const MINUTE: u64 = 60_000;

fn completed_at(ms: u64) -> SessionState {
    let mut state = SessionState::default();
    state.apply(&DomainEvent {
        env: Envelope {
            seq: serde_json::from_value(serde_json::json!(1)).unwrap(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(ms),
            producer: Producer::Main,
        },
        kind: EventKind::TurnCompleted {
            ok: true,
            exit_reason: ExitReason::Completed,
        },
    });
    state
}

// Catches: a recap that spends a paid request by default, or that a cloned
// repository or a per-run flag turns on.
#[test]
fn the_recap_is_off_by_default_and_only_the_owner_turns_it_on() {
    let off = resolve(&[], Trust::Trusted, None).unwrap();
    assert!(!off.recap.enabled);
    assert_eq!(off.recap.after_minutes.get(), 30);
    let user = ConfigLayer::parse(Source::User, "[recap]\nenabled=true\nafter_minutes=5").unwrap();
    let on = resolve(&[user], Trust::Trusted, None).unwrap();
    assert!(on.recap.enabled);
    assert_eq!(on.recap.after_minutes.get(), 5);
    for source in [Source::Project, Source::Local, Source::Flags] {
        let layer = ConfigLayer::parse(source, "[recap]\nenabled=true").unwrap();
        let ignored = resolve(&[layer], Trust::Trusted, None).unwrap();
        assert!(!ignored.recap.enabled, "{source}");
    }
}

// Catches: the absence measured in another unit or from another moment than
// the last completed turn, and a recap for a session with nothing to recap.
#[test]
fn a_recap_is_due_once_the_absence_since_the_last_completed_turn_is_reached() {
    let user = ConfigLayer::parse(Source::User, "[recap]\nenabled=true\nafter_minutes=30").unwrap();
    let recap = resolve(&[user], Trust::Trusted, None).unwrap().recap;
    let state = completed_at(1_000);
    assert!(!recap.due(&state, Timestamp::from_millis(1_000 + 30 * MINUTE - 1)));
    assert!(recap.due(&state, Timestamp::from_millis(1_000 + 30 * MINUTE)));
    assert!(!recap.due(
        &SessionState::default(),
        Timestamp::from_millis(u64::MAX / 2)
    ));
    let off = resolve(&[], Trust::Trusted, None).unwrap().recap;
    assert!(!off.due(&state, Timestamp::from_millis(1_000 + 300 * MINUTE)));
}
