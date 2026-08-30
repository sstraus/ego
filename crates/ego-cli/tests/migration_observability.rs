//! RED contract for observable, lossless v8-to-v9 session migration.

use ego_cli::session::Opened;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::store::codec::{Header, encode_header, encode_record};
use ego_tools::store::{JsonlStore, LeaseInputs, OsIo};
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
fn clean_v8_migration_is_reported_as_migration_not_torn_tail_loss() {
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
    let open = store
        .open_writer(
            &id,
            LeaseInputs {
                pid: 64064,
                now: Timestamp::from_millis(4_200),
                stale_after: Duration::from_secs(30),
                is_alive: ALIVE,
            },
        )
        .expect("open migrates the v8 session");

    let repair_debug = format!("{:?}", open.repair);
    let repair_identifies_migration = repair_debug.contains("migrated: true");
    let lossless = open.repair.truncated_bytes == 0 && open.repair.dropped_lines == 0;
    let opened = Opened::from(open);
    let note = opened.repair_note().unwrap_or_default();
    let note_identifies_migration = note.to_ascii_lowercase().contains("migrat");
    let note_does_not_claim_tail_loss = !note.to_ascii_lowercase().contains("torn tail");

    assert!(
        repair_identifies_migration,
        "Repair must identify a clean codec migration: {repair_debug}"
    );
    assert!(lossless, "migration must drop zero bytes and lines");
    assert!(
        note_identifies_migration && note_does_not_claim_tail_loss,
        "CLI repair note must identify migration, not torn-tail loss: {note:?}"
    );
}
