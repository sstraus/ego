//! contract for the opaque durable cache boundary.

use ego_core::event::time::Timestamp;
use ego_tools::model_catalogue_cache::{CacheError, ModelCatalogueCache};
use ego_tools::store::StoreIo;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn cache_exposes_only_the_exact_machine_paths_and_opaque_bytes() {
    let io = MemoryIo::default();
    let home = tempfile::tempdir().unwrap();
    let cache = ModelCatalogueCache::new(&io, home.path());
    assert_eq!(
        cache.snapshot_path(),
        home.path().join("model-catalogue/snapshot-v1.json")
    );
    assert_eq!(
        cache.refresh_lock_path(),
        home.path().join("model-catalogue/refresh.lock")
    );
    let bytes = br#"{"v":1,"providers":{"ollama":{"opaque":true}}}"#;
    let lease = cache
        .lease(Timestamp::from_millis(1), std::process::id(), &|pid| {
            pid == std::process::id()
        })
        .unwrap();
    cache.publish_under_lease(&lease, bytes).unwrap();
    assert_eq!(cache.read_bytes().unwrap(), Some(bytes.to_vec()));
}

#[test]
fn lease_serializes_refresh_and_is_held_across_baseline_read_and_publish() {
    let io = MemoryIo::default();
    let home = tempfile::tempdir().unwrap();
    let cache = ModelCatalogueCache::new(&io, home.path());
    let lease = cache
        .lease(Timestamp::from_millis(1), std::process::id(), &|pid| {
            pid == std::process::id()
        })
        .unwrap();
    assert!(matches!(
        cache.lease(Timestamp::from_millis(1), std::process::id() + 1, &|_| true,),
        Err(CacheError::Held { .. })
    ));
    cache.publish_under_lease(&lease, b"new-bytes").unwrap();
    assert_eq!(cache.read_bytes().unwrap(), Some(b"new-bytes".to_vec()));
}

#[test]
fn a_lease_from_another_cache_path_cannot_publish() {
    let io = MemoryIo::default();
    let home_a = tempfile::tempdir().unwrap();
    let home_b = tempfile::tempdir().unwrap();
    let cache_a = ModelCatalogueCache::new(&io, home_a.path());
    let cache_b = ModelCatalogueCache::new(&io, home_b.path());
    let lease_a = cache_a
        .lease(Timestamp::from_millis(1), std::process::id(), &|pid| {
            pid == std::process::id()
        })
        .unwrap();

    let error = cache_b
        .publish_under_lease(&lease_a, b"must-not-publish")
        .unwrap_err();
    assert!(matches!(error, CacheError::WrongLease { .. }));
    assert_eq!(cache_b.read_bytes().unwrap(), None);
}

#[test]
fn failed_atomic_publish_preserves_the_previous_opaque_bytes() {
    let io = MemoryIo::default();
    let home = tempfile::tempdir().unwrap();
    let cache = ModelCatalogueCache::new(&io, home.path());
    let lease = cache
        .lease(Timestamp::from_millis(1), std::process::id(), &|pid| {
            pid == std::process::id()
        })
        .unwrap();
    cache.publish_under_lease(&lease, b"old-bytes").unwrap();
    io.fail_replace.store(true, Ordering::SeqCst);
    let error = cache.publish_under_lease(&lease, b"new-bytes").unwrap_err();
    assert!(matches!(error, CacheError::Io(_)));
    assert_eq!(cache.read_bytes().unwrap(), Some(b"old-bytes".to_vec()));
}

#[derive(Default)]
struct MemoryIo {
    files: std::sync::Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    fail_replace: AtomicBool,
}

impl StoreIo for MemoryIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        let bytes = files.entry(path.to_path_buf()).or_default();
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
        Ok(())
    }
    fn fsync(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        Ok(self.files.lock().unwrap().get(path).cloned())
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        Ok(self.files.lock().unwrap().contains_key(path))
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if self.fail_replace.load(Ordering::SeqCst) {
            return Err(io::Error::other("atomic replace failed"));
        }
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        if let Some(bytes) = self.files.lock().unwrap().get_mut(path) {
            bytes.truncate(len as usize);
        }
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }
    fn list_dirs(&self, _: &Path) -> io::Result<Vec<std::path::PathBuf>> {
        Ok(Vec::new())
    }
}
