//! `JsonlStore`: append/read roundtrip, manifest cataloguing and atomicity,
//! the turn-boundary fsync policy, and per-line read failures.
//!
//! See `plans/m0a-contracts.md` Step 4 and `ARCHITECTURE.md` §3.3 / §5.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::Duration;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, Producer, QueuedPromptId, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{
    ConfigProvenance, EvidenceAdmissionService, RunContextEvidence, RunInvocationId,
};
use ego_core::run::{RunAppend, RunId, RunRevision, RunTransaction};
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_core::store::codec::{CODEC_VERSION, CodecError, StoreLine, decode_line, encode_record};
use ego_core::store::session_store::{SessionEntry, SessionStore, StoreError};
use ego_tools::store::{AtomicReplacePublication, JsonlStore, LeaseInputs, OsIo, StoreIo};
use futures::StreamExt;
use futures::executor::block_on;
use tempfile::tempdir;

// Wrapped in a module named `store` so `cargo test -p ego-tools store` (the
// story's VERIFY command) matches these tests by substring: cargo's test
// filter matches the full test path, not the source file name, and
// tests/store.rs's bare functions wouldn't otherwise contain "store".
mod store {
    use super::*;

    #[test]
    fn notice_delivery_survives_reopen_without_becoming_a_conversation_turn() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let session = SessionId::new();
        let notice = ego_core::store::session_store::NoticeDelivery {
            card_id: "orchestration-result".to_owned(),
            source_path: Some("/workspace/results/worker.md".into()),
            delivered_at: Timestamp::from_millis(61001),
        };
        assert!(matches!(
            store.record_notice(&session, notice.clone()),
            Err(StoreError::NotFound)
        ));
        store
            .append(
                &session,
                &ev(
                    Seq::ZERO,
                    TurnId::new(),
                    EventKind::TurnCompleted {
                        ok: true,
                        exit_reason: ExitReason::Completed,
                    },
                ),
            )
            .unwrap();
        store.record_notice(&session, notice.clone()).unwrap();
        drop(store);
        let reopened = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let entries = block_on(reopened.read(&session).unwrap().collect::<Vec<_>>());
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1].as_ref().unwrap(),
            &SessionEntry::NoticeDelivered(notice)
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry, Ok(SessionEntry::Event(_))))
                .count(),
            1
        );
    }

    const ALIVE: &dyn Fn(u32) -> bool = &|_| true;

    fn evidence_lease() -> LeaseInputs<'static> {
        LeaseInputs {
            pid: 66066,
            now: Timestamp::from_millis(6_606_600),
            stale_after: Duration::from_secs(30),
            is_alive: ALIVE,
        }
    }

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

    fn run_lock_path(root: &std::path::Path, id: &SessionId) -> PathBuf {
        root.join(session_dir_name(id)).join("events.lock")
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

    #[derive(Clone, Copy)]
    enum GateSide {
        A,
        B,
    }

    #[derive(Default)]
    struct GateState {
        a_read_authoritatively: bool,
        release_a: bool,
        b_read_while_a_held: bool,
    }

    /// A real-filesystem adapter that pauses A after its authoritative log
    /// read. The pause is inside the StoreIo call, so a production store that
    /// short-locks the session log holds `<session>/events.lock` throughout
    /// the gate. B uses an independent store and adapter instance.
    struct GateIo {
        inner: OsIo,
        target: PathBuf,
        side: GateSide,
        state: Arc<(Mutex<GateState>, Condvar)>,
        b_read: Option<SyncSender<()>>,
    }

    impl GateIo {
        fn wait_for_a_read(state: &Arc<(Mutex<GateState>, Condvar)>) {
            let (lock, changed) = (&state.0, &state.1);
            let mut state = lock.lock().expect("CAS gate state");
            while !state.a_read_authoritatively {
                state = changed.wait(state).expect("CAS gate wait");
            }
        }

        fn release_a(state: &Arc<(Mutex<GateState>, Condvar)>) {
            let (lock, changed) = (&state.0, &state.1);
            let mut state = lock.lock().expect("CAS gate state");
            state.release_a = true;
            changed.notify_all();
        }
    }

    impl StoreIo for GateIo {
        fn append_line(&self, path: &std::path::Path, line: &[u8]) -> std::io::Result<()> {
            self.inner.append_line(path, line)
        }

        fn fsync(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.fsync(path)
        }

        fn read(&self, path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
            let bytes = self.inner.read(path)?;
            if path != self.target {
                return Ok(bytes);
            }
            let (lock, changed) = (&self.state.0, &self.state.1);
            let mut state = lock.lock().expect("CAS gate state");
            match self.side {
                GateSide::A => {
                    state.a_read_authoritatively = true;
                    changed.notify_all();
                    while !state.release_a {
                        state = changed.wait(state).expect("CAS gate wait");
                    }
                }
                GateSide::B => {
                    if !state.release_a {
                        state.b_read_while_a_held = true;
                    }
                    changed.notify_all();
                    if let Some(signal) = &self.b_read {
                        signal.send(()).expect("CAS B read observer");
                    }
                }
            }
            Ok(bytes)
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

    fn admitted_run_transaction(
        session: &SessionId,
        revision: u64,
        expected_revision: Option<u64>,
        start: bool,
    ) -> RunTransaction {
        let session = serde_json::to_value(session).expect("session id serialises");
        serde_json::from_value(serde_json::json!({
            "revision": revision,
            "run_id": {
                "session_id": session,
                "nonce": "00000000-0000-7000-8000-000000000054"
            },
            "origin": {"kind": "top_level"},
            "root": "/workspace",
            "selections": {
                "model": "model/test",
                "thinking": null,
                "mode": null,
                "budget": null,
                "profile": null
            },
            "command": {
                "kind": "command_admitted",
                "request_id": "00000000-0000-7000-8000-000000000055",
                "digest": "store-race",
                "expected_revision": expected_revision,
                "lease": "00000000-0000-7000-8000-000000000056",
                "executor_id": "00000000-0000-7000-8000-000000000057",
                "prompt": "store race",
                "model": "model/test",
                "start": start
            },
            "status": "running",
            "observed_at": 0,
            "pruned_receipts": []
        }))
        .expect("admitted run transaction fixture deserialises")
    }

    fn run_lock_is_held(path: &std::path::Path) -> Result<(), String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("run CAS lock could not be opened: {error}"))?;
        let mut lock = fd_lock::RwLock::new(file);
        match lock.try_write() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Ok(_) => Err("a second writer acquired the held run lock".to_owned()),
            Err(error) => Err(format!("run CAS lock probe failed: {error}")),
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

        fn read_first_line(&self, path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
            self.inner.read_first_line(path)
        }

        fn read_last_line(&self, path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
            self.inner.read_last_line(path)
        }

        fn exists(&self, path: &std::path::Path) -> std::io::Result<bool> {
            self.inner.exists(path)
        }

        fn replace_atomic(&self, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
            self.inner.replace_atomic(path, bytes)
        }

        fn replace_atomic_publication(
            &self,
            path: &std::path::Path,
            bytes: &[u8],
        ) -> std::io::Result<AtomicReplacePublication> {
            self.inner.replace_atomic_publication(path, bytes)
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

    /// A store I/O adapter that writes a record and then fails its requested
    /// durability barrier.  The queue contract treats that as rejection: the
    /// caller must not clear the composer, request interruption, or begin a
    /// provider request after this error.
    struct FailingFsyncIo {
        inner: CountingIo,
        fail_on: usize,
    }

    impl StoreIo for FailingFsyncIo {
        fn append_line(&self, path: &std::path::Path, line: &[u8]) -> std::io::Result<()> {
            self.inner.append_line(path, line)
        }

        fn fsync(&self, path: &std::path::Path) -> std::io::Result<()> {
            let call = self.inner.fsyncs.fetch_add(1, Ordering::SeqCst) + 1;
            if call == self.fail_on {
                Err(std::io::Error::other("simulated durability failure"))
            } else {
                self.inner.inner.fsync(path)
            }
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
    fn independent_run_cas_holds_a_session_lock_and_preserves_one_valid_stream() {
        let dir = tempdir().unwrap();
        let session = SessionId::new();
        let initial = admitted_run_transaction(&session, 1, None, true);
        let initial_store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        assert!(matches!(
            initial_store.compare_and_append_run(&session, None, initial.clone()),
            Ok(RunAppend::Committed { revision }) if revision == RunRevision::new(1)
        ));

        let collision = initial_store
            .compare_and_append_run(&session, None, initial)
            .expect("an existing revision-one session must report a collision");
        assert_eq!(
            collision,
            RunAppend::Conflict {
                actual: RunRevision::new(1)
            }
        );

        let target = log_path(dir.path(), &session);
        let state = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
        let (b_read_signal, b_read_observer) = mpsc::sync_channel(1);
        let left = Arc::new(JsonlStore::new(
            dir.path(),
            "/workspace",
            Box::new(GateIo {
                inner: OsIo,
                target: target.clone(),
                side: GateSide::A,
                state: Arc::clone(&state),
                b_read: None,
            }),
        ));
        let right = Arc::new(JsonlStore::new(
            dir.path(),
            "/workspace",
            Box::new(GateIo {
                inner: OsIo,
                target,
                side: GateSide::B,
                state: Arc::clone(&state),
                b_read: Some(b_read_signal),
            }),
        ));
        let candidate = admitted_run_transaction(&session, 2, Some(1), false);
        let left_candidate = candidate.clone();
        let right_candidate = candidate;
        let b_start = Arc::new(Barrier::new(2));

        let (left_result, right_result) = std::thread::scope(|scope| {
            let left_worker = scope.spawn({
                let left = Arc::clone(&left);
                let state = Arc::clone(&state);
                move || {
                    let result = left.compare_and_append_run(
                        &session,
                        Some(RunRevision::new(1)),
                        left_candidate,
                    );
                    GateIo::release_a(&state);
                    result
                }
            });
            GateIo::wait_for_a_read(&state);
            let lock_probe = run_lock_is_held(&run_lock_path(dir.path(), &session));

            let right_worker = scope.spawn({
                let right = Arc::clone(&right);
                let b_start = Arc::clone(&b_start);
                move || {
                    b_start.wait();
                    right.compare_and_append_run(
                        &session,
                        Some(RunRevision::new(1)),
                        right_candidate,
                    )
                }
            });
            b_start.wait();
            // This bounded receive is the deterministic scheduling point. On
            // An implementation that enters its authoritative read while A
            // is paused must keep B outside that read once production owns
            // events.lock around it. Either way A is released below before
            // joining, so the fixture cannot deadlock.
            let b_read_before_release = b_read_observer
                .recv_timeout(Duration::from_millis(250))
                .is_ok();
            GateIo::release_a(&state);
            let results = (
                left_worker.join().expect("left run CAS worker"),
                right_worker.join().expect("right run CAS worker"),
            );
            assert!(
                lock_probe.is_ok(),
                "A must hold the per-session events.lock during its authoritative read: {lock_probe:?}"
            );
            assert_eq!(
                state.0.lock().expect("CAS gate state").b_read_while_a_held,
                b_read_before_release,
                "the bounded B-read observation must match the gate state"
            );
            results
        });

        let results = [left_result.unwrap(), right_result.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|result| {
                    matches!(
                        result,
                        &&RunAppend::Committed { revision }
                            if revision == RunRevision::new(2)
                    )
                })
                .count(),
            1,
            "exactly one store must commit revision two: {results:?}"
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| {
                    matches!(
                        result,
                        &&RunAppend::Conflict { actual } if actual == RunRevision::new(2)
                    )
                })
                .count(),
            1,
            "exactly one store must observe the committed revision: {results:?}"
        );
        assert!(
            results.contains(&RunAppend::Committed {
                revision: RunRevision::new(2)
            }),
            "exactly one store must commit revision two: {results:?}"
        );
        assert!(
            results.contains(&RunAppend::Conflict {
                actual: RunRevision::new(2)
            }),
            "the other store must observe the committed revision: {results:?}"
        );
        let final_store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let entries = block_on(
            final_store
                .read(&session)
                .expect("valid final stream")
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            entries.len(),
            2,
            "one initial and one winning append remain"
        );
        assert!(entries.iter().all(|entry| entry.is_ok()));
        let metadata = final_store.list().expect("valid final manifest");
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].id, session);
        assert_eq!(metadata[0].workspace_root, PathBuf::from("/workspace"));
    }

    #[test]
    fn initial_run_adopts_a_matching_header_only_durable_session() {
        let dir = tempdir().unwrap();
        let session = SessionId::new();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        store
            .publish_empty_session(
                session,
                PathBuf::from("/workspace").as_path(),
                Timestamp::EPOCH,
            )
            .expect("header-only session");
        let initial = admitted_run_transaction(&session, 1, None, true);

        assert!(matches!(
            store.compare_and_append_run(&session, None, initial),
            Ok(RunAppend::Committed { revision }) if revision == RunRevision::new(1)
        ));
    }

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
                images: Vec::new(),
                queued_prompt: None,
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
            .map(
                |result| match result.expect("every appended event should decode cleanly") {
                    SessionEntry::Event(event) => event,
                    SessionEntry::RunTransaction(_) => {
                        panic!("event-only session contained a run transaction")
                    }
                    SessionEntry::Evidence(_) | SessionEntry::NoticeDelivered(_) => {
                        panic!("event-only session contained evidence")
                    }
                },
            )
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
                images: Vec::new(),
                queued_prompt: None,
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
                            images: Vec::new(),
                            queued_prompt: None,
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
                            images: Vec::new(),
                            queued_prompt: None,
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

        let results: Vec<Result<SessionEntry, StoreError>> =
            block_on(store.read(&id).unwrap().collect());
        assert_eq!(
            results.len(),
            3,
            "a corrupt line must not swallow its siblings"
        );

        match &results[0] {
            Ok(SessionEntry::Event(e)) => assert_eq!(
                e.kind,
                EventKind::UserPrompt {
                    text: "first-event-marker".to_string(),
                    images: Vec::new(),
                    queued_prompt: None,
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
            Ok(SessionEntry::Event(e)) => assert_eq!(
                e.kind,
                EventKind::UserPrompt {
                    text: "third-event-marker".to_string(),
                    images: Vec::new(),
                    queued_prompt: None,
                }
            ),
            other => {
                panic!("expected the line after the corrupt one to decode cleanly, got {other:?}")
            }
        }
    }

    // -- fsync policy ------------------------------------------------------

    #[test]
    fn store_append_fsyncs_queue_acceptance_expedition_and_consumption_before_turn_end() {
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
                        images: Vec::new(),
                        queued_prompt: None,
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
            "ordinary non-boundary events do not force a barrier"
        );

        let queued = QueuedPromptId::new();
        store
            .append(
                &id,
                &ev(
                    seq.next(),
                    turn,
                    EventKind::PromptQueued {
                        id: queued,
                        text: "follow up".to_owned(),
                        images: Vec::new(),
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            fsyncs.load(Ordering::SeqCst),
            1,
            "acceptance must be durable before the composer is cleared"
        );

        store
            .append(
                &id,
                &ev(
                    seq.next(),
                    turn,
                    EventKind::PromptDeliveryExpedited { through: queued },
                ),
            )
            .unwrap();
        assert_eq!(
            fsyncs.load(Ordering::SeqCst),
            2,
            "expedited intent must be durable before interruption"
        );

        store
            .append(
                &id,
                &ev(
                    seq.next(),
                    turn,
                    EventKind::UserPrompt {
                        text: "follow up".to_owned(),
                        images: Vec::new(),
                        queued_prompt: Some(queued),
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            fsyncs.load(Ordering::SeqCst),
            3,
            "correlated consumption must be durable before the next request"
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
            4,
            "the turn boundary adds its own barrier after queue barriers"
        );
    }

    #[test]
    fn queue_durability_failure_rejects_each_stable_storage_gate() {
        for kind in [
            EventKind::PromptQueued {
                id: QueuedPromptId::new(),
                text: "accepted only after fsync".to_owned(),
                images: Vec::new(),
            },
            EventKind::PromptDeliveryExpedited {
                through: QueuedPromptId::new(),
            },
            EventKind::UserPrompt {
                text: "consumed only after fsync".to_owned(),
                images: Vec::new(),
                queued_prompt: Some(QueuedPromptId::new()),
            },
        ] {
            let dir = tempdir().unwrap();
            let fsyncs = Arc::new(AtomicUsize::new(0));
            let store = JsonlStore::new(
                dir.path(),
                "/ws",
                Box::new(FailingFsyncIo {
                    inner: CountingIo {
                        inner: OsIo,
                        fsyncs,
                    },
                    fail_on: 1,
                }),
            );
            let id = SessionId::new();
            let turn = TurnId::new();
            let mut seq = Seq::default();

            let failure = store.append(&id, &ev(seq.next(), turn, kind.clone()));
            assert!(
                failure.is_err(),
                "{kind:?} cannot be acknowledged without fsync"
            );
        }
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

    #[test]
    fn lease_local_evidence_admissions_do_not_read_the_whole_log_per_record() {
        let dir = tempdir().unwrap();
        let reads = Arc::new(Mutex::new(HashMap::new()));
        let io = ReadCountingIo {
            inner: OsIo,
            reads: reads.clone(),
        };
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(io));
        let session = SessionId::new();
        let opened = store.open_writer(&session, evidence_lease()).unwrap();
        let log = log_path(dir.path(), &session);
        let reads_before = *reads.lock().unwrap().get(&log).unwrap_or(&0);
        let service = EvidenceAdmissionService::new();
        for _ in 0..4 {
            let invocation = RunInvocationId::new();
            service
                .admit_run_context(
                    &opened.writer,
                    RunContextEvidence::new(
                        session,
                        invocation,
                        RunId::new(session, invocation.uuid()),
                        ConfigProvenance::default(),
                    ),
                )
                .unwrap();
        }
        let reads_after = *reads.lock().unwrap().get(&log).unwrap_or(&0);
        assert_eq!(
            reads_after,
            reads_before,
            "evidence admission read the whole log {} additional times",
            reads_after - reads_before
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
                        images: Vec::new(),
                        queued_prompt: None,
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
                        images: Vec::new(),
                        queued_prompt: None,
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
                            images: Vec::new(),
                            queued_prompt: None,
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

    /// A copied session directory is listed under neither name.
    ///
    /// `list` is the only method that reads a session's id off the disk
    /// instead of being handed one; every other method derives the directory
    /// from the id. So a directory whose manifest names a different session
    /// is the one place the two can disagree, and what it would produce is an
    /// id `ego ls` prints and `ego resume` opens as something else — the
    /// copy's contents under the original's name.
    ///
    /// `cp -r` is how it happens: nothing in this crate writes a manifest
    /// anywhere but its own directory. The copy is skipped and the original
    /// survives, because the original's two halves still agree.
    #[test]
    fn store_list_skips_a_session_directory_that_names_a_different_id_than_its_manifest() {
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
                        images: Vec::new(),
                        queued_prompt: None,
                    },
                ),
            )
            .unwrap();

        // The snapshot someone takes by hand: same bytes, different name.
        let copy_id = SessionId::new();
        let original = manifest_path(dir.path(), &good_id);
        let copy = manifest_path(dir.path(), &copy_id);
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::copy(&original, &copy).unwrap();

        let metas = store.list().unwrap();
        assert_eq!(
            metas.len(),
            1,
            "the copy must not be listed: its manifest names a session that \
             lives in another directory"
        );
        assert_eq!(
            metas[0].id, good_id,
            "the directory that still agrees with its own manifest is the one \
             that survives"
        );
    }
}
