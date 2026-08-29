//! Tool-evidence semantics over the session's generic blob CAS.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ego_core::event::id::SessionId;
use ego_core::store::blob::{BlobError, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter};
use ego_core::store::evidence::{
    EvidenceError, EvidenceId, EvidencePage, EvidenceRead, EvidenceRef, EvidenceStore,
    EvidenceWriter,
};
use futures::future::BoxFuture;

use super::blob::SessionBlobStore;
use super::io::StoreIo;
use super::lease::Lease;

/// Tool evidence bound to the same generic session CAS as every other blob.
#[derive(Clone)]
pub struct SessionEvidenceStore {
    blobs: SessionBlobStore,
}

impl SessionEvidenceStore {
    pub fn open(
        sessions_root: impl AsRef<Path>,
        session: SessionId,
        hard_cap: u64,
    ) -> Result<Self, EvidenceError> {
        Ok(Self {
            blobs: SessionBlobStore::open(sessions_root, session, hard_cap)
                .map_err(evidence_error)?,
        })
    }

    pub(super) fn with_io(
        session_directory: PathBuf,
        hard_cap: u64,
        io: Arc<dyn StoreIo>,
        lease: Arc<Lease>,
    ) -> Result<Self, EvidenceError> {
        Ok(Self {
            blobs: SessionBlobStore::with_io(session_directory, hard_cap, io, lease)
                .map_err(evidence_error)?,
        })
    }

    pub fn hard_cap(&self) -> u64 {
        self.blobs.hard_cap()
    }

    pub fn blob_store(&self) -> &SessionBlobStore {
        &self.blobs
    }

    pub(super) fn collect_garbage(&self, reachable: &[BlobRef]) -> Result<usize, EvidenceError> {
        self.blobs
            .collect_garbage(reachable)
            .map_err(evidence_error)
    }
}

impl BlobStore for SessionEvidenceStore {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        self.blobs.begin()
    }

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        self.blobs.read(reference)
    }

    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        self.blobs.read_page(request)
    }

    fn collect_garbage(&self, reachable: &[BlobRef]) -> Result<usize, BlobError> {
        self.blobs.collect_garbage(reachable)
    }
}

impl EvidenceStore for SessionEvidenceStore {
    fn blob_store(&self) -> &dyn BlobStore {
        &self.blobs
    }

    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Ok(Box::new(EvidenceBlobWriter {
            inner: self.blobs.begin().map_err(evidence_error)?,
        }))
    }

    fn read_page<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(async move {
            let blob_request = BlobRead::new(request.reference.blob_ref(), request.offset)
                .map_err(evidence_error)?;
            let page = self
                .blobs
                .read_page(&blob_request)
                .await
                .map_err(evidence_error)?;
            EvidencePage::new(
                EvidenceRef::from_blob(page.reference),
                page.offset,
                page.bytes,
            )
        })
    }
}

struct EvidenceBlobWriter {
    inner: Box<dyn BlobWriter>,
}

impl EvidenceWriter for EvidenceBlobWriter {
    fn append(&mut self, chunk: &[u8]) -> Result<(), EvidenceError> {
        self.inner.append(chunk).map_err(evidence_error)
    }

    fn commit(self: Box<Self>) -> Result<EvidenceRef, EvidenceError> {
        self.inner
            .commit()
            .map(EvidenceRef::from_blob)
            .map_err(evidence_error)
    }

    fn abort(self: Box<Self>) -> Result<(), EvidenceError> {
        self.inner.abort().map_err(evidence_error)
    }
}

fn evidence_error(error: BlobError) -> EvidenceError {
    match error {
        BlobError::InvalidReference { reference } => EvidenceError::InvalidReference { reference },
        BlobError::NotFound { reference } => EvidenceError::NotFound {
            reference: EvidenceId::parse(reference.as_str())
                .expect("a blob id is already a validated SHA-256"),
        },
        BlobError::Corrupt { expected, found } => EvidenceError::Corrupt {
            expected: EvidenceId::parse(expected.as_str())
                .expect("a blob id is already a validated SHA-256"),
            found: EvidenceId::parse(found.as_str())
                .expect("a blob id is already a validated SHA-256"),
        },
        BlobError::WrongLength { expected, found } => {
            EvidenceError::WrongLength { expected, found }
        }
        BlobError::InvalidOffset { offset, bytes } => {
            EvidenceError::InvalidOffset { offset, bytes }
        }
        BlobError::PageTooLarge { bytes } => EvidenceError::PageTooLarge { bytes },
        BlobError::Limit { limit } => EvidenceError::Limit { limit },
        BlobError::Io { message } => EvidenceError::Io { message },
    }
}
