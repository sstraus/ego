//! Contract tests for journal-owned plans (not inferred from objectives or prose).
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::plan::{Plan, PlanStep, PlanUpdate, StepStatus};
use ego_core::session::PreparedFork;
use ego_core::state::SessionState;

fn step(id: &str, status: StepStatus) -> PlanStep {
    PlanStep {
        id: id.into(),
        text: format!("Work on {id}"),
        status,
    }
}
fn events(updates: Vec<PlanUpdate>) -> Vec<DomainEvent> {
    let mut seq = Seq::ZERO;
    let turn = TurnId::new();
    updates
        .into_iter()
        .map(|update| DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn,
                item: None,
                parent: None,
                ts: Timestamp::from_millis(1),
                producer: Producer::Main,
            },
            kind: EventKind::PlanUpdated { update },
        })
        .collect()
}

#[test]
fn plan_replay_preserves_reordered_ids_and_completed_and_blocked_statuses() {
    // Catches: a replacement appends stale steps or replay resets their identity/progress.
    let log = events(vec![
        PlanUpdate::Replace {
            steps: vec![
                step("a", StepStatus::Pending),
                step("b", StepStatus::Pending),
            ],
        },
        PlanUpdate::Step {
            id: "a".into(),
            status: StepStatus::InProgress,
        },
        PlanUpdate::Step {
            id: "a".into(),
            status: StepStatus::Completed,
        },
        PlanUpdate::Replace {
            steps: vec![
                step("b", StepStatus::Pending),
                step("a", StepStatus::Completed),
            ],
        },
        PlanUpdate::Step {
            id: "b".into(),
            status: StepStatus::Blocked,
        },
    ]);
    let wire = serde_json::to_string(&log).unwrap();
    let decoded: Vec<DomainEvent> = serde_json::from_str(&wire).unwrap();
    let replayed = SessionState::replay(decoded);
    assert_eq!(
        replayed.plan.steps,
        vec![
            step("b", StepStatus::Blocked),
            step("a", StepStatus::Completed)
        ]
    );
    assert!(
        replayed.goal.is_none(),
        "plan completion cannot achieve a checked objective"
    );
    let mut live = SessionState::default();
    for event in &log {
        live.apply(event);
    }
    assert_eq!(live, replayed);
    let fork = PreparedFork::new(SessionId::new(), &live, Timestamp::from_millis(2)).unwrap();
    assert_eq!(
        SessionState::replay(fork.seed().events().cloned()).plan,
        live.plan
    );
}

#[test]
fn invalid_plan_updates_are_atomic_and_clear_is_an_explicit_replacement() {
    // Catches: duplicate/unknown IDs partly mutate a plan, or clearing leaves stale work.
    let mut plan = Plan {
        steps: vec![step("a", StepStatus::Completed)],
    };
    let original = plan.clone();
    for update in [
        PlanUpdate::Replace {
            steps: vec![
                step("a", StepStatus::Pending),
                step("a", StepStatus::Blocked),
            ],
        },
        PlanUpdate::Replace {
            steps: vec![step("", StepStatus::Pending)],
        },
        PlanUpdate::Step {
            id: "unknown".into(),
            status: StepStatus::Completed,
        },
        PlanUpdate::Replace {
            steps: (0..129)
                .map(|id| step(&id.to_string(), StepStatus::Pending))
                .collect(),
        },
    ] {
        assert!(plan.apply(&update).is_err());
        assert_eq!(plan, original);
    }
    plan.apply(&PlanUpdate::Replace { steps: vec![] }).unwrap();
    assert!(plan.steps.is_empty());
}

// Catches: stale model or child status updates reopen completed work without replacement.
#[test]
fn completed_plan_step_requires_explicit_replacement_to_reopen() {
    for status in [
        StepStatus::Pending,
        StepStatus::InProgress,
        StepStatus::Blocked,
    ] {
        let mut plan = Plan {
            steps: vec![step("a", StepStatus::Completed)],
        };
        let before = plan.clone();
        assert!(
            plan.apply(&PlanUpdate::Step {
                id: "a".into(),
                status
            })
            .is_err()
        );
        assert_eq!(plan, before);
        let replay = SessionState::replay(events(vec![
            PlanUpdate::Replace {
                steps: before.steps.clone(),
            },
            PlanUpdate::Step {
                id: "a".into(),
                status,
            },
        ]));
        assert_eq!(replay.plan, before);
        plan.apply(&PlanUpdate::Step {
            id: "a".into(),
            status: StepStatus::Completed,
        })
        .unwrap();
        plan.apply(&PlanUpdate::Replace {
            steps: vec![step("a", status)],
        })
        .unwrap();
        assert_eq!(plan.steps[0].status, status);
    }
}
