//! Real-file regressions for cold session storage. No fakes: compression,
//! leases and locks are the production `OsIo`, `fd-lock` and zstd paths.
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::export::Exporter;
use ego_core::store::blob::{BlobRef, BlobStore};
use ego_core::store::session_store::{SessionEntry, SessionStore};
use futures::StreamExt as _;

use super::{ColdStoragePolicy, Outcome, packed_path};
use crate::store::{JsonlStore, LeaseInputs, OsIo};

const DAY: Duration = Duration::from_secs(86_400);

fn policy() -> ColdStoragePolicy {
    ColdStoragePolicy {
        cold_after: 7 * DAY,
        idle_ratio: 0,
    }
}

/// Eight days after the files were written: every seeded session is cold.
fn later() -> SystemTime {
    SystemTime::now() + 8 * DAY
}

/// Kernel lock release decides liveness; never steal a held lease.
fn alive(_pid: u32) -> bool {
    true
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn event(position: u64, item: Option<ItemId>, kind: EventKind) -> DomainEvent {
    let mut seq = Seq::ZERO;
    for _ in 0..position {
        seq.next();
    }
    DomainEvent {
        env: Envelope {
            seq,
            id: EventId::new(),
            turn: TurnId::new(),
            item,
            parent: None,
            ts: Timestamp::from_millis(1_000),
            producer: Producer::Main,
        },
        kind,
    }
}

fn assistant(text: &str) -> EventKind {
    EventKind::AssistantMessage { text: text.into() }
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<JsonlStore>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonlStore::new(
            dir.path().join("sessions"),
            dir.path(),
            Box::new(OsIo),
        ));
        Self { dir, store }
    }

    fn path(&self, id: &SessionId, name: &str) -> PathBuf {
        self.dir
            .path()
            .join("sessions")
            .join(id.to_string())
            .join(name)
    }

    /// Three events, a manifest and one evidence blob.
    fn seed(&self) -> (SessionId, BlobRef) {
        let id = SessionId::new();
        let title = EventKind::SessionTitleChanged {
            title: Some("Cold storage".into()),
        };
        let prompt = EventKind::UserPrompt {
            audio: Vec::new(),
            text: "compress coldneedle".into(),
            images: Vec::new(),
            queued_prompt: None,
        };
        for (position, item, kind) in [
            (1, None, title),
            (2, Some(ItemId::new()), prompt),
            (
                3,
                Some(ItemId::new()),
                assistant(&"answer coldneedle ".repeat(256)),
            ),
        ] {
            self.store
                .append(&id, &event(position, item, kind))
                .unwrap();
        }
        let blob = self.blob(&id, &b"evidence line\n".repeat(512));
        (id, blob)
    }

    fn blob(&self, id: &SessionId, bytes: &[u8]) -> BlobRef {
        let evidence = self.store.probe_evidence_store(*id, 1 << 20).unwrap();
        let mut writer = evidence.blob_store().begin().unwrap();
        writer.append(bytes).unwrap();
        writer.commit().unwrap()
    }

    fn read_blob(&self, id: &SessionId, blob: &BlobRef) -> Vec<u8> {
        let evidence = self.store.probe_evidence_store(*id, 1 << 20).unwrap();
        evidence.blob_store().read(blob).unwrap()
    }

    fn blob_path(&self, id: &SessionId, blob: &BlobRef) -> PathBuf {
        self.path(id, &format!("evidence/{}.blob", blob.id.as_str()))
    }

    fn entries(&self, id: &SessionId) -> Vec<SessionEntry> {
        let stream = SessionStore::read(self.store.as_ref(), id).unwrap();
        futures::executor::block_on(stream.map(Result::unwrap).collect())
    }

    fn compress(&self) -> super::ColdStorageReport {
        self.store
            .compress_cold_sessions(&policy(), later())
            .unwrap()
    }

    fn staged_files(&self, id: &SessionId) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for dir in [self.path(id, ""), self.path(id, "evidence")] {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.to_string_lossy().ends_with(".tmp") {
                    found.push(path);
                }
            }
        }
        found
    }

    fn lease(&self, now: SystemTime) -> LeaseInputs<'static> {
        LeaseInputs {
            pid: std::process::id(),
            now: Timestamp::from_millis(millis(now)),
            stale_after: Duration::from_secs(60),
            is_alive: &alive,
        }
    }
}

// Catches: a lossy or partial compression that drops events, and a resume
// that appends beside the compressed log instead of restoring it first.
#[test]
fn cold_session_survives_compression_and_resume_with_byte_identical_events() {
    let fixture = Fixture::new();
    let (id, blob) = fixture.seed();
    let log = fixture.path(&id, "events.jsonl");
    let manifest = fixture.path(&id, "manifest.json");
    let plain_log = fs::read(&log).unwrap();
    let plain_manifest = fs::read(&manifest).unwrap();
    let entries = fixture.entries(&id);

    let report = fixture.compress();

    assert_eq!(report.compressed, 1, "{report:?}");
    assert!(report.bytes_after < report.bytes_before, "{report:?}");
    for plain in [&log, &fixture.blob_path(&id, &blob)] {
        assert!(!plain.exists(), "{} kept", plain.display());
        assert!(
            packed_path(plain).exists(),
            "{} not packed",
            plain.display()
        );
    }
    assert!(fixture.staged_files(&id).is_empty());
    assert_eq!(fixture.entries(&id), entries);

    let opened = fixture
        .store
        .open_writer(&id, fixture.lease(SystemTime::now()))
        .unwrap();
    assert_eq!(fs::read(&log).unwrap(), plain_log);
    assert_eq!(fs::read(&manifest).unwrap(), plain_manifest);
    assert!(!packed_path(&log).exists() && !packed_path(&manifest).exists());
    opened
        .writer
        .append(
            &id,
            &event(4, Some(ItemId::new()), assistant("after resume")),
        )
        .unwrap();
    drop(opened);
    let resumed = fs::read(&log).unwrap();
    assert!(resumed.starts_with(&plain_log));
    assert_eq!(
        resumed[plain_log.len()..]
            .iter()
            .filter(|b| **b == b'\n')
            .count(),
        1
    );
    assert_eq!(fixture.entries(&id).len(), entries.len() + 1);
}

// Catches: compression of an active session — a held lease, a fresh lease
// heartbeat, or a held events.lock.
#[test]
fn session_with_a_live_writer_is_skipped() {
    let fixture = Fixture::new();
    let (id, _) = fixture.seed();
    let log = fixture.path(&id, "events.jsonl");
    let plain = fs::read(&log).unwrap();
    let untouched = |report: super::ColdStorageReport, case: &str| {
        assert_eq!(report.compressed, 0, "{case}: {report:?}");
        assert_eq!(report.live, 1, "{case}: {report:?}");
        assert_eq!(fs::read(&log).unwrap(), plain, "{case}");
        assert!(!packed_path(&log).exists(), "{case}");
    };

    // Held lease whose heartbeat is as old as the log: only the flock says live.
    let held = fixture
        .store
        .open_writer(&id, fixture.lease(UNIX_EPOCH))
        .unwrap();
    untouched(fixture.compress(), "held lease");
    drop(held);

    // Released lease with a heartbeat one day before the job's clock.
    drop(
        fixture
            .store
            .open_writer(&id, fixture.lease(later() - DAY))
            .unwrap(),
    );
    untouched(fixture.compress(), "fresh heartbeat");
    drop(
        fixture
            .store
            .open_writer(&id, fixture.lease(UNIX_EPOCH))
            .unwrap(),
    );

    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.path(&id, "events.lock"))
        .unwrap();
    let mut events_lock = fd_lock::RwLock::new(file);
    let guard = events_lock.write().unwrap();
    untouched(fixture.compress(), "held events.lock");
    drop(guard);

    assert_eq!(fixture.compress().compressed, 1);
}

// Catches: a commit that publishes a compressed copy without the event a
// writer appended while the copy was being written.
#[test]
fn writer_that_appends_during_compression_wins() {
    let fixture = Fixture::new();
    let (id, blob) = fixture.seed();
    let outcome = fixture
        .store
        .compress_session(&id, &policy(), later(), &mut || {
            fixture
                .store
                .append(&id, &event(4, Some(ItemId::new()), assistant("mid-job")))
                .unwrap();
        })
        .unwrap();

    assert_eq!(outcome, Outcome::Live);
    for name in ["events.jsonl", "manifest.json"] {
        let plain = fixture.path(&id, name);
        assert!(plain.exists() && !packed_path(&plain).exists(), "{name}");
    }
    assert!(fixture.blob_path(&id, &blob).exists());
    assert!(fixture.staged_files(&id).is_empty());
    assert_eq!(fixture.entries(&id).len(), 4);
}

// Catches: a reader that opens the plain path directly and loses a
// compressed session (history, export, catalogue or evidence fetch).
#[test]
fn every_reader_returns_the_same_result_for_compressed_and_plain_sessions() {
    let fixture = Fixture::new();
    let (id, blob) = fixture.seed();
    let history = fixture.store.history();
    let observe = || {
        let mut listed = fixture.store.list().unwrap();
        listed.retain(|meta| meta.id == id);
        (
            fixture.entries(&id),
            listed,
            fixture.store.readable_meta(&id).unwrap(),
            history.search("coldneedle").unwrap(),
            history.open(id).unwrap(),
            Exporter::new(fixture.store.as_ref()).sanitized(id),
            fixture.read_blob(&id, &blob),
        )
    };
    let plain = observe();
    assert_eq!(fixture.compress().compressed, 1);
    assert!(!fixture.path(&id, "events.jsonl").exists());
    assert_eq!(observe(), plain);
}

// Catches: a reader that prefers a stale compressed copy left by a crash
// after the rename, or reads a torn staging file left before it.
#[test]
fn plain_file_wins_over_crash_leftovers() {
    let fixture = Fixture::new();
    let (id, _) = fixture.seed();
    let (other, _) = fixture.seed();
    let log = fixture.path(&id, "events.jsonl");
    let entries = fixture.entries(&id);
    let stale = zstd::stream::encode_all(
        &fs::read(fixture.path(&other, "events.jsonl")).unwrap()[..],
        3,
    )
    .unwrap();
    fs::write(packed_path(&log), stale).unwrap();
    fs::write(super::tmp_path_for(&packed_path(&log)), b"torn").unwrap();

    assert_eq!(fixture.entries(&id), entries);
    assert_eq!(fixture.compress().compressed, 2);
    assert!(fixture.staged_files(&id).is_empty());
    assert_eq!(fixture.entries(&id), entries);
}

// Catches: garbage collection that matches only `.blob` and keeps
// unreachable compressed evidence forever, or drops a reachable one.
#[test]
fn garbage_collection_handles_compressed_blobs() {
    let fixture = Fixture::new();
    let (id, kept) = fixture.seed();
    let garbage = fixture.blob(&id, &b"unreachable evidence\n".repeat(512));
    assert_eq!(fixture.compress().compressed, 1);

    let evidence = fixture.store.probe_evidence_store(id, 1 << 20).unwrap();
    assert_eq!(
        evidence
            .collect_garbage(std::slice::from_ref(&kept))
            .unwrap(),
        1
    );
    assert!(!packed_path(&fixture.blob_path(&id, &garbage)).exists());
    assert_eq!(
        fixture.read_blob(&id, &kept),
        b"evidence line\n".repeat(512)
    );
}

// Catches: a job thread that keeps normal priority, or a renice that slows
// the whole process (every thread, turn path included) instead of the job.
#[cfg(target_os = "linux")]
#[test]
fn job_thread_runs_below_the_rest_of_the_process() {
    use rustix::process::getpriority_process;
    use rustix::thread::gettid;
    let caller = getpriority_process(Some(gettid())).unwrap();
    let job = std::thread::spawn(|| {
        let before = getpriority_process(Some(gettid())).unwrap();
        super::lower_job_thread_priority().unwrap();
        (before, getpriority_process(Some(gettid())).unwrap())
    })
    .join()
    .unwrap();
    assert_eq!(
        job.1,
        (job.0 + super::JOB_NICE_INCREMENT).min(19),
        "{job:?}"
    );
    assert_eq!(getpriority_process(Some(gettid())).unwrap(), caller);
}

// Catches: compression of files that fit one disk block, which saves no
// allocated space but costs a create, fsync, rename and unlink each.
#[test]
fn files_up_to_one_block_stay_plain() {
    let fixture = Fixture::new();
    let (id, large) = fixture.seed();
    let small = fixture.blob(&id, &[b'x'; 4096]);
    assert_eq!(fixture.compress().compressed, 1);
    let small_path = fixture.blob_path(&id, &small);
    assert!(small_path.exists() && !packed_path(&small_path).exists());
    assert!(packed_path(&fixture.blob_path(&id, &large)).exists());
    assert_eq!(fixture.read_blob(&id, &small), vec![b'x'; 4096]);
}

// Catches: a compressed copy created with umask permissions, which exposes
// private session data that the plain file kept owner-only.
#[cfg(unix)]
#[test]
fn compressed_files_keep_the_plain_file_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let (id, blob) = fixture.seed();
    let plains = [
        fixture.path(&id, "events.jsonl"),
        fixture.blob_path(&id, &blob),
    ];
    let mode = |path: &PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    for plain in &plains {
        fs::set_permissions(plain, fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert_eq!(fixture.compress().compressed, 1);
    for plain in &plains {
        assert_eq!(mode(&packed_path(plain)), 0o600, "{}", plain.display());
    }
}
