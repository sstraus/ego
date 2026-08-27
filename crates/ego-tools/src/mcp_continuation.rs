//! Durable JSONL adapter for suspended MCP operations.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use ego_core::event::time::Clock;
use ego_core::suspension::{
    ClaimOutcome, ContinuationError, ContinuationJournal, ContinuationRecord, ContinuationState,
};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

use crate::clock::SystemClock;
use crate::store::{AtomicReplacePublication, OsIo, StoreIo};

const JOURNAL_FILE: &str = "continuations.jsonl";
const LOCK_FILE: &str = "continuations.lock";
const RECORD_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    record: ContinuationRecord,
}

#[derive(Debug, Deserialize)]
struct VersionProbe {
    version: u32,
}

/// Machine-local journal for sealed MCP continuation authority.
pub struct JsonlContinuationJournal {
    path: PathBuf,
    lock_path: PathBuf,
    io: Box<dyn StoreIo>,
    clock: Box<dyn Clock>,
}

impl JsonlContinuationJournal {
    /// Open the production adapter rooted at the supplied machine directory.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ContinuationError> {
        Ok(Self::new(root, Box::new(OsIo), Box::new(SystemClock)))
    }

    /// Build the adapter with explicit I/O and clock ports.
    pub fn new(root: impl AsRef<Path>, io: Box<dyn StoreIo>, clock: Box<dyn Clock>) -> Self {
        let root = root.as_ref();
        Self {
            path: root.join(JOURNAL_FILE),
            lock_path: root.join(LOCK_FILE),
            io,
            clock,
        }
    }

    /// Persist an answer and its sealed continuation before returning its id.
    pub fn accept(
        &self,
        mut record: ContinuationRecord,
        answer: &[u8],
    ) -> Result<String, ContinuationError> {
        let id = record.id().to_owned();
        record.accept_answer(answer.to_vec())?;
        self.with_lock(|| {
            let records = self.load_locked()?;
            if records.contains_key(&id) {
                return Err(ContinuationError::DuplicateAnswer);
            }
            self.append_locked(record)?;
            Ok(id)
        })
    }

    /// Read the latest durable state for one continuation.
    pub fn state(&self, id: &str) -> Result<ContinuationState, ContinuationError> {
        self.with_lock(|| Ok(self.get_locked(id)?.state()))
    }

    /// Atomically consume a continuation at most once.
    pub fn claim(&self, id: &str) -> Result<ClaimOutcome, ContinuationError> {
        self.with_lock(|| {
            let mut record = self.get_locked(id)?;
            let outcome = record.claim(self.clock.now())?;
            if outcome.is_first() {
                self.append_locked(record)?;
            }
            Ok(outcome)
        })
    }

    /// Record that a claimed remote effect has an unknown settlement.
    pub fn mark_crashed_after_claim(&self, id: &str) -> Result<(), ContinuationError> {
        self.with_lock(|| {
            let mut record = self.get_locked(id)?;
            record.settle(ContinuationState::Uncertain)?;
            self.append_locked(record)
        })
    }

    /// Return only a safely replayable, accepted continuation.
    pub fn replay(&self, id: &str) -> Result<Option<ContinuationRecord>, ContinuationError> {
        self.with_lock(|| {
            let record = self.get_locked(id)?;
            Ok((record.state() == ContinuationState::Accepted).then_some(record))
        })
    }

    /// Load an accepted continuation only when every authority fingerprint matches.
    pub fn resume_with_fingerprint(
        &self,
        id: &str,
        admission_generation: &str,
        descriptor_fingerprint: &str,
        prepared_call_fingerprint: &str,
    ) -> Result<ContinuationRecord, ContinuationError> {
        let record = self
            .replay(id)?
            .ok_or(ContinuationError::InvalidTransition)?;
        record.validate_fingerprints(
            admission_generation,
            descriptor_fingerprint,
            prepared_call_fingerprint,
        )?;
        Ok(record)
    }

    fn with_lock<T>(
        &self,
        work: impl FnOnce() -> Result<T, ContinuationError>,
    ) -> Result<T, ContinuationError> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent).map_err(|_| unavailable())?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|_| unavailable())?;
        let mut lock = RwLock::new(file);
        let _guard = lock.write().map_err(|_| unavailable())?;
        work()
    }

    fn get_locked(&self, id: &str) -> Result<ContinuationRecord, ContinuationError> {
        self.load_locked()?
            .remove(id)
            .ok_or(ContinuationError::NotFound)
    }

    fn load_locked(&self) -> Result<BTreeMap<String, ContinuationRecord>, ContinuationError> {
        let Some(bytes) = self.io.read(&self.path).map_err(|_| unavailable())? else {
            return Ok(BTreeMap::new());
        };
        let committed = committed_prefix(&bytes);
        let records = decode_committed(committed)?;
        if committed.len() != bytes.len() {
            match self
                .io
                .replace_atomic_publication(&self.path, committed)
                .map_err(|_| unavailable())?
            {
                AtomicReplacePublication::PublishedDurably => {}
                AtomicReplacePublication::PublishedDurabilityUncertain { .. } => {
                    return Err(ContinuationError::Unavailable(
                        "journal tail repair durability is uncertain".to_owned(),
                    ));
                }
            }
        }
        Ok(records)
    }

    fn append_locked(&self, record: ContinuationRecord) -> Result<(), ContinuationError> {
        let line = serde_json::to_vec(&Snapshot {
            version: RECORD_VERSION,
            record,
        })
        .map_err(|_| corrupt())?;
        self.io
            .append_line(&self.path, &line)
            .map_err(|_| unavailable())?;
        self.io.fsync(&self.path).map_err(|_| {
            ContinuationError::Unavailable("journal durability is uncertain".to_owned())
        })
    }
}

impl ContinuationJournal for JsonlContinuationJournal {
    fn record(&self, record: ContinuationRecord) -> Result<(), ContinuationError> {
        let id = record.id().to_owned();
        self.with_lock(|| {
            if self.load_locked()?.contains_key(&id) {
                return Err(ContinuationError::InvalidTransition);
            }
            self.append_locked(record)
        })
    }

    fn accept_answer(&self, id: &str, answer: &[u8]) -> Result<(), ContinuationError> {
        self.with_lock(|| {
            let mut record = self.get_locked(id)?;
            record.accept_answer(answer.to_vec())?;
            self.append_locked(record)
        })
    }

    fn load(&self, id: &str) -> Result<ContinuationRecord, ContinuationError> {
        self.with_lock(|| self.get_locked(id))
    }

    fn claim(
        &self,
        id: &str,
        now: ego_core::event::time::Timestamp,
    ) -> Result<ClaimOutcome, ContinuationError> {
        self.with_lock(|| {
            let mut record = self.get_locked(id)?;
            let outcome = record.claim(now)?;
            if outcome.is_first() {
                self.append_locked(record)?;
            }
            Ok(outcome)
        })
    }

    fn settle(&self, id: &str, state: ContinuationState) -> Result<(), ContinuationError> {
        self.with_lock(|| {
            let mut record = self.get_locked(id)?;
            record.settle(state)?;
            self.append_locked(record)
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
) -> Result<BTreeMap<String, ContinuationRecord>, ContinuationError> {
    let mut records = BTreeMap::new();
    if committed.is_empty() {
        return Ok(records);
    }
    for line in committed[..committed.len() - 1].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            return Err(corrupt());
        }
        let probe: VersionProbe = serde_json::from_slice(line).map_err(|_| corrupt())?;
        if probe.version != RECORD_VERSION {
            return Err(ContinuationError::Corrupt(
                "unsupported journal version".to_owned(),
            ));
        }
        let snapshot: Snapshot = serde_json::from_slice(line).map_err(|_| corrupt())?;
        records.insert(snapshot.record.id().to_owned(), snapshot.record);
    }
    Ok(records)
}

fn corrupt() -> ContinuationError {
    ContinuationError::Corrupt("invalid committed record".to_owned())
}

fn unavailable() -> ContinuationError {
    ContinuationError::Unavailable("journal I/O failed".to_owned())
}
