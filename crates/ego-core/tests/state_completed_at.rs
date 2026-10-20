use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::permit::ToolResult;
use ego_core::state::{ItemState, SessionState};

// Catches: old completed snapshots acquiring invented wall time during deserialization.
#[test]
fn legacy_completed_item_without_timestamp_remains_unknown() {
    let item: ItemState = serde_json::from_value(serde_json::json!({
        "turn": "019a0000-0000-7000-8000-000000000001",
        "seq": 1,
        "kind": "tool_call",
        "tool_call_started": null,
        "call_id": null,
        "tool_name": "exec",
        "args": "{}",
        "text": "",
        "tool_result": null,
        "completed": true,
        "ok": false
    }))
    .unwrap();
    assert!(item.completed);
    assert_eq!(item.completed_at, None);
}

// Catches: completion time taken from start, current clock, or clamped on clock regression.
#[test]
fn replay_tool_completion_preserves_exact_event_timestamp_for_success_and_failure() {
    for (ok, millis) in [(true, 725), (false, 0)] {
        let turn = TurnId::new();
        let item_id = ItemId::new();
        let mut seq = Seq::default();
        let started = DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn,
                item: Some(item_id),
                parent: None,
                ts: Timestamp::from_millis(1_000),
                producer: Producer::Main,
            },
            kind: EventKind::ToolCallStarted {
                name: "exec".into(),
                args: "{}".into(),
                call_id: None,
            },
        };
        let pending = SessionState::replay([started.clone()]);
        assert_eq!(pending.items[&item_id].completed_at, None);
        let completed = DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                ts: Timestamp::from_millis(millis),
                parent: Some(started.env.id),
                ..started.env.clone()
            },
            kind: EventKind::ToolCallCompleted {
                ok,
                result: ToolResult::builtin("tool output"),
            },
        };
        let state = SessionState::replay([started, completed]);
        assert!(state.items[&item_id].completed);
        assert_eq!(state.items[&item_id].ok, Some(ok));
        assert_eq!(
            state.items[&item_id].completed_at,
            Some(Timestamp::from_millis(millis))
        );
        let restored: SessionState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(
            restored.items[&item_id].completed_at,
            state.items[&item_id].completed_at
        );
    }
}
