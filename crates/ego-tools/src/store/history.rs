//! Rebuildable conversational FTS5; JSONL remains the source of truth.
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::Arc;

use ego_core::event::domain::EventKind;
use ego_core::event::id::SessionId;
use ego_core::session::ReplaySource;
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_core::store::session_store::SessionEntry;
use rusqlite::{Connection, ErrorCode, params};
use serde::Serialize;

use super::JsonlStore;

/// Only the rebuildable index, never a session log, may be removed on error.
#[derive(Debug)]
pub struct HistoryError(String);
impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for HistoryError {}
impl From<std::io::Error> for HistoryError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}
impl From<rusqlite::Error> for HistoryError {
    fn from(error: rusqlite::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryHit {
    pub id: SessionId,
    pub title: String,
    pub updated: String,
    pub snippet: String,
}

/// A session omitted because its authoritative log cannot be read.
#[derive(Debug, Serialize)]
struct UnreadableSession {
    id: SessionId,
    error: String,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    hits: Vec<HistoryHit>,
    unreadable: Vec<UnreadableSession>,
    omitted_unreadable: usize,
}

/// One store-owned derived index. Connections never survive the file lock.
#[derive(Clone)]
pub struct HistoryIndex {
    path: PathBuf,
    store: Arc<JsonlStore>,
}
impl HistoryIndex {
    pub(super) fn new(path: PathBuf, store: Arc<JsonlStore>) -> Self {
        Self { path, store }
    }

    /// Search all catalogue roots. Callers can map ids back to SessionMeta.
    pub fn matches(&self, query: &str) -> Result<Vec<HistoryHit>, HistoryError> {
        self.search_result(query).map(|result| result.hits)
    }

    fn search_result(&self, query: &str) -> Result<SearchResult, HistoryError> {
        let query = query
            .split_whitespace()
            .filter(|term| term.chars().any(char::is_alphanumeric))
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        if query.is_empty() {
            return Ok(SearchResult {
                hits: Vec::new(),
                unreadable: Vec::new(),
                omitted_unreadable: 0,
            });
        }
        self.with_index(|connection, unreadable| {
            let mut statement = connection.prepare(
                "SELECT id, title, updated, snippet(history, -1, '', '', '…', 20) FROM history
                 WHERE history MATCH ?1 ORDER BY rank, updated DESC, id LIMIT 50",
            )?;
            let rows = statement.query_map([query], |row| {
                let id: String = row.get(0)?;
                Ok((
                    id,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let mut hits = Vec::new();
            for row in rows {
                let (id, title, updated, snippet) = row?;
                let id = serde_json::from_value(serde_json::Value::String(id))
                    .map_err(|error| HistoryError(error.to_string()))?;
                hits.push(HistoryHit {
                    id,
                    title,
                    updated,
                    snippet,
                });
            }
            Ok(SearchResult {
                hits,
                unreadable,
                omitted_unreadable: 0,
            })
        })
    }

    /// Model-facing JSON always fits one KiB, including punctuation and escaping.
    pub fn search(&self, query: &str) -> Result<String, HistoryError> {
        let result = self.search_result(query)?;
        let mut selected = SearchResult {
            hits: Vec::new(),
            unreadable: Vec::new(),
            omitted_unreadable: result.unreadable.len(),
        };
        // Keep room for readable hits even when many sessions are unreadable.
        for mut unreadable in result.unreadable {
            unreadable.error = bounded(&unreadable.error, 240).to_owned();
            selected.unreadable.push(unreadable);
            selected.omitted_unreadable -= 1;
            if encode(&selected)?.len() > 512 {
                selected.unreadable.pop();
                selected.omitted_unreadable += 1;
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

    /// Opening bypasses the index and reads only this session's conversation.
    pub fn open(&self, id: SessionId) -> Result<String, HistoryError> {
        let (text, deleted) = self.conversation(id)?;
        if deleted {
            return Err(HistoryError(format!("session {id} is deleted")));
        }
        if text.len() <= 8192 {
            return Ok(text);
        }
        Ok(format!(
            "{}\n[remaining conversation omitted]",
            bounded(&text, 8150)
        ))
    }

    fn conversation(&self, id: SessionId) -> Result<(String, bool), HistoryError> {
        let entries = self
            .store
            .entries(&id)
            .map_err(|error| HistoryError(error.to_string()))?
            .ok_or_else(|| HistoryError(format!("session {id} is missing")))?;
        let mut parts = Vec::new();
        let mut deleted = false;
        let mut assistants: HashMap<ego_core::event::id::ItemId, usize> = HashMap::new();
        for entry in entries {
            let SessionEntry::Event(event) = entry else {
                continue;
            };
            match event.kind {
                EventKind::SessionDeleted => deleted = true,
                EventKind::UserPrompt { text, .. } => parts.push(format!("user: {text}")),
                EventKind::AssistantMessage { text } => {
                    if let Some(item) = event.env.item {
                        if let Some(index) = assistants.get(&item) {
                            parts[*index] = format!("assistant: {text}");
                        } else {
                            assistants.insert(item, parts.len());
                            parts.push(format!("assistant: {text}"));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok((parts.join("\n"), deleted))
    }

    fn with_index<T>(
        &self,
        action: impl FnOnce(&Connection, Vec<UnreadableSession>) -> Result<T, HistoryError>,
    ) -> Result<T, HistoryError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| HistoryError("index has no directory".into()))?;
        fs::create_dir_all(parent)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))?;
        let mut lock = fd_lock::RwLock::new(lock);
        let _guard = lock.write()?;
        let mut connection = match self.connect() {
            Ok(connection) => connection,
            Err(error) if is_corrupt(&error) => {
                fs::remove_file(&self.path)?;
                Connection::open(&self.path)?
            }
            Err(error) => return Err(error.into()),
        };
        connection.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS history USING fts5(
             id UNINDEXED, title, summary, conversation, updated UNINDEXED, source_key UNINDEXED);",
        )?;
        let unreadable = self.refresh(&mut connection)?;
        action(&connection, unreadable)
    }

    fn connect(&self) -> rusqlite::Result<Connection> {
        let connection = Connection::open(&self.path)?;
        let check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if check != "ok" {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(check),
            ));
        }
        // FTS shadow-table consistency is separate from SQLite page integrity.
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='history')",
            [],
            |row| row.get(0),
        )?;
        if exists {
            connection.execute(
                "INSERT INTO history(history) VALUES ('integrity-check')",
                [],
            )?;
        }
        Ok(connection)
    }

    fn refresh(&self, connection: &mut Connection) -> Result<Vec<UnreadableSession>, HistoryError> {
        let root = self
            .path
            .parent()
            .ok_or_else(|| HistoryError("index has no directory".into()))?;
        let directories = self.store.io.list_dirs(root)?;
        let cached: HashMap<String, String> = {
            let mut statement = connection.prepare("SELECT id, source_key FROM history")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let transaction = connection.transaction()?;
        let mut present = HashSet::new();
        let mut unreadable = Vec::new();
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
            let file = match fs::metadata(self.store.log_path(&id)) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let source_key = format!("{}:{:?}", file.len(), file.modified()?);
            let key = id.to_string();
            present.insert(key.clone());
            if cached.get(&key) == Some(&source_key) {
                continue;
            }
            let session = (|| {
                let meta = self
                    .store
                    .lifecycle_meta(id)
                    .map_err(|error| HistoryError(error.to_string()))?;
                let Some(meta) = meta else {
                    return Ok(None);
                };
                let conversation = self.conversation(id)?;
                Ok::<_, HistoryError>(Some((meta, conversation)))
            })();
            let (meta, (conversation, deleted)) = match session {
                Ok(Some(session)) => session,
                Ok(None) => {
                    present.remove(&key);
                    continue;
                }
                Err(error) => {
                    tracing::warn!(session = %id, %error, "history skipped unreadable session");
                    unreadable.push(UnreadableSession {
                        id,
                        error: error.to_string(),
                    });
                    present.remove(&key);
                    continue;
                }
            };
            transaction.execute("DELETE FROM history WHERE id=?1", [&key])?;
            if deleted {
                continue;
            }
            let id = key;
            transaction.execute("INSERT INTO history (id,title,summary,conversation,updated,source_key) VALUES (?1,?2,?3,?4,?5,?6)",
                params![id, meta.title.as_deref().unwrap_or("Untitled session"), meta.summary.as_deref().unwrap_or(""), conversation, meta.updated.to_rfc3339(), source_key])?;
        }
        for id in cached.keys().filter(|id| !present.contains(*id)) {
            transaction.execute("DELETE FROM history WHERE id=?1", [id])?;
        }
        transaction.commit()?;
        Ok(unreadable)
    }
}

fn encode(result: &SearchResult) -> Result<String, HistoryError> {
    serde_json::to_string(result).map_err(|error| HistoryError(error.to_string()))
}

fn is_corrupt(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(error, _) if matches!(error.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase))
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
                ego_core::history::HistoryRequest::Open { session } => index.open(session),
            })
            .await
            .map_err(|error| ego_core::history::HistoryReadError(error.to_string()))?
            .map_err(|error| ego_core::history::HistoryReadError(error.to_string()))
        })
    }
}
