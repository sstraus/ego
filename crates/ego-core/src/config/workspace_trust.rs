//! What a workspace offers to be trusted with, and what a remembered answer
//! is still an answer to.
//!
//! See `docs/decisions/2026-09-05-a-trust-answer-is-remembered-per-root.md`.
//! Split the way everything else in this crate is split: the *decisions* —
//! what an inventory is, when two of them are the same, and whether a stored
//! answer still applies — are pure and live here. Which paths get probed,
//! reading them, and the locked document under `$EGO_HOME` are effects and
//! live in the binary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::source::Trust;
use crate::digest::sha256_hex;

/// How much of one file the inventory digest covers.
///
/// The probe runs *before* the trust question, over paths a hostile checkout
/// controls, so an unbounded read here would be a startup hang waiting to be
/// committed. A mebibyte is far past every configuration, instruction and
/// skill file that exists; the entry also carries the full length, so growing
/// a file past the bound is still a change the digest sees.
pub const TRUST_DIGEST_MAX_BYTES: usize = 1024 * 1024;

/// The version of the persisted ledger document.
const TRUST_LEDGER_VERSION: u8 = 1;

/// One discoverable file whose admission the trust answer governs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedContent {
    path: String,
    bytes: u64,
    digest: String,
}

impl TrustedContent {
    /// One regular file at `path`, `bytes` long, whose first
    /// [`TRUST_DIGEST_MAX_BYTES`] the caller has read into `prefix`.
    ///
    /// `path` is workspace-relative, so a trusted answer survives a `-C` that
    /// spells the same root differently and does not survive a move — which
    /// is the root key's job, not this one's.
    pub fn regular(path: impl Into<String>, bytes: u64, prefix: &[u8]) -> Self {
        Self {
            path: path.into(),
            bytes,
            digest: sha256_hex(&prefix[..prefix.len().min(TRUST_DIGEST_MAX_BYTES)]),
        }
    }

    /// The workspace-relative path this entry describes.
    pub fn path(&self) -> &str {
        &self.path
    }

    fn line(&self) -> String {
        format!("{}\0{}\0{}\n", self.path, self.bytes, self.digest)
    }
}

/// Everything a workspace currently offers to be trusted with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustInventory {
    entries: Vec<TrustedContent>,
}

impl TrustInventory {
    /// Order and de-duplicate what the probe found.
    ///
    /// Sorted here rather than trusted from the caller: directory order is a
    /// filesystem's business, and two launches that read the same workspace
    /// must produce the same digest.
    pub fn new(entries: impl IntoIterator<Item = TrustedContent>) -> Self {
        let mut entries: Vec<_> = entries.into_iter().collect();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        entries.dedup_by(|left, right| left.path == right.path);
        Self { entries }
    }

    /// Whether this workspace offers nothing a trust answer would change.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// What each entry describes, in canonical order.
    pub fn entries(&self) -> &[TrustedContent] {
        &self.entries
    }

    /// The one hex value a stored answer is remembered against.
    pub fn digest(&self) -> String {
        let canonical: String = self.entries.iter().map(TrustedContent::line).collect();
        sha256_hex(canonical.as_bytes())
    }
}

/// One remembered answer and the inventory it answered about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustRecord {
    pub decision: Trust,
    pub digest: String,
}

/// Every remembered answer, keyed by canonical workspace root.
///
/// A `BTreeMap` so the document a person may open is in a stable order rather
/// than a hash order that churns on every write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustLedger {
    v: u8,
    workspaces: BTreeMap<String, TrustRecord>,
}

impl Default for TrustLedger {
    fn default() -> Self {
        Self {
            v: TRUST_LEDGER_VERSION,
            workspaces: BTreeMap::new(),
        }
    }
}

impl TrustLedger {
    /// The answer already given about `root`, if it answered *this* inventory.
    ///
    /// `None` covers both "never asked" and "asked about content that has
    /// since changed", because the caller does the same thing with either:
    /// ask. A refusal is a `Some(Untrusted)` and is honoured, so declining
    /// once is not re-asked on every launch.
    pub fn remembered(&self, root: &str, digest: &str) -> Option<Trust> {
        self.workspaces
            .get(root)
            .filter(|record| record.digest == digest)
            .map(|record| record.decision)
    }

    /// Replace whatever was remembered about `root`.
    pub fn remember(
        &mut self,
        root: impl Into<String>,
        digest: impl Into<String>,
        decision: Trust,
    ) {
        self.workspaces.insert(
            root.into(),
            TrustRecord {
                decision,
                digest: digest.into(),
            },
        );
    }
}
