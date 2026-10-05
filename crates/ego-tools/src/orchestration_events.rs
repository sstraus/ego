//! Fixed host-owned observation sources and one durable cursor. No model calls.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ego_core::event::id::SessionId;
use ego_core::orchestration_events::{Card, Cursor, Observation, Salience, content_id};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::store::{Lease, OsIo, StoreIo};

#[derive(Debug, Deserialize)]
pub struct InboxPage {
    pub messages: Vec<InboxMessage>,
    pub next_since: u64,
    pub has_more: bool,
    #[serde(default)]
    pub missed_count: u64,
}

#[derive(Debug, Deserialize)]
pub struct InboxMessage {
    pub content: String,
    pub timestamp: u64,
    pub from_name: String,
}

/// A local lease prevents two observers from advancing the same root's cursor.
pub struct ObservationPump {
    cursor: Cursor,
    cursor_path: PathBuf,
    results: PathBuf,
    children: PathBuf,
    _lease: Lease,
}

impl ObservationPump {
    pub fn open(home: &Path, root: &Path) -> io::Result<Self> {
        let state = home.join("Gits/.tmp/coordinator");
        let cursor_path = state.join(format!(
            "ego-observer-{}.json",
            content_id(&root.to_string_lossy())
        ));
        let lease = Lease::probe(&cursor_path.with_extension("lock"))?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "an observer already owns this root",
            )
        })?;
        let mut cursor: Cursor = match std::fs::read(&cursor_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Cursor::default(),
            Err(error) => return Err(error),
        };
        if cursor.observer.is_empty() {
            // Dedicated identity, never copied from the launching coordinator.
            cursor.observer = SessionId::new().to_string();
        }
        let pump = Self {
            cursor,
            cursor_path,
            results: home.join("Gits/.tmp/results"),
            children: state.join("children.tsv"),
            _lease: lease,
        };
        pump.persist()?;
        Ok(pump)
    }

    pub fn observer(&self) -> &str {
        &self.cursor.observer
    }
    pub fn cursor_path(&self) -> &Path {
        &self.cursor_path
    }
    pub fn ready(&self, now_ms: u64) -> &[Card] {
        self.cursor.ready(now_ms)
    }

    fn persist(&self) -> io::Result<()> {
        OsIo.replace_atomic_private(
            &self.cursor_path,
            &serde_json::to_vec(&self.cursor).map_err(io::Error::other)?,
        )
    }

    pub fn acknowledge(&mut self) -> io::Result<()> {
        let mut next = self.cursor.clone();
        next.acknowledge();
        OsIo.replace_atomic_private(
            &self.cursor_path,
            &serde_json::to_vec(&next).map_err(io::Error::other)?,
        )?;
        self.cursor = next;
        Ok(())
    }

    /// Prepare argv with only this observer's caller identity, even under Claude.
    fn command(&self, operation: &str, params: Value) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("tuic");
        command
            .args(["mcp", operation, &params.to_string()])
            .env("TUIC_SESSION", self.observer())
            .env_remove("TUIC_PARENT")
            .kill_on_drop(true);
        command
    }

    async fn call(&self, operation: &str, params: Value) -> io::Result<Value> {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            self.command(operation, params).output(),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TUIC observer read timed out"))??;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "TUIC observer {operation} failed: {}",
                output.status
            )));
        }
        serde_json::from_slice(&output.stdout).map_err(io::Error::other)
    }

    pub async fn register(&self) -> io::Result<()> {
        self.call(
            "agent",
            json!({"action":"register", "tuic_session":self.observer(), "name":"ego-observer"}),
        )
        .await?;
        Ok(())
    }

    pub async fn poll(&mut self, now_ms: u64) -> io::Result<()> {
        // Files first survive mail-source failure; no shared watch stamp is changed.
        self.scan_results(now_ms)?;
        self.persist()?;
        // Bound the cycle even if mail keeps arriving; retained pages resume next tick.
        for _ in 0..2 {
            let page: InboxPage = serde_json::from_value(
                self.call(
                    "agent",
                    json!({"action":"inbox", "since":self.cursor.inbox_since}),
                )
                .await?,
            )
            .map_err(io::Error::other)?;
            let more = page.has_more;
            let previous = self.cursor.inbox_since;
            self.ingest_inbox(page, now_ms)?;
            if !more {
                break;
            }
            if self.cursor.inbox_since <= previous {
                return Err(io::Error::other("TUIC inbox cursor did not advance"));
            }
        }
        let sessions = self.call("session", json!({"action":"list"})).await?;
        self.ingest_sessions(&sessions, now_ms)?;
        Ok(())
    }

    /// A recorded page is committed before the next request advances its cursor.
    pub fn ingest_inbox(&mut self, page: InboxPage, now_ms: u64) -> io::Result<()> {
        if page.next_since < self.cursor.inbox_since {
            return Err(io::Error::other("TUIC inbox cursor regressed"));
        }
        for message in page.messages {
            self.observe(
                Observation {
                    text: message.content,
                    at_ms: message.timestamp,
                    result: None,
                    peer: Some(message.from_name),
                },
                now_ms,
            );
        }
        if page.missed_count != 0 {
            self.failure(
                format!(
                    "TUIC observer missed {} evicted messages at {}",
                    page.missed_count, page.next_since
                ),
                now_ms,
            );
        }
        self.cursor.inbox_since = page.next_since;
        self.persist()
    }

    pub fn scan_results(&mut self, now_ms: u64) -> io::Result<()> {
        let entries = match std::fs::read_dir(&self.results) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.path().extension().is_some_and(|ext| ext == "md")
            {
                files.push((entry.metadata()?.modified()?, entry.path()));
            }
        }
        files.sort();
        for (modified, path) in files {
            let mut bytes = Vec::new();
            std::fs::File::open(&path)?
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            let text = if bytes.len() > 64 * 1024 {
                format!("BLOCKED: result exceeds 64 KiB: {}", path.display())
            } else {
                String::from_utf8(bytes).map_err(io::Error::other)?
            };
            let revision = content_id(&text);
            if self.cursor.files.get(&path) == Some(&revision) {
                continue;
            }
            self.cursor.files.insert(path.clone(), revision);
            let peer = tracked_peer(&self.children, &path)?;
            self.observe(
                Observation {
                    text,
                    at_ms: millis(modified),
                    result: Some(path),
                    peer,
                },
                now_ms,
            );
        }
        self.persist()
    }

    pub fn ingest_sessions(&mut self, sessions: &Value, now_ms: u64) -> io::Result<()> {
        let sessions = sessions
            .as_array()
            .ok_or_else(|| io::Error::other("invalid TUIC session list"))?;
        let tracking = read_tracking(&self.children)?;
        for row in tracking.lines() {
            let fields: Vec<_> = row.split('\t').collect();
            let [id, name, ..] = fields.as_slice() else {
                return Err(io::Error::other("invalid tracked child"));
            };
            if let Some(session) = sessions
                .iter()
                .find(|s| s["session_id"].as_str() == Some(id))
            {
                let state = session["agent_state"]
                    .as_str()
                    .or_else(|| session["shell_state"].as_str());
                self.cursor.child_state(name, state == Some("idle"), now_ms);
            }
        }
        self.persist()
    }

    fn observe(&mut self, event: Observation, now_ms: u64) {
        let text = event.text.clone();
        let salience = self.cursor.observe(event, now_ms);
        if salience == Salience::Activity {
            tracing::info!(event = %text, "orchestration activity");
        }
    }

    pub fn failure(&mut self, error: String, now_ms: u64) {
        self.observe(
            Observation {
                text: format!("BLOCKED: observer source failure: {error}"),
                at_ms: now_ms,
                result: None,
                peer: None,
            },
            now_ms,
        );
    }

    pub fn commit(&self) -> io::Result<()> {
        self.persist()
    }
}

fn read_tracking(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        other => other,
    }
}

fn tracked_peer(children: &Path, result: &Path) -> io::Result<Option<String>> {
    let filename = result.file_name().unwrap_or_default().to_string_lossy();
    Ok(read_tracking(children)?
        .lines()
        .filter_map(|row| row.split('\t').nth(1))
        .filter(|name| filename.starts_with(&format!("{name}-")))
        .max_by_key(|name| name.len())
        .map(str::to_owned))
}

pub fn millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    // Catches: the observer consumes its launching Claude coordinator's inbox.
    #[test]
    fn observer_command_replaces_caller_identity_and_removes_parent_identity() {
        let home = tempfile::tempdir().unwrap();
        let pump = ObservationPump::open(home.path(), home.path()).unwrap();
        let command = pump.command("agent", json!({"action":"inbox", "since":0}));
        let env: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
        assert_eq!(
            env[OsStr::new("TUIC_SESSION")],
            Some(OsStr::new(pump.observer()))
        );
        assert_eq!(env[OsStr::new("TUIC_PARENT")], None);
        assert_eq!(command.as_std().get_args().next(), Some(OsStr::new("mcp")));
    }
}
