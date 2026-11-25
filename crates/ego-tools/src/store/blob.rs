//! Transactional, content-addressed blobs bound to one durable session.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ego_core::event::id::SessionId;
use ego_core::store::blob::{
    BLOB_PAGE_BYTES, BlobError, BlobId, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter,
};
use futures::future::BoxFuture;
use sha2::{Digest as _, Sha256};

use super::io::{OsIo, StoreIo};
use super::lease::Lease;

static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);

/// The one generic immutable-blob CAS for exactly one session.
#[derive(Clone)]
pub struct SessionBlobStore {
    directory: PathBuf,
    hard_cap: u64,
    io: Arc<dyn StoreIo>,
    _lease: Option<Arc<Lease>>,
    publishes: Arc<Mutex<()>>,
}

impl SessionBlobStore {
    /// Open an unleased handle without performing destructive recovery.
    pub fn open(
        sessions_root: impl AsRef<Path>,
        session: SessionId,
        hard_cap: u64,
    ) -> Result<Self, BlobError> {
        Self::construct(
            sessions_root.as_ref().join(session.to_string()),
            hard_cap,
            Arc::new(OsIo),
            None,
        )
    }

    /// Open an unleased read handle through the enclosing session adapter's I/O.
    pub(super) fn unleased_with_io(
        sessions_root: &Path,
        session: SessionId,
        hard_cap: u64,
        io: Arc<dyn StoreIo>,
    ) -> Result<Self, BlobError> {
        Self::construct(sessions_root.join(session.to_string()), hard_cap, io, None)
    }

    /// Open through the session writer's held lease and recover stale staging.
    pub(super) fn with_io(
        session_directory: PathBuf,
        hard_cap: u64,
        io: Arc<dyn StoreIo>,
        lease: Arc<Lease>,
    ) -> Result<Self, BlobError> {
        let store = Self::construct(session_directory, hard_cap, io, Some(lease))?;
        store.recover_staging()?;
        Ok(store)
    }

    fn construct(
        session_directory: PathBuf,
        hard_cap: u64,
        io: Arc<dyn StoreIo>,
        lease: Option<Arc<Lease>>,
    ) -> Result<Self, BlobError> {
        if hard_cap == 0 {
            return Err(BlobError::Limit { limit: 0 });
        }
        // Preserve the established on-disk directory. Tool evidence and
        // attachments are semantic users of this one CAS, not two stores.
        let directory = session_directory.join("evidence");
        Ok(Self {
            directory,
            hard_cap,
            io,
            _lease: lease,
            publishes: Arc::new(Mutex::new(())),
        })
    }

    pub(super) fn mcp_output_root(&self) -> io::Result<ego_core::permit::CanonPath> {
        let directory = self
            .directory
            .parent()
            .expect("session evidence has a parent")
            .join("mcp-output");
        std::fs::create_dir_all(&directory)?;
        if std::fs::symlink_metadata(&directory)?
            .file_type()
            .is_symlink()
        {
            return Err(io::Error::other(
                "session output directory must not be a symlink",
            ));
        }
        crate::privacy::harden_private_path(
            &directory,
            crate::privacy::PrivatePathKind::Directory,
        )?;
        Ok(ego_core::permit::CanonPath::new(std::fs::canonicalize(
            directory,
        )?))
    }

    pub(super) fn spill_mcp_output(
        &self,
        run: ego_core::evidence::RunInvocationId,
        call: ego_core::event::id::ItemId,
        bytes: &[u8],
    ) -> io::Result<PathBuf> {
        let _publication = self
            .publishes
            .lock()
            .map_err(|_| io::Error::other("session publication lock poisoned"))?;
        let root = self.mcp_output_root()?;
        let target = root.as_path().join(format!("{run}-{call}.txt"));
        let existing_matches = |existing: Vec<u8>| {
            if existing == bytes {
                Ok(target.clone())
            } else {
                Err(io::Error::other(
                    "session output identity already has different bytes",
                ))
            }
        };
        if let Some(existing) = self.io.read(&target)? {
            return existing_matches(existing);
        }
        let nonce = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
        let staging = root.as_path().join(format!(
            ".{run}-{call}-{}-{nonce}.staging",
            std::process::id()
        ));
        self.io.create_new(&staging)?;
        let published = (|| {
            crate::privacy::harden_private_path(&staging, crate::privacy::PrivatePathKind::File)?;
            self.io.append_bytes(&staging, bytes)?;
            self.io.fsync(&staging)?;
            if self.io.publish_new(&staging, &target)? {
                Ok(target.clone())
            } else {
                existing_matches(
                    self.io
                        .read(&target)?
                        .ok_or_else(|| io::Error::other("published output disappeared"))?,
                )
            }
        })();
        // A successful publish consumes staging. Failed writes must not retain payloads.
        if self.io.exists(&staging).unwrap_or(false) {
            let _ = self.io.remove_file(&staging);
        }
        published
    }

    fn recover_staging(&self) -> Result<(), BlobError> {
        for path in self.io.list_files(&self.directory)? {
            if path
                .extension()
                .is_some_and(|extension| extension == "staging")
            {
                self.io.remove_file(&path)?;
            }
        }
        Ok(())
    }

    pub fn hard_cap(&self) -> u64 {
        self.hard_cap
    }

    fn blob_path(&self, id: &BlobId) -> PathBuf {
        self.directory.join(format!("{}.blob", id.as_str()))
    }

    fn staging_path(&self) -> PathBuf {
        self.directory.join(format!(
            "{}-{}.staging",
            std::process::id(),
            NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn verify(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        if reference.bytes > self.hard_cap {
            return Err(BlobError::Limit {
                limit: self.hard_cap,
            });
        }
        let capacity = usize::try_from(reference.bytes).map_err(|_| BlobError::Limit {
            limit: self.hard_cap,
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        let mut digest = Sha256::new();
        let found = self
            .io
            .visit_file(&self.blob_path(&reference.id), &mut |chunk| {
                let next = bytes
                    .len()
                    .checked_add(chunk.len())
                    .ok_or_else(|| io::Error::other("blob length overflow"))?;
                if next as u64 > self.hard_cap {
                    return Err(io::Error::other("blob exceeds its store hard cap"));
                }
                digest.update(chunk);
                bytes.extend_from_slice(chunk);
                Ok(())
            })?;
        if !found {
            return Err(BlobError::NotFound {
                reference: reference.id.clone(),
            });
        }
        let length = bytes.len() as u64;
        if length != reference.bytes {
            return Err(BlobError::WrongLength {
                expected: reference.bytes,
                found: length,
            });
        }
        let found_id = blob_id(digest.finalize());
        if found_id != reference.id {
            return Err(BlobError::Corrupt {
                expected: reference.id.clone(),
                found: found_id,
            });
        }
        Ok(bytes)
    }

    fn verify_and_page(&self, request: &BlobRead) -> Result<BlobPage, BlobError> {
        let complete = self.verify(&request.reference)?;
        let start = usize::try_from(request.offset).map_err(|_| BlobError::InvalidOffset {
            offset: request.offset,
            bytes: request.reference.bytes,
        })?;
        let end = start.saturating_add(BLOB_PAGE_BYTES).min(complete.len());
        BlobPage::new(
            request.reference.clone(),
            request.offset,
            complete[start..end].to_vec(),
        )
    }
}

impl BlobStore for SessionBlobStore {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        let staging = self.staging_path();
        self.io.create_new(&staging)?;
        Ok(Box::new(SessionBlobWriter {
            store: self.clone(),
            staging: Some(staging),
            digest: Sha256::new(),
            bytes: 0,
            failure: None,
        }))
    }

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        self.verify(reference)
    }

    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        Box::pin(async move { self.verify_and_page(request) })
    }

    fn collect_garbage(&self, reachable: &[BlobRef]) -> Result<usize, BlobError> {
        for reference in reachable {
            self.verify(reference)?;
        }
        let mut reference_counts: HashMap<&str, usize> = HashMap::new();
        for reference in reachable {
            *reference_counts.entry(reference.id.as_str()).or_default() += 1;
        }
        // A cold blob is `<id>.blob.zst`; a crash can leave both forms. One
        // removal of the plain path drops both, so each id counts once.
        let mut unreachable = std::collections::BTreeSet::new();
        for path in self.io.list_files(&self.directory)? {
            let Some(stem) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| {
                    name.strip_suffix(".blob")
                        .or_else(|| name.strip_suffix(".blob.zst"))
                })
            else {
                continue;
            };
            if BlobId::parse(stem).is_err() || reference_counts.contains_key(stem) {
                continue;
            }
            unreachable.insert(self.directory.join(format!("{stem}.blob")));
        }
        let removed = unreachable.len();
        for path in unreachable {
            self.io.remove_file(&path)?;
        }
        Ok(removed)
    }
}

struct SessionBlobWriter {
    store: SessionBlobStore,
    staging: Option<PathBuf>,
    digest: Sha256,
    bytes: u64,
    failure: Option<BlobError>,
}

impl SessionBlobWriter {
    fn cleanup(&mut self) -> Result<(), BlobError> {
        if let Some(path) = self.staging.take() {
            self.store.io.remove_file(&path)?;
        }
        Ok(())
    }

    fn commit_inner(&mut self) -> Result<BlobRef, BlobError> {
        if let Some(error) = self.failure.clone() {
            self.cleanup()?;
            return Err(error);
        }
        let staging = self
            .staging
            .as_ref()
            .expect("a blob transaction is consumed exactly once")
            .clone();
        self.store.io.fsync(&staging)?;
        let reference = BlobRef::new(blob_id(self.digest.clone().finalize()), self.bytes);

        let mut staged_digest = Sha256::new();
        let mut staged_bytes = 0_u64;
        self.store.io.visit_file(&staging, &mut |chunk| {
            staged_digest.update(chunk);
            staged_bytes = staged_bytes.saturating_add(chunk.len() as u64);
            Ok(())
        })?;
        if staged_bytes != reference.bytes {
            self.cleanup()?;
            return Err(BlobError::WrongLength {
                expected: reference.bytes,
                found: staged_bytes,
            });
        }
        let staged_id = blob_id(staged_digest.finalize());
        if staged_id != reference.id {
            self.cleanup()?;
            return Err(BlobError::Corrupt {
                expected: reference.id.clone(),
                found: staged_id,
            });
        }

        let store = self.store.clone();
        let _publish = store.publishes.lock().map_err(|_| BlobError::Io {
            message: "blob publish lock was poisoned".to_string(),
        })?;
        let target = store.blob_path(&reference.id);
        if store.io.exists(&target)? {
            store.verify(&reference)?;
            self.cleanup()?;
            return Ok(reference);
        }
        if !store.io.publish_new(&staging, &target)? {
            store.verify(&reference)?;
            self.cleanup()?;
            return Ok(reference);
        }
        self.staging = None;
        store.verify(&reference)?;
        Ok(reference)
    }
}

impl BlobWriter for SessionBlobWriter {
    fn append(&mut self, chunk: &[u8]) -> Result<(), BlobError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let next = self.bytes.saturating_add(chunk.len() as u64);
        if next > self.store.hard_cap {
            let error = BlobError::Limit {
                limit: self.store.hard_cap,
            };
            self.failure = Some(error.clone());
            return Err(error);
        }
        let path = self
            .staging
            .as_ref()
            .expect("a blob transaction is consumed exactly once");
        if let Err(error) = self.store.io.append_bytes(path, chunk) {
            let error = BlobError::from(error);
            self.failure = Some(error.clone());
            return Err(error);
        }
        self.digest.update(chunk);
        self.bytes = next;
        Ok(())
    }

    fn commit(mut self: Box<Self>) -> Result<BlobRef, BlobError> {
        let result = self.commit_inner();
        if result.is_err() {
            let _ = self.cleanup();
        }
        result
    }

    fn abort(mut self: Box<Self>) -> Result<(), BlobError> {
        self.cleanup()
    }
}

impl Drop for SessionBlobWriter {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn blob_id(digest: impl AsRef<[u8]>) -> BlobId {
    let mut text = String::with_capacity(64);
    for byte in digest.as_ref() {
        use std::fmt::Write as _;
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    BlobId::parse(text).expect("SHA-256 is a lowercase 64-character blob id")
}
