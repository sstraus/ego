//! Rebuildable episode FTS5. Ordered JSONL records remain the evidence authority.
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use ego_core::event::domain::EventKind;
use ego_core::event::id::SessionId;
use ego_core::store::codec::{CODEC_VERSION, StoreLine, decode_line};
use ego_core::store::session_store::SessionEntry;
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::JsonlStore;

#[derive(Debug)]
pub struct HistoryError(String, bool);
impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for HistoryError {}
impl From<std::io::Error> for HistoryError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string(), false)
    }
}
impl From<rusqlite::Error> for HistoryError {
    fn from(error: rusqlite::Error) -> Self {
        let corrupt = matches!(&error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase));
        Self(error.to_string(), corrupt)
    }
}
fn invalid(error: impl fmt::Display) -> HistoryError {
    HistoryError(error.to_string(), false)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryHit {
    pub id: SessionId,
    pub title: String,
    pub updated: String,
    pub snippet: String,
    /// Header digest, event identity and exact record digest; never an offset identity.
    pub anchor: String,
}
#[derive(Debug, Serialize)]
struct UnreadableSession {
    id: SessionId,
    error: String,
}
#[derive(Debug, Serialize, Default)]
struct SearchResult {
    hits: Vec<HistoryHit>,
    unreadable: Vec<UnreadableSession>,
    omitted_unreadable: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    incomplete: Vec<SessionId>,
}

/// Persisted only in the disposable index. A decoder change invalidates this table.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Source {
    revision: String,
    identity: String,
    generation: String,
    offset: u64,
    position: u64,
    title: String,
    summary: String,
    updated: String,
    deleted: bool,
    incomplete: bool,
    error: Option<String>,
}
#[derive(Default)]
struct Work {
    decoded_records: AtomicU64,
}
#[cfg(feature = "test-support")]
#[derive(Debug, Clone, Copy)]
pub struct HistoryWork {
    pub decoded_records: u64,
}

/// One serialized maintenance writer; readers never run whole-index integrity scans.
#[derive(Clone)]
pub struct HistoryIndex {
    path: PathBuf,
    store: Arc<JsonlStore>,
    work: Arc<Work>,
}
impl HistoryIndex {
    pub(super) fn new(path: PathBuf, store: Arc<JsonlStore>) -> Self {
        Self {
            path,
            store,
            work: Arc::default(),
        }
    }
    #[cfg(feature = "test-support")]
    pub fn work(&self) -> HistoryWork {
        HistoryWork {
            decoded_records: self.work.decoded_records.load(Ordering::Relaxed),
        }
    }
    pub fn matches(&self, query: &str) -> Result<Vec<HistoryHit>, HistoryError> {
        self.search_result(query).map(|result| result.hits)
    }
    fn search_result(&self, query: &str) -> Result<SearchResult, HistoryError> {
        if query.len() > 4096 {
            return Err(invalid("history query exceeds 4096 bytes"));
        }
        let terms: Vec<_> = query
            .split_whitespace()
            .filter(|term| term.chars().any(char::is_alphanumeric))
            .collect();
        if terms.len() > 32 {
            return Err(invalid("history query exceeds 32 terms"));
        }
        if terms.is_empty() {
            return Ok(SearchResult::default());
        }
        let query = terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        self.with_index(|connection| {
            let mut result = self.source_status(connection)?;
            // No global rank/sort: stop after the bounded candidate set in FTS order.
            let mut statement = connection.prepare(
                "SELECT session, anchor, title, updated, snippet(episodes, 2, '', '', '…', 20)
                 FROM episodes WHERE episodes MATCH ?1 LIMIT 50")?;
            let rows = statement.query_map([query], |row| Ok((
                row.get::<_, String>(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
            for row in rows {
                let (id, mut anchor, title, updated, snippet) = row?;
                let (turn, kind): (String, String) = connection.query_row(
                    "SELECT turn,kind FROM records WHERE session=?1 AND anchor=?2", params![id, anchor],
                    |row| Ok((row.get(0)?, row.get(1)?)))?;
                if kind != "metadata" {
                    let mut candidates = connection.prepare("SELECT anchor,text FROM records WHERE session=?1 AND turn=?2 AND current=1 AND kind IN ('user','assistant') ORDER BY position")?;
                    let mut best = 0;
                    for candidate in candidates.query_map(params![id, turn], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
                        let (candidate, text) = candidate?;
                        let text = text.to_lowercase();
                        let score = terms.iter().filter(|term| text.contains(&term.to_lowercase())).count();
                        if score > best { best = score; anchor = candidate; }
                    }
                }
                result.hits.push(HistoryHit { id: id.parse().map_err(invalid)?, anchor, title, updated, snippet });
            }
            Ok(result)
        })
    }
    /// Model-facing JSON always fits one KiB, including escaping and status.
    pub fn search(&self, query: &str) -> Result<String, HistoryError> {
        let result = self.search_result(query)?;
        let mut selected = SearchResult {
            omitted_unreadable: result.unreadable.len(),
            ..Default::default()
        };
        for mut unreadable in result.unreadable {
            unreadable.error = bounded(&unreadable.error, 160).to_owned();
            selected.unreadable.push(unreadable);
            selected.omitted_unreadable -= 1;
            if encode(&selected)?.len() > 384 {
                selected.unreadable.pop();
                selected.omitted_unreadable += 1;
                break;
            }
        }
        for id in result.incomplete {
            selected.incomplete.push(id);
            if encode(&selected)?.len() > 512 {
                selected.incomplete.pop();
                break;
            }
        }
        for mut hit in result.hits {
            hit.title = bounded(&hit.title, 60).to_owned();
            hit.snippet = bounded(&hit.snippet, 80).to_owned();
            selected.hits.push(hit);
            if encode(&selected)?.len() > 1024 {
                selected.hits.pop();
                break;
            }
        }
        encode(&selected)
    }

    /// Browse the first episode when no search anchor is supplied.
    pub fn open(&self, id: SessionId) -> Result<String, HistoryError> {
        self.open_at(id, None, false, None)
    }
    pub fn open_at(
        &self,
        id: SessionId,
        anchor: Option<&str>,
        evidence: bool,
        continuation: Option<&str>,
    ) -> Result<String, HistoryError> {
        if anchor.is_some_and(|s| s.len() > 256) || continuation.is_some_and(|s| s.len() > 1024) {
            return Err(invalid("oversized history reference"));
        }
        self.with_index(|connection| {
            let source: String = connection.query_row("SELECT state FROM sources WHERE session=?1", [id.to_string()], |row| row.get(0))
                .optional()?.ok_or_else(|| invalid("history source is missing"))?;
            let source: Source = serde_json::from_str(&source).map_err(invalid)?;
            if source.deleted || source.error.is_some() { return Err(invalid("history source is deleted or unreadable")); }
            let anchor = match anchor {
                Some(anchor) => anchor.to_owned(),
                None => connection.query_row("SELECT anchor FROM records WHERE session=?1 AND current=1 AND kind IN ('user','assistant','metadata') ORDER BY position LIMIT 1", [id.to_string()], |row| row.get(0))
                    .optional()?.ok_or_else(|| invalid("session has no conversation episode"))?,
            };
            let (position, turn): (i64, String) = connection.query_row(
                "SELECT position, turn FROM records WHERE session=?1 AND anchor=?2", params![id.to_string(), anchor],
                |row| Ok((row.get(0)?, row.get(1)?))).optional()?.ok_or_else(|| invalid("stale history anchor"))?;
            let cursor = match continuation {
                Some(cursor) => {
                    let cursor: Cursor = serde_json::from_str(cursor).map_err(invalid)?;
                    if cursor.anchor != anchor || cursor.evidence != evidence || cursor.session != id || cursor.end > source.position {
                        return Err(invalid("history continuation does not match the source or selection"));
                    }
                    cursor
                }
                None => Cursor { session: id, anchor: anchor.clone(), evidence, end: source.position, index: 0, offset: 0, source: None },
            };
            // Pin revision visibility as well as the episode watermark. An append
            // can clear today's current flag without superseding a record at cursor.end.
            // The explicit anchor remains first, even when it is a historical revision.
            let mut statement = connection.prepare(
                "SELECT anchor, event, position, start, length, kind FROM records AS selected
                 WHERE session=?1 AND position<=?2 AND (anchor=?3 OR (turn=?4 AND
                     (kind NOT IN ('assistant','metadata') OR NOT EXISTS (
                         SELECT 1 FROM records AS newer
                         WHERE newer.session=selected.session AND newer.item=selected.item
                           AND newer.kind=selected.kind AND newer.position>selected.position
                           AND newer.position<=?2))))
                   AND (kind IN ('user','assistant','metadata') OR (?5 AND kind IN ('tool_call','tool_result')))
                 ORDER BY (position=?6) DESC, position")?;
            let records = statement.query_map(params![id.to_string(), i64::try_from(cursor.end).map_err(invalid)?, anchor, turn, evidence, position], |row| Ok(Located {
                source: row.get(0)?, event: row.get(1)?, position: row.get(2)?, start: row.get(3)?, length: row.get(4)?, kind: row.get(5)?,
            }))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if cursor.index >= records.len() { return Err(invalid("stale history continuation")); }
            let lock = self.store.open_events_lock(&id).map_err(invalid)?;
            let _guard = lock.read()?;
            if self.store.io.exists(&self.store.session_dir(&id).join("quarantined"))? {
                return Err(invalid("history source is quarantined"));
            }
            let log_path = self.store.log_path(&id);
            let metadata = super::cold_read::metadata(&log_path)?
                .ok_or_else(|| invalid("missing history log"))?;
            let current_revision = super::FileRevision::from_metadata(&metadata)?;
            if source.revision != format!("{CODEC_VERSION}:{current_revision:?}") {
                return Err(invalid("history source changed while opening; retry the reference"));
            }
            // The header is also part of source identity, not just the event line.
            use std::io::{BufRead, Read};
            // A cold log is decompressed once for the header and every record.
            let mut log = super::cold_read::open(&log_path)?;
            let mut header = Vec::new();
            std::io::BufReader::new((&mut log).take(16384)).read_until(b'\n', &mut header)?;
            if header.last() != Some(&b'\n') || digest(&header[..header.len()-1]) != source.generation {
                return Err(invalid("stale history source generation"));
            }
            let mut cursor = cursor;
            let mut opened = Vec::new();
            loop {
            let located = &records[cursor.index];
            if cursor.source.as_ref().is_some_and(|s| s != &located.source) { return Err(invalid("stale history continuation record")); }
            // The index locates bytes; only the exact authoritative record can supply evidence.
            let raw = read_record(&mut *log, located.start, located.length)?;
            let actual = format!("{}:{}:{}", source.generation, located.event, digest(&raw));
            if actual != located.source { return Err(invalid("stale history source record")); }
            let StoreLine::Record(record) = decode_line(&raw).map_err(invalid)? else { return Err(invalid("history locator is not a record")); };
            let SessionEntry::Event(event) = serde_json::from_str(record.payload.get()).map_err(invalid)? else { return Err(invalid("history locator is not an event")); };
            let text = event_text(&event.kind).ok_or_else(|| invalid("history event is not selected evidence"))?;
            if cursor.offset > text.len() || !text.is_char_boundary(cursor.offset) { return Err(invalid("invalid history text cursor")); }
            let remaining = &text[cursor.offset..];
            let mut length = remaining.len().min(4096);
            loop {
                let fragment = bounded(remaining, length);
                let mut next = cursor.clone();
                next.offset += fragment.len();
                if next.offset == text.len() { next.index += 1; next.offset = 0; }
                next.source = records.get(next.index).map(|record| record.source.clone());
                let continuation = if next.index < records.len() { Some(encode(&next)?) } else { None };
                opened.push(Opened { event: located.event.clone(), source: located.source.clone(), position: located.position,
                    kind: located.kind.clone(), offset: cursor.offset, text: fragment.to_owned() });
                let page = Page { session: id, anchor: &anchor, historical: true, incomplete: source.incomplete,
                    records: &opened, continuation };
                let encoded = encode(&page)?;
                if encoded.len() <= 8192 {
                    if next.index >= records.len() || next.offset != 0 || opened.len() >= 16 {
                        return Ok(encoded);
                    }
                    cursor = next;
                    break;
                }
                opened.pop();
                if !opened.is_empty() {
                    // Keep the next record untouched for the following page.
                    let page = Page { session: id, anchor: &anchor, historical: true, incomplete: source.incomplete,
                        records: &opened, continuation: Some(encode(&cursor)?) };
                    return encode(&page);
                }
                if length <= 1 { return Err(invalid("history provenance exceeds response budget")); }
                length /= 2;
            }
            }
        })
    }
    fn with_index<T>(
        &self,
        action: impl FnOnce(&Connection) -> Result<T, HistoryError>,
    ) -> Result<T, HistoryError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("index has no directory"))?;
        fs::create_dir_all(parent)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))?;
        let mut lock = fd_lock::RwLock::new(lock);
        let _guard = lock.write()?;
        let run = || {
            let mut connection = Connection::open(&self.path)?;
            connection.busy_timeout(std::time::Duration::from_secs(2))?;
            // Version the disposable schema independently of the authoritative codec.
            let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version != 2 {
                connection.execute_batch("DROP TABLE IF EXISTS history; DROP TABLE IF EXISTS episodes; DROP TABLE IF EXISTS records; DROP TABLE IF EXISTS sources;
                    PRAGMA user_version=2;")?;
            }
            connection.execute_batch("CREATE TABLE IF NOT EXISTS sources(session TEXT PRIMARY KEY, state TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS records(session TEXT, position INTEGER, anchor TEXT, event TEXT, turn TEXT, item TEXT, kind TEXT, text TEXT, start INTEGER, length INTEGER, current INTEGER, PRIMARY KEY(session,position));
                CREATE INDEX IF NOT EXISTS record_anchor ON records(session,anchor);
                CREATE INDEX IF NOT EXISTS record_turn ON records(session,turn,position);
                CREATE VIRTUAL TABLE IF NOT EXISTS episodes USING fts5(session UNINDEXED, anchor UNINDEXED, text, title UNINDEXED, updated UNINDEXED);")?;
            self.refresh(&mut connection)?;
            Ok::<_, HistoryError>(connection)
        };
        let connection = match run() {
            Ok(connection) => connection,
            Err(error) if corrupt_message(&error) => {
                // Only the derived file is disposable. Retry initialization once.
                fs::remove_file(&self.path)?;
                run()?
            }
            Err(error) => return Err(error),
        };
        action(&connection)
    }
    fn source_status(&self, connection: &Connection) -> Result<SearchResult, HistoryError> {
        let mut statement =
            connection.prepare("SELECT session,state FROM sources ORDER BY session")?;
        let mut result = SearchResult::default();
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, state) = row?;
            let id = id.parse().map_err(invalid)?;
            let source: Source = serde_json::from_str(&state).map_err(invalid)?;
            if let Some(error) = source.error {
                result.unreadable.push(UnreadableSession { id, error });
            } else if source.incomplete {
                result.incomplete.push(id);
            }
        }
        Ok(result)
    }
    fn refresh(&self, connection: &mut Connection) -> Result<(), HistoryError> {
        let root = self
            .path
            .parent()
            .ok_or_else(|| invalid("index has no directory"))?;
        let directories = self.store.io.list_dirs(root)?;
        let transaction = connection.transaction()?;
        let mut present = HashSet::new();
        for directory in directories {
            let Some(id) = directory
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<SessionId>().ok())
            else {
                continue;
            };
            if self.store.io.exists(&directory.join("quarantined"))? {
                continue;
            }
            // A cold log is `events.jsonl.zst`: its new inode forces one rebuild,
            // which reads the decompressed bytes from offset 0.
            let Some(metadata) = super::cold_read::metadata(&self.store.log_path(&id))? else {
                continue;
            };
            let key = id.to_string();
            present.insert(key.clone());
            let old: Option<String> = transaction
                .query_row(
                    "SELECT state FROM sources WHERE session=?1",
                    [&key],
                    |row| row.get(0),
                )
                .optional()?;
            let mut source: Source = old
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(invalid)?
                .unwrap_or_default();
            let revision = format!(
                "{CODEC_VERSION}:{:?}",
                super::FileRevision::from_metadata(&metadata)?
            );
            if source.revision == revision {
                continue;
            }
            let lock = self.store.open_events_lock(&id).map_err(invalid)?;
            let _guard = lock.read()?;
            let metadata = super::cold_read::metadata(&self.store.log_path(&id))?
                .ok_or_else(|| invalid("missing history log"))?;
            let revision = format!(
                "{CODEC_VERSION}:{:?}",
                super::FileRevision::from_metadata(&metadata)?
            );
            let identity = identity(&metadata);
            // Store writers append under this lock. Replacement and truncation rebuild only this source.
            if source.identity != identity
                || metadata.len() <= source.offset
                || source.error.is_some()
            {
                source = Source::default();
                transaction.execute("DELETE FROM records WHERE session=?1", [&key])?;
            }
            source.identity = identity;
            source.error = None;
            let result = self.consume(&transaction, id, &mut source);
            if let Err(error) = result {
                source.error = Some(error.to_string());
                transaction.execute("DELETE FROM records WHERE session=?1", [&key])?;
            }
            source.revision = revision;
            transaction.execute(
                "INSERT OR REPLACE INTO sources VALUES (?1,?2)",
                params![key, encode(&source)?],
            )?;
            self.project(&transaction, id, &source)?;
        }
        let cached: Vec<String> = {
            let mut statement = transaction.prepare("SELECT session FROM sources")?;
            statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in cached.iter().filter(|id| !present.contains(*id)) {
            transaction.execute("DELETE FROM episodes WHERE session=?1", [id])?;
            transaction.execute("DELETE FROM records WHERE session=?1", [id])?;
            transaction.execute("DELETE FROM sources WHERE session=?1", [id])?;
        }
        transaction.commit()?;
        Ok(())
    }
    fn consume(
        &self,
        connection: &Connection,
        id: SessionId,
        source: &mut Source,
    ) -> Result<(), HistoryError> {
        let bytes = self
            .store
            .io
            .read_from(&self.store.log_path(&id), source.offset)?
            .ok_or_else(|| invalid("missing history log"))?;
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        source.incomplete = complete != bytes.len();
        let mut consumed = 0;
        for line in bytes[..complete].split_inclusive(|byte| *byte == b'\n') {
            let raw = &line[..line.len() - 1];
            if source.offset == 0 && consumed == 0 {
                let StoreLine::Header(header) = decode_line(raw).map_err(invalid)? else {
                    return Err(invalid("history log has no header"));
                };
                if header.v != CODEC_VERSION || header.session != id {
                    return Err(invalid(
                        "history header has unsupported codec or wrong session",
                    ));
                }
                source.generation = digest(raw);
                source.updated = header.created.to_rfc3339();
            } else {
                self.work.decoded_records.fetch_add(1, Ordering::Relaxed);
                let StoreLine::Record(record) = decode_line(raw).map_err(invalid)? else {
                    return Err(invalid("unknown history record"));
                };
                if record.seq.get() != source.position + 1 {
                    return Err(invalid("noncontiguous history source position"));
                }
                source.position = record.seq.get();
                let entry: SessionEntry =
                    serde_json::from_str(record.payload.get()).map_err(invalid)?;
                if let SessionEntry::Event(event) = entry {
                    source.updated = event.env.ts.to_rfc3339();
                    let (kind, text) = match &event.kind {
                        EventKind::UserPrompt { text, .. } => ("user", text.clone()),
                        EventKind::AssistantMessage { text } if event.env.item.is_some() => {
                            ("assistant", text.clone())
                        }
                        EventKind::ToolCallStarted { .. } => ("tool_call", String::new()),
                        EventKind::ToolCallCompleted { .. } => ("tool_result", String::new()),
                        EventKind::SessionTitleChanged { title } => {
                            source.title = title.clone().unwrap_or_default();
                            ("metadata", source.title.clone())
                        }
                        EventKind::Compacted { summary } => {
                            source.summary = summary.clone();
                            ("metadata", summary.clone())
                        }
                        EventKind::SessionDeleted => {
                            source.deleted = true;
                            ("", String::new())
                        }
                        _ => ("", String::new()),
                    };
                    if !kind.is_empty() {
                        let item = event.env.item.map(|item| item.to_string());
                        if kind == "assistant" {
                            connection.execute("UPDATE records SET current=0 WHERE session=?1 AND item=?2 AND kind='assistant'", params![id.to_string(), item])?;
                        }
                        if kind == "metadata" {
                            // Only the latest value of each metadata field participates in recall.
                            let field =
                                if matches!(event.kind, EventKind::SessionTitleChanged { .. }) {
                                    "title"
                                } else {
                                    "summary"
                                };
                            connection.execute("UPDATE records SET current=0 WHERE session=?1 AND item=?2 AND kind='metadata'", params![id.to_string(), field])?;
                        }
                        let item = if kind == "metadata" {
                            Some(
                                if matches!(event.kind, EventKind::SessionTitleChanged { .. }) {
                                    "title".into()
                                } else {
                                    "summary".into()
                                },
                            )
                        } else {
                            item
                        };
                        let event_id = identity_text(&event.env.id)?;
                        let anchor = format!("{}:{}:{}", source.generation, event_id, digest(raw));
                        connection.execute(
                            "INSERT INTO records VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1)",
                            params![
                                id.to_string(),
                                i64::try_from(record.seq.get()).map_err(invalid)?,
                                anchor,
                                event_id,
                                identity_text(&event.env.turn)?,
                                item,
                                kind,
                                text,
                                i64::try_from(source.offset + consumed as u64).map_err(invalid)?,
                                i64::try_from(raw.len()).map_err(invalid)?
                            ],
                        )?;
                    }
                }
            }
            consumed += line.len();
        }
        source.offset += consumed as u64;
        Ok(())
    }
    fn project(
        &self,
        connection: &Connection,
        id: SessionId,
        source: &Source,
    ) -> Result<(), HistoryError> {
        connection.execute("DELETE FROM episodes WHERE session=?1", [id.to_string()])?;
        if !source.deleted && source.error.is_none() {
            let title = if source.title.is_empty() {
                "Untitled session"
            } else {
                &source.title
            };
            // AND spans one turn, never unrelated turns or metadata. Each metadata
            // value has its own explicit source anchor.
            connection.execute("INSERT INTO episodes(session,anchor,text,title,updated)
                SELECT session,anchor,group_concat(text,char(10)),?2,?3 FROM
                    (SELECT * FROM records WHERE session=?1 AND current=1 AND kind IN ('user','assistant') ORDER BY position)
                GROUP BY turn", params![id.to_string(), title, source.updated])?;
            connection.execute("INSERT INTO episodes(session,anchor,text,title,updated)
                SELECT session,anchor,text,?2,?3 FROM records WHERE session=?1 AND current=1 AND kind='metadata'",
                params![id.to_string(), title, source.updated])?;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    session: SessionId,
    anchor: String,
    evidence: bool,
    end: u64,
    index: usize,
    offset: usize,
    source: Option<String>,
}
struct Located {
    source: String,
    event: String,
    position: i64,
    start: i64,
    length: i64,
    kind: String,
}
#[derive(Serialize)]
struct Opened {
    event: String,
    source: String,
    position: i64,
    kind: String,
    offset: usize,
    text: String,
}
#[derive(Serialize)]
struct Page<'a> {
    session: SessionId,
    anchor: &'a str,
    historical: bool,
    incomplete: bool,
    records: &'a [Opened],
    continuation: Option<String>,
}
fn event_text(kind: &EventKind) -> Option<String> {
    match kind {
        EventKind::UserPrompt { text, .. } => Some(format!("user: {text}")),
        EventKind::AssistantMessage { text } => Some(format!("assistant: {text}")),
        EventKind::SessionTitleChanged { title } => Some(title.clone().unwrap_or_default()),
        EventKind::Compacted { summary } => Some(summary.clone()),
        EventKind::ToolCallStarted { name, args, .. } => {
            Some(serde_json::json!({"name": name, "args": args}).to_string())
        }
        EventKind::ToolCallCompleted { result, .. } => serde_json::to_string(result).ok(),
        _ => None,
    }
}
fn encode(value: &impl Serialize) -> Result<String, HistoryError> {
    serde_json::to_string(value).map_err(invalid)
}
fn identity_text(value: &impl Serialize) -> Result<String, HistoryError> {
    match serde_json::to_value(value).map_err(invalid)? {
        serde_json::Value::String(value) => Ok(value),
        _ => Err(invalid("history identity is not a string")),
    }
}
fn read_record(
    log: &mut dyn super::cold_read::ReadSeek,
    start: i64,
    length: i64,
) -> Result<Vec<u8>, HistoryError> {
    use std::io::SeekFrom;
    log.seek(SeekFrom::Start(u64::try_from(start).map_err(invalid)?))?;
    let length = usize::try_from(length).map_err(invalid)?;
    let mut bytes = vec![0; length];
    log.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn identity(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        format!("{:?}", metadata.created())
    }
}
fn corrupt_message(error: &HistoryError) -> bool {
    error.1
}
fn bounded(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
impl ego_core::history::SessionHistory for HistoryIndex {
    fn read(
        &self,
        request: ego_core::history::HistoryRequest,
    ) -> futures::future::BoxFuture<'_, Result<String, ego_core::history::HistoryReadError>> {
        let index = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || match request {
                ego_core::history::HistoryRequest::Search { query } => index.search(&query),
                ego_core::history::HistoryRequest::Open {
                    session,
                    anchor,
                    evidence,
                    continuation,
                } => index.open_at(
                    session,
                    anchor.as_deref(),
                    evidence,
                    continuation.as_deref(),
                ),
            })
            .await
            .map_err(|error| ego_core::history::HistoryReadError(error.to_string()))?
            .map_err(|error| ego_core::history::HistoryReadError(error.to_string()))
        })
    }
}
