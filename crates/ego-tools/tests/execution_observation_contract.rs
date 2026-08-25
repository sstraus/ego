//! RED contract for durable execution observations.
//!
//! The log is authoritative and the manifest is only its private catalogue
//! cache.  These tests deliberately inspect bytes through the public
//! `JsonlStore`/`StoreIo` surface, never a private manifest wrapper.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::permit::{BackendIdentity, ExecCaps, ExecObservation, PolicyDigest};
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, OsIo, StoreIo};
use tempfile::tempdir;

fn observation(backend: &str, revision: &str) -> ExecObservation {
    let digest: PolicyDigest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        .try_into()
        .expect("observation digest");
    ExecObservation::new(
        BackendIdentity::new(backend, revision),
        ExecCaps::empty(),
        digest,
    )
}

fn event(seq: &mut Seq, turn: TurnId, timestamp: u64, kind: EventKind) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn,
            item: None,
            parent: None,
            ts: Timestamp::from_millis(timestamp),
            producer: Producer::Main,
        },
        kind,
    }
}

fn session_dir(root: &Path, id: &SessionId) -> PathBuf {
    let id = serde_json::to_value(id).expect("session id JSON");
    root.join(id.as_str().expect("session id is a string"))
}

fn manifest_bytes(root: &Path, id: &SessionId) -> Vec<u8> {
    std::fs::read(session_dir(root, id).join("manifest.json")).expect("manifest exists")
}

fn event_log_lines(root: &Path, id: &SessionId) -> Vec<String> {
    std::fs::read_to_string(session_dir(root, id).join("events.jsonl"))
        .expect("event log exists")
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn execution_observation_fsyncs_the_log_before_replacing_the_manifest() {
    #[derive(Clone)]
    struct OrderingIo {
        inner: OsIo,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl StoreIo for OrderingIo {
        fn append_line(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
            self.calls.lock().unwrap().push("append");
            self.inner.append_line(path, line)
        }

        fn fsync(&self, path: &Path) -> std::io::Result<()> {
            self.calls.lock().unwrap().push("fsync");
            self.inner.fsync(path)
        }

        fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
            self.inner.read(path)
        }

        fn exists(&self, path: &Path) -> std::io::Result<bool> {
            self.inner.exists(path)
        }

        fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            self.calls.lock().unwrap().push("replace");
            self.inner.replace_atomic(path, bytes)
        }

        fn truncate(&self, path: &Path, len: u64) -> std::io::Result<()> {
            self.inner.truncate(path, len)
        }

        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            self.inner.remove_file(path)
        }

        fn list_dirs(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
            self.inner.list_dirs(path)
        }
    }

    let root = tempdir().expect("store root");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let io = OrderingIo {
        inner: OsIo,
        calls: Arc::clone(&calls),
    };
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(io));
    let id = SessionId::new();
    let mut seq = Seq::default();
    let turn = TurnId::new();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                1,
                EventKind::ExecutionObserved {
                    observation: observation("null", "test"),
                },
            ),
        )
        .expect("observation append");

    let calls = calls.lock().unwrap();
    let fsync = calls.iter().position(|call| *call == "fsync");
    let replace = calls.iter().position(|call| *call == "replace");
    assert!(fsync.is_some(), "ExecutionObserved must force the log");
    assert!(
        replace.is_some(),
        "ExecutionObserved must update the manifest"
    );
    assert!(
        fsync < replace,
        "log fsync must precede manifest replacement: {calls:?}"
    );
}

#[test]
fn normal_manifest_cadence_preserves_the_latest_execution_observation() {
    let root = tempdir().expect("store root");
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let id = SessionId::new();
    let turn = TurnId::new();
    let mut seq = Seq::default();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                1,
                EventKind::ExecutionObserved {
                    observation: observation("landlock", "v1"),
                },
            ),
        )
        .unwrap();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                2,
                EventKind::UserPrompt {
                    text: "prompt".to_string(),
                    queued_prompt: None,
                },
            ),
        )
        .unwrap();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                3,
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ExitReason::Completed,
                },
            ),
        )
        .unwrap();

    let manifest = serde_json::from_slice::<serde_json::Value>(&manifest_bytes(root.path(), &id))
        .expect("manifest JSON");
    assert!(
        manifest.get("meta").is_none(),
        "SessionMeta must be flattened"
    );
    assert_eq!(manifest["execution"]["backend"]["name"], "landlock");
    assert_eq!(manifest["execution"]["backend"]["revision"], "v1");
    assert_eq!(
        manifest["execution"]["policy_digest"],
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        store.list().unwrap().len(),
        1,
        "catalogue remains SessionMeta-only"
    );
}

#[test]
fn recovery_clears_a_stale_manifest_observation_when_the_event_is_not_in_valid_log_prefix() {
    let root = tempdir().expect("store root");
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let id = SessionId::new();
    let turn = TurnId::new();
    let mut seq = Seq::default();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                1,
                EventKind::ExecutionObserved {
                    observation: observation("landlock", "v1"),
                },
            ),
        )
        .unwrap();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                2,
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ExitReason::Completed,
                },
            ),
        )
        .unwrap();

    let mut lines = event_log_lines(root.path(), &id);
    assert!(
        lines.len() >= 3,
        "header, observation, and turn boundary are required"
    );
    lines.remove(1);
    std::fs::write(
        session_dir(root.path(), &id).join("events.jsonl"),
        lines.join("\n") + "\n",
    )
    .unwrap();

    store.recover(&id).expect("recovery");
    let manifest = String::from_utf8(manifest_bytes(root.path(), &id)).unwrap();
    assert!(!manifest.contains("landlock"));
    assert!(!manifest.contains("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));
}

#[test]
fn recovery_rebuilds_the_latest_observation_from_a_valid_log_when_manifest_is_missing() {
    let root = tempdir().expect("store root");
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let id = SessionId::new();
    let turn = TurnId::new();
    let mut seq = Seq::default();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                1,
                EventKind::ExecutionObserved {
                    observation: observation("landlock", "v1"),
                },
            ),
        )
        .unwrap();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                2,
                EventKind::ExecutionObserved {
                    observation: observation("seatbelt", "v2"),
                },
            ),
        )
        .unwrap();

    std::fs::remove_file(session_dir(root.path(), &id).join("manifest.json"))
        .expect("remove derived manifest");
    store.recover(&id).expect("recovery rebuilds manifest");

    let manifest = String::from_utf8(manifest_bytes(root.path(), &id)).unwrap();
    assert!(
        manifest.contains("seatbelt"),
        "latest valid observation was not rebuilt"
    );
    assert!(
        manifest.contains("v2"),
        "latest observation revision was not rebuilt"
    );
}

#[test]
fn historical_log_without_execution_observed_reconstructs_without_observation() {
    let root = tempdir().expect("store root");
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let id = SessionId::new();
    let turn = TurnId::new();
    let mut seq = Seq::default();
    store
        .append(
            &id,
            &event(
                &mut seq,
                turn,
                1,
                EventKind::UserPrompt {
                    text: "old".to_string(),
                    queued_prompt: None,
                },
            ),
        )
        .unwrap();
    store.recover(&id).expect("recovery");
    let manifest = String::from_utf8(manifest_bytes(root.path(), &id)).unwrap();
    assert!(!manifest.contains("policy_digest"));
    assert!(!manifest.contains("landlock"));
}
