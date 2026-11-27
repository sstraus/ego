//! contract for durable prompt queues and safe steering.
//!
//! This module deliberately exercises the final event ledger, not an
//! in-memory inbox. Acceptance, expedited intent, and consumption are three
//! different durable facts; replay must derive the queue from those facts in
//! sequence order and must never infer acknowledgement from a missing turn.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, QueuedPromptId, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::state::SessionState;
use serde_json::json;

fn event(seq: &mut Seq, turn: TurnId, item: Option<ItemId>, kind: EventKind) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn,
            item,
            parent: None,
            ts: Timestamp::from_millis(1_787_206_085_352),
            producer: Producer::Main,
        },
        kind,
    }
}

fn replay(kinds: impl IntoIterator<Item = EventKind>) -> SessionState {
    let turn = TurnId::new();
    let mut seq = Seq::default();
    let mut state = SessionState::default();
    for kind in kinds {
        let item = matches!(
            &kind,
            EventKind::UserPrompt { .. }
                | EventKind::AssistantMessage { .. }
                | EventKind::AssistantDelta { .. }
                | EventKind::ToolCallStarted { .. }
                | EventKind::ToolCallCompleted { .. }
        )
        .then(ItemId::new);
        state.apply(&event(&mut seq, turn, item, kind));
    }
    state
}

#[test]
fn queued_prompt_id_is_a_stable_serde_newtype() {
    let id = QueuedPromptId::new();
    let encoded = serde_json::to_string(&id).expect("queued prompt ids are durable");
    let decoded: QueuedPromptId =
        serde_json::from_str(&encoded).expect("queued prompt ids round-trip");

    assert_eq!(decoded, id);
}

#[test]
fn prompt_queued_has_the_final_durable_shape() {
    let id = QueuedPromptId::new();
    let encoded = serde_json::to_value(EventKind::PromptQueued {
        audio: Vec::new(),
        id,
        text: "inspect the failing test".to_owned(),
        images: Vec::new(),
    })
    .expect("prompt acceptance is serialisable");

    assert_eq!(
        encoded,
        json!({
            "kind": "PromptQueued",
            "id": id,
            "text": "inspect the failing test",
        })
    );
}

#[test]
fn expedited_intent_names_the_fifo_tail_and_is_not_a_boolean() {
    let id = QueuedPromptId::new();
    let encoded = serde_json::to_value(EventKind::PromptDeliveryExpedited { through: id })
        .expect("expedited intent is serialisable");

    assert_eq!(
        encoded,
        json!({
            "kind": "PromptDeliveryExpedited",
            "through": id,
        })
    );
}

#[test]
fn ordinary_user_prompts_carry_no_queue_correlation() {
    let kind = EventKind::UserPrompt {
        audio: Vec::new(),
        text: "ordinary prompt".to_owned(),
        images: Vec::new(),
        queued_prompt: None,
    };

    assert_eq!(
        serde_json::to_value(kind).unwrap(),
        json!({"kind": "UserPrompt", "text": "ordinary prompt"})
    );
}

#[test]
fn delivered_user_prompts_carry_exactly_one_queue_id() {
    let id = QueuedPromptId::new();
    let kind = EventKind::UserPrompt {
        audio: Vec::new(),
        text: "steer now".to_owned(),
        images: Vec::new(),
        queued_prompt: Some(id),
    };

    assert_eq!(
        serde_json::to_value(kind).unwrap(),
        json!({"kind": "UserPrompt", "text": "steer now", "queued_prompt": id})
    );
}

#[test]
fn replay_preserves_multiple_acceptances_in_fifo_sequence_order() {
    let first = QueuedPromptId::new();
    let second = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: first,
            text: "first steer".to_owned(),
            images: Vec::new(),
        },
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: second,
            text: "second steer".to_owned(),
            images: Vec::new(),
        },
    ]);

    assert_eq!(
        state
            .unacknowledged_prompts()
            .iter()
            .map(|prompt| prompt.id())
            .collect::<Vec<_>>(),
        vec![first, second]
    );
}

#[test]
fn replay_does_not_invent_a_pending_or_queued_class() {
    let id = QueuedPromptId::new();
    let state = replay([EventKind::PromptQueued {
        audio: Vec::new(),
        id,
        text: "idle prompt".to_owned(),
        images: Vec::new(),
    }]);

    // PromptQueued is intentionally class-free.  The live coordinator
    // decides PendingSteer versus ordinary Queued from the phase in which it
    // accepts the record; replay can only expose the unacknowledged FIFO.
    assert_eq!(
        state
            .unacknowledged_prompts()
            .iter()
            .map(|prompt| prompt.id())
            .collect::<Vec<_>>(),
        vec![id]
    );
}

#[test]
fn expedited_tail_is_monotonic_and_repeated_esc_is_idempotent() {
    let first = QueuedPromptId::new();
    let second = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: first,
            text: "first".to_owned(),
            images: Vec::new(),
        },
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: second,
            text: "second".to_owned(),
            images: Vec::new(),
        },
        EventKind::PromptDeliveryExpedited { through: first },
        EventKind::PromptDeliveryExpedited { through: first },
        EventKind::PromptDeliveryExpedited { through: second },
        EventKind::PromptDeliveryExpedited { through: first },
    ]);

    assert_eq!(state.expedited_through(), Some(second));
    assert!(state.interrupt_pending());
}

#[test]
fn crash_before_correlated_ack_replays_the_prompt_once() {
    let first = QueuedPromptId::new();
    let second = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: first,
            text: "first".to_owned(),
            images: Vec::new(),
        },
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: second,
            text: "second".to_owned(),
            images: Vec::new(),
        },
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "first".to_owned(),
            images: Vec::new(),
            queued_prompt: Some(first),
        },
    ]);

    assert_eq!(
        state
            .unacknowledged_prompts()
            .iter()
            .map(|prompt| prompt.id())
            .collect::<Vec<_>>(),
        vec![second]
    );
}

#[test]
fn crash_after_correlated_ack_does_not_replay_the_prompt() {
    let id = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id,
            text: "once".to_owned(),
            images: Vec::new(),
        },
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "once".to_owned(),
            images: Vec::new(),
            queued_prompt: Some(id),
        },
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "once".to_owned(),
            images: Vec::new(),
            queued_prompt: Some(id),
        },
    ]);

    assert_eq!(state.consumed_prompt_count(id), 1);
    assert!(state.unacknowledged_prompts().is_empty());
}

#[test]
fn a_delivery_batch_keeps_each_prompt_correlated_and_fifo() {
    let first = QueuedPromptId::new();
    let second = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: first,
            text: "first".to_owned(),
            images: Vec::new(),
        },
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: second,
            text: "second".to_owned(),
            images: Vec::new(),
        },
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "first".to_owned(),
            images: Vec::new(),
            queued_prompt: Some(first),
        },
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "second".to_owned(),
            images: Vec::new(),
            queued_prompt: Some(second),
        },
    ]);

    assert_eq!(state.consumed_prompt_ids(), vec![first, second]);
    assert!(state.unacknowledged_prompts().is_empty());
}

#[test]
fn queue_text_is_not_a_tool_result_or_a_call_result_boundary() {
    let id = QueuedPromptId::new();
    let turn = TurnId::new();
    let mut seq = Seq::default();
    let mut state = SessionState::default();
    let call_item = ItemId::new();
    let prompt_item = ItemId::new();
    for (item, kind) in [
        (
            None,
            EventKind::PromptQueued {
                audio: Vec::new(),
                id,
                text: "follow up".to_owned(),
                images: Vec::new(),
            },
        ),
        (
            Some(call_item),
            EventKind::ToolCallStarted {
                name: "read".to_owned(),
                args: "{}".to_owned(),
                call_id: Some("call-1".to_owned()),
            },
        ),
        (
            Some(call_item),
            EventKind::ToolCallCompleted {
                ok: true,
                result: ego_core::permit::ToolResult::builtin("tool output"),
            },
        ),
        (
            Some(prompt_item),
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "follow up".to_owned(),
                images: Vec::new(),
                queued_prompt: Some(id),
            },
        ),
    ] {
        state.apply(&event(&mut seq, turn, item, kind));
    }

    let prompt_item = state
        .items
        .values()
        .find(|item| item.text == "follow up")
        .expect("the correlated prompt is a user item");
    assert_eq!(prompt_item.kind, ego_core::state::ItemKind::UserPrompt);
    assert!(
        !state.items.values().any(
            |item| item.text == "follow up" && item.kind == ego_core::state::ItemKind::ToolCall
        )
    );
}

#[test]
fn a_no_tool_final_still_leaves_the_follow_up_replayable_until_consumed() {
    let id = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id,
            text: "answer this next".to_owned(),
            images: Vec::new(),
        },
        EventKind::AssistantMessage {
            text: "final answer".to_owned(),
        },
    ]);

    assert_eq!(
        state
            .unacknowledged_prompts()
            .iter()
            .map(|p| p.id())
            .collect::<Vec<_>>(),
        vec![id]
    );
}

#[test]
fn the_prompt_ledger_retains_accounting_across_labelled_interruption_phases() {
    for phase in ["sampling", "permission", "running_tool", "sibling_batch"] {
        let id = QueuedPromptId::new();
        let state = replay([
            EventKind::PromptQueued {
                audio: Vec::new(),
                id,
                text: format!("interrupt during {phase}"),
                images: Vec::new(),
            },
            EventKind::PromptDeliveryExpedited { through: id },
        ]);

        assert_eq!(state.expedited_through(), Some(id), "phase={phase}");
        assert_eq!(
            state
                .unacknowledged_prompts()
                .iter()
                .map(|p| p.id())
                .collect::<Vec<_>>(),
            vec![id]
        );
    }
}

#[test]
fn queue_status_replays_active_completed_and_expedited_turns_exactly_once() {
    let active_turn = TurnId::new();
    let completed_turn = TurnId::new();
    let ordinary_id = QueuedPromptId::new();
    let expedited_id = QueuedPromptId::new();
    let mut seq = Seq::default();
    let events = vec![
        event(
            &mut seq,
            active_turn,
            Some(ItemId::new()),
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "initial prompt".to_owned(),
                images: Vec::new(),
                queued_prompt: None,
            },
        ),
        event(
            &mut seq,
            active_turn,
            None,
            EventKind::PromptQueued {
                audio: Vec::new(),
                id: expedited_id,
                text: "busy steer".to_owned(),
                images: Vec::new(),
            },
        ),
        event(
            &mut seq,
            active_turn,
            None,
            EventKind::PromptDeliveryExpedited {
                through: expedited_id,
            },
        ),
        event(
            &mut seq,
            active_turn,
            None,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ego_core::event::domain::ExitReason::Completed,
            },
        ),
        event(
            &mut seq,
            completed_turn,
            None,
            EventKind::PromptQueued {
                audio: Vec::new(),
                id: ordinary_id,
                text: "idle prompt".to_owned(),
                images: Vec::new(),
            },
        ),
    ];

    let mut state = SessionState::default();
    state.apply(&events[0]);
    state.apply(&events[1]);
    assert_eq!(state.queue_status().pending_steers, 1);
    assert_eq!(state.queue_status().queued, 0);
    assert!(!state.queue_status().interrupt_pending);

    state.apply(&events[2]);
    assert!(state.queue_status().interrupt_pending);
    state.apply(&events[3]);
    assert_eq!(state.queue_status().pending_steers, 0);
    assert_eq!(state.queue_status().queued, 1);
    state.apply(&events[4]);
    assert_eq!(state.queue_status().queued, 2);

    let mut replayed = SessionState::default();
    for event in &events {
        replayed.apply(event);
    }
    assert_eq!(replayed.queue_status(), state.queue_status());

    let ack = event(
        &mut seq,
        completed_turn,
        Some(ItemId::new()),
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "busy steer".to_owned(),
            images: Vec::new(),
            queued_prompt: Some(expedited_id),
        },
    );
    state.apply(&ack);
    assert!(!state.queue_status().interrupt_pending);
    assert_eq!(state.consumed_prompt_count(expedited_id), 1);
}

#[test]
fn non_cancellable_builtin_tool_records_its_truthful_builtin_completion() {
    let turn = TurnId::new();
    let mut seq = Seq::default();
    let mut state = SessionState::default();
    let item = ItemId::new();
    let call_id = "call-builtin";

    state.apply(&event(
        &mut seq,
        turn,
        Some(item),
        EventKind::ToolCallStarted {
            name: "write".to_owned(),
            args: "{\"path\":\"notes.md\"}".to_owned(),
            call_id: Some(call_id.to_owned()),
        },
    ));
    state.apply(&event(
        &mut seq,
        turn,
        Some(item),
        EventKind::ToolCallCompleted {
            ok: true,
            result: ego_core::permit::ToolResult::builtin("write completed before stop"),
        },
    ));

    let call = state
        .items
        .values()
        .find(|item| item.call_id.as_deref() == Some(call_id))
        .expect("the builtin call remains represented after it settles");
    assert_eq!(call.ok, Some(true));
    assert_eq!(
        call.tool_result
            .as_ref()
            .expect("the builtin result is durable")
            .model_text(),
        "write completed before stop"
    );
}

#[test]
fn not_yet_run_siblings_close_with_explicit_interrupted_results() {
    let turn = TurnId::new();
    let mut seq = Seq::default();
    let mut state = SessionState::default();
    let first_item = ItemId::new();
    let second_item = ItemId::new();

    for (item, call_id, name) in [
        (first_item, "call-first", "read"),
        (second_item, "call-not-run", "write"),
    ] {
        state.apply(&event(
            &mut seq,
            turn,
            Some(item),
            EventKind::ToolCallStarted {
                name: name.to_owned(),
                args: "{}".to_owned(),
                call_id: Some(call_id.to_owned()),
            },
        ));
    }
    state.apply(&event(
        &mut seq,
        turn,
        Some(first_item),
        EventKind::ToolCallCompleted {
            ok: true,
            result: ego_core::permit::ToolResult::builtin("first completed"),
        },
    ));
    state.apply(&event(
        &mut seq,
        turn,
        Some(second_item),
        EventKind::ToolCallCompleted {
            ok: false,
            result: ego_core::permit::ToolResult::builtin("interrupted: not run"),
        },
    ));

    let first = state
        .items
        .values()
        .find(|item| item.call_id.as_deref() == Some("call-first"))
        .expect("first sibling exists");
    let not_run = state
        .items
        .values()
        .find(|item| item.call_id.as_deref() == Some("call-not-run"))
        .expect("not-run sibling exists");
    assert_eq!(first.ok, Some(true));
    assert_eq!(not_run.ok, Some(false));
    assert_eq!(
        not_run
            .tool_result
            .as_ref()
            .expect("an interrupted sibling has an explicit result")
            .model_text(),
        "interrupted: not run"
    );
}

// Catches: replay resurrecting withdrawn text as a pending steer on resume.
#[test]
fn retracted_prompt_is_absent_on_replay_and_preserves_other_input() {
    let first = QueuedPromptId::new();
    let last = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: first,
            text: "keep".into(),
            images: vec![],
        },
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: last,
            text: "withdraw".into(),
            images: vec![],
        },
        EventKind::PromptRetracted { id: last },
        EventKind::PromptRetracted { id: last },
    ]);
    assert_eq!(
        state
            .unacknowledged_prompts()
            .iter()
            .map(|prompt| prompt.text())
            .collect::<Vec<_>>(),
        ["keep"]
    );
}
// Catches: a stale retraction removing an older queue entry instead of its tail.
#[test]
fn retraction_cannot_remove_a_non_tail_prompt() {
    let first = QueuedPromptId::new();
    let last = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: first,
            text: "first".into(),
            images: vec![],
        },
        EventKind::PromptQueued {
            audio: Vec::new(),
            id: last,
            text: "last".into(),
            images: vec![],
        },
        EventKind::PromptRetracted { id: first },
    ]);
    assert_eq!(state.unacknowledged_prompts().len(), 2);
}

// Catches: stale withdrawal hiding an already consumed conversation prompt.
#[test]
fn retraction_does_not_remove_consumed_conversation_text() {
    let id = QueuedPromptId::new();
    let state = replay([
        EventKind::PromptQueued {
            audio: Vec::new(),
            id,
            text: "delivered".into(),
            images: vec![],
        },
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "delivered".into(),
            images: vec![],
            queued_prompt: Some(id),
        },
        EventKind::PromptRetracted { id },
    ]);
    assert!(state.unacknowledged_prompts().is_empty());
    assert_eq!(
        state
            .items
            .values()
            .filter(|item| item.text == "delivered")
            .count(),
        1
    );
}
