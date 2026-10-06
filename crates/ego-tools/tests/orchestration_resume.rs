use ego_core::orchestration_events::{BATCH_WINDOW_MS, Card};
use ego_core::orchestrator::SpawnReceipt;
use ego_tools::orchestration_events::{InboxPage, ObservationPump};

const MAIL: &str = include_str!("fixtures/orchestrator/inbox.json");
const CHILDREN: &str = include_str!("fixtures/orchestrator/children.tsv");
// Catches: mail/file duplication reserves two turns, or restart silently reexecutes reserved work.
#[test]
fn resume_file_and_recorded_mail_share_one_persisted_reservation_and_restart_failure() {
    let home = tempfile::tempdir().unwrap();
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    let results = home.path().join("Gits/.tmp/results");
    std::fs::create_dir_all(&results).unwrap();
    std::fs::write(
        home.path().join("Gits/.tmp/coordinator/children.tsv"),
        CHILDREN,
    )
    .unwrap();
    let page: InboxPage = serde_json::from_str(MAIL).unwrap();
    let child = page.messages[0].from_tuic_session.clone().unwrap();
    std::fs::write(
        results.join("example-coordinator-report.md"),
        &page.messages[0].content,
    )
    .unwrap();
    pump.ingest_inbox(page, 100).unwrap();
    pump.scan_results(100).unwrap();
    let cards = pump.ready(100 + BATCH_WINDOW_MS).to_vec();
    assert_eq!(cards.len(), 1);
    let receipt = SpawnReceipt {
        session_id: child,
        name: "example-coordinator".into(),
        cwd: home.path().into(),
        prompt: "Recorded task".into(),
        spawned_at_ms: 1,
    };
    assert!(
        pump.reserve_resume(&cards[0], "launching-chat", &receipt)
            .unwrap()
    );
    assert!(
        !pump
            .reserve_resume(&cards[0], "other-chat", &receipt)
            .unwrap()
    );
    drop(pump);
    let mut reopened = ObservationPump::open(home.path(), home.path()).unwrap();
    assert!(
        !reopened
            .reserve_resume(&cards[0], "launching-chat", &receipt)
            .unwrap()
    );
    let failures = reopened.interrupted().to_vec();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, "launching-chat");
    reopened.acknowledge_interrupted(&failures[0].1.id).unwrap();
    drop(reopened);
    assert!(
        ObservationPump::open(home.path(), home.path())
            .unwrap()
            .interrupted()
            .is_empty()
    );
}
// Catches: acknowledging one conversation discards another disconnected conversation's result.
#[test]
fn resume_acknowledges_each_card_without_losing_other_pending_cards() {
    let home = tempfile::tempdir().unwrap();
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    pump.failure("first".into(), 100);
    pump.failure("second".into(), 100);
    let cards: Vec<Card> = pump.ready(100 + BATCH_WINDOW_MS).to_vec();
    assert_eq!(cards.len(), 2);
    pump.acknowledge_card(&cards[0].id).unwrap();
    drop(pump);
    let reopened = ObservationPump::open(home.path(), home.path()).unwrap();
    assert_eq!(reopened.ready(100 + BATCH_WINDOW_MS), &cards[1..]);
}
