//! The `SessionStore` port: append, read and list durable sessions.
//!
//! The trait lives here; the filesystem implementation lives in `ego-tools`, because
//! `ego-core` opens no files.

use std::path::PathBuf;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use super::codec::CodecError;
use crate::event::domain::DomainEvent;
use crate::event::id::SessionId;
use crate::event::time::Timestamp;
use crate::evidence::EvidenceRecord;
use crate::run::{RunAppend, RunRevision, RunTransaction};
use crate::session::SessionLineage;

/// One durable item in a session's single ordered append-only stream.
///
/// Conversation events, run-control transitions, and pre-effect evidence
/// deliberately share the same store and read path. A second lifecycle or
/// evidence log would allow replay and conversation history to disagree about
/// which durable facts exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEntry {
    Event(DomainEvent),
    RunTransaction(RunTransaction),
    Evidence(EvidenceRecord),
    NoticeDelivered(NoticeDelivery),
    /// An explicit operator choice, independent of the model that last generated.
    ModelSelected(crate::config::ModelRef),
}

/// ACP transport acceptance outside conversation turns; this is not a host render receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeDelivery {
    pub card_id: String,
    pub source_path: Option<PathBuf>,
    pub delivered_at: Timestamp,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SessionEntryWire {
    Event { event: DomainEvent },
    RunTransaction { run_transaction: RunTransaction },
    Evidence { evidence: EvidenceRecord },
    NoticeDelivered { notice: NoticeDelivery },
    ModelSelected { model: crate::config::ModelRef },
}

impl Serialize for SessionEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Event(event) => SessionEntryWire::Event {
                event: event.clone(),
            },
            Self::RunTransaction(run_transaction) => SessionEntryWire::RunTransaction {
                run_transaction: run_transaction.clone(),
            },
            Self::Evidence(evidence) => SessionEntryWire::Evidence {
                evidence: evidence.clone(),
            },
            Self::ModelSelected(model) => SessionEntryWire::ModelSelected {
                model: model.clone(),
            },
            Self::NoticeDelivered(notice) => SessionEntryWire::NoticeDelivered {
                notice: notice.clone(),
            },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match SessionEntryWire::deserialize(deserializer)? {
            SessionEntryWire::Event { event } => Self::Event(event),
            SessionEntryWire::RunTransaction { run_transaction } => {
                Self::RunTransaction(run_transaction)
            }
            SessionEntryWire::Evidence { evidence } => {
                if let Some(error) = evidence.current_schema_error() {
                    if let EvidenceRecord::GenerationAdmission(admission) = &evidence {
                        let found = admission.capabilities().schema_version;
                        let required =
                            crate::generator::capabilities::ModelCapabilities::SCHEMA_VERSION;
                        if admission.version() == crate::evidence::EVIDENCE_SCHEMA_VERSION
                            && found != required
                        {
                            let age = if found < required {
                                "an old"
                            } else {
                                "a newer"
                            };
                            return Err(serde::de::Error::custom(format!(
                                "session was written with {age}, unsupported capabilities schema {found}; this build requires schema {required}"
                            )));
                        }
                    }
                    return Err(serde::de::Error::custom(error));
                }
                Self::Evidence(evidence)
            }
            SessionEntryWire::NoticeDelivered { notice } => Self::NoticeDelivered(notice),
            SessionEntryWire::ModelSelected { model } => Self::ModelSelected(model),
        })
    }
}

/// Immutable profile and prompt selections at session creation, before any run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProfiles {
    pub profile: Option<String>,
    pub ceiling_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_options: Option<crate::config::SessionPromptOptions>,
}

impl SessionProfiles {
    pub fn is_empty(&self) -> bool {
        self.profile.is_none() && self.ceiling_profile.is_none() && self.prompt_options.is_none()
    }
}

/// Per-session storage metadata.
///
/// This is the port-level shape: every `SessionStore` implementation
/// reports `list()` results through it, regardless of how it durably
/// stores them — the exact on-disk bytes are the implementation's concern,
/// not this type's.
///
/// `model` and `title` are `Option` because an event-first session provides
/// neither. A run admission projects its selected model, while a
/// `SessionTitleChanged` event projects the optional display title.
///
/// `workspace_root` is required: the store records it once at session birth
/// and reads it from the header. An optional value would expose a state
/// no current store can produce.
///
/// `created` and `updated` are never optional: both come from the durable
/// entry timestamps and the header authored at session birth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "SessionProfiles::is_empty")]
    pub profiles: SessionProfiles,
    pub id: SessionId,
    pub workspace_root: PathBuf,
    pub model: Option<String>,
    pub created: Timestamp,
    pub updated: Timestamp,
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub summary_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<SessionLineage>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// A `SessionStore` failure.
///
/// The append/read/list error vocabulary includes `WrongSession` so a
/// store scoped to a subset can refuse a session it does not serve.
/// Tail repair and writer leases have their own errors.
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
    /// Distinct from [`MissingHeader`](StoreError::MissingHeader): a damaged
    /// header may still precede intact records. Neither condition permits
    /// tail repair to discard those records.
    UnreadableHeader { reason: String },
    /// The log's header names a codec version this build does not implement.
    /// Reject unsupported codec versions instead of interpreting future records
    /// under rules that did not write them.
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

/// One session's repaired state and append authority under its writer lease.
/// Dropping the writer releases the lease; consumers retain it through admission.
pub struct SessionWriterLease<'a> {
    pub state: crate::state::SessionState,
    pub writer: Box<dyn SessionStore + 'a>,
}

/// Durable session storage: append events, read them back, and catalogue sessions.
pub trait SessionStore: Send + Sync {
    /// Acquire the existing session writer lease before reading journal state.
    /// Stores without a writer authority fail closed on continuation admission.
    fn open_writer_lease(
        &self,
        _id: &SessionId,
        _now: crate::event::time::Timestamp,
    ) -> Result<SessionWriterLease<'_>, StoreError> {
        Err(StoreError::Io(std::io::Error::other(
            "session writer lease is unavailable",
        )))
    }

    /// Append `ev` to `id`'s durable log, writing the session's versioned
    /// header first if this is the first event for `id`. When to force
    /// bytes to stable storage is a durability concern owned by the
    /// implementation, decided from `ev`'s `EventKind` directly — not from
    /// a predicate carried on `DomainEvent`, which stays a pure domain type
    /// with no notion of filesystem durability.
    fn append(&self, id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError>;

    /// Read back `id`'s durable log as a stream of ordered entry results.
    ///
    /// Each item is its own `Result`: a corrupt or truncated line must
    /// never be silently skipped, since replay integrity and later tail
    /// repair both depend on knowing exactly which lines failed and why.
    /// The outer `Result` is reserved for failing to open the log at all —
    /// no session with that id, or a missing/corrupt header line.
    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError>;

    /// Append one run-control transaction when the durable run revision is
    /// exactly `expected_revision`.
    ///
    /// `None` means that no run transaction may exist. A successful first
    /// append either publishes its header and transaction as one atomic image,
    /// or adopts a matching header-only or event-first log with a
    /// revision-one admission.
    /// Run revisions count only `RunTransaction` entries; interleaved
    /// conversation events never advance the compare value.
    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError>;

    /// List every session this store knows about, from its manifests.
    fn list(&self) -> Result<Vec<SessionMeta>, StoreError>;

    /// List resumable entries independently and report each unreadable session.
    /// Stores without a separate manifest cache already provide their listing authority.
    fn list_readable(
        &self,
        _unreadable: &mut dyn FnMut(SessionId, StoreError),
    ) -> Result<Vec<SessionMeta>, StoreError> {
        self.list()
    }
}
