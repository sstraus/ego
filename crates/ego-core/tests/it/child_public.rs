use ego_core::event::domain::{DomainEvent, Envelope, EventKind, SubagentTerminal};
use ego_core::event::id::{
    EventId, ItemId, Producer, Seq, SessionId, SubagentInvocationId, TurnId,
};
use ego_core::event::time::Timestamp;
use ego_core::public::{
    ChildIdentity, ChildOutcome, ChildPhase, PublicEvent, project_child_lifecycle, project_public,
};
use ego_core::run::{CancelReason, RunId, RunSettlement};

// Catches: child identity or cancellation is discarded, or private diagnostics enter metadata.
#[test]
fn interrupted_child_settles_as_interrupted_without_private_diagnostics() {
    let invocation = SubagentInvocationId::new();
    let child = RunId::new(SessionId::new(), invocation.uuid());
    let item = ItemId::new();
    let turn = TurnId::new();
    let envelope = Envelope {
        seq: Seq::ZERO,
        id: EventId::new(),
        turn,
        item: Some(item),
        parent: None,
        ts: Timestamp::from_millis(1),
        producer: Producer::Main,
    };
    let spawned = DomainEvent {
        env: envelope.clone(),
        kind: EventKind::SubagentSpawned { invocation, child },
    };
    assert!(matches!(
        project_public(&spawned),
        Some(PublicEvent::ItemStarted { .. })
    ));
    assert_eq!(
        project_child_lifecycle(&spawned),
        Some(PublicEvent::ChildLifecycle {
            turn,
            item,
            child: ChildIdentity::Durable {
                invocation,
                run: child
            },
            phase: ChildPhase::Started
        })
    );
    for (settlement, expected) in [
        (
            RunSettlement::Cancelled {
                reason: CancelReason::User,
            },
            ChildOutcome::Interrupted,
        ),
        (RunSettlement::CancelledBeforeStart, ChildOutcome::Cancelled),
        (
            RunSettlement::Interrupted {
                diagnostic: Some("private failure".into()),
            },
            ChildOutcome::Failed,
        ),
        (
            RunSettlement::Uncertain {
                diagnostic: Some("private uncertainty".into()),
            },
            ChildOutcome::Failed,
        ),
    ] {
        let event = DomainEvent {
            env: envelope.clone(),
            kind: EventKind::SubagentSettled {
                invocation,
                child: Some(child),
                terminal: SubagentTerminal::Failed { settlement },
            },
        };
        let public = project_child_lifecycle(&event).unwrap();
        assert_eq!(
            public,
            PublicEvent::ChildLifecycle {
                turn,
                item,
                child: ChildIdentity::Durable {
                    invocation,
                    run: child
                },
                phase: ChildPhase::Settled { outcome: expected }
            }
        );
        assert!(!serde_json::to_string(&public).unwrap().contains("private"));
    }
    let mut missing = spawned;
    missing.env.item = None;
    assert!(project_child_lifecycle(&missing).is_none());
}
