//! contract for fail-closed rejection of retired session codecs.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::store::codec::{Header, encode_header, encode_record};
use ego_core::store::session_store::StoreError;
use ego_tools::store::{JsonlStore, LeaseInputs, OpenError, OsIo};
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

const ALIVE: &dyn Fn(u32) -> bool = &|_| true;

fn seq(value: u64) -> Seq {
    let mut result = Seq::ZERO;
    for _ in 0..value {
        result.next();
    }
    result
}

fn event() -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: seq(41),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(4_100),
            producer: Producer::Main,
        },
        kind: EventKind::UserPrompt {
            text: "lossless migration".to_owned(),
            images: Vec::new(),
            queued_prompt: None,
        },
    }
}

fn log_path(root: &Path, id: &SessionId) -> std::path::PathBuf {
    root.join(id.to_string()).join("events.jsonl")
}

#[test]
fn retired_v8_log_is_refused_without_mutating_bytes() {
    let root = tempdir().expect("temporary session root");
    let id = SessionId::new();
    let path = log_path(root.path(), &id);
    std::fs::create_dir_all(path.parent().expect("session directory"))
        .expect("create session directory");

    let mut bytes = encode_header(&Header {
        v: 8,
        session: id,
        created: Timestamp::from_millis(4_000),
        workspace_root: "/workspace".into(),
    });
    bytes.push(b'\n');
    bytes.extend(encode_record(seq(9), &event()).expect("encode v8 event"));
    bytes.push(b'\n');
    std::fs::write(&path, bytes).expect("write v8 fixture");

    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let before = std::fs::read(&path).expect("read v8 fixture before refusal");
    let error = match store.open_writer(
        &id,
        LeaseInputs {
            pid: 64064,
            now: Timestamp::from_millis(4_200),
            stale_after: Duration::from_secs(30),
            is_alive: ALIVE,
        },
    ) {
        Err(error) => error,
        Ok(_) => panic!("retired v8 session codec must be refused"),
    };
    assert!(matches!(
        error,
        OpenError::Recover(StoreError::UnsupportedLog { found: 8 })
    ));
    assert_eq!(
        std::fs::read(&path).expect("read v8 fixture after refusal"),
        before,
        "unsupported codec refusal must not rewrite the source log"
    );
}
