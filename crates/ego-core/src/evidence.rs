//! Durable, descriptor-only evidence admitted before external effects.
//!
//! The policy and narrow persistence port live in core. Filesystem adapters
//! implement the port only while holding the session writer lease.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::budget::MeteringIdentity;
use crate::ceiling::Ceilings;
use crate::config::{ActiveProfileProvenance, ModelRef};
use crate::cost::PricingRevision;
use crate::digest::sha256_hex;
use crate::event::domain::GenerationRole;
use crate::event::id::SessionId;
use crate::generator::attempt::AttemptRef;
use crate::generator::capabilities::ModelCapabilities;
use crate::mcp::McpOperationKind;
use crate::permit::{Confinement, PolicyDigest};
use crate::public::InstructionDescriptor;
use crate::run::{RunId, RunOrigin};

pub const EVIDENCE_SCHEMA_VERSION: u32 = 3;

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
#[serde(try_from = "String", into = "String")]
pub struct EvidenceDigest(String);

impl EvidenceDigest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(sha256_hex(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EvidenceDigest {
    type Error = InvalidEvidenceDigest;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if crate::digest::is_lower_sha256(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidEvidenceDigest)
        }
    }
}

impl From<EvidenceDigest> for String {
    fn from(value: EvidenceDigest) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidEvidenceDigest;

impl fmt::Display for InvalidEvidenceDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("evidence digest must be lowercase SHA-256")
    }
}

impl std::error::Error for InvalidEvidenceDigest {}

impl fmt::Display for EvidenceDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validated version of the ego binary that admitted one invocation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EgoVersion(String);

impl EgoVersion {
    pub fn current() -> Self {
        env!("CARGO_PKG_VERSION")
            .to_owned()
            .try_into()
            .expect("Cargo package versions are valid ego versions")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EgoVersion {
    type Error = InvalidEgoVersion;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = (1..=64).contains(&value.len())
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_digit())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'));
        valid.then_some(Self(value)).ok_or(InvalidEgoVersion)
    }
}

impl From<EgoVersion> for String {
    fn from(value: EgoVersion) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidEgoVersion;

impl fmt::Display for InvalidEgoVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ego version must be a bounded ASCII package version")
    }
}

impl std::error::Error for InvalidEgoVersion {}

/// Safe provenance of the resolved configuration, never its raw contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigProvenance {
    Resolved {
        #[serde(skip_serializing_if = "Option::is_none")]
        profile: Option<ActiveProfileProvenance>,
    },
}

impl ConfigProvenance {
    pub fn resolved(profile: Option<ActiveProfileProvenance>) -> Self {
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

/// A validated identity retained for an admitted MCP server.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct McpServerIdentity(String);

impl TryFrom<String> for McpServerIdentity {
    type Error = InvalidMcpEvidence;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = (1..=64).contains(&value.len())
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && value.bytes().skip(1).all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            });
        valid.then_some(Self(value)).ok_or(InvalidMcpEvidence)
    }
}

impl From<McpServerIdentity> for String {
    fn from(value: McpServerIdentity) -> Self {
        value.0
    }
}

impl McpServerIdentity {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The only transport facts durable MCP evidence can express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

/// One admitted MCP operation fingerprint without free-form operation text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpAdmissionDescriptor {
    operation: McpOperationKind,
    generation_digest: EvidenceDigest,
    schema_digest: EvidenceDigest,
}

impl McpAdmissionDescriptor {
    pub fn new(
        operation: McpOperationKind,
        generation_digest: EvidenceDigest,
        schema_digest: EvidenceDigest,
    ) -> Self {
        Self {
            operation,
            generation_digest,
            schema_digest,
        }
    }

    pub const fn operation(&self) -> McpOperationKind {
        self.operation
    }

    pub fn generation_digest(&self) -> &EvidenceDigest {
        &self.generation_digest
    }

    pub fn schema_digest(&self) -> &EvidenceDigest {
        &self.schema_digest
    }
}

/// A closed MCP server context. Raw transport configuration is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpContextDescriptor {
    server: McpServerIdentity,
    transport: McpTransportKind,
    #[serde(deserialize_with = "deserialize_mcp_admissions")]
    admissions: Vec<McpAdmissionDescriptor>,
    provenance_digest: EvidenceDigest,
}

impl McpContextDescriptor {
    pub fn new(
        server: impl Into<String>,
        transport: McpTransportKind,
        mut admissions: Vec<McpAdmissionDescriptor>,
        provenance_digest: EvidenceDigest,
    ) -> Result<Self, InvalidMcpEvidence> {
        normalize_mcp_admissions(&mut admissions);
        Ok(Self {
            server: server.into().try_into()?,
            transport,
            admissions,
            provenance_digest,
        })
    }

    pub fn server(&self) -> &McpServerIdentity {
        &self.server
    }

    pub const fn transport(&self) -> McpTransportKind {
        self.transport
    }

    pub fn admissions(&self) -> &[McpAdmissionDescriptor] {
        &self.admissions
    }

    pub fn provenance_digest(&self) -> &EvidenceDigest {
        &self.provenance_digest
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMcpEvidence;

impl fmt::Display for InvalidMcpEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MCP evidence is not a closed admitted descriptor")
    }
}

impl std::error::Error for InvalidMcpEvidence {}

/// Pre-effect execution authority. Backend observations remain domain events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAdmissionDescriptor {
    confinement: Confinement,
    process_network: bool,
    workspace_digest: EvidenceDigest,
    capabilities_digest: EvidenceDigest,
}

impl ExecutionAdmissionDescriptor {
    pub fn new(
        confinement: Confinement,
        process_network: bool,
        workspace_digest: EvidenceDigest,
        capabilities_digest: EvidenceDigest,
    ) -> Self {
        Self {
            confinement,
            process_network,
            workspace_digest,
            capabilities_digest,
        }
    }

    pub const fn confinement(&self) -> Confinement {
        self.confinement
    }

    pub const fn process_network(&self) -> bool {
        self.process_network
    }

    pub fn workspace_digest(&self) -> &EvidenceDigest {
        &self.workspace_digest
    }

    pub fn capabilities_digest(&self) -> &EvidenceDigest {
        &self.capabilities_digest
    }
}

/// Safe run-level facts bound before the first effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunContextEvidence {
    version: u32,
    session: SessionId,
    invocation: RunInvocationId,
    run_id: RunId,
    origin: RunOrigin,
    ego_version: EgoVersion,
    config: ConfigProvenance,
    config_digest: EvidenceDigest,
    policy_digest: PolicyDigest,
    ceilings: Ceilings,
    #[serde(deserialize_with = "deserialize_instructions")]
    instructions: Vec<InstructionDescriptor>,
    #[serde(deserialize_with = "deserialize_mcp_contexts")]
    mcp: Vec<McpContextDescriptor>,
    execution: ExecutionAdmissionDescriptor,
}

impl RunContextEvidence {
    pub fn new(
        session: SessionId,
        invocation: RunInvocationId,
        run_id: RunId,
        config: ConfigProvenance,
    ) -> Self {
        let config_digest =
            canonical_digest(&config).expect("configuration provenance always serializes");
        let policy_digest =
            PolicyDigest::try_from(sha256_hex(b"")).expect("sha256_hex is a valid policy digest");
        Self::complete(
            session,
            invocation,
            run_id,
            EgoVersion::current(),
            config,
            config_digest,
            policy_digest,
            Ceilings::default(),
            Vec::new(),
            Vec::new(),
            ExecutionAdmissionDescriptor::new(
                Confinement::Workspace,
                false,
                EvidenceDigest::of_bytes(b""),
                EvidenceDigest::of_bytes(b""),
            ),
        )
    }

    pub fn with_instructions(mut self, mut instructions: Vec<InstructionDescriptor>) -> Self {
        normalize_instructions(&mut instructions);
        self.instructions = instructions;
        self
    }

    pub fn with_origin(mut self, origin: RunOrigin) -> Self {
        self.origin = origin;
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn complete(
        session: SessionId,
        invocation: RunInvocationId,
        run_id: RunId,
        ego_version: EgoVersion,
        config: ConfigProvenance,
        config_digest: EvidenceDigest,
        policy_digest: PolicyDigest,
        ceilings: Ceilings,
        mut instructions: Vec<InstructionDescriptor>,
        mut mcp: Vec<McpContextDescriptor>,
        execution: ExecutionAdmissionDescriptor,
    ) -> Self {
        normalize_instructions(&mut instructions);
        normalize_mcp_contexts(&mut mcp);
        Self {
            version: EVIDENCE_SCHEMA_VERSION,
            session,
            invocation,
            run_id,
            origin: RunOrigin::TopLevel,
            ego_version,
            config,
            config_digest,
            policy_digest,
            ceilings,
            instructions,
            mcp,
            execution,
        }
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub const fn invocation(&self) -> RunInvocationId {
        self.invocation
    }

    pub const fn version(&self) -> u32 {
        self.version
    }

    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn origin(&self) -> &RunOrigin {
        &self.origin
    }

    pub fn config(&self) -> &ConfigProvenance {
        &self.config
    }

    pub fn ego_version(&self) -> &EgoVersion {
        &self.ego_version
    }

    pub fn config_digest(&self) -> &EvidenceDigest {
        &self.config_digest
    }

    pub fn policy_digest(&self) -> &PolicyDigest {
        &self.policy_digest
    }

    pub fn ceilings(&self) -> &Ceilings {
        &self.ceilings
    }

    pub fn instructions(&self) -> &[InstructionDescriptor] {
        &self.instructions
    }

    pub fn mcp(&self) -> &[McpContextDescriptor] {
        &self.mcp
    }

    pub fn execution(&self) -> &ExecutionAdmissionDescriptor {
        &self.execution
    }
}

fn normalize_instructions(instructions: &mut Vec<InstructionDescriptor>) {
    instructions.sort_by_key(|value| serde_json::to_string(value).unwrap_or_default());
    instructions.dedup();
}

fn deserialize_instructions<'de, D>(deserializer: D) -> Result<Vec<InstructionDescriptor>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut instructions = Vec::<InstructionDescriptor>::deserialize(deserializer)?;
    normalize_instructions(&mut instructions);
    Ok(instructions)
}

fn operation_ordinal(operation: McpOperationKind) -> u8 {
    match operation {
        McpOperationKind::CallTool => 0,
        McpOperationKind::ReadResource => 1,
        McpOperationKind::GetPrompt => 2,
    }
}

fn normalize_mcp_admissions(admissions: &mut Vec<McpAdmissionDescriptor>) {
    admissions.sort_by(|left, right| {
        operation_ordinal(left.operation)
            .cmp(&operation_ordinal(right.operation))
            .then_with(|| left.generation_digest.cmp(&right.generation_digest))
            .then_with(|| left.schema_digest.cmp(&right.schema_digest))
    });
    admissions.dedup();
}

fn deserialize_mcp_admissions<'de, D>(
    deserializer: D,
) -> Result<Vec<McpAdmissionDescriptor>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut admissions = Vec::<McpAdmissionDescriptor>::deserialize(deserializer)?;
    normalize_mcp_admissions(&mut admissions);
    Ok(admissions)
}

fn normalize_mcp_contexts(descriptors: &mut Vec<McpContextDescriptor>) {
    for descriptor in descriptors.iter_mut() {
        normalize_mcp_admissions(&mut descriptor.admissions);
    }
    descriptors.sort_by_key(|value| serde_json::to_string(value).unwrap_or_default());
    descriptors.dedup();
}

fn deserialize_mcp_contexts<'de, D>(deserializer: D) -> Result<Vec<McpContextDescriptor>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut descriptors = Vec::<McpContextDescriptor>::deserialize(deserializer)?;
    normalize_mcp_contexts(&mut descriptors);
    Ok(descriptors)
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

    pub const fn version(&self) -> u32 {
        self.version
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

    pub const fn version(&self) -> u32 {
        match self {
            Self::RunContext(value) => value.version,
            Self::GenerationAdmission(value) => value.version,
        }
    }

    /// Return the exact current-schema refusal before this record reaches a
    /// writer, durable reader, or manifest projection.
    pub fn current_schema_error(&self) -> Option<EvidenceError> {
        if self.version() != EVIDENCE_SCHEMA_VERSION {
            return Some(EvidenceError::UnsupportedVersion {
                key: self.key(),
                found: self.version(),
                required: EVIDENCE_SCHEMA_VERSION,
            });
        }
        match self {
            Self::GenerationAdmission(value)
                if value.capabilities.schema_version != ModelCapabilities::SCHEMA_VERSION =>
            {
                Some(EvidenceError::UnsupportedVersion {
                    key: self.key(),
                    found: value.capabilities.schema_version,
                    required: ModelCapabilities::SCHEMA_VERSION,
                })
            }
            Self::RunContext(_) | Self::GenerationAdmission(_) => None,
        }
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
    UnsupportedVersion {
        key: EvidenceKey,
        found: u32,
        required: u32,
    },
    IdempotencyConflict {
        key: EvidenceKey,
        existing: EvidenceDigest,
        proposed: EvidenceDigest,
    },
    AttemptOwnershipConflict {
        session: SessionId,
        attempt: AttemptRef,
        existing_invocation: RunInvocationId,
        requested_invocation: RunInvocationId,
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
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion {
                found, required, ..
            } => write!(
                f,
                "unsupported evidence version {found}; version {required} is required"
            ),
            Self::IdempotencyConflict { key, .. } => {
                write!(f, "evidence idempotency conflict for {key:?}")
            }
            Self::AttemptOwnershipConflict {
                session,
                attempt,
                existing_invocation,
                requested_invocation,
            } => write!(
                f,
                "attempt {attempt:?} in session {session} is owned by invocation \
                 {existing_invocation}, not {requested_invocation}"
            ),
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
        if let Some(error) = record.current_schema_error() {
            return Err(error);
        }
        store.write(record.key(), record)
    }

    pub fn admit_generation(
        &self,
        store: &dyn EvidenceStore,
        evidence: GenerationAdmissionEvidence,
    ) -> Result<EvidenceWrite, EvidenceError> {
        let record = EvidenceRecord::GenerationAdmission(evidence);
        if let Some(error) = record.current_schema_error() {
            return Err(error);
        }
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
        let mut attempt_owners = std::collections::HashMap::new();
        for record in records {
            if let Some(error) = record.current_schema_error() {
                return Err(match error {
                    EvidenceError::UnsupportedVersion {
                        found, required, ..
                    } => ManifestEvidenceError::UnsupportedVersion { found, required },
                    _ => unreachable!("schema validation returns only schema errors"),
                });
            }
            match record {
                EvidenceRecord::RunContext(value) => projected.run_contexts.push(value.clone()),
                EvidenceRecord::GenerationAdmission(value) => {
                    if let Some(existing_invocation) =
                        attempt_owners.insert(value.attempt(), value.invocation())
                        && existing_invocation != value.invocation()
                    {
                        return Err(ManifestEvidenceError::AttemptOwnershipConflict {
                            session: value.session(),
                            attempt: value.attempt(),
                            existing_invocation,
                            requested_invocation: value.invocation(),
                        });
                    }
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
    UnsupportedVersion {
        found: u32,
        required: u32,
    },
    AttemptOwnershipConflict {
        session: SessionId,
        attempt: AttemptRef,
        existing_invocation: RunInvocationId,
        requested_invocation: RunInvocationId,
    },
    Store(String),
}

impl fmt::Display for ManifestEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRecord => f.write_str("unknown evidence record in valid log prefix"),
            Self::UnsupportedVersion { found, required } => write!(
                f,
                "unsupported evidence version {found}; version {required} is required"
            ),
            Self::AttemptOwnershipConflict {
                session,
                attempt,
                existing_invocation,
                requested_invocation,
            } => write!(
                f,
                "attempt {attempt:?} in session {session} is owned by invocation \
                 {existing_invocation}, not {requested_invocation}"
            ),
            Self::Store(diagnostic) => f.write_str(diagnostic),
        }
    }
}

impl std::error::Error for ManifestEvidenceError {}
