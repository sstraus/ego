use ego_core::orchestration_events::{
    BATCH_WINDOW_MS, Cursor, Observation, STALL_MS, Salience, classify,
};

fn event(text: &str, at_ms: u64) -> Observation {
    Observation {
        text: text.into(),
        at_ms,
        result: None,
        peer: None,
        sender: None,
    }
}

// Catches: quiet deterministic events cause cards, or a judge suppresses a failure.
#[test]
fn result_question_failure_and_nonzero_background_exit_are_cards_without_a_judge() {
    for text in [
        "RESULT child: done",
        "BLOCKED child: missing input",
        "QUESTION operator: choose?",
        "failure: command rejected",
        "BG DONE exit=1",
        "unknown event",
    ] {
        assert_eq!(classify(text), Salience::Card, "{text}");
    }
    for text in [
        "ack",
        "ACK: accepted",
        "ack child registered",
        "",
        "heartbeat",
        "BG DONE exit=0 log=build.log",
    ] {
        assert_eq!(classify(text), Salience::Activity, "{text}");
    }
}

// Catches: duplicate cards, one batch per event, or pending events lost on restart.
#[test]
fn restart_keeps_one_ordered_pending_batch_and_committed_events_do_not_repeat() {
    let mut cursor = Cursor::default();
    cursor.observe(event("RESULT one", 100), 100);
    cursor.observe(event("BLOCKED three", 102), 102);
    cursor.observe(event("QUESTION two", 101), 103);
    assert_eq!(
        cursor.observe(event("  RESULT one\n", 104), 104),
        Salience::Activity
    );
    assert!(cursor.ready(100 + BATCH_WINDOW_MS - 1).is_empty());
    let mut restored: Cursor =
        serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
    let batch = restored.ready(100 + BATCH_WINDOW_MS);
    assert_eq!(
        batch
            .iter()
            .map(|card| card.text.as_str())
            .collect::<Vec<_>>(),
        ["RESULT one", "BLOCKED three", "QUESTION two"]
    );
    assert_eq!(cursor.ready(100 + BATCH_WINDOW_MS), batch);
    restored.acknowledge();
    let mut restored: Cursor =
        serde_json::from_slice(&serde_json::to_vec(&restored).unwrap()).unwrap();
    restored.observe(event("RESULT one", 200), 200);
    assert!(restored.ready(200 + BATCH_WINDOW_MS).is_empty());
}

// Catches: silent idle stalls, repeated stall cards, or a completed peer reported stalled.
#[test]
fn idle_stall_survives_restart_but_result_and_active_work_clear_the_alarm() {
    let mut cursor = Cursor::default();
    cursor.child_state("child", true, 100);
    let mut cursor: Cursor = serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
    cursor.child_state("child", true, 100 + STALL_MS);
    assert!(cursor.ready(u64::MAX).is_empty());
    cursor.child_state("child", true, 101 + STALL_MS);
    assert_eq!(cursor.ready(u64::MAX).len(), 1);
    cursor.acknowledge();
    cursor.child_state("child", true, 200 + STALL_MS);
    assert!(cursor.ready(u64::MAX).is_empty());
    cursor.child_state("child", false, 300 + STALL_MS);
    cursor.observe(
        Observation {
            peer: Some("child".into()),
            sender: None,
            ..event("RESULT child complete", 301 + STALL_MS)
        },
        301 + STALL_MS,
    );
    cursor.child_state("child", false, 302 + STALL_MS);
    cursor.acknowledge();
    cursor.child_state("child", true, 400 + STALL_MS);
    cursor.child_state("child", true, 401 + 2 * STALL_MS);
    assert!(cursor.ready(u64::MAX).is_empty());
}
