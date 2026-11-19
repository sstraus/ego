//! Cache immutable price blobs and one atomic mutable pointer through `StoreIo`. Core owns
//! types; `ego-ai` fetches and decodes.
//!
//! Blobs use content hashes; `current.json` names the selected blob and freshness metadata.
//! Refuse missing, corrupt, stale, future-dated, or mismatched entries without directory
//! fallback.
//!
//! Publication reuses a lease because processes otherwise share the same scratch path. Readers
//! need no lease with immutable blobs and atomic pointer replacement. Retain artifacts
//! referenced by past pricing revisions.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ego_core::cost::PricingRevision;
use ego_core::digest::sha256_hex;
use ego_core::event::time::Timestamp;
use serde::{Deserialize, Serialize};

use crate::store::{Lease, LeaseError, StoreIo};

/// The directory under `$EGO_HOME`.
pub const DIR: &str = "pricing";
/// The pointer's file name.
pub const POINTER: &str = "current.json";
/// The lock file publication is serialised on.
const LOCK: &str = "pricing.lock";

/// The pointer schema this build writes and is willing to read.
///
/// Refusing an unknown one is the same rule
/// [`RuleId::is_known`](ego_core::permit::rule::RuleId::is_known) applies to a
/// rule from a future grammar: a document from a generation this build cannot
/// verify does not mean something merely because its familiar fields parse.
const POINTER_VERSION: u32 = 1;

/// Maximum age since the last successful check. Seven days is the default policy exposure
/// window, not evidence that prices cannot change sooner.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long a lease holder may be silent before a caller that also believes it
/// is dead may take over.
///
/// Publication is a fetch, a hash and two writes, so a holder silent for a
/// minute is not slow, it is gone.
const LEASE_STALE_AFTER: Duration = Duration::from_secs(60);

/// The reader's view: the bytes, and what to call them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    pub bytes: Vec<u8>,
    /// Names the source, the upstream version and the content hash — the
    /// three facts `cost.rs:444-452` asks of a revision, so a past figure
    /// stays attributable to the artefact that produced it.
    pub revision: PricingRevision,
    /// The validator upstream served these bytes under, on its own.
    ///
    /// Also inside [`revision`](Self::revision), and carried separately
    /// anyway, because the two are read by different callers for different
    /// reasons. A revision is opaque — `ego-core` neither parses nor orders it
    /// — so a caller recovering the validator by taking a substring would be
    /// picking apart a type that exists in order not to be picked apart. This
    /// one is a header value, and its use is to be sent back on the next
    /// `If-None-Match`.
    pub upstream_version: String,
    pub fetched_at: Timestamp,
}

/// Why the cache could not answer, or could not be written.
#[derive(Debug)]
pub enum CacheError {
    /// No cache pointer. Keep absence distinct from stale and corrupt entries so diagnostics
    /// identify the actual remedy.
    Empty,
    /// The cached table is older than the caller allows.
    Stale {
        age: Duration,
        max_age: Duration,
    },
    /// The pointer is stamped after the reader's own clock.
    ///
    /// Reachable without tampering: `event/time.rs:22-25` says the wall clock
    /// may go backwards. It matters because the alternative is an age
    /// computed by underflow, which would present an arbitrarily old table as
    /// brand new — wrong in the one direction that costs money.
    FromTheFuture {
        fetched_at: Timestamp,
        now: Timestamp,
    },
    /// The pointer parses and names a schema this build cannot verify.
    UnknownPointerVersion {
        found: u32,
    },
    /// The cache is damaged: a torn pointer, a missing blob, or bytes that do
    /// not hash to the name they are stored under.
    Corrupt {
        message: String,
    },
    /// Another process is publishing.
    Held {
        pid: u32,
    },
    Io(io::Error),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Empty => write!(f, "no pricing table has been fetched on this machine"),
            CacheError::Stale { age, max_age } => write!(
                f,
                "the cached pricing table is {} days old and the limit is {}",
                age.as_secs() / 86_400,
                max_age.as_secs() / 86_400
            ),
            CacheError::FromTheFuture { fetched_at, now } => write!(
                f,
                "the cached pricing table is stamped {} and this machine's clock says {} — \
                 its age cannot be established",
                fetched_at.as_millis(),
                now.as_millis()
            ),
            CacheError::UnknownPointerVersion { found } => write!(
                f,
                "the pricing cache is version {found} and this build understands \
                 {POINTER_VERSION}"
            ),
            CacheError::Corrupt { message } => write!(f, "the pricing cache is damaged: {message}"),
            CacheError::Held { pid } => {
                write!(f, "process {pid} is refreshing the pricing table")
            }
            CacheError::Io(err) => write!(f, "the pricing cache could not be read: {err}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<io::Error> for CacheError {
    fn from(err: io::Error) -> Self {
        CacheError::Io(err)
    }
}

impl From<LeaseError> for CacheError {
    fn from(err: LeaseError) -> Self {
        match err {
            LeaseError::LeaseHeld { pid } => CacheError::Held { pid },
            LeaseError::Io(err) => CacheError::Io(err),
            other => CacheError::Corrupt {
                message: other.to_string(),
            },
        }
    }
}

/// Which blob is current, and when it arrived.
///
/// `fetched_at` is `Timestamp`'s integer milliseconds and not RFC 3339: two
/// spellings of one instant are two things to get wrong, and this is the
/// spelling the durable codec already uses.
#[derive(Debug, Serialize, Deserialize)]
struct Pointer {
    v: u32,
    sha256: String,
    upstream_version: String,
    fetched_at: Timestamp,
}

/// The cache under one `$EGO_HOME`.
pub struct PricingCache<'a> {
    io: &'a dyn StoreIo,
    dir: PathBuf,
}

impl<'a> PricingCache<'a> {
    pub fn new(io: &'a dyn StoreIo, ego_home: &Path) -> PricingCache<'a> {
        PricingCache {
            io,
            dir: ego_home.join(DIR),
        }
    }

    fn blob(&self, sha256: &str) -> PathBuf {
        self.dir.join(format!("{sha256}.json"))
    }

    fn pointer(&self) -> PathBuf {
        self.dir.join(POINTER)
    }

    /// Take the publication lease without publishing.
    ///
    /// Public so a caller that is about to fetch can hold the lease across the
    /// network call rather than racing to it afterwards — and so the exclusion
    /// itself is testable, which a lease taken and released inside
    /// [`publish`](Self::publish) would not be.
    pub fn lease(
        &self,
        now: Timestamp,
        pid: u32,
        is_alive: &dyn Fn(u32) -> bool,
    ) -> Result<Lease, CacheError> {
        Ok(Lease::acquire(
            &self.dir.join(LOCK),
            pid,
            now,
            LEASE_STALE_AFTER,
            is_alive,
        )?)
    }

    /// Publish blob first and pointer last. Failure before atomic pointer replacement leaves
    /// the prior table selected. Identical bytes retain blob identity while refreshing the
    /// check timestamp.
    pub fn publish(
        &self,
        bytes: &[u8],
        upstream_version: &str,
        now: Timestamp,
        pid: u32,
        is_alive: &dyn Fn(u32) -> bool,
    ) -> Result<PricingRevision, CacheError> {
        let _held = self.lease(now, pid, is_alive)?;
        self.publish_held(bytes, upstream_version, now)
    }

    /// The publication itself, for a caller that already holds the lease.
    pub fn publish_held(
        &self,
        bytes: &[u8],
        upstream_version: &str,
        now: Timestamp,
    ) -> Result<PricingRevision, CacheError> {
        let sha256 = sha256_hex(bytes);
        self.io.replace_atomic(&self.blob(&sha256), bytes)?;
        let pointer = Pointer {
            v: POINTER_VERSION,
            sha256: sha256.clone(),
            upstream_version: upstream_version.to_string(),
            fetched_at: now,
        };
        let encoded = serde_json::to_vec(&pointer).map_err(|err| CacheError::Corrupt {
            message: err.to_string(),
        })?;
        self.io.replace_atomic(&self.pointer(), &encoded)?;
        Ok(revision(upstream_version, &sha256))
    }

    /// Read the selected table only if it is young enough to bound the requested ceiling.
    /// Refuse stale data rather than returning it with a warning.
    pub fn load(&self, now: Timestamp, max_age: Duration) -> Result<Cached, CacheError> {
        let cached = self.current(now)?;
        let age = Duration::from_millis(
            now.as_millis()
                .saturating_sub(cached.fetched_at.as_millis()),
        );
        if age > max_age {
            return Err(CacheError::Stale { age, max_age });
        }
        Ok(cached)
    }

    /// The current table whatever its age.
    ///
    /// [`load`](Self::load) less one judgement, and every other refusal
    /// intact — an absent pointer, a torn one, a version this build cannot
    /// verify, a blob that does not hash to its name, and a clock that has run
    /// backwards are all still failures here. Dropping the age check is not
    /// dropping the fail-closed posture; `FromTheFuture` in particular stays,
    /// because it *is* the age computation refusing to underflow, and a reader
    /// that skipped it would be trusting a pointer by not looking at it.
    ///
    /// It exists for the caller that has to *fix* a stale table rather than
    /// price against one. Refreshing means offering the server back the
    /// validator of what is already on disk, and a caller that could not read
    /// a stale entry would have to download the whole document to discover it
    /// already had those exact bytes.
    pub fn current(&self, now: Timestamp) -> Result<Cached, CacheError> {
        let Some(raw) = self.io.read(&self.pointer())? else {
            return Err(CacheError::Empty);
        };
        let pointer: Pointer = serde_json::from_slice(&raw).map_err(|err| CacheError::Corrupt {
            message: format!("{POINTER}: {err}"),
        })?;
        if pointer.v != POINTER_VERSION {
            return Err(CacheError::UnknownPointerVersion { found: pointer.v });
        }

        // The clock before the bytes. This is not the age *judgement* — that
        // belongs to `load` — it is the arithmetic refusing to underflow: a
        // pointer stamped after the reader's own clock would otherwise present
        // an arbitrarily old table as brand new, which is wrong in the one
        // direction that costs money.
        if now.as_millis() < pointer.fetched_at.as_millis() {
            return Err(CacheError::FromTheFuture {
                fetched_at: pointer.fetched_at,
                now,
            });
        }

        let Some(bytes) = self.io.read(&self.blob(&pointer.sha256))? else {
            return Err(CacheError::Corrupt {
                message: format!("{POINTER} names {}, which is not here", pointer.sha256),
            });
        };
        // Every read, because the pointer says *which* artefact and never
        // *what is in it*. Without this a blob corrupted in place would be
        // priced from silently.
        let found = sha256_hex(&bytes);
        if found != pointer.sha256 {
            return Err(CacheError::Corrupt {
                message: format!("{} hashes to {found}", pointer.sha256),
            });
        }

        Ok(Cached {
            bytes,
            revision: revision(&pointer.upstream_version, &pointer.sha256),
            upstream_version: pointer.upstream_version,
            fetched_at: pointer.fetched_at,
        })
    }
}

/// Encode source, upstream revision, and content hash in one unambiguous revision. Core treats
/// this identity as opaque.
fn revision(upstream_version: &str, sha256: &str) -> PricingRevision {
    PricingRevision(format!("litellm@{upstream_version}#{sha256}"))
}
