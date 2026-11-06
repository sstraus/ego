//! contract for the real session-bound evidence store.
//!
//! An `EvidenceWriter` appends fixed-size chunks to private staging and publishes only
//! after digest, length and durability checks. Readers receive one fixed page
//! addressed by the original-byte offset; no test in this module uses a
//! whole-blob read compatibility method.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::store::evidence::{
    EVIDENCE_PAGE_BYTES, EvidenceError, EvidenceRead, EvidenceRef, EvidenceStore,
};
use ego_tools::store::{JsonlStore, LeaseInputs, OsIo, SessionEvidenceStore, StoreIo};
use futures::executor::block_on;
use tempfile::{TempDir, tempdir};

struct Fixture {
    root: TempDir,
    session: SessionId,
    store: SessionEvidenceStore,
}

struct FaultIo {
    inner: OsIo,
    point: &'static str,
}

impl StoreIo for FaultIo {
    fn create_new(&self, path: &Path) -> io::Result<()> {
        self.inner.create_new(path)
    }

    fn append_bytes(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if self.point == "append" {
            return Err(io::Error::other("injected evidence append failure"));
        }
        self.inner.append_bytes(path, bytes)
    }

    fn visit_file(
        &self,
        path: &Path,
        visitor: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<bool> {
        self.inner.visit_file(path, visitor)
    }

    fn publish_new(&self, staging: &Path, target: &Path) -> io::Result<bool> {
        if self.point == "publish" {
            return Err(io::Error::other("injected evidence publish failure"));
        }
        self.inner.publish_new(staging, target)
    }

    fn list_files(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list_files(path)
    }

    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.inner.append_line(path, line)
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
        if self.point == "fsync" {
            return Err(io::Error::other("injected evidence fsync failure"));
        }
        self.inner.fsync(path)
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace_atomic(path, bytes)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.inner.truncate(path, len)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list_dirs(path)
    }
}

impl Fixture {
    fn new(cap: u64) -> Self {
        let root = tempdir().expect("evidence root");
        let session = SessionId::new();
        let store =
            SessionEvidenceStore::open(root.path(), session, cap).expect("session evidence store");
        Self {
            root,
            session,
            store,
        }
    }

    fn with_fault(point: &'static str) -> Self {
        let root = tempdir().expect("fault evidence root");
        let workspace = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let store = Box::leak(Box::new(JsonlStore::new(
            root.path(),
            workspace,
            Box::new(FaultIo { inner: OsIo, point }),
        )));
        let id = SessionId::new();
        const ALIVE: &dyn Fn(u32) -> bool = &|_| true;
        let opened = store
            .open_writer(
                &id,
                LeaseInputs {
                    pid: std::process::id(),
                    now: Timestamp::from_millis(1),
                    stale_after: Duration::from_secs(30),
                    is_alive: ALIVE,
                },
            )
            .expect("fault fixture writer");
        let evidence = opened.writer.evidence_store(1024).expect("evidence facade");
        std::mem::forget(opened);
        Self {
            root,
            session: id,
            store: evidence,
        }
    }

    fn append(&self, chunks: &[&[u8]]) -> Result<EvidenceRef, EvidenceError> {
        let mut writer = self.store.begin()?;
        for chunk in chunks {
            writer.append(chunk)?;
        }
        writer.commit()
    }
}

fn files_under(path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(path) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn read_pages(store: &SessionEvidenceStore, reference: &EvidenceRef) -> Vec<u8> {
    let mut offset = 0;
    let mut reconstructed = Vec::new();
    loop {
        let request = EvidenceRead::new(reference.clone(), offset).expect("page offset in range");
        let page = block_on(store.read_page(&request)).expect("evidence page");
        assert_eq!(page.reference, *reference);
        assert_eq!(page.offset, offset);
        assert!(page.bytes.len() <= EVIDENCE_PAGE_BYTES);
        reconstructed.extend_from_slice(&page.bytes);
        match page.next_offset {
            Some(next) => {
                assert!(next > offset, "pagination must make progress");
                offset = next;
            }
            None => break,
        }
    }
    reconstructed
}

#[test]
fn evidence_writer_is_incremental_and_commits_multichunk_digest_once() {
    let fixture = Fixture::new(4 * EVIDENCE_PAGE_BYTES as u64);
    let chunks: [&[u8]; 4] = [b"alpha", b"\0binary", b"middle", b"omega"];
    let reference = fixture.append(&chunks).expect("multichunk commit");
    let mut expected = Vec::new();
    for chunk in chunks {
        expected.extend_from_slice(chunk);
    }
    assert_eq!(reference.bytes, expected.len() as u64);
    assert_eq!(read_pages(&fixture.store, &reference), expected);
}

#[test]
fn evidence_page_contract_covers_zero_middle_and_eof_offsets() {
    let fixture = Fixture::new(4 * EVIDENCE_PAGE_BYTES as u64);
    let bytes = vec![b'x'; EVIDENCE_PAGE_BYTES * 2 + 17];
    let reference = fixture.append(&[&bytes]).expect("large evidence commit");

    let first = block_on(
        fixture
            .store
            .read_page(&EvidenceRead::new(reference.clone(), 0).unwrap()),
    )
    .expect("page zero");
    assert_eq!(first.offset, 0);
    assert_eq!(first.bytes.len(), EVIDENCE_PAGE_BYTES);
    assert_eq!(first.next_offset, Some(EVIDENCE_PAGE_BYTES as u64));

    let middle_offset = EVIDENCE_PAGE_BYTES as u64;
    let middle = block_on(
        fixture
            .store
            .read_page(&EvidenceRead::new(reference.clone(), middle_offset).unwrap()),
    )
    .expect("middle page");
    assert_eq!(middle.offset, middle_offset);
    assert_eq!(middle.bytes.len(), EVIDENCE_PAGE_BYTES);
    assert_eq!(middle.next_offset, Some((2 * EVIDENCE_PAGE_BYTES) as u64));

    let eof = block_on(
        fixture
            .store
            .read_page(&EvidenceRead::new(reference.clone(), reference.bytes).unwrap()),
    )
    .expect("empty EOF page");
    assert!(eof.bytes.is_empty());
    assert_eq!(eof.next_offset, None);
    assert_eq!(read_pages(&fixture.store, &reference), bytes);
}

#[test]
fn evidence_cap_accepts_exact_limit_and_rejects_cap_plus_one_before_publish() {
    let fixture = Fixture::new(8);
    let exact = fixture
        .append(&[b"1234", b"5678"])
        .expect("exact cap is valid");
    assert_eq!(exact.bytes, 8);
    let before = files_under(fixture.root.path());

    let mut writer = fixture.store.begin().expect("second writer");
    writer
        .append(b"12345678")
        .expect("writer accepts exact bytes");
    let error = writer
        .append(b"!")
        .expect_err("cap plus one is rejected live");
    assert!(matches!(error, EvidenceError::Limit { limit: 8 }));
    let _ = writer.abort();

    let after = files_under(fixture.root.path());
    assert_eq!(
        after
            .iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "blob"))
            .count(),
        before
            .iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "blob"))
            .count(),
        "rejected staging never publishes a partial blob"
    );
}

#[test]
fn abort_drop_and_restart_sweep_leave_no_retrievable_partial_evidence() {
    let fixture = Fixture::new(1024);
    let mut writer = fixture.store.begin().expect("begin staging");
    writer.append(b"aborted bytes").unwrap();
    writer.abort().expect("abort removes staging");

    let mut dropped = fixture.store.begin().expect("second staging");
    dropped.append(b"dropped bytes").unwrap();
    drop(dropped);

    let files = files_under(fixture.root.path());
    assert!(
        files
            .iter()
            .all(|path| path.extension().is_none_or(|ext| ext != "blob")),
        "an incomplete writer must not publish a blob"
    );

    let reopened = SessionEvidenceStore::open(fixture.root.path(), fixture.session, 1024)
        .expect("reopen performs stale staging sweep");
    let files = files_under(fixture.root.path());
    assert!(files.iter().all(|path| {
        path.extension()
            .is_none_or(|ext| ext != "blob" && ext != "staging")
    }));
    drop(reopened);
}

#[test]
fn dedup_is_idempotent_and_corrupt_existing_blob_fails_verification() {
    let fixture = Fixture::new(1024);
    let first = fixture.append(&[b"same bytes"]).expect("first commit");
    let second = fixture.append(&[b"same ", b"bytes"]).expect("dedup commit");
    assert_eq!(first, second);

    let blob = files_under(fixture.root.path())
        .into_iter()
        .find(|path| path.extension().is_some_and(|ext| ext == "blob"))
        .expect("published blob");
    std::fs::write(blob, b"corrupted").expect("inject existing corruption");
    let error = block_on(
        fixture
            .store
            .read_page(&EvidenceRead::new(first.clone(), 0).unwrap()),
    )
    .expect_err("corrupt existing evidence must fail closed");
    assert!(matches!(
        error,
        EvidenceError::Corrupt { .. } | EvidenceError::WrongLength { .. }
    ));
}

#[test]
fn evidence_reference_is_session_bound_and_forged_offsets_never_cross_sessions() {
    let root = tempdir().expect("shared evidence root");
    let source = SessionEvidenceStore::open(root.path(), SessionId::new(), 1024).unwrap();
    let target = SessionEvidenceStore::open(root.path(), SessionId::new(), 1024).unwrap();
    let reference = {
        let mut writer = source.begin().unwrap();
        writer.append(b"private session bytes").unwrap();
        writer.commit().unwrap()
    };
    let error = block_on(target.read_page(&EvidenceRead::new(reference.clone(), 0).unwrap()))
        .expect_err("reference from another session is not a global capability");
    assert!(matches!(error, EvidenceError::NotFound { .. }));
    assert!(EvidenceRead::new(reference, u64::MAX).is_err());
}

#[test]
fn injected_append_fsync_and_publish_failures_abort_without_partial_reference() {
    // These are named kill points in the store contract.  The fixture uses
    // the public fault-injection constructor supplied by the storage owner;
    // each failure must be observed while staging and leave no committed ref.
    let fault_points = ["append", "fsync", "publish"];
    for point in fault_points {
        let fixture = Fixture::with_fault(point);
        let mut writer = fixture.store.begin().expect("fault fixture begin");
        let result = if point == "append" {
            let error = writer
                .append(b"must not become complete")
                .expect_err("append fault must surface while staging");
            let _ = writer.abort();
            Err(error)
        } else {
            writer.append(b"must not become complete").unwrap();
            writer.commit()
        };
        assert!(result.is_err(), "fault point {point} was not injected");
        assert!(
            files_under(fixture.root.path())
                .iter()
                .all(|path| path.extension().is_none_or(|ext| ext != "blob")),
            "fault point {point} left a published partial blob"
        );
    }
}
