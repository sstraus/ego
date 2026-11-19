//! User-local advisory opinions, isolated from workspace and session storage.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use ego_core::permit::analysis::{AnalysisLease, AnalysisOpinion, PermissionAnalysisCache};
use futures::future::BoxFuture;
use futures::lock::{Mutex as AsyncMutex, OwnedMutexGuard};
use serde::{Deserialize, Serialize};

use crate::privacy::{PrivatePathKind, harden_private_path, validate_private_path};
use crate::store::{Lease, OsIo, StoreIo};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    key: String,
    opinion: AnalysisOpinion,
}

/// Entry order is insertion age; reads never update the document.
pub struct FilePermissionAnalysisCache {
    directory: PathBuf,
    io: Arc<dyn StoreIo>,
}

impl FilePermissionAnalysisCache {
    pub fn new(home: &Path) -> Self {
        Self::with_io(home, Arc::new(OsIo))
    }

    pub fn with_io(home: &Path, io: Arc<dyn StoreIo>) -> Self {
        Self {
            directory: home.join("permission-analysis"),
            io,
        }
    }

    fn read(&self) -> Result<Document, String> {
        let path = self.directory.join("v1.json");
        match fs::symlink_metadata(&self.directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty()),
            Err(_) => return Err("permission analysis directory cannot be read".into()),
            Ok(_) => private(&self.directory, true)?,
        }
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty()),
            Err(_) => return Err("permission analysis document cannot be read".into()),
            Ok(_) => private(&path, false)?,
        }
        let mut bytes = Vec::new();
        self.io
            .visit_file(&path, &mut |chunk| {
                if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "analysis cache size limit",
                    ));
                }
                bytes.extend_from_slice(chunk);
                Ok(())
            })
            .map_err(|_| "permission analysis document read failed".to_owned())?;
        let document: Document = serde_json::from_slice(&bytes)
            .map_err(|_| "permission analysis document is malformed".to_owned())?;
        if document.version != 1 || document.entries.len() > MAX_ENTRIES {
            return Err("permission analysis document version or size is unsupported".into());
        }
        let mut keys = HashSet::new();
        if document
            .entries
            .iter()
            .any(|entry| !valid_key(&entry.key) || !keys.insert(&entry.key))
        {
            return Err("permission analysis document has invalid or duplicate keys".into());
        }
        Ok(document)
    }

    fn insert(&self, key: String, opinion: AnalysisOpinion) -> Result<(), String> {
        if !self.directory.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(&self.directory)
                .map_err(|_| "permission analysis directory creation failed".to_owned())?;
            harden_private_path(&self.directory, PrivatePathKind::Directory)
                .map_err(|_| "permission analysis directory hardening failed".to_owned())?;
        }
        private(&self.directory, true)?;
        // Kernel lease exists only during reload, merge and atomic publication.
        let _lease = Lease::probe(&self.directory.join("write.lock"))
            .map_err(|_| "permission analysis write lease failed".to_owned())?
            .ok_or_else(|| "permission analysis writer is busy".to_owned())?;
        let mut document = self.read()?;
        if document.entries.iter().any(|entry| entry.key == key) {
            return Ok(());
        }
        if document.entries.len() == MAX_ENTRIES {
            document.entries.remove(0);
        }
        document.entries.push(Entry { key, opinion });
        let bytes = serde_json::to_vec(&document)
            .map_err(|_| "permission analysis encoding failed".to_owned())?;
        if bytes.len() > MAX_BYTES {
            return Err("permission analysis encoded size exceeded limit".into());
        }
        self.io
            .replace_atomic_private(&self.directory.join("v1.json"), &bytes)
            .map_err(|_| "permission analysis atomic publication failed".into())
    }
}

fn empty() -> Document {
    Document {
        version: 1,
        entries: Vec::new(),
    }
}
fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn private(path: &Path, directory: bool) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "permission analysis metadata cannot be read".to_owned())?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err("permission analysis path has an unexpected file type".into());
    }
    let kind = if directory {
        PrivatePathKind::Directory
    } else {
        PrivatePathKind::File
    };
    validate_private_path(path, kind)
        .map_err(|_| "permission analysis path permissions are not private".into())
}

type LocalLocks = HashMap<(PathBuf, [u8; 32]), Weak<AsyncMutex<()>>>;

impl PermissionAnalysisCache for FilePermissionAnalysisCache {
    fn acquire(&self, key: [u8; 32]) -> BoxFuture<'_, Result<Box<dyn AnalysisLease + '_>, String>> {
        Box::pin(async move {
            static LOCKS: OnceLock<Mutex<LocalLocks>> = OnceLock::new();
            let lock = {
                let mut locks = LOCKS
                    .get_or_init(Mutex::default)
                    .lock()
                    .map_err(|_| "permission analysis local coordination failed".to_owned())?;
                locks.retain(|_, lock| lock.strong_count() > 0);
                let slot = locks.entry((self.directory.clone(), key)).or_default();
                match slot.upgrade() {
                    Some(lock) => lock,
                    None => {
                        let lock = Arc::new(AsyncMutex::new(()));
                        *slot = Arc::downgrade(&lock);
                        lock
                    }
                }
            };
            let guard = lock.lock_owned().await;
            let key = key.iter().map(|byte| format!("{byte:02x}")).collect();
            Ok(Box::new(LocalLease {
                cache: self,
                key,
                _guard: guard,
            }) as Box<dyn AnalysisLease>)
        })
    }
}

struct LocalLease<'a> {
    cache: &'a FilePermissionAnalysisCache,
    key: String,
    _guard: OwnedMutexGuard<()>,
}
impl AnalysisLease for LocalLease<'_> {
    fn lookup(&self) -> Result<Option<AnalysisOpinion>, String> {
        Ok(self
            .cache
            .read()?
            .entries
            .into_iter()
            .find(|entry| entry.key == self.key)
            .map(|entry| entry.opinion))
    }
    fn insert(&self, opinion: AnalysisOpinion) -> Result<(), String> {
        self.cache.insert(self.key.clone(), opinion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    #[test]
    fn reuse_across_handles_does_not_write_on_hit_or_store_raw_facts() {
        let home = tempfile::tempdir().unwrap();
        let first = FilePermissionAnalysisCache::new(home.path());
        {
            let lease = block_on(first.acquire([7; 32])).unwrap();
            assert_eq!(lease.lookup().unwrap(), None);
            lease.insert(AnalysisOpinion::Allow).unwrap();
        }
        let path = home.path().join("permission-analysis/v1.json");
        let before = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let second = FilePermissionAnalysisCache::new(home.path());
        let lease = block_on(second.acquire([7; 32])).unwrap();
        assert_eq!(lease.lookup().unwrap(), Some(AnalysisOpinion::Allow));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        let document: serde_json::Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(document["entries"][0].as_object().unwrap().len(), 2);
        private(&path, false).unwrap();
        private(path.parent().unwrap(), true).unwrap();
    }

    #[test]
    fn corrupted_or_foreign_version_cache_is_never_overwritten_or_authoritative() {
        let home = tempfile::tempdir().unwrap();
        let cache = FilePermissionAnalysisCache::new(home.path());
        let lease = block_on(cache.acquire([1; 32])).unwrap();
        lease.insert(AnalysisOpinion::Escalate).unwrap();
        let path = cache.directory.join("v1.json");
        for invalid in [
            b"{".as_slice(),
            b"{\"version\":2,\"entries\":[]}",
            b"{\"version\":1,\"entries\":[],\"extra\":true}",
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(lease.lookup().is_err());
            assert!(lease.insert(AnalysisOpinion::Allow).is_err());
            assert_eq!(fs::read(&path).unwrap(), invalid);
        }
    }

    #[test]
    fn insertion_age_evicts_without_refreshing_hits() {
        let home = tempfile::tempdir().unwrap();
        let cache = FilePermissionAnalysisCache::new(home.path());
        let entries = (0..MAX_ENTRIES)
            .map(|n| Entry {
                key: format!("{n:064x}"),
                opinion: AnalysisOpinion::Escalate,
            })
            .collect();
        cache
            .insert(format!("{:064x}", 0), AnalysisOpinion::Escalate)
            .unwrap();
        let path = cache.directory.join("v1.json");
        fs::write(
            &path,
            serde_json::to_vec(&Document {
                version: 1,
                entries,
            })
            .unwrap(),
        )
        .unwrap();
        cache
            .insert(format!("{:064x}", MAX_ENTRIES), AnalysisOpinion::Deny)
            .unwrap();
        let document = cache.read().unwrap();
        assert_eq!(document.entries.len(), MAX_ENTRIES);
        assert_eq!(document.entries[0].key, format!("{:064x}", 1));
        assert!(fs::metadata(path).unwrap().len() < MAX_BYTES as u64);
    }

    #[test]
    fn local_same_key_coalesces_without_holding_a_disk_lease_during_generation() {
        use std::task::{Context, Poll};
        let home = tempfile::tempdir().unwrap();
        let first = FilePermissionAnalysisCache::new(home.path());
        let second = FilePermissionAnalysisCache::new(home.path());
        let lease = block_on(first.acquire([1; 32])).unwrap();
        let mut same = second.acquire([1; 32]);
        let waker = futures::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(same.as_mut().poll(&mut context), Poll::Pending));
        {
            let other = block_on(second.acquire([2; 32])).unwrap();
            other.insert(AnalysisOpinion::Deny).unwrap();
        }
        lease.insert(AnalysisOpinion::Allow).unwrap();
        drop(lease);
        let same = block_on(same).unwrap();
        assert_eq!(same.lookup().unwrap(), Some(AnalysisOpinion::Allow));
        assert_eq!(first.read().unwrap().entries.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_permissions_and_symlinks_are_misses_and_preserve_the_document() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let home = tempfile::tempdir().unwrap();
        let cache = FilePermissionAnalysisCache::new(home.path());
        let lease = block_on(cache.acquire([1; 32])).unwrap();
        lease.insert(AnalysisOpinion::Allow).unwrap();
        let path = cache.directory.join("v1.json");
        let bytes = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(lease.lookup().is_err());
        assert!(lease.insert(AnalysisOpinion::Deny).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let target = home.path().join("target");
        fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(lease.lookup().is_err());
        assert!(lease.insert(AnalysisOpinion::Deny).is_err());
        assert_eq!(fs::read(&target).unwrap(), bytes);
    }

    #[test]
    fn failed_atomic_publication_preserves_prior_document() {
        let home = tempfile::tempdir().unwrap();
        let cache = FilePermissionAnalysisCache::new(home.path());
        cache
            .insert(format!("{:064x}", 1), AnalysisOpinion::Escalate)
            .unwrap();
        let path = cache.directory.join("v1.json");
        let bytes = fs::read(&path).unwrap();
        // An existing staging name prevents create_new before any prior bytes change.
        fs::create_dir(cache.directory.join("v1.json.tmp")).unwrap();
        assert!(
            cache
                .insert(format!("{:064x}", 2), AnalysisOpinion::Allow)
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(cache.read().unwrap().entries.len(), 1);
    }
}
