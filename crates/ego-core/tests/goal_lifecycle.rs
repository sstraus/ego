//! RED contract for the durable goal reducer (regression case).

use ego_core::ceiling::Ceilings;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, GoalId, Producer, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::goal::{GoalOwner, GoalState, GoalTransition};
use ego_core::state::SessionState;

fn event(turn: TurnId, seq: u64, kind: EventKind) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: serde_json::from_value(serde_json::json!(seq)).unwrap(),
            id: EventId::new(),
            turn,
            item: None,
            parent: None,
            ts: Timestamp::from_millis(seq),
            producer: Producer::Main,
        },
        kind,
    }
}

fn started(turn: TurnId, seq: u64, owner: GoalOwner) -> (DomainEvent, GoalId) {
    let id = GoalId::new();
    (
        event(
            turn,
            seq,
            EventKind::GoalSet {
                id,
                objective: "ship the final product".into(),
                owner,
                ceilings: Ceilings::default(),
                derived_from: None,
            },
        ),
        id,
    )
}

#[test]
fn blocker_audit_counts_distinct_goal_turns_and_blocks_on_third_observation() {
    let turns = [TurnId::new(), TurnId::new(), TurnId::new()];
    let mut state = SessionState::default();
    let (creation, id) = started(turns[0], 1, GoalOwner::Operator);
    state.apply(&creation);
    for (turn, seq) in [(turns[0], 2), (turns[1], 3)] {
        state.apply(&event(
            turn,
            seq,
            EventKind::GoalTransitioned {
                id,
                transition: GoalTransition::BlockerObserved {
                    reason: " API   unavailable ".into(),
                },
            },
        ));
        assert!(matches!(
            state.goal.as_ref().unwrap().state,
            GoalState::Active
        ));
    }
    state.apply(&event(
        turns[2],
        4,
        EventKind::GoalTransitioned {
            id,
            transition: GoalTransition::BlockerObserved {
                reason: "api unavailable".into(),
            },
        },
    ));
    assert!(matches!(
        state.goal.as_ref().unwrap().state,
        GoalState::Blocked
    ));
}

#[test]
fn changed_condition_and_intervening_completed_turn_reset_blocker_audit() {
    let mut state = SessionState::default();
    let first = TurnId::new();
    let (creation, id) = started(first, 1, GoalOwner::Operator);
    state.apply(&creation);
    state.apply(&event(
        first,
        2,
        EventKind::GoalTransitioned {
            id,
            transition: GoalTransition::BlockerObserved {
                reason: "network".into(),
            },
        },
    ));
    state.apply(&event(
        TurnId::new(),
        3,
        EventKind::GoalTransitioned {
            id,
            transition: GoalTransition::BlockerObserved {
                reason: "disk".into(),
            },
        },
    ));
    state.apply(&event(
        TurnId::new(),
        4,
        EventKind::TurnCompleted {
            ok: true,
            exit_reason: ExitReason::Completed,
        },
    ));
    state.apply(&event(
        TurnId::new(),
        5,
        EventKind::GoalTransitioned {
            id,
            transition: GoalTransition::BlockerObserved {
                reason: "disk".into(),
            },
        },
    ));
    assert!(matches!(
        state.goal.as_ref().unwrap().state,
        GoalState::Active
    ));
    assert_eq!(state.goal.as_ref().unwrap().blocker_observations, 1);
}
