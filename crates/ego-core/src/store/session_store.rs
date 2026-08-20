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
/// `workspace_root`, `model` and `title` are `Option` because
/// `SessionStore::append` is the only way this port learns anything: it
/// receives just a `SessionId` and a `DomainEvent`, and no `EventKind`
/// (`ARCHITECTURE.md` §1) carries workspace, model or title information. A
/// store built only from `append` calls therefore has no way to populate
/// them at this milestone; a later story (session creation, once the loop
/// assembles that context) is what turns these into `Some`. `created` and
/// `updated` are never optional: both come straight from the envelope
/// timestamp of an event the store actually received.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub workspace_root: Option<PathBuf>,
    pub model: Option<String>,
    pub created: Timestamp,
    pub updated: Timestamp,
    pub title: Option<String>,
}

/// A `SessionStore` failure.
///
/// Deliberately small: this is the Step 4 contract (append/read/list on a
/// healthy store), not the tail-repair or writer-lease vocabulary later
/// steps add — see `plans/m0a-contracts.md` Steps 5-7.
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
    /// must report, not paper over — see `HeaderV1` in `crate::store::codec`.
    MissingHeader,
    /// `read` was asked for a session with no durable log.
    NotFound,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "session store I/O error: {e}"),
            StoreError::Codec(e) => write!(f, "session store codec error: {e}"),
            StoreError::Manifest(e) => write!(f, "session manifest error: {e}"),
            StoreError::MissingHeader => write!(f, "session log is missing its header record"),
            StoreError::NotFound => write!(f, "session not found"),
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
