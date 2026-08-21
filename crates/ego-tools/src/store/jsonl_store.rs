//! `JsonlStore`: the filesystem `SessionStore` — a JSONL event log plus a
//! JSON manifest, per session directory (`ARCHITECTURE.md` §5).
//!
//! ```text
//! <root>/<session-id>/
//!     manifest.json      # SessionMeta, replaced atomically
//!     events.jsonl        # the log; first line is a versioned header record
//! ```

use std::path::PathBuf;

use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::id::{Seq, SessionId};
use ego_core::event::time::Timestamp;
use ego_core::state::SessionState;
use ego_core::store::codec::{
    CodecError, HeaderV1, StoreLine, decode_line, encode_header, encode_record,
};
use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
use futures::stream::{self, BoxStream};

use super::io::{StoreIo, tmp_path_for};
use super::snapshot::{SNAPSHOT_VERSION, Snapshot, SnapshotRejection, validate};

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

/// The end of the last *complete, valid* line in `bytes`, plus how many
/// lines were rejected from there on.
///
/// A line counts as durable only if it is terminated by a newline: a final
/// line without one is by definition a half-finished append, which is the
/// single most likely crash artefact for an append-only log. Beyond that,
/// every line must decode — checksum included — and must be the shape its
/// position demands (a header first, records after).
///
/// The scan stops at the *first* rejected line rather than skipping it and
/// resuming, and everything after it is dropped too. That is deliberate: a
/// gap in an append-only log means the byte stream desynchronised, so
/// "valid" lines after the gap are not trustworthy neighbours — they are
/// bytes that happened to parse.
fn valid_prefix(bytes: &[u8]) -> (usize, usize) {
    let mut end = 0usize;
    let mut index = 0usize;

    while end < bytes.len() {
        let Some(offset) = bytes[end..].iter().position(|&b| b == b'\n') else {
            break; // torn final line: no terminating newline
        };
        let line = &bytes[end..end + offset];
        let decoded = match decode_line(line) {
            Ok(decoded) => decoded,
            Err(_) => break,
        };
        let shape_ok = match (index, &decoded) {
            (0, StoreLine::Header(_)) => true,
            (0, _) => false,
            (_, StoreLine::Record(_)) => true,
            (_, _) => false,
        };
        if !shape_ok {
            break;
        }
        end += offset + 1;
        index += 1;
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

/// A valid log's `created` (from its header) and the timestamp of its last
/// record, or `None` if it has no header or no records.
///
/// Only ever called on a prefix [`valid_prefix`] has already accepted, so
/// every line here is known to decode; a line that somehow does not is
/// skipped rather than failing recovery, since refusing to rebuild a
/// manifest is a worse outcome than rebuilding it from one event less.
fn log_bounds(log: &[u8]) -> Option<(Timestamp, Timestamp)> {
    let mut lines = split_lines(log).into_iter();
    let created = match decode_line(lines.next()?) {
        Ok(StoreLine::Header(header)) => header.created,
        _ => return None,
    };
    let last_ts = lines
        .filter_map(|line| match decode_line(line) {
            Ok(StoreLine::Record(record)) => {
                serde_json::from_str::<DomainEvent>(record.payload.get()).ok()
            }
            _ => None,
        })
        .map(|ev| ev.env.ts)
        .next_back()?;
    Some((created, last_ts))
}

/// The filesystem `SessionStore`. Real I/O goes through the injected
/// [`StoreIo`], so tests can wrap it (e.g. to count fsyncs) without faking
/// the filesystem itself.
pub struct JsonlStore {
    root: PathBuf,
    io: Box<dyn StoreIo>,
}

impl JsonlStore {
    /// Build a store rooted at `root` (e.g. `~/.ego/sessions`), performing
    /// its I/O through `io`.
    pub fn new(root: impl Into<PathBuf>, io: Box<dyn StoreIo>) -> Self {
        Self {
            root: root.into(),
            io,
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

    /// Read and parse `id`'s manifest, or `None` if it does not exist yet.
    fn read_manifest(&self, id: &SessionId) -> Result<Option<SessionMeta>, StoreError> {
        match self.io.read(&self.manifest_path(id))? {
            None => Ok(None),
            Some(bytes) => {
                let meta = serde_json::from_slice(&bytes).map_err(StoreError::Manifest)?;
                Ok(Some(meta))
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
    fn read_events(&self, id: &SessionId) -> Result<Vec<DomainEvent>, StoreError> {
        let stream = SessionStore::read(self, id)?;
        Ok(futures::executor::block_on(async {
            use futures::StreamExt;
            stream.filter_map(|r| async move { r.ok() }).collect().await
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
    /// 3. **Stale or unreadable manifest** — rebuilt from the log when the
    ///    log is ahead of it, which is exactly the "missing fsync window"
    ///    case: `append` only rewrites the manifest on a turn boundary, so
    ///    every event since the last one is in the log but not the manifest.
    ///    The log is the authority; the manifest is a derived cache.
    ///
    /// Note the deliberate absence of a fourth: an orphaned *lease* needs no
    /// repair here. The advisory lock behind `Lease` is owned by the open
    /// file description, so the kernel drops it when the holder's descriptor
    /// closes — including on `SIGKILL` — and a stale PID/heartbeat record is
    /// handled by `Lease::acquire`'s takeover path, not by rewriting files.
    /// Deleting a lease file here would be actively wrong: it would evict a
    /// *live* writer.
    pub fn recover(&self, id: &SessionId) -> Result<Repair, StoreError> {
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

        let (valid_len, dropped_lines) = valid_prefix(&bytes);
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

        if self.rebuild_manifest_if_stale(id, &bytes[..valid_len])? {
            repair.manifest_rebuilt = true;
        }

        Ok(repair)
    }

    /// Rewrite `id`'s manifest from `log` when the manifest is missing,
    /// unreadable, or older than the log's last event. Returns whether it
    /// wrote anything.
    fn rebuild_manifest_if_stale(&self, id: &SessionId, log: &[u8]) -> Result<bool, StoreError> {
        let Some((created, last_ts)) = log_bounds(log) else {
            // A log with a header but no surviving records has no `updated`
            // to offer, so it cannot make an existing manifest more
            // accurate than it already is.
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

        if let Some(meta) = &previous
            && meta.updated >= last_ts
        {
            return Ok(false);
        }

        tracing::warn!(
            session = %session_dir_name(id),
            "rebuilding a session manifest that is missing, unreadable or behind its log"
        );
        let meta = SessionMeta {
            id: *id,
            workspace_root: previous.as_ref().and_then(|m| m.workspace_root.clone()),
            model: previous.as_ref().and_then(|m| m.model.clone()),
            // `created` comes from the log header rather than the old
            // manifest: the header is written once, at session creation, and
            // is checksum-protected, so it is the better authority for a
            // value that must never drift.
            created,
            updated: last_ts,
            title: previous.as_ref().and_then(|m| m.title.clone()),
        };
        let bytes = serde_json::to_vec(&meta).expect("SessionMeta always serialises");
        self.io.replace_atomic(&self.manifest_path(id), &bytes)?;
        Ok(true)
    }

    /// Replace `id`'s manifest, carrying its `created`/`workspace_root`/
    /// `model`/`title` forward from whatever was there before (see
    /// `SessionMeta`'s doc comment for why this store cannot populate the
    /// latter three from an `append` call alone) and updating `updated` to
    /// `ev`'s envelope timestamp.
    fn write_manifest(&self, id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
        let previous = self.read_manifest(id)?;
        let meta = SessionMeta {
            id: *id,
            workspace_root: previous.as_ref().and_then(|m| m.workspace_root.clone()),
            model: previous.as_ref().and_then(|m| m.model.clone()),
            created: previous.as_ref().map_or(ev.env.ts, |m| m.created),
            updated: ev.env.ts,
            title: previous.as_ref().and_then(|m| m.title.clone()),
        };
        let bytes = serde_json::to_vec(&meta).expect("SessionMeta always serialises");
        self.io.replace_atomic(&self.manifest_path(id), &bytes)?;
        Ok(())
    }
}

impl SessionStore for JsonlStore {
    fn append(&self, id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
        let log_path = self.log_path(id);
        let is_new_session = !self.io.exists(&log_path)?;
        if is_new_session {
            let header = HeaderV1::new(*id, ev.env.ts);
            self.io.append_line(&log_path, &encode_header(&header))?;
        }

        let record_line = encode_record(ev.env.seq, ev)?;
        self.io.append_line(&log_path, &record_line)?;

        // ARCHITECTURE.md §5's documented fsync policy: force bytes to
        // stable storage on a turn boundary, not on every event. Decided
        // here, from `ev.kind` directly — see `SessionStore::append`'s doc
        // comment on why this predicate does not live on `DomainEvent`.
        let is_turn_boundary = matches!(ev.kind, EventKind::TurnCompleted { .. });
        if is_turn_boundary {
            self.io.fsync(&log_path)?;
        }

        // The manifest is rewritten at session creation and at every turn
        // boundary — the same cadence as the fsync policy above — not on
        // every event. Streamed deltas (`AssistantDelta`, `ToolCallDelta`,
        // …) can arrive many times a second; paying a tmp+rename for each
        // would make the manifest the append path's bottleneck for no
        // benefit `list()` needs, since nothing reads `updated` at
        // sub-turn granularity.
        if is_new_session || is_turn_boundary {
            self.write_manifest(id, ev)?;
        }

        Ok(())
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<DomainEvent, StoreError>>, StoreError> {
        let bytes = self
            .io
            .read(&self.log_path(id))?
            .ok_or(StoreError::NotFound)?;
        let mut lines = split_lines(&bytes).into_iter();

        match lines.next() {
            None => return Err(StoreError::MissingHeader),
            Some(header_line) => match decode_line(header_line)? {
                StoreLine::Header(_) => {}
                StoreLine::Record(_) | StoreLine::Unknown(_) => {
                    return Err(StoreError::MissingHeader);
                }
            },
        }

        // Read eagerly into an owned `Vec` rather than streaming
        // incrementally off disk: this store already reads the whole log
        // into memory to validate the header above, so a second, lazy pass
        // would buy nothing this milestone needs while entangling the
        // stream's lifetime with `&self`. Owning the decoded results makes
        // the stream `'static`, so a caller does not have to keep this
        // `JsonlStore` borrowed for as long as it holds the stream.
        let events: Vec<Result<DomainEvent, StoreError>> = lines
            .map(|line| match decode_line(line) {
                Ok(StoreLine::Record(record)) => {
                    serde_json::from_str::<DomainEvent>(record.payload.get())
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

        Ok(Box::pin(stream::iter(events)))
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
            match serde_json::from_slice::<SessionMeta>(&bytes) {
                Ok(meta) => metas.push(meta),
                Err(error) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %error,
                        "skipping session with an unreadable manifest"
                    );
                }
            }
        }
        Ok(metas)
    }
}
