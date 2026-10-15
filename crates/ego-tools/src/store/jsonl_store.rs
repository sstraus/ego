//! `JsonlStore`: the filesystem `SessionStore` — a JSONL event log plus a
//! JSON manifest, per session directory (`ARCHITECTURE.md` §5).
//!
//! ```text
//! <root>/<session-id>/
//!     manifest.json      # private RunManifest, replaced atomically
//!     events.jsonl        # the log; first line is a versioned header record
//! ```

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ego_core::compact::{
    CompactionTargetError, CompactionTargetFacts, CompactionTargetInspection, CompactionTargetPort,
};
use ego_core::event::domain::{DomainEvent, EventKind, GenerationRole};
use ego_core::event::hook::HookCatalogueEvidence;
use ego_core::event::id::{Producer, Seq, SessionId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{
    ConfigProvenance, EvidenceError, EvidenceKey, EvidenceLogSource, EvidenceRecord, EvidenceWrite,
    ManifestEvidence, ManifestEvidenceError, RunInvocationId,
};
use ego_core::generator::attempt::AttemptRef;
use ego_core::permit::ExecObservation;
use ego_core::run::{RunAppend, RunCommand, RunRevision, RunTransaction};
use ego_core::session::{
    ReplaySource, ReplaySourceError, SessionBlobSource, SessionLineage, TargetPublication,
    TargetSeed,
};
use ego_core::session_lifecycle::{
    InactiveSessionLease, LeasedLifecycleAppend, LifecycleAppendError, LifecyclePublication,
    SessionLifecycleStore,
};
use ego_core::state::SessionState;
use ego_core::store::blob::{BlobError, BlobStore};
use ego_core::store::codec::{
    CODEC_VERSION, CodecError, Header, StoreLine, decode_line, encode_header, encode_record,
};
use ego_core::store::session_store::{
    SessionEntry, SessionMeta, SessionProfiles, SessionStore, StoreError,
};
use fd_lock::RwLock;
use futures::stream::{self, BoxStream};

use super::blob::SessionBlobStore;
use super::io::{AtomicReplacePublication, StoreIo, tmp_path_for};
use super::lease::Lease;
use super::snapshot::{SNAPSHOT_VERSION, Snapshot, SnapshotRejection, validate};

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
    #[serde(default)]
    deleted: bool,
}

impl RunManifest {
    fn omit_ci_prose(&mut self, log_evidence: Option<&ManifestEvidence>) {
        fn contains_ci(evidence: &ManifestEvidence) -> bool {
            evidence.run_contexts().iter().any(|context| {
                matches!(
                    context.config(),
                    ConfigProvenance::Resolved { profile: Some(profile) } if profile.name == "ci"
                )
            })
        }

        if contains_ci(&self.evidence) || log_evidence.is_some_and(contains_ci) {
            self.meta.title = None;
            self.meta.summary = None;
        }
    }
}

struct LogManifestFacts {
    profiles: SessionProfiles,
    created: Timestamp,
    workspace_root: PathBuf,
    last_ts: Option<Timestamp>,
    model: Option<String>,
    lineage: Option<SessionLineage>,
    execution: Option<ExecObservation>,
    hooks: HookCatalogueEvidence,
    evidence: ManifestEvidence,
    title: Option<String>,
    summary: Option<String>,
    summary_failures: u32,
    deleted: bool,
}

fn meta_from_log_facts(id: SessionId, facts: &LogManifestFacts) -> SessionMeta {
    SessionMeta {
        profiles: facts.profiles.clone(),
        id,
        workspace_root: facts.workspace_root.clone(),
        model: facts.model.clone(),
        created: facts.created,
        updated: facts.last_ts.unwrap_or(facts.created),
        title: facts.title.clone(),
        summary: facts.summary.clone(),
        summary_failures: facts.summary_failures,
        lineage: facts.lineage,
    }
}

/// Lease-local evidence facts derived from the repaired valid prefix.
/// The session log remains the only authority and this value is never stored.
#[derive(Debug, Default)]
pub(super) struct EvidenceIndex {
    records: HashMap<EvidenceKey, EvidenceRecord>,
    attempt_owners: HashMap<AttemptRef, RunInvocationId>,
}

impl EvidenceIndex {
    fn from_records(id: SessionId, records: Vec<EvidenceRecord>) -> Result<Self, StoreError> {
        let mut index = Self::default();
        for record in records {
            if let Some(error) = record.current_schema_error() {
                return Err(invalid_log(error.to_string()));
            }
            let key = record.key();
            if key.session() != id || index.records.contains_key(&key) {
                return Err(invalid_log(
                    "conflicting evidence key in the valid session prefix",
                ));
            }
            if let EvidenceKey::GenerationAdmission {
                session,
                invocation,
                attempt,
            } = key.clone()
            {
                if let EvidenceRecord::GenerationAdmission(admission) = &record
                    && let Some(digest) = admission.tool_catalogue()
                    && !index.records.contains_key(&EvidenceKey::ToolCatalogue {
                        session,
                        digest: digest.clone(),
                    })
                {
                    return Err(invalid_log(
                        "generation evidence precedes its tool catalogue",
                    ));
                }
                let context = EvidenceKey::RunContext {
                    session,
                    invocation,
                };
                if !index.records.contains_key(&context) {
                    return Err(invalid_log(
                        "generation evidence precedes its run context in the valid prefix",
                    ));
                }
                if index
                    .attempt_owners
                    .insert(attempt, invocation)
                    .is_some_and(|owner| owner != invocation)
                {
                    return Err(invalid_log(
                        "one attempt is owned by two invocations in the valid prefix",
                    ));
                }
            }
            index.records.insert(key, record);
        }
        Ok(index)
    }
}

#[derive(Debug, Clone, Copy)]
struct RunLogState {
    revision: Option<RunRevision>,
    last_position: Seq,
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
/// `ARCHITECTURE.md` §1.
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

fn lifecycle_publication(
    publication: AtomicReplacePublication,
    manifest_diagnostic: Option<String>,
) -> LifecyclePublication {
    match publication {
        AtomicReplacePublication::PublishedDurably => match manifest_diagnostic {
            Some(diagnostic) => LifecyclePublication::PublishedWithProjectionWarning { diagnostic },
            None => LifecyclePublication::Published,
        },
        AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
            let diagnostic = match manifest_diagnostic {
                Some(manifest) => format!("{diagnostic}; {manifest}"),
                None => diagnostic,
            };
            LifecyclePublication::PublishedDurabilityUncertain { diagnostic }
        }
    }
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

/// Validate the positions this codec can see. An unknown current record is kept
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
    let (created, workspace_root, profiles) = match decode_line(header_line) {
        Ok(StoreLine::Header(header)) => (header.created, header.workspace_root, header.profiles),
        _ => return Ok(None),
    };
    let mut last_ts = None;
    let mut model = None;
    let mut lineage = None;
    let mut execution = None;
    let mut hooks = HookCatalogueEvidence::default();
    let mut evidence_records = Vec::new();
    let mut state = SessionState::default();
    let mut deleted = false;
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
                state.apply(&event);
                match event.kind {
                    EventKind::SessionDeleted => deleted = true,
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
                    EventKind::Forked { source, at_seq, .. } => {
                        lineage = Some(SessionLineage::Fork { source, at_seq })
                    }
                    EventKind::ExecutionObserved { observation, .. } => {
                        execution = Some(observation)
                    }
                    EventKind::HookCatalogueAdmitted { descriptors } => hooks = descriptors,
                    _ => {}
                }
            }
            SessionEntry::ModelSelected(_) => {}
            SessionEntry::NoticeDelivered(notice) => {
                last_ts = Some(notice.delivered_at);
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
        profiles,
        created,
        workspace_root,
        last_ts,
        model,
        lineage,
        execution,
        hooks,
        evidence,
        title: state.title,
        summary: state.summary,
        summary_failures: state.summary_failures,
        deleted,
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

    /// Open the immutable evidence CAS without leasing or replaying the session writer.
    pub fn probe_evidence_store(
        &self,
        session: SessionId,
        hard_cap: u64,
    ) -> Result<super::SessionEvidenceStore, ego_core::store::evidence::EvidenceError> {
        super::SessionEvidenceStore::unleased_with_io(
            &self.root,
            session,
            hard_cap,
            Arc::clone(&self.io),
        )
    }

    /// Conversational search is a derived cache beside the authoritative logs.
    pub fn history(self: &Arc<Self>) -> super::HistoryIndex {
        super::HistoryIndex::new(self.root.join("history.sqlite"), Arc::clone(self))
    }

    pub(super) fn session_dir(&self, id: &SessionId) -> PathBuf {
        self.root.join(session_dir_name(id))
    }

    pub(super) fn log_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("events.jsonl")
    }

    /// Present once a refused session was hidden without its writer lease.
    fn quarantine_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("quarantined")
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
                SessionEntry::Event(_) => continue,
                SessionEntry::Evidence(_)
                | SessionEntry::NoticeDelivered(_)
                | SessionEntry::ModelSelected(_) => continue,
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
        let events: Vec<_> = seed.events().collect();
        let mut first = Seq::ZERO;
        first.next();
        let valid_kind = match (&event.kind, seed.lineage()) {
            (
                EventKind::Forked { source, at_seq, .. },
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
        let trailing = events.get(1..).unwrap_or_default();
        let is_title = |event: &&DomainEvent| {
            matches!(
                &event.kind,
                EventKind::SessionTitleChanged { title: Some(_) }
            )
        };
        let is_goal = |event: &&DomainEvent| {
            matches!(
                &event.kind,
                EventKind::GoalSet {
                    derived_from: Some(_),
                    ..
                }
            )
        };
        // Context metadata is an ordered suffix of the same atomic seed,
        // never an arbitrary additional conversation record.
        let (metadata, context) = match trailing.iter().position(|event| {
            matches!(
                event.kind,
                EventKind::ContextRequested { .. } | EventKind::ContextCooldown { .. }
            )
        }) {
            Some(index) => trailing.split_at(index),
            None => (trailing, &[][..]),
        };
        let valid_context = match context {
            [] => true,
            [cooldown] => matches!(cooldown.kind, EventKind::ContextCooldown { .. }),
            [request, consumed, cooldown] => {
                matches!(request.kind, EventKind::ContextRequested { .. })
                    && matches!(consumed.kind, EventKind::ContextRequestConsumed)
                    && matches!(cooldown.kind, EventKind::ContextCooldown { .. })
            }
            _ => false,
        };
        let valid_shape = valid_context
            && match metadata {
                [] => true,
                [only] => is_title(only) || is_goal(only),
                [title, goal] if is_title(title) => is_title(title) && is_goal(goal),
                [goal, allowance] => {
                    is_goal(goal)
                        && matches!((&goal.kind, &allowance.kind), (EventKind::GoalSet { id, .. }, EventKind::GoalAllowanceInherited { id: inherited, .. }) if id == inherited)
                }
                [title, goal, allowance] => {
                    is_title(title)
                        && is_goal(goal)
                        && matches!((&goal.kind, &allowance.kind), (EventKind::GoalSet { id, .. }, EventKind::GoalAllowanceInherited { id: inherited, .. }) if id == inherited)
                }
                _ => false,
            };
        let mut expected_seq = first;
        let mut previous_id = event.env.id;
        let valid_chain = trailing.iter().all(|trailing| {
            expected_seq.next();
            let valid = trailing.env.seq == expected_seq
                && trailing.env.parent == Some(previous_id)
                && trailing.env.turn == event.env.turn
                && trailing.env.item.is_none()
                && trailing.env.ts == event.env.ts
                && trailing.env.producer == Producer::Main;
            previous_id = trailing.env.id;
            valid
        });
        let valid_seed = event.env.seq == first
            && event.env.parent.is_none()
            && event.env.producer == Producer::Main
            && valid_kind
            && valid_shape
            && valid_chain;
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
        let mut position = Seq::ZERO;
        for event in events {
            let record = match encode_record(position.next(), &SessionEntry::Event(event.clone())) {
                Ok(record) => record,
                Err(error) => {
                    return TargetPublication::NotPublished {
                        diagnostic: format!("the target seed could not be encoded: {error}"),
                    };
                }
            };
            log.extend_from_slice(&record);
            log.push(b'\n');
        }

        let publication = match self.io.replace_atomic_publication(&log_path, &log) {
            Ok(publication) => publication,
            Err(error) => {
                return TargetPublication::NotPublished {
                    diagnostic: format!("the target was not published: {error}"),
                };
            }
        };

        let manifest_diagnostic = self
            .rebuild_manifest_from_log(id, &log)
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
        #[derive(serde::Deserialize)]
        struct SnapshotVersion {
            v: u32,
        }

        let version: SnapshotVersion = serde_json::from_slice(&bytes)
            .map_err(|error| SnapshotRejection::Unreadable(error.to_string()))?;
        if version.v != SNAPSHOT_VERSION {
            return Err(SnapshotRejection::Version { found: version.v });
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| SnapshotRejection::Unreadable(error.to_string()))
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
                        SessionEntry::RunTransaction(_)
                        | SessionEntry::Evidence(_)
                        | SessionEntry::NoticeDelivered(_)
                        | SessionEntry::ModelSelected(_) => None,
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

        if let Some((header, _)) = decoded.as_ref()
            && header.v != CODEC_VERSION
        {
            return Err(StoreError::UnsupportedLog { found: header.v });
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
        let Some(facts) = self.catalogue_facts(log)? else {
            return Ok(false);
        };

        // A manifest that fails to parse is treated exactly like a missing
        // one — it is a derived cache, and the log can regenerate it — but
        // an I/O error is propagated, because "cannot read this path" is
        // not evidence about the manifest's contents.
        let previous = match self.read_manifest(id) {
            Ok(previous) => previous,
            Err(StoreError::Manifest(_)) => None,
            Err(e) => return Err(e),
        };

        let projected_meta = meta_from_log_facts(*id, &facts);
        let meta = SessionMeta {
            profiles: projected_meta.profiles,
            id: projected_meta.id,
            // From the log header, for exactly the reason `created` is —
            // and this is the whole point of the field being there. Carrying
            // it forward from the previous manifest, as this line used to,
            // meant a session whose manifest was lost came back without a
            // workspace and silently left the one it belonged to. The
            // recovery path that loses the manifest is the one directly
            // above.
            workspace_root: projected_meta.workspace_root,
            // Main-generation evidence owns the model, including inherited
            // evidence at the durable lineage cutoff before the first turn.
            model: projected_meta.model,
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
            // `ARCHITECTURE.md`.
            created: projected_meta.created,
            updated: projected_meta.updated,
            title: projected_meta.title,
            summary: projected_meta.summary,
            summary_failures: projected_meta.summary_failures,
            lineage: projected_meta.lineage,
        };
        let mut manifest = RunManifest {
            meta,
            execution: facts.execution,
            hooks: facts.hooks,
            evidence: facts.evidence,
            deleted: facts.deleted,
        };
        manifest.omit_ci_prose(None);
        if previous.as_ref() == Some(&manifest) {
            return Ok(false);
        }

        // `append` rewrites the manifest only at boundaries, so after any
        // ordinary session the cached `updated` trails the last event. That
        // is the steady state, not damage: refresh it, but do not report it
        // as a repair. A manifest *ahead* of its log is not lag: the log
        // lost events the manifest saw.
        let only_updated_trails = previous.as_ref().is_some_and(|previous| {
            let mut current = manifest.clone();
            current.meta.updated = previous.meta.updated;
            current == *previous && previous.meta.updated < manifest.meta.updated
        });
        if !only_updated_trails {
            tracing::warn!(
                session = %session_dir_name(id),
                "rebuilding a session manifest from its authoritative log"
            );
        }
        let bytes = serde_json::to_vec(&manifest).expect("RunManifest always serialises");
        self.io.replace_atomic(&self.manifest_path(id), &bytes)?;
        Ok(!only_updated_trails)
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
        let refresh_synopsis = matches!(
            ev.kind,
            EventKind::TurnCompleted { .. }
                | EventKind::CompactionSeeded { .. }
                | EventKind::Compacted { .. }
        );
        let log_facts = if (previous.is_none() && !is_new_session) || refresh_synopsis {
            match self.io.read(&self.log_path(id))? {
                Some(bytes) => self.catalogue_facts(&bytes)?,
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
            profiles: previous
                .as_ref()
                .map(|manifest| manifest.meta.profiles.clone())
                .or_else(|| log_facts.as_ref().map(|facts| facts.profiles.clone()))
                .unwrap_or_default(),
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
                .or_else(|| log_facts.as_ref().and_then(|facts| facts.model.clone()))
                .map(|model| Ok(Some(model)))
                .unwrap_or_else(|| {
                    self.inherited_model(previous.as_ref().and_then(|m| m.meta.lineage))
                })?,
            created: previous
                .as_ref()
                .map(|m| m.meta.created)
                .or_else(|| log_facts.as_ref().map(|facts| facts.created))
                .unwrap_or(ev.env.ts),
            updated: ev.env.ts,
            title: match &ev.kind {
                EventKind::SessionTitleChanged { title } => title.clone(),
                _ => log_facts
                    .as_ref()
                    .and_then(|facts| facts.title.clone())
                    .or_else(|| {
                        previous
                            .as_ref()
                            .and_then(|manifest| manifest.meta.title.clone())
                    }),
            },
            summary: log_facts
                .as_ref()
                .and_then(|facts| facts.summary.clone())
                .or_else(|| {
                    previous
                        .as_ref()
                        .and_then(|manifest| manifest.meta.summary.clone())
                }),
            summary_failures: log_facts.as_ref().map_or_else(
                || {
                    previous
                        .as_ref()
                        .map_or(0, |manifest| manifest.meta.summary_failures)
                },
                |facts| facts.summary_failures,
            ),
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
                EventKind::Forked { source, at_seq, .. } => Some(SessionLineage::Fork {
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
            EventKind::ExecutionObserved { observation, .. } => Some(observation.clone()),
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
        let mut manifest = RunManifest {
            meta,
            execution,
            hooks,
            evidence,
            deleted: matches!(ev.kind, EventKind::SessionDeleted)
                || previous.as_ref().is_some_and(|manifest| manifest.deleted)
                || log_facts.as_ref().is_some_and(|facts| facts.deleted),
        };
        manifest.omit_ci_prose(log_facts.as_ref().map(|facts| &facts.evidence));
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

    pub(super) fn evidence_index(&self, id: &SessionId) -> Result<EvidenceIndex, StoreError> {
        let Some(bytes) = self.io.read(&self.log_path(id))? else {
            return Ok(EvidenceIndex::default());
        };
        let (records, _) = self.inspect_evidence_log(id, &bytes)?;
        EvidenceIndex::from_records(*id, records)
    }

    pub(super) fn write_evidence(
        &self,
        id: &SessionId,
        index: &Mutex<EvidenceIndex>,
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
        let mut index = index.lock().map_err(|_| EvidenceError::Store {
            key: key.clone(),
            diagnostic: "lease-local evidence index lock is poisoned".to_owned(),
        })?;
        self.write_evidence_already_locked(id, &mut index, key, record)
    }

    fn write_evidence_already_locked(
        &self,
        id: &SessionId,
        index: &mut EvidenceIndex,
        key: EvidenceKey,
        record: EvidenceRecord,
    ) -> Result<EvidenceWrite, EvidenceError> {
        if key != record.key() || key.session() != *id {
            return Err(EvidenceError::Store {
                key,
                diagnostic: "evidence key, payload, and lease-held session do not agree".to_owned(),
            });
        }
        if let Some(error) = record.current_schema_error() {
            return Err(error);
        }
        if let Some(previous) = index.records.get(&key) {
            if previous == &record {
                return Ok(EvidenceWrite::AlreadyRecorded);
            }
            return Err(EvidenceError::idempotency_conflict(
                key,
                previous.digest(),
                record.digest(),
            ));
        }
        if let EvidenceRecord::GenerationAdmission(admission) = &record
            && let Some(digest) = admission.tool_catalogue()
            && !index.records.contains_key(&EvidenceKey::ToolCatalogue {
                session: *id,
                digest: digest.clone(),
            })
        {
            return Err(EvidenceError::Store {
                key: key.clone(),
                diagnostic: "generation evidence requires its durable tool catalogue".to_owned(),
            });
        }
        if let EvidenceKey::GenerationAdmission {
            session,
            invocation,
            attempt,
        } = &key
        {
            let context = EvidenceKey::RunContext {
                session: *session,
                invocation: *invocation,
            };
            if !index.records.contains_key(&context) {
                return Err(EvidenceError::RunContextRequired { key: record.key() });
            }
            if let Some(owner) = index.attempt_owners.get(attempt)
                && owner != invocation
            {
                return Err(EvidenceError::AttemptOwnershipConflict {
                    session: *session,
                    attempt: *attempt,
                    existing_invocation: *owner,
                    requested_invocation: *invocation,
                });
            }
        }

        let log_path = self.log_path(id);
        let is_new_session = !self
            .io
            .exists(&log_path)
            .map_err(|error| EvidenceError::Store {
                key: key.clone(),
                diagnostic: error.to_string(),
            })?;
        let position = if is_new_session {
            let mut first = Seq::ZERO;
            first.next()
        } else {
            self.next_physical_position(&log_path, id)
                .map_err(|error| EvidenceError::Store {
                    key: key.clone(),
                    diagnostic: error.to_string(),
                })?
        };
        let line =
            encode_record(position, &SessionEntry::Evidence(record.clone())).map_err(|error| {
                EvidenceError::Store {
                    key: key.clone(),
                    diagnostic: error.to_string(),
                }
            })?;

        let publication = if !is_new_session {
            self.io
                .append_line(&log_path, &line)
                .map_err(|error| EvidenceError::Store {
                    key: key.clone(),
                    diagnostic: error.to_string(),
                })?;
            index.records.insert(key.clone(), record.clone());
            if let EvidenceKey::GenerationAdmission {
                invocation,
                attempt,
                ..
            } = &key
            {
                index.attempt_owners.insert(*attempt, *invocation);
            }
            match self.io.fsync(&log_path) {
                Ok(()) => AtomicReplacePublication::PublishedDurably,
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
            index.records.insert(key.clone(), record.clone());
            if let EvidenceKey::GenerationAdmission {
                invocation,
                attempt,
                ..
            } = &key
            {
                index.attempt_owners.insert(*attempt, *invocation);
            }
            publication
        };

        if let AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } = publication {
            return Err(EvidenceError::DurabilityUncertain { key, diagnostic });
        }
        match self.write_evidence_manifest(id, &record, is_new_session) {
            Ok(_) => Ok(EvidenceWrite::Published),
            Err(error) => Ok(EvidenceWrite::PublishedWithProjectionWarning {
                diagnostic: error.to_string(),
            }),
        }
    }

    fn write_evidence_manifest(
        &self,
        id: &SessionId,
        record: &EvidenceRecord,
        is_new_session: bool,
    ) -> Result<(), StoreError> {
        let previous = self.read_manifest(id)?;
        let mut manifest = match previous {
            Some(manifest) => manifest,
            None if is_new_session => RunManifest {
                meta: SessionMeta {
                    profiles: Default::default(),
                    id: *id,
                    workspace_root: self.workspace.clone(),
                    model: None,
                    created: Timestamp::EPOCH,
                    updated: Timestamp::EPOCH,
                    title: None,
                    summary: None,
                    summary_failures: 0,
                    lineage: None,
                },
                execution: None,
                hooks: HookCatalogueEvidence::default(),
                evidence: ManifestEvidence::default(),
                deleted: false,
            },
            None => {
                return Err(StoreError::Io(std::io::Error::other(
                    "manifest cache disappeared while the writer lease was held",
                )));
            }
        };
        let mut records = manifest
            .evidence
            .run_contexts()
            .iter()
            .cloned()
            .map(EvidenceRecord::RunContext)
            .chain(
                manifest
                    .evidence
                    .generations()
                    .iter()
                    .cloned()
                    .map(EvidenceRecord::GenerationAdmission),
            )
            .collect::<Vec<_>>();
        records.push(record.clone());
        manifest.evidence = ManifestEvidence::from_records(&records)
            .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?;
        if let EvidenceRecord::GenerationAdmission(admission) = record
            && admission.role() == GenerationRole::Main
        {
            manifest.meta.model = Some(admission.target().canonical_model().0.clone());
        }
        manifest.omit_ci_prose(None);
        let bytes = serde_json::to_vec(&manifest).expect("RunManifest always serialises");
        self.io.replace_atomic(&self.manifest_path(id), &bytes)?;
        Ok(())
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
        let is_execution_measurement = matches!(ev.kind, EventKind::ExecutionMeasured { .. });
        let is_hook_admission = matches!(ev.kind, EventKind::HookCatalogueAdmitted { .. });
        let is_hook_invocation = matches!(
            ev.kind,
            EventKind::HookLifecycleReached { .. }
                | EventKind::HookInvocationStarted { .. }
                | EventKind::HookInvocationSettled { .. }
        );
        let is_skill_admission = matches!(
            ev.kind,
            EventKind::SkillCatalogueAdmitted { .. }
                | EventKind::SkillInstructionAdmitted { .. }
                | EventKind::SkillResourceAdmitted { .. }
        );
        let is_queue_boundary = matches!(
            ev.kind,
            EventKind::PromptQueued { .. }
                | EventKind::PromptDeliveryExpedited { .. }
                | EventKind::PromptRetracted { .. }
                | EventKind::UserPrompt {
                    queued_prompt: Some(_),
                    ..
                }
        );
        if is_turn_boundary
            || is_execution_observation
            || is_execution_measurement
            || is_hook_admission
            || is_hook_invocation
            || is_skill_admission
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
                if transaction.revision() != RunRevision::new(1) || !first_admission {
                    return Err(invalid_log(
                        "durable session adoption requires a revision-one initial admission",
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

struct InactiveLifecycleAppend<'a> {
    store: &'a JsonlStore,
    id: SessionId,
    _lease: Lease,
}

impl LeasedLifecycleAppend for InactiveLifecycleAppend<'_> {
    fn session_id(&self) -> SessionId {
        self.id
    }

    fn compare_and_append_lifecycle(
        &self,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        self.store
            .compare_and_append_lifecycle_leased(self.id, expected, event)
    }
}

impl InactiveSessionLease for InactiveLifecycleAppend<'_> {}

impl SessionStore for JsonlStore {
    fn open_writer_lease(
        &self,
        id: &SessionId,
        now: ego_core::event::time::Timestamp,
    ) -> Result<ego_core::store::session_store::SessionWriterLease<'_>, StoreError> {
        let opened = self
            .open_writer(
                id,
                super::LeaseInputs {
                    pid: std::process::id(),
                    now,
                    stale_after: std::time::Duration::from_secs(60),
                    // Kernel lease release handles dead processes. Never steal a live lock.
                    is_alive: &|_| true,
                },
            )
            .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?;
        Ok(ego_core::store::session_store::SessionWriterLease {
            state: opened.state,
            writer: Box::new(opened.writer),
        })
    }

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
            let cached = match self.io.read(&manifest_path) {
                Ok(Some(bytes)) => serde_json::from_slice::<RunManifest>(&bytes)
                    .map_err(|error| error.to_string())
                    .and_then(|manifest| {
                        // A manifest naming another session is as unusable
                        // as a corrupt one: the log decides.
                        if dir
                            .file_name()
                            .is_some_and(|name| name == &*session_dir_name(&manifest.meta.id))
                        {
                            Ok((manifest.meta, manifest.deleted))
                        } else {
                            Err("the manifest names a different session".to_owned())
                        }
                    }),
                Ok(None) => Err("the manifest is missing".to_owned()),
                Err(error) => Err(error.to_string()),
            };
            // The manifest is a derived cache and the log is the authority,
            // so a session whose manifest is missing, unreadable or corrupt
            // is still listed — from its log, without editing anything on
            // disk. A directory with no readable log either is skipped.
            let (meta, deleted) = match cached {
                Ok(listed) => listed,
                Err(reason) => match self.meta_from_log_prefix(&dir) {
                    Some(listed) => listed,
                    None => {
                        if self.io.exists(&dir.join("events.jsonl")).unwrap_or(false) {
                            tracing::warn!(
                                dir = %dir.display(),
                                reason = %reason,
                                "skipping session with neither a usable manifest nor a usable log"
                            );
                        }
                        continue;
                    }
                },
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
                .is_none_or(|name| name != &*session_dir_name(&meta.id))
            {
                tracing::warn!(
                    dir = %dir.display(),
                    manifest_id = %session_dir_name(&meta.id),
                    "skipping session whose manifest names a different id than its directory"
                );
                continue;
            }
            // A quarantined session is refused for good, so no catalogue
            // reader may see it, whichever of manifest or log named it.
            if self.io.exists(&self.quarantine_path(&meta.id))? {
                continue;
            }
            if !deleted {
                metas.push(meta);
            }
        }
        Ok(metas)
    }
}

impl JsonlStore {
    /// Read the last explicit model choice from the authoritative session stream.
    pub fn selected_model(
        &self,
        session: &SessionId,
    ) -> Result<Option<ego_core::config::ModelRef>, StoreError> {
        use futures::StreamExt;
        let entries =
            futures::executor::block_on(SessionStore::read(self, session)?.collect::<Vec<_>>());
        let mut selected = None;
        for entry in entries {
            if let SessionEntry::ModelSelected(model) = entry? {
                selected = Some(model);
            }
        }
        Ok(selected)
    }

    /// Publish an operator's model choice before acknowledging it to the host.
    pub fn record_model_selection(
        &self,
        session: &SessionId,
        model: ego_core::config::ModelRef,
    ) -> Result<(), StoreError> {
        self.with_events_lock(session, || {
            let log = self.log_path(session);
            if !self.io.exists(&log)? {
                return Err(StoreError::NotFound);
            }
            let position = self.next_physical_position(&log, session)?;
            let line = encode_record(position, &SessionEntry::ModelSelected(model))?;
            self.io.append_line(&log, &line)?;
            self.io.fsync(&log)?;
            Ok(())
        })
    }

    /// Persist ACP transport acceptance under the same short append lock as turns.
    /// No synthetic conversation turn or second evidence file is created.
    pub fn record_notice(
        &self,
        session: &SessionId,
        notice: ego_core::store::session_store::NoticeDelivery,
    ) -> Result<(), StoreError> {
        self.with_events_lock(session, || {
            let log = self.log_path(session);
            if !self.io.exists(&log)? {
                return Err(StoreError::NotFound);
            }
            let position = self.next_physical_position(&log, session)?;
            let line = encode_record(position, &SessionEntry::NoticeDelivered(notice))?;
            self.io.append_line(&log, &line)?;
            self.io.fsync(&log)?;
            Ok(())
        })
    }
}

impl JsonlStore {
    fn catalogue_facts(&self, log: &[u8]) -> Result<Option<LogManifestFacts>, StoreError> {
        let Some(mut facts) = log_manifest_facts(log)? else {
            return Ok(None);
        };
        if facts.model.is_none() {
            facts.model = self.inherited_model(facts.lineage)?;
        }
        Ok(Some(facts))
    }

    /// Project the last main model visible at each durable ancestry cutoff.
    /// Target admissions take precedence; manifests never supply authority.
    fn inherited_model(
        &self,
        mut lineage: Option<SessionLineage>,
    ) -> Result<Option<String>, StoreError> {
        let mut visited = HashSet::new();
        while let Some(edge) = lineage {
            let (source, cutoff) = match edge {
                SessionLineage::Fork { source, at_seq } => (source, at_seq),
                SessionLineage::Compaction {
                    source, source_seq, ..
                } => (source, source_seq),
            };
            if !visited.insert(source) {
                return Err(invalid_log("model projection encountered a lineage cycle"));
            }
            let Some(entries) = ReplaySource::entries(self, &source)
                .map_err(|error| invalid_log(error.to_string()))?
            else {
                return Ok(None);
            };
            lineage = None;
            let mut model = None;
            for entry in entries {
                match entry {
                    SessionEntry::Event(event) => {
                        if event.env.seq > cutoff {
                            break;
                        }
                        match event.kind {
                            EventKind::Forked { source, at_seq, .. } => {
                                lineage = Some(SessionLineage::Fork { source, at_seq });
                            }
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
                                });
                            }
                            _ => {}
                        }
                    }
                    SessionEntry::Evidence(EvidenceRecord::GenerationAdmission(admission))
                        if admission.role() == GenerationRole::Main =>
                    {
                        model = Some(admission.target().canonical_model().0.clone());
                    }
                    _ => {}
                }
            }
            if model.is_some() {
                return Ok(model);
            }
        }
        Ok(None)
    }

    /// The listing facts of the session in `dir`, projected from the valid
    /// prefix of its log. `None` when there is no readable log; a torn tail
    /// is ignored rather than refused, because listing never edits a session.
    fn meta_from_log_prefix(&self, dir: &std::path::Path) -> Option<(SessionMeta, bool)> {
        let bytes = self.io.read(&dir.join("events.jsonl")).ok()??;
        let (header, start) = decoded_header(&bytes).ok()??;
        if header.v != CODEC_VERSION {
            return None;
        }
        let (valid_len, _) = valid_prefix(&bytes, start);
        let facts = self.catalogue_facts(&bytes[..valid_len]).ok()??;
        Some((meta_from_log_facts(header.session, &facts), facts.deleted))
    }

    fn publish_empty_session_lifecycle(
        &self,
        id: SessionId,
        root: &std::path::Path,
        created: Timestamp,
        profiles: SessionProfiles,
    ) -> Result<LifecyclePublication, StoreError> {
        self.with_events_lock(&id, || {
            let log_path = self.log_path(&id);
            if self.io.exists(&log_path)? {
                return Ok(LifecyclePublication::NotPublished {
                    diagnostic: "the session already has an event log".to_owned(),
                });
            }
            let mut header = Header::new(id, created, root.to_path_buf());
            header.profiles = profiles;
            let mut image = encode_header(&header);
            image.push(b'\n');
            let publication = self.io.replace_atomic_publication(&log_path, &image)?;
            let manifest_diagnostic =
                self.rebuild_manifest_from_log(&id, &image)
                    .err()
                    .map(|error| {
                        tracing::warn!(
                            session = %session_dir_name(&id),
                            error = %error,
                            "created session is visible but its manifest projection failed"
                        );
                        format!("manifest cache update failed: {error}")
                    });
            Ok(lifecycle_publication(publication, manifest_diagnostic))
        })
    }
}

impl JsonlStore {
    pub(super) fn compare_and_append_lifecycle_leased(
        &self,
        id: SessionId,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        self.with_events_lock(&id, || {
            let log_path = self.log_path(&id);
            let bytes = self.io.read(&log_path)?.ok_or(StoreError::NotFound)?;
            let (header, start) = decoded_header(&bytes)?.ok_or(StoreError::MissingHeader)?;
            if header.v != CODEC_VERSION {
                return Err(StoreError::UnsupportedLog { found: header.v });
            }
            if header.session != id {
                return Err(StoreError::WrongSession {
                    expected: header.session,
                    asked: id,
                });
            }
            let (valid_len, _) = valid_prefix(&bytes, start);
            if valid_len != bytes.len() {
                return Err(invalid_log("session log has a corrupt or torn tail"));
            }

            let mut physical = Seq::ZERO;
            let mut semantic = Seq::ZERO;
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
                accept_physical_position(&mut physical, record.seq)?;
                let entry = serde_json::from_str::<SessionEntry>(record.payload.get())
                    .map_err(|_| invalid_log("unknown payload in the valid session prefix"))?;
                if let SessionEntry::Event(previous) = entry {
                    let wanted = semantic.next();
                    if previous.env.seq != wanted {
                        return Err(invalid_log("semantic event sequence is not contiguous"));
                    }
                }
            }
            if semantic != expected {
                return Ok(Err(LifecycleAppendError::Conflict));
            }
            let mut next_semantic = expected;
            let valid_event = event.env.seq == next_semantic.next()
                && event.env.item.is_none()
                && event.env.parent.is_none()
                && event.env.producer == Producer::Main
                && matches!(
                    &event.kind,
                    EventKind::SessionTitleChanged { .. }
                        | EventKind::SessionDeleted
                        | EventKind::GoalSet { .. }
                        | EventKind::GoalTransitioned { .. }
                        // A hold is written by whoever holds the writer lease,
                        // which is exactly this path: the two appends of the
                        // pause pair and the release that ends it.
                        | EventKind::SessionPauseRequested { .. }
                        | EventKind::SessionPaused { .. }
                        | EventKind::SessionPauseObserved { .. }
                        | EventKind::SessionResumed { .. }
                );
            if !valid_event {
                return Err(invalid_log("invalid lifecycle event append"));
            }

            let mut image = bytes;
            image.extend_from_slice(&encode_record(
                physical.next(),
                &SessionEntry::Event(event),
            )?);
            image.push(b'\n');
            let publication = self.io.replace_atomic_publication(&log_path, &image)?;
            let manifest_diagnostic =
                self.rebuild_manifest_from_log(&id, &image)
                    .err()
                    .map(|error| {
                        tracing::warn!(
                            session = %session_dir_name(&id),
                            error = %error,
                            "lifecycle event is visible but its manifest projection failed"
                        );
                        format!("manifest cache update failed: {error}")
                    });
            Ok(Ok(lifecycle_publication(publication, manifest_diagnostic)))
        })
        .map_err(LifecycleAppendError::Store)?
    }
}

impl SessionLifecycleStore for JsonlStore {
    fn publish_session(
        &self,
        id: SessionId,
        root: &std::path::Path,
        created: Timestamp,
        profiles: SessionProfiles,
    ) -> Result<LifecyclePublication, StoreError> {
        self.publish_empty_session_lifecycle(id, root, created, profiles)
    }

    fn lifecycle_meta(&self, id: SessionId) -> Result<Option<SessionMeta>, StoreError> {
        let Some(bytes) = self.io.read(&self.log_path(&id))? else {
            return Ok(None);
        };
        let (header, start) = decoded_header(&bytes)?.ok_or(StoreError::MissingHeader)?;
        if header.v != CODEC_VERSION {
            return Err(StoreError::UnsupportedLog { found: header.v });
        }
        if header.session != id {
            return Err(StoreError::WrongSession {
                expected: header.session,
                asked: id,
            });
        }
        let (valid_len, _) = valid_prefix(&bytes, start);
        if valid_len != bytes.len() {
            return Err(invalid_log("session log has a corrupt or torn tail"));
        }
        Ok(self
            .catalogue_facts(&bytes)?
            .map(|facts| meta_from_log_facts(id, &facts)))
    }

    fn acquire_inactive_lease(
        &self,
        id: SessionId,
    ) -> Result<Box<dyn InactiveSessionLease + '_>, LifecycleAppendError> {
        Lease::probe(&self.lease_path(&id))
            .map_err(StoreError::from)?
            .map(|lease| {
                Box::new(InactiveLifecycleAppend {
                    store: self,
                    id,
                    _lease: lease,
                }) as Box<dyn InactiveSessionLease + '_>
            })
            .ok_or(LifecycleAppendError::LeaseHeld)
    }

    fn publish_fork(&self, id: SessionId, seed: &TargetSeed) -> TargetPublication {
        self.publish_target(&id, seed)
    }

    fn quarantine(&self, id: SessionId) -> Result<(), StoreError> {
        Ok(self.io.replace_atomic(&self.quarantine_path(&id), b"")?)
    }

    fn is_quarantined(&self, id: SessionId) -> Result<bool, StoreError> {
        Ok(self.io.exists(&self.quarantine_path(&id))?)
    }
}

impl ReplaySource for JsonlStore {
    fn lineage(&self, session: &SessionId) -> Result<Option<SessionLineage>, ReplaySourceError> {
        let Some(entries) = ReplaySource::entries(self, session)? else {
            return Ok(None);
        };
        let mut events = entries.into_iter().filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(event),
            SessionEntry::RunTransaction(_)
            | SessionEntry::Evidence(_)
            | SessionEntry::NoticeDelivered(_)
            | SessionEntry::ModelSelected(_) => None,
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
            EventKind::Forked { source, at_seq, .. } => {
                Some(SessionLineage::Fork { source, at_seq })
            }
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
        if !self.io.exists(&self.log_path(session)).map_err(|error| {
            ReplaySourceError::Unavailable {
                session: *session,
                reason: error.to_string(),
            }
        })? {
            return Ok(None);
        }
        // Share the short append lock, never the long-lived source writer lease.
        let lock =
            self.open_events_lock(session)
                .map_err(|error| ReplaySourceError::Unavailable {
                    session: *session,
                    reason: error.to_string(),
                })?;
        let _guard = lock
            .read()
            .map_err(|error| ReplaySourceError::Unavailable {
                session: *session,
                reason: error.to_string(),
            })?;
        let bytes = self.io.read(&self.log_path(session)).map_err(|error| {
            ReplaySourceError::Unavailable {
                session: *session,
                reason: error.to_string(),
            }
        })?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        decode_session_entries(session, &bytes).map(Some)
    }
}

fn decode_session_entries(
    session: &SessionId,
    bytes: &[u8],
) -> Result<Vec<SessionEntry>, ReplaySourceError> {
    let (header, start) = decoded_header(bytes)
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
    let (valid_len, _) = valid_prefix(bytes, start);
    if valid_len != bytes.len() {
        return Err(ReplaySourceError::CorruptSession {
            session: *session,
            reason: "session log has a corrupt or torn tail".to_owned(),
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
                        "physical position {} followed {}, so the session \
                             file is out of order",
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
        let entry = serde_json::from_str::<SessionEntry>(record.payload.get()).map_err(|_| {
            ReplaySourceError::UnknownEntry {
                session: *session,
                position: record.seq,
            }
        })?;
        entries.push(entry);
    }
    Ok(entries)
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

        let entries = decode_session_entries(target, &bytes).map_err(|error| {
            invalid_log(format!(
                "the deterministic compaction target could not be decoded: {error}"
            ))
        })?;
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
        ManifestEvidence::from_records(&records)?;
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
