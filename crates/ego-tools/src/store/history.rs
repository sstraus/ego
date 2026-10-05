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
        let query = query
            .split_whitespace()
            .filter(|term| term.chars().any(char::is_alphanumeric))
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        if query.is_empty() {
            return Ok(Vec::new());
        }
        self.with_index(|connection| {
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
            Ok(hits)
        })
    }

    /// Model-facing JSON always fits one KiB, including punctuation and escaping.
    pub fn search(&self, query: &str) -> Result<String, HistoryError> {
        let mut selected = Vec::new();
        for mut hit in self.matches(query)? {
            hit.title = bounded(&hit.title, 60).to_owned();
            hit.snippet = bounded(&hit.snippet, 80).to_owned();
            selected.push(hit);
            let encoded = serde_json::to_string(&selected)
                .map_err(|error| HistoryError(error.to_string()))?;
            if encoded.len() > 1024 {
                selected.pop();
                break;
            }
        }
        serde_json::to_string(&selected).map_err(|error| HistoryError(error.to_string()))
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
        action: impl FnOnce(&Connection) -> Result<T, HistoryError>,
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
        self.refresh(&mut connection)?;
        action(&connection)
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

    fn refresh(&self, connection: &mut Connection) -> Result<(), HistoryError> {
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
            let Some(meta) = self
                .store
                .lifecycle_meta(id)
                .map_err(|error| HistoryError(error.to_string()))?
            else {
                present.remove(&key);
                continue;
            };
            let (conversation, deleted) = self.conversation(id)?;
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
        Ok(())
    }
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
