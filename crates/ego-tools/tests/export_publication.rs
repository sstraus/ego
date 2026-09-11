//! contract for regression case and regression case: sanitized artifact publication.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ego_core::run::RunId;
use ego_core::store::run_artifact::{
    ArtifactPublication, PublishedRunArtifact, RunArtifactId, RunArtifactStore,
    RunArtifactStoreError,
};
use ego_tools::store::{AtomicReplacePublication, JsonlStore, OsIo, StoreIo};

struct BeforePublicationFailure;

impl StoreIo for BeforePublicationFailure {
    fn append_line(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, bytes)
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

    fn replace_atomic(&self, _path: &Path, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::other("injected pre-publication failure"))
    }

    fn replace_atomic_publication(
        &self,
        _path: &Path,
        _bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        Err(io::Error::other("injected pre-publication failure"))
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

struct UncertainAfterWrite {
    replace_calls: Arc<AtomicUsize>,
    remove_calls: Arc<AtomicUsize>,
}

impl StoreIo for UncertainAfterWrite {
    fn append_line(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, bytes)
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
        self.replace_calls.fetch_add(1, Ordering::SeqCst);
        OsIo.replace_atomic(path, bytes)?;
        Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
            diagnostic: "injected post-publication uncertainty".to_owned(),
        })
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OsIo.truncate(path, len)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        OsIo.remove_file(path)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        OsIo.list_dirs(path)
    }
}

fn run() -> RunId {
    "00000000-0000-0000-0000-000000000069.00000000-0000-0000-0000-000000000001"
        .parse()
        .expect("valid run id")
}

fn artifact(bytes: &[u8]) -> PublishedRunArtifact {
    PublishedRunArtifact::sanitized_bundle(bytes.to_vec())
}

#[test]
fn sanitized_bundle_has_a_distinct_stable_identity_and_durable_result() {
    assert_eq!(
        RunArtifactId::sanitized_bundle().as_str(),
        "sanitized_bundle"
    );
    assert_ne!(RunArtifactId::sanitized_bundle(), RunArtifactId::output());

    let root = tempfile::tempdir().expect("artifact root");
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let result = store
        .publish(run(), artifact(br#"{"v":1,"records":[],"redactions":[]}"#))
        .expect("sanitized bundle publication");
    assert_eq!(result, ArtifactPublication::PublishedDurably);
    assert_eq!(
        store
            .read(run(), RunArtifactId::sanitized_bundle())
            .expect("published bundle")
            .bytes(),
        br#"{"v":1,"records":[],"redactions":[]}"#
    );
}

#[test]
fn prepublication_failure_leaves_no_sanitized_bundle_and_is_retry_safe() {
    let root = tempfile::tempdir().expect("artifact root");
    let store = JsonlStore::new(
        root.path(),
        "/workspace",
        Box::new(BeforePublicationFailure),
    );
    assert!(matches!(
        store.publish(run(), artifact(b"bundle")),
        Err(RunArtifactStoreError::Io { message }) if message.contains("pre-publication")
    ));
    assert!(matches!(
        store.read(run(), RunArtifactId::sanitized_bundle()),
        Err(RunArtifactStoreError::Absent { .. })
    ));
}

#[test]
fn identical_sanitized_bytes_are_idempotent_and_different_bytes_conflict() {
    let root = tempfile::tempdir().expect("artifact root");
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let bytes = br#"{"v":1,"records":[],"redactions":[]}"#;
    assert_eq!(
        store
            .publish(run(), artifact(bytes))
            .expect("first publication"),
        ArtifactPublication::PublishedDurably
    );
    assert_eq!(
        store
            .publish(run(), artifact(bytes))
            .expect("idempotent publication"),
        ArtifactPublication::PublishedDurably
    );
    assert!(matches!(
        store.publish(run(), artifact(b"different")),
        Err(RunArtifactStoreError::Conflict { .. })
    ));
    assert_eq!(
        store
            .read(run(), RunArtifactId::sanitized_bundle())
            .expect("original publication remains")
            .bytes(),
        bytes
    );
}

#[test]
fn postpublication_uncertainty_returns_typed_result_and_keeps_readable_bytes() {
    let root = tempfile::tempdir().expect("artifact root");
    let replace_calls = Arc::new(AtomicUsize::new(0));
    let remove_calls = Arc::new(AtomicUsize::new(0));
    let store = JsonlStore::new(
        root.path(),
        "/workspace",
        Box::new(UncertainAfterWrite {
            replace_calls: Arc::clone(&replace_calls),
            remove_calls: Arc::clone(&remove_calls),
        }),
    );
    let bytes = br#"{"v":1,"records":[],"redactions":[]}"#;
    assert!(matches!(
        store.publish(run(), artifact(bytes)),
        Ok(ArtifactPublication::PublishedDurabilityUncertain { diagnostic })
            if diagnostic.contains("post-publication uncertainty")
    ));
    assert_eq!(replace_calls.load(Ordering::SeqCst), 1);
    assert_eq!(remove_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .read(run(), RunArtifactId::sanitized_bundle())
            .expect("uncertain publication remains readable")
            .bytes(),
        bytes
    );
}
