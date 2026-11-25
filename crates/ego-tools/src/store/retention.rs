//! Session retention: delete the sessions nobody opened for the retention
//! period, with their rows in `history.sqlite`, then compact that file.
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_core::state::PauseState;
use ego_core::store::session_store::StoreError;

use super::catalogue::SessionTable;
use super::jsonl_store::PageError;
use super::{JsonlStore, Lease};

/// What one retention pass deleted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    pub deleted: Vec<SessionId>,
    /// Bytes of the deleted session directories.
    pub freed_bytes: u64,
    /// Expired sessions kept because they are open or held.
    pub kept_in_use: usize,
    pub history_bytes_before: u64,
    pub history_bytes_after: u64,
}

impl JsonlStore {
    /// Delete every session not opened since `now - retain`. A session that is
    /// open (its writer lease is held), held (paused), or the source of a kept
    /// session stays. With `daily`, a pass less than one day after the last
    /// one does nothing. `None`: no pass ran, because it was not due or
    /// another process runs one.
    pub fn prune_expired_sessions(
        &self,
        retain: Duration,
        now: Timestamp,
        daily: bool,
    ) -> Result<Option<RetentionReport>, StoreError> {
        let mut job = fd_lock::RwLock::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.root().join("retention.lock"))?,
        );
        let _job = match job.try_write() {
            Ok(guard) => guard,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let path = self.session_table_path();
        let mut table = SessionTable::open(&path).map_err(table_error)?;
        if daily && !table.retention_due(now).map_err(table_error)? {
            return Ok(None);
        }
        self.reconcile_catalogue(&mut table)
            .map_err(|error| match error {
                PageError::Table(error) => table_error(error),
                PageError::Store(error) => error,
            })?;
        let cutoff = now
            .as_millis()
            .saturating_sub(u64::try_from(retain.as_millis()).unwrap_or(u64::MAX));
        let rows = table.retention_rows().map_err(table_error)?;
        let sources: std::collections::HashMap<_, _> =
            rows.iter().map(|row| (row.id, row.source)).collect();
        let mut keep = HashSet::new();
        let protect = |keep: &mut HashSet<SessionId>, mut id: SessionId| {
            while keep.insert(id) {
                match sources.get(&id).copied().flatten() {
                    Some(source) => id = source,
                    None => break,
                }
            }
        };
        for row in &rows {
            if row.last_opened.as_millis() >= cutoff {
                protect(&mut keep, row.id);
            }
        }
        let mut report = RetentionReport {
            history_bytes_before: file_len(&path),
            ..RetentionReport::default()
        };
        // Descendants come first, so a session found in use protects its
        // sources before they are reached.
        for row in &rows {
            if keep.contains(&row.id) {
                continue;
            }
            match self.expire_session(row.id, cutoff) {
                Ok(Some(bytes)) => {
                    tracing::info!(session = %row.id, last_opened = %row.last_opened.to_rfc3339(), bytes, "retention deleted a session");
                    report.deleted.push(row.id);
                    report.freed_bytes += bytes;
                }
                Ok(None) => {
                    report.kept_in_use += 1;
                    protect(&mut keep, row.id);
                }
                Err(error) => {
                    tracing::warn!(session = %row.id, %error, "retention kept a session it could not delete");
                    protect(&mut keep, row.id);
                }
            }
        }
        if !report.deleted.is_empty()
            && let Err(error) = table.forget(&report.deleted)
        {
            // The rows are derived: the next listing and index refresh drop them.
            tracing::warn!(%error, "retention could not compact history.sqlite");
        }
        table.record_retention(now).map_err(table_error)?;
        report.history_bytes_after = file_len(&path);
        tracing::info!(
            deleted = report.deleted.len(),
            freed_bytes = report.freed_bytes,
            kept_in_use = report.kept_in_use,
            history_bytes_before = report.history_bytes_before,
            history_bytes_after = report.history_bytes_after,
            "session retention pass finished"
        );
        Ok(Some(report))
    }

    /// Delete one expired session under its writer lease, after checking its
    /// manifest again. `None`: the session is open, held, or not expired.
    fn expire_session(&self, id: SessionId, cutoff: u64) -> Result<Option<u64>, StoreError> {
        let dir = self.session_dir(&id);
        if !fs::symlink_metadata(&dir)?.file_type().is_dir() {
            return Err(StoreError::Io(io::Error::other(
                "session is not a real directory",
            )));
        }
        let Some(_lease) = Lease::probe(&self.lease_path(&id))? else {
            return Ok(None);
        };
        let Some(meta) = self.listed_meta(id)? else {
            return Err(StoreError::NotFound);
        };
        let held = self
            .listed_facts(id)?
            .is_some_and(|facts| facts.pause != PauseState::Running);
        if held || meta.updated.as_millis() >= cutoff {
            return Ok(None);
        }
        let mut lock = self.open_events_lock(&id)?;
        let _guard = lock.write()?;
        let bytes = dir_len(&dir);
        fs::remove_dir_all(&dir)?;
        Ok(Some(bytes))
    }
}

fn table_error(error: rusqlite::Error) -> StoreError {
    StoreError::Io(io::Error::other(format!("session table: {error}")))
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn dir_len(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_len(&entry.path()),
            Ok(_) => entry.metadata().map_or(0, |metadata| metadata.len()),
            Err(_) => 0,
        })
        .sum()
}
