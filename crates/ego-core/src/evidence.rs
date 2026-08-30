//! Durable, descriptor-only evidence admitted before external effects.
//!
//! The policy and narrow persistence port live in core. Filesystem adapters
//! implement the port only while holding the session writer lease.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::budget::MeteringIdentity;
use crate::config::ModelRef;
use crate::cost::PricingRevision;
use crate::digest::sha256_hex;
use crate::event::domain::GenerationRole;
use crate::event::id::SessionId;
use crate::generator::attempt::AttemptRef;
use crate::generator::capabilities::ModelCapabilities;
use crate::mcp::{McpAdmissionFingerprint, McpSubject};
use crate::permit::ExecObservation;
use crate::public::InstructionDescriptor;

const EVIDENCE_SCHEMA_VERSION: u32 = 1;

/// Stable identity for one host invocation of a rooted run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunInvocationId(Uuid);

impl RunInvocationId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub const fn uuid(self) -> Uuid {
        self.0
    }
}

impl Default for RunInvocationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunInvocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Canonical lowercase SHA-256 identity of an evidence payload.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidenceDigest(String);

impl EvidenceDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EvidenceDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Safe provenance of the resolved configuration, never its raw contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigProvenance {
    Resolved {
        #[serde(skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
}

impl ConfigProvenance {
    pub fn resolved(profile: Option<String>) -> Self {
        Self::Resolved { profile }
    }
}

impl Default for ConfigProvenance {
    fn default() -> Self {
        Self::resolved(None)
    }
}

/// Exact provider selection admitted for one generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationTarget {
    provider: String,
    canonical_model: ModelRef,
    wire_model: String,
}

impl GenerationTarget {
    pub fn new(
        provider: impl Into<String>,
        canonical_model: ModelRef,
        wire_model: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            canonical_model,
            wire_model: wire_model.into(),
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn canonical_model(&self) -> &ModelRef {
        &self.canonical_model
    }

    pub fn wire_model(&self) -> &str {
        &self.wire_model
    }
}

/// Pricing authority recorded explicitly, including an explicit unpriced fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PricingEvidence {
    Priced {
        source: String,
        revision: PricingRevision,
        metering: MeteringIdentity,
    },
    Unpriced {
        source: String,
        reason: String,
    },
}

/// Safe run-level facts bound before the first effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunContextEvidence {
    version: u32,
    session: SessionId,
    invocation: RunInvocationId,
    run_id: String,
    config: ConfigProvenance,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    instruction_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    instructions: Vec<InstructionDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mcp_subjects: Vec<McpSubject>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mcp_admissions: Vec<McpAdmissionFingerprint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution: Option<ExecObservation>,
}

impl RunContextEvidence {
    pub fn new(
        session: SessionId,
        invocation: RunInvocationId,
        run_id: impl Into<String>,
        config: ConfigProvenance,
    ) -> Self {
        Self {
            version: EVIDENCE_SCHEMA_VERSION,
            session,
            invocation,
            run_id: run_id.into(),
            config,
            context_digest: None,
            instruction_paths: Vec::new(),
            instructions: Vec::new(),
            mcp_subjects: Vec::new(),
            mcp_admissions: Vec::new(),
            execution: None,
        }
    }

    pub fn with_context_digest(mut self, digest: impl Into<String>) -> Self {
        self.context_digest = Some(digest.into());
        self
    }

    pub fn with_instruction_paths<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.instruction_paths = paths.into_iter().map(Into::into).collect();
        self.instruction_paths.sort();
        self.instruction_paths.dedup();
        self
    }

    pub fn with_instructions(mut self, mut instructions: Vec<InstructionDescriptor>) -> Self {
        instructions.sort_by_key(|value| serde_json::to_string(value).unwrap_or_default());
        instructions.dedup();
        self.instructions = instructions;
        self
    }

    pub fn with_mcp_projection(
        mut self,
        mut subjects: Vec<McpSubject>,
        mut admissions: Vec<McpAdmissionFingerprint>,
    ) -> Self {
        subjects.sort_by_key(|value| serde_json::to_string(value).unwrap_or_default());
        admissions.sort_by_key(|value| serde_json::to_string(value).unwrap_or_default());
        subjects.dedup();
        admissions.dedup();
        self.mcp_subjects = subjects;
        self.mcp_admissions = admissions;
        self
    }

    pub fn with_execution(mut self, execution: ExecObservation) -> Self {
        self.execution = Some(execution);
        self
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub const fn invocation(&self) -> RunInvocationId {
        self.invocation
    }

    pub fn instructions(&self) -> &[InstructionDescriptor] {
        &self.instructions
    }
}

/// Exact generation facts bound before a provider call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationAdmissionEvidence {
    version: u32,
    session: SessionId,
    invocation: RunInvocationId,
    attempt: AttemptRef,
    role: GenerationRole,
    target: GenerationTarget,
    capabilities: ModelCapabilities,
    pricing: PricingEvidence,
}

impl GenerationAdmissionEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: SessionId,
        invocation: RunInvocationId,
        attempt: AttemptRef,
        role: GenerationRole,
        target: GenerationTarget,
        capabilities: ModelCapabilities,
        pricing: PricingEvidence,
    ) -> Self {
        Self {
            version: EVIDENCE_SCHEMA_VERSION,
            session,
            invocation,
            attempt,
            role,
            target,
            capabilities,
            pricing,
        }
    }

    pub fn pricing(&self) -> &PricingEvidence {
        &self.pricing
    }

    pub fn target(&self) -> &GenerationTarget {
        &self.target
    }

    pub fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub const fn invocation(&self) -> RunInvocationId {
        self.invocation
    }

    pub const fn attempt(&self) -> AttemptRef {
        self.attempt
    }

    pub const fn role(&self) -> GenerationRole {
        self.role
    }
}

/// Idempotency key owned by the evidence consumer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceKey {
    RunContext {
        session: SessionId,
        invocation: RunInvocationId,
    },
    GenerationAdmission {
        session: SessionId,
        invocation: RunInvocationId,
        attempt: AttemptRef,
    },
}

impl EvidenceKey {
    pub const fn session(&self) -> SessionId {
        match self {
            Self::RunContext { session, .. } | Self::GenerationAdmission { session, .. } => {
                *session
            }
        }
    }
}

/// One durable evidence payload in the session stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceRecord {
    RunContext(RunContextEvidence),
    GenerationAdmission(GenerationAdmissionEvidence),
}

impl EvidenceRecord {
    pub fn key(&self) -> EvidenceKey {
        match self {
            Self::RunContext(value) => EvidenceKey::RunContext {
                session: value.session,
                invocation: value.invocation,
            },
            Self::GenerationAdmission(value) => EvidenceKey::GenerationAdmission {
                session: value.session,
                invocation: value.invocation,
                attempt: value.attempt,
            },
        }
    }

    pub fn digest(&self) -> EvidenceDigest {
        canonical_digest(self).expect("evidence records always serialize")
    }

    pub const fn semantic_event_sequence(&self) -> Option<crate::event::id::Seq> {
        None
    }
}

/// Result of publishing one evidence key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceWrite {
    Published,
    AlreadyRecorded,
    PublishedWithProjectionWarning { diagnostic: String },
}

impl EvidenceWrite {
    pub const fn is_already_recorded(&self) -> bool {
        matches!(self, Self::AlreadyRecorded)
    }
}

/// Evidence publication failure with uncertainty kept distinct from refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceError {
    IdempotencyConflict {
        key: EvidenceKey,
        existing: EvidenceDigest,
        proposed: EvidenceDigest,
    },
    RunContextRequired {
        key: EvidenceKey,
    },
    DurabilityUncertain {
        key: EvidenceKey,
        diagnostic: String,
    },
    Store {
        key: EvidenceKey,
        diagnostic: String,
    },
}

impl EvidenceError {
    pub fn idempotency_conflict(
        key: EvidenceKey,
        existing: EvidenceDigest,
        proposed: EvidenceDigest,
    ) -> Self {
        Self::IdempotencyConflict {
            key,
            existing,
            proposed,
        }
    }

    pub fn key(&self) -> &EvidenceKey {
        match self {
            Self::IdempotencyConflict { key, .. }
            | Self::RunContextRequired { key }
            | Self::DurabilityUncertain { key, .. }
            | Self::Store { key, .. } => key,
        }
    }
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdempotencyConflict { key, .. } => {
                write!(f, "evidence idempotency conflict for {key:?}")
            }
            Self::RunContextRequired { key } => {
                write!(f, "run context evidence is required before {key:?}")
            }
            Self::DurabilityUncertain { diagnostic, .. } => {
                write!(f, "evidence durability is uncertain: {diagnostic}")
            }
            Self::Store { diagnostic, .. } => f.write_str(diagnostic),
        }
    }
}

impl std::error::Error for EvidenceError {}

/// Consumer-owned persistence port. Only a lease-held adapter should implement it.
pub trait EvidenceStore: Send + Sync {
    fn write(
        &self,
        key: EvidenceKey,
        record: EvidenceRecord,
    ) -> Result<EvidenceWrite, EvidenceError>;
}

/// Stateless admission policy shared by every host.
#[derive(Debug, Default)]
pub struct EvidenceAdmissionService;

impl EvidenceAdmissionService {
    pub const fn new() -> Self {
        Self
    }

    pub fn canonical_digest<T: Serialize>(
        &self,
        value: &T,
    ) -> Result<EvidenceDigest, CanonicalEvidenceError> {
        canonical_digest(value)
    }

    pub fn admit_run_context(
        &self,
        store: &dyn EvidenceStore,
        evidence: RunContextEvidence,
    ) -> Result<EvidenceWrite, EvidenceError> {
        let record = EvidenceRecord::RunContext(evidence);
        store.write(record.key(), record)
    }

    pub fn admit_generation(
        &self,
        store: &dyn EvidenceStore,
        evidence: GenerationAdmissionEvidence,
    ) -> Result<EvidenceWrite, EvidenceError> {
        let record = EvidenceRecord::GenerationAdmission(evidence);
        store.write(record.key(), record)
    }
}

#[derive(Debug)]
pub struct CanonicalEvidenceError(serde_json::Error);

impl fmt::Display for CanonicalEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot encode canonical evidence: {}", self.0)
    }
}

impl std::error::Error for CanonicalEvidenceError {}

fn canonical_digest<T: Serialize>(value: &T) -> Result<EvidenceDigest, CanonicalEvidenceError> {
    let payload = serde_json::to_vec(value).map_err(CanonicalEvidenceError)?;
    let mut framed = Vec::with_capacity(payload.len() + 24);
    framed.extend_from_slice(b"ego-evidence");
    framed.extend_from_slice(&EVIDENCE_SCHEMA_VERSION.to_be_bytes());
    framed.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    framed.extend_from_slice(&payload);
    Ok(EvidenceDigest(sha256_hex(&framed)))
}

/// Evidence projection held by the rebuildable session manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEvidence {
    run_contexts: Vec<RunContextEvidence>,
    generations: Vec<GenerationAdmissionEvidence>,
}

impl ManifestEvidence {
    pub fn from_records(records: &[EvidenceRecord]) -> Result<Self, ManifestEvidenceError> {
        let mut projected = Self::default();
        for record in records {
            match record {
                EvidenceRecord::RunContext(value) => projected.run_contexts.push(value.clone()),
                EvidenceRecord::GenerationAdmission(value) => {
                    projected.generations.push(value.clone())
                }
            }
        }
        Ok(projected)
    }

    /// Refuse a projection whose authoritative prefix included an unknown item.
    pub fn from_records_with_unknown(
        _records: &[EvidenceRecord],
    ) -> Result<Self, ManifestEvidenceError> {
        Err(ManifestEvidenceError::UnknownRecord)
    }

    pub fn rebuild<S: EvidenceLogSource + ?Sized>(
        source: &S,
        session: &SessionId,
    ) -> Result<Self, ManifestEvidenceError> {
        Self::from_records(&source.evidence_records(session)?)
    }

    pub fn run_contexts(&self) -> &[RunContextEvidence] {
        &self.run_contexts
    }

    pub fn generations(&self) -> &[GenerationAdmissionEvidence] {
        &self.generations
    }
}

/// Read-side port used to rebuild a derived manifest projection.
pub trait EvidenceLogSource: Send + Sync {
    fn evidence_records(
        &self,
        session: &SessionId,
    ) -> Result<Vec<EvidenceRecord>, ManifestEvidenceError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestEvidenceError {
    UnknownRecord,
    Store(String),
}

impl fmt::Display for ManifestEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRecord => f.write_str("unknown evidence record in valid log prefix"),
            Self::Store(diagnostic) => f.write_str(diagnostic),
        }
    }
}

impl std::error::Error for ManifestEvidenceError {}
