use ego_core::orchestration_events::{Card, Cursor};
use ego_core::orchestrator::SpawnReceipt;

fn receipt() -> SpawnReceipt {
    SpawnReceipt {
        session_id: "child-session".into(),
        name: "worker".into(),
        cwd: "/work".into(),
        prompt: "work".into(),
        spawned_at_ms: 10,
    }
}
fn card() -> Card {
    Card {
        id: "report-one".into(),
        text: "RESULT: done".into(),
        result: None,
        at_ms: 11,
        peer: Some("worker".into()),
        sender: Some("child-session".into()),
    }
}
// Catches: duplicate delivery or a forged same-name sender resumes another conversation.
#[test]
fn resume_reservation_rejects_duplicate_stale_and_foreign_reports() {
    let mut cursor = Cursor::default();
    let mut report = card();
    report.sender = Some("unrelated-session".into());
    assert!(!cursor.reserve_resume(&report, "chat", &receipt()));
    report = card();
    report.at_ms = 9;
    assert!(!cursor.reserve_resume(&report, "chat", &receipt()));
    assert!(cursor.reserve_resume(&card(), "chat", &receipt()));
    assert!(!cursor.reserve_resume(&card(), "other-chat", &receipt()));
    cursor.finish_resume("report-one");
    let mut restored: Cursor =
        serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
    assert!(!restored.reserve_resume(&card(), "chat", &receipt()));
    assert!(restored.interrupt_resumes(100).is_empty());
}
// Catches: a restart replays a queued or admitted resume and its external effects.
#[test]
fn resume_restart_fails_unfinished_reservations_once_without_replay() {
    let mut cursor = Cursor::default();
    assert!(cursor.reserve_resume(&card(), "chat", &receipt()));
    let mut restored: Cursor =
        serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
    let failures = restored.interrupt_resumes(100);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, "chat");
    assert!(failures[0].1.text.contains("interrupted"));
    assert!(failures[0].1.text.contains("not replayed"));
    assert!(!restored.reserve_resume(&card(), "chat", &receipt()));
    assert!(restored.interrupt_resumes(101).is_empty());
}

// Catches: global peer-name deduplication swallows a new child with a reused display name.
#[test]
fn resume_sender_identity_keeps_reused_peer_names_distinct() {
    use ego_core::orchestration_events::{BATCH_WINDOW_MS, Observation};
    let mut cursor = Cursor::default();
    for sender in ["child-one", "child-two"] {
        cursor.observe(
            Observation {
                text: "RESULT: done".into(),
                at_ms: 11,
                result: None,
                peer: Some("worker".into()),
                sender: Some(sender.into()),
            },
            100,
        );
    }
    assert_eq!(cursor.ready(100 + BATCH_WINDOW_MS).len(), 2);
}
