use std::io;
use std::path::{Path, PathBuf};

use ego_core::run::RunId;
use ego_core::store::run_artifact::{
    ArtifactPublication, PublishedRunArtifact, RunArtifactId, RunArtifactStore,
    RunArtifactStoreError,
};
use ego_tools::store::{AtomicReplacePublication, JsonlStore, OsIo, StoreIo};

struct FaultIo {
    inner: OsIo,
    uncertain: bool,
}

impl StoreIo for FaultIo {
    fn append_line(&self, p: &Path, b: &[u8]) -> io::Result<()> {
        self.inner.append_line(p, b)
    }
    fn fsync(&self, p: &Path) -> io::Result<()> {
        self.inner.fsync(p)
    }
    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(p)
    }
    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.inner.exists(p)
    }
    fn replace_atomic(&self, p: &Path, b: &[u8]) -> io::Result<()> {
        self.inner.replace_atomic(p, b)
    }
    fn replace_atomic_publication(
        &self,
        p: &Path,
        b: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        self.inner.replace_atomic(p, b)?;
        if self.uncertain {
            Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
                diagnostic: "injected uncertainty".into(),
            })
        } else {
            Ok(AtomicReplacePublication::PublishedDurably)
        }
    }
    fn truncate(&self, p: &Path, n: u64) -> io::Result<()> {
        self.inner.truncate(p, n)
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        self.inner.remove_file(p)
    }
    fn list_dirs(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list_dirs(p)
    }
}

fn run(s: &str) -> RunId {
    s.parse().expect("valid run id")
}
fn artifact(bytes: &[u8]) -> PublishedRunArtifact {
    PublishedRunArtifact::output("text/plain", bytes.to_vec())
}

#[test]
fn publication_is_idempotent_replaces_changed_bytes_and_isolated_by_nonce() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    let a = run("00000000-0000-0000-0000-000000000058.00000000-0000-0000-0000-000000000001");
    let b = run("00000000-0000-0000-0000-000000000058.00000000-0000-0000-0000-000000000002");
    store.publish(a, artifact(b"one")).unwrap();
    store.publish(a, artifact(b"one")).unwrap();
    store.publish(a, artifact(b"two")).unwrap();
    assert_eq!(
        store.read(a, RunArtifactId::output()).unwrap().bytes(),
        b"two"
    );
    assert!(matches!(
        store.read(b, RunArtifactId::output()),
        Err(RunArtifactStoreError::Absent { .. })
    ));
    assert!(
        dir.path()
            .join(a.session_id().to_string())
            .join("artifacts")
            .join(a.nonce().to_string())
            .join("output.json")
            .exists()
    );
}

#[test]
fn malformed_and_wrong_artifact_records_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    let id = run("00000000-0000-0000-0000-000000000058.00000000-0000-0000-0000-000000000003");
    let path = dir
        .path()
        .join(id.session_id().to_string())
        .join("artifacts")
        .join(id.nonce().to_string())
        .join("output.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not json").unwrap();
    assert!(matches!(
        store.read(id, RunArtifactId::output()),
        Err(RunArtifactStoreError::Io { .. })
    ));
    std::fs::write(
        &path,
        br#"{"artifact_id":"other","mime_type":"text/plain","bytes":[]}"#,
    )
    .unwrap();
    assert!(matches!(
        store.read(id, RunArtifactId::output()),
        Err(RunArtifactStoreError::Conflict { .. })
    ));
}

#[test]
fn uncertain_atomic_publication_is_reported_as_typed_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(
        dir.path(),
        "/workspace",
        Box::new(FaultIo {
            inner: OsIo,
            uncertain: true,
        }),
    );
    let id = run("00000000-0000-0000-0000-000000000058.00000000-0000-0000-0000-000000000004");
    assert!(matches!(
        store.publish(id, artifact(b"bytes")),
        Ok(ArtifactPublication::PublishedDurabilityUncertain { diagnostic })
            if diagnostic.contains("uncertainty")
    ));
}
