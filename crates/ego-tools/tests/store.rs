//! `JsonlStore`: append/read roundtrip, manifest cataloguing and atomicity,
//! the turn-boundary fsync policy, and per-line read failures.
//!
//! See `plans/m0a-contracts.md` Step 4 and `ARCHITECTURE.md` §3.3 / §5.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::store::codec::{CODEC_VERSION, CodecError, StoreLine, decode_line, encode_record};
use ego_core::store::session_store::{SessionStore, StoreError};
use ego_tools::store::{JsonlStore, OsIo, StoreIo};
use futures::StreamExt;
use futures::executor::block_on;
use tempfile::tempdir;

// Wrapped in a module named `store` so `cargo test -p ego-tools store` (the
// story's VERIFY command) matches these tests by substring: cargo's test
// filter matches the full test path, not the source file name, and
// tests/store.rs's bare functions wouldn't otherwise contain "store".
mod store {
    use super::*;

    fn ev_at(seq: Seq, turn: TurnId, kind: EventKind, ts_millis: u64) -> DomainEvent {
        DomainEvent {
            env: Envelope {
                seq,
                id: EventId::new(),
                turn,
                item: None,
                parent: None,
                ts: Timestamp::from_millis(ts_millis),
                producer: Producer::Main,
            },
            kind,
        }
    }

    fn ev(seq: Seq, turn: TurnId, kind: EventKind) -> DomainEvent {
        ev_at(seq, turn, kind, 1_000)
    }

    /// The on-disk directory a `JsonlStore` puts a session's files under
    /// (`ARCHITECTURE.md` §5: `<root>/<session-id>/`). `SessionId`'s field
    /// is private with no accessor, so — exactly like `JsonlStore` itself —
    /// this reads the id out through its derived `Serialize` impl, which
    /// (being a single-field tuple struct) serialises transparently as the
    /// inner `Uuid`'s plain string form.
    fn session_dir_name(id: &SessionId) -> String {
        match serde_json::to_value(id).expect("SessionId always serialises") {
            serde_json::Value::String(s) => s,
            other => panic!("SessionId did not serialise as a JSON string: {other:?}"),
        }
    }

    fn log_path(root: &std::path::Path, id: &SessionId) -> PathBuf {
        root.join(session_dir_name(id)).join("events.jsonl")
    }

    fn manifest_path(root: &std::path::Path, id: &SessionId) -> PathBuf {
        root.join(session_dir_name(id)).join("manifest.json")
    }

    /// A [`StoreIo`] that counts `fsync` calls, wrapping the real
    /// filesystem for everything else — the fsync-on-turn-boundary
    /// criterion needs a way to observe when durability was actually
    /// requested without faking the whole filesystem.
    struct CountingIo {
        inner: OsIo,
        fsyncs: Arc<AtomicUsize>,
    }

    impl StoreIo for CountingIo {
        fn append_line(&self, path: &std::path::Path, line: &[u8]) -> std::io::Result<()> {
            self.inner.append_line(path, line)
        }

        fn fsync(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.fsyncs.fetch_add(1, Ordering::SeqCst);
            self.inner.fsync(path)
        }

        fn read(&self, path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
            self.inner.read(path)
        }

        fn exists(&self, path: &std::path::Path) -> std::io::Result<bool> {
            self.inner.exists(path)
        }

        fn replace_atomic(&self, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
            self.inner.replace_atomic(path, bytes)
        }

        fn truncate(&self, path: &std::path::Path, len: u64) -> std::io::Result<()> {
            self.inner.truncate(path, len)
        }

        fn remove_file(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.remove_file(path)
        }

        fn list_dirs(&self, path: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
            self.inner.list_dirs(path)
        }
    }

    /// A [`StoreIo`] that counts `read` calls per path, wrapping the real
    /// filesystem for everything else — used to prove `JsonlStore::append`
    /// does not re-read the whole log on every call.
    struct ReadCountingIo {
        inner: OsIo,
        reads: Arc<Mutex<HashMap<PathBuf, usize>>>,
    }

    impl StoreIo for ReadCountingIo {
        fn append_line(&self, path: &std::path::Path, line: &[u8]) -> std::io::Result<()> {
            self.inner.append_line(path, line)
        }

        fn fsync(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.fsync(path)
        }

        fn read(&self, path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
            *self
                .reads
                .lock()
                .unwrap()
                .entry(path.to_path_buf())
                .or_insert(0) += 1;
            self.inner.read(path)
        }

        fn exists(&self, path: &std::path::Path) -> std::io::Result<bool> {
            self.inner.exists(path)
        }

        fn replace_atomic(&self, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
            self.inner.replace_atomic(path, bytes)
        }

        fn truncate(&self, path: &std::path::Path, len: u64) -> std::io::Result<()> {
            self.inner.truncate(path, len)
        }

        fn remove_file(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.remove_file(path)
        }

        fn list_dirs(&self, path: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
            self.inner.list_dirs(path)
        }
    }

    /// A [`StoreIo`] that fails `read` for one specific path (simulating,
    /// e.g., a permissions error), wrapping the real filesystem for
    /// everything else.
    struct FailingReadIo {
        inner: OsIo,
        fail_path: PathBuf,
    }

    impl StoreIo for FailingReadIo {
        fn append_line(&self, path: &std::path::Path, line: &[u8]) -> std::io::Result<()> {
            self.inner.append_line(path, line)
        }

        fn fsync(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.fsync(path)
        }

        fn read(&self, path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
            if path == self.fail_path {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "simulated permission error",
                ))
            } else {
                self.inner.read(path)
            }
        }

        fn exists(&self, path: &std::path::Path) -> std::io::Result<bool> {
            self.inner.exists(path)
        }

        fn replace_atomic(&self, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
            self.inner.replace_atomic(path, bytes)
        }

        fn truncate(&self, path: &std::path::Path, len: u64) -> std::io::Result<()> {
            self.inner.truncate(path, len)
        }

        fn remove_file(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.remove_file(path)
        }

        fn list_dirs(&self, path: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
            self.inner.list_dirs(path)
        }
    }

    // -- StoreIo (OsIo) --------------------------------------------------

    #[test]
    fn store_os_io_append_line_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("events.jsonl");
        let io = OsIo;
        io.append_line(&path, b"{\"a\":1}").unwrap();
        assert_eq!(io.read(&path).unwrap().unwrap(), b"{\"a\":1}\n");
    }

    #[test]
    fn store_os_io_replace_atomic_leaves_no_tmp_file_and_final_content_wins() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let io = OsIo;
        io.replace_atomic(&path, b"{\"v\":1}").unwrap();
        io.replace_atomic(&path, b"{\"v\":2}").unwrap();
        assert_eq!(io.read(&path).unwrap().unwrap(), b"{\"v\":2}");
        let tmp_path = path.with_extension("tmp");
        assert!(
            !tmp_path.exists(),
            "tmp file must not remain after atomic replace"
        );
    }

    /// Distinguishes rename-based atomic replace from a plain in-place
    /// write, which the previous test does not: a naive `fs::write(path,
    /// bytes)` follows a symlink at `path` and clobbers whatever it points
    /// to, while `fs::rename` replaces the entry at `path` itself. Verified
    /// by temporarily reverting `replace_atomic` to a plain write while
    /// developing this test: it fails there (`real_target` gets
    /// overwritten, `link_path` stops being a symlink) and passes against
    /// the real tmp+rename implementation.
    #[test]
    #[cfg(unix)]
    fn store_os_io_replace_atomic_replaces_the_path_itself_not_a_symlinks_target() {
        let dir = tempdir().unwrap();
        let real_target = dir.path().join("real-target.json");
        std::fs::write(&real_target, b"do not touch").unwrap();
        let link_path = dir.path().join("manifest.json");
        std::os::unix::fs::symlink(&real_target, &link_path).unwrap();

        let io = OsIo;
        io.replace_atomic(&link_path, b"{\"v\":1}").unwrap();

        assert!(
            !link_path.is_symlink(),
            "replace_atomic must replace the path itself, not write through a symlink"
        );
        assert_eq!(
            std::fs::read(&real_target).unwrap(),
            b"do not touch",
            "the symlink's old target must be untouched"
        );
        assert_eq!(std::fs::read(&link_path).unwrap(), b"{\"v\":1}");
    }

    #[test]
    fn store_os_io_read_missing_file_is_none() {
        let dir = tempdir().unwrap();
        let io = OsIo;
        assert_eq!(io.read(&dir.path().join("nope.json")).unwrap(), None);
    }

    #[test]
    fn store_os_io_exists_reflects_presence_without_reading_content() {
        let dir = tempdir().unwrap();
        let io = OsIo;
        let path = dir.path().join("events.jsonl");
        assert!(!io.exists(&path).unwrap());
        io.append_line(&path, b"{}").unwrap();
        assert!(io.exists(&path).unwrap());
    }

    // -- append / read roundtrip ------------------------------------------

    #[test]
    fn store_append_then_read_roundtrips_events_in_order() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();

        let e1 = ev(
            seq.next(),
            turn,
            EventKind::UserPrompt {
                text: "hi".to_string(),
            },
        );
        let e2 = ev(
            seq.next(),
            turn,
            EventKind::AssistantMessage {
                text: "hello".to_string(),
            },
        );
        store.append(&id, &e1).unwrap();
        store.append(&id, &e2).unwrap();

        let events: Vec<DomainEvent> = block_on(store.read(&id).unwrap().collect::<Vec<_>>())
            .into_iter()
            .map(|r| r.expect("every appended event should decode cleanly"))
            .collect();
        assert_eq!(events, vec![e1, e2]);
    }

    #[test]
    fn store_append_writes_header_before_first_record() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();

        let e1 = ev_at(
            seq.next(),
            turn,
            EventKind::UserPrompt {
                text: "hi".to_string(),
            },
            5_000,
        );
        store.append(&id, &e1).unwrap();

        let bytes = std::fs::read(log_path(dir.path(), &id)).unwrap();
        let header_line = bytes.split(|&b| b == b'\n').next().unwrap();
        match decode_line(header_line).unwrap() {
            StoreLine::Header(h) => {
                assert_eq!(h.v, CODEC_VERSION);
                assert_eq!(h.session, id);
                assert_eq!(h.created, Timestamp::from_millis(5_000));
            }
            other => panic!("expected the first line to be a header, got {other:?}"),
        }
    }

    #[test]
    fn store_append_never_rewrites_the_header_on_later_events() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();

        for text in ["a", "b", "c"] {
            store
                .append(
                    &id,
                    &ev(
                        seq.next(),
                        turn,
                        EventKind::UserPrompt {
                            text: text.to_string(),
                        },
                    ),
                )
                .unwrap();
        }

        let bytes = std::fs::read(log_path(dir.path(), &id)).unwrap();
        let header_count = bytes
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .filter(|line| matches!(decode_line(line), Ok(StoreLine::Header(_))))
            .count();
        assert_eq!(header_count, 1, "the header must be written exactly once");
    }

    #[test]
    fn store_read_missing_session_returns_not_found() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();
        match store.read(&id) {
            Err(StoreError::NotFound) => {}
            Err(other) => panic!("expected StoreError::NotFound, got {other}"),
            Ok(_) => panic!("expected StoreError::NotFound, got Ok"),
        }
    }

    #[test]
    fn store_read_rejects_a_log_whose_first_line_is_not_a_header() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();

        let path = log_path(dir.path(), &id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut seq = Seq::default();
        let record_line = encode_record(seq.next(), &"not a header").unwrap();
        std::fs::write(&path, [record_line, b"\n".to_vec()].concat()).unwrap();

        match store.read(&id) {
            Err(StoreError::MissingHeader) => {}
            Err(other) => panic!("expected StoreError::MissingHeader, got {other}"),
            Ok(_) => panic!("expected StoreError::MissingHeader, got Ok"),
        }
    }

    #[test]
    fn store_read_surfaces_a_corrupt_line_without_hiding_the_rest_of_the_stream() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();

        for marker in [
            "first-event-marker",
            "second-event-marker",
            "third-event-marker",
        ] {
            store
                .append(
                    &id,
                    &ev(
                        seq.next(),
                        turn,
                        EventKind::UserPrompt {
                            text: marker.to_string(),
                        },
                    ),
                )
                .unwrap();
        }

        let path = log_path(dir.path(), &id);
        let text = std::fs::read_to_string(&path).unwrap();
        let corrupted = text.replacen("second-event-marker", "SECOND-EVENT-MARKER", 1);
        assert_ne!(text, corrupted, "fixture must actually corrupt a byte");
        std::fs::write(&path, corrupted).unwrap();

        let results: Vec<Result<DomainEvent, StoreError>> =
            block_on(store.read(&id).unwrap().collect());
        assert_eq!(
            results.len(),
            3,
            "a corrupt line must not swallow its siblings"
        );

        match &results[0] {
            Ok(e) => assert_eq!(
                e.kind,
                EventKind::UserPrompt {
                    text: "first-event-marker".to_string()
                }
            ),
            other => panic!("expected the first line to decode cleanly, got {other:?}"),
        }
        assert!(
            matches!(
                &results[1],
                Err(StoreError::Codec(CodecError::ChecksumMismatch { .. }))
            ),
            "expected a checksum mismatch on the corrupted line, got {:?}",
            results[1]
        );
        match &results[2] {
            Ok(e) => assert_eq!(
                e.kind,
                EventKind::UserPrompt {
                    text: "third-event-marker".to_string()
                }
            ),
            other => {
                panic!("expected the line after the corrupt one to decode cleanly, got {other:?}")
            }
        }
    }

    // -- fsync policy ------------------------------------------------------

    #[test]
    fn store_append_fsyncs_only_on_turn_completed() {
        let dir = tempdir().unwrap();
        let fsyncs = Arc::new(AtomicUsize::new(0));
        let io = CountingIo {
            inner: OsIo,
            fsyncs: fsyncs.clone(),
        };
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(io));
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();

        store
            .append(
                &id,
                &ev(
                    seq.next(),
                    turn,
                    EventKind::UserPrompt {
                        text: "hi".to_string(),
                    },
                ),
            )
            .unwrap();
        store
            .append(
                &id,
                &ev(
                    seq.next(),
                    turn,
                    EventKind::AssistantDelta {
                        text: "partial".to_string(),
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            fsyncs.load(Ordering::SeqCst),
            0,
            "no fsync before a turn boundary"
        );

        store
            .append(
                &id,
                &ev(
                    seq.next(),
                    turn,
                    EventKind::TurnCompleted {
                        ok: true,
                        exit_reason: ExitReason::Completed,
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            fsyncs.load(Ordering::SeqCst),
            1,
            "fsync exactly once, at the turn boundary"
        );
    }

    // -- append cost ---------------------------------------------------------

    /// `append` must not re-read the whole log on every call just to decide
    /// whether a header is needed — that makes a single append O(n) in log
    /// size and a session O(n²) overall. Verified while developing this
    /// fix: against the pre-fix implementation (`self.io.read(&log_path)?
    /// .is_none()`), this test failed with `log_reads = 20` (one whole-log
    /// read per appended event); against the `StoreIo::exists`-based fix it
    /// asserts zero.
    #[test]
    fn store_append_does_not_read_the_whole_log_per_event() {
        let dir = tempdir().unwrap();
        let reads = Arc::new(Mutex::new(HashMap::new()));
        let io = ReadCountingIo {
            inner: OsIo,
            reads: reads.clone(),
        };
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(io));
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();

        let event_count = 20;
        for i in 0..event_count {
            store
                .append(
                    &id,
                    &ev(
                        seq.next(),
                        turn,
                        EventKind::AssistantDelta {
                            text: format!("chunk-{i}"),
                        },
                    ),
                )
                .unwrap();
        }

        let log_path = log_path(dir.path(), &id);
        let log_reads = *reads.lock().unwrap().get(&log_path).unwrap_or(&0);
        assert_eq!(
            log_reads, 0,
            "a nonzero count here means something in JsonlStore::append is calling \
             StoreIo::read on the log path again (e.g. to check whether the session \
             already exists) instead of StoreIo::exists — that re-introduces the O(n) \
             whole-log read per event this test exists to catch; saw {log_reads} reads \
             across {event_count} appends"
        );
    }

    // -- manifest / list -----------------------------------------------------

    #[test]
    fn store_list_on_empty_root_returns_no_sessions() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("sessions"); // deliberately does not exist yet
        let store = JsonlStore::new(&root, "/ws", Box::new(OsIo));
        assert_eq!(store.list().unwrap(), Vec::new());
    }

    #[test]
    fn store_list_reports_meta_written_at_creation_and_turn_boundaries_only() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();

        store
            .append(
                &id,
                &ev_at(
                    seq.next(),
                    turn,
                    EventKind::UserPrompt {
                        text: "hi".to_string(),
                    },
                    1_000,
                ),
            )
            .unwrap();
        let after_creation = store.list().unwrap();
        assert_eq!(after_creation.len(), 1);
        assert_eq!(after_creation[0].id, id);
        assert_eq!(after_creation[0].created, Timestamp::from_millis(1_000));
        assert_eq!(after_creation[0].updated, Timestamp::from_millis(1_000));
        // The one the store was built for, stamped into the header at
        // creation. `model` and `title` are still `None` because nothing in
        // `append` can learn them; the workspace stopped being in that
        // category when the header started carrying it.
        assert_eq!(
            after_creation[0].workspace_root,
            std::path::PathBuf::from("/ws")
        );
        assert_eq!(after_creation[0].model, None);
        assert_eq!(after_creation[0].title, None);

        store
            .append(
                &id,
                &ev_at(
                    seq.next(),
                    turn,
                    EventKind::AssistantDelta {
                        text: "partial".to_string(),
                    },
                    2_000,
                ),
            )
            .unwrap();
        let after_delta = store.list().unwrap();
        assert_eq!(
            after_delta[0].updated,
            Timestamp::from_millis(1_000),
            "a mid-turn delta must not rewrite the manifest"
        );

        store
            .append(
                &id,
                &ev_at(
                    seq.next(),
                    turn,
                    EventKind::TurnCompleted {
                        ok: true,
                        exit_reason: ExitReason::Completed,
                    },
                    3_000,
                ),
            )
            .unwrap();
        let after_turn = store.list().unwrap();
        assert_eq!(after_turn[0].created, Timestamp::from_millis(1_000));
        assert_eq!(
            after_turn[0].updated,
            Timestamp::from_millis(3_000),
            "the turn boundary must rewrite the manifest"
        );
    }

    #[test]
    fn store_list_skips_a_session_with_an_unreadable_manifest_rather_than_failing() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let turn = TurnId::new();
        let mut seq = Seq::default();

        let good_id = SessionId::new();
        store
            .append(
                &good_id,
                &ev(
                    seq.next(),
                    turn,
                    EventKind::UserPrompt {
                        text: "hi".to_string(),
                    },
                ),
            )
            .unwrap();

        let bad_id = SessionId::new();
        let bad_manifest = manifest_path(dir.path(), &bad_id);
        std::fs::create_dir_all(bad_manifest.parent().unwrap()).unwrap();
        std::fs::write(&bad_manifest, b"not json").unwrap();

        let metas = store.list().unwrap();
        assert_eq!(
            metas.len(),
            1,
            "the whole catalogue must not fail on one bad manifest"
        );
        assert_eq!(metas[0].id, good_id);
    }

    /// The parse-error case above and this I/O-error case must be handled
    /// the same way: both are per-session failures reading one manifest,
    /// so both are skipped-and-warned rather than one failing the whole
    /// `list()` call and the other not.
    #[test]
    fn store_list_skips_a_session_whose_manifest_read_fails_rather_than_failing_the_whole_call() {
        let dir = tempdir().unwrap();
        let good_id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();
        {
            let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
            store
                .append(
                    &good_id,
                    &ev(
                        seq.next(),
                        turn,
                        EventKind::UserPrompt {
                            text: "hi".to_string(),
                        },
                    ),
                )
                .unwrap();
        }

        let bad_id = SessionId::new();
        let bad_manifest = manifest_path(dir.path(), &bad_id);
        std::fs::create_dir_all(bad_manifest.parent().unwrap()).unwrap();
        std::fs::write(&bad_manifest, b"{}").unwrap(); // content is irrelevant; read() itself fails

        let io = FailingReadIo {
            inner: OsIo,
            fail_path: bad_manifest,
        };
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(io));

        let metas = store.list().unwrap();
        assert_eq!(
            metas.len(),
            1,
            "an I/O error reading one manifest must not fail the whole catalogue"
        );
        assert_eq!(metas[0].id, good_id);
    }
}
