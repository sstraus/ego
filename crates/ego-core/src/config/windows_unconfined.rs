//! Remembered acknowledgement of the Windows unconfined-execution contract.
//!
//! This is a host-risk fact, not project trust. The owner learns once per
//! contract revision that Windows does not confine execution; the limitation
//! is a property of the platform, not of a workspace, so one answer covers
//! every workspace of the same user. The filesystem transaction and the human
//! question live in `ego-cli`; only the versioned lookup rule lives here.

use serde::{Deserialize, Serialize};

/// The host-risk contract an acknowledgement applies to.
pub const UNCONFINED_CONTRACT_REVISION: u32 = 1;

const UNCONFINED_LEDGER_VERSION: u8 = 1;

/// The user's acknowledgement of the Windows unconfined-execution contract.
///
/// Unknown fields are refused, so a workspace trust document can never be
/// read as this host acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnconfinedLedger {
    v: u8,
    revision: Option<u32>,
}

impl Default for UnconfinedLedger {
    fn default() -> Self {
        Self {
            v: UNCONFINED_LEDGER_VERSION,
            revision: None,
        }
    }
}

impl UnconfinedLedger {
    /// Whether the user acknowledged this exact contract revision.
    pub fn acknowledged(&self, revision: u32) -> bool {
        self.revision == Some(revision)
    }

    /// Replace the user's acknowledgement with `revision`.
    pub fn acknowledge(&mut self, revision: u32) {
        self.revision = Some(revision);
    }
}
