use ego_core::orchestration_events::{BATCH_WINDOW_MS, SCAN_INTERVAL_MS, STALL_MS};
use ego_tools::orchestration_events::{InboxPage, ObservationPump};
use serde_json::Value;

const MAIL: &str = include_str!("fixtures/orchestrator/inbox.json");
const SESSIONS: &str = include_str!("fixtures/orchestrator/sessions.json");
const CHILDREN: &str = include_str!("fixtures/orchestrator/children.tsv");

// Catches: duplicate file/mail cards, lost disconnected cards, or shared watch-stamp mutation.
#[test]
fn recorded_mail_and_result_file_materialize_one_durable_card_within_sixty_seconds() {
    let home = tempfile::tempdir().unwrap();
    let results = home.path().join("Gits/.tmp/results");
    std::fs::create_dir_all(&results).unwrap();
    let page: InboxPage = serde_json::from_str(MAIL).unwrap();
    assert_eq!(page.messages.len(), 1);
    let result = results.join("example-coordinator-fixture.md");
    std::fs::write(&result, &page.messages[0].content).unwrap();
    let stamp = results.join(".watch-stamp");
    std::fs::write(&stamp, "observer must not consume Claude's stamp").unwrap();
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    // The recorded tracking maps this result filename to the mail sender.
    std::fs::write(
        home.path().join("Gits/.tmp/coordinator/children.tsv"),
        CHILDREN,
    )
    .unwrap();
    let observer = pump.observer().to_owned();
    assert_ne!(observer, "ec07d385-51fd-5d74-a472-9917b30a6ad2");
    pump.ingest_inbox(page, SCAN_INTERVAL_MS).unwrap();
    pump.scan_results(SCAN_INTERVAL_MS).unwrap();
    assert!(
        pump.ready(SCAN_INTERVAL_MS + BATCH_WINDOW_MS - 1)
            .is_empty()
    );
    let deadline = SCAN_INTERVAL_MS + BATCH_WINDOW_MS;
    assert!(deadline < 60_000);
    let cards = pump.ready(deadline).to_vec();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].result.as_ref(), Some(&result));
    drop(pump); // Disconnection/restart before delivery does not discard the card.
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    assert_eq!(pump.observer(), observer);
    assert_eq!(pump.ready(deadline), cards);
    pump.acknowledge().unwrap();
    drop(pump);
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    pump.scan_results(deadline + 1).unwrap();
    pump.ingest_inbox(serde_json::from_str(MAIL).unwrap(), deadline + 1)
        .unwrap();
    assert!(pump.ready(60_000).is_empty());
    std::fs::write(&result, "BLOCKED updated result: consumer needs a decision").unwrap();
    pump.scan_results(60_000).unwrap();
    assert_eq!(pump.ready(60_000 + BATCH_WINDOW_MS).len(), 1);
    assert_eq!(
        std::fs::read_to_string(stamp).unwrap(),
        "observer must not consume Claude's stamp"
    );
}

// Catches: concurrent observers consume the same inbox, corrupt cursor resets, or idle timers vanish.
#[test]
fn cursor_lease_corruption_and_recorded_child_idle_are_not_silent() {
    let home = tempfile::tempdir().unwrap();
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    assert!(
        matches!(ObservationPump::open(home.path(), home.path()), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    let children = home.path().join("Gits/.tmp/coordinator/children.tsv");
    std::fs::write(children, CHILDREN).unwrap();
    let sessions: Value = serde_json::from_str(SESSIONS).unwrap();
    pump.ingest_sessions(&sessions, 100).unwrap();
    let cursor = pump.cursor_path().to_owned();
    drop(pump);
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    pump.ingest_sessions(&sessions, STALL_MS + 101).unwrap();
    // Three recorded tracked peers are idle: example-worker-a, example-worker-b, example-worker-d.
    assert_eq!(pump.ready(STALL_MS + 101 + BATCH_WINDOW_MS).len(), 3);
    drop(pump);
    std::fs::write(cursor, "torn cursor").unwrap();
    assert!(ObservationPump::open(home.path(), home.path()).is_err());
}
