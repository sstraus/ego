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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ego_core::store::blob::{
    BlobError, BlobId, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter,
};
use futures::future::BoxFuture;

use ego_core::config::{Mode, Ruleset};
use ego_core::permit::{CanonPath, PolicyEngine};
use ego_core::turn::{GenerationPolicyCapture, GenerationPolicySnapshot, GenerationPolicySource};

struct FixedPolicySource {
    root: CanonPath,
    mode: Mode,
    rules: Ruleset,
}

impl GenerationPolicySource for FixedPolicySource {
    fn current(&self) -> Result<Arc<PolicyEngine>, String> {
        Ok(Arc::new(PolicyEngine::new(
            self.root.clone(),
            self.mode,
            self.rules.clone(),
        )))
    }

    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::new(self.root.clone(), self.mode, self.rules.clone(), None)
            .map_err(|error| error.to_string())
    }

    fn restore(
        &self,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(snapshot, None).map_err(|error| error.to_string())
    }
}

pub fn default_policy_source() -> Arc<dyn GenerationPolicySource> {
    policy_source(Mode::Default)
}

pub fn policy_source(mode: Mode) -> Arc<dyn GenerationPolicySource> {
    policy_source_with_rules(mode, Ruleset::default())
}

pub fn policy_source_with_rules(mode: Mode, rules: Ruleset) -> Arc<dyn GenerationPolicySource> {
    policy_source_with_root("/ws", mode, rules)
}

pub fn policy_source_with_root(
    root: &str,
    mode: Mode,
    rules: Ruleset,
) -> Arc<dyn GenerationPolicySource> {
    Arc::new(FixedPolicySource {
        root: CanonPath::new(root),
        mode,
        rules,
    })
}

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

/// A small writable content-addressed blob backend for integration tests that
/// exercise durable admission. It intentionally implements only the generic
/// blob boundary; semantic evidence remains owned by each test's wrapper.
type MemoryBlobMap = Arc<Mutex<BTreeMap<String, (BlobRef, Vec<u8>)>>>;

#[derive(Clone, Default)]
pub struct MemoryBlobStore {
    blobs: MemoryBlobMap,
}

struct MemoryBlobWriter {
    owner: MemoryBlobStore,
    bytes: Vec<u8>,
}

impl BlobWriter for MemoryBlobWriter {
    fn append(&mut self, chunk: &[u8]) -> Result<(), BlobError> {
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<BlobRef, BlobError> {
        let id = BlobId::parse(ego_core::digest::sha256_hex(&self.bytes))?;
        let bytes =
            u64::try_from(self.bytes.len()).map_err(|_| BlobError::Limit { limit: u64::MAX })?;
        let reference = BlobRef::new(id, bytes);
        self.owner
            .blobs
            .lock()
            .expect("memory blob store")
            .insert(reference.id.to_string(), (reference.clone(), self.bytes));
        Ok(reference)
    }

    fn abort(self: Box<Self>) -> Result<(), BlobError> {
        Ok(())
    }
}

impl BlobStore for MemoryBlobStore {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        Ok(Box::new(MemoryBlobWriter {
            owner: self.clone(),
            bytes: Vec::new(),
        }))
    }

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        self.blobs
            .lock()
            .expect("memory blob store")
            .get(&reference.id.to_string())
            .filter(|(stored, _)| stored == reference)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| BlobError::NotFound {
                reference: reference.id.clone(),
            })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        let result = self
            .read(&request.reference)
            .and_then(|bytes| BlobPage::new(request.reference.clone(), request.offset, bytes));
        Box::pin(futures::future::ready(result))
    }

    fn collect_garbage(&self, reachable: &[BlobRef]) -> Result<usize, BlobError> {
        let mut blobs = self.blobs.lock().expect("memory blob store");
        let before = blobs.len();
        blobs.retain(|_, (reference, _)| reachable.contains(reference));
        Ok(before - blobs.len())
    }
}

/// Deserialise a fixed UUID literal into any of the id newtypes, via their
/// already-derived (and already-public) `Deserialize` impl. This gets
/// deterministic, golden-comparable ids without adding a constructor to
/// `event::id` (out of scope: owned by another story).
pub fn id<T: serde::de::DeserializeOwned>(uuid: &str) -> T {
    serde_json::from_str(&format!("{uuid:?}")).unwrap()
}
