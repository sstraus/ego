//! `JsonlStore`: the filesystem `SessionStore` — a JSONL event log plus a
//! JSON manifest, per session directory (`ARCHITECTURE.md` §5).
//!
//! ```text
//! <root>/<session-id>/
//!     manifest.json      # private RunManifest, replaced atomically
//!     events.jsonl        # the log; first line is a versioned header record
//! ```

use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::Arc;

use ego_core::compact::{
    CompactionTargetError, CompactionTargetFacts, CompactionTargetInspection,
    CompactionTargetPort,
};
use ego_core::event::domain::{DomainEvent, EventKind, GenerationRole};
use ego_core::event::hook::HookCatalogueEvidence;
use ego_core::event::id::{Producer, Seq, SessionId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{
    EvidenceError, EvidenceKey, EvidenceLogSource, EvidenceRecord, EvidenceWrite, ManifestEvidence,
    ManifestEvidenceError,
};
use ego_core::permit::ExecObservation;
use ego_core::run::{RunAppend, RunCommand, RunRevision, RunTransaction};
use ego_core::session::{
    ReplaySource, ReplaySourceError, SessionBlobSource, SessionLineage, TargetPublication,
    TargetSeed,
};
use ego_core::state::SessionState;
use ego_core::store::blob::{BlobError, BlobStore};
use ego_core::store::codec::{
    CODEC_VERSION, CodecError, Header, StoreLine, decode_line, encode_header, encode_record,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use fd_lock::RwLock;
use futures::stream::{self, BoxStream};

use super::blob::SessionBlobStore;
use super::io::{AtomicReplacePublication, StoreIo, tmp_path_for};
use super::snapshot::{SNAPSHOT_VERSION, Snapshot, SnapshotRejection, validate};

/// The only on-disk log version this build migrates. Writers emit only v8;
/// this constant names the single v7 -> v8 ownership transition.
const MIGRATABLE_CODEC_VERSION: u32 = 7;

/// The sole decoder for the untagged v7 payload shape.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum LegacySessionEntry {
    Event(DomainEvent),
    RunTransaction(RunTransaction),
}

impl From<LegacySessionEntry> for SessionEntry {
    fn from(value: LegacySessionEntry) -> Self {
        match value {
            LegacySessionEntry::Event(event) => Self::Event(event),
            LegacySessionEntry::RunTransaction(transaction) => Self::RunTransaction(transaction),
        }
    }
}

/// Filesystem-private cache projection. The public store port exposes only
/// [`SessionMeta`]; execution and admitted-hook evidence remain internal log
/// facts, projected here only for bounded catalogue reads and recovery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RunManifest {
    #[serde(flatten)]
    meta: SessionMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution: Option<ExecObservation>,
    #[serde(default)]
    hooks: HookCatalogueEvidence,
    #[serde(default)]
    evidence: ManifestEvidence,
}

struct LogManifestFacts {
    created: Timestamp,
    workspace_root: PathBuf,
    last_ts: Option<Timestamp>,
    model: Option<String>,
    lineage: Option<SessionLineage>,
    execution: Option<ExecObservation>,
    hooks: HookCatalogueEvidence,
    evidence: ManifestEvidence,
}

#[derive(Debug, Clone, Copy)]
struct RunLogState {
    revision: Option<RunRevision>,
    last_position: Seq,
    has_event: bool,
}

/// What [`JsonlStore::recover`] actually had to repair.
///
/// Returned rather than swallowed because "we silently rewrote your session
/// log" is not something a store should do invisibly: a caller decides
/// whether dropping N events is routine (the last append was interrupted)
/// or alarming (half the session vanished). Every field is zero/false on
/// the overwhelmingly common path where nothing was wrong.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Repair {
    /// Whether the session image was migrated between codec versions. This is
    /// distinct from tail repair: a clean migration changes the header and
    /// physical positions, but drops no bytes or lines.
    pub migrated: bool,
    /// Bytes cut from a torn tail.
    pub truncated_bytes: u64,
    /// Lines those bytes contained — a torn final line counts as one.
    pub dropped_lines: usize,
    /// Whether the manifest was rewritten from the log.
    pub manifest_rebuilt: bool,
    /// Whether a scratch file orphaned mid-`replace_atomic` was removed.
    pub removed_tmp: bool,
}

impl Repair {
    /// Whether anything at all was repaired.
    pub fn is_clean(&self) -> bool {
        *self == Self::default()
    }
}

/// Check the log's header line and return the offset just past it.
///
/// **Separate from the tail scan below, and the separation is the point.**
/// The header is not a candidate for repair. A prefix scan that rejects the
/// line at index 0 has established a valid prefix of zero bytes, and a
/// caller that then truncates to that length deletes the entire session —
/// which is what this store did, reporting it as an ordinary `Repair`. See
/// `docs/decisions/2026-08-21-a-header-the-repair-must-not-delete.md` §1.
///
/// So the header is answered here, before anything is allowed to remove a
/// byte. `read` and `recover` both come through this function so the two
/// cannot drift on what a readable header is.
///
/// `Ok(None)` is the one case where damaged-header bytes may still be thrown
/// away: no newline anywhere means the file is a single unterminated line, so
/// by this store's own durability rule there is no complete header **and no
/// record can exist under it** — nothing to lose. That is an interrupted
/// first append, the ordinary way a session dies before it begins, and
/// refusing it would leave a brand-new session permanently unopenable over
/// bytes that never held an event. Every other damaged header has a
/// terminated line, and therefore may have records beneath it.
fn decoded_header(bytes: &[u8]) -> Result<Option<(Header, usize)>, StoreError> {
    let Some(offset) = bytes.iter().position(|&b| b == b'\n') else {
        return Ok(None);
    };
    match decode_line(&bytes[..offset]) {
        Ok(StoreLine::Header(header)) => Ok(Some((header, offset + 1))),
        Ok(StoreLine::Record(_) | StoreLine::Unknown(_)) => Err(StoreError::MissingHeader),
        Err(e) => Err(StoreError::UnreadableHeader {
            reason: e.to_string(),
        }),
    }
}

fn header_end(bytes: &[u8]) -> Result<Option<usize>, StoreError> {
    match decoded_header(bytes)? {
        Some((header, end)) if header.v == CODEC_VERSION => Ok(Some(end)),
        // ADR §3. Nothing compared `v` before, so a log written by a later
        // build was read as though it were this version's.
        Some((header, _)) => Err(StoreError::UnsupportedLog { found: header.v }),
        None => Ok(None),
    }
}

/// The end of the last *complete, valid* record line in `bytes`, plus how
/// many lines were rejected from there on. `start` is [`header_end`]'s
/// offset: the header is already validated and is never a repair candidate.
///
/// A line counts as durable only if it is terminated by a newline: a final
/// line without one is by definition a half-finished append, which is the
/// single most likely crash artefact for an append-only log. Beyond that,
/// every line must decode — checksum included — and must be a recognised
/// record or a syntactically valid unknown line. Unknown lines are immutable
/// bytes owned by a newer codec; recovery preserves them rather than treating
/// them as torn tail.
///
/// The scan stops at the *first* rejected line rather than skipping it and
/// resuming, and everything after it is dropped too. That is deliberate: a
/// gap in an append-only log means the byte stream desynchronised, so
/// "valid" lines after the gap are not trustworthy neighbours — they are
/// bytes that happened to parse.
fn valid_prefix(bytes: &[u8], start: usize) -> (usize, usize) {
    let mut end = start;

    while end < bytes.len() {
        let Some(offset) = bytes[end..].iter().position(|&b| b == b'\n') else {
            break; // torn final line: no terminating newline
        };
        match decode_line(&bytes[end..end + offset]) {
            Ok(StoreLine::Record(_) | StoreLine::Unknown(_)) => {}
            _ => break,
        }
        end += offset + 1;
    }

    // Count what is being discarded, in lines rather than just bytes, so a
    // caller can tell "the last append was cut short" (1) from "the log
    // desynchronised early" (many).
    let dropped = if end < bytes.len() {
        bytes[end..].split(|&b| b == b'\n').count() - usize::from(bytes.ends_with(b"\n"))
    } else {
        0
    };
    (end, dropped)
}

fn invalid_log(message: impl Into<String>) -> StoreError {
    StoreError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    ))
}

fn accept_physical_position(current: &mut Seq, actual: Seq) -> Result<(), StoreError> {
    let expected = current.next();
    if actual != expected {
        return Err(invalid_log(format!(
            "session record position is not contiguous: expected {expected:?}, found {actual:?}"
        )));
    }
    Ok(())
}

/// Validate the positions this codec can see. An unknown v8 record is kept
/// verbatim by recovery; because its position is opaque to this build, later
/// records cannot be ordered relative to it and validation stops there.
fn validate_physical_positions<'a>(
    lines: impl Iterator<Item = &'a [u8]>,
) -> Result<(), StoreError> {
    let mut current = Seq::ZERO;
    for line in lines {
        match decode_line(line) {
            Ok(StoreLine::Record(record)) => {
                accept_physical_position(&mut current, record.seq)?;
            }
            Ok(StoreLine::Unknown(_)) => break,
            Ok(StoreLine::Header(_)) => {
                return Err(invalid_log(
                    "unexpected header record past the log's first line",
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn migrate_v7_image(
    bytes: &[u8],
    mut header: Header,
    start: usize,
) -> Result<(Vec<u8>, usize, usize), StoreError> {
    // A future payload remains someone else's bytes even when an earlier line
    // is corrupt. Refuse before constructing or publishing any replacement.
    let mut cursor = start;
    while cursor < bytes.len() {
        let Some(offset) = bytes[cursor..].iter().position(|byte| *byte == b'\n') else {
            break;
        };
        match decode_line(&bytes[cursor..cursor + offset]) {
            Ok(StoreLine::Record(record)) => {
                serde_json::from_str::<LegacySessionEntry>(record.payload.get()).map_err(|_| {
                    invalid_log("cannot migrate a v7 log containing an unknown payload")
                })?;
            }
            Ok(StoreLine::Unknown(_)) => {
                return Err(invalid_log(
                    "cannot migrate a v7 log containing an unknown record",
                ));
            }
            Ok(StoreLine::Header(_)) => {
                return Err(invalid_log(
                    "unexpected header record past the log's first line",
                ));
            }
            Err(_) => {}
        }
        cursor += offset + 1;
    }

    let (valid_len, dropped_lines) = valid_prefix(bytes, start);
    header.v = CODEC_VERSION;
    let mut migrated = encode_header(&header);
    migrated.push(b'\n');
    let mut position = Seq::ZERO;
    for line in bytes[start..valid_len]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record = match decode_line(line)? {
            StoreLine::Record(record) => record,
            StoreLine::Unknown(_) => {
                return Err(invalid_log(
                    "cannot migrate a v7 log containing an unknown record",
                ));
            }
            StoreLine::Header(_) => {
                return Err(invalid_log(
                    "unexpected header record past the log's first line",
                ));
            }
        };
        let entry = serde_json::from_str::<LegacySessionEntry>(record.payload.get())
            .map(SessionEntry::from)
            .map_err(|error| CodecError::Malformed(error.to_string()))?;
        let next = position.next();
        migrated.extend_from_slice(&encode_record(next, &entry)?);
        migrated.push(b'\n');
    }
    Ok((migrated, valid_len, dropped_lines))
}

/// A `SessionId`'s value, as a plain hyphenated UUID string.
///
/// `SessionId` is a single-field tuple struct with a derived `Serialize`
/// impl, so serde treats it as a newtype and serialises it exactly as its
/// inner `Uuid` would — a plain string, no wrapper. That is the only public
/// way this crate has to read a `SessionId`'s value (its field is private,
/// with no accessor), so round-tripping through serde to name a session's
/// directory is deliberate, not a workaround.
fn session_dir_name(id: &SessionId) -> String {
    match serde_json::to_value(id).expect("SessionId always serialises") {
        serde_json::Value::String(s) => s,
        other => unreachable!("SessionId did not serialise as a JSON string: {other}"),
    }
}

/// Split a whole log's bytes into lines (each without its trailing
/// newline), tolerating one trailing newline at end-of-file.
///
/// Mirrors `ego_core::store::codec::decode_log`'s own splitting, duplicated
/// rather than called because that function stops at the first line that
/// fails to decode; `JsonlStore::read` must not — see `SessionStore::read`'s
/// doc comment on why a per-line failure cannot abort the stream.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// The manifest facts derived from a valid log prefix.
///
/// Only ever called on a prefix [`valid_prefix`] has already accepted, so
/// every physical line here is checksummed. Payload decoding is deliberately
/// stricter: an unknown typed entry refuses projection so recovery never
/// rewrites cache state from an authority this build cannot interpret.
fn log_manifest_facts(log: &[u8]) -> Result<Option<LogManifestFacts>, StoreError> {
    let mut lines = split_lines(log).into_iter();
    let Some(header_line) = lines.next() else {
        return Ok(None);
    };
    let (created, workspace_root) = match decode_line(header_line) {
        Ok(StoreLine::Header(header)) => (header.created, header.workspace_root),
        _ => return Ok(None),
    };
    let mut last_ts = None;
    let mut model = None;
    let mut lineage = None;
    let mut execution = None;
    let mut hooks = HookCatalogueEvidence::default();
    let mut evidence_records = Vec::new();
    for line in lines {
        let record = match decode_line(line)? {
            StoreLine::Record(record) => record,
            StoreLine::Unknown(_) => {
                return Err(invalid_log("unknown record in the valid session prefix"));
            }
            StoreLine::Header(_) => {
                return Err(invalid_log(
                    "unexpected header record past the log's first line",
                ));
            }
        };
        let entry = serde_json::from_str::<SessionEntry>(record.payload.get())
            .map_err(|_| invalid_log("unknown payload in the valid session prefix"))?;
        match entry {
            SessionEntry::Event(event) => {
                last_ts = Some(event.env.ts);
                match event.kind {
                    EventKind::CompactionSeeded {
                        source,
                        source_seq,
                        prompt_revision,
                        ..
                    } => {
                        lineage = Some(SessionLineage::Compaction {
                            source,
                            source_seq,
                            prompt_revision,
                        })
                    }
                    EventKind::Forked { source, at_seq } => {
                        lineage = Some(SessionLineage::Fork { source, at_seq })
                    }
                    EventKind::ExecutionObserved { observation } => execution = Some(observation),
                    EventKind::HookCatalogueAdmitted { descriptors } => hooks = descriptors,
                    _ => {}
                }
            }
            SessionEntry::RunTransaction(transaction) => {
                last_ts = Some(transaction.observed_at());
            }
            SessionEntry::Evidence(record) => {
                last_ts.get_or_insert(created);
                if let EvidenceRecord::GenerationAdmission(admission) = &record
                    && admission.role() == GenerationRole::Main
                {
                    model = Some(admission.target().canonical_model().0.clone());
                }
                evidence_records.push(record);
            }
        }
    }
    let evidence = ManifestEvidence::from_records(&evidence_records)
        .map_err(|error| invalid_log(error.to_string()))?;
    Ok(Some(LogManifestFacts {
        created,
        workspace_root,
        last_ts,
        model,
        lineage,
        execution,
        hooks,
        evidence,
    }))
}

/// The filesystem `SessionStore`. Real I/O goes through the injected
/// [`StoreIo`], so tests can wrap it (e.g. to count fsyncs) without faking
/// the filesystem itself.
pub struct JsonlStore {
    root: PathBuf,
    workspace: PathBuf,
    pub(super) io: Arc<dyn StoreIo>,
}

impl JsonlStore {
    /// Build a store rooted at `root` (e.g. `~/.ego/sessions`), creating new
    /// sessions on behalf of `workspace`, and performing its I/O through
    /// `io`.
    ///
    /// **`workspace` is required, and it is only ever written.** It is
    /// stamped into the header of a session this store *creates* and is read
    /// back from the header of every session it lists — so `list` still
    /// returns sessions belonging to every workspace, and filtering to one is
    /// the caller's question, not this constructor's. One store serves one
    /// process, and a process has exactly one workspace: `run.rs` applies
    /// `-C DIR` before anything reads a path, so there is a single notion of
    /// "here".
    ///
    /// It must already be canonical. This type does not resolve it — that
    /// would be a second canonicaliser to keep in step with `run.rs`'s, and
    /// `AGENTS.md`'s reuse-first rule says one. A path that has not been
    /// through `std::fs::canonicalize` will not compare equal to the same
    /// directory reached through a symlink or through `/var` instead of
    /// `/private/var`, and the failure is a session that quietly does not
    /// belong to the workspace it was created in.
    pub fn new(
        root: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        io: Box<dyn StoreIo>,
    ) -> Self {
        Self {
            root: root.into(),
            workspace: workspace.into(),
            io: Arc::from(io),
        }
    }

    pub(super) fn session_dir(&self, id: &SessionId) -> PathBuf {
        self.root.join(session_dir_name(id))
    }

    fn log_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("events.jsonl")
    }

    fn manifest_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("manifest.json")
    }

    fn events_lock_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("events.lock")
    }

    fn open_events_lock(&self, id: &SessionId) -> Result<RwLock<std::fs::File>, StoreError> {
        let lock_path = self.events_lock_path(id);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        Ok(RwLock::new(file))
    }

    /// Serialize one complete log mutation and its manifest projection.
    /// This is deliberately distinct from the long-held session writer lease.
    fn with_events_lock<T>(
        &self,
        id: &SessionId,
        operation: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut lock = self.open_events_lock(id)?;
        let _guard = lock.write()?;
        operation()
    }

    fn run_log_state_from_lines<'a>(
        &self,
        lines: impl Iterator<Item = &'a [u8]>,
    ) -> Result<RunLogState, StoreError> {
        let mut revision = None;
        let mut last_position = Seq::ZERO;
        let mut has_event = false;
        for line in lines {
            let record = match decode_line(line) {
                Ok(StoreLine::Record(record)) => record,
                Ok(StoreLine::Header(_)) => {
                    return Err(StoreError::Codec(CodecError::Malformed(
                        "unexpected header record past the log's first line".to_string(),
                    )));
                }
                Ok(StoreLine::Unknown(_)) => {
                    return Err(StoreError::Codec(CodecError::Malformed(
                        "cannot compare a run revision across an unknown record".to_string(),
                    )));
                }
                Err(error) => return Err(error.into()),
            };
            accept_physical_position(&mut last_position, record.seq)?;
            let entry = serde_json::from_str::<SessionEntry>(record.payload.get())
                .map_err(|error| StoreError::Codec(CodecError::Malformed(error.to_string())))?;
            let transaction = match entry {
                SessionEntry::RunTransaction(transaction) => transaction,
                SessionEntry::Event(_) => {
                    has_event = true;
                    continue;
                }
                SessionEntry::Evidence(_) => continue,
            };
            let expected = revision
                .map(RunRevision::next)
                .unwrap_or_else(|| RunRevision::new(1));
            if transaction.revision() != expected {
                return Err(invalid_log("run transaction revisions are not contiguous"));
            }
            revision = Some(transaction.revision());
        }
        Ok(RunLogState {
            revision,
            last_position,
            has_event,
        })
    }

    /// Publish the complete first log image for a typed target.
    ///
    /// The phase-aware replace is the authority boundary: before rename the
    /// source may remain active and retry; after rename the target is visible
    /// and must be selected even when the directory durability barrier or the
    /// derived manifest cache reports a failure.
    pub(super) fn publish_target(&self, id: &SessionId, seed: &TargetSeed) -> TargetPublication {
        self.with_events_lock(id, || Ok(self.publish_target_already_locked(id, seed)))
            .unwrap_or_else(|error| TargetPublication::NotPublished {
                diagnostic: format!("the target lock could not be acquired: {error}"),
            })
    }

    fn publish_target_already_locked(
        &self,
        id: &SessionId,
        seed: &TargetSeed,
    ) -> TargetPublication {
        let event = seed.event();
        let mut first = Seq::ZERO;
        first.next();
        let valid_kind = match (&event.kind, seed.lineage()) {
            (
                EventKind::Forked { source, at_seq },
                SessionLineage::Fork {
                    source: lineage_source,
                    at_seq: lineage_seq,
                },
            ) => event.env.item.is_none() && *source == lineage_source && *at_seq == lineage_seq,
            (
                EventKind::CompactionSeeded {
                    source,
                    source_seq,
                    prompt_revision,
                    ..
                },
                SessionLineage::Compaction {
                    source: lineage_source,
                    source_seq: lineage_seq,
                    prompt_revision: lineage_revision,
                },
            ) => {
                event.env.item.is_some()
                    && *source == lineage_source
                    && *source_seq == lineage_seq
                    && *prompt_revision == lineage_revision
            }
            _ => false,
        };
        let valid_seed = event.env.seq == first
            && event.env.parent.is_none()
            && event.env.producer == Producer::Main
            && valid_kind;
        if !valid_seed {
            return TargetPublication::NotPublished {
                diagnostic: "a target requires one canonical typed sequence-1 seed event"
                    .to_string(),
            };
        }

        let log_path = self.log_path(id);
        match self.io.exists(&log_path) {
            Ok(false) => {}
            Ok(true) => {
                return TargetPublication::NotPublished {
                    diagnostic: "the target already has an event log".to_string(),
                };
            }
            Err(error) => {
                return TargetPublication::NotPublished {
                    diagnostic: format!("the target could not be inspected: {error}"),
                };
            }
        }

        let header = Header::new(*id, event.env.ts, self.workspace.clone());
        let mut log = encode_header(&header);
        log.push(b'\n');
        let record = match encode_record(first, &SessionEntry::Event(event.clone())) {
            Ok(record) => record,
            Err(error) => {
                return TargetPublication::NotPublished {
                    diagnostic: format!("the target seed could not be encoded: {error}"),
                };
            }
        };
        log.extend_from_slice(&record);
        log.push(b'\n');

        let publication = match self.io.replace_atomic_publication(&log_path, &log) {
            Ok(publication) => publication,
            Err(error) => {
                return TargetPublication::NotPublished {
                    diagnostic: format!("the target was not published: {error}"),
                };
            }
        };

        let manifest_diagnostic = self
            .write_manifest(id, event, true)
            .err()
            .map(|error| format!("manifest cache update failed: {error}"));
        match publication {
            super::io::AtomicReplacePublication::PublishedDurably => {
                TargetPublication::PublishedDurably {
                    diagnostic: manifest_diagnostic,
                }
            }
            super::io::AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                let diagnostic = match manifest_diagnostic {
                    Some(manifest) => format!("{diagnostic}; {manifest}"),
                    None => diagnostic,
                };
                TargetPublication::PublishedDurabilityUncertain { diagnostic }
            }
        }
    }

    /// Read and parse `id`'s manifest, or `None` if it does not exist yet.
    fn read_manifest(&self, id: &SessionId) -> Result<Option<RunManifest>, StoreError> {
        match self.io.read(&self.manifest_path(id))? {
            None => Ok(None),
            Some(bytes) => {
                let manifest = serde_json::from_slice(&bytes).map_err(StoreError::Manifest)?;
                Ok(Some(manifest))
            }
        }
    }

    fn snapshot_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("snapshot.json")
    }

    /// Write `state` as `id`'s snapshot, as of `source_seq`.
    ///
    /// Replaced atomically and durably, like the manifest: a half-written
    /// snapshot must never be readable, and one that survives a crash must
    /// be complete. Unlike the manifest, a torn snapshot would be *safe*
    /// anyway — [`JsonlStore::resume`] discards anything it cannot parse —
    /// but leaving a corrupt file on disk to be silently ignored on every
    /// later resume would turn a bounded replay into a full one forever,
    /// with nothing to show why.
    pub fn write_snapshot(
        &self,
        id: &SessionId,
        source_seq: Seq,
        state: &SessionState,
    ) -> Result<(), StoreError> {
        let snapshot = Snapshot::new(*id, source_seq, state.clone());
        let bytes = serde_json::to_vec(&snapshot).expect("Snapshot always serialises");
        self.io.replace_atomic(&self.snapshot_path(id), &bytes)?;
        Ok(())
    }

    /// Reduce `id`'s log to a [`SessionState`], using its snapshot when one
    /// can be trusted and replaying the whole log when it cannot.
    ///
    /// Returns the state and, when a snapshot was *not* used, the reason —
    /// so a caller can tell a bounded resume from a full replay, and log or
    /// act on a snapshot that keeps being rejected. A rejected snapshot is
    /// never an error: see this module's `snapshot` sibling for why the
    /// snapshot is a cache and the log is the authority.
    pub fn resume(
        &self,
        id: &SessionId,
    ) -> Result<(SessionState, Option<SnapshotRejection>), StoreError> {
        let events = self.read_events(id)?;

        let rejection = match self.read_snapshot(id) {
            Err(rejection) => Some(rejection),
            Ok(snapshot) => {
                match (events.first(), events.last()) {
                    // An empty log cannot corroborate any snapshot, and has
                    // nothing to replay either.
                    (None, _) | (_, None) => Some(SnapshotRejection::Absent),
                    (Some(first), Some(last)) => {
                        match validate(&snapshot, id, first.env.seq, last.env.seq) {
                            Err(rejection) => Some(rejection),
                            Ok(()) => {
                                let mut state = snapshot.state;
                                for ev in events.iter().filter(|e| e.env.seq > snapshot.source_seq)
                                {
                                    state.apply(ev);
                                }
                                return Ok((state, None));
                            }
                        }
                    }
                }
            }
        };

        let mut state = SessionState::default();
        for ev in &events {
            state.apply(ev);
        }
        Ok((state, rejection))
    }

    /// `id`'s snapshot, or why it cannot be used. Errors are rejections,
    /// not failures — every one of them means "replay the log instead".
    fn read_snapshot(&self, id: &SessionId) -> Result<Snapshot, SnapshotRejection> {
        let bytes = match self.io.read(&self.snapshot_path(id)) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err(SnapshotRejection::Absent),
            Err(e) => return Err(SnapshotRejection::Unreadable(e.to_string())),
        };
        serde_json::from_slice(&bytes)
            .map_err(|e| SnapshotRejection::Unreadable(e.to_string()))
            .and_then(|snapshot: Snapshot| {
                // Checked before anything else looks at the payload: a
                // future version's `state` may not even deserialise into
                // today's `SessionState`, so a parse failure above is
                // already the common outcome — this catches the case where
                // it happens to parse but must still not be trusted.
                if snapshot.v == SNAPSHOT_VERSION {
                    Ok(snapshot)
                } else {
                    Err(SnapshotRejection::Version { found: snapshot.v })
                }
            })
    }

    /// Every event in `id`'s log, in order, skipping any line that does not
    /// decode.
    ///
    /// Skipping is the **opposite** of `SessionStore::read`'s contract, which
    /// is explicit that "a corrupt or truncated line must never be silently
    /// skipped" (`session_store.rs`) — and this doc comment used to claim it
    /// mirrored that contract, which was backwards.
    ///
    /// What makes the skip safe is not the port, it is the caller: `recover`
    /// is what removes bad lines, and `JsonlStore::open_writer` runs it before
    /// `resume` for exactly this reason. A caller that reaches `resume` on an
    /// unrepaired log gets a state quietly missing its tail. That is the price
    /// of totality here, and it is stated rather than dressed up: replay must
    /// not abort on one bad byte, but the honest report of *which* lines
    /// failed lives on `read`, where the contract puts it.
    pub(super) fn read_events(&self, id: &SessionId) -> Result<Vec<DomainEvent>, StoreError> {
        let stream = SessionStore::read(self, id)?;
        Ok(futures::executor::block_on(async {
            use futures::StreamExt;
            stream
                .filter_map(|result| async move {
                    match result.ok()? {
                        SessionEntry::Event(event) => Some(event),
                        SessionEntry::RunTransaction(_) | SessionEntry::Evidence(_) => None,
                    }
                })
                .collect()
                .await
        }))
    }

    /// Bring `id`'s on-disk state back to something every later read can
    /// trust, and report what had to change.
    ///
    /// This is the recovery half of `ARCHITECTURE.md` §5 and
    /// `plans/m0a-contracts.md` Step 6, and it must be run before a session
    /// is read or appended to after a crash. It is idempotent: running it on
    /// an already-clean session returns a clean [`Repair`] and touches
    /// nothing, so a caller can run it unconditionally on open rather than
    /// having to first work out whether a crash happened — which is not
    /// knowable from the outside anyway.
    ///
    /// The three kill points, in the order they are repaired:
    ///
    /// 1. **Orphan scratch file** — a crash between `replace_atomic`'s write
    ///    and its rename leaves `manifest.json.tmp` behind. Removed first,
    ///    so a later manifest rebuild cannot trip over a stale one.
    /// 2. **Torn tail** — the log is truncated to its last complete, valid,
    ///    checksummed line (see [`valid_prefix`]).
    /// 3. **Stale or unreadable manifest** — projected from the valid log
    ///    prefix. This repairs both the ordinary "missing fsync window" and
    ///    an execution observation cached in the manifest whose log event was
    ///    removed with a damaged tail. The log is the authority; the manifest
    ///    is a derived cache.
    ///
    /// Note the deliberate absence of a fourth: an orphaned *lease* needs no
    /// repair here. The advisory lock behind `Lease` is owned by the open
    /// file description, so the kernel drops it when the holder's descriptor
    /// closes — including on `SIGKILL` — and a stale PID/heartbeat record is
    /// handled by `Lease::acquire`'s takeover path, not by rewriting files.
    /// Deleting a lease file here would be actively wrong: it would evict a
    /// *live* writer.
    pub fn recover(&self, id: &SessionId) -> Result<Repair, StoreError> {
        if !self.io.exists(&self.session_dir(id))? {
            return Ok(Repair::default());
        }
        self.with_events_lock(id, || self.recover_already_locked(id))
    }

    fn recover_already_locked(&self, id: &SessionId) -> Result<Repair, StoreError> {
        let mut repair = Repair::default();

        let manifest_path = self.manifest_path(id);
        let tmp_path = tmp_path_for(&manifest_path);
        if self.io.exists(&tmp_path)? {
            self.io.remove_file(&tmp_path)?;
            repair.removed_tmp = true;
        }

        let log_path = self.log_path(id);
        let Some(bytes) = self.io.read(&log_path)? else {
            // No log at all: nothing to repair, and nothing to rebuild a
            // manifest from. Not an error — `recover` is safe to call on a
            // session id that was never written.
            return Ok(repair);
        };

        // Before any truncation, and it must stay before: this is the one
        // question whose wrong answer costs the whole log rather than a torn
        // tail. An error here leaves the bytes exactly as they were found.
        // `None` is the interrupted first append — see `header_end` — where
        // truncating to nothing is right because there is nothing under it.
        let decoded = decoded_header(&bytes)?;
        let start = decoded.as_ref().map(|(_, end)| *end).unwrap_or(0);

        if let Some((header, _)) = decoded.as_ref() {
            if header.v == MIGRATABLE_CODEC_VERSION {
                let (migrated, valid_len, dropped_lines) =
                    migrate_v7_image(&bytes, header.clone(), start)?;
                if valid_len < bytes.len() {
                    repair.truncated_bytes = (bytes.len() - valid_len) as u64;
                    repair.dropped_lines = dropped_lines;
                }
                // One atomic publication both removes any torn v7 tail and
                // installs the v8 header/physical positions. No v8 writer ever
                // appends under a v7 header.
                self.io.replace_atomic(&log_path, &migrated)?;
                repair.migrated = true;
                tracing::warn!(
                    session = %session_dir_name(id),
                    from_codec = MIGRATABLE_CODEC_VERSION,
                    to_codec = CODEC_VERSION,
                    "migrated a session log to the current codec"
                );
                if self.rebuild_manifest_from_log(id, &migrated)? {
                    repair.manifest_rebuilt = true;
                }
                return Ok(repair);
            }
            if header.v != CODEC_VERSION {
                return Err(StoreError::UnsupportedLog { found: header.v });
            }
        }

        let (valid_len, dropped_lines) = valid_prefix(&bytes, start);
        validate_physical_positions(
            bytes[start..valid_len]
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty()),
        )?;
        if valid_len < bytes.len() {
            repair.truncated_bytes = (bytes.len() - valid_len) as u64;
            repair.dropped_lines = dropped_lines;
            tracing::warn!(
                session = %session_dir_name(id),
                truncated_bytes = repair.truncated_bytes,
                dropped_lines = repair.dropped_lines,
                "repairing a torn session log tail"
            );
            self.io.truncate(&log_path, valid_len as u64)?;
        }

        if self.rebuild_manifest_from_log(id, &bytes[..valid_len])? {
            repair.manifest_rebuilt = true;
        }

        Ok(repair)
    }

    /// Project the authoritative valid log prefix back into the private
    /// manifest. The comparison covers the observation as well as metadata:
    /// repair may remove an `ExecutionObserved` event without changing the
    /// surviving last event's timestamp relative to the cached manifest.
    fn rebuild_manifest_from_log(&self, id: &SessionId, log: &[u8]) -> Result<bool, StoreError> {
        let Some(facts) = log_manifest_facts(log)? else {
            return Ok(false);
        };

        // A manifest that fails to parse is treated exactly like a missing
        // one — it is a derived cache, and the log can regenerate it — but
        // an I/O error is propagated, because "cannot read this path" is
        // not evidence about the manifest's contents.
        let (previous, manifest_unreadable) = match self.read_manifest(id) {
            Ok(previous) => (previous, false),
            Err(StoreError::Manifest(_)) => (None, true),
            Err(e) => return Err(e),
        };

        let Some(last_ts) = facts.last_ts else {
            if previous.is_some() || manifest_unreadable {
                self.io.remove_file(&self.manifest_path(id))?;
                return Ok(true);
            }
            return Ok(false);
        };

        let meta = SessionMeta {
            id: *id,
            // From the log header, for exactly the reason `created` is —
            // and this is the whole point of the field being there. Carrying
            // it forward from the previous manifest, as this line used to,
            // meant a session whose manifest was lost came back without a
            // workspace and silently left the one it belonged to. The
            // recovery path that loses the manifest is the one directly
            // above.
            workspace_root: facts.workspace_root,
            // Only the latest authoritative main-generation admission owns
            // this projection. A prior cache value is not evidence and must
            // disappear when the log no longer supplies its source.
            model: facts.model,
            // `created` comes from the log header rather than the old
            // manifest: the header is written once, at session creation, and
            // never rewritten, whereas this manifest is a derived cache that
            // a rebuild can lose — so the header is the better authority for
            // a value that must never drift.
            //
            // This comment used to add "and is checksum-protected". It is
            // not: `encode_header` is a plain `serde_json::to_vec`
            // (`codec.rs:186-190`) and the checksum covers `StoreRecord`
            // only, which makes the header the *least* protected line in the
            // file. The conclusion survives on the write-once clause alone;
            // the deleted clause was never checked. ADR §2 of
            // `docs/decisions/2026-08-21-a-header-the-repair-must-not-delete.md`.
            created: facts.created,
            updated: last_ts,
            title: previous.as_ref().and_then(|m| m.meta.title.clone()),
            lineage: facts.lineage,
        };
        let manifest = RunManifest {
            meta,
            execution: facts.execution,
            hooks: facts.hooks,
            evidence: facts.evidence,
        };
        if previous.as_ref() == Some(&manifest) {
            return Ok(false);
        }

        tracing::warn!(
            session = %session_dir_name(id),
            "rebuilding a session manifest from its authoritative log"
        );
        let bytes = serde_json::to_vec(&manifest).expect("RunManifest always serialises");
        self.io.replace_atomic(&self.manifest_path(id), &bytes)?;
        Ok(true)
    }

    /// Replace `id`'s manifest, carrying its `created`/`title` and projected
    /// evidence forward from whatever was there before and updating `updated`
    /// to `ev`'s envelope timestamp. Model identity is re-derived from that
    /// evidence rather than trusted as an independent cache fact.
    fn write_manifest(
        &self,
        id: &SessionId,
        ev: &DomainEvent,
        is_new_session: bool,
    ) -> Result<(), StoreError> {
        let previous = self.read_manifest(id)?;
        // A missing cache is not evidence of an empty catalogue. Consult the
        // current authoritative log once while recreating it for an existing
        // session; a newly-created session has no earlier facts to recover,
        // and reading back the header and event just appended would turn its
        // first append into an unnecessary whole-log read.
        let log_facts = if previous.is_none() && !is_new_session {
            match self.io.read(&self.log_path(id))? {
                Some(bytes) => log_manifest_facts(&bytes)?,
                None => None,
            }
        } else {
            None
        };
        let workspace_root = if is_new_session {
            self.workspace.clone()
        } else {
            previous
                .as_ref()
                .map(|manifest| manifest.meta.workspace_root.clone())
                .or_else(|| log_facts.as_ref().map(|facts| facts.workspace_root.clone()))
                .ok_or(StoreError::MissingHeader)?
        };
        let meta = SessionMeta {
            id: *id,
            // Only an ordinary event-first session takes the constructor's
            // workspace. Once a header exists, preserve its projection: a
            // run-first header was authored from the admitted RunRoot and a
            // differently configured store must not overwrite that fact.
            workspace_root,
            model: previous
                .as_ref()
                .and_then(|manifest| {
                    manifest
                        .evidence
                        .generations()
                        .iter()
                        .rfind(|admission| admission.role() == GenerationRole::Main)
                        .map(|admission| admission.target().canonical_model().0.clone())
                })
                .or_else(|| log_facts.as_ref().and_then(|facts| facts.model.clone())),
            created: previous
                .as_ref()
                .map(|m| m.meta.created)
                .or_else(|| log_facts.as_ref().map(|facts| facts.created))
                .unwrap_or(ev.env.ts),
            updated: ev.env.ts,
            title: previous.as_ref().and_then(|m| m.meta.title.clone()),
            lineage: match &ev.kind {
                EventKind::CompactionSeeded {
                    source,
                    source_seq,
                    prompt_revision,
                    ..
                } => Some(SessionLineage::Compaction {
                    source: *source,
                    source_seq: *source_seq,
                    prompt_revision: *prompt_revision,
                }),
                EventKind::Forked { source, at_seq } => Some(SessionLineage::Fork {
                    source: *source,
                    at_seq: *at_seq,
                }),
                _ => previous
                    .as_ref()
                    .and_then(|manifest| manifest.meta.lineage)
                    .or_else(|| log_facts.as_ref().and_then(|facts| facts.lineage)),
            },
        };
        let execution = match &ev.kind {
            EventKind::ExecutionObserved { observation } => Some(observation.clone()),
            _ => previous
                .as_ref()
                .and_then(|manifest| manifest.execution.clone())
                .or_else(|| log_facts.as_ref().and_then(|facts| facts.execution.clone())),
        };
        let hooks = match &ev.kind {
            EventKind::HookCatalogueAdmitted { descriptors } => descriptors.clone(),
            _ => previous
                .as_ref()
                .map(|manifest| manifest.hooks.clone())
                .or_else(|| log_facts.as_ref().map(|facts| facts.hooks.clone()))
                .unwrap_or_default(),
        };
        let evidence = previous
            .as_ref()
            .map(|manifest| manifest.evidence.clone())
            .or_else(|| log_facts.as_ref().map(|facts| facts.evidence.clone()))
            .unwrap_or_default();
        let manifest = RunManifest {
            meta,
            execution,
            hooks,
            evidence,
        };
        let bytes = serde_json::to_vec(&manifest).expect("RunManifest always serialises");
        self.io.replace_atomic(&self.manifest_path(id), &bytes)?;
        Ok(())
    }

    fn next_physical_position(
        &self,
        log_path: &std::path::Path,
        id: &SessionId,
    ) -> Result<Seq, StoreError> {
        let header_line = self
            .io
            .read_first_line(log_path)?
            .ok_or_else(|| invalid_log("an existing session log is empty"))?;
        let header = match decode_line(&header_line)? {
            StoreLine::Header(header) => header,
            StoreLine::Record(_) | StoreLine::Unknown(_) => {
                return Err(StoreError::MissingHeader);
            }
        };
        if header.v != CODEC_VERSION {
            return Err(StoreError::UnsupportedLog { found: header.v });
        }
        if header.session != *id {
            return Err(StoreError::WrongSession {
                expected: header.session,
                asked: *id,
            });
        }

        let tail = self
            .io
            .read_last_line(log_path)?
            .ok_or_else(|| invalid_log("an existing session log is empty"))?;
        match decode_line(&tail)? {
            StoreLine::Record(record) => {
                let mut position = record.seq;
                Ok(position.next())
            }
            StoreLine::Header(_) => {
                let mut position = Seq::ZERO;
                Ok(position.next())
            }
            StoreLine::Unknown(_) => {
                Err(invalid_log("cannot append after an unknown session record"))
            }
        }
    }

    fn inspect_evidence_log(
        &self,
        id: &SessionId,
        bytes: &[u8],
    ) -> Result<(Vec<EvidenceRecord>, Seq), StoreError> {
        let (header, start) = decoded_header(bytes)?.ok_or(StoreError::MissingHeader)?;
        if header.v != CODEC_VERSION {
            return Err(StoreError::UnsupportedLog { found: header.v });
        }
        if header.session != *id {
            return Err(StoreError::WrongSession {
                expected: header.session,
                asked: *id,
            });
        }

        let mut evidence = Vec::new();
        let mut last_position = Seq::ZERO;
        for line in split_lines(&bytes[start..]) {
            let record = match decode_line(line)? {
                StoreLine::Record(record) => record,
                StoreLine::Header(_) => {
                    return Err(invalid_log(
                        "unexpected header record past the log's first line",
                    ));
                }
                StoreLine::Unknown(_) => {
                    return Err(invalid_log("unknown record in the valid session prefix"));
                }
            };
            accept_physical_position(&mut last_position, record.seq)?;
            let entry = serde_json::from_str::<SessionEntry>(record.payload.get())
                .map_err(|_| invalid_log("unknown payload in the valid session prefix"))?;
            if let SessionEntry::Evidence(record) = entry {
                evidence.push(record);
            }
        }
        Ok((evidence, last_position))
    }

    pub(super) fn write_evidence(
        &self,
        id: &SessionId,
        key: EvidenceKey,
        record: EvidenceRecord,
    ) -> Result<EvidenceWrite, EvidenceError> {
        let map_store = |error: StoreError| EvidenceError::Store {
            key: key.clone(),
            diagnostic: error.to_string(),
        };
        let mut lock = self.open_events_lock(id).map_err(map_store)?;
        let _guard = lock.write().map_err(|error| EvidenceError::Store {
            key: key.clone(),
            diagnostic: error.to_string(),
        })?;
        self.write_evidence_already_locked(id, key, record)
    }

    fn write_evidence_already_locked(
        &self,
        id: &SessionId,
        key: EvidenceKey,
        record: EvidenceRecord,
    ) -> Result<EvidenceWrite, EvidenceError> {
        if key != record.key() || key.session() != *id {
            return Err(EvidenceError::Store {
                key,
                diagnostic: "evidence key, payload, and lease-held session do not agree".to_owned(),
            });
        }
        let log_path = self.log_path(id);
        let existing = self
            .io
            .read(&log_path)
            .map_err(|error| EvidenceError::Store {
                key: key.clone(),
                diagnostic: error.to_string(),
            })?;
        let (records, mut last_position) = match existing.as_deref() {
            Some(bytes) => {
                self.inspect_evidence_log(id, bytes)
                    .map_err(|error| EvidenceError::Store {
                        key: key.clone(),
                        diagnostic: error.to_string(),
                    })?
            }
            None => (Vec::new(), Seq::ZERO),
        };

        if let Some(previous) = records.iter().find(|candidate| candidate.key() == key) {
            if previous == &record {
                return Ok(EvidenceWrite::AlreadyRecorded);
            }
            return Err(EvidenceError::idempotency_conflict(
                key,
                previous.digest(),
                record.digest(),
            ));
        }
        if let EvidenceKey::GenerationAdmission {
            session,
            invocation,
            ..
        } = &key
            && !records.iter().any(|candidate| {
                candidate.key()
                    == EvidenceKey::RunContext {
                        session: *session,
                        invocation: *invocation,
                    }
            })
        {
            return Err(EvidenceError::RunContextRequired { key: record.key() });
        }

        let position = last_position.next();
        let line = encode_record(position, &SessionEntry::Evidence(record)).map_err(|error| {
            EvidenceError::Store {
                key: key.clone(),
                diagnostic: error.to_string(),
            }
        })?;

        let (published, publication) = if let Some(mut bytes) = existing {
            self.io
                .append_line(&log_path, &line)
                .map_err(|error| EvidenceError::Store {
                    key: key.clone(),
                    diagnostic: error.to_string(),
                })?;
            bytes.extend_from_slice(&line);
            bytes.push(b'\n');
            match self.io.fsync(&log_path) {
                Ok(()) => (bytes, AtomicReplacePublication::PublishedDurably),
                Err(error) => {
                    return Err(EvidenceError::DurabilityUncertain {
                        key,
                        diagnostic: error.to_string(),
                    });
                }
            }
        } else {
            let header = Header::new(*id, Timestamp::EPOCH, self.workspace.clone());
            let mut image = encode_header(&header);
            image.push(b'\n');
            image.extend_from_slice(&line);
            image.push(b'\n');
            let publication = self
                .io
                .replace_atomic_publication(&log_path, &image)
                .map_err(|error| EvidenceError::Store {
                    key: key.clone(),
                    diagnostic: error.to_string(),
                })?;
            (image, publication)
        };

        if let AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } = publication {
            return Err(EvidenceError::DurabilityUncertain { key, diagnostic });
        }
        match self.rebuild_manifest_from_log(id, &published) {
            Ok(_) => Ok(EvidenceWrite::Published),
            Err(error) => Ok(EvidenceWrite::PublishedWithProjectionWarning {
                diagnostic: error.to_string(),
            }),
        }
    }

    fn append_already_locked(&self, id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
        let log_path = self.log_path(id);
        let is_new_session = !self.io.exists(&log_path)?;
        let position = if is_new_session {
            let header = Header::new(*id, ev.env.ts, self.workspace.clone());
            self.io.append_line(&log_path, &encode_header(&header))?;
            let mut first = Seq::ZERO;
            first.next()
        } else {
            self.next_physical_position(&log_path, id)?
        };

        let record_line = encode_record(position, &SessionEntry::Event(ev.clone()))?;
        self.io.append_line(&log_path, &record_line)?;

        let is_turn_boundary = matches!(ev.kind, EventKind::TurnCompleted { .. });
        let is_execution_observation = matches!(ev.kind, EventKind::ExecutionObserved { .. });
        let is_hook_admission = matches!(ev.kind, EventKind::HookCatalogueAdmitted { .. });
        let is_hook_invocation = matches!(
            ev.kind,
            EventKind::HookInvocationStarted { .. } | EventKind::HookInvocationSettled { .. }
        );
        let is_queue_boundary = matches!(
            ev.kind,
            EventKind::PromptQueued { .. }
                | EventKind::PromptDeliveryExpedited { .. }
                | EventKind::UserPrompt {
                    queued_prompt: Some(_),
                    ..
                }
        );
        if is_turn_boundary
            || is_execution_observation
            || is_hook_admission
            || is_hook_invocation
            || is_queue_boundary
        {
            self.io.fsync(&log_path)?;
        }

        if (is_new_session && !is_hook_invocation)
            || is_turn_boundary
            || is_execution_observation
            || is_hook_admission
        {
            self.write_manifest(id, ev, is_new_session)?;
        }

        Ok(())
    }

    fn compare_and_append_run_already_locked(
        &self,
        session: &SessionId,
        expected_revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        if transaction.run_id().session_id() != *session {
            return Err(StoreError::WrongSession {
                expected: transaction.run_id().session_id(),
                asked: *session,
            });
        }

        let log_path = self.log_path(session);
        let existing = self.io.read(&log_path)?;
        if existing.is_none() {
            let first_admission = matches!(
                transaction.command(),
                RunCommand::CommandAdmitted {
                    expected_revision: None,
                    start: true,
                    ..
                }
            );
            if expected_revision.is_some()
                || transaction.revision() != RunRevision::new(1)
                || !first_admission
            {
                return Err(StoreError::NotFound);
            }

            let header = Header::new(
                *session,
                transaction.observed_at(),
                transaction.root().as_path().to_path_buf(),
            );
            let mut image = encode_header(&header);
            image.push(b'\n');
            let mut record_seq = Seq::ZERO;
            let record_seq = record_seq.next();
            image.extend_from_slice(&encode_record(
                record_seq,
                &SessionEntry::RunTransaction(transaction.clone()),
            )?);
            image.push(b'\n');

            match self.io.replace_atomic_publication(&log_path, &image)? {
                AtomicReplacePublication::PublishedDurably => {
                    if let Err(error) = self.rebuild_manifest_from_log(session, &image) {
                        tracing::warn!(
                            session = %session_dir_name(session),
                            error = %error,
                            "run transaction committed but its manifest projection failed"
                        );
                    }
                    return Ok(RunAppend::Committed {
                        revision: transaction.revision(),
                    });
                }
                AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                    if let Err(error) = self.rebuild_manifest_from_log(session, &image) {
                        tracing::warn!(
                            session = %session_dir_name(session),
                            error = %error,
                            "uncertain run publication also failed its manifest projection"
                        );
                    }
                    return Ok(RunAppend::CommittedDurabilityUncertain { diagnostic });
                }
            }
        }

        let bytes = existing.expect("checked above");
        let mut lines = split_lines(&bytes).into_iter();
        let header = match lines.next().and_then(|line| decode_line(line).ok()) {
            Some(StoreLine::Header(header)) if header.v == CODEC_VERSION => header,
            _ => {
                header_end(&bytes)?;
                return Err(StoreError::MissingHeader);
            }
        };
        if header.session != *session {
            return Err(StoreError::WrongSession {
                expected: header.session,
                asked: *session,
            });
        }
        if header.workspace_root.as_path() != transaction.root().as_path() {
            return Err(invalid_log(
                "run transaction root does not match the session header",
            ));
        }
        let state = self.run_log_state_from_lines(lines)?;
        let committed = match expected_revision {
            None => {
                if let Some(actual) = state.revision {
                    return Ok(RunAppend::Conflict { actual });
                }
                let first_admission = matches!(
                    transaction.command(),
                    RunCommand::CommandAdmitted {
                        expected_revision: None,
                        start: true,
                        ..
                    }
                );
                if !state.has_event {
                    return Err(StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "an initial run append requires an absent or event-first session log",
                    )));
                }
                if transaction.revision() != RunRevision::new(1) || !first_admission {
                    return Err(invalid_log(
                        "event-first adoption requires a revision-one initial admission",
                    ));
                }
                RunRevision::new(1)
            }
            Some(expected) => {
                if state.revision != Some(expected) {
                    return state.revision.map_or(Err(StoreError::NotFound), |actual| {
                        Ok(RunAppend::Conflict { actual })
                    });
                }
                expected.next()
            }
        };
        if transaction.revision() != committed {
            return Err(invalid_log(
                "run transaction revision is not the expected successor",
            ));
        }

        let mut record_seq = state.last_position;
        let record_seq = record_seq.next();
        let line = encode_record(
            record_seq,
            &SessionEntry::RunTransaction(transaction.clone()),
        )?;
        self.io.append_line(&log_path, &line)?;
        let mut published = bytes;
        published.extend_from_slice(&line);
        published.push(b'\n');
        if let Err(error) = self.io.fsync(&log_path) {
            if let Err(manifest_error) = self.rebuild_manifest_from_log(session, &published) {
                tracing::warn!(
                    session = %session_dir_name(session),
                    error = %manifest_error,
                    "uncertain run append also failed its manifest projection"
                );
            }
            return Ok(RunAppend::CommittedDurabilityUncertain {
                diagnostic: error.to_string(),
            });
        }

        if let Err(error) = self.rebuild_manifest_from_log(session, &published) {
            tracing::warn!(
                session = %session_dir_name(session),
                error = %error,
                "run transaction committed but its manifest projection failed"
            );
        }
        Ok(RunAppend::Committed {
            revision: transaction.revision(),
        })
    }
}

impl SessionStore for JsonlStore {
    fn append(&self, id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
        self.with_events_lock(id, || self.append_already_locked(id, ev))
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let bytes = self
            .io
            .read(&self.log_path(id))?
            .ok_or(StoreError::NotFound)?;
        // The same check `recover` makes, through the same function, so a
        // header one of them accepts cannot be one the other rejects. An
        // unterminated first line is `None` there because it costs nothing to
        // discard; here it is simply not a header, which is what it always
        // was to a reader.
        if header_end(&bytes)?.is_none() {
            return Err(StoreError::MissingHeader);
        }
        let mut lines = split_lines(&bytes).into_iter();
        lines.next();

        // Read eagerly into an owned `Vec` rather than streaming
        // incrementally off disk: this store already reads the whole log
        // into memory to validate the header above, so a second, lazy pass
        // would buy nothing this milestone needs while entangling the
        // stream's lifetime with `&self`. Owning the decoded results makes
        // the stream `'static`, so a caller does not have to keep this
        // `JsonlStore` borrowed for as long as it holds the stream.
        let entries: Vec<Result<SessionEntry, StoreError>> = lines
            .map(|line| match decode_line(line) {
                Ok(StoreLine::Record(record)) => {
                    serde_json::from_str::<SessionEntry>(record.payload.get())
                        .map_err(|e| StoreError::Codec(CodecError::Malformed(e.to_string())))
                }
                Ok(StoreLine::Header(_)) => Err(StoreError::Codec(CodecError::Malformed(
                    "unexpected header record past the log's first line".to_string(),
                ))),
                Ok(StoreLine::Unknown(_)) => Err(StoreError::Codec(CodecError::Malformed(
                    "record line is neither a header nor a recognised record".to_string(),
                ))),
                Err(e) => Err(StoreError::Codec(e)),
            })
            .collect();

        Ok(Box::pin(stream::iter(entries)))
    }

    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        self.with_events_lock(session, || {
            self.compare_and_append_run_already_locked(session, expected_revision, transaction)
        })
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        let dirs = self.io.list_dirs(&self.root)?;
        let mut metas = Vec::new();
        for dir in dirs {
            let manifest_path = dir.join("manifest.json");
            // A catalogue that dies on one bad directory is a worse failure
            // mode than one that quietly hides a session: the event log's
            // own integrity is independently protected by `StoreRecord`
            // checksums (Step 3), and a corrupt or unreadable manifest is a
            // defect a later story can rebuild from the log
            // (`ARCHITECTURE.md` §5) rather than something that should take
            // down `list()` for every session. That reasoning applies
            // equally to an I/O error reading the manifest (e.g. a
            // permissions problem on one session directory) and to a parse
            // error on its contents — both are per-session failures, so
            // both are skipped and warned about the same way, rather than
            // one killing the whole call and the other not.
            let bytes = match self.io.read(&manifest_path) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue, // session directory exists, but no append has landed yet
                Err(error) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %error,
                        "skipping session with an unreadable manifest"
                    );
                    continue;
                }
            };
            let manifest = match serde_json::from_slice::<RunManifest>(&bytes) {
                Ok(manifest) => manifest,
                Err(error) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %error,
                        "skipping session with an unreadable manifest"
                    );
                    continue;
                }
            };
            // The id in the manifest has to be the id in the path, because
            // every other method here goes the other way: `read`, `append`
            // and `meta` all derive the directory from the id
            // (`session_dir`). An entry whose two halves disagree is an id
            // this method would hand out and those methods would then look
            // for somewhere else — `ego ls` printing a session `ego resume`
            // cannot open, or, if a directory of that name does exist,
            // opening a different session than the one listed.
            //
            // Nothing this crate writes can produce the mismatch; a person
            // copying a session directory to keep a snapshot can, and that
            // is exactly when a wrong answer would be trusted. Skipped
            // rather than repaired for the same reason the two arms above
            // are: `list` reports what is on disk and never edits it, and
            // which of the two names is the true one is not knowable from
            // here.
            if dir
                .file_name()
                .is_none_or(|name| name != &*session_dir_name(&manifest.meta.id))
            {
                tracing::warn!(
                    dir = %dir.display(),
                    manifest_id = %session_dir_name(&manifest.meta.id),
                    "skipping session whose manifest names a different id than its directory"
                );
                continue;
            }
            metas.push(manifest.meta);
        }
        Ok(metas)
    }
}

impl ReplaySource for JsonlStore {
    fn lineage(&self, session: &SessionId) -> Result<Option<SessionLineage>, ReplaySourceError> {
        let Some(entries) = ReplaySource::entries(self, session)? else {
            return Ok(None);
        };
        let mut events = entries.into_iter().filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(event),
            SessionEntry::RunTransaction(_) | SessionEntry::Evidence(_) => None,
        });
        let Some(first_event) = events.next() else {
            return Ok(None);
        };
        let mut first = Seq::ZERO;
        first.next();
        let lineage = match first_event.kind {
            EventKind::CompactionSeeded {
                source,
                source_seq,
                prompt_revision,
                ..
            } => Some(SessionLineage::Compaction {
                source,
                source_seq,
                prompt_revision,
            }),
            EventKind::Forked { source, at_seq } => Some(SessionLineage::Fork { source, at_seq }),
            _ => None,
        };
        if lineage.is_some() && first_event.env.seq != first {
            return Err(ReplaySourceError::CorruptSession {
                session: *session,
                reason: "lineage is not the semantic sequence-1 event".to_string(),
            });
        }
        if events.any(|event| {
            matches!(
                event.kind,
                EventKind::CompactionSeeded { .. } | EventKind::Forked { .. }
            )
        }) {
            return Err(ReplaySourceError::CorruptSession {
                session: *session,
                reason: "lineage is not the unique first semantic event".to_string(),
            });
        }
        Ok(lineage)
    }

    fn entries(&self, session: &SessionId) -> Result<Option<Vec<SessionEntry>>, ReplaySourceError> {
        let bytes = self.io.read(&self.log_path(session)).map_err(|error| {
            ReplaySourceError::Unavailable {
                session: *session,
                reason: error.to_string(),
            }
        })?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let (header, start) = decoded_header(&bytes)
            .map_err(|error| ReplaySourceError::CorruptSession {
                session: *session,
                reason: error.to_string(),
            })?
            .ok_or_else(|| ReplaySourceError::CorruptSession {
                session: *session,
                reason: "session log has no complete header".to_string(),
            })?;
        if header.v != CODEC_VERSION || header.session != *session {
            return Err(ReplaySourceError::CorruptSession {
                session: *session,
                reason: "session log header has the wrong codec or identity".to_string(),
            });
        }

        let mut entries = Vec::new();
        let mut expected = Seq::ZERO;
        for line in split_lines(&bytes[start..]) {
            let position = expected.next();
            let record = match decode_line(line) {
                Ok(StoreLine::Record(record)) if record.seq == position => record,
                Ok(StoreLine::Record(record)) => {
                    return Err(ReplaySourceError::CorruptSession {
                        session: *session,
                        reason: format!(
                            "physical position {:?} followed {:?}",
                            record.seq, expected
                        ),
                    });
                }
                Ok(StoreLine::Unknown(_)) => {
                    return Err(ReplaySourceError::UnknownEntry {
                        session: *session,
                        position,
                    });
                }
                Ok(StoreLine::Header(_)) => {
                    return Err(ReplaySourceError::CorruptSession {
                        session: *session,
                        reason: "session log contains a second header".to_string(),
                    });
                }
                Err(error) => {
                    return Err(ReplaySourceError::CorruptSession {
                        session: *session,
                        reason: error.to_string(),
                    });
                }
            };
            let entry =
                serde_json::from_str::<SessionEntry>(record.payload.get()).map_err(|_| {
                    ReplaySourceError::UnknownEntry {
                        session: *session,
                        position: record.seq,
                    }
                })?;
            entries.push(entry);
        }
        Ok(Some(entries))
    }
}

impl CompactionTargetPort for JsonlStore {
    fn inspect(
        &self,
        target: SessionId,
    ) -> Result<CompactionTargetInspection, CompactionTargetError> {
        let log_path = self.log_path(&target);
        if !self
            .io
            .exists(&log_path)
            .map_err(StoreError::Io)
            .map_err(compaction_target_error)?
        {
            return Ok(CompactionTargetInspection::Missing);
        }
        self.with_events_lock(&target, || self.inspect_compaction_target_locked(&target))
            .map_err(compaction_target_error)
    }

    fn publish(&self, target: SessionId, seed: &TargetSeed) -> TargetPublication {
        self.publish_target(&target, seed)
    }

    fn confirm(
        &self,
        target: SessionId,
        expected: CompactionTargetFacts,
    ) -> Result<TargetPublication, CompactionTargetError> {
        self.with_events_lock(&target, || {
            let actual = self.inspect_compaction_target_locked(&target)?;
            if actual != CompactionTargetInspection::Compaction(expected) {
                return Err(invalid_log(
                    "the deterministic compaction target has different authoritative facts",
                ));
            }

            let log_path = self.log_path(&target);
            let bytes = self
                .io
                .read(&log_path)?
                .ok_or_else(|| invalid_log("the deterministic compaction target is missing"))?;
            let manifest_diagnostic = self
                .rebuild_manifest_from_log(&target, &bytes)
                .err()
                .map(|error| format!("manifest cache update failed: {error}"));

            let barrier_diagnostic = self
                .io
                .fsync(&log_path)
                .and_then(|()| {
                    let parent = log_path.parent().ok_or_else(|| {
                        std::io::Error::other("the compaction target log has no parent directory")
                    })?;
                    self.io.fsync(parent)
                })
                .err()
                .map(|error| error.to_string());

            Ok(match barrier_diagnostic {
                Some(barrier) => TargetPublication::PublishedDurabilityUncertain {
                    diagnostic: match manifest_diagnostic {
                        Some(manifest) => format!("{barrier}; {manifest}"),
                        None => barrier,
                    },
                },
                None => TargetPublication::PublishedDurably {
                    diagnostic: manifest_diagnostic,
                },
            })
        })
        .map_err(compaction_target_error)
    }
}

impl JsonlStore {
    fn inspect_compaction_target_locked(
        &self,
        target: &SessionId,
    ) -> Result<CompactionTargetInspection, StoreError> {
        let log_path = self.log_path(target);
        let Some(bytes) = self.io.read(&log_path)? else {
            return Ok(CompactionTargetInspection::Missing);
        };
        if !bytes.ends_with(b"\n") {
            return Err(invalid_log(
                "the deterministic compaction target has an unterminated log tail",
            ));
        }

        let entries = ReplaySource::entries(self, target).map_err(|error| {
            invalid_log(format!(
                "the deterministic compaction target could not be decoded: {error}"
            ))
        })?;
        let Some(entries) = entries else {
            return Ok(CompactionTargetInspection::Missing);
        };
        let Some(SessionEntry::Event(first_event)) = entries.first() else {
            return Ok(CompactionTargetInspection::Occupied);
        };

        let mut first = Seq::ZERO;
        first.next();
        let EventKind::CompactionSeeded {
            source,
            source_seq,
            prompt_revision,
            ..
        } = &first_event.kind
        else {
            return Ok(CompactionTargetInspection::Occupied);
        };
        if first_event.env.seq != first
            || first_event.env.item.is_none()
            || first_event.env.parent.is_some()
            || first_event.env.producer != Producer::Main
        {
            return Err(invalid_log(
                "the deterministic compaction target has a non-canonical sequence-1 seed",
            ));
        }

        Ok(CompactionTargetInspection::Compaction(
            CompactionTargetFacts {
                source: *source,
                source_seq: *source_seq,
                prompt_revision: *prompt_revision,
            },
        ))
    }
}

fn compaction_target_error(error: StoreError) -> CompactionTargetError {
    CompactionTargetError {
        diagnostic: error.to_string(),
    }
}

impl EvidenceLogSource for JsonlStore {
    fn evidence_records(
        &self,
        session: &SessionId,
    ) -> Result<Vec<EvidenceRecord>, ManifestEvidenceError> {
        let map_store = |error: StoreError| ManifestEvidenceError::Store(error.to_string());
        let mut lock = self.open_events_lock(session).map_err(map_store)?;
        let _guard = lock
            .write()
            .map_err(|error| ManifestEvidenceError::Store(error.to_string()))?;
        let bytes = self
            .io
            .read(&self.log_path(session))
            .map_err(|error| ManifestEvidenceError::Store(error.to_string()))?
            .ok_or_else(|| ManifestEvidenceError::Store("session not found".to_owned()))?;
        let (records, _) = self
            .inspect_evidence_log(session, &bytes)
            .map_err(|error| ManifestEvidenceError::Store(error.to_string()))?;
        self.rebuild_manifest_from_log(session, &bytes)
            .map_err(|error| ManifestEvidenceError::Store(error.to_string()))?;
        Ok(records)
    }
}

impl SessionBlobSource for JsonlStore {
    fn read(
        &self,
        session: &SessionId,
        reference: &ego_core::store::blob::BlobRef,
    ) -> Result<Vec<u8>, BlobError> {
        let blobs = SessionBlobStore::unleased_with_io(
            &self.root,
            *session,
            reference.bytes.max(1),
            Arc::clone(&self.io),
        )?;
        BlobStore::read(&blobs, reference)
    }
}
