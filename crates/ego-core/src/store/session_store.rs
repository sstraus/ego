//! The `SessionStore` port: append, read and list durable sessions.
//!
//! See `ARCHITECTURE.md` §3.3 and §5. The trait lives here; the filesystem
//! implementation lives in `ego-tools`, because `ego-core` opens no files.

use std::path::PathBuf;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use super::codec::CodecError;
use crate::event::domain::DomainEvent;
use crate::event::id::SessionId;
use crate::event::time::Timestamp;

/// The catalogue entry for one session (`ARCHITECTURE.md` §5's
/// `manifest.json` fields: `id, workspace_root, model, created, updated,
/// title`).
///
/// This is the port-level shape: every `SessionStore` implementation
/// reports `list()` results through it, regardless of how it durably
/// stores them — the exact on-disk bytes are the implementation's concern,
/// not this type's.
///
/// `model` and `title` are `Option` because `SessionStore::append` is the
/// only way this port learns anything: it receives just a `SessionId` and a
/// `DomainEvent`, and no `EventKind` (`ARCHITECTURE.md` §1) carries model or
/// title information. A store built only from `append` calls therefore has
/// no way to populate them at this milestone.
///
/// `workspace_root` is **not** among them, and used to be. The reason it was
/// optional — that `append` cannot learn a workspace — stopped being true
/// when the log header started carrying one at `CODEC_VERSION` 5: the store
/// does not learn it from an event, it writes it once at session birth and
/// reads it back from the header. An `Option` here would be a state no store
/// in this build can produce, handed to `ego ls` and `ego resume` to write
/// dead branches for. See
/// `docs/decisions/2026-08-21-the-two-commands-the-header-unblocked.md` §1.
///
/// `created` and `updated` are never optional: both come straight from the
/// envelope timestamp of an event the store actually received.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub workspace_root: PathBuf,
    pub model: Option<String>,
    pub created: Timestamp,
    pub updated: Timestamp,
    pub title: Option<String>,
}

/// A `SessionStore` failure.
///
/// Deliberately small: this is the Step 4 contract (append/read/list on a
/// healthy store), not the tail-repair or writer-lease vocabulary later
/// steps add — see `plans/m0a-contracts.md` Steps 5-7. `WrongSession` is the
/// one addition and is neither of those: it is a store saying it does not
/// serve that session at all, which any implementation scoped to a subset of
/// sessions can say (see
/// `docs/decisions/2026-08-21-opening-a-session-for-writing.md` §3).
#[derive(Debug)]
pub enum StoreError {
    /// The underlying I/O failed: open, read, write, fsync or rename.
    Io(std::io::Error),
    /// A record line failed to decode: malformed JSON or a checksum
    /// mismatch (see `crate::store::codec`).
    Codec(CodecError),
    /// `manifest.json` exists but is not valid `SessionMeta` JSON.
    Manifest(serde_json::Error),
    /// The log's first line is not a header record. A defect the store
    /// must report, not paper over — see `Header` in `crate::store::codec`.
    MissingHeader,
    /// The log's first line is a header and will not decode as one.
    ///
    /// Distinct from [`MissingHeader`](StoreError::MissingHeader) because the
    /// remedy is different — the header is *there* and damaged, so the
    /// records under it are probably fine — and because tail repair must
    /// treat the two identically only in one respect: neither may cost a
    /// byte. See `docs/decisions/2026-08-21-a-header-the-repair-must-not-delete.md`
    /// §1.
    UnreadableHeader { reason: String },
    /// The log's header names a codec version this build does not implement.
    ///
    /// Refused rather than read as though it were ours: nothing compared `v`
    /// before, so a log from a future build decoded as the current version
    /// and its records were interpreted under rules that were never used to
    /// write them (ADR §3).
    UnsupportedLog { found: u32 },
    /// `read` was asked for a session with no durable log.
    NotFound,
    /// This store serves one session and was asked about another.
    ///
    /// Returning it rather than delegating anyway is what keeps a
    /// session-scoped store honest: a writer holding a lease for one session
    /// that quietly appended to a second would be writing unleased, which is
    /// the exact thing the lease exists to prevent.
    WrongSession {
        expected: SessionId,
        asked: SessionId,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "session store I/O error: {e}"),
            StoreError::Codec(e) => write!(f, "session store codec error: {e}"),
            StoreError::Manifest(e) => write!(f, "session manifest error: {e}"),
            StoreError::MissingHeader => write!(f, "session log is missing its header record"),
            StoreError::UnreadableHeader { reason } => {
                write!(f, "session log's header record cannot be read: {reason}")
            }
            StoreError::UnsupportedLog { found } => write!(
                f,
                "session log was written at codec version {found}, which this build does not read"
            ),
            StoreError::NotFound => write!(f, "session not found"),
            StoreError::WrongSession { expected, asked } => {
                // `Display` and not `Debug`: this reaches an operator through
                // `StoreError`, and `SessionId(0192…)` tells them the name of
                // a Rust type they cannot act on. The id alone is the thing
                // they can pass back to `ego resume`.
                write!(f, "this store serves session {expected}, not {asked}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<CodecError> for StoreError {
    fn from(e: CodecError) -> Self {
        StoreError::Codec(e)
    }
}

/// Durable session storage: append events, read them back, and catalogue
/// sessions. `ARCHITECTURE.md` §3.3; `JsonlStore`, the filesystem
/// implementation, lives in `ego-tools`, because this crate opens no files.
pub trait SessionStore: Send + Sync {
    /// Append `ev` to `id`'s durable log, writing the session's versioned
    /// header first if this is the first event for `id`. When to force
    /// bytes to stable storage is a durability concern owned by the
    /// implementation, decided from `ev`'s `EventKind` directly — not from
    /// a predicate carried on `DomainEvent`, which stays a pure domain type
    /// with no notion of filesystem durability.
    fn append(&self, id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError>;

    /// Read back `id`'s durable log as a stream of per-event results.
    ///
    /// Each item is its own `Result`: a corrupt or truncated line must
    /// never be silently skipped, since replay integrity and later tail
    /// repair both depend on knowing exactly which lines failed and why.
    /// The outer `Result` is reserved for failing to open the log at all —
    /// no session with that id, or a missing/corrupt header line.
    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<DomainEvent, StoreError>>, StoreError>;

    /// List every session this store knows about, from its manifests.
    fn list(&self) -> Result<Vec<SessionMeta>, StoreError>;
}
