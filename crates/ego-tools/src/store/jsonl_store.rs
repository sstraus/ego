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
use ego_core::event::id::SessionId;
use ego_core::store::codec::{
    CodecError, HeaderV1, StoreLine, decode_line, encode_header, encode_record,
};
use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
use futures::stream::{self, BoxStream};

use super::io::StoreIo;

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

    fn session_dir(&self, id: &SessionId) -> PathBuf {
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
        let is_new_session = self.io.read(&log_path)?.is_none();
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
            match self.io.read(&manifest_path)? {
                None => continue, // session directory exists, but no append has landed yet
                Some(bytes) => match serde_json::from_slice::<SessionMeta>(&bytes) {
                    Ok(meta) => metas.push(meta),
                    Err(error) => {
                        // A catalogue that dies on one bad directory is a
                        // worse failure mode than one that quietly hides a
                        // session: the event log's own integrity is
                        // independently protected by `StoreRecord`
                        // checksums (Step 3), and a corrupt manifest is a
                        // defect a later story can rebuild from the log
                        // (`ARCHITECTURE.md` §5) rather than something that
                        // should take down `list()` for every session. Still
                        // warn, so a session missing from the catalogue is
                        // diagnosable rather than silently absent.
                        tracing::warn!(
                            dir = %dir.display(),
                            error = %error,
                            "skipping session with an unreadable manifest"
                        );
                    }
                },
            }
        }
        Ok(metas)
    }
}
