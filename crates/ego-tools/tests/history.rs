//! Real JSONL/SQLite boundary regressions; no provider or fabricated protocol.
use std::fs;
use std::sync::Arc;
use std::time::Instant;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::permit::ToolResult;
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, NonDurableOsIo};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    store: Arc<JsonlStore>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonlStore::new(
            dir.path().join("sessions"),
            dir.path(),
            Box::new(NonDurableOsIo),
        ));
        Self { dir, store }
    }
    fn seed(&self, title: &str) -> SessionId {
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::ZERO;
        let assistant = ItemId::new();
        for (item, kind) in [
            (
                None,
                EventKind::SessionTitleChanged {
                    title: Some(title.into()),
                },
            ),
            (
                Some(ItemId::new()),
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "acp trust userneedle".into(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            ),
            (
                Some(assistant),
                EventKind::AssistantDelta {
                    text: "deltaneedle".into(),
                },
            ),
            (
                Some(assistant),
                EventKind::AssistantMessage {
                    text: "acp trust assistantneedle".into(),
                },
            ),
            (
                Some(ItemId::new()),
                EventKind::ToolCallCompleted {
                    ok: true,
                    result: ToolResult::builtin("toolneedle".repeat(1000)),
                },
            ),
            (
                None,
                EventKind::Compacted {
                    summary: "summaryneedle".into(),
                },
            ),
        ] {
            self.store
                .append(
                    &id,
                    &DomainEvent {
                        env: Envelope {
                            seq: seq.next(),
                            id: EventId::new(),
                            turn,
                            item,
                            parent: None,
                            ts: Timestamp::from_millis(1000),
                            producer: Producer::Main,
                        },
                        kind,
                    },
                )
                .unwrap();
        }
        id
    }
    fn log(&self, id: SessionId) -> Vec<u8> {
        fs::read(
            self.dir
                .path()
                .join("sessions")
                .join(id.to_string())
                .join("events.jsonl"),
        )
        .unwrap()
    }
}

// Catches: history leaks raw tool payload or repeated streamed deltas and exceeds 1 KiB.
#[test]
fn history_search_indexes_conversation_not_tool_payload_and_caps_json() {
    let fixture = Fixture::new();
    for _ in 0..8 {
        fixture.seed(&format!("titleneedle {}", "🦀\"\n".repeat(120)));
    }
    let history = fixture.store.history();
    for query in [
        "acp trust",
        "userneedle",
        "assistantneedle",
        "titleneedle",
        "summaryneedle",
    ] {
        let result = history.search(query).unwrap();
        assert!(result.len() <= 1024, "{} bytes", result.len());
        let hits: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(!hits.as_array().unwrap().is_empty(), "{query}: {result}");
        for hit in hits.as_array().unwrap() {
            assert!(hit["title"].is_string());
            assert_eq!(hit["updated"], "1970-01-01T00:00:01.000Z");
            assert!(hit["snippet"].is_string());
        }
    }
    assert_eq!(history.search("toolneedle").unwrap(), "[]");
    assert_eq!(history.search("deltaneedle").unwrap(), "[]");
    assert_eq!(history.search(" \t ").unwrap(), "[]");
    assert!(history.search("\" : OR *").is_ok());
}

// Catches: derived index deletion/corruption changes results or rewrites durable logs.
#[test]
fn history_deleted_and_corrupt_index_rebuild_identically_without_mutating_jsonl() {
    let fixture = Fixture::new();
    let id = fixture.seed("Acp trust decisions");
    let source = fixture.log(id);
    let index = fixture.dir.path().join("sessions/history.sqlite");
    let expected = fixture.store.history().search("acp trust").unwrap();
    fs::remove_file(&index).unwrap();
    assert_eq!(
        fixture.store.history().search("acp trust").unwrap(),
        expected
    );
    fs::write(&index, b"not a sqlite database").unwrap();
    assert_eq!(
        fixture.store.history().search("acp trust").unwrap(),
        expected
    );
    assert_eq!(fixture.log(id), source);
}

// Catches: cached title/conversation survives a same-timestamp durable append or deletion.
#[test]
fn history_refresh_and_open_use_current_durable_conversation() {
    let fixture = Fixture::new();
    let id = fixture.seed("Old title");
    let history = fixture.store.history();
    assert!(!history.matches("Old").unwrap().is_empty());
    let opened = history.open(id).unwrap();
    assert!(opened.contains("user: acp trust userneedle"));
    assert!(opened.contains("assistant: acp trust assistantneedle"));
    assert!(!opened.contains("toolneedle") && !opened.contains("deltaneedle"));
    let mut seq = Seq::ZERO;
    for _ in 0..6 {
        seq.next();
    }
    let mut event = |kind| DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(1000),
            producer: Producer::Main,
        },
        kind,
    };
    fixture
        .store
        .append(
            &id,
            &event(EventKind::SessionTitleChanged {
                title: Some("New title".into()),
            }),
        )
        .unwrap();
    assert_eq!(history.search("Old").unwrap(), "[]");
    assert_eq!(history.matches("New").unwrap()[0].title, "New title");
    fixture
        .store
        .append(&id, &event(EventKind::SessionDeleted))
        .unwrap();
    assert_eq!(history.search("acp trust").unwrap(), "[]");
}

// Catches: per-query raw transcript scanning replaces indexed lookup at catalogue scale.
#[test]
fn history_search_of_one_thousand_sessions_stays_below_one_hundred_ms() {
    let fixture = Fixture::new();
    for n in 0..1000 {
        fixture.seed(&format!("History session {n:04}"));
    }
    let history = fixture.store.history();
    let expected = history.search("acp trust").unwrap(); // First build is not a search-latency sample.
    let started = Instant::now();
    let actual = history.search("acp trust").unwrap();
    let elapsed = started.elapsed();
    assert_eq!(actual, expected);
    assert!(
        elapsed.as_millis() < 100,
        "warmed search of 1000 real-store sessions took {elapsed:?}"
    );
}

// Catches: an active writer with no first prompt makes all history searches fail.
#[test]
fn history_empty_active_session_does_not_hide_other_sessions() {
    use ego_tools::store::LeaseInputs;
    use std::time::Duration;
    let fixture = Fixture::new();
    let saved = fixture.seed("Saved history");
    let active = SessionId::new();
    let alive = |_: u32| true;
    let _writer = fixture
        .store
        .open_writer(
            &active,
            LeaseInputs {
                pid: std::process::id(),
                now: Timestamp::from_millis(1001),
                stale_after: Duration::from_secs(60),
                is_alive: &alive,
            },
        )
        .unwrap();
    assert_eq!(
        fixture.store.history().matches("acp trust").unwrap()[0].id,
        saved
    );
}
