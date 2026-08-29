//! Helpers and fakes shared by more than one integration test in this crate.
//!
//! Lives under `tests/common/` rather than `tests/` so cargo treats it as a
//! module to include, not as a test binary of its own.
//!
//! `#![allow(dead_code)]` because Rust compiles this module separately into
//! every test binary that declares `mod common;`, so anything only one of them
//! uses is dead in the others. That is a property of the test harness, not a
//! sign of unused code.
#![allow(dead_code)]

use ego_core::store::blob::{BlobError, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter};
use futures::future::BoxFuture;

/// A no-op blob backend for tests whose contract does not use attachments.
pub static EMPTY_BLOB_STORE: EmptyBlobStore = EmptyBlobStore;

pub struct EmptyBlobStore;

impl BlobStore for EmptyBlobStore {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        Err(BlobError::Limit { limit: 0 })
    }

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Err(BlobError::NotFound {
            reference: reference.id.clone(),
        })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        Box::pin(futures::future::ready(Err(BlobError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }

    fn collect_garbage(&self, _reachable: &[BlobRef]) -> Result<usize, BlobError> {
        Ok(0)
    }
}

/// Deserialise a fixed UUID literal into any of the id newtypes, via their
/// already-derived (and already-public) `Deserialize` impl. This gets
/// deterministic, golden-comparable ids without adding a constructor to
/// `event::id` (out of scope: owned by another story).
pub fn id<T: serde::de::DeserializeOwned>(uuid: &str) -> T {
    serde_json::from_str(&format!("{uuid:?}")).unwrap()
}
