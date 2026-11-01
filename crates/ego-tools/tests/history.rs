//! Real JSONL/SQLite boundary regressions; no provider or fabricated protocol.
use std::fs;
use std::sync::Arc;

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
        "userneedle assistantneedle",
        "titleneedle",
        "summaryneedle",
    ] {
        let result = history.search(query).unwrap();
        assert!(result.len() <= 1024, "{} bytes", result.len());
        let hits: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            !hits["hits"].as_array().unwrap().is_empty(),
            "{query}: {result}"
        );
        for hit in hits["hits"].as_array().unwrap() {
            assert!(hit["title"].is_string());
            assert_eq!(hit["updated"], "1970-01-01T00:00:01.000Z");
            assert!(hit["snippet"].is_string());
        }
    }
    assert_eq!(
        history.search("toolneedle").unwrap(),
        r#"{"hits":[],"unreadable":[],"omitted_unreadable":0}"#
    );
    assert_eq!(
        history.search("deltaneedle").unwrap(),
        r#"{"hits":[],"unreadable":[],"omitted_unreadable":0}"#
    );
    assert_eq!(
        history.search(" \t ").unwrap(),
        r#"{"hits":[],"unreadable":[],"omitted_unreadable":0}"#
    );
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
    assert_eq!(
        history.search("Old").unwrap(),
        r#"{"hits":[],"unreadable":[],"omitted_unreadable":0}"#
    );
    assert_eq!(history.matches("New").unwrap()[0].title, "New title");
    fixture
        .store
        .append(&id, &event(EventKind::SessionDeleted))
        .unwrap();
    assert_eq!(
        history.search("acp trust").unwrap(),
        r#"{"hits":[],"unreadable":[],"omitted_unreadable":0}"#
    );
}

// Catches: per-query raw transcript scanning replaces indexed lookup at catalogue scale.
#[test]
fn history_warm_search_of_one_thousand_sessions_does_not_decode_sources() {
    let fixture = Fixture::new();
    for n in 0..1000 {
        fixture.seed(&format!("History session {n:04}"));
    }
    let history = fixture.store.history();
    let expected = history.search("acp trust").unwrap();
    let decoded = history.work().decoded_records;
    let actual = history.search("acp trust").unwrap();
    assert_eq!(actual, expected);
    assert_eq!(history.work().decoded_records, decoded);
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

fn append_history_event(
    fixture: &Fixture,
    id: SessionId,
    turn: TurnId,
    seq: &mut Seq,
    item: Option<ItemId>,
    kind: EventKind,
) {
    fixture
        .store
        .append(
            &id,
            &DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn,
                    item,
                    parent: None,
                    ts: Timestamp::from_millis(2000),
                    producer: Producer::Main,
                },
                kind,
            },
        )
        .unwrap();
}

// Catches: prefix-only recall, evidence leaking by default, and dropped UTF-8 continuation bytes.
#[test]
fn history_anchors_open_late_evidence_and_continue_exact_bytes() {
    let fixture = Fixture::new();
    let id = fixture.seed("Archive");
    let mut seq = Seq::ZERO;
    for _ in 0..6 {
        seq.next();
    }
    append_history_event(
        &fixture,
        id,
        TurnId::new(),
        &mut seq,
        Some(ItemId::new()),
        EventKind::UserPrompt {
            text: "IRRELEVANT-PREFIX".repeat(2000),
            audio: vec![],
            images: vec![],
            queued_prompt: None,
        },
    );
    let turn = TurnId::new();
    append_history_event(
        &fixture,
        id,
        turn,
        &mut seq,
        Some(ItemId::new()),
        EventKind::UserPrompt {
            text: "lateanchor decision".into(),
            audio: vec![],
            images: vec![],
            queued_prompt: None,
        },
    );
    let evidence = "EXACT-EVIDENCE 🦀\n\"".repeat(900);
    append_history_event(
        &fixture,
        id,
        turn,
        &mut seq,
        Some(ItemId::new()),
        EventKind::ToolCallCompleted {
            ok: true,
            result: ToolResult::builtin(&evidence),
        },
    );
    let history = fixture.store.history();
    let result: serde_json::Value =
        serde_json::from_str(&history.search("lateanchor").unwrap()).unwrap();
    let anchor = result["hits"][0]["anchor"].as_str().unwrap();
    let plain = history.open_at(id, Some(anchor), false, None).unwrap();
    assert!(plain.contains("lateanchor decision"));
    assert!(!plain.contains("IRRELEVANT-PREFIX") && !plain.contains("EXACT-EVIDENCE"));
    let mut cursor = None;
    let mut received = String::new();
    loop {
        let page = history
            .open_at(id, Some(anchor), true, cursor.as_deref())
            .unwrap();
        assert!(page.len() <= 8192);
        let page: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert_eq!(page["session"], id.to_string());
        for record in page["records"].as_array().unwrap() {
            assert!(record["event"].is_string());
            assert!(record["source"].is_string());
            if record["kind"] == "tool_result" {
                received.push_str(record["text"].as_str().unwrap());
            }
        }
        cursor = page["continuation"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    // The original serialized result, not a summary, is the evidence oracle.
    let original: serde_json::Value = serde_json::from_str(&received).unwrap();
    assert_eq!(
        original,
        serde_json::to_value(ToolResult::builtin(&evidence)).unwrap()
    );
}

// Catches: revisions silently retarget immutable anchors, truncation leaves ghosts, and deletion remains readable.
#[test]
fn history_revision_replacement_and_deletion_never_retarget_anchor() {
    let fixture = Fixture::new();
    let id = fixture.seed("Revision");
    let original = fixture.log(id);
    let mut seq = Seq::ZERO;
    for _ in 0..6 {
        seq.next();
    }
    let item = ItemId::new();
    let turn = TurnId::new();
    append_history_event(
        &fixture,
        id,
        turn,
        &mut seq,
        Some(item),
        EventKind::AssistantMessage {
            text: "oldrevision".into(),
        },
    );
    let history = fixture.store.history();
    let result: serde_json::Value =
        serde_json::from_str(&history.search("oldrevision").unwrap()).unwrap();
    let anchor = result["hits"][0]["anchor"].as_str().unwrap();
    append_history_event(
        &fixture,
        id,
        turn,
        &mut seq,
        Some(item),
        EventKind::AssistantMessage {
            text: "newrevision".into(),
        },
    );
    assert!(history.matches("oldrevision").unwrap().is_empty());
    assert!(
        history
            .open_at(id, Some(anchor), false, None)
            .unwrap()
            .contains("oldrevision")
    );
    let path = fixture
        .dir
        .path()
        .join("sessions")
        .join(id.to_string())
        .join("events.jsonl");
    fs::write(&path, &original).unwrap();
    assert!(history.open_at(id, Some(anchor), false, None).is_err());
    let mut seq = Seq::ZERO;
    for _ in 0..6 {
        seq.next();
    }
    append_history_event(
        &fixture,
        id,
        turn,
        &mut seq,
        None,
        EventKind::SessionDeleted,
    );
    assert!(history.open_at(id, None, false, None).is_err());
}

// Catches: an append changes pinned revision visibility, invalidates a cursor, or drops pending evidence.
#[test]
fn history_continuation_keeps_revisions_visible_at_its_pinned_end() {
    for prompt_length in [16, 5000, 12000] {
        let fixture = Fixture::new();
        let id = SessionId::new();
        let turn = TurnId::new();
        let item = ItemId::new();
        let mut seq = Seq::ZERO;
        let prompt = format!("pinnedneedle {}", "u".repeat(prompt_length));
        append_history_event(
            &fixture,
            id,
            turn,
            &mut seq,
            Some(ItemId::new()),
            EventKind::UserPrompt {
                text: prompt.clone(),
                audio: vec![],
                images: vec![],
                queued_prompt: None,
            },
        );
        let selected = "selected-assistant 🦀".repeat(400);
        for text in ["superseded-assistant", selected.as_str()] {
            append_history_event(
                &fixture,
                id,
                turn,
                &mut seq,
                Some(item),
                EventKind::AssistantMessage { text: text.into() },
            );
        }
        for title in ["superseded-title", "selected-title"] {
            append_history_event(
                &fixture,
                id,
                turn,
                &mut seq,
                None,
                EventKind::SessionTitleChanged {
                    title: Some(title.into()),
                },
            );
        }
        for summary in ["superseded-summary", "selected-summary"] {
            append_history_event(
                &fixture,
                id,
                turn,
                &mut seq,
                None,
                EventKind::Compacted {
                    summary: summary.into(),
                },
            );
        }
        let history = fixture.store.history();
        let anchor = history.matches("pinnedneedle").unwrap()[0].anchor.clone();
        let first = history.open_at(id, Some(&anchor), false, None).unwrap();
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        let mut cursor = Some(first["continuation"].as_str().unwrap().to_owned());
        let mut received = String::new();
        for record in first["records"].as_array().unwrap() {
            received.push_str(record["text"].as_str().unwrap());
        }
        for kind in [
            EventKind::AssistantMessage {
                text: "appended-assistant".into(),
            },
            EventKind::SessionTitleChanged {
                title: Some("appended-title".into()),
            },
            EventKind::Compacted {
                summary: "appended-summary".into(),
            },
        ] {
            append_history_event(&fixture, id, turn, &mut seq, Some(item), kind);
        }
        // A fresh index handle also must recover the same historical selection.
        let restarted = fixture.store.history();
        while let Some(continuation) = cursor {
            let page = restarted
                .open_at(id, Some(&anchor), false, Some(&continuation))
                .unwrap();
            assert!(page.len() <= 8192);
            let page: serde_json::Value = serde_json::from_str(&page).unwrap();
            for record in page["records"].as_array().unwrap() {
                received.push_str(record["text"].as_str().unwrap());
            }
            cursor = page["continuation"].as_str().map(str::to_owned);
        }
        assert_eq!(
            received,
            format!("user: {prompt}assistant: {selected}selected-titleselected-summary")
        );
        let fresh = restarted.open_at(id, Some(&anchor), false, None).unwrap();
        let fresh: serde_json::Value = serde_json::from_str(&fresh).unwrap();
        assert!(fresh["records"].as_array().unwrap().iter().all(|record| {
            !record["text"].as_str().unwrap().contains("selected-")
                && !record["text"].as_str().unwrap().contains("superseded-")
        }));
        assert_eq!(restarted.matches("appended-assistant").unwrap().len(), 1);
    }
}

// Catches: repeated source decoding, replaying unrelated logs on append, and publishing a partial record.
#[test]
fn history_refresh_work_tracks_complete_source_delta_across_restart() {
    let fixture = Fixture::new();
    let id = fixture.seed("First");
    fixture.seed("Second");
    let history = fixture.store.history();
    history.search("acp").unwrap();
    let cold = history.work();
    history.search("acp").unwrap();
    assert_eq!(history.work().decoded_records, cold.decoded_records);
    let mut seq = Seq::ZERO;
    for _ in 0..6 {
        seq.next();
    }
    append_history_event(
        &fixture,
        id,
        TurnId::new(),
        &mut seq,
        Some(ItemId::new()),
        EventKind::AssistantMessage {
            text: "incrementalneedle".into(),
        },
    );
    history.search("incrementalneedle").unwrap();
    assert!(
        history
            .matches("userneedle incrementalneedle")
            .unwrap()
            .is_empty()
    );
    assert_eq!(history.work().decoded_records - cold.decoded_records, 1);
    let path = fixture
        .dir
        .path()
        .join("sessions")
        .join(id.to_string())
        .join("events.jsonl");
    let source = fs::read(&path).unwrap();
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"incomplete\":")
        .unwrap();
    assert!(!history.matches("incrementalneedle").unwrap().is_empty());
    let restarted = fixture.store.history();
    assert!(!restarted.matches("incrementalneedle").unwrap().is_empty());
    assert_eq!(restarted.work().decoded_records, 0);
    fs::write(&path, &source).unwrap();
    assert!(!restarted.matches("incrementalneedle").unwrap().is_empty());
}

// Catches: an unchanged incompatible archive is decoded and diagnosed again on every query.
#[test]
fn history_unreadable_sources_are_cached_until_revision_changes() {
    let fixture = Fixture::new();
    let id = fixture.seed("Unreadable");
    let path = fixture
        .dir
        .path()
        .join("sessions")
        .join(id.to_string())
        .join("events.jsonl");
    let source = fixture.log(id);
    let mut lines = source.split_inclusive(|byte| *byte == b'\n');
    let header = lines.next().unwrap();
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(b"broken committed record\n");
    fs::write(&path, bytes).unwrap();
    let history = fixture.store.history();
    let first = history.search("acp").unwrap();
    let decoded = history.work().decoded_records;
    assert!(first.contains("unreadable") && first.contains(&id.to_string()));
    assert_eq!(history.search("acp").unwrap(), first);
    assert_eq!(history.work().decoded_records, decoded);
    fs::write(&path, source).unwrap();
    assert_eq!(history.matches("userneedle").unwrap()[0].id, id);
}
