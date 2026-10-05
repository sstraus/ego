//! contract for regression case's generalized atomic fork publication.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::session::{TargetPublication, TargetSeed, TargetStore};
use ego_core::store::session_store::{SessionEntry, SessionStore};
use ego_tools::store::{AtomicReplacePublication, JsonlStore, LeaseInputs, OsIo, StoreIo};
use futures::executor::block_on;
use futures::stream::StreamExt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;

const ALIVE: &dyn Fn(u32) -> bool = &|_| true;
const SEED_TIMESTAMP: Timestamp = Timestamp::from_millis(6_506_501);

fn lease() -> LeaseInputs<'static> {
    LeaseInputs {
        pid: 65065,
        now: Timestamp::from_millis(6_506_500),
        stale_after: Duration::from_secs(30),
        is_alive: ALIVE,
    }
}

fn seq(value: u64) -> Seq {
    serde_json::from_value(serde_json::json!(value)).unwrap()
}

fn seed(source: SessionId) -> TargetSeed {
    TargetSeed::fork(source, seq(17), SEED_TIMESTAMP)
}

fn source_event() -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: seq(1),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(6_506_500),
            producer: Producer::Main,
        },
        kind: EventKind::UserPrompt {
            text: "source must remain unchanged".to_owned(),
            images: Vec::new(),
            queued_prompt: None,
        },
    }
}

fn path(root: &Path, id: &SessionId, name: &str) -> PathBuf {
    root.join(id.to_string()).join(name)
}

fn read_entries(store: &JsonlStore, id: &SessionId) -> Vec<SessionEntry> {
    block_on(store.read(id).unwrap().collect::<Vec<_>>())
        .into_iter()
        .map(|entry| entry.unwrap())
        .collect()
}

struct AppendFailure;

impl StoreIo for AppendFailure {
    fn append_line(&self, _path: &Path, _line: &[u8]) -> io::Result<()> {
        Err(io::Error::other("injected target append failure"))
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
        OsIo.fsync(path)
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        OsIo.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.replace_atomic(path, bytes)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OsIo.truncate(path, len)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        OsIo.remove_file(path)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        OsIo.list_dirs(path)
    }
}

struct UncertainReplace;

impl StoreIo for UncertainReplace {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, line)
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
        OsIo.fsync(path)
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        OsIo.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.replace_atomic(path, bytes)
    }

    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        OsIo.replace_atomic(path, bytes)?;
        Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
            diagnostic: "injected parent-directory durability failure".to_owned(),
        })
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OsIo.truncate(path, len)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        OsIo.remove_file(path)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        OsIo.list_dirs(path)
    }
}

#[test]
fn valid_fork_publication_writes_one_tagged_event_and_manifest_atomically() {
    let root = tempdir().unwrap();
    let source = SessionId::new();
    let target = SessionId::new();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    store.append(&source, &source_event()).unwrap();
    let source_log_before = std::fs::read(path(root.path(), &source, "events.jsonl")).unwrap();
    let source_manifest_before =
        std::fs::read(path(root.path(), &source, "manifest.json")).unwrap();
    let opened = store.open_writer(&target, lease()).unwrap();
    let publication = opened.writer.publish_target(&seed(source));
    assert!(matches!(
        publication,
        TargetPublication::PublishedDurably { .. }
    ));
    drop(opened);

    let entries = read_entries(&store, &target);
    assert_eq!(entries.len(), 1);
    assert!(matches!(
        &entries[0],
        SessionEntry::Event(DomainEvent { env, kind: EventKind::Forked { source: actual, at_seq , .. } })
            if env.seq == seq(1)
                && env.ts == SEED_TIMESTAMP
                && env.item.is_none()
                && *actual == source
                && *at_seq == seq(17)
    ));

    let manifest = std::fs::read_to_string(path(root.path(), &target, "manifest.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(value["lineage"]["kind"], "fork");
    assert_eq!(
        value["lineage"]["source"],
        serde_json::to_value(source).unwrap()
    );
    assert_eq!(
        value["lineage"]["at_seq"],
        serde_json::to_value(seq(17)).unwrap()
    );
    assert_eq!(
        std::fs::read(path(root.path(), &source, "events.jsonl")).unwrap(),
        source_log_before
    );
    assert_eq!(
        std::fs::read(path(root.path(), &source, "manifest.json")).unwrap(),
        source_manifest_before
    );
}

#[test]
fn publication_failure_leaves_no_target_and_durable_success_is_not_retryable() {
    let root = tempdir().unwrap();
    let source = SessionId::new();
    let target = SessionId::new();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(AppendFailure));
    let opened = store.open_writer(&target, lease()).unwrap();
    let publication = opened.writer.publish_target(&seed(source));
    assert!(matches!(
        publication,
        TargetPublication::NotPublished { .. }
    ));
    assert!(!publication.switch_target());
    assert!(publication.retry_safe());
    assert!(!path(root.path(), &target, "events.jsonl").exists());
    assert!(!path(root.path(), &target, "manifest.json").exists());

    let second_root = tempdir().unwrap();
    let second_target = SessionId::new();
    let store = JsonlStore::new(second_root.path(), "/workspace", Box::new(OsIo));
    let opened = store.open_writer(&second_target, lease()).unwrap();
    let publication = opened.writer.publish_target(&seed(source));
    assert!(matches!(
        publication,
        TargetPublication::PublishedDurably { .. }
    ));
    assert!(publication.switch_target());
    assert!(!publication.retry_safe());
}

#[test]
fn uncertain_publication_is_visible_but_retry_is_unsafe_and_collision_is_no_effect() {
    let root = tempdir().unwrap();
    let source = SessionId::new();
    let target = SessionId::new();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(UncertainReplace));
    let opened = store.open_writer(&target, lease()).unwrap();
    let publication = opened.writer.publish_target(&seed(source));
    assert!(matches!(
        publication,
        TargetPublication::PublishedDurabilityUncertain { .. }
    ));
    assert!(publication.switch_target());
    assert!(!publication.retry_safe());
    let before = std::fs::read(path(root.path(), &target, "events.jsonl")).unwrap();
    let manifest_before = std::fs::read(path(root.path(), &target, "manifest.json")).unwrap();
    let collision = opened.writer.publish_target(&seed(SessionId::new()));
    assert!(matches!(collision, TargetPublication::NotPublished { .. }));
    assert_eq!(
        std::fs::read(path(root.path(), &target, "events.jsonl")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(path(root.path(), &target, "manifest.json")).unwrap(),
        manifest_before
    );
}
