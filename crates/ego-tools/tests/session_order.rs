//! Executable RED contract for regression case physical session ordering.
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::run::{RunAppend, RunRevision, RunTransaction};
use ego_core::state::SessionState;
use ego_core::store::codec::{
    CODEC_VERSION, Header, StoreLine, decode_line, encode_header, encode_record,
};
use ego_core::store::session_store::{SessionEntry, SessionStore};
use ego_tools::store::{JsonlStore, OsIo, SNAPSHOT_VERSION, Snapshot, SnapshotRejection, StoreIo};
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn seq(value: u64) -> Seq {
    serde_json::from_value(serde_json::json!(value)).unwrap()
}
fn event(sequence: u64, text: &str) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: seq(sequence),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(sequence),
            producer: Producer::Main,
        },
        kind: EventKind::UserPrompt {
            text: text.into(),
            images: vec![],
            queued_prompt: None,
        },
    }
}
fn log_path(root: &Path, id: &SessionId) -> PathBuf {
    let name = serde_json::to_string(id).unwrap();
    root.join(name.trim_matches('"')).join("events.jsonl")
}
fn manifest_path(root: &Path, id: &SessionId) -> PathBuf {
    root.join(serde_json::to_string(id).unwrap().trim_matches('"'))
        .join("manifest.json")
}
fn admitted(
    session: &SessionId,
    revision: u64,
    expected_revision: Option<u64>,
    root: &str,
) -> RunTransaction {
    let session = serde_json::to_value(session).unwrap();
    serde_json::from_value(serde_json::json!({
        "revision": revision, "run_id": { "session_id": session, "nonce": "00000000-0000-7000-8000-000000000054" }, "root": root,
        "selections": { "model": "model/test", "thinking": null, "mode": null, "budget": null, "profile": null },
        "command": { "kind": "command_admitted", "request_id": "00000000-0000-7000-8000-000000000055", "digest": "store-race", "expected_revision": expected_revision, "lease": "00000000-0000-7000-8000-000000000056", "executor_id": "00000000-0000-7000-8000-000000000057", "prompt": "store race", "model": "model/test", "start": true },
        "status": "running", "observed_at": 0, "pruned_receipts": []
    })).unwrap()
}
fn records(bytes: &[u8]) -> Vec<(Seq, String)> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| match decode_line(line).unwrap() {
            StoreLine::Record(record) => Some((record.seq, record.payload.get().to_owned())),
            StoreLine::Header(_) | StoreLine::Unknown(_) => None,
        })
        .collect()
}

struct FailingFsyncIo {
    inner: OsIo,
}

impl StoreIo for FailingFsyncIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
        self.inner.append_line(path, line)
    }

    fn fsync(&self, _path: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("simulated durability failure"))
    }

    fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.inner.read(path)
    }

    fn exists(&self, path: &Path) -> std::io::Result<bool> {
        self.inner.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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

#[test]
fn codec_version_is_bumped_for_physical_positions() {
    assert_eq!(CODEC_VERSION, 8);
}

#[test]
fn interleaved_event_and_run_records_use_one_physical_order() {
    let dir = tempdir().unwrap();
    let id = SessionId::new();
    let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    store.append(&id, &event(1, "first")).unwrap();
    assert!(
        matches!(store.compare_and_append_run(&id, None, admitted(&id, 1, None, "/workspace")), Ok(RunAppend::Committed { revision }) if revision == RunRevision::new(1))
    );
    store.append(&id, &event(2, "second")).unwrap();
    let actual = records(&std::fs::read(log_path(dir.path(), &id)).unwrap());
    assert_eq!(
        actual
            .iter()
            .map(|(position, _)| *position)
            .collect::<Vec<_>>(),
        vec![seq(1), seq(2), seq(3)]
    );
    assert!(actual[0].1.contains("first"));
    assert!(actual[1].1.contains("store-race"));
    assert!(actual[2].1.contains("second"));
}

#[test]
fn recovery_rejects_gap_and_regression_without_changing_bytes() {
    for positions in [vec![1, 3], vec![1, 2, 2]] {
        let dir = tempdir().unwrap();
        let id = SessionId::new();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        store.append(&id, &event(1, "seed")).unwrap();
        let path = log_path(dir.path(), &id);
        let header = std::fs::read(&path)
            .unwrap()
            .split(|b| *b == b'\n')
            .next()
            .unwrap()
            .to_vec();
        let mut bytes = header;
        bytes.push(b'\n');
        for position in positions {
            bytes.extend(encode_record(seq(position), &event(position, "damaged")).unwrap());
            bytes.push(b'\n');
        }
        std::fs::write(&path, &bytes).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(store.recover(&id).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn recognized_v7_log_migrates_to_v8_preserving_counters_and_payloads() {
    let dir = tempdir().unwrap();
    let id = SessionId::new();
    let path = log_path(dir.path(), &id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = encode_header(&Header {
        v: 7,
        session: id,
        created: Timestamp::from_millis(1),
        workspace_root: "/workspace".into(),
    });
    bytes.push(b'\n');
    bytes.extend(encode_record(seq(41), &SessionEntry::Event(event(11, "v7-first"))).unwrap());
    bytes.push(b'\n');
    bytes.extend(
        encode_record(
            seq(1),
            &SessionEntry::RunTransaction(admitted(&id, 1, None, "/workspace")),
        )
        .unwrap(),
    );
    bytes.push(b'\n');
    bytes.extend(encode_record(seq(99), &SessionEntry::Event(event(12, "v7-second"))).unwrap());
    bytes.push(b'\n');
    std::fs::write(&path, &bytes).unwrap();
    let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));

    store.recover(&id).unwrap();
    let migrated = std::fs::read(&path).unwrap();
    let mut lines = migrated.split(|byte| *byte == b'\n');
    let StoreLine::Header(header) = decode_line(lines.next().unwrap()).unwrap() else {
        panic!("migration must retain a header")
    };
    assert_eq!(header.v, CODEC_VERSION);
    assert_eq!(header.v, 8);
    let mut migrated_records = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let StoreLine::Record(record) = decode_line(line).unwrap() else {
            panic!("recognized v7 entries must remain records")
        };
        migrated_records.push((
            record.seq,
            serde_json::from_str::<SessionEntry>(record.payload.get()).unwrap(),
        ));
    }
    assert_eq!(migrated_records.len(), 3);
    assert_eq!(
        migrated_records
            .iter()
            .map(|(position, _)| *position)
            .collect::<Vec<_>>(),
        vec![seq(1), seq(2), seq(3)]
    );
    match &migrated_records[0].1 {
        SessionEntry::Event(actual) => {
            assert_eq!(actual.env.seq, seq(11));
            assert_eq!(
                &actual.kind,
                &EventKind::UserPrompt {
                    text: "v7-first".into(),
                    images: vec![],
                    queued_prompt: None,
                }
            );
        }
        other => panic!("first migrated payload changed shape: {other:?}"),
    }
    assert!(matches!(
        &migrated_records[1].1,
        SessionEntry::RunTransaction(transaction) if transaction.revision() == RunRevision::new(1)
    ));
    match &migrated_records[2].1 {
        SessionEntry::Event(actual) => {
            assert_eq!(actual.env.seq, seq(12));
            assert_eq!(
                &actual.kind,
                &EventKind::UserPrompt {
                    text: "v7-second".into(),
                    images: vec![],
                    queued_prompt: None,
                }
            );
        }
        other => panic!("last migrated payload changed shape: {other:?}"),
    }
}

#[test]
fn v7_log_with_valid_unknown_record_is_refused_without_mutation() {
    let dir = tempdir().unwrap();
    let id = SessionId::new();
    let path = log_path(dir.path(), &id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = encode_header(&Header {
        v: 7,
        session: id,
        created: Timestamp::from_millis(1),
        workspace_root: "/workspace".into(),
    });
    bytes.push(b'\n');
    bytes.extend(encode_record(seq(1), &SessionEntry::Event(event(1, "v7"))).unwrap());
    bytes.push(b'\n');
    bytes.extend_from_slice(br#"{"future":true}"#);
    bytes.push(b'\n');
    std::fs::write(&path, &bytes).unwrap();
    let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    let before_unknown = std::fs::read(&path).unwrap();
    assert!(store.recover(&id).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before_unknown);
}

#[test]
fn real_v6_snapshot_is_rejected_and_full_replay_uses_v7() {
    let dir = tempdir().unwrap();
    let id = SessionId::new();
    let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    let first = event(1, "replay-first");
    let second = event(2, "replay-second");
    store.append(&id, &first).unwrap();
    store.append(&id, &second).unwrap();

    let mut snapshot_state = SessionState::default();
    snapshot_state.apply(&first);
    let mut old_snapshot = Snapshot::new(id, first.env.seq, snapshot_state);
    old_snapshot.v = 6;
    std::fs::write(
        dir.path()
            .join(serde_json::to_string(&id).unwrap().trim_matches('"'))
            .join("snapshot.json"),
        serde_json::to_vec(&old_snapshot).unwrap(),
    )
    .unwrap();

    let mut expected = SessionState::default();
    expected.apply(&first);
    expected.apply(&second);
    let (state, rejection) = store.resume(&id).unwrap();
    assert_eq!(rejection, Some(SnapshotRejection::Version { found: 6 }));
    assert_eq!(state, expected);
    assert_eq!(SNAPSHOT_VERSION, 7);
}

#[test]
fn adoption_wrong_root_second_initial_and_wrong_cas_preserve_log_and_manifest() {
    let dir = tempdir().unwrap();
    let id = SessionId::new();
    let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    store.append(&id, &event(1, "adopt")).unwrap();
    let log = log_path(dir.path(), &id);
    let manifest = manifest_path(dir.path(), &id);
    let before_wrong_root_log = std::fs::read(&log).unwrap();
    let before_wrong_root_manifest = std::fs::read(&manifest).unwrap();
    assert!(
        store
            .compare_and_append_run(&id, None, admitted(&id, 1, None, "/wrong-root"))
            .is_err()
    );
    assert_eq!(std::fs::read(&log).unwrap(), before_wrong_root_log);
    assert_eq!(
        std::fs::read(&manifest).unwrap(),
        before_wrong_root_manifest
    );

    assert!(matches!(
        store.compare_and_append_run(&id, None, admitted(&id, 1, None, "/workspace")),
        Ok(RunAppend::Committed { revision }) if revision == RunRevision::new(1)
    ));
    let before_log = std::fs::read(&log).unwrap();
    let before_manifest = std::fs::read(&manifest).unwrap();
    assert!(matches!(
        store.compare_and_append_run(&id, None, admitted(&id, 1, None, "/workspace")),
        Ok(RunAppend::Conflict { actual }) if actual == RunRevision::new(1)
    ));
    assert_eq!(std::fs::read(&log).unwrap(), before_log);
    assert_eq!(std::fs::read(&manifest).unwrap(), before_manifest);

    assert!(matches!(
        store.compare_and_append_run(
            &id,
            Some(RunRevision::new(99)),
            admitted(&id, 2, Some(99), "/workspace")
        ),
        Ok(RunAppend::Conflict { actual }) if actual == RunRevision::new(1)
    ));
    assert_eq!(std::fs::read(&log).unwrap(), before_log);
    assert_eq!(std::fs::read(&manifest).unwrap(), before_manifest);
}

#[test]
fn event_first_adoption_reports_uncertain_durability_after_appending_valid_record() {
    let dir = tempdir().unwrap();
    let id = SessionId::new();
    let initial = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    initial.append(&id, &event(1, "adopt")).unwrap();

    let failing = JsonlStore::new(
        dir.path(),
        "/workspace",
        Box::new(FailingFsyncIo { inner: OsIo }),
    );
    assert!(matches!(
        failing.compare_and_append_run(&id, None, admitted(&id, 1, None, "/workspace")),
        Ok(RunAppend::CommittedDurabilityUncertain { .. })
    ));

    let bytes = std::fs::read(log_path(dir.path(), &id)).unwrap();
    let last = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .last()
        .unwrap();
    let StoreLine::Record(record) = decode_line(last).unwrap() else {
        panic!("event-first adoption must append a recognised run record")
    };
    assert_eq!(record.seq, seq(2));
    assert!(matches!(
        serde_json::from_str::<SessionEntry>(record.payload.get()).unwrap(),
        SessionEntry::RunTransaction(transaction) if transaction.revision() == RunRevision::new(1)
    ));
}
