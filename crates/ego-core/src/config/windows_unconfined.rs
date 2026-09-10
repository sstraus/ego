//! Remembered acknowledgement of the Windows unconfined-execution contract.
//!
//! This is a host-risk fact, not project trust. The filesystem transaction and
//! the human question live in `ego-cli`; only the versioned lookup rule lives
//! here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The host-risk contract an acknowledgement applies to.
pub const UNCONFINED_CONTRACT_REVISION: u32 = 1;

const UNCONFINED_LEDGER_VERSION: u8 = 1;

/// One acknowledgement of the Windows unconfined-execution contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnconfinedRecord {
    pub revision: u32,
}

/// Acknowledgements keyed by canonical workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnconfinedLedger {
    v: u8,
    workspaces: BTreeMap<String, UnconfinedRecord>,
}

impl Default for UnconfinedLedger {
    fn default() -> Self {
        Self {
            v: UNCONFINED_LEDGER_VERSION,
            workspaces: BTreeMap::new(),
        }
    }
}

impl UnconfinedLedger {
    /// Whether `root` acknowledged this exact contract revision.
    pub fn acknowledged(&self, root: &str, revision: u32) -> bool {
        self.workspaces
            .get(root)
            .is_some_and(|record| record.revision == revision)
    }

    /// Replace the acknowledgement recorded for `root`.
    pub fn acknowledge(&mut self, root: impl Into<String>, revision: u32) {
        self.workspaces
            .insert(root.into(), UnconfinedRecord { revision });
    }
}
