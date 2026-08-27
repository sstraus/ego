//! Append-only machine evidence for MCP registry reconciliation.

use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use ego_core::turn::{
    McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError, McpRegistryJournalErrorKind,
    McpRegistryJournalOperation,
};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

use crate::store::{AtomicReplacePublication, StoreIo};

const JOURNAL_FILE: &str = "registry.jsonl";
const LOCK_FILE: &str = "registry.lock";
const RECORD_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    event: McpRegistryEvent,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

/// JSONL adapter under `$EGO_HOME/mcp/`.
pub struct JsonlMcpRegistryJournal {
    path: PathBuf,
    lock_path: PathBuf,
    io: Box<dyn StoreIo>,
}

impl JsonlMcpRegistryJournal {
    pub fn new(root: impl Into<PathBuf>, io: Box<dyn StoreIo>) -> Self {
        let root = root.into();
        Self {
            path: root.join(JOURNAL_FILE),
            lock_path: root.join(LOCK_FILE),
            io,
        }
    }

    fn error(
        operation: McpRegistryJournalOperation,
        kind: McpRegistryJournalErrorKind,
    ) -> McpRegistryJournalError {
        McpRegistryJournalError::new(operation, kind)
    }

    fn with_lock<T>(
        &self,
        operation: McpRegistryJournalOperation,
        work: impl FnOnce() -> Result<T, McpRegistryJournalError>,
    ) -> Result<T, McpRegistryJournalError> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::Unavailable))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::Unavailable))?;
        let mut lock = RwLock::new(file);
        let _guard = lock
            .write()
            .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::Unavailable))?;
        work()
    }

    fn recover_torn_tail(
        &self,
        bytes: &[u8],
        operation: McpRegistryJournalOperation,
    ) -> Result<(), McpRegistryJournalError> {
        let committed_len = committed_prefix(bytes).len();
        if committed_len == bytes.len() {
            return Ok(());
        }
        match self
            .io
            .replace_atomic_publication(&self.path, &bytes[..committed_len])
            .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::Unavailable))?
        {
            AtomicReplacePublication::PublishedDurably => Ok(()),
            AtomicReplacePublication::PublishedDurabilityUncertain { .. } => Err(Self::error(
                operation,
                McpRegistryJournalErrorKind::DurabilityUncertain,
            )),
        }
    }

    fn append_locked(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        let operation = McpRegistryJournalOperation::Append;
        if let Some(bytes) = self
            .io
            .read(&self.path)
            .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::Unavailable))?
        {
            decode_committed(committed_prefix(&bytes), operation)?;
            self.recover_torn_tail(&bytes, operation)?;
        }

        let line = serde_json::to_vec(&Record {
            version: RECORD_VERSION,
            event,
        })
        .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::Corrupt))?;
        self.io
            .append_line(&self.path, &line)
            .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::Unavailable))?;
        self.io
            .fsync(&self.path)
            .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::DurabilityUncertain))
    }

    fn replay_locked(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        let operation = McpRegistryJournalOperation::Replay;
        let Some(bytes) = self
            .io
            .read(&self.path)
            .map_err(|_| Self::error(operation, McpRegistryJournalErrorKind::Unavailable))?
        else {
            return Ok(Vec::new());
        };

        decode_committed(committed_prefix(&bytes), operation)
    }
}

impl McpRegistryJournal for JsonlMcpRegistryJournal {
    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        self.with_lock(McpRegistryJournalOperation::Append, || {
            self.append_locked(event)
        })
    }

    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        self.with_lock(McpRegistryJournalOperation::Replay, || self.replay_locked())
    }
}

fn committed_prefix(bytes: &[u8]) -> &[u8] {
    bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(&[][..], |last| &bytes[..=last])
}

fn decode_committed(
    committed: &[u8],
    operation: McpRegistryJournalOperation,
) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
    let error = |kind| McpRegistryJournalError::new(operation, kind);
    let mut events = Vec::new();
    if committed.is_empty() {
        return Ok(events);
    }
    for line in committed[..committed.len() - 1].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            return Err(error(McpRegistryJournalErrorKind::Corrupt));
        }
        let version: VersionProbe = serde_json::from_slice(line)
            .map_err(|_| error(McpRegistryJournalErrorKind::Corrupt))?;
        if version.version != RECORD_VERSION {
            return Err(error(McpRegistryJournalErrorKind::UnsupportedVersion));
        }
        let record: Record = serde_json::from_slice(line)
            .map_err(|_| error(McpRegistryJournalErrorKind::Corrupt))?;
        events.push(record.event);
    }
    Ok(events)
}
