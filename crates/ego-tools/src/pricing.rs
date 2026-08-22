//! The cached price table: immutable blobs, one mutable pointer, and a clock.
//!
//! `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §6 and its §6.1
//! amendment. §6 splits the work three ways and this is the middle third:
//! `ego-ai` fetches and decodes because it owns `Http` and the wire formats,
//! **`ego-tools` reads and writes because it owns `StoreIo`**, and `ego-core`
//! holds the types and opens nothing.
//!
//! # The layout, and the hole §6 left
//!
//! §6 says the artefacts are named by their content hash so a later fetch
//! cannot overwrite the bytes an earlier reservation used, and also that a
//! table older than a stated age is refused. Those do not compose: a content
//! hash is not a clock, and a directory of hashes says nothing about which of
//! them is current. §6.1 resolves it with two kinds of file:
//!
//! - `$EGO_HOME/pricing/<sha256>.json` — the fetched bytes, verbatim, never
//!   rewritten. §6's immutability is about *these*.
//! - `$EGO_HOME/pricing/current.json` — which blob is current, what upstream
//!   called it, and when it arrived. Replaced atomically.
//!
//! The pointer is not a table artefact; it is the mutable index that selects
//! one. Replacing it mutates no bytes a past reservation was computed from.
//!
//! # Everything here fails closed
//!
//! A cache exists to bound a guarantee the operator explicitly asked for, so
//! "I could not establish which table this is" must never resolve to "use one
//! anyway". A missing pointer, a torn pointer, a version this build does not
//! know, a missing blob, a hash mismatch, an age past the limit, or a stamp in
//! the reader's future are all refusals — and none of them scans the directory
//! for something else to use. A fallback would price a run from a table the
//! pointer does not name, which is exactly the attribution the hash exists to
//! make impossible.
//!
//! # Why publication takes a lease and reading does not
//!
//! [`StoreIo`]'s scratch path is [`tmp_path_for`](crate::store::tmp_path_for),
//! a pure function of its target, so two ego processes refreshing this
//! **machine-scoped** cache both write `current.json.tmp` and one can rename a
//! file the other is still writing. The existing [`Lease`] is reused rather
//! than a second lock invented. Readers need none: immutable blobs plus an
//! atomic pointer swap are enough.
//!
//! Nothing is ever collected. A quote carries its [`PricingRevision`] and the
//! artefact it names has to stay readable for that attribution to mean
//! anything; a rule that could justify deleting one needs its own proof.

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

/// How long a cached table may go unrefreshed before a ceiling refuses it.
///
/// §6's default. Seven days is a policy choice and not evidence — prices
/// change on announcement rather than on a schedule, so no age is *safe* —
/// and what it buys is a bounded exposure window plus a roughly weekly fetch
/// cadence.
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
    pub fetched_at: Timestamp,
}

/// Why the cache could not answer, or could not be written.
#[derive(Debug)]
pub enum CacheError {
    /// No pointer at all: no ceiling has ever been used on this host.
    ///
    /// Distinct from [`Stale`](Self::Stale) and from
    /// [`Corrupt`](Self::Corrupt) because the three send an operator to three
    /// different places, and §7's table gives the first two different exit
    /// codes depending on the egress posture.
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

    /// Store `bytes` and make them current, returning the revision they were
    /// stored under.
    ///
    /// Blob first, pointer last. Any failure before the final atomic replace
    /// leaves the previous pointer selecting the previous blob, which is
    /// §Failure semantics' "a bad fetch never half-replaces a good table".
    ///
    /// Re-publishing identical bytes writes the same blob and a fresh pointer:
    /// the freshness gate is about when ego last *checked*, not about when the
    /// document last changed.
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

    /// The current table, if there is one and it is young enough.
    pub fn load(&self, now: Timestamp, max_age: Duration) -> Result<Cached, CacheError> {
        let Some(raw) = self.io.read(&self.pointer())? else {
            return Err(CacheError::Empty);
        };
        let pointer: Pointer = serde_json::from_slice(&raw).map_err(|err| CacheError::Corrupt {
            message: format!("{POINTER}: {err}"),
        })?;
        if pointer.v != POINTER_VERSION {
            return Err(CacheError::UnknownPointerVersion { found: pointer.v });
        }

        // Age before bytes: a stale table is refused whatever its blob says,
        // and reading a megabyte to then discard it is work for nothing.
        let Some(elapsed) = now.as_millis().checked_sub(pointer.fetched_at.as_millis()) else {
            return Err(CacheError::FromTheFuture {
                fetched_at: pointer.fetched_at,
                now,
            });
        };
        let age = Duration::from_millis(elapsed);
        if age > max_age {
            return Err(CacheError::Stale { age, max_age });
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
            fetched_at: pointer.fetched_at,
        })
    }
}

/// The three facts §6 requires of a revision, in one string.
///
/// Minted here and nowhere else so the value a reservation records and the
/// value a later read reports are the same spelling. `ego-core` treats it as
/// opaque — it neither parses nor compares beyond equality — so the format is
/// this module's to choose and its only job is to be unambiguous.
fn revision(upstream_version: &str, sha256: &str) -> PricingRevision {
    PricingRevision(format!("litellm@{upstream_version}#{sha256}"))
}
