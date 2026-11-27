//! The session table in the derived `history.sqlite`.
//!
//! Manifests stay the source of truth. This table only orders and filters the
//! catalogue, so one page is one query; every row it returns is checked again
//! against its manifest. A missing or damaged file is rebuilt from the
//! manifests, and a caller that cannot open it scans the manifests instead.
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::session::SessionLineage;
use ego_core::store::session_store::SessionMeta;
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions(
        id TEXT PRIMARY KEY, cwd BLOB NOT NULL, created INTEGER NOT NULL,
        updated INTEGER NOT NULL, title TEXT, deleted INTEGER NOT NULL, lineage TEXT,
        opened INTEGER);
    CREATE TABLE IF NOT EXISTS maintenance(name TEXT PRIMARY KEY, at INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS sessions_page ON sessions(cwd, created DESC, id);
    CREATE INDEX IF NOT EXISTS sessions_created ON sessions(created DESC, id);";

pub(super) struct SessionTable(Connection);

impl SessionTable {
    /// Open the table, rebuilding a damaged file once. Only the derived file is
    /// disposable; the caller repopulates it from the manifests.
    pub(super) fn open(path: &Path) -> rusqlite::Result<Self> {
        match Self::try_open(path) {
            Err(error) if corrupt(&error) => {
                tracing::warn!(path = %path.display(), %error, "rebuilding the damaged session table");
                if let Err(error) = std::fs::remove_file(path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(path = %path.display(), %error, "the damaged session table could not be removed");
                }
                Self::try_open(path)
            }
            result => result,
        }
    }

    fn try_open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        // A long index writer must not stall a listing: it scans the manifests.
        connection.busy_timeout(Duration::from_millis(250))?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self(connection))
    }

    /// Keep the latest time a client opened `id`.
    pub(super) fn note_opened(&self, id: SessionId, at: Timestamp) -> rusqlite::Result<()> {
        self.0
            .prepare_cached("UPDATE sessions SET opened=max(coalesce(opened, 0), ?2) WHERE id=?1")?
            .execute(params![id.to_string(), millis(at)])?;
        Ok(())
    }

    /// Every row, newest first: a fork is always created after its source, so
    /// a descendant comes before its ancestors.
    pub(super) fn retention_rows(&self) -> rusqlite::Result<Vec<RetentionRow>> {
        let mut statement = self.0.prepare(
            "SELECT id, max(updated, coalesce(opened, 0)), lineage FROM sessions
             ORDER BY created DESC, id",
        )?;
        let rows = statement.query_map([], |row| {
            let id: String = row.get(0)?;
            let last: i64 = row.get(1)?;
            let lineage: Option<String> = row.get(2)?;
            Ok(RetentionRow {
                id: parse_id(&id)?,
                last_opened: Timestamp::from_millis(u64::try_from(last).unwrap_or(0)),
                source: lineage.as_deref().map(parse_id).transpose()?,
            })
        })?;
        rows.collect()
    }

    /// Whether the last retention pass is at least one day before `now`.
    pub(super) fn retention_due(&self, now: Timestamp) -> rusqlite::Result<bool> {
        let last: Option<i64> = self
            .0
            .query_row(
                "SELECT at FROM maintenance WHERE name='retention'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(last.is_none_or(|last| millis(now).saturating_sub(last) >= DAY_MILLIS))
    }

    pub(super) fn record_retention(&self, now: Timestamp) -> rusqlite::Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO maintenance(name, at) VALUES ('retention', ?1)",
            [millis(now)],
        )?;
        Ok(())
    }

    /// Remove the rows of deleted sessions from this table and from the
    /// history index beside it, then compact the file.
    pub(super) fn forget(&mut self, ids: &[SessionId]) -> rusqlite::Result<()> {
        let transaction = self.0.transaction()?;
        let history: Vec<String> = {
            let mut statement = transaction.prepare(
                "SELECT name FROM sqlite_master WHERE type='table'
                 AND name IN ('sources', 'records', 'episodes')",
            )?;
            statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            let id = id.to_string();
            transaction.execute("DELETE FROM sessions WHERE id=?1", [&id])?;
            for table in &history {
                transaction.execute(&format!("DELETE FROM {table} WHERE session=?1"), [&id])?;
            }
        }
        transaction.commit()?;
        self.0.busy_timeout(Duration::from_secs(10))?;
        self.0.execute_batch("VACUUM")
    }

    pub(super) fn ids(&self) -> rusqlite::Result<HashSet<String>> {
        let mut statement = self.0.prepare("SELECT id FROM sessions")?;
        statement.query_map([], |row| row.get(0))?.collect()
    }

    /// Write rows in one transaction.
    pub(super) fn upsert_all<'a>(
        &mut self,
        rows: impl IntoIterator<Item = (&'a SessionMeta, bool)>,
    ) -> rusqlite::Result<()> {
        let transaction = self.0.transaction()?;
        for (meta, deleted) in rows {
            upsert(&transaction, meta, deleted)?;
        }
        transaction.commit()
    }

    pub(super) fn upsert(&self, meta: &SessionMeta, deleted: bool) -> rusqlite::Result<()> {
        upsert(&self.0, meta, deleted)
    }

    pub(super) fn remove_all<'a>(
        &mut self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> rusqlite::Result<()> {
        let transaction = self.0.transaction()?;
        for id in ids {
            transaction.execute("DELETE FROM sessions WHERE id=?1", [id])?;
        }
        transaction.commit()
    }

    /// Up to `limit` live rows after `after`, newest first and then by id: the
    /// order and the keyset of `SessionLifecycle::list`.
    pub(super) fn page(
        &self,
        root: Option<&Path>,
        after: Option<(Timestamp, SessionId)>,
        limit: usize,
    ) -> rusqlite::Result<Vec<(Timestamp, SessionId)>> {
        let (created, id) = match after {
            Some((created, id)) => (Some(millis(created)), Some(id.to_string())),
            None => (None, None),
        };
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let cwd = root.map(path_bytes);
        let mut statement = self.0.prepare_cached(
            "SELECT created, id FROM sessions
             WHERE (?1 IS NULL OR cwd=?1) AND deleted=0
               AND (?2 IS NULL OR created<?2 OR (created=?2 AND id>?3))
             ORDER BY created DESC, id LIMIT ?4",
        )?;
        let rows = statement.query_map(params![cwd, created, id, limit], |row| {
            let created: i64 = row.get(0)?;
            let id: String = row.get(1)?;
            let id = id.parse::<SessionId>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    error.to_string().into(),
                )
            })?;
            Ok((
                Timestamp::from_millis(u64::try_from(created).unwrap_or(0)),
                id,
            ))
        })?;
        rows.collect()
    }
}

/// One session as retention sees it.
pub(super) struct RetentionRow {
    pub(super) id: SessionId,
    /// The later of the last write and the last time a client opened it.
    pub(super) last_opened: Timestamp,
    /// The session its lineage starts from, which it may replay.
    pub(super) source: Option<SessionId>,
}

const DAY_MILLIS: i64 = 86_400_000;

fn parse_id(id: &str) -> rusqlite::Result<SessionId> {
    id.parse().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("invalid session id {id}").into(),
        )
    })
}

fn upsert(connection: &Connection, meta: &SessionMeta, deleted: bool) -> rusqlite::Result<()> {
    let lineage = meta.lineage.map(|lineage| {
        let (SessionLineage::Fork { source, .. } | SessionLineage::Compaction { source, .. }) =
            lineage;
        source.to_string()
    });
    connection
        .prepare_cached(
            "INSERT INTO sessions(id, cwd, created, updated, title, deleted, lineage)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET cwd=excluded.cwd, created=excluded.created,
                 updated=excluded.updated, title=excluded.title, deleted=excluded.deleted,
                 lineage=excluded.lineage",
        )?
        .execute(params![
            meta.id.to_string(),
            path_bytes(&meta.workspace_root),
            millis(meta.created),
            millis(meta.updated),
            meta.title,
            deleted,
            lineage,
        ])?;
    Ok(())
}

fn millis(at: Timestamp) -> i64 {
    i64::try_from(at.as_millis()).unwrap_or(i64::MAX)
}

/// Exact path bytes: a lossy string could make two roots compare equal.
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

/// Remove a file whose damage showed only after it opened; the next listing
/// rebuilds it.
pub(super) fn discard_if_corrupt(path: &Path, error: &rusqlite::Error) {
    if corrupt(error) {
        let _ = std::fs::remove_file(path);
    }
}

fn corrupt(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(code, _)
        if matches!(code.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase))
}
