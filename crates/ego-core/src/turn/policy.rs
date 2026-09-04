//! Immutable generation policy paired with its confidential canonical form.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::{Mode, OriginSet, Ruleset, Source, SourcedRule};
use crate::credentials::SecretMaterial;
use crate::digest::{is_lower_sha256, sha256_hex};
use crate::permit::rule::Rule;
use crate::permit::{CanonPath, HumanApprover, PolicyEngine};

const WIRE_VERSION: u32 = 1;
const DIGEST_DOMAIN: &[u8] = b"ego:generation-policy-capture:v1\0";
const MAX_GENERATION_POLICY_SNAPSHOT_BYTES: usize = 1024 * 1024;

/// The non-secret, domain-separated identity of one canonical policy snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GenerationPolicyDigest(String);

impl TryFrom<String> for GenerationPolicyDigest {
    type Error = GenerationPolicyCaptureError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if is_lower_sha256(&value) {
            Ok(Self(value))
        } else {
            Err(GenerationPolicyCaptureError::InvalidDigest)
        }
    }
}

impl From<GenerationPolicyDigest> for String {
    fn from(value: GenerationPolicyDigest) -> Self {
        value.0
    }
}

/// Confidential canonical policy semantics retained across a process boundary.
pub struct GenerationPolicySnapshot {
    workspace: CanonPath,
    mode: Mode,
    ruleset: Ruleset,
    canonical: SecretMaterial,
    digest: GenerationPolicyDigest,
}

impl GenerationPolicySnapshot {
    pub fn digest(&self) -> GenerationPolicyDigest {
        self.digest.clone()
    }

    /// Copy the canonical bytes into a fresh zeroizing owner for storage.
    pub fn encode_confidential(&self) -> SecretMaterial {
        SecretMaterial::new(self.canonical.expose_secret_bytes().to_vec())
    }

    /// Decode only the canonical v1 representation of a policy snapshot.
    pub fn decode_confidential(
        encoded: &SecretMaterial,
    ) -> Result<Self, GenerationPolicyCaptureError> {
        let encoded = encoded.expose_secret_bytes();
        if encoded.len() > MAX_GENERATION_POLICY_SNAPSHOT_BYTES {
            return Err(GenerationPolicyCaptureError::InvalidSnapshot);
        }
        let PolicyWire {
            version,
            workspace,
            mode,
            allow,
            deny,
        } = serde_json::from_slice(encoded)
            .map_err(|_| GenerationPolicyCaptureError::InvalidSnapshot)?;
        if version != WIRE_VERSION {
            return Err(GenerationPolicyCaptureError::UnsupportedVersion);
        }

        let ruleset = Ruleset {
            allow: decode_rules(allow)?,
            deny: decode_rules(deny)?,
        };
        let snapshot = Self::from_semantics(workspace, mode, ruleset)?;
        if snapshot.canonical.expose_secret_bytes() != encoded {
            return Err(GenerationPolicyCaptureError::NonCanonicalSnapshot);
        }
        Ok(snapshot)
    }

    fn from_semantics(
        workspace: CanonPath,
        mode: Mode,
        ruleset: Ruleset,
    ) -> Result<Self, GenerationPolicyCaptureError> {
        let wire = PolicyWire::from_semantics(&workspace, mode, &ruleset);
        let canonical =
            serde_json::to_vec(&wire).map_err(|_| GenerationPolicyCaptureError::EncodingFailed)?;
        if canonical.len() > MAX_GENERATION_POLICY_SNAPSHOT_BYTES {
            return Err(GenerationPolicyCaptureError::InvalidSnapshot);
        }
        let digest = digest(&canonical)?;
        Ok(Self {
            workspace,
            mode,
            ruleset,
            canonical: SecretMaterial::new(canonical),
            digest,
        })
    }
}

impl fmt::Debug for GenerationPolicySnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenerationPolicySnapshot")
            .field("mode", &self.mode)
            .field("digest", &self.digest)
            .field("policy", &"<redacted>")
            .finish()
    }
}

/// One immutable engine and the canonical semantics from which it was built.
pub struct GenerationPolicyCapture {
    policy: Arc<PolicyEngine>,
    snapshot: Arc<GenerationPolicySnapshot>,
}

impl GenerationPolicyCapture {
    pub fn new(
        workspace: CanonPath,
        mode: Mode,
        ruleset: Ruleset,
        human: Option<Arc<dyn HumanApprover>>,
    ) -> Result<Self, GenerationPolicyCaptureError> {
        Self::restore(
            Arc::new(GenerationPolicySnapshot::from_semantics(
                workspace, mode, ruleset,
            )?),
            human,
        )
    }

    pub fn restore(
        snapshot: Arc<GenerationPolicySnapshot>,
        human: Option<Arc<dyn HumanApprover>>,
    ) -> Result<Self, GenerationPolicyCaptureError> {
        let policy = match human {
            Some(human) => PolicyEngine::with_human(
                snapshot.workspace.clone(),
                snapshot.mode,
                snapshot.ruleset.clone(),
                human,
            ),
            None => PolicyEngine::new(
                snapshot.workspace.clone(),
                snapshot.mode,
                snapshot.ruleset.clone(),
            ),
        };
        Ok(Self {
            policy: Arc::new(policy),
            snapshot,
        })
    }

    pub fn policy(&self) -> Arc<PolicyEngine> {
        Arc::clone(&self.policy)
    }

    pub fn snapshot(&self) -> Arc<GenerationPolicySnapshot> {
        Arc::clone(&self.snapshot)
    }
}

impl fmt::Debug for GenerationPolicyCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenerationPolicyCapture")
            .field("snapshot", &self.snapshot)
            .field("policy", &"<redacted>")
            .finish()
    }
}

/// Confidential storage for one generation policy keyed by continuation id.
pub trait GenerationPolicyCaptureStore: Send + Sync {
    fn store(
        &self,
        id: &str,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), GenerationPolicyCaptureStoreError>;

    fn load(
        &self,
        id: &str,
        expected: &GenerationPolicyDigest,
    ) -> Result<Arc<GenerationPolicySnapshot>, GenerationPolicyCaptureStoreError>;

    fn delete(&self, id: &str) -> Result<(), GenerationPolicyCaptureStoreError>;
}

/// Capture-store operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationPolicyCaptureStoreOperation {
    Store,
    Load,
    Delete,
}

impl fmt::Display for GenerationPolicyCaptureStoreOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Store => "store",
            Self::Load => "load",
            Self::Delete => "delete",
        })
    }
}

/// Closed, non-secret capture-store failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationPolicyCaptureStoreErrorKind {
    Unavailable,
    Missing,
    CapacityExceeded,
    Corrupt,
    DigestMismatch,
}

impl fmt::Display for GenerationPolicyCaptureStoreErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "unavailable",
            Self::Missing => "missing",
            Self::CapacityExceeded => "capacity exceeded",
            Self::Corrupt => "corrupt",
            Self::DigestMismatch => "digest mismatch",
        })
    }
}

/// Redacted failure at the generation-policy capture-store boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationPolicyCaptureStoreError {
    operation: GenerationPolicyCaptureStoreOperation,
    kind: GenerationPolicyCaptureStoreErrorKind,
}

impl GenerationPolicyCaptureStoreError {
    pub fn new(
        operation: GenerationPolicyCaptureStoreOperation,
        kind: GenerationPolicyCaptureStoreErrorKind,
    ) -> Self {
        Self { operation, kind }
    }

    pub fn operation(&self) -> GenerationPolicyCaptureStoreOperation {
        self.operation
    }

    pub fn kind(&self) -> GenerationPolicyCaptureStoreErrorKind {
        self.kind
    }

    pub fn diagnostic(&self) -> &'static str {
        match self.kind {
            GenerationPolicyCaptureStoreErrorKind::Unavailable => {
                "generation policy capture store is unavailable"
            }
            GenerationPolicyCaptureStoreErrorKind::Missing => {
                "generation policy capture is missing"
            }
            GenerationPolicyCaptureStoreErrorKind::CapacityExceeded => {
                "generation policy capture exceeds storage capacity"
            }
            GenerationPolicyCaptureStoreErrorKind::Corrupt => {
                "stored generation policy capture is corrupt"
            }
            GenerationPolicyCaptureStoreErrorKind::DigestMismatch => {
                "stored generation policy capture digest does not match"
            }
        }
    }
}

impl fmt::Display for GenerationPolicyCaptureStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "generation policy capture {operation} operation failed ({kind}): {diagnostic}",
            operation = self.operation,
            kind = self.kind,
            diagnostic = self.diagnostic(),
        )
    }
}

impl std::error::Error for GenerationPolicyCaptureStoreError {}

/// Closed failures that never retain or render confidential policy material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationPolicyCaptureError {
    EncodingFailed,
    InvalidDigest,
    InvalidSnapshot,
    NonCanonicalSnapshot,
    UnsupportedVersion,
}

impl fmt::Display for GenerationPolicyCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EncodingFailed => "generation policy capture encoding failed",
            Self::InvalidDigest => "generation policy capture digest is invalid",
            Self::InvalidSnapshot => "generation policy capture is invalid",
            Self::NonCanonicalSnapshot => "generation policy capture is not canonical",
            Self::UnsupportedVersion => "generation policy capture version is unsupported",
        })
    }
}

impl std::error::Error for GenerationPolicyCaptureError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyWire {
    version: u32,
    workspace: CanonPath,
    mode: Mode,
    allow: Vec<RuleWire>,
    deny: Vec<RuleWire>,
}

impl PolicyWire {
    fn from_semantics(workspace: &CanonPath, mode: Mode, ruleset: &Ruleset) -> Self {
        Self {
            version: WIRE_VERSION,
            workspace: workspace.clone(),
            mode,
            allow: encode_rules(&ruleset.allow),
            deny: encode_rules(&ruleset.deny),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleWire {
    rule: String,
    origins: Vec<Source>,
}

fn encode_rules(rules: &[SourcedRule]) -> Vec<RuleWire> {
    rules
        .iter()
        .map(|entry| RuleWire {
            rule: entry.rule().to_string(),
            origins: entry.origins().iter().collect(),
        })
        .collect()
}

fn decode_rules(wire: Vec<RuleWire>) -> Result<Vec<SourcedRule>, GenerationPolicyCaptureError> {
    let mut decoded: Vec<SourcedRule> = Vec::with_capacity(wire.len());
    for RuleWire { rule, origins } in wire {
        let parsed =
            Rule::load(&rule).map_err(|_| GenerationPolicyCaptureError::InvalidSnapshot)?;
        if parsed.to_string() != rule || decoded.iter().any(|entry| entry.rule() == &parsed) {
            return Err(GenerationPolicyCaptureError::InvalidSnapshot);
        }

        let origin_set = OriginSet::from(origins.clone());
        if origins.is_empty()
            || origin_set.len() != origins.len()
            || origin_set.iter().collect::<Vec<_>>() != origins
        {
            return Err(GenerationPolicyCaptureError::InvalidSnapshot);
        }

        let mut origins = origins.into_iter();
        let first = origins
            .next()
            .ok_or(GenerationPolicyCaptureError::InvalidSnapshot)?;
        let mut sourced = SourcedRule::new(parsed, first);
        for origin in origins {
            sourced.also_carried_by(origin);
        }
        decoded.push(sourced);
    }
    Ok(decoded)
}

fn digest(bytes: &[u8]) -> Result<GenerationPolicyDigest, GenerationPolicyCaptureError> {
    let length = u64::try_from(bytes.len())
        .map_err(|_| GenerationPolicyCaptureError::EncodingFailed)?
        .to_be_bytes();
    let mut framed = SecretMaterial::new(DIGEST_DOMAIN.to_vec());
    framed.extend_from_slice(&length);
    framed.extend_from_slice(bytes);
    Ok(GenerationPolicyDigest(sha256_hex(
        framed.expose_secret_bytes(),
    )))
}
