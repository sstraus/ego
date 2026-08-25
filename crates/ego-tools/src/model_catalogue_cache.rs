//! Opaque durable storage for the machine-local model discovery snapshot.
//!
//! `ego-ai` owns the document codec and merge policy. This adapter owns only
//! the paths, refresh lease, byte reads, and atomic publication described by
//! `docs/decisions/2026-08-25-the-catalogue-is-a-snapshot-not-generation-authority.md`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ego_core::event::time::Timestamp;

use crate::store::{Lease, LeaseError, StoreIo};

const DIRECTORY: &str = "model-catalogue";
const SNAPSHOT: &str = "snapshot-v1.json";
const REFRESH_LOCK: &str = "refresh.lock";
const LEASE_STALE_AFTER: Duration = Duration::from_secs(60);

/// A failure at the opaque model-catalogue storage boundary.
#[derive(Debug)]
pub enum CacheError {
    /// Another live process owns the refresh lease.
    Held { pid: u32 },
    /// The lease record cannot be interpreted safely.
    Corrupt { message: String },
    /// A lease acquired for another cache path cannot authorize publication.
    WrongLease { expected: PathBuf, actual: PathBuf },
    /// The backing store failed.
    Io(io::Error),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held { pid } => write!(f, "process {pid} is refreshing the model catalogue"),
            Self::Corrupt { message } => {
                write!(f, "the model catalogue refresh lease is damaged: {message}")
            }
            Self::WrongLease { expected, actual } => write!(
                f,
                "the model catalogue lease is for {}, not {}",
                actual.display(),
                expected.display()
            ),
            Self::Io(error) => write!(
                f,
                "the model catalogue cache could not be accessed: {error}"
            ),
        }
    }
}

/// A held lease tied to one model-catalogue refresh path.
pub struct ModelCatalogueLease {
    // Ownership keeps the underlying advisory lock held until this wrapper is
    // dropped. Callers cannot construct a lease for an arbitrary path.
    _held: Lease,
    refresh_lock_path: PathBuf,
}

impl std::error::Error for CacheError {}

impl From<io::Error> for CacheError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<LeaseError> for CacheError {
    fn from(error: LeaseError) -> Self {
        match error {
            LeaseError::LeaseHeld { pid } => Self::Held { pid },
            LeaseError::Io(error) => Self::Io(error),
            other => Self::Corrupt {
                message: other.to_string(),
            },
        }
    }
}

/// The opaque catalogue cache below one `$EGO_HOME`.
pub struct ModelCatalogueCache<'a> {
    io: &'a dyn StoreIo,
    directory: PathBuf,
}

impl<'a> ModelCatalogueCache<'a> {
    pub fn new(io: &'a dyn StoreIo, ego_home: &Path) -> Self {
        Self {
            io,
            directory: ego_home.join(DIRECTORY),
        }
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.directory.join(SNAPSHOT)
    }

    pub fn refresh_lock_path(&self) -> PathBuf {
        self.directory.join(REFRESH_LOCK)
    }

    /// Acquire the lease before reading a refresh baseline.
    pub fn lease(
        &self,
        now: Timestamp,
        pid: u32,
        is_alive: &dyn Fn(u32) -> bool,
    ) -> Result<ModelCatalogueLease, CacheError> {
        let refresh_lock_path = self.refresh_lock_path();
        let held = Lease::acquire(&refresh_lock_path, pid, now, LEASE_STALE_AFTER, is_alive)?;
        Ok(ModelCatalogueLease {
            _held: held,
            refresh_lock_path,
        })
    }

    /// Read the last complete opaque document, if one has been published.
    pub fn read_bytes(&self) -> Result<Option<Vec<u8>>, CacheError> {
        Ok(self.io.read(&self.snapshot_path())?)
    }

    /// Atomically publish bytes while the caller keeps the refresh lease.
    pub fn publish_under_lease(
        &self,
        lease: &ModelCatalogueLease,
        bytes: &[u8],
    ) -> Result<(), CacheError> {
        let expected = self.refresh_lock_path();
        if lease.refresh_lock_path != expected {
            return Err(CacheError::WrongLease {
                expected,
                actual: lease.refresh_lock_path.clone(),
            });
        }
        Ok(self.io.replace_atomic(&self.snapshot_path(), bytes)?)
    }
}
