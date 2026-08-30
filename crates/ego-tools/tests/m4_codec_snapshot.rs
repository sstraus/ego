//! RED contract for M4 S1's one-way codec and snapshot migration.

use ego_core::event::id::{Seq, SessionId};
use ego_core::state::SessionState;
use ego_core::store::codec::{decode_line, StoreLine};
use ego_tools::store::{
    JsonlStore, LeaseInputs, OsIo, Snapshot, SnapshotRejection, SNAPSHOT_VERSION,
};
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

const ALWAYS_ALIVE: &dyn Fn(u32) -> bool = &|_| true;
const FIXTURE_SESSION: &str = "00000000-0000-0000-0000-000000000001";

fn lease_inputs() -> LeaseInputs<'static> {
    LeaseInputs {
        pid: 73073,
        now: ego_core::event::time::Timestamp::from_millis(9_000),
        stale_after: Duration::from_secs(30),
        is_alive: ALWAYS_ALIVE,
    }
}

fn session_dir(root: &Path, id: &SessionId) -> std::path::PathBuf {
    root.join(id.to_string())
}

fn write_fixture(root: &Path, id: &SessionId, bytes: &[u8]) -> std::path::PathBuf {
    let directory = session_dir(root, id);
    std::fs::create_dir_all(&directory).expect("fixture session directory");
    let path = directory.join("events.jsonl");
    std::fs::write(&path, bytes).expect("codec-8 fixture");
    path
}

mod regression_case {
    use super::*;

    #[test]
    fn current_writers_are_codec_and_snapshot_nine() {
        assert_eq!(ego_core::store::codec::CODEC_VERSION, 9);
        assert_eq!(SNAPSHOT_VERSION, 9);
    }

    #[test]
    fn owned_open_writer_migrates_real_codec_eight_then_reopens_cleanly() {
        let dir = tempdir().unwrap();
        let id: SessionId = FIXTURE_SESSION.parse().unwrap();
        let path = write_fixture(
            dir.path(),
            &id,
            include_bytes!("../../ego-core/tests/golden/store_log.jsonl"),
        );
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let opened = store
            .open_writer(&id, lease_inputs())
            .expect("owned open path migrates codec 8");
        assert!(opened.repair.migrated);
        assert_eq!(opened.repair.truncated_bytes, 0);
        drop(opened);

        let migrated = std::fs::read(&path).unwrap();
        let mut lines = migrated
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty());
        let StoreLine::Header(header) = decode_line(lines.next().unwrap()).unwrap() else {
            panic!("migration must retain one header")
        };
        assert_eq!(header.v, 9);
        let records = lines
            .map(|line| decode_line(line).expect("migrated record"))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 8);
        for (position, record) in records.iter().enumerate() {
            let StoreLine::Record(record) = record else {
                panic!("recognized codec-8 record was not retained")
            };
            assert_eq!(
                record.seq,
                serde_json::from_value::<Seq>(serde_json::json!(position + 1)).unwrap()
            );
        }

        let reopen_store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let reopened = reopen_store
            .open_writer(&id, lease_inputs())
            .expect("migrated log reopens through the owned path");
        assert!(reopened.repair.is_clean());
    }

    #[test]
    fn snapshot_eight_is_a_cache_miss_not_a_current_snapshot() {
        let session = SessionId::new();
        let old = Snapshot {
            v: 8,
            session,
            source_seq: Seq::ZERO,
            state: SessionState::default(),
        };
        assert_eq!(
            ego_tools::store::validate(&old, &session, Seq::ZERO, Seq::ZERO),
            Err(SnapshotRejection::Version { found: 8 })
        );
    }

    #[test]
    fn unknown_codec_eight_records_fail_migration_closed_without_mutating_bytes() {
        let dir = tempdir().unwrap();
        let id: SessionId = FIXTURE_SESSION.parse().unwrap();
        let unknown_line = br#"{"future_m4_record":true}"#;
        let StoreLine::Unknown(raw) = decode_line(unknown_line).unwrap() else {
            panic!("future JSON record must remain opaque to this codec")
        };
        assert_eq!(raw.as_bytes(), unknown_line);
        let mut fixture = include_bytes!("../../ego-core/tests/golden/store_log.jsonl").to_vec();
        fixture.extend_from_slice(unknown_line);
        fixture.push(b'\n');
        let path = write_fixture(dir.path(), &id, &fixture);
        let before = std::fs::read(&path).unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        assert!(store.open_writer(&id, lease_inputs()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let mut header = serde_json::from_slice::<serde_json::Value>(
            fixture.split(|byte| *byte == b'\n').next().unwrap(),
        )
        .unwrap();
        header["v"] = serde_json::json!(9);
        let mut future_image = serde_json::to_vec(&header).unwrap();
        future_image.push(b'\n');
        future_image.extend_from_slice(fixture.splitn(2, |byte| *byte == b'\n').nth(1).unwrap());
        let future_path = write_fixture(dir.path(), &id, &future_image);
        let before_future = std::fs::read(&future_path).unwrap();
        let future_store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        assert!(future_store.open_writer(&id, lease_inputs()).is_err());
        assert_eq!(std::fs::read(&future_path).unwrap(), before_future);
    }
}
