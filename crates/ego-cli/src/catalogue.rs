//! The session catalogue: which sessions belong here, and how they are shown.
//!
//! `ego ls` retains authoritative event-time ordering. Resume pages use log
//! last-write time as a cheap last-use signal, without replaying older sessions.

use std::path::Path;

use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::session::SessionLineage;
use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
use serde::Serialize;

use crate::cli::UsageError;

/// The version of the shape `--output json` writes (`CLI.md` §4.6).
///
/// Its own number, not `PUBLIC_EVENT_VERSION`. That one versions the event
/// stream a run publishes; this versions a catalogue listing, and the two
/// change for unrelated reasons — sharing a number would make a new event
/// field bump a contract that did not move.
pub const CATALOGUE_VERSION: u32 = 3;

/// The sessions belonging to `root`, most recently updated first.
pub fn in_workspace(metas: Vec<SessionMeta>, root: &Path) -> Vec<SessionMeta> {
    let mut kept: Vec<SessionMeta> = metas
        .into_iter()
        .filter(|meta| meta.workspace_root == root)
        .collect();
    by_recency(&mut kept);
    kept
}

/// Most recently updated first, and the id breaks a tie.
///
/// Two sessions can share an `updated` — the timestamp is milliseconds and a
/// scripted pair of runs can land inside one. Leaving that to the order
/// `list()` happened to read directories in would make `ego ls` unstable.
pub fn by_recency(metas: &mut [SessionMeta]) {
    metas.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
    });
}

/// The session `ego resume` continues.
///
/// `id` is what the caller typed, already known to parse. `None` means the
/// latest of this workspace.
pub fn resolve(
    store: &ego_tools::store::JsonlStore,
    root: &Path,
    id: Option<SessionId>,
) -> Result<SessionId, UsageError> {
    let metas = match id {
        Some(wanted) => store
            .readable_meta(&wanted)
            .map(|meta| meta.into_iter().collect()),
        None => store
            .resume_listing()
            .map(|mut listing| listing.next_page(store, Some(root), None, 1, |_| true)),
    }
    .map_err(|err| UsageError(format!("the session store could not be listed: {err}")))?;

    let Some(wanted) = id else {
        return in_workspace(metas, root)
            .first()
            .map(|meta| meta.id)
            .ok_or_else(|| {
                UsageError(format!(
                    "no session belongs to {} yet; list other workspaces with 'ego ls --all'",
                    root.display()
                ))
            });
    };

    // Named explicitly, so the catalogue is asked whether it exists rather
    // than asked to choose. A `run` would have created it; `resume` must not
    // (ADR §3), so an id nobody has used is a refusal and not a fresh session.
    //
    // `{wanted}` and not `{wanted:?}` in both refusals below: these reach an
    // operator, and `SessionId(0192…)` names a Rust type they cannot act on.
    // The bare id is the thing they can pass back to `ego resume`.
    let Some(meta) = metas.into_iter().find(|meta| meta.id == wanted) else {
        return Err(UsageError(format!(
            "no session {wanted} was found in this store; list saved sessions with 'ego ls --all'"
        )));
    };

    // Its history was recorded against one tree and this run's tools would
    // work in another, so continuing it here is not continuing it. Both paths
    // are named, which is all `-C` needs to fix it.
    if meta.workspace_root != root {
        return Err(UsageError(format!(
            "session {wanted} belongs to {}, and this is {}: resuming it here \
             would run its history against a different tree. Use '-C {}'",
            meta.workspace_root.display(),
            root.display(),
            meta.workspace_root.display()
        )));
    }
    Ok(meta.id)
}

/// The one session of `root` whose title is exactly `title`.
///
/// Exact and case-sensitive, so a near-miss is a refusal and not a guess.
/// Titles are not unique; when several sessions carry this one, the refusal
/// lists them, most recent first, and resumes none: picking the newest would
/// continue a session the caller may not have meant.
pub fn resolve_title(
    store: &ego_tools::store::JsonlStore,
    root: &Path,
    title: &str,
) -> Result<SessionId, UsageError> {
    let metas = readable_sessions(store, &crate::output::Stderr)
        .map_err(|err| UsageError(format!("the session store could not be listed: {err}")))?;
    let matches: Vec<SessionMeta> = in_workspace(metas, root)
        .into_iter()
        .filter(|meta| meta.title.as_deref() == Some(title))
        .collect();
    match matches.as_slice() {
        [] => Err(UsageError(format!(
            "no session titled `{title}` belongs to {}; list its sessions and titles with 'ego ls'",
            root.display()
        ))),
        [meta] => Ok(meta.id),
        _ => Err(UsageError(format!(
            "{} sessions in {} are titled `{title}`; resume one by its id:\n{}",
            matches.len(),
            root.display(),
            table(&matches, false).join("\n")
        ))),
    }
}

/// One row of `--output json` and one line of `--output jsonl` (`CLI.md` §4.6).
///
/// A shape of its own rather than `SessionMeta` serialised directly: that type
/// is the store port's, and publishing it would make a field added for a
/// store's benefit a contract a script depends on.
#[derive(Debug, Serialize)]
pub struct Row {
    pub id: SessionId,
    pub workspace_root: String,
    pub model: Option<String>,
    pub created: u64,
    pub updated: u64,
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineage: Option<SessionLineage>,
}

impl Row {
    pub fn of(meta: &SessionMeta) -> Self {
        Row {
            id: meta.id,
            workspace_root: meta.workspace_root.display().to_string(),
            model: meta.model.clone(),
            created: meta.created.as_millis(),
            updated: meta.updated.as_millis(),
            title: meta.title.clone(),
            lineage: meta.lineage,
        }
    }
}

/// The single object `ego ls --output json` writes (`CLI.md` §4.6).
///
/// An object with a `sessions` key and not a bare array, so the listing can
/// carry its own `v` — a top-level array has nowhere to put one, and a
/// consumer that cannot see the version is a consumer that cannot be told the
/// shape changed.
#[derive(Debug, Serialize)]
pub struct Listing {
    pub v: u32,
    pub sessions: Vec<Row>,
}

impl Listing {
    pub fn of(metas: &[SessionMeta]) -> Self {
        Listing {
            v: CATALOGUE_VERSION,
            sessions: metas.iter().map(Row::of).collect(),
        }
    }
}

/// The table `ego ls` prints to a terminal.
///
/// Columns are padded to the widest cell rather than to a fixed width, because
/// a model slug is `ollama/qwen3` for one operator and
/// `openrouter/z-ai/glm-4.7` for another, and a truncated slug is not one an
/// operator can pass back to `--model`.
///
/// `workspace_root` is a column only under `--all`. Without it every row would
/// carry the same path — the workspace the caller is standing in — which is
/// the definition of a column that says nothing.
pub fn table(metas: &[SessionMeta], all: bool) -> Vec<String> {
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(metas.len() + 1);
    let mut header = vec![
        "ID".to_string(),
        "UPDATED".to_string(),
        "MODEL".to_string(),
        "TITLE".to_string(),
    ];
    if all {
        header.push("WORKSPACE".to_string());
    }
    rows.push(header);

    for meta in metas {
        // An em dash and not an empty cell: a column that is blank for every
        // row reads as a rendering fault, and "nothing has ever set this"
        // is a fact worth showing rather than a gap worth hiding.
        let dash = || "—".to_string();
        let mut row = vec![
            meta.id.to_string(),
            stamp(meta.updated.as_millis()),
            meta.model.clone().unwrap_or_else(dash),
            meta.title.clone().unwrap_or_else(dash),
        ];
        if all {
            row.push(meta.workspace_root.display().to_string());
        }
        rows.push(row);
    }

    crate::render::table::plain_table(rows)
}

/// `updated` as a UTC calendar time an operator can read.
///
/// The calendar arithmetic itself lives on [`Timestamp`], because this is not
/// the only surface that needs a date out of one — the ACP `SessionInfo` wire
/// wants the same instant spelled RFC 3339 — and two renderings of one clock
/// must not be two implementations of one calendar. What stays here is only
/// the shape `ego ls` prints: minute precision, no seconds, no `T`.
fn stamp(millis: u64) -> String {
    if millis == 0 {
        return "—".to_string();
    }
    let at = Timestamp::from_millis(millis).civil();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        at.year, at.month, at.day, at.hour, at.minute
    )
}

/// What `ego ls` was asked for, listed and ordered.
pub fn listing(
    store: &dyn SessionStore,
    root: &Path,
    all: bool,
) -> Result<Vec<SessionMeta>, StoreError> {
    let metas = readable_sessions(store, &crate::output::Stderr)?;
    Ok(if all {
        let mut every = metas;
        by_recency(&mut every);
        every
    } else {
        in_workspace(metas, root)
    })
}

/// `ego ls` keeps its complete, diagnostic catalogue and event-time ordering.
pub(crate) fn saved_sessions(
    store: &ego_tools::store::JsonlStore,
    diagnostics: &dyn crate::output::Diagnostics,
) -> Result<Vec<SessionMeta>, StoreError> {
    let mut metas = readable_sessions(store, diagnostics)?;
    by_recency(&mut metas);
    Ok(metas)
}

/// A modal cancellation keeps the current prompt; quit must leave it.
pub enum SessionPick {
    Selected(Box<SessionMeta>),
    Cancelled,
    Quit,
}

/// A lazy page source handed to the terminal owner. Constructing it performs no I/O.
pub struct ResumePicker {
    pub store: std::sync::Arc<ego_tools::store::JsonlStore>,
    pub root: Option<std::path::PathBuf>,
    pub exclude: Option<SessionId>,
    pub matching: Option<std::collections::HashSet<SessionId>>,
    listing: Option<ego_tools::store::ResumeListing>,
}

impl ResumePicker {
    pub fn new(
        store: std::sync::Arc<ego_tools::store::JsonlStore>,
        root: std::path::PathBuf,
        exclude: Option<SessionId>,
    ) -> Self {
        Self {
            store,
            root: Some(root),
            exclude,
            matching: None,
            listing: None,
        }
    }

    pub fn next_page(&mut self) -> Result<Vec<SessionMeta>, StoreError> {
        if self.listing.is_none() {
            self.listing = Some(self.store.resume_listing()?);
        }
        Ok(self
            .listing
            .as_mut()
            .expect("listing initialized")
            .next_page(
                &self.store,
                self.root.as_deref(),
                self.exclude,
                10,
                |meta| {
                    self.matching
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&meta.id))
                },
            ))
    }

    pub fn has_more(&self) -> bool {
        self.listing
            .as_ref()
            .is_none_or(|listing| listing.has_more())
    }
}

fn readable_sessions(
    store: &dyn SessionStore,
    diagnostics: &dyn crate::output::Diagnostics,
) -> Result<Vec<SessionMeta>, StoreError> {
    let mut incompatible = 0;
    let metas = store.list_readable(&mut |id, error| {
        if matches!(error, StoreError::UnsupportedLog { .. }) {
            incompatible += 1;
        } else {
            diagnostics.note(&format!("session {id} is unreadable: {error}"));
        }
    })?;
    if incompatible > 0 {
        diagnostics.note(&format!(
            "{incompatible} incompatible sessions; remove them with: ego rm --incompatible --yes"
        ));
    }
    Ok(metas)
}

#[cfg(test)]
mod unreadable_tests {
    use super::*;
    use ego_core::event::id::Seq;
    use ego_core::session_lifecycle::SessionLifecycleStore;
    use ego_core::store::codec::encode_record;
    use ego_tools::store::{JsonlStore, OsIo};
    use serde_json::json;

    struct NoListingIo {
        log: std::path::PathBuf,
    }

    impl ego_tools::store::StoreIo for NoListingIo {
        fn append_line(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
            OsIo.append_line(path, line)
        }
        fn fsync(&self, path: &Path) -> std::io::Result<()> {
            OsIo.fsync(path)
        }
        fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
            assert_eq!(
                path, self.log,
                "named lookup must read only its authoritative log"
            );
            OsIo.read(path)
        }
        fn exists(&self, path: &Path) -> std::io::Result<bool> {
            OsIo.exists(path)
        }
        fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            OsIo.replace_atomic(path, bytes)
        }
        fn truncate(&self, path: &Path, length: u64) -> std::io::Result<()> {
            OsIo.truncate(path, length)
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            OsIo.remove_file(path)
        }
        fn list_dirs(&self, _path: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
            Err(std::io::Error::other(
                "named session lookup must not enumerate the store",
            ))
        }
    }

    // Catches: explicit resume enumerates and replays all unrelated session histories.
    #[test]
    fn named_resume_reads_requested_session_without_catalogue_enumeration() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        store
            .publish_empty_session(id, Path::new("/workspace"), Timestamp::EPOCH)
            .unwrap();
        let no_listing = JsonlStore::new(
            dir.path(),
            "/workspace",
            Box::new(NoListingIo {
                log: dir.path().join(id.to_string()).join("events.jsonl"),
            }),
        );
        assert_eq!(
            resolve(&no_listing, Path::new("/workspace"), Some(id)).unwrap(),
            id
        );
        assert!(
            resolve(&no_listing, Path::new("/workspace"), None).is_err(),
            "latest session still requires a catalogue"
        );
    }

    // Catches: direct metadata lookup trusts a stale manifest for deleted/quarantined/corrupt sessions.
    #[test]
    fn named_resume_still_refuses_hidden_and_corrupt_sessions() {
        use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
        use ego_core::event::id::{EventId, Producer, TurnId};
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let ids = [SessionId::new(), SessionId::new(), SessionId::new()];
        for id in ids {
            store
                .publish_empty_session(id, Path::new("/workspace"), Timestamp::EPOCH)
                .unwrap();
        }
        let mut seq = Seq::ZERO;
        store
            .append(
                &ids[0],
                &DomainEvent {
                    env: Envelope {
                        seq: seq.next(),
                        id: EventId::new(),
                        turn: TurnId::new(),
                        item: None,
                        parent: None,
                        ts: Timestamp::EPOCH,
                        producer: Producer::Main,
                    },
                    kind: EventKind::SessionDeleted,
                },
            )
            .unwrap();
        store.quarantine(ids[1]).unwrap();
        let corrupt = dir.path().join(ids[2].to_string()).join("events.jsonl");
        std::fs::write(corrupt, "invalid header\n").unwrap();
        for id in ids {
            assert!(
                resolve(&store, Path::new("/workspace"), Some(id)).is_err(),
                "{id} must not be resumed"
            );
        }
    }

    #[derive(Default)]
    struct Notes(std::sync::Mutex<Vec<String>>);

    impl crate::output::Diagnostics for Notes {
        fn note(&self, message: &str) {
            self.0.lock().unwrap().push(message.to_owned());
        }
    }

    fn append_payload(root: &Path, id: SessionId, payload: serde_json::Value) {
        use std::io::Write;
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join(id.to_string()).join("events.jsonl"))
            .unwrap();
        let mut position = Seq::ZERO;
        let mut record = encode_record(position.next(), &payload).unwrap();
        record.push(b'\n');
        log.write_all(&record).unwrap();
    }

    // Catches: stale manifests or unpublished directories emit NotFound on every ls.
    #[test]
    fn listing_omits_or_repairs_index_entries_without_session_data() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let readable = SessionId::new();
        let missing = SessionId::new();
        for id in [readable, missing] {
            store
                .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
                .unwrap();
        }
        std::fs::remove_file(dir.path().join(missing.to_string()).join("events.jsonl")).unwrap();
        let unpublished = SessionId::new();
        let opened = store
            .open_writer(
                &unpublished,
                ego_tools::store::LeaseInputs {
                    pid: std::process::id(),
                    now: Timestamp::from_millis(1),
                    stale_after: std::time::Duration::from_secs(60),
                    is_alive: &|_| true,
                },
            )
            .unwrap();
        drop(opened);
        assert!(
            !dir.path()
                .join(unpublished.to_string())
                .join("events.jsonl")
                .exists()
        );
        let notes = Notes::default();
        for _ in 0..2 {
            let metas = readable_sessions(&store, &notes).unwrap();
            assert_eq!(
                metas.iter().map(|meta| meta.id).collect::<Vec<_>>(),
                vec![readable]
            );
        }
        assert!(
            notes.0.lock().unwrap().is_empty(),
            "directories without authoritative data are absent, not unreadable"
        );
        assert!(
            dir.path()
                .join(readable.to_string())
                .join("events.jsonl")
                .exists()
        );
        assert!(
            dir.path()
                .join(missing.to_string())
                .join("manifest.json")
                .exists()
        );
    }

    #[test]
    fn unreadable_catalogue_entry_does_not_hide_readable_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let readable = SessionId::new();
        let unreadable = SessionId::new();
        for id in [readable, unreadable] {
            store
                .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
                .unwrap();
        }
        append_payload(
            dir.path(),
            unreadable,
            json!({"kind": "future_writer_entry"}),
        );
        let notes = Notes::default();
        let metas = readable_sessions(&store, &notes)
            .expect("one unreadable log must not abort the catalogue");
        assert_eq!(
            metas.iter().map(|meta| meta.id).collect::<Vec<_>>(),
            vec![readable]
        );
        let notes = notes.0.lock().unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains(&unreadable.to_string()));
        assert!(notes[0].contains("unknown payload"));
    }

    #[test]
    fn unreadable_real_record_shape_names_the_session_instead_of_an_anonymous_payload() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let id: SessionId = "01a070f9-69dd-7440-82ab-79f2520f2f0a".parse().unwrap();
        store
            .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
            .unwrap();
        // Structure-only fixture of the copied store's seq 1, not session content.
        // Nulls deliberately do not invent replacement values for private fields.
        append_payload(
            dir.path(),
            id,
            json!({"kind": "evidence", "evidence": {
                "kind": "run_context", "ceilings": null, "config": null,
                "config_digest": null, "ego_version": null, "execution": null,
                "instructions": null, "invocation": null, "mcp": null,
                "origin": null, "policy_digest": null, "run_id": null,
                "session": null, "version": null
            }}),
        );
        let reason = store.lifecycle_meta(id).unwrap_err().to_string();
        assert!(
            reason.contains(&id.to_string()),
            "unreadable session must be named: {reason}"
        );
    }

    #[test]
    fn unreadable_logs_without_manifests_are_still_named() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let id = SessionId::new();
        store
            .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
            .unwrap();
        append_payload(dir.path(), id, json!({"kind": "future_writer_entry"}));
        std::fs::remove_file(dir.path().join(id.to_string()).join("manifest.json")).unwrap();
        let notes = Notes::default();
        assert!(readable_sessions(&store, &notes).unwrap().is_empty());
        let notes = notes.0.lock().unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains(&id.to_string()));
    }

    #[test]
    fn torn_and_unsupported_logs_get_diagnostics_but_missing_logs_are_absent() {
        use ego_core::store::codec::{Header, encode_header};
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        let ids = [SessionId::new(), SessionId::new(), SessionId::new()];
        for id in ids {
            store
                .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
                .unwrap();
        }
        std::fs::remove_file(dir.path().join(ids[0].to_string()).join("events.jsonl")).unwrap();
        let torn = dir.path().join(ids[1].to_string()).join("events.jsonl");
        let mut bytes = std::fs::read(&torn).unwrap();
        bytes.extend_from_slice(b"{\"torn\":");
        std::fs::write(torn, bytes).unwrap();
        let mut header = Header::new(ids[2], Timestamp::from_millis(1), "/workspace".into());
        header.v += 1;
        let mut bytes = encode_header(&header);
        bytes.push(b'\n');
        std::fs::write(
            dir.path().join(ids[2].to_string()).join("events.jsonl"),
            bytes,
        )
        .unwrap();
        let notes = Notes::default();
        assert!(readable_sessions(&store, &notes).unwrap().is_empty());
        let notes = notes.0.lock().unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes.iter().all(|note| !note.contains(&ids[0].to_string())));
        assert_eq!(
            notes
                .iter()
                .filter(|note| note.contains(&ids[1].to_string()))
                .count(),
            1
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("1 incompatible sessions")
                    && note.contains("ego rm --incompatible --yes"))
        );
    }
}

#[cfg(test)]
mod resume_pages_tests {
    use super::*;
    use ego_core::session_lifecycle::SessionLifecycleStore;
    use ego_tools::store::{JsonlStore, OsIo, StoreIo};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    struct CountReads(Arc<Mutex<Vec<PathBuf>>>);

    impl StoreIo for CountReads {
        fn file_revision(
            &self,
            path: &Path,
        ) -> std::io::Result<Option<ego_tools::store::FileRevision>> {
            OsIo.file_revision(path)
        }
        fn append_line(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
            OsIo.append_line(path, line)
        }
        fn fsync(&self, path: &Path) -> std::io::Result<()> {
            OsIo.fsync(path)
        }
        fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
            self.0.lock().unwrap().push(path.to_owned());
            OsIo.read(path)
        }
        fn exists(&self, path: &Path) -> std::io::Result<bool> {
            OsIo.exists(path)
        }
        fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            OsIo.replace_atomic(path, bytes)
        }
        fn truncate(&self, path: &Path, length: u64) -> std::io::Result<()> {
            OsIo.truncate(path, length)
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            OsIo.remove_file(path)
        }
        fn list_dirs(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
            OsIo.list_dirs(path)
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        store: Arc<JsonlStore>,
        reads: Arc<Mutex<Vec<PathBuf>>>,
        ids: Vec<SessionId>,
    }

    impl Fixture {
        fn new(count: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let reads = Arc::new(Mutex::new(Vec::new()));
            let seed = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
            let ids: Vec<_> = (0..count).map(|_| SessionId::new()).collect();
            for (index, id) in ids.iter().enumerate() {
                seed.publish_empty_session(
                    *id,
                    Path::new("/workspace"),
                    Timestamp::from_millis((index as u64 + 1) * 1000),
                )
                .unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(dir.path().join(id.to_string()).join("events.jsonl"))
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(index as u64 + 1),
                    ))
                    .unwrap();
            }
            let store = JsonlStore::new(
                dir.path(),
                "/workspace",
                Box::new(CountReads(Arc::clone(&reads))),
            );
            Self {
                dir,
                store: Arc::new(store),
                reads,
                ids,
            }
        }

        fn page(&self) -> Result<Vec<SessionMeta>, StoreError> {
            ResumePicker::new(Arc::clone(&self.store), "/workspace".into(), None).next_page()
        }

        fn log_reads(&self) -> usize {
            self.reads
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.file_name().is_some_and(|name| name == "events.jsonl"))
                .count()
        }
    }

    // Catches: truncating the displayed rows after replaying the whole store.
    #[test]
    fn resume_picker_reads_only_the_newest_page_of_ten() {
        let fixture = Fixture::new(24);
        let rows = fixture.page().unwrap();
        assert_eq!(rows.len(), 10, "the first page contains ten rows");
        assert_eq!(
            fixture.log_reads(),
            10,
            "older logs must not be opened for the first page"
        );
        assert_eq!(rows[0].id, fixture.ids[23]);
    }

    // Catches: unreadable or in-flight directories flooding the resume transcript.
    #[test]
    fn resume_listing_skips_unreadable_sessions() {
        let fixture = Fixture::new(4);
        std::fs::write(
            fixture
                .dir
                .path()
                .join(fixture.ids[0].to_string())
                .join("events.jsonl"),
            b"invalid header\n",
        )
        .unwrap();
        std::fs::remove_file(
            fixture
                .dir
                .path()
                .join(fixture.ids[1].to_string())
                .join("events.jsonl"),
        )
        .unwrap();
        let log = fixture
            .dir
            .path()
            .join(fixture.ids[2].to_string())
            .join("events.jsonl");
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(log)
            .unwrap()
            .write_all(b"{\"torn\":")
            .unwrap();
        std::fs::create_dir(fixture.dir.path().join(SessionId::new().to_string())).unwrap();
        let rows = fixture.page().unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![fixture.ids[3]]
        );
    }

    fn files(root: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        let mut result = Vec::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.extend(files(&path));
            } else {
                result.push((
                    path.clone(),
                    std::fs::read(&path).unwrap(),
                    std::fs::metadata(path).unwrap().modified().unwrap(),
                ));
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    // Catches: catalogue reads creating events.lock or repairing a log/manifest.
    #[test]
    fn listing_sessions_writes_nothing_and_takes_no_lease() {
        let fixture = Fixture::new(2);
        for id in &fixture.ids {
            let lock = fixture.dir.path().join(id.to_string()).join("events.lock");
            if lock.exists() {
                std::fs::remove_file(lock).unwrap();
            }
        }
        let before = files(fixture.dir.path());
        assert_eq!(fixture.page().unwrap().len(), 2);
        assert_eq!(
            files(fixture.dir.path()),
            before,
            "listing may not create a lock or alter bytes/mtimes"
        );
    }

    // Catches: unnamed headless resume replaying every session to find one ID.
    #[test]
    fn headless_resume_without_id_picks_the_latest_without_scanning_every_session() {
        let fixture = Fixture::new(24);
        assert_eq!(
            resolve(&fixture.store, Path::new("/workspace"), None).unwrap(),
            fixture.ids[23]
        );
        assert_eq!(
            fixture.log_reads(),
            1,
            "headless resume needs only its latest readable candidate"
        );
    }
    // Catches: re-sorting each page after a live update repeats or skips IDs.
    #[test]
    fn next_page_continues_after_the_tenth_by_recency_with_id_tiebreak() {
        let fixture = Fixture::new(23);
        let stamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(100);
        for id in &fixture.ids {
            std::fs::File::options()
                .write(true)
                .open(fixture.dir.path().join(id.to_string()).join("events.jsonl"))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(stamp))
                .unwrap();
        }
        let mut expected = fixture.ids.clone();
        expected.sort_by_key(|id| std::cmp::Reverse(id.to_string()));
        let mut source = ResumePicker::new(Arc::clone(&fixture.store), "/workspace".into(), None);
        assert_eq!(
            source
                .next_page()
                .unwrap()
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            expected[..10]
        );
        std::fs::File::options()
            .write(true)
            .open(
                fixture
                    .dir
                    .path()
                    .join(expected[22].to_string())
                    .join("events.jsonl"),
            )
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(stamp + std::time::Duration::from_secs(100)),
            )
            .unwrap();
        assert_eq!(
            source
                .next_page()
                .unwrap()
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            expected[10..20]
        );
        assert_eq!(
            source
                .next_page()
                .unwrap()
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            expected[20..]
        );
        assert!(!source.has_more());
        assert_eq!(fixture.log_reads(), 23);
    }

    // Catches: counting unreadable candidates instead of readable workspace rows.
    #[test]
    fn unreadable_sessions_do_not_use_up_a_page_slot() {
        let fixture = Fixture::new(14);
        for id in &fixture.ids[10..] {
            std::fs::write(
                fixture
                    .dir
                    .path()
                    .join(id.to_string())
                    .join("manifest.json"),
                b"invalid manifest",
            )
            .unwrap();
        }
        let rows = fixture.page().unwrap();
        assert_eq!(rows.len(), 10);
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            fixture.ids[..10].iter().rev().copied().collect::<Vec<_>>()
        );
        assert_eq!(
            fixture.log_reads(),
            10,
            "invalid manifests need no journal read"
        );
    }
}
