//! Transactional, content-addressed evidence bound to one durable session.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ego_core::event::id::SessionId;
use ego_core::store::evidence::{
    EVIDENCE_PAGE_BYTES, EvidenceError, EvidenceId, EvidencePage, EvidenceRead, EvidenceRef,
    EvidenceStore, EvidenceWriter,
};
use futures::future::BoxFuture;
use sha2::{Digest as _, Sha256};

use super::io::{OsIo, StoreIo};
use super::lease::Lease;

static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);

/// Complete evidence for exactly one session.
#[derive(Clone)]
pub struct SessionEvidenceStore {
    directory: PathBuf,
    hard_cap: u64,
    io: Arc<dyn StoreIo>,
    _lease: Option<Arc<Lease>>,
    publishes: Arc<Mutex<()>>,
}

impl SessionEvidenceStore {
    pub fn open(
        sessions_root: impl AsRef<Path>,
        session: SessionId,
        hard_cap: u64,
    ) -> Result<Self, EvidenceError> {
        Self::with_io(
            sessions_root.as_ref().join(session.to_string()),
            hard_cap,
            Arc::new(OsIo),
            None,
        )
    }

    pub(super) fn with_io(
        session_directory: PathBuf,
        hard_cap: u64,
        io: Arc<dyn StoreIo>,
        lease: Option<Arc<Lease>>,
    ) -> Result<Self, EvidenceError> {
        if hard_cap == 0 {
            return Err(EvidenceError::Limit { limit: 0 });
        }
        let directory = session_directory.join("evidence");
        for path in io.list_files(&directory)? {
            if path
                .extension()
                .is_some_and(|extension| extension == "staging")
            {
                io.remove_file(&path)?;
            }
        }
        Ok(Self {
            directory,
            hard_cap,
            io,
            _lease: lease,
            publishes: Arc::new(Mutex::new(())),
        })
    }

    pub fn hard_cap(&self) -> u64 {
        self.hard_cap
    }

    fn blob_path(&self, id: &EvidenceId) -> PathBuf {
        self.directory.join(format!("{}.blob", id.as_str()))
    }

    fn staging_path(&self) -> PathBuf {
        self.directory.join(format!(
            "{}-{}.staging",
            std::process::id(),
            NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn verify_and_page(&self, request: &EvidenceRead) -> Result<EvidencePage, EvidenceError> {
        if request.reference.bytes > self.hard_cap {
            return Err(EvidenceError::Limit {
                limit: self.hard_cap,
            });
        }
        let mut digest = Sha256::new();
        let mut length = 0_u64;
        let mut page = Vec::with_capacity(EVIDENCE_PAGE_BYTES);
        let page_start = request.offset;
        let page_end = page_start
            .saturating_add(EVIDENCE_PAGE_BYTES as u64)
            .min(request.reference.bytes);
        let found = self
            .io
            .visit_file(&self.blob_path(&request.reference.id), &mut |chunk| {
                digest.update(chunk);
                let chunk_start = length;
                length = length.saturating_add(chunk.len() as u64);
                let chunk_end = length;
                let copy_start = chunk_start.max(page_start);
                let copy_end = chunk_end.min(page_end);
                if copy_start < copy_end {
                    let from = usize::try_from(copy_start - chunk_start)
                        .map_err(|_| io::Error::other("evidence page offset overflow"))?;
                    let to = usize::try_from(copy_end - chunk_start)
                        .map_err(|_| io::Error::other("evidence page offset overflow"))?;
                    page.extend_from_slice(&chunk[from..to]);
                }
                Ok(())
            })?;
        if !found {
            return Err(EvidenceError::NotFound {
                reference: request.reference.id.clone(),
            });
        }
        if length != request.reference.bytes {
            return Err(EvidenceError::WrongLength {
                expected: request.reference.bytes,
                found: length,
            });
        }
        let found_id = evidence_id(digest.finalize());
        if found_id != request.reference.id {
            return Err(EvidenceError::Corrupt {
                expected: request.reference.id.clone(),
                found: found_id,
            });
        }
        EvidencePage::new(request.reference.clone(), request.offset, page)
    }

    fn verify_complete(&self, reference: &EvidenceRef) -> Result<(), EvidenceError> {
        self.verify_and_page(&EvidenceRead::new(reference.clone(), reference.bytes)?)?;
        Ok(())
    }
}

impl EvidenceStore for SessionEvidenceStore {
    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        let staging = self.staging_path();
        self.io.create_new(&staging)?;
        Ok(Box::new(SessionEvidenceWriter {
            store: self.clone(),
            staging: Some(staging),
            digest: Sha256::new(),
            bytes: 0,
            failure: None,
        }))
    }

    fn read_page<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(async move { self.verify_and_page(request) })
    }
}

struct SessionEvidenceWriter {
    store: SessionEvidenceStore,
    staging: Option<PathBuf>,
    digest: Sha256,
    bytes: u64,
    failure: Option<EvidenceError>,
}

impl SessionEvidenceWriter {
    fn cleanup(&mut self) -> Result<(), EvidenceError> {
        if let Some(path) = self.staging.take() {
            self.store.io.remove_file(&path)?;
        }
        Ok(())
    }

    fn commit_inner(&mut self) -> Result<EvidenceRef, EvidenceError> {
        if let Some(error) = self.failure.clone() {
            self.cleanup()?;
            return Err(error);
        }
        let staging = self
            .staging
            .as_ref()
            .expect("an evidence transaction is consumed exactly once")
            .clone();
        self.store.io.fsync(&staging)?;
        let reference = EvidenceRef::new(evidence_id(self.digest.clone().finalize()), self.bytes);

        let mut staged_digest = Sha256::new();
        let mut staged_bytes = 0_u64;
        self.store.io.visit_file(&staging, &mut |chunk| {
            staged_digest.update(chunk);
            staged_bytes = staged_bytes.saturating_add(chunk.len() as u64);
            Ok(())
        })?;
        if staged_bytes != reference.bytes {
            self.cleanup()?;
            return Err(EvidenceError::WrongLength {
                expected: reference.bytes,
                found: staged_bytes,
            });
        }
        let staged_id = evidence_id(staged_digest.finalize());
        if staged_id != reference.id {
            self.cleanup()?;
            return Err(EvidenceError::Corrupt {
                expected: reference.id.clone(),
                found: staged_id,
            });
        }

        let store = self.store.clone();
        let _publish = store.publishes.lock().map_err(|_| EvidenceError::Io {
            message: "tool evidence publish lock was poisoned".to_string(),
        })?;
        let target = store.blob_path(&reference.id);
        if store.io.exists(&target)? {
            store.verify_complete(&reference)?;
            self.cleanup()?;
            return Ok(reference);
        }
        if !store.io.publish_new(&staging, &target)? {
            store.verify_complete(&reference)?;
            self.cleanup()?;
            return Ok(reference);
        }
        self.staging = None;
        store.verify_complete(&reference)?;
        Ok(reference)
    }
}

impl EvidenceWriter for SessionEvidenceWriter {
    fn append(&mut self, chunk: &[u8]) -> Result<(), EvidenceError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let next = self.bytes.saturating_add(chunk.len() as u64);
        if next > self.store.hard_cap {
            let error = EvidenceError::Limit {
                limit: self.store.hard_cap,
            };
            self.failure = Some(error.clone());
            return Err(error);
        }
        let path = self
            .staging
            .as_ref()
            .expect("an evidence transaction is consumed exactly once");
        if let Err(error) = self.store.io.append_bytes(path, chunk) {
            let error = EvidenceError::from(error);
            self.failure = Some(error.clone());
            return Err(error);
        }
        self.digest.update(chunk);
        self.bytes = next;
        Ok(())
    }

    fn commit(mut self: Box<Self>) -> Result<EvidenceRef, EvidenceError> {
        let result = self.commit_inner();
        if result.is_err() {
            let _ = self.cleanup();
        }
        result
    }

    fn abort(mut self: Box<Self>) -> Result<(), EvidenceError> {
        self.cleanup()
    }
}

impl Drop for SessionEvidenceWriter {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn evidence_id(digest: impl AsRef<[u8]>) -> EvidenceId {
    let mut text = String::with_capacity(64);
    for byte in digest.as_ref() {
        use std::fmt::Write as _;
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    EvidenceId::parse(text).expect("SHA-256 is a lowercase 64-character evidence id")
}
