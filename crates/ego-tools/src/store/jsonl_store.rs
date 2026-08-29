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

use ego_core::compact::{CompactionLineage, CompactionPublication};
use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::hook::HookCatalogueEvidence;
use ego_core::event::id::{Producer, Seq, SessionId};
use ego_core::event::time::Timestamp;
use ego_core::permit::ExecObservation;
use ego_core::run::{RunAppend, RunCommand, RunRevision, RunTransaction};
use ego_core::state::SessionState;
use ego_core::store::codec::{
    CODEC_VERSION, CodecError, Header, StoreLine, decode_line, encode_header, encode_record,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use fd_lock::RwLock;
use futures::stream::{self, BoxStream};

use super::io::{AtomicReplacePublication, StoreIo, tmp_path_for};
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
}

struct LogManifestFacts {
    created: Timestamp,
    workspace_root: PathBuf,
    last_ts: Option<Timestamp>,
    model: Option<String>,
    lineage: Option<CompactionLineage>,
    execution: Option<ExecObservation>,
    hooks: HookCatalogueEvidence,
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
fn header_end(bytes: &[u8]) -> Result<Option<usize>, StoreError> {
    let Some(offset) = bytes.iter().position(|&b| b == b'\n') else {
        return Ok(None);
    };
    match decode_line(&bytes[..offset]) {
        Ok(StoreLine::Header(header)) if header.v == CODEC_VERSION => Ok(Some(offset + 1)),
        // ADR §3. Nothing compared `v` before, so a log written by a later
        // build was read as though it were this version's.
        Ok(StoreLine::Header(header)) => Err(StoreError::UnsupportedLog { found: header.v }),
        Ok(StoreLine::Record(_) | StoreLine::Unknown(_)) => Err(StoreError::MissingHeader),
        Err(e) => Err(StoreError::UnreadableHeader {
            reason: e.to_string(),
        }),
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
/// every line here is known to decode; a line that somehow does not is
/// skipped rather than failing recovery, since refusing to rebuild a
/// manifest is a worse outcome than rebuilding it from one event less.
fn log_manifest_facts(log: &[u8]) -> Option<LogManifestFacts> {
    let mut lines = split_lines(log).into_iter();
    let (created, workspace_root) = match decode_line(lines.next()?) {
        Ok(StoreLine::Header(header)) => (header.created, header.workspace_root),
        _ => return None,
    };
    let mut last_ts = None;
    let mut model = None;
    let mut lineage = None;
    let mut execution = None;
    let mut hooks = HookCatalogueEvidence::default();
    for entry in lines.filter_map(|line| match decode_line(line) {
        Ok(StoreLine::Record(record)) => {
            serde_json::from_str::<SessionEntry>(record.payload.get()).ok()
        }
        _ => None,
    }) {
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
                        lineage = Some(CompactionLineage {
                            source,
                            source_seq,
                            prompt_revision,
                        })
                    }
                    EventKind::ExecutionObserved { observation } => execution = Some(observation),
                    EventKind::HookCatalogueAdmitted { descriptors } => hooks = descriptors,
                    _ => {}
                }
            }
            SessionEntry::RunTransaction(transaction) => {
                last_ts = Some(transaction.observed_at());
                if let RunCommand::CommandAdmitted {
                    model: admitted_model,
                    ..
                } = transaction.command()
                {
                    model = Some(admitted_model.0.clone());
                }
            }
        }
    }
    Some(LogManifestFacts {
        created,
        workspace_root,
        last_ts,
        model,
        lineage,
        execution,
        hooks,
    })
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

    /// Serialize one complete log mutation and its manifest projection.
    /// This is deliberately distinct from the long-held session writer lease.
    fn with_events_lock<T>(
        &self,
        id: &SessionId,
        operation: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
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
        let mut lock = RwLock::new(file);
        let _guard = lock.write()?;
        operation()
    }

    fn current_run_revision_from_lines<'a>(
        &self,
        lines: impl Iterator<Item = &'a [u8]>,
    ) -> Result<Option<RunRevision>, StoreError> {
        let mut current = None;
        for line in lines {
            let entry = match decode_line(line) {
                Ok(StoreLine::Record(record)) => {
                    serde_json::from_str::<SessionEntry>(record.payload.get()).map_err(|error| {
                        StoreError::Codec(CodecError::Malformed(error.to_string()))
                    })?
                }
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
            let SessionEntry::RunTransaction(transaction) = entry else {
                continue;
            };
            let expected = current
                .map(RunRevision::next)
                .unwrap_or_else(|| RunRevision::new(1));
            if transaction.revision() != expected {
                return Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "run transaction revisions are not contiguous",
                )));
            }
            current = Some(transaction.revision());
        }
        Ok(current)
    }

    /// Publish the complete first log image for a clean compaction target.
    ///
    /// The phase-aware replace is the authority boundary: before rename the
    /// source may remain active and retry; after rename the target is visible
    /// and must be selected even when the directory durability barrier or the
    /// derived manifest cache reports a failure.
    pub(super) fn publish_compaction_seed(
        &self,
        id: &SessionId,
        seed: &DomainEvent,
    ) -> CompactionPublication {
        self.with_events_lock(id, || {
            Ok(self.publish_compaction_seed_already_locked(id, seed))
        })
        .unwrap_or_else(|error| CompactionPublication::NotPublished {
            diagnostic: format!("the compaction target lock could not be acquired: {error}"),
        })
    }

    fn publish_compaction_seed_already_locked(
        &self,
        id: &SessionId,
        seed: &DomainEvent,
    ) -> CompactionPublication {
        let mut first = Seq::ZERO;
        first.next();
        let valid_seed = seed.env.seq == first
            && seed.env.item.is_some()
            && seed.env.parent.is_none()
            && seed.env.producer == Producer::Main
            && matches!(seed.kind, EventKind::CompactionSeeded { .. });
        if !valid_seed {
            return CompactionPublication::NotPublished {
                diagnostic: "a compaction target requires one canonical sequence-1 seed event"
                    .to_string(),
            };
        }

        let log_path = self.log_path(id);
        match self.io.exists(&log_path) {
            Ok(false) => {}
            Ok(true) => {
                return CompactionPublication::NotPublished {
                    diagnostic: "the compaction target already has an event log".to_string(),
                };
            }
            Err(error) => {
                return CompactionPublication::NotPublished {
                    diagnostic: format!("the compaction target could not be inspected: {error}"),
                };
            }
        }

        let header = Header::new(*id, seed.env.ts, self.workspace.clone());
        let mut log = encode_header(&header);
        log.push(b'\n');
        let record = match encode_record(seed.env.seq, seed) {
            Ok(record) => record,
            Err(error) => {
                return CompactionPublication::NotPublished {
                    diagnostic: format!("the compaction seed could not be encoded: {error}"),
                };
            }
        };
        log.extend_from_slice(&record);
        log.push(b'\n');

        let publication = match self.io.replace_atomic_publication(&log_path, &log) {
            Ok(publication) => publication,
            Err(error) => {
                return CompactionPublication::NotPublished {
                    diagnostic: format!("the compaction target was not published: {error}"),
                };
            }
        };

        let manifest_diagnostic = self
            .write_manifest(id, seed, true)
            .err()
            .map(|error| format!("manifest cache update failed: {error}"));
        match publication {
            super::io::AtomicReplacePublication::PublishedDurably => {
                CompactionPublication::PublishedDurably {
                    diagnostic: manifest_diagnostic,
                }
            }
            super::io::AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                let diagnostic = match manifest_diagnostic {
                    Some(manifest) => format!("{diagnostic}; {manifest}"),
                    None => diagnostic,
                };
                CompactionPublication::PublishedDurabilityUncertain { diagnostic }
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
                        SessionEntry::RunTransaction(_) => None,
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
        let start = header_end(&bytes)?.unwrap_or(0);

        let (valid_len, dropped_lines) = valid_prefix(&bytes, start);
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
        let Some(facts) = log_manifest_facts(log) else {
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
            model: facts.model.or_else(|| {
                previous
                    .as_ref()
                    .and_then(|manifest| manifest.meta.model.clone())
            }),
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

    /// Replace `id`'s manifest, carrying its `created`/`model`/`title`
    /// forward from whatever was there before (see `SessionMeta`'s doc
    /// comment for why this store cannot populate the latter two from an
    /// `append` call alone) and updating `updated` to `ev`'s envelope
    /// timestamp.
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
            self.io
                .read(&self.log_path(id))?
                .as_deref()
                .and_then(log_manifest_facts)
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
                .and_then(|manifest| manifest.meta.model.clone())
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
                } => Some(CompactionLineage {
                    source: *source,
                    source_seq: *source_seq,
                    prompt_revision: *prompt_revision,
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
        let manifest = RunManifest {
            meta,
            execution,
            hooks,
        };
        let bytes = serde_json::to_vec(&manifest).expect("RunManifest always serialises");
        self.io.replace_atomic(&self.manifest_path(id), &bytes)?;
        Ok(())
    }

    fn append_already_locked(&self, id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
        let log_path = self.log_path(id);
        let is_new_session = !self.io.exists(&log_path)?;
        if is_new_session {
            let header = Header::new(*id, ev.env.ts, self.workspace.clone());
            self.io.append_line(&log_path, &encode_header(&header))?;
        }

        let record_line = encode_record(ev.env.seq, &SessionEntry::Event(ev.clone()))?;
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
            let record_seq: Seq = serde_json::from_value(
                serde_json::to_value(transaction.revision())
                    .expect("RunRevision always serialises"),
            )
            .expect("RunRevision and Seq share the durable integer representation");
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
        let current = self.current_run_revision_from_lines(lines)?;
        if expected_revision.is_none() {
            return current.map_or_else(
                || {
                    Err(StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "an initial run append requires an absent session log",
                    )))
                },
                |actual| Ok(RunAppend::Conflict { actual }),
            );
        }
        if current != expected_revision {
            return current.map_or(Err(StoreError::NotFound), |actual| {
                Ok(RunAppend::Conflict { actual })
            });
        }
        if header.workspace_root.as_path() != transaction.root().as_path() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "run transaction root does not match the session header",
            )));
        }
        let committed = current
            .map(RunRevision::next)
            .unwrap_or_else(|| RunRevision::new(1));
        if transaction.revision() != committed {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "run transaction revision is not the expected successor",
            )));
        }

        let record_seq: Seq = serde_json::from_value(
            serde_json::to_value(transaction.revision()).expect("RunRevision always serialises"),
        )
        .expect("RunRevision and Seq share the durable integer representation");
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
