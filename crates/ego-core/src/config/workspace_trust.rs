//! What a workspace offers to be trusted with, and which executable changes
//! require another answer.
//!
//! See `docs/architecture.md`.
//! Split the way everything else in this crate is split: the *decisions* —
//! what an inventory is, when two of them are the same, and whether a stored
//! answer still applies — are pure and live here. Which paths get probed,
//! reading them, and the locked document under `$EGO_HOME` are effects and
//! live in the binary.

use std::collections::{BTreeMap, BTreeSet};

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
const TRUST_LEDGER_VERSION: u8 = 2;

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

/// Redacted executable authority presented by a workspace. Environment values
/// are never stored here, so they cannot leak through a later trust question.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableSurface {
    pub declarations: BTreeMap<String, ExecutableDeclaration>,
    pub programs: BTreeMap<String, ProgramEvidence>,
    /// Exact external instruction targets and SHA-256 bytes approved for disclosure.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub external_instructions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableDeclaration {
    pub config: String,
    pub kind: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramEvidence {
    pub bytes: u64,
    pub digest: String,
    pub executable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}

impl ExecutableSurface {
    pub fn fingerprint(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("surface fields serialize");
        sha256_hex(&bytes)
    }

    /// Only changed authority, with both sides labeled. The caller prepends
    /// this to its trust question; unmodified declarations never appear.
    pub fn changes_from(&self, old: &Self) -> String {
        let mut lines = Vec::new();
        let mut before = old.declarations.clone();
        let mut after = self.declarations.clone();
        for (old_key, old_entry) in &old.declarations {
            if let Some(new_key) = after
                .iter()
                .find_map(|(key, entry)| old_entry.same_when_reindexed(entry).then(|| key.clone()))
            {
                before.remove(old_key);
                after.remove(&new_key);
            }
        }
        let declaration_keys: BTreeSet<_> = before.keys().chain(after.keys()).collect();
        for key in declaration_keys {
            let label = match (before.get(key), after.get(key)) {
                (None, Some(_)) => "added",
                (Some(_), None) => "removed",
                _ => "changed",
            };
            lines.push(format!("{key:?}: {label}"));
            if let Some(entry) = before.get(key) {
                lines.push(format!("  before: {}", entry.describe()));
            }
            if let Some(entry) = after.get(key) {
                lines.push(format!("  after: {}", entry.describe()));
            }
        }
        if before.is_empty() && after.is_empty() && old.declarations != self.declarations {
            for (old_key, old_entry) in &old.declarations {
                if old_entry.kind != "hook" {
                    continue;
                }
                if let Some((new_key, new_entry)) = self
                    .declarations
                    .iter()
                    .find(|(_, entry)| old_entry.same_when_reindexed(entry))
                    && old_key != new_key
                {
                    lines.push(format!(
                        "hook order changed: {old_key:?} -> {new_key:?}\n  before: {}\n  after: {}",
                        old_entry.describe(),
                        new_entry.describe()
                    ));
                }
            }
        }
        let program_keys: BTreeSet<_> = old.programs.keys().chain(self.programs.keys()).collect();
        for key in program_keys {
            if old.programs.get(key) == self.programs.get(key) {
                continue;
            }
            lines.push(format!("program {key:?}: changed"));
            if let Some(before) = old.programs.get(key) {
                lines.push(format!(
                    "  before: size={} executable={} target={:?}",
                    before.bytes, before.executable, before.link_target
                ));
            }
            if let Some(after) = self.programs.get(key) {
                lines.push(format!(
                    "  after: size={} executable={} target={:?}",
                    after.bytes, after.executable, after.link_target
                ));
            }
        }
        for (path, digest) in &self.external_instructions {
            if old.external_instructions.get(path) != Some(digest) {
                lines.push(format!("external instruction {path:?}: SHA-256 {digest}"));
            }
        }
        for path in old.external_instructions.keys() {
            if !self.external_instructions.contains_key(path) {
                lines.push(format!("external instruction {path:?}: removed"));
            }
        }
        lines.join("\n")
    }
}

impl ExecutableDeclaration {
    fn same_when_reindexed(&self, other: &Self) -> bool {
        if self == other {
            return true;
        }
        self.kind == "hook"
            && other.kind == "hook"
            && self.config == other.config
            && self.name.split('#').next() == other.name.split('#').next()
            && self.command == other.command
            && self.args == other.args
            && self.env_keys == other.env_keys
            && self.details == other.details
    }

    fn describe(&self) -> String {
        let command_line = std::iter::once(&self.command)
            .chain(self.args.iter())
            .map(|part| format!("{part:?}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{} {} in {} command line={} args={:?} env keys={:?} details={:?}",
            self.kind, self.name, self.config, command_line, self.args, self.env_keys, self.details
        )
    }
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

/// One remembered answer and its redacted executable surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustRecord {
    pub decision: Trust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_surface: Option<ExecutableSurface>,
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
    /// The answer already given about `root`, if it answered this surface.
    ///
    /// Old digest-only records retain their decision until first migration.
    /// A refusal is `Some(Untrusted)` and is honored like an approval.
    pub fn remembered(&self, root: &str, surface: &ExecutableSurface) -> Option<Trust> {
        self.workspaces
            .get(root)
            .filter(|record| match &record.executable_surface {
                Some(stored) => stored == surface,
                None => record.digest.is_some() && surface.external_instructions.is_empty(),
            })
            .map(|record| record.decision)
    }

    pub fn prior_surface(&self, root: &str) -> Option<&ExecutableSurface> {
        self.workspaces.get(root)?.executable_surface.as_ref()
    }

    pub fn is_legacy(&self, root: &str) -> bool {
        self.workspaces
            .get(root)
            .is_some_and(|record| record.executable_surface.is_none())
    }

    /// Replace whatever was remembered about `root`.
    pub fn remember(
        &mut self,
        root: impl Into<String>,
        surface: ExecutableSurface,
        decision: Trust,
    ) {
        self.v = TRUST_LEDGER_VERSION;
        self.workspaces.insert(
            root.into(),
            TrustRecord {
                decision,
                digest: None,
                executable_surface: Some(surface),
            },
        );
    }
}
