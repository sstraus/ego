//! contract for the durable goal reducer (Story 085).

use ego_core::ceiling::Ceilings;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, GoalId, Producer, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::goal::{GoalOwner, GoalState, GoalTransition};
use ego_core::state::SessionState;
use ego_core::store::evidence::{EvidenceId, EvidenceRef};

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
fn blocker_audit_counts_distinct_goal_turns_and_blocks_on_fifth_observation() {
    let turns = [
        TurnId::new(),
        TurnId::new(),
        TurnId::new(),
        TurnId::new(),
        TurnId::new(),
    ];
    let mut state = SessionState::default();
    let (creation, id) = started(turns[0], 1, GoalOwner::Operator);
    state.apply(&creation);
    for (turn, seq) in [(turns[0], 2), (turns[1], 3), (turns[2], 4), (turns[3], 5)] {
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
        turns[4],
        6,
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

#[test]
fn achievement_matrix_enforces_attester_and_evidence_and_is_terminal() {
    let evidence = || EvidenceRef {
        id: EvidenceId::parse("a".repeat(64)).unwrap(),
        bytes: 1,
    };
    let achieve = |owner, attested_by, current_evidence| {
        let turn = TurnId::new();
        let (creation, id) = started(turn, 1, owner);
        let mut state = SessionState::default();
        state.apply(&creation);
        state.apply(&event(
            turn,
            2,
            EventKind::GoalTransitioned {
                id,
                transition: GoalTransition::Achieved {
                    attested_by,
                    evidence: current_evidence,
                },
            },
        ));
        (state, turn, id)
    };

    for owner in [GoalOwner::Operator, GoalOwner::Automated] {
        let (state, _, _) = achieve(owner, GoalOwner::Operator, None);
        assert!(matches!(state.goal.unwrap().state, GoalState::Achieved));
    }
    let current_evidence = evidence();
    let (state, turn, id) = achieve(
        GoalOwner::Automated,
        GoalOwner::Automated,
        Some(current_evidence.clone()),
    );
    let record = state.goal.as_ref().unwrap();
    assert!(matches!(record.state, GoalState::Achieved));
    assert_eq!(record.achieved_by, Some(GoalOwner::Automated));
    assert_eq!(record.achievement_evidence, Some(current_evidence.clone()));
    let admitted = EventKind::GoalTransitioned {
        id,
        transition: GoalTransition::Achieved {
            attested_by: GoalOwner::Automated,
            evidence: Some(current_evidence.clone()),
        },
    };
    assert_eq!(
        admitted.blob_references(),
        vec![current_evidence.blob_ref()]
    );
    let mut state = state;
    state.apply(&event(
        turn,
        3,
        EventKind::GoalTransitioned {
            id,
            transition: GoalTransition::Achieved {
                attested_by: GoalOwner::Operator,
                evidence: None,
            },
        },
    ));
    assert_eq!(
        state.goal.as_ref().unwrap().achieved_by,
        Some(GoalOwner::Automated)
    );
    assert_eq!(
        state.goal.as_ref().unwrap().achievement_evidence,
        Some(current_evidence)
    );

    let (state, _, _) = achieve(GoalOwner::Automated, GoalOwner::Automated, None);
    assert!(matches!(state.goal.unwrap().state, GoalState::Active));
    let (state, _, _) = achieve(GoalOwner::Operator, GoalOwner::Automated, Some(evidence()));
    assert!(matches!(state.goal.unwrap().state, GoalState::Active));
}

#[test]
fn independent_check_can_achieve_operator_goal_but_requires_evidence() {
    // Catches: operator ownership silently rejects independent goal checks.
    let turn = TurnId::new();
    let (creation, id) = started(turn, 1, GoalOwner::Operator);
    let mut state = SessionState::default();
    state.apply(&creation);
    for (seq, evidence) in [
        (2, None),
        (
            3,
            Some(EvidenceRef {
                id: EvidenceId::parse("b".repeat(64)).unwrap(),
                bytes: 9,
            }),
        ),
    ] {
        state.apply(&event(
            turn,
            seq,
            EventKind::GoalTransitioned {
                id,
                transition: GoalTransition::Achieved {
                    attested_by: GoalOwner::Check,
                    evidence,
                },
            },
        ));
        assert_eq!(
            state.goal.as_ref().unwrap().state,
            if seq == 2 {
                GoalState::Active
            } else {
                GoalState::Achieved
            }
        );
    }
    assert_eq!(state.goal.unwrap().achieved_by, Some(GoalOwner::Check));
}

#[test]
fn goal_continuation_count_and_stop_survive_replay_for_managed_consumers() {
    // Catches: a resumed loop forgets its allowance and reports a stopped goal as ordinary completion.
    let turn = TurnId::new();
    let (creation, id) = started(turn, 1, GoalOwner::Operator);
    let events = vec![
        creation,
        event(
            turn,
            2,
            EventKind::GoalContinuation {
                id,
                text: "continue verification".into(),
            },
        ),
        event(
            turn,
            3,
            EventKind::GoalCheckStopped {
                id,
                reason: "goal continuation budget exhausted (1)".into(),
            },
        ),
        event(
            turn,
            4,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            },
        ),
    ];
    let state = SessionState::replay(events);
    let goal = serde_json::to_value(state.goal.unwrap()).unwrap();
    assert_eq!(goal["continuation_turns"], 1);
    assert_eq!(goal["check_stop"], "goal continuation budget exhausted (1)");
}

// Catches: an immediate operator wait is lost on replay or fabricated as five observations.
#[test]
fn checked_operator_block_is_durable_without_a_repeated_condition_audit() {
    let turn = TurnId::new();
    let (creation, id) = started(turn, 1, GoalOwner::Operator);
    let blocked = event(
        turn,
        2,
        EventKind::GoalTransitioned {
            id,
            transition: GoalTransition::Blocked {
                reason: "Operator login required".into(),
            },
        },
    );
    let encoded = serde_json::to_string(&blocked).unwrap();
    let decoded = serde_json::from_str::<DomainEvent>(&encoded).unwrap();
    let state = SessionState::replay([creation, decoded]);
    let goal = state.goal.unwrap();
    assert_eq!(goal.state, GoalState::Blocked);
    assert_eq!(
        goal.blocker_reason.as_deref(),
        Some("Operator login required")
    );
    assert_eq!(goal.blocker_observations, 0);
    assert_eq!(goal.continuation_turns, 0);
    assert!(goal.achievement_evidence.is_none());
}

// Catches: a successful formatter silently drops the actionable refusal reason.
#[test]
fn goal_refusal_display_preserves_each_actionable_reason() {
    use ego_core::goal::GoalRefusal;

    for (refusal, expected) in [
        (GoalRefusal::NoActiveGoal, "session has no active goal"),
        (
            GoalRefusal::InvalidExtension,
            "goal extension requires a positive turn count and must not overflow the allowance",
        ),
        (
            GoalRefusal::AlreadyTerminal(GoalState::Achieved),
            "goal is already Achieved",
        ),
        (
            GoalRefusal::AlreadyTerminal(GoalState::Blocked),
            "goal is already Blocked",
        ),
        (
            GoalRefusal::ActiveGoalExists,
            "session already has an active goal",
        ),
        (
            GoalRefusal::EvidenceRequired,
            "automated goal achievement requires evidence",
        ),
        (
            GoalRefusal::AttestationWouldWiden,
            "goal achievement would widen automated authority",
        ),
    ] {
        assert_eq!(refusal.to_string(), expected, "{refusal:?}");
    }
}

// Catches: a retired model update rewrites an immutable outcome during replay.
#[test]
fn automatic_goal_replay_refuses_text_rewrites_and_keeps_default_allowance() {
    let turn = TurnId::new();
    let (creation, id) = started(turn, 1, GoalOwner::Automated);
    let mut state = SessionState::default();
    state.apply(&creation);
    state.apply(&event(
        turn,
        2,
        EventKind::GoalUpdated {
            id,
            objective: "easier outcome".into(),
        },
    ));
    let goal = state.goal.unwrap();
    assert_eq!(goal.objective, "ship the final product");
    assert_eq!(goal.owner, GoalOwner::Automated);
    assert_eq!(goal.continuation_budget, Some(20));
}

// Catches: an operator stop pause outlives a genuine new request or explicit
// operator goal, is lost on replay, or a model cancellation pauses automatic work.
#[test]
fn operator_stop_pause_is_released_only_by_new_user_request_or_operator_goal() {
    let turn = TurnId::new();
    let cancel = |seq, id, by| {
        event(
            turn,
            seq,
            EventKind::GoalTransitioned {
                id,
                transition: GoalTransition::Cancelled { cancelled_by: by },
            },
        )
    };
    let prompt = |seq| {
        event(
            turn,
            seq,
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "next request".into(),
                images: Vec::new(),
                queued_prompt: None,
            },
        )
    };
    let (set, id) = started(turn, 1, GoalOwner::Operator);
    let mut events = vec![set, cancel(2, id, GoalOwner::Operator)];
    assert!(SessionState::replay(events.clone()).automatic_work_paused());
    // A duplicate stop keeps the pause.
    events.push(cancel(3, id, GoalOwner::Operator));
    assert!(SessionState::replay(events.clone()).automatic_work_paused());
    let (replacement, _) = started(turn, 4, GoalOwner::Operator);
    let mut with_goal = events.clone();
    with_goal.push(replacement);
    assert!(!SessionState::replay(with_goal).automatic_work_paused());
    events.push(prompt(4));
    let released = SessionState::replay(events);
    assert!(!released.automatic_work_paused());
    assert_eq!(released.goal.unwrap().state, GoalState::Cancelled);

    let (set, id) = started(turn, 1, GoalOwner::Automated);
    let model_cancelled = SessionState::replay(vec![set, cancel(2, id, GoalOwner::Automated)]);
    assert!(!model_cancelled.automatic_work_paused());
}
