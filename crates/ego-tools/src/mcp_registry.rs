//! Append-only machine evidence for MCP registry reconciliation.

use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use ego_core::turn::{
    McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError, McpRegistryJournalErrorKind,
    McpRegistryJournalOperation,
};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

use crate::privacy::{PrivatePathKind, harden_private_path, validate_private_path};
use crate::store::{AtomicReplacePublication, StoreIo};

const JOURNAL_FILE: &str = "registry.jsonl";
const LOCK_FILE: &str = "registry.lock";
const RECORD_VERSION: u32 = 1;
const PRIVATE_DIRECTORY: &str = "private-diagnostics";
const PRIVATE_LOG: &str = "errors.jsonl";

#[derive(Serialize)]
struct PrivateDiagnostic<'a> {
    server: &'a str,
    cause: &'a str,
}

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
    private_root: PathBuf,
}

impl JsonlMcpRegistryJournal {
    pub fn new(root: impl Into<PathBuf>, io: Box<dyn StoreIo>) -> Self {
        let root = root.into();
        Self::with_private_diagnostic_root(&root, root.join(PRIVATE_DIRECTORY), io)
    }

    /// Keep private diagnostics beyond an ephemeral registry's lifetime.
    pub fn with_private_diagnostic_root(
        root: impl Into<PathBuf>,
        private_root: impl Into<PathBuf>,
        io: Box<dyn StoreIo>,
    ) -> Self {
        let root = root.into();
        Self {
            private_root: private_root.into(),
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

    fn append_locked(
        &self,
        event: McpRegistryEvent,
        operation: McpRegistryJournalOperation,
    ) -> Result<(), McpRegistryJournalError> {
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

    fn replay_locked(
        &self,
        operation: McpRegistryJournalOperation,
    ) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
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
    fn record_private_diagnostic(
        &self,
        server: &str,
        cause: &str,
    ) -> Result<(), McpRegistryJournalError> {
        let operation = McpRegistryJournalOperation::PrivateDiagnostic;
        let error = || Self::error(operation, McpRegistryJournalErrorKind::Unavailable);
        self.with_lock(operation, || {
            let root = &self.private_root;
            if let Some(parent) = root.parent() {
                fs::create_dir_all(parent).map_err(|_| error())?;
            }
            // The directory is private before the first backend byte is written.
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&root) {
                Ok(()) => {
                    harden_private_path(&root, PrivatePathKind::Directory).map_err(|_| error())?
                }
                Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(error()),
            }
            validate_private_path(root, PrivatePathKind::Directory).map_err(|_| error())?;
            let lock_path = root.join("errors.lock");
            let mut options = OpenOptions::new();
            options.create(true).read(true).write(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            // Reject substituted objects before opening a private lock.
            match fs::symlink_metadata(&lock_path) {
                Ok(_) => {
                    validate_private_path(&lock_path, PrivatePathKind::File).map_err(|_| error())?
                }
                Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(error()),
            }
            let lock_file = options.open(&lock_path).map_err(|_| error())?;
            harden_private_path(&lock_path, PrivatePathKind::File).map_err(|_| error())?;
            let mut private_lock = RwLock::new(lock_file);
            let _guard = private_lock.write().map_err(|_| error())?;
            let path = root.join(PRIVATE_LOG);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    validate_private_path(&path, PrivatePathKind::File).map_err(|_| error())?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if metadata.nlink() != 1 {
                            return Err(error());
                        }
                    }
                    #[cfg(not(unix))]
                    let _ = metadata;
                }
                Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(error()),
            }
            let mut bytes = self
                .io
                .read(&path)
                .map_err(|_| error())?
                .unwrap_or_default();
            serde_json::to_writer(&mut bytes, &PrivateDiagnostic { server, cause })
                .map_err(|_| error())?;
            bytes.push(b'\n');
            // Reuse the existing private atomic writer; ordinary append_line does
            // not enforce private permissions. The writer lock serializes appends.
            self.io
                .replace_atomic_private(&path, &bytes)
                .map_err(|_| error())?;
            validate_private_path(&path, PrivatePathKind::File).map_err(|_| error())
        })
    }

    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        self.with_lock(McpRegistryJournalOperation::Append, || {
            self.append_locked(event, McpRegistryJournalOperation::Append)
        })
    }

    fn begin_refresh(
        &self,
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
    ) -> Result<u64, McpRegistryJournalError> {
        let operation = McpRegistryJournalOperation::BeginRefresh;
        self.with_lock(operation, || {
            let next = self
                .replay_locked(operation)?
                .into_iter()
                .filter_map(|event| match event {
                    McpRegistryEvent::RefreshRequested {
                        config_revision, ..
                    } => Some(config_revision),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| Self::error(operation, McpRegistryJournalErrorKind::Exhausted))?;
            self.append_locked(
                McpRegistryEvent::RefreshRequested {
                    request_id,
                    trigger,
                    server_scope,
                    config_revision: next,
                },
                operation,
            )?;
            Ok(next)
        })
    }

    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        self.with_lock(McpRegistryJournalOperation::Replay, || {
            self.replay_locked(McpRegistryJournalOperation::Replay)
        })
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
