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
        reason: None,
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
            reason: None,
        },
        PlanUpdate::Step {
            id: "a".into(),
            status: StepStatus::Completed,
            reason: None,
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
            reason: None,
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
        started: ["a".into()].into(),
        ..Plan::default()
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
            reason: None,
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
    for terminal in [StepStatus::Completed, StepStatus::Cancelled] {
        for status in [
            StepStatus::Pending,
            StepStatus::InProgress,
            StepStatus::Blocked,
            StepStatus::Waiting,
        ] {
            let mut plan = Plan {
                steps: vec![step("a", terminal)],
                started: if terminal == StepStatus::Completed {
                    ["a".into()].into()
                } else {
                    Default::default()
                },
                ..Plan::default()
            };
            let before = plan.clone();
            assert!(
                plan.apply(&PlanUpdate::Step {
                    id: "a".into(),
                    status,
                    reason: None,
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
                    reason: None,
                },
            ]));
            assert_eq!(replay.plan, before);
            plan.apply(&PlanUpdate::Step {
                id: "a".into(),
                status: terminal,
                reason: None,
            })
            .unwrap();
            plan.apply(&PlanUpdate::Replace {
                steps: vec![step("a", status)],
            })
            .unwrap();
            assert_eq!(plan.steps[0].status, status);
        }
    }
}

// Catches: status polling or replayed settlement releases a no-progress wait repeatedly.
#[test]
fn worklist_background_settlement_releases_the_guard_once_without_reactivating_waiting_work() {
    use ego_core::event::id::DetachedExecId;
    use ego_core::permit::{DetachedExecOutcome, DetachedExecSnapshot};
    let mut log = events(vec![PlanUpdate::Replace {
        steps: vec![
            step("ready", StepStatus::Pending),
            step("external", StepStatus::Waiting),
            step("cancelled", StepStatus::Cancelled),
            step("blocked", StepStatus::Blocked),
        ],
    }]);
    let mut append = |kind| {
        let mut env = log.last().unwrap().env.clone();
        env.seq = env.seq.next();
        env.id = EventId::new();
        log.push(DomainEvent { env, kind });
    };
    let id = DetachedExecId::new();
    append(EventKind::DetachedExecStarted { id });
    append(EventKind::WorklistContinuation {
        completed: vec![],
        text: "Unfinished work".into(),
    });
    let running = DetachedExecSnapshot {
        id,
        outcome: None,
        stdout_tail: vec![],
        stderr_tail: vec![],
        stdout_omitted: 0,
        stderr_omitted: 0,
    };
    append(EventKind::DetachedExecSettled {
        snapshot: running.clone(),
    });
    let before = SessionState::replay(log.clone());
    assert_eq!(before.plan.continuation_completed, Some(vec![]));
    let settled = DetachedExecSnapshot {
        outcome: Some(DetachedExecOutcome::Exited {
            code: 0,
            result: ego_core::permit::ToolResult::builtin("finished"),
        }),
        ..running
    };
    let mut env = log.last().unwrap().env.clone();
    env.seq = env.seq.next();
    env.id = EventId::new();
    log.push(DomainEvent {
        env,
        kind: EventKind::DetachedExecSettled {
            snapshot: settled.clone(),
        },
    });
    let after = SessionState::replay(log.clone());
    assert_eq!(after.plan.continuation_completed, None);
    assert_eq!(after.plan.steps[1].status, StepStatus::Waiting);
    assert_eq!(after.plan.steps[2].status, StepStatus::Cancelled);
    let fork = PreparedFork::new(SessionId::new(), &before, Timestamp::from_millis(2)).unwrap();
    assert_eq!(
        SessionState::replay(fork.seed().events().cloned()).plan,
        before.plan
    );
    let mut env = log.last().unwrap().env.clone();
    env.seq = env.seq.next();
    env.id = EventId::new();
    log.push(DomainEvent {
        env: env.clone(),
        kind: EventKind::WorklistContinuation {
            completed: vec![],
            text: "Unfinished work".into(),
        },
    });
    env.seq = env.seq.next();
    env.id = EventId::new();
    log.push(DomainEvent {
        env,
        kind: EventKind::DetachedExecSettled { snapshot: settled },
    });
    let wire = serde_json::to_vec(&log).unwrap();
    let replay = SessionState::replay(serde_json::from_slice::<Vec<DomainEvent>>(&wire).unwrap());
    assert_eq!(replay.plan.continuation_completed, Some(vec![]));
}

// Catches: fork/compaction seeds lose started IDs once their visible row is removed.
#[test]
fn worklist_seed_keeps_removed_started_ids_and_suspension_causes() {
    let mut suspended = step("paused", StepStatus::Waiting);
    suspended.reason = Some("external build remains live".into());
    let live = SessionState::replay(events(vec![
        PlanUpdate::Replace {
            steps: vec![
                step("removed", StepStatus::InProgress),
                step("paused", StepStatus::InProgress),
            ],
        },
        PlanUpdate::Replace {
            steps: vec![suspended.clone()],
        },
    ]));
    let fork = PreparedFork::new(SessionId::new(), &live, Timestamp::from_millis(2)).unwrap();
    let seeded = SessionState::replay(fork.seed().events().cloned());
    assert_eq!(seeded.plan, live.plan);
    assert!(seeded.plan.started.contains("removed"));
    assert!(seeded.plan.started.contains("paused"));
    assert_eq!(seeded.plan.steps, vec![suspended]);
}
