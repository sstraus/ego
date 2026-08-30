//! Tail repair and the crash kill-point matrix: what `JsonlStore::recover`
//! must salvage from a session directory a crash left half-written.
//!
//! See `plans/m0a-contracts.md` Step 6 and `ARCHITECTURE.md` §5. This is the
//! milestone's **storage gate**: if JSONL cannot survive this matrix, the
//! plan's standing instruction is to flip to SQLite behind the same
//! `SessionStore` trait.
//!
//! ## Why the faults are injected with `std::fs`, not a `FailingFs` fake
//!
//! The plan sketched a `FailingFs` injecting partial writes at parametrised
//! points. This does it by writing the damage directly to a real file
//! instead, for one reason: a fake that produces the damage is a fake that
//! *encodes our belief about* what a crash leaves behind, and the test would
//! then agree with the implementation because both were written from the
//! same belief. Corrupting real bytes on a real filesystem — a half-written
//! final line, a flipped payload byte, an orphaned scratch file — states the
//! damage in the only vocabulary the recovery code actually meets on disk.
//!
//! `StoreIo` is still the seam that makes this testable at all; it is simply
//! not the seam the *damage* goes through.

use std::path::{Path, PathBuf};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::store::codec::CODEC_VERSION;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_tools::store::{JsonlStore, NonDurableOsIo, OsIo, tmp_path_for};
use futures::StreamExt;
use futures::executor::block_on;
use tempfile::{TempDir, tempdir};

// Wrapped in a module named `faults` so `cargo test -p ego-tools faults`
// (the story's VERIFY command) matches these tests by substring: cargo's
// filter matches the full test path, not the source file name, so bare
// top-level functions here would not contain "faults" and the VERIFY
// command would run zero tests while still reporting success.
mod faults {
    use super::*;

    /// A session on disk, plus the handles a test needs to damage it.
    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        store: JsonlStore,
        id: SessionId,
    }

    impl Fixture {
        fn log_path(&self) -> PathBuf {
            self.root
                .join(session_dir_name(&self.id))
                .join("events.jsonl")
        }

        fn manifest_path(&self) -> PathBuf {
            self.root
                .join(session_dir_name(&self.id))
                .join("manifest.json")
        }

        fn log_bytes(&self) -> Vec<u8> {
            std::fs::read(self.log_path()).unwrap()
        }

        fn manifest(&self) -> SessionMeta {
            serde_json::from_slice(&std::fs::read(self.manifest_path()).unwrap()).unwrap()
        }

        /// Every event `read` yields, requiring all of them to decode. Used
        /// as the post-recovery health check: a log that still contains a
        /// bad line surfaces here as an `Err`.
        fn read_all(&self) -> Result<Vec<DomainEvent>, StoreError> {
            let stream = self.store.read(&self.id)?;
            block_on(stream.collect::<Vec<_>>())
                .into_iter()
                .map(|result| match result? {
                    SessionEntry::Event(event) => Ok(event),
                    SessionEntry::RunTransaction(_) => Err(StoreError::Io(std::io::Error::other(
                        "event-only fixture contained a run transaction",
                    ))),
                    SessionEntry::Evidence(_) => Err(StoreError::Io(std::io::Error::other(
                        "event-only fixture contained evidence",
                    ))),
                })
                .collect()
        }
    }

    fn session_dir_name(id: &SessionId) -> String {
        match serde_json::to_value(id).expect("SessionId always serialises") {
            serde_json::Value::String(s) => s,
            other => panic!("SessionId did not serialise as a JSON string: {other:?}"),
        }
    }

    fn ev(seq: Seq, turn: TurnId, kind: EventKind, ts_millis: u64) -> DomainEvent {
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

    /// A healthy session: one turn of four events, the last of which is a
    /// `TurnCompleted`, so the manifest has been written and fsynced and
    /// the log is complete and well-formed before any damage is applied.
    fn healthy_session() -> Fixture {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = JsonlStore::new(&root, "/ws", Box::new(OsIo));
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();

        for (kind, ts) in [
            (
                EventKind::UserPrompt {
                    text: "list the files".to_string(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
                1_000,
            ),
            (
                EventKind::AssistantDelta {
                    text: "Sure".to_string(),
                },
                1_100,
            ),
            (
                EventKind::AssistantMessage {
                    text: "Sure".to_string(),
                },
                1_200,
            ),
            (
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ExitReason::Completed,
                },
                1_300,
            ),
        ] {
            store.append(&id, &ev(seq.next(), turn, kind, ts)).unwrap();
        }

        Fixture {
            _dir: dir,
            root,
            store,
            id,
        }
    }

    /// Plant a session under `root` whose log is exactly `log` (and which
    /// has no manifest), returning its id.
    ///
    /// This exists for the truncation sweeps. Going through `append` there
    /// would re-run four fsynced appends plus two fsynced manifest replaces
    /// for *every* byte offset — around a thousand times over — which took
    /// over a minute of real disk syncing to assert something that depends
    /// only on the log's bytes. Each cut gets a fresh session id under one
    /// shared root rather than a fresh temporary directory, since sessions
    /// are already isolated from each other by id.
    ///
    /// How the bytes got there is precisely what these sweeps are not
    /// testing; the kill-point tests above cover the append path that
    /// writes them.
    fn plant_log(root: &Path, log: &[u8]) -> SessionId {
        let id = SessionId::new();
        let dir = root.join(session_dir_name(&id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.jsonl"), log).unwrap();
        id
    }

    /// Every event `read` yields for `id`, requiring all of them to decode.
    fn read_all_of(store: &JsonlStore, id: &SessionId) -> Result<Vec<DomainEvent>, StoreError> {
        let stream = store.read(id)?;
        block_on(stream.collect::<Vec<_>>())
            .into_iter()
            .map(|result| match result? {
                SessionEntry::Event(event) => Ok(event),
                SessionEntry::RunTransaction(_) => Err(StoreError::Io(std::io::Error::other(
                    "event-only fixture contained a run transaction",
                ))),
                SessionEntry::Evidence(_) => Err(StoreError::Io(std::io::Error::other(
                    "event-only fixture contained evidence",
                ))),
            })
            .collect()
    }

    fn append_raw(path: &Path, bytes: &[u8]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    // -- AC1: the kill-point matrix ---------------------------------------

    /// **Kill point: torn final line.** A crash mid-`append` leaves a
    /// partial line with no terminating newline. Recovery must cut it and
    /// keep everything before it.
    #[test]
    fn faults_torn_final_line_is_truncated_and_earlier_events_survive() {
        let fx = healthy_session();
        let before = fx.log_bytes();
        let healthy_events = fx.read_all().unwrap().len();

        // Exactly what an interrupted write leaves: a prefix of a real
        // record line, no newline.
        const TORN: &[u8] = br#"{"seq":5,"checksum":123,"payl"#;
        append_raw(&fx.log_path(), TORN);
        assert!(
            fx.read_all().is_err(),
            "precondition: the torn line must actually break read(), \
             otherwise this test proves nothing about recovery"
        );

        let repair = fx.store.recover(&fx.id).unwrap();

        assert_eq!(repair.truncated_bytes, TORN.len() as u64);
        assert_eq!(repair.dropped_lines, 1);
        assert_eq!(fx.log_bytes(), before, "log must be byte-identical again");
        assert_eq!(fx.read_all().unwrap().len(), healthy_events);
    }

    /// **Kill point: the newline itself was never written.** `append_line`
    /// writes the record and its terminator as two separate calls, so a
    /// crash between them leaves a line whose bytes are complete and whose
    /// checksum validates — only the newline is missing.
    ///
    /// This is dropped anyway, and the choice is deliberate rather than
    /// incidental: an unterminated final line is indistinguishable *as a
    /// prefix* from one still being written, and the alternative (keep it
    /// and re-terminate it) would mean deciding a partial write is finished
    /// on the strength of a checksum that a truncated-but-lucky byte range
    /// could also satisfy. The lost event is mid-turn by construction —
    /// `append` only fsyncs on turn boundaries — so it was never promised
    /// durable in the first place.
    ///
    /// Without this test the newline rule is never exercised on its own:
    /// the torn-line test above feeds invalid JSON, which `decode_line`
    /// rejects for an entirely different reason.
    #[test]
    fn faults_complete_final_line_missing_only_its_newline_is_still_dropped() {
        let fx = healthy_session();
        let before = fx.log_bytes();
        let healthy_events = fx.read_all().unwrap().len();

        // A byte-for-byte copy of the log's own last line: valid JSON,
        // valid checksum, correct shape — just unterminated.
        let last_line: Vec<u8> = before
            .strip_suffix(b"\n")
            .unwrap()
            .rsplit(|&b| b == b'\n')
            .next()
            .unwrap()
            .to_vec();
        append_raw(&fx.log_path(), &last_line);
        assert!(
            !fx.log_bytes().ends_with(b"\n"),
            "precondition: the injected line must be unterminated"
        );

        let repair = fx.store.recover(&fx.id).unwrap();

        assert_eq!(repair.truncated_bytes, last_line.len() as u64);
        assert_eq!(repair.dropped_lines, 1);
        assert_eq!(fx.log_bytes(), before);
        assert_eq!(fx.read_all().unwrap().len(), healthy_events);
    }

    /// **Kill point: torn final line, whole-record variant.** A crash can
    /// also land after a *complete* line was written but before its
    /// checksum could describe it — a flipped byte in the payload. The
    /// record parses as JSON and only the checksum betrays it, which is
    /// exactly what `StoreRecord`'s checksum exists for.
    #[test]
    fn faults_final_line_with_a_corrupt_checksum_is_truncated() {
        let fx = healthy_session();
        let healthy_events = fx.read_all().unwrap().len();
        let before = fx.log_bytes();

        // A syntactically perfect record whose checksum does not match its
        // payload.
        append_raw(
            &fx.log_path(),
            br#"{"seq":5,"checksum":1,"payload":{"kind":"Error","message":"x"}}"#,
        );
        append_raw(&fx.log_path(), b"\n");
        assert!(
            fx.read_all().is_err(),
            "precondition: a checksum mismatch must break read()"
        );

        let repair = fx.store.recover(&fx.id).unwrap();

        assert_eq!(repair.dropped_lines, 1);
        assert_eq!(fx.log_bytes(), before);
        assert_eq!(fx.read_all().unwrap().len(), healthy_events);
    }

    /// **Kill point: the header line itself is damaged.** Every test above
    /// damages the *tail*, which is what a crash mid-append leaves. This one
    /// damages the first line, and the difference is total: `valid_prefix`
    /// requires a decodable header at index 0, so a header that will not
    /// decode makes the valid prefix **zero bytes**, and `recover` truncates
    /// the file to nothing — every record in the session, however healthy,
    /// however long ago it was fsynced.
    ///
    /// Tail repair exists to salvage a log. It must not be the thing that
    /// destroys one. A first line that is not a header this build understands
    /// is not a torn append: it is a log this build cannot speak for, and the
    /// answer is to refuse it and leave the bytes alone so a later build, or
    /// a human, still has something to read.
    ///
    /// This is also the gate on ever adding a field to `Header`. Without
    /// it, the commit that does so deletes every session written before it,
    /// on first open — `open_writer` calls `recover` before `resume`.
    #[test]
    fn faults_a_header_this_build_cannot_read_is_refused_not_truncated() {
        let fx = healthy_session();
        let before = fx.log_bytes();
        let healthy_events = fx.read_all().unwrap().len();
        assert!(
            healthy_events > 0,
            "precondition: there must be records to lose"
        );

        // Still recognisable as a header — it carries `v` — and still
        // undecodable, because `session` is not an id. A flipped byte in the
        // first line produces exactly this, and so does any future build
        // whose header requires a field this one does not write.
        let tail = before
            .iter()
            .position(|&b| b == b'\n')
            .map(|at| before[at..].to_vec())
            .unwrap();
        let mut damaged = br#"{"v":4,"session":"not-an-id","created":1}"#.to_vec();
        damaged.extend_from_slice(&tail);
        std::fs::write(fx.log_path(), &damaged).unwrap();

        let outcome = fx.store.recover(&fx.id);

        assert!(
            outcome.is_err(),
            "an unreadable header must be reported, not repaired: {outcome:?}"
        );
        assert_eq!(
            fx.log_bytes(),
            damaged,
            "not one byte may be removed: the records after a bad header are \
             the whole session, and this build cannot say they are wrong"
        );
    }

    /// **Kill point: a log from a build that is not this one.** `v` was
    /// written into every header and compared nowhere, so a log written by a
    /// later codec was read as though it were this version's — its records
    /// interpreted under rules that were never used to write them.
    ///
    /// Refused, and refused without a write, for the same reason as above:
    /// this build cannot say those bytes are wrong, only that it cannot read
    /// them. Note that serde ignores unknown fields, so a *future* header is
    /// undetectable by decoding alone — only `v` distinguishes it, which is
    /// exactly why it has to be looked at.
    #[test]
    fn faults_a_log_from_a_later_codec_version_is_refused_not_truncated() {
        let fx = healthy_session();
        let before = fx.log_bytes();

        let tail = before
            .iter()
            .position(|&b| b == b'\n')
            .map(|at| before[at..].to_vec())
            .unwrap();
        // A perfectly well-formed header, one version ahead, carrying a field
        // this build has never heard of — which decodes fine and must still
        // be refused.
        let session = serde_json::to_string(&fx.id).unwrap();
        let future_version = CODEC_VERSION + 1;
        let mut future = format!(
            r#"{{"v":{future_version},"session":{session},"created":1,"workspace_root":"/ws","invented":true}}"#
        )
        .into_bytes();
        future.extend_from_slice(&tail);
        std::fs::write(fx.log_path(), &future).unwrap();

        match fx.store.recover(&fx.id) {
            Err(StoreError::UnsupportedLog { found }) => assert_eq!(found, future_version),
            other => panic!("a later codec version must be named, not repaired: {other:?}"),
        }
        assert_eq!(fx.log_bytes(), future, "not one byte may be removed");
    }

    /// **Kill point: torn manifest tmp.** A crash between
    /// `replace_atomic`'s write and its rename leaves a scratch file.
    /// Recovery must remove it — and must not mistake it for the manifest.
    #[test]
    fn faults_orphan_manifest_scratch_file_is_removed() {
        let fx = healthy_session();
        let tmp = tmp_path_for(&fx.manifest_path());
        std::fs::write(&tmp, b"{ this is a half-written manifest").unwrap();
        let good_manifest = fx.manifest();

        let repair = fx.store.recover(&fx.id).unwrap();

        assert!(repair.removed_tmp);
        assert!(
            !tmp.exists(),
            "scratch file must be gone: {}",
            tmp.display()
        );
        assert_eq!(
            fx.manifest(),
            good_manifest,
            "a healthy manifest must not be disturbed by clearing the scratch file"
        );
    }

    /// **Kill point: missing fsync window.** `append` only rewrites the
    /// manifest on a turn boundary, so a crash mid-turn leaves the log
    /// ahead of the manifest. Recovery must rebuild the manifest from the
    /// log, which is the authority.
    #[test]
    fn faults_manifest_behind_its_log_is_rebuilt_from_the_log() {
        let fx = healthy_session();
        let stale = fx.manifest();

        // A further turn's worth of events, none of them a turn boundary,
        // so the manifest is deliberately left behind — this is the real
        // steady-state window, not synthetic damage.
        let turn = TurnId::new();
        let mut seq = Seq::default();
        for _ in 0..5 {
            seq.next();
        }
        fx.store
            .append(
                &fx.id,
                &ev(
                    seq.next(),
                    turn,
                    EventKind::UserPrompt {
                        text: "and again".to_string(),
                        images: Vec::new(),
                        queued_prompt: None,
                    },
                    9_000,
                ),
            )
            .unwrap();

        assert_eq!(
            fx.manifest().updated,
            stale.updated,
            "precondition: a non-boundary append must not have touched the manifest"
        );

        let repair = fx.store.recover(&fx.id).unwrap();

        assert!(repair.manifest_rebuilt);
        assert_eq!(fx.manifest().updated, Timestamp::from_millis(9_000));
        assert_eq!(
            fx.manifest().created,
            stale.created,
            "created must be carried from the log header, never bumped"
        );
    }

    /// A manifest whose bytes do not parse is treated exactly like a
    /// missing one: it is a derived cache, and the log can regenerate it.
    #[test]
    fn faults_unparseable_manifest_is_rebuilt_from_the_log() {
        let fx = healthy_session();
        std::fs::write(fx.manifest_path(), b"not json at all").unwrap();

        let repair = fx.store.recover(&fx.id).unwrap();

        assert!(repair.manifest_rebuilt);
        let rebuilt = fx.manifest();
        assert_eq!(rebuilt.id, fx.id);
        assert_eq!(rebuilt.updated, Timestamp::from_millis(1_300));
    }

    /// A manifest deleted outright is likewise rebuilt, so `list()` can see
    /// the session again.
    #[test]
    fn faults_missing_manifest_is_rebuilt_so_the_session_is_listed_again() {
        let fx = healthy_session();
        std::fs::remove_file(fx.manifest_path()).unwrap();
        assert!(
            fx.store.list().unwrap().is_empty(),
            "precondition: without a manifest the session is invisible to list()"
        );

        let repair = fx.store.recover(&fx.id).unwrap();

        assert!(repair.manifest_rebuilt);
        assert_eq!(fx.store.list().unwrap().len(), 1);
    }

    /// **Kill point: orphan lease.** A lease file left by a killed writer
    /// must not stop recovery — and recovery must not delete it, because
    /// the same file may belong to a *live* writer. Ownership of stale
    /// leases is `Lease::acquire`'s takeover path, not this one.
    #[test]
    fn faults_orphan_lease_file_neither_blocks_nor_is_deleted_by_recovery() {
        let fx = healthy_session();
        let lease_path = fx.root.join(session_dir_name(&fx.id)).join("lease");
        std::fs::write(&lease_path, br#"{"pid":999999,"heartbeat":1}"#).unwrap();

        let repair = fx.store.recover(&fx.id).unwrap();

        assert!(repair.is_clean(), "an orphan lease is not log damage");
        assert!(
            lease_path.exists(),
            "recovery must not evict a lease it cannot prove is dead"
        );
        assert_eq!(fx.read_all().unwrap().len(), 4);
    }

    /// Recovery on an untouched session must change nothing and say so —
    /// otherwise a caller cannot run it unconditionally on open, and
    /// "repaired" would lose its meaning.
    #[test]
    fn faults_recover_is_a_no_op_on_a_healthy_session_and_is_idempotent() {
        let fx = healthy_session();
        let before = fx.log_bytes();
        let manifest_before = fx.manifest();

        for round in 0..3 {
            let repair = fx.store.recover(&fx.id).unwrap();
            assert!(repair.is_clean(), "round {round}: {repair:?}");
            assert_eq!(fx.log_bytes(), before, "round {round}");
            assert_eq!(fx.manifest(), manifest_before, "round {round}");
        }
    }

    /// Recovering a session that was never written is not an error: a
    /// caller should not have to know whether an id exists before it can
    /// safely recover it.
    #[test]
    fn faults_recover_on_an_unknown_session_is_clean_and_creates_nothing() {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();

        let repair = store.recover(&id).unwrap();

        assert!(repair.is_clean());
        assert!(!dir.path().join(session_dir_name(&id)).exists());
    }

    // -- AC2: truncation at any byte offset -------------------------------

    /// **The property.** Truncating the log at *every* byte offset and
    /// recovering must never leave state a reader chokes on.
    ///
    /// This is exhaustive over all offsets rather than randomly sampled:
    /// the log is small enough that "every offset" is cheap, and it is a
    /// strictly stronger statement than any number of random draws — there
    /// is no seed, no flake, and no offset a future run might miss.
    ///
    /// The invariant is stated in two halves, because "no corrupt state"
    /// has two legitimate outcomes:
    ///
    /// - the header survived ⇒ `read` succeeds and *every* event it yields
    ///   decodes cleanly;
    /// - the header did not ⇒ `read` fails with `MissingHeader`, which is
    ///   an honest report about a log with no header, not corruption.
    ///
    /// What must never happen is the third case: `read` succeeding and then
    /// yielding a checksum or parse error mid-stream.
    #[test]
    fn faults_truncation_at_any_byte_offset_recovers_to_a_readable_log() {
        let full = healthy_session().log_bytes();
        assert!(full.len() > 100, "fixture too small to be a real test");

        let dir = tempdir().unwrap();
        // `NonDurableOsIo`, not `OsIo`, and this is the only test in the
        // file that departs from the real one.
        //
        // What this sweep proves is a property of *bytes*: that `recover`
        // leaves a readable log after a cut at every one of the ~1142
        // offsets, never inventing, reordering or duplicating an event. It
        // asserts nothing about durability — every check below reads the log
        // back in this same process, where the page cache answers
        // identically whether or not the barrier was issued. The fsyncs are
        // therefore invisible to all of it.
        //
        // They were not invisible to the clock. Each cut drove up to three
        // `F_FULLFSYNC` calls (one in `truncate`, two in `replace_atomic`
        // when the manifest was rebuilt) — 2744 device barriers per run,
        // measured at ~5.9ms each. That was 21.9s of this test's 22.2s and
        // 21.9s of a 29.4s workspace suite: 92% of the milestone's whole
        // test budget, spent on a guarantee nothing here reads.
        // Hardware-bound, so on a slower disk or a shared runner it is
        // several times worse without testing anything more.
        //
        // Durability is not lost from the file, only from this test: the
        // twelve kill-point tests above stay on `OsIo` and are where the
        // crash contract of `ARCHITECTURE.md` §5 is actually asserted. And
        // `NonDurableOsIo` is `OsIo` with the barrier elided and nothing
        // else changed — same tmp-write-then-`rename`, same `set_len` — so
        // the sweep still meets real filesystem semantics rather than a
        // fake's idea of them.
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(NonDurableOsIo));

        for cut in 0..=full.len() {
            let id = plant_log(dir.path(), &full[..cut]);

            let repair = store
                .recover(&id)
                .unwrap_or_else(|e| panic!("cut {cut}: recover failed: {e:?}"));

            match read_all_of(&store, &id) {
                Ok(events) => {
                    // Recovery may only ever remove whole events from the
                    // end; it must never invent or reorder one.
                    assert!(
                        events.len() <= 4,
                        "cut {cut}: recovery produced {} events, more than were ever written",
                        events.len()
                    );
                    let mut seqs: Vec<_> = events.iter().map(|e| e.env.seq).collect();
                    let sorted = {
                        let mut s = seqs.clone();
                        s.sort();
                        s
                    };
                    assert_eq!(seqs, sorted, "cut {cut}: events out of seq order");
                    seqs.dedup();
                    assert_eq!(seqs.len(), events.len(), "cut {cut}: duplicated seq");
                }
                Err(StoreError::MissingHeader) => {
                    assert!(
                        cut < full.iter().position(|&b| b == b'\n').unwrap() + 1,
                        "cut {cut}: MissingHeader although the header line was complete"
                    );
                }
                Err(e) => panic!("cut {cut}: left unreadable state: {e:?} (repair: {repair:?})"),
            }
        }
    }

    /// The same sweep, but asserting the *positive* half: a cut that lands
    /// exactly on a line boundary must lose nothing at all, since every
    /// line before it is complete and valid.
    #[test]
    fn faults_truncation_on_a_line_boundary_loses_nothing() {
        let full = healthy_session().log_bytes();
        let boundaries: Vec<usize> = full
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(i, _)| i + 1)
            .collect();
        assert_eq!(boundaries.len(), 5, "header + 4 records");

        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));

        // Skip the header-only boundary: it has no records to yield.
        for (records, cut) in boundaries.iter().copied().enumerate().skip(1) {
            let id = plant_log(dir.path(), &full[..cut]);

            let repair = store.recover(&id).unwrap();

            assert_eq!(
                repair.truncated_bytes, 0,
                "cut {cut} is a line boundary; nothing should have been truncated"
            );
            assert_eq!(
                read_all_of(&store, &id).unwrap().len(),
                records,
                "cut {cut}"
            );
        }
    }

    // -- Durability of the manifest replace path --------------------------

    /// `replace_atomic` must leave no scratch file behind on the success
    /// path either — otherwise every ordinary manifest write would look
    /// like a crash to the next `recover`, and `removed_tmp` would be
    /// meaningless.
    #[test]
    fn faults_successful_manifest_write_leaves_no_scratch_file() {
        let fx = healthy_session();
        let tmp = tmp_path_for(&fx.manifest_path());

        assert!(!tmp.exists(), "{} should not exist", tmp.display());
        assert!(fx.manifest_path().exists());
        assert!(fx.store.recover(&fx.id).unwrap().is_clean());
    }
}
