//! Serialized MCP reconciliation and immutable admitted snapshots.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, Instant};

use ego_core::config::{McpConfigProvenance, McpServerConfig, McpServerName};
use ego_core::event::time::Clock;
use ego_core::generator::types::ToolDescriptor as GeneratorToolDescriptor;
use ego_core::mcp::{
    CacheScope, ContextEntry, ContextError, ContextSensitivity, ContextSnapshot, ContextSource,
    ForeignQuestionPort, McpAdmissionFingerprint, McpOperationKind, McpRuntimeBinding,
    McpServerInstructions, McpSubject,
};
use ego_core::permit::{PreparedCall, Tool, ToolError};
use ego_core::suspension::{
    ContinuationJournal, ContinuationKind, ContinuationSubject, McpCapturePin, ResumeFingerprints,
    ResumeFloor, ResumeKeySource, ResumeSealer,
};
use ego_core::turn::{
    AdmittedToolSource, CapturedToolCatalogue, GenerationPolicyCaptureStore,
    GenerationPolicySnapshot, McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError,
    RegisteredTool, ToolCatalogueError, ToolEffectPorts,
};
use rmcp::model::{Prompt, Resource};
use sha2::{Digest, Sha256};

use crate::dynamic::{McpCapturedTool, McpOperation, McpToolDefinition};
use crate::hook::McpHookPort;
use crate::operation::{McpCaptureResolver, McpForeignQuestionPort};
use crate::{
    McpCollapsedBootstrap, McpCollapsedCatalogue, McpContinuationOwner, McpDuplexEffectsFactory,
    McpError, McpFailure, McpFuture, McpHttpEffects, McpOperationFactory,
    McpRuntimeCandidateAdmission, McpServerAdmission,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryTrigger {
    Add,
    Remove,
    Login,
    Reload,
    ConfigChanged,
    AuthorizationSucceeded,
    TtlExpired,
    ToolsListChanged,
    Rescan,
    Recovery,
}

impl RegistryTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Login => "login",
            Self::Reload => "reload",
            Self::ConfigChanged => "config_changed",
            Self::AuthorizationSucceeded => "authorization_succeeded",
            Self::TtlExpired => "ttl_expired",
            Self::ToolsListChanged => "tools_list_changed",
            Self::Rescan => "rescan",
            Self::Recovery => "recovery",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryScope {
    All,
    Server(McpServerName),
}

impl RegistryScope {
    fn journal_value(&self) -> Option<String> {
        match self {
            Self::All => None,
            Self::Server(name) => Some(name.to_string()),
        }
    }

    fn includes(&self, name: &McpServerName) -> bool {
        matches!(self, Self::All) || matches!(self, Self::Server(selected) if selected == name)
    }
}

#[derive(Clone)]
pub struct McpRegistryCandidate {
    provenance: McpConfigProvenance,
    materialization: CandidateMaterialization,
}

#[derive(Clone)]
enum CandidateMaterialization {
    Ready(Arc<McpServerAdmission>),
    Rejected {
        name: McpServerName,
        config: McpServerConfig,
        failure: McpFailure,
    },
}

impl McpRegistryCandidate {
    pub fn ready(admission: Arc<McpServerAdmission>, provenance: McpConfigProvenance) -> Self {
        Self {
            provenance,
            materialization: CandidateMaterialization::Ready(admission),
        }
    }

    pub fn rejected(
        name: McpServerName,
        config: McpServerConfig,
        provenance: McpConfigProvenance,
        failure: McpFailure,
    ) -> Self {
        Self {
            provenance,
            materialization: CandidateMaterialization::Rejected {
                name,
                config,
                failure,
            },
        }
    }

    pub fn name(&self) -> &McpServerName {
        match &self.materialization {
            CandidateMaterialization::Ready(admission) => admission.name(),
            CandidateMaterialization::Rejected { name, .. } => name,
        }
    }

    pub fn config(&self) -> &McpServerConfig {
        match &self.materialization {
            CandidateMaterialization::Ready(admission) => admission.config(),
            CandidateMaterialization::Rejected { config, .. } => config,
        }
    }

    pub fn provenance(&self) -> &McpConfigProvenance {
        &self.provenance
    }
}

#[derive(Clone)]
pub struct McpRegistryRefresh {
    candidates: Vec<McpRegistryCandidate>,
    trigger: RegistryTrigger,
    scope: RegistryScope,
}

impl McpRegistryRefresh {
    pub fn new(
        mut candidates: Vec<McpRegistryCandidate>,
        trigger: RegistryTrigger,
        scope: RegistryScope,
    ) -> Result<Self, McpRegistryError> {
        if candidates
            .iter()
            .any(|candidate| candidate.config().validate().is_err())
        {
            return Err(McpRegistryError::InvalidCandidate(
                "invalid MCP registry candidate configuration".to_owned(),
            ));
        }
        candidates.sort_by(|left, right| left.name().cmp(right.name()));
        if candidates
            .windows(2)
            .any(|pair| pair[0].name() == pair[1].name())
        {
            return Err(McpRegistryError::InvalidCandidate(
                "duplicate MCP registry candidate name".to_owned(),
            ));
        }
        Ok(Self {
            candidates,
            trigger,
            scope,
        })
    }

    pub fn candidates(&self) -> &[McpRegistryCandidate] {
        &self.candidates
    }

    pub fn trigger(&self) -> RegistryTrigger {
        self.trigger
    }

    pub fn scope(&self) -> &RegistryScope {
        &self.scope
    }
}

#[derive(Clone)]
pub struct McpCandidateRequest {
    config_revision: u64,
    admission: Arc<McpServerAdmission>,
    provenance: McpConfigProvenance,
}

impl McpCandidateRequest {
    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    pub fn admission(&self) -> Arc<McpServerAdmission> {
        Arc::clone(&self.admission)
    }

    pub fn name(&self) -> &McpServerName {
        self.admission.name()
    }

    pub fn config(&self) -> &McpServerConfig {
        self.admission.config()
    }

    pub fn provenance(&self) -> &McpConfigProvenance {
        &self.provenance
    }
}

pub trait McpCandidateAdmission: Send + Sync {
    fn admit(
        &self,
        request: McpCandidateRequest,
    ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpCandidateError {
    CredentialRequired,
    Invalid {
        cause: String,
    },
    AuthorizationRequired {
        issuer: String,
        protected_resource: String,
        scopes: Vec<String>,
    },
}

impl McpCandidateError {
    pub fn failure(&self) -> McpFailure {
        match self {
            Self::CredentialRequired => McpFailure::Credential,
            Self::Invalid { .. } => McpFailure::InvalidValue,
            Self::AuthorizationRequired { .. } => McpFailure::Authorization,
        }
    }
}

struct CandidateFailure {
    failure: McpFailure,
    authorization: Option<CandidateAuthorizationFailure>,
    detail: Option<String>,
}

struct CandidateAuthorizationFailure {
    issuer: String,
    protected_resource: String,
    scopes: Vec<String>,
}

impl CandidateFailure {
    fn materialization(failure: McpFailure) -> Self {
        Self {
            failure,
            authorization: None,
            detail: None,
        }
    }

    fn admission(error: McpCandidateError) -> Self {
        match error {
            McpCandidateError::CredentialRequired => Self::materialization(McpFailure::Credential),
            McpCandidateError::Invalid { cause } => Self {
                detail: Some(cause),
                ..Self::materialization(McpFailure::InvalidValue)
            },
            McpCandidateError::AuthorizationRequired {
                issuer,
                protected_resource,
                scopes,
            } => Self {
                failure: McpFailure::Authorization,
                authorization: Some(CandidateAuthorizationFailure {
                    issuer,
                    protected_resource,
                    scopes,
                }),
                detail: None,
            },
        }
    }

    fn sanitized_cause(&self) -> &'static str {
        failure_message(self.failure)
    }

    /// Public diagnostics contain only the fixed failure classification.
    fn diagnostic_message(&self) -> String {
        self.sanitized_cause().to_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpCandidateIdentity(Arc<str>);

impl McpCandidateIdentity {
    pub fn validated(value: impl Into<String>) -> Self {
        Self(Arc::from(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpCapabilities {
    tools: bool,
    resources: bool,
    prompts: bool,
}

impl McpCapabilities {
    pub fn new(tools: bool, resources: bool, prompts: bool) -> Self {
        Self {
            tools,
            resources,
            prompts,
        }
    }

    pub fn tools() -> Self {
        Self::new(true, false, false)
    }

    pub fn has_tools(&self) -> bool {
        self.tools
    }

    pub fn has_resources(&self) -> bool {
        self.resources
    }

    pub fn has_prompts(&self) -> bool {
        self.prompts
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpCacheMetadata {
    source_revision: Option<Arc<str>>,
    expires_at: Option<Instant>,
}

impl McpCacheMetadata {
    pub fn new(source_revision: Option<String>, expires_at: Option<Instant>) -> Self {
        Self {
            source_revision: source_revision.map(Arc::from),
            expires_at,
        }
    }

    pub fn source_revision(&self) -> Option<&str> {
        self.source_revision.as_deref()
    }

    pub fn expires_at(&self) -> Option<Instant> {
        self.expires_at
    }
}

#[derive(Clone)]
pub struct McpCandidateInventory {
    capabilities: McpCapabilities,
    instructions: Option<String>,
    tools: Vec<GeneratorToolDescriptor>,
    resources: Vec<Resource>,
    prompts: Vec<Prompt>,
    cache: McpCacheMetadata,
    collapsed: Option<McpCollapsedCatalogue>,
}

impl McpCandidateInventory {
    /// `instructions` is what the server said about itself at `initialize`;
    /// `None` is a server that said nothing, which behaves exactly as before.
    pub fn new(
        capabilities: McpCapabilities,
        instructions: Option<String>,
        tools: Vec<GeneratorToolDescriptor>,
        resources: Vec<Resource>,
        prompts: Vec<Prompt>,
        cache: McpCacheMetadata,
        collapsed: Option<McpCollapsedCatalogue>,
    ) -> Self {
        Self {
            capabilities,
            instructions,
            tools,
            resources,
            prompts,
            cache,
            collapsed,
        }
    }
}

#[derive(Clone)]
pub struct McpAdmittedCandidate {
    admission: Arc<McpServerAdmission>,
    identity: McpCandidateIdentity,
    capabilities: McpCapabilities,
    instructions: Option<String>,
    descriptors: Arc<[GeneratorToolDescriptor]>,
    tools: Arc<[McpToolDefinition]>,
    resources: Arc<[Resource]>,
    prompts: Arc<[Prompt]>,
    cache: McpCacheMetadata,
    collapsed_bootstrap: Option<McpCollapsedBootstrap>,
    factory: Arc<dyn McpOperationFactory>,
}

impl McpAdmittedCandidate {
    pub fn new(
        admission: Arc<McpServerAdmission>,
        identity: McpCandidateIdentity,
        inventory: McpCandidateInventory,
        factory: Arc<dyn McpOperationFactory>,
    ) -> Result<Self, McpRegistryError> {
        if identity.as_str().is_empty() || admission.name().as_str() != identity.as_str() {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate identity does not match its configured name".to_owned(),
            ));
        }
        admission
            .config()
            .validate()
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        let McpCandidateInventory {
            capabilities,
            instructions,
            tools,
            resources,
            prompts,
            cache,
            collapsed,
        } = inventory;
        validate_candidate_contents(&capabilities, &tools, &resources, &prompts)?;
        let collapsed_bootstrap = McpCollapsedBootstrap::detect(&tools)
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        match (&collapsed_bootstrap, &collapsed) {
            (Some(_), Some(_)) => {}
            (Some(_), None) => {}
            (None, None) => {}
            _ => {
                return Err(McpRegistryError::InvalidCandidate(
                    "collapsed MCP catalogue has no compatible bootstrap".to_owned(),
                ));
            }
        }
        let descriptors: Arc<[GeneratorToolDescriptor]> = tools.into();
        let definitions = compile_candidate_tool_definitions(
            &descriptors,
            collapsed_bootstrap.as_ref(),
            collapsed.as_ref(),
        )?;
        let mut names = std::collections::BTreeSet::new();
        for definition in &definitions {
            if !names.insert(definition.name().to_owned()) {
                return Err(McpRegistryError::InvalidCandidate(format!(
                    "duplicate MCP tool {}",
                    definition.name()
                )));
            }
        }
        Ok(Self {
            admission,
            identity,
            capabilities,
            instructions,
            descriptors,
            tools: definitions.into(),
            resources: resources.into(),
            prompts: prompts.into(),
            cache,
            collapsed_bootstrap,
            factory,
        })
    }
}

fn compile_candidate_tool_definitions(
    descriptors: &[GeneratorToolDescriptor],
    collapsed_bootstrap: Option<&McpCollapsedBootstrap>,
    collapsed: Option<&McpCollapsedCatalogue>,
) -> Result<Vec<McpToolDefinition>, McpRegistryError> {
    let mut definitions = Vec::new();
    for descriptor in descriptors {
        let definition = match collapsed_bootstrap {
            Some(bootstrap) if descriptor.name == bootstrap.dispatch_name() => {
                collapsed.map(|catalogue| {
                    McpToolDefinition::collapsed(descriptor.clone(), catalogue.clone())
                })
            }
            _ => Some(McpToolDefinition::direct(descriptor.clone())),
        };
        if let Some(definition) = definition {
            definitions.push(
                definition
                    .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?,
            );
        }
    }
    Ok(definitions)
}

#[derive(Debug)]
pub enum McpRegistryError {
    Journal(McpRegistryJournalError),
    Candidate { server: String, cause: String },
    InvalidCandidate(String),
    PublicationUnavailable,
}

impl fmt::Display for McpRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(error) => error.fmt(f),
            Self::Candidate { server, cause } => {
                write!(f, "MCP candidate {server} was rejected: {cause}")
            }
            Self::InvalidCandidate(cause) => write!(f, "invalid MCP candidate: {cause}"),
            Self::PublicationUnavailable => f.write_str("MCP registry publication is unavailable"),
        }
    }
}

impl std::error::Error for McpRegistryError {}

impl From<McpRegistryJournalError> for McpRegistryError {
    fn from(error: McpRegistryJournalError) -> Self {
        Self::Journal(error)
    }
}

struct AdmittedServerSlice {
    admission: Arc<McpServerAdmission>,
    identity: McpCandidateIdentity,
    provenance: McpConfigProvenance,
    capabilities: McpCapabilities,
    instructions: Option<String>,
    descriptors: Arc<[GeneratorToolDescriptor]>,
    tools: Arc<[McpToolDefinition]>,
    resources: Arc<[Resource]>,
    prompts: Arc<[Prompt]>,
    cache: McpCacheMetadata,
    collapsed_bootstrap: Option<McpCollapsedBootstrap>,
    factory: Arc<dyn McpOperationFactory>,
}

impl AdmittedServerSlice {
    fn from_candidate(candidate: McpAdmittedCandidate, provenance: McpConfigProvenance) -> Self {
        Self {
            admission: candidate.admission,
            identity: candidate.identity,
            instructions: candidate.instructions,
            provenance,
            capabilities: candidate.capabilities,
            descriptors: candidate.descriptors,
            tools: candidate.tools,
            resources: candidate.resources,
            prompts: candidate.prompts,
            cache: candidate.cache,
            collapsed_bootstrap: candidate.collapsed_bootstrap,
            factory: candidate.factory,
        }
    }

    fn with_collapsed_catalogue(
        &self,
        catalogue: McpCollapsedCatalogue,
    ) -> Result<Self, McpRegistryError> {
        let bootstrap = self.collapsed_bootstrap.as_ref().ok_or_else(|| {
            McpRegistryError::InvalidCandidate(
                "tool schema targets a non-collapsed MCP server".to_owned(),
            )
        })?;
        let tools = compile_candidate_tool_definitions(
            &self.descriptors,
            Some(bootstrap),
            Some(&catalogue),
        )?;
        Ok(Self {
            admission: Arc::clone(&self.admission),
            identity: self.identity.clone(),
            provenance: self.provenance.clone(),
            capabilities: self.capabilities.clone(),
            instructions: self.instructions.clone(),
            descriptors: Arc::clone(&self.descriptors),
            tools: tools.into(),
            resources: Arc::clone(&self.resources),
            prompts: Arc::clone(&self.prompts),
            cache: self.cache.clone(),
            collapsed_bootstrap: self.collapsed_bootstrap,
            factory: Arc::clone(&self.factory),
        })
    }
}

#[derive(Clone)]
pub struct McpCapturedServer {
    admission: Arc<McpServerAdmission>,
    identity: McpCandidateIdentity,
    provenance: McpConfigProvenance,
    capabilities: McpCapabilities,
    instructions: Option<String>,
    cache: McpCacheMetadata,
    collapsed_bootstrap: Option<McpCollapsedBootstrap>,
    factory: Arc<dyn McpOperationFactory>,
}

impl McpCapturedServer {
    pub fn identity(&self) -> &McpCandidateIdentity {
        &self.identity
    }

    pub fn provenance(&self) -> &McpConfigProvenance {
        &self.provenance
    }

    pub fn capabilities(&self) -> &McpCapabilities {
        &self.capabilities
    }

    pub fn admission(&self) -> &McpServerAdmission {
        &self.admission
    }

    pub fn cache(&self) -> &McpCacheMetadata {
        &self.cache
    }

    pub fn collapsed_bootstrap(&self) -> Option<&McpCollapsedBootstrap> {
        self.collapsed_bootstrap.as_ref()
    }

    pub fn operation_factory(&self) -> Arc<dyn McpOperationFactory> {
        Arc::clone(&self.factory)
    }
}

impl From<McpCapturedServer> for Arc<McpServerAdmission> {
    fn from(server: McpCapturedServer) -> Self {
        server.admission
    }
}

pub(crate) struct McpToolSchemaCommand {
    server_admission: Arc<McpServerAdmission>,
    generation: String,
    admission_fingerprint: McpAdmissionFingerprint,
    requested_name: String,
    structured_content: serde_json::Value,
}

impl McpToolSchemaCommand {
    pub(crate) fn new(
        server: impl Into<Arc<McpServerAdmission>>,
        generation: String,
        admission_fingerprint: McpAdmissionFingerprint,
        requested_name: String,
        structured_content: serde_json::Value,
    ) -> Self {
        Self {
            server_admission: server.into(),
            generation,
            admission_fingerprint,
            requested_name,
            structured_content,
        }
    }
}

pub(crate) trait McpToolSchemaAdmission: Send + Sync {
    fn reconcile(&self, command: McpToolSchemaCommand) -> McpFuture<'static, Result<(), McpError>>;
}

struct ControllerToolSchemaAdmission {
    controller: Weak<McpRegistryController>,
}

impl McpToolSchemaAdmission for ControllerToolSchemaAdmission {
    fn reconcile(&self, command: McpToolSchemaCommand) -> McpFuture<'static, Result<(), McpError>> {
        let controller = self.controller.clone();
        Box::pin(async move {
            let controller = controller.upgrade().ok_or_else(|| {
                McpError::new(
                    McpFailure::Authority,
                    "collapsed MCP registry controller is unavailable",
                )
            })?;
            controller
                .reconcile_tool_schema(command)
                .await
                .map(|_| ())
                .map_err(|error| McpError::new(McpFailure::Authority, error.to_string()))
        })
    }
}

#[derive(Clone)]
pub struct McpCapturedContextOperation {
    entry: ContextEntry,
    subject: McpSubject,
    operation: McpOperationKind,
    admission: McpAdmissionFingerprint,
    server_admission: Arc<McpServerAdmission>,
    factory: Arc<dyn McpOperationFactory>,
}

type PreparedCapturedOperation = (
    Arc<McpServerAdmission>,
    Arc<dyn McpOperationFactory>,
    PreparedCall,
);

pub(crate) type PreparedResumeOperation = (
    Arc<McpServerAdmission>,
    Arc<dyn McpOperationFactory>,
    PreparedCall,
    ResumeFingerprints,
);

impl McpCapturedContextOperation {
    pub fn entry(&self) -> &ContextEntry {
        &self.entry
    }

    pub fn subject(&self) -> &McpSubject {
        &self.subject
    }

    pub fn operation(&self) -> McpOperationKind {
        self.operation
    }

    pub fn admission(&self) -> &McpAdmissionFingerprint {
        &self.admission
    }

    pub fn server_admission(&self) -> Arc<McpServerAdmission> {
        Arc::clone(&self.server_admission)
    }

    pub fn operation_factory(&self) -> Arc<dyn McpOperationFactory> {
        Arc::clone(&self.factory)
    }

    fn matches_resume(
        &self,
        subject: &ContinuationSubject,
        fingerprints: &ResumeFingerprints,
    ) -> bool {
        self.subject.server() == subject.server()
            && self.operation.method() == subject.operation()
            && self.admission.capture() == fingerprints.capture()
            && self.admission.operation() == fingerprints.tool()
            && self.admission.schema() == fingerprints.schema()
            && matches!(
                (subject.kind(), self.operation),
                (ContinuationKind::Resource, McpOperationKind::ReadResource)
                    | (ContinuationKind::Prompt, McpOperationKind::GetPrompt)
            )
    }

    fn prepare_captured(
        &self,
        parameters: serde_json::Value,
    ) -> Result<PreparedCapturedOperation, McpError> {
        let operation = McpOperation::captured(
            self.subject.clone(),
            self.operation,
            self.admission.clone(),
            parameters,
            Vec::new(),
        );
        let call = self
            .factory
            .prepare(Arc::clone(&self.server_admission), operation)
            .map_err(|error| {
                McpError::new(
                    McpFailure::Authority,
                    format!("continued MCP context could not be reconstructed: {error}"),
                )
            })?;
        Ok((
            Arc::clone(&self.server_admission),
            Arc::clone(&self.factory),
            call,
        ))
    }
}

pub struct McpCapturedContextSource {
    snapshot: ContextSnapshot,
    operations: Arc<[McpCapturedContextOperation]>,
}

impl McpCapturedContextSource {
    fn new(operations: Vec<McpCapturedContextOperation>) -> Result<Self, McpRegistryError> {
        let snapshot = ContextSnapshot::new(
            operations
                .iter()
                .map(|operation| operation.entry.clone())
                .collect(),
        )
        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        Ok(Self {
            snapshot,
            operations: operations.into(),
        })
    }

    pub fn operations(&self) -> &[McpCapturedContextOperation] {
        &self.operations
    }
}

impl ContextSource for McpCapturedContextSource {
    fn capture(&self) -> Result<ContextSnapshot, ContextError> {
        Ok(self.snapshot.clone())
    }
}

#[derive(Clone)]
pub struct McpRegistryCapture {
    inner: Arc<McpRegistryCaptureInner>,
}

struct McpRegistryCaptureInner {
    generation: Arc<str>,
    stable_fingerprint: Arc<str>,
    tools: Arc<[Arc<dyn Tool>]>,
    captured_tools: Arc<[Arc<McpCapturedTool>]>,
    contexts: Arc<McpCapturedContextSource>,
    servers: Arc<[McpCapturedServer]>,
    /// One entry per captured server that described itself, bounded, in the
    /// ascending server-name order `servers` is already canonicalised into.
    instructions: Arc<[McpServerInstructions]>,
}

impl McpRegistryCapture {
    pub fn generation(&self) -> &str {
        &self.inner.generation
    }

    pub fn stable_fingerprint(&self) -> &str {
        &self.inner.stable_fingerprint
    }

    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.inner.tools
    }

    pub fn servers(&self) -> &[McpCapturedServer] {
        &self.inner.servers
    }

    /// Prepare one hook call from this immutable registry generation.
    pub fn prepare_hook_call(
        &self,
        server: &str,
        tool: &str,
        parameters: serde_json::Value,
    ) -> Result<PreparedCall, ToolError> {
        if !self
            .inner
            .servers
            .iter()
            .any(|candidate| candidate.identity().as_str() == server)
        {
            return Err(ToolError::InvalidParams(
                "hook MCP server is absent from the captured registry".to_owned(),
            ));
        }
        let captured = self
            .inner
            .captured_tools
            .iter()
            .find(|candidate| candidate.descriptor().name == tool)
            .ok_or_else(|| {
                ToolError::InvalidParams(
                    "hook MCP tool is absent from the captured registry".to_owned(),
                )
            })?;
        let (_, _, call) = captured.prepare_captured(parameters)?;
        let matches_server = call.mcp_envelope().is_some_and(|envelope| {
            envelope.operation() == McpOperationKind::CallTool
                && envelope.subject().server() == server
                && envelope.subject().operation_name() == tool
        });
        if !matches_server {
            return Err(ToolError::InvalidParams(
                "hook MCP target does not match the captured operation".to_owned(),
            ));
        }
        Ok(call)
    }

    pub(crate) fn prepare_resume(
        &self,
        subject: &ContinuationSubject,
        fingerprints: &ResumeFingerprints,
        original_parameters: &[u8],
    ) -> Result<PreparedResumeOperation, McpError> {
        let parameters = serde_json::from_slice(original_parameters).map_err(|error| {
            McpError::new(
                McpFailure::Authority,
                format!("stored MCP parameters are invalid: {error}"),
            )
        })?;
        let (admission, factory, call) = match subject.kind() {
            ContinuationKind::Tool => {
                let captured = self
                    .inner
                    .captured_tools
                    .iter()
                    .find(|tool| {
                        tool.matches_subject_parts(
                            subject.server(),
                            fingerprints.capture(),
                            fingerprints.tool(),
                            fingerprints.schema(),
                        )
                    })
                    .ok_or_else(|| {
                        McpError::new(
                            McpFailure::Authority,
                            "continued MCP tool is absent from the current registry capture",
                        )
                    })?;
                captured.prepare_captured(parameters).map_err(|error| {
                    McpError::new(
                        McpFailure::Authority,
                        format!("continued MCP tool could not be reconstructed: {error}"),
                    )
                })?
            }
            ContinuationKind::Resource | ContinuationKind::Prompt => {
                let captured = self
                    .inner
                    .contexts
                    .operations()
                    .iter()
                    .find(|operation| operation.matches_resume(subject, fingerprints))
                    .ok_or_else(|| {
                        McpError::new(
                            McpFailure::Authority,
                            "continued MCP context operation is absent from the current registry capture",
                        )
                    })?;
                captured.prepare_captured(parameters)?
            }
        };
        let envelope = call.mcp_envelope().ok_or_else(|| {
            McpError::new(
                McpFailure::Authority,
                "reconstructed MCP continuation has no protocol envelope",
            )
        })?;
        let current = ResumeFingerprints::new(
            envelope.admission().capture(),
            envelope.admission().operation(),
            envelope.admission().schema(),
            envelope.transport_fingerprint(),
        )
        .map_err(|error| McpError::new(McpFailure::Authority, error.to_string()))?;
        Ok((admission, factory, call, current))
    }
}

impl McpCapturePin for McpRegistryCapture {
    fn stable_fingerprint(&self) -> &str {
        McpRegistryCapture::stable_fingerprint(self)
    }

    fn instructions(&self) -> &[McpServerInstructions] {
        &self.inner.instructions
    }

    fn server_names(&self) -> Vec<String> {
        self.inner
            .servers
            .iter()
            .map(|server| server.identity.as_str().to_owned())
            .collect()
    }
}

/// Bound and order what the captured servers said about themselves.
///
/// The order is the servers' own names ascending, which is what
/// `captured_servers` already carries because it is built from a `BTreeMap`
/// keyed by name. Restated here rather than assumed, because the composition
/// order is a documented contract and an incidental one is not a contract.
fn captured_instructions(servers: &[McpCapturedServer]) -> Vec<McpServerInstructions> {
    let mut instructions: Vec<_> = servers
        .iter()
        .filter_map(|server| {
            let text = server.instructions.as_deref()?;
            let text = text.trim();
            (!text.is_empty()).then(|| McpServerInstructions::new(server.identity.as_str(), text))
        })
        .collect();
    instructions.sort_by(|left, right| left.server().cmp(right.server()));
    instructions
}

struct AdmittedRegistrySnapshot {
    number: u64,
    generation: Arc<str>,
    config_revision: u64,
    servers: BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
    tools: Arc<[RegisteredTool]>,
    capture: McpRegistryCapture,
}

impl AdmittedRegistrySnapshot {
    fn empty() -> Self {
        let contexts = Arc::new(
            McpCapturedContextSource::new(Vec::new())
                .expect("the empty MCP context capture is valid"),
        );
        Self {
            number: 0,
            generation: Arc::from("generation-0"),
            config_revision: 0,
            servers: BTreeMap::new(),
            tools: Arc::from(Vec::<RegisteredTool>::new()),
            capture: McpRegistryCapture {
                inner: Arc::new(McpRegistryCaptureInner {
                    generation: Arc::from("generation-0"),
                    stable_fingerprint: Arc::from(
                        stable_capture_fingerprint(&BTreeMap::new())
                            .expect("the empty MCP capture has a stable fingerprint"),
                    ),
                    tools: Arc::from(Vec::<Arc<dyn Tool>>::new()),
                    captured_tools: Arc::from(Vec::<Arc<McpCapturedTool>>::new()),
                    contexts,
                    servers: Arc::from(Vec::<McpCapturedServer>::new()),
                    instructions: Arc::from(Vec::<McpServerInstructions>::new()),
                }),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpServerState {
    Admitted,
    Degraded,
    AuthorizationRequired,
    Absent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpCacheFreshness {
    Uncached,
    Fresh,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpAuthorizationStatus {
    issuer: String,
    protected_resource: String,
    scopes: Arc<[String]>,
    request_correlation: Arc<str>,
}

impl McpAuthorizationStatus {
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn protected_resource(&self) -> &str {
        &self.protected_resource
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn request_correlation(&self) -> &str {
        &self.request_correlation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerDiagnostic {
    failure: McpFailure,
    message: Arc<str>,
}

impl McpServerDiagnostic {
    pub fn failure(&self) -> McpFailure {
        self.failure
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerStatus {
    name: McpServerName,
    state: McpServerState,
    last_good_generation: Option<Arc<str>>,
    capabilities: Option<McpCapabilities>,
    cache_source_revision: Option<Arc<str>>,
    cache_expires_at: Option<Instant>,
    cache_freshness: McpCacheFreshness,
    authorization: Option<McpAuthorizationStatus>,
    diagnostic: Option<McpServerDiagnostic>,
}

impl McpServerStatus {
    pub fn name(&self) -> &McpServerName {
        &self.name
    }

    pub fn state(&self) -> McpServerState {
        self.state
    }

    pub fn generation(&self) -> Option<&str> {
        self.last_good_generation.as_deref()
    }

    pub fn capabilities(&self) -> Option<&McpCapabilities> {
        self.capabilities.as_ref()
    }

    pub fn cache_freshness(&self) -> McpCacheFreshness {
        self.cache_freshness
    }

    pub fn cache_source_revision(&self) -> Option<&str> {
        self.cache_source_revision.as_deref()
    }

    pub fn authorization(&self) -> Option<&McpAuthorizationStatus> {
        self.authorization.as_ref()
    }

    pub fn diagnostic(&self) -> Option<&McpServerDiagnostic> {
        self.diagnostic.as_ref()
    }

    fn refresh_cache_freshness(&mut self, now: Instant) {
        self.cache_freshness = cache_freshness(
            self.cache_source_revision.as_deref(),
            self.cache_expires_at,
            now,
        );
    }
}

#[derive(Clone, Default)]
pub struct McpRegistryStatus {
    servers: Arc<[McpServerStatus]>,
}

impl McpRegistryStatus {
    pub fn servers(&self) -> &[McpServerStatus] {
        &self.servers
    }

    pub fn server(&self, name: &McpServerName) -> Option<&McpServerStatus> {
        self.servers
            .binary_search_by(|status| status.name.cmp(name))
            .ok()
            .map(|index| &self.servers[index])
    }

    pub fn is_degraded(&self) -> bool {
        self.servers
            .iter()
            .any(|server| server.state != McpServerState::Admitted)
    }

    pub fn mcp_errors(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter_map(|server| {
                server.diagnostic.as_ref().map(|diagnostic| {
                    format!("MCP server `{}`: {}", server.name, diagnostic.message)
                })
            })
            .collect()
    }

    fn refreshed_at(&self, now: Instant) -> Self {
        let mut servers = self.servers.to_vec();
        for server in &mut servers {
            server.refresh_cache_freshness(now);
        }
        Self {
            servers: servers.into(),
        }
    }
}

fn cache_freshness(
    source_revision: Option<&str>,
    expires_at: Option<Instant>,
    now: Instant,
) -> McpCacheFreshness {
    if source_revision.is_none() && expires_at.is_none() {
        McpCacheFreshness::Uncached
    } else if expires_at.is_some_and(|expires_at| expires_at <= now) {
        McpCacheFreshness::Expired
    } else {
        McpCacheFreshness::Fresh
    }
}

fn status_from_snapshot(
    snapshot: &AdmittedRegistrySnapshot,
    overrides: BTreeMap<McpServerName, McpServerStatus>,
    now: Instant,
) -> McpRegistryStatus {
    let mut servers = snapshot
        .servers
        .iter()
        .map(|(name, server)| {
            (
                name.clone(),
                McpServerStatus {
                    name: name.clone(),
                    state: McpServerState::Admitted,
                    last_good_generation: Some(Arc::clone(&snapshot.generation)),
                    capabilities: Some(server.capabilities.clone()),
                    cache_source_revision: server.cache.source_revision.clone(),
                    cache_expires_at: server.cache.expires_at,
                    cache_freshness: cache_freshness(
                        server.cache.source_revision(),
                        server.cache.expires_at,
                        now,
                    ),
                    authorization: None,
                    diagnostic: None,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    servers.extend(overrides);
    McpRegistryStatus {
        servers: servers.into_values().collect::<Vec<_>>().into(),
    }
}

fn failed_server_status(
    name: &McpServerName,
    failure: &CandidateFailure,
    request_id: &str,
    predecessor: &AdmittedRegistrySnapshot,
    now: Instant,
) -> McpServerStatus {
    let retained = predecessor.servers.get(name);
    let authorization = failure.authorization.as_ref().map(|authorization| {
        let mut scopes = authorization.scopes.clone();
        scopes.sort();
        scopes.dedup();
        McpAuthorizationStatus {
            issuer: authorization.issuer.clone(),
            protected_resource: authorization.protected_resource.clone(),
            scopes: scopes.into(),
            request_correlation: Arc::from(request_id),
        }
    });
    let cache_source_revision = retained.and_then(|server| server.cache.source_revision.clone());
    let cache_expires_at = retained.and_then(|server| server.cache.expires_at);
    McpServerStatus {
        name: name.clone(),
        state: if authorization.is_some() {
            McpServerState::AuthorizationRequired
        } else if retained.is_some() {
            McpServerState::Degraded
        } else {
            McpServerState::Absent
        },
        last_good_generation: retained.map(|_| Arc::clone(&predecessor.generation)),
        capabilities: retained.map(|server| server.capabilities.clone()),
        cache_freshness: cache_freshness(cache_source_revision.as_deref(), cache_expires_at, now),
        cache_source_revision,
        cache_expires_at,
        authorization,
        diagnostic: Some(McpServerDiagnostic {
            failure: failure.failure,
            message: Arc::from(failure.diagnostic_message().replace("NAME", name.as_str())),
        }),
    }
}

fn failed_publication_status(
    predecessor: &AdmittedRegistrySnapshot,
    attempted_servers: Vec<McpServerName>,
    now: Instant,
) -> McpRegistryStatus {
    let mut servers = BTreeMap::new();
    for (name, server) in &predecessor.servers {
        servers.insert(
            name.clone(),
            McpServerStatus {
                name: name.clone(),
                state: McpServerState::Degraded,
                last_good_generation: Some(Arc::clone(&predecessor.generation)),
                capabilities: Some(server.capabilities.clone()),
                cache_source_revision: server.cache.source_revision.clone(),
                cache_expires_at: server.cache.expires_at,
                cache_freshness: cache_freshness(
                    server.cache.source_revision(),
                    server.cache.expires_at,
                    now,
                ),
                authorization: None,
                diagnostic: Some(McpServerDiagnostic {
                    failure: McpFailure::Protocol,
                    message: Arc::from("registry publication failed"),
                }),
            },
        );
    }
    for name in attempted_servers {
        servers.entry(name.clone()).or_insert(McpServerStatus {
            name,
            state: McpServerState::Absent,
            last_good_generation: None,
            capabilities: None,
            cache_source_revision: None,
            cache_expires_at: None,
            cache_freshness: McpCacheFreshness::Uncached,
            authorization: None,
            diagnostic: Some(McpServerDiagnostic {
                failure: McpFailure::Protocol,
                message: Arc::from("registry publication failed"),
            }),
        });
    }
    McpRegistryStatus {
        servers: servers.into_values().collect::<Vec<_>>().into(),
    }
}

fn failure_message(failure: McpFailure) -> &'static str {
    match failure {
        McpFailure::Absent => "server is unavailable",
        McpFailure::InvalidValue => "server configuration is invalid",
        McpFailure::InvalidResponse => "server returned an invalid response",
        McpFailure::Authority => "server authority was rejected",
        McpFailure::Transport => "server transport is unavailable",
        McpFailure::Protocol => "server protocol failed",
        McpFailure::Authorization => {
            "authorization is required; run ego mcp login NAME with an explicit registration strategy"
        }
        McpFailure::Credential => {
            "server credential is unavailable; run ego mcp login NAME with an explicit registration strategy"
        }
        McpFailure::Cancelled => "server admission was cancelled",
    }
}

struct PublishedState {
    snapshot: Arc<AdmittedRegistrySnapshot>,
    status: McpRegistryStatus,
    captures: BTreeMap<Arc<str>, Vec<Weak<McpRegistryCaptureInner>>>,
}

impl PublishedState {
    fn empty() -> Self {
        Self {
            snapshot: Arc::new(AdmittedRegistrySnapshot::empty()),
            status: McpRegistryStatus::default(),
            captures: BTreeMap::new(),
        }
    }
}

struct PublishedCaptureResolver {
    state: Arc<RwLock<PublishedState>>,
}

impl McpCaptureResolver for PublishedCaptureResolver {
    fn resolve_capture(
        &self,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, McpRegistryError> {
        resolve_capture_in_state(&self.state, stable_fingerprint)
    }
}

fn resolve_capture_in_state(
    state: &RwLock<PublishedState>,
    stable_fingerprint: &str,
) -> Result<McpRegistryCapture, McpRegistryError> {
    state
        .read()
        .map_err(|_| McpRegistryError::PublicationUnavailable)?
        .captures
        .get(stable_fingerprint)
        .and_then(|captures| captures.iter().find_map(Weak::upgrade))
        .map(|inner| McpRegistryCapture { inner })
        .ok_or_else(|| {
            McpRegistryError::InvalidCandidate(
                "MCP registry capture identity is unavailable".to_owned(),
            )
        })
}

#[derive(Clone)]
pub struct McpRegistryPublication {
    generation: Arc<str>,
    predecessor: Option<Arc<str>>,
    config_revision: u64,
    admitted_servers: Vec<String>,
    admitted_tools: Vec<String>,
    capture: McpRegistryCapture,
}

impl McpRegistryPublication {
    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn predecessor(&self) -> Option<&str> {
        self.predecessor.as_deref()
    }

    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    pub fn admitted_servers(&self) -> &[String] {
        &self.admitted_servers
    }

    pub fn admitted_tools(&self) -> &[String] {
        &self.admitted_tools
    }

    pub fn capture(&self) -> &McpRegistryCapture {
        &self.capture
    }
}

struct RegistryToolSource {
    state: Arc<RwLock<PublishedState>>,
}

impl AdmittedToolSource for RegistryToolSource {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        let snapshot = self
            .state
            .read()
            .map_err(|_| ToolCatalogueError::CaptureUnavailable)?
            .snapshot
            .clone();
        CapturedToolCatalogue::new_mcp(
            snapshot.generation.to_string(),
            snapshot.tools.iter().cloned().collect(),
            snapshot.capture.clone(),
        )
    }
}

#[derive(Clone)]
pub struct McpRegistryRuntime {
    controller: Arc<McpRegistryController>,
    continuations: Arc<McpContinuationOwner>,
    resume_sealer: Arc<ResumeSealer>,
}

impl McpRegistryRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry_journal: Arc<dyn McpRegistryJournal>,
        continuation_journal: Arc<dyn ContinuationJournal>,
        policy_captures: Arc<dyn GenerationPolicyCaptureStore>,
        http_management: Arc<dyn McpHttpEffects>,
        stdio_management: Arc<dyn McpDuplexEffectsFactory>,
        keys: Arc<dyn ResumeKeySource>,
        clock: Arc<dyn Clock>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Result<Self, McpRegistryError> {
        Self::new_with_acp(
            registry_journal,
            continuation_journal,
            policy_captures,
            http_management,
            stdio_management,
            keys,
            clock,
            max_sse_event_size,
            timeout,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_acp(
        registry_journal: Arc<dyn McpRegistryJournal>,
        continuation_journal: Arc<dyn ContinuationJournal>,
        policy_captures: Arc<dyn GenerationPolicyCaptureStore>,
        http_management: Arc<dyn McpHttpEffects>,
        stdio_management: Arc<dyn McpDuplexEffectsFactory>,
        keys: Arc<dyn ResumeKeySource>,
        clock: Arc<dyn Clock>,
        max_sse_event_size: usize,
        timeout: Duration,
        acp_management: Option<Arc<dyn crate::McpAcpEffects>>,
    ) -> Result<Self, McpRegistryError> {
        if max_sse_event_size == 0 || timeout.is_zero() {
            return Err(McpRegistryError::InvalidCandidate(
                "invalid MCP runtime limits".to_owned(),
            ));
        }
        let lifetime_ms = u64::try_from(timeout.as_millis()).map_err(|_| {
            McpRegistryError::InvalidCandidate("invalid MCP runtime timeout".to_owned())
        })?;
        let resume_sealer = Arc::new(
            ResumeSealer::new(Arc::clone(&keys), Arc::clone(&clock), lifetime_ms)
                .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?,
        );
        registry_journal.replay()?;
        let state = Arc::new(RwLock::new(PublishedState::empty()));
        let resolver: Arc<dyn McpCaptureResolver> = Arc::new(PublishedCaptureResolver {
            state: Arc::clone(&state),
        });
        let continuations = Arc::new(McpContinuationOwner::new(
            continuation_journal,
            policy_captures,
            resolver,
            keys,
            clock,
        ));
        let owner = Arc::clone(&continuations);
        let controller = Arc::new_cyclic(move |controller| {
            let schemas: Arc<dyn McpToolSchemaAdmission> =
                Arc::new(ControllerToolSchemaAdmission {
                    controller: controller.clone(),
                });
            let admission: Arc<dyn McpCandidateAdmission> =
                Arc::new(McpRuntimeCandidateAdmission::new(
                    Arc::clone(&http_management),
                    Arc::clone(&stdio_management),
                    Arc::clone(&owner),
                    schemas,
                    max_sse_event_size,
                    timeout,
                    acp_management,
                ));
            McpRegistryController {
                journal: registry_journal,
                admission,
                reconcile: tokio::sync::Mutex::new(()),
                state,
                next_request: AtomicU64::new(1),
            }
        });
        Ok(Self {
            controller,
            continuations,
            resume_sealer,
        })
    }

    pub fn controller(&self) -> &Arc<McpRegistryController> {
        &self.controller
    }
}

impl McpRuntimeBinding for McpRegistryRuntime {
    fn resume_sealer(&self) -> Option<Arc<ResumeSealer>> {
        Some(Arc::clone(&self.resume_sealer))
    }

    fn load_pending_foreign_policy(
        &self,
        continuation_id: &str,
    ) -> Result<Arc<GenerationPolicySnapshot>, String> {
        self.continuations
            .load_pending_foreign_policy(continuation_id)
            .map_err(|error| error.to_string())
    }

    fn bind_pending_foreign_continuation(
        &self,
        continuation_id: &str,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), String> {
        self.continuations
            .bind_pending_foreign_continuation(continuation_id, snapshot)
            .map_err(|error| error.to_string())
    }

    fn hook_invocation_port<'a>(
        &self,
        policy: Arc<ego_core::permit::PolicyEngine>,
        effects: ToolEffectPorts<'a>,
    ) -> Result<Arc<dyn ego_core::hook::HookInvocationPort + 'a>, String> {
        let capture = self
            .controller
            .capture()
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(McpHookPort::new(capture, policy, effects)))
    }

    fn foreign_question_port<'session, 'turn>(
        &self,
        runtime: ego_core::hook::SessionHookRuntime<'session>,
        floor: ResumeFloor,
        effects: ToolEffectPorts<'turn>,
    ) -> Arc<dyn ForeignQuestionPort + 'turn>
    where
        'session: 'turn,
    {
        Arc::new(McpForeignQuestionPort::new(
            Arc::clone(&self.continuations),
            runtime,
            floor,
            effects,
        ))
    }
}

pub struct McpRegistryController {
    journal: Arc<dyn McpRegistryJournal>,
    admission: Arc<dyn McpCandidateAdmission>,
    reconcile: tokio::sync::Mutex<()>,
    state: Arc<RwLock<PublishedState>>,
    next_request: AtomicU64,
}

impl McpRegistryController {
    pub fn new(
        journal: Arc<dyn McpRegistryJournal>,
        admission: Arc<dyn McpCandidateAdmission>,
    ) -> Result<Arc<Self>, McpRegistryError> {
        journal.replay()?;
        Ok(Arc::new(Self {
            journal,
            admission,
            reconcile: tokio::sync::Mutex::new(()),
            state: Arc::new(RwLock::new(PublishedState::empty())),
            next_request: AtomicU64::new(1),
        }))
    }

    pub async fn reconcile(
        &self,
        refresh: McpRegistryRefresh,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let _serial = self.reconcile.lock().await;
        let request_id = format!(
            "request-{}",
            self.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let config_revision = self.journal.begin_refresh(
            request_id.clone(),
            refresh.trigger.as_str().to_owned(),
            refresh.scope.journal_value(),
        )?;
        let (predecessor, prior_status) = self.current_state()?;
        let mut successor = if matches!(&refresh.scope, RegistryScope::All) {
            BTreeMap::new()
        } else {
            predecessor.servers.clone()
        };
        let mut status_overrides = match &refresh.scope {
            RegistryScope::All => BTreeMap::new(),
            RegistryScope::Server(selected) => prior_status
                .servers()
                .iter()
                .filter(|status| status.name() != selected)
                .cloned()
                .map(|status| (status.name().clone(), status))
                .collect(),
        };

        for candidate in refresh.candidates() {
            let name = candidate.name();
            if !refresh.scope.includes(name) {
                continue;
            }
            let failure = match &candidate.materialization {
                CandidateMaterialization::Rejected { failure, .. } => {
                    Some(CandidateFailure::materialization(*failure))
                }
                CandidateMaterialization::Ready(admission) => {
                    let request = McpCandidateRequest {
                        config_revision,
                        admission: Arc::clone(admission),
                        provenance: candidate.provenance.clone(),
                    };
                    match self.admission.admit(request).await {
                        Ok(admitted) if Arc::ptr_eq(&admitted.admission, admission) => {
                            successor.insert(
                                name.clone(),
                                Arc::new(AdmittedServerSlice::from_candidate(
                                    admitted,
                                    candidate.provenance.clone(),
                                )),
                            );
                            None
                        }
                        Ok(_) => Some(CandidateFailure::admission(McpCandidateError::Invalid {
                            cause: "candidate changed its live admission authority".to_owned(),
                        })),
                        Err(error) => Some(CandidateFailure::admission(error)),
                    }
                }
            };

            if let Some(failure) = failure {
                self.record_candidate_failure(&request_id, name, &failure, &predecessor)?;
                if let Some(retained) = predecessor.servers.get(name) {
                    successor.insert(name.clone(), Arc::clone(retained));
                } else {
                    successor.remove(name);
                }
                status_overrides.insert(
                    name.clone(),
                    failed_server_status(name, &failure, &request_id, &predecessor, Instant::now()),
                );
            }
        }

        if let RegistryScope::Server(name) = &refresh.scope
            && !refresh
                .candidates()
                .iter()
                .any(|candidate| candidate.name() == name)
        {
            successor.remove(name);
            status_overrides.remove(name);
        }

        self.publish(
            &request_id,
            config_revision,
            predecessor,
            successor,
            status_overrides,
        )
    }

    pub(crate) async fn reconcile_tool_schema(
        &self,
        command: McpToolSchemaCommand,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let _serial = self.reconcile.lock().await;
        let (predecessor, prior_status) = self.current_state()?;
        if command.generation.as_str() != predecessor.generation.as_ref()
            || command.admission_fingerprint.capture() != predecessor.capture.stable_fingerprint()
        {
            return Err(McpRegistryError::InvalidCandidate(
                "collapsed schema lookup source generation is stale".to_owned(),
            ));
        }
        let name = command.server_admission.name().clone();
        let server = predecessor.servers.get(&name).ok_or_else(|| {
            McpRegistryError::InvalidCandidate(
                "collapsed schema lookup source server is not admitted".to_owned(),
            )
        })?;
        if !Arc::ptr_eq(&server.admission, &command.server_admission) {
            return Err(McpRegistryError::InvalidCandidate(
                "collapsed schema lookup changed its live server admission".to_owned(),
            ));
        }
        let bootstrap = server.collapsed_bootstrap.as_ref().ok_or_else(|| {
            McpRegistryError::InvalidCandidate(
                "tool schema targets a non-collapsed MCP server".to_owned(),
            )
        })?;
        let lookup_name = bootstrap.lookup_name();
        let lookup = server
            .tools
            .iter()
            .find(|definition| definition.name() == lookup_name)
            .ok_or_else(|| {
                McpRegistryError::InvalidCandidate(
                    "collapsed schema lookup operation is not admitted".to_owned(),
                )
            })?;
        let expected = McpAdmissionFingerprint::new(
            predecessor.capture.stable_fingerprint(),
            format!("{name}/{lookup_name}"),
            lookup.admission_schema_fingerprint(),
        )
        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        if command.admission_fingerprint.capture() != expected.capture()
            || command.admission_fingerprint.operation() != expected.operation()
            || command.admission_fingerprint.schema() != expected.schema()
        {
            return Err(McpRegistryError::InvalidCandidate(
                "collapsed schema lookup admission fingerprint is stale".to_owned(),
            ));
        }
        let previous = server
            .tools
            .iter()
            .find_map(McpToolDefinition::collapsed_catalogue);
        let catalogue = McpCollapsedCatalogue::reconcile_structured_content(
            previous,
            &command.requested_name,
            command.structured_content,
        )
        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        let successor_server = Arc::new(server.with_collapsed_catalogue(catalogue)?);
        let request_id = format!(
            "request-{}",
            self.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let config_revision = self.journal.begin_refresh(
            request_id.clone(),
            RegistryTrigger::ToolsListChanged.as_str().to_owned(),
            Some(name.to_string()),
        )?;
        let status_overrides = prior_status
            .servers()
            .iter()
            .filter(|status| status.name() != &name)
            .cloned()
            .map(|status| (status.name().clone(), status))
            .collect();
        let mut successor = predecessor.servers.clone();
        successor.insert(name, successor_server);
        self.publish(
            &request_id,
            config_revision,
            predecessor,
            successor,
            status_overrides,
        )
    }

    pub fn publication(&self) -> McpRegistryPublication {
        self.current_snapshot()
            .map(|snapshot| publication_of(&snapshot))
            .unwrap_or_else(|_| publication_of(&Arc::new(AdmittedRegistrySnapshot::empty())))
    }

    pub fn status(&self) -> McpRegistryStatus {
        self.state.read().map_or_else(
            |_| McpRegistryStatus::default(),
            |state| state.status.refreshed_at(Instant::now()),
        )
    }

    pub fn admitted_tools(&self) -> Arc<dyn AdmittedToolSource> {
        Arc::new(RegistryToolSource {
            state: Arc::clone(&self.state),
        })
    }

    pub fn capture(&self) -> Result<McpRegistryCapture, McpRegistryError> {
        self.current_snapshot()
            .map(|snapshot| snapshot.capture.clone())
    }

    pub fn resolve_capture(
        &self,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, McpRegistryError> {
        resolve_capture_in_state(&self.state, stable_fingerprint)
    }

    fn current_snapshot(&self) -> Result<Arc<AdmittedRegistrySnapshot>, McpRegistryError> {
        self.state
            .read()
            .map(|state| Arc::clone(&state.snapshot))
            .map_err(|_| McpRegistryError::PublicationUnavailable)
    }

    fn current_state(
        &self,
    ) -> Result<(Arc<AdmittedRegistrySnapshot>, McpRegistryStatus), McpRegistryError> {
        self.state
            .read()
            .map(|state| (Arc::clone(&state.snapshot), state.status.clone()))
            .map_err(|_| McpRegistryError::PublicationUnavailable)
    }

    fn record_candidate_failure(
        &self,
        request_id: &str,
        server: &McpServerName,
        failure: &CandidateFailure,
        predecessor: &Arc<AdmittedRegistrySnapshot>,
    ) -> Result<(), McpRegistryError> {
        if let Some(cause) = &failure.detail {
            let _ = self
                .journal
                .record_private_diagnostic(server.as_str(), cause);
        }
        if let Some(authorization) = &failure.authorization {
            self.journal
                .append(McpRegistryEvent::AuthorizationRequired {
                    server: server.to_string(),
                    issuer: authorization.issuer.clone(),
                    protected_resource: authorization.protected_resource.clone(),
                    scopes: authorization.scopes.clone(),
                    request_correlation: request_id.to_owned(),
                })?;
        }
        let retained = predecessor
            .servers
            .contains_key(server)
            .then(|| predecessor.generation.to_string());
        self.journal.append(McpRegistryEvent::RefreshFailed {
            request_id: request_id.to_owned(),
            cause: failure.sanitized_cause().to_owned(),
            retained_generation: retained,
        })?;
        Ok(())
    }

    fn publish(
        &self,
        request_id: &str,
        config_revision: u64,
        predecessor: Arc<AdmittedRegistrySnapshot>,
        servers: BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
        status_overrides: BTreeMap<McpServerName, McpServerStatus>,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let number = match predecessor.number.checked_add(1) {
            Some(number) => number,
            None => {
                return self.fail_publication(
                    request_id,
                    "registry generation space exhausted".to_owned(),
                    &predecessor,
                    servers.keys(),
                );
            }
        };
        let generation: Arc<str> = Arc::from(format!("generation-{number}"));
        let stable_fingerprint: Arc<str> = match stable_capture_fingerprint(&servers) {
            Ok(fingerprint) => Arc::from(fingerprint),
            Err(error) => {
                return self.fail_publication(
                    request_id,
                    error.to_string(),
                    &predecessor,
                    servers.keys(),
                );
            }
        };
        let mut captured_tools = Vec::<Arc<McpCapturedTool>>::new();
        for (name, server) in &servers {
            for definition in server.tools.iter() {
                let tool = match definition.capture(
                    name.as_str(),
                    &generation,
                    &stable_fingerprint,
                    Arc::clone(&server.admission),
                    Arc::clone(&server.factory),
                ) {
                    Ok(tool) => tool,
                    Err(error) => {
                        return self.fail_publication(
                            request_id,
                            error.to_string(),
                            &predecessor,
                            servers.keys(),
                        );
                    }
                };
                captured_tools.push(tool);
            }
        }
        captured_tools.sort_by_key(|tool| tool.descriptor().name);
        let effect_tools = captured_tools
            .iter()
            .cloned()
            .map(|tool| -> Arc<dyn Tool> { tool })
            .collect::<Vec<_>>();
        let tools = effect_tools
            .iter()
            .cloned()
            .map(RegisteredTool::Effect)
            .collect::<Vec<_>>();
        if let Err(error) = CapturedToolCatalogue::new(generation.to_string(), tools.clone()) {
            return self.fail_publication(
                request_id,
                error.to_string(),
                &predecessor,
                servers.keys(),
            );
        }
        let contexts = match capture_contexts(&stable_fingerprint, &servers) {
            Ok(contexts) => Arc::new(contexts),
            Err(error) => {
                return self.fail_publication(
                    request_id,
                    error.to_string(),
                    &predecessor,
                    servers.keys(),
                );
            }
        };
        let captured_servers = servers
            .values()
            .map(|server| McpCapturedServer {
                admission: Arc::clone(&server.admission),
                identity: server.identity.clone(),
                provenance: server.provenance.clone(),
                capabilities: server.capabilities.clone(),
                instructions: server.instructions.clone(),
                cache: server.cache.clone(),
                collapsed_bootstrap: server.collapsed_bootstrap,
                factory: Arc::clone(&server.factory),
            })
            .collect::<Vec<_>>();
        let instructions = captured_instructions(&captured_servers);
        let capture = McpRegistryCapture {
            inner: Arc::new(McpRegistryCaptureInner {
                generation: Arc::clone(&generation),
                stable_fingerprint,
                tools: effect_tools.into(),
                captured_tools: captured_tools.into(),
                contexts,
                servers: captured_servers.into(),
                instructions: instructions.into(),
            }),
        };
        let snapshot = Arc::new(AdmittedRegistrySnapshot {
            number,
            generation: Arc::clone(&generation),
            config_revision,
            servers,
            tools: tools.into(),
            capture,
        });
        let publication = publication_of(&snapshot);
        self.journal.append(McpRegistryEvent::SnapshotPublished {
            generation: generation.to_string(),
            predecessor: (predecessor.number > 0).then(|| predecessor.generation.to_string()),
            servers: publication.admitted_servers.clone(),
            descriptor_digests: descriptor_digests(&snapshot.tools),
        })?;
        let mut state = self
            .state
            .write()
            .map_err(|_| McpRegistryError::PublicationUnavailable)?;
        state.status = status_from_snapshot(&snapshot, status_overrides, Instant::now());
        let captures = state
            .captures
            .entry(Arc::clone(&snapshot.capture.inner.stable_fingerprint))
            .or_default();
        captures.retain(|capture| capture.strong_count() > 0);
        captures.push(Arc::downgrade(&snapshot.capture.inner));
        state.snapshot = snapshot;
        Ok(publication)
    }

    fn fail_publication<'a>(
        &self,
        request_id: &str,
        _cause: String,
        predecessor: &Arc<AdmittedRegistrySnapshot>,
        attempted_servers: impl Iterator<Item = &'a McpServerName>,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let retained_generation =
            (predecessor.number > 0).then(|| predecessor.generation.to_string());
        self.journal.append(McpRegistryEvent::RefreshFailed {
            request_id: request_id.to_owned(),
            cause: "registry publication failed".to_owned(),
            retained_generation,
        })?;

        let attempted_servers = attempted_servers.cloned().collect::<Vec<_>>();
        let mut state = self
            .state
            .write()
            .map_err(|_| McpRegistryError::PublicationUnavailable)?;
        state.status = failed_publication_status(predecessor, attempted_servers, Instant::now());
        Err(McpRegistryError::InvalidCandidate(
            "registry publication failed".to_owned(),
        ))
    }
}

impl McpCaptureResolver for McpRegistryController {
    fn resolve_capture(
        &self,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, McpRegistryError> {
        resolve_capture_in_state(&self.state, stable_fingerprint)
    }
}

fn publication_of(snapshot: &Arc<AdmittedRegistrySnapshot>) -> McpRegistryPublication {
    let mut admitted_tools = snapshot
        .tools
        .iter()
        .map(|tool| tool.descriptor().name)
        .collect::<Vec<_>>();
    admitted_tools.sort();
    McpRegistryPublication {
        generation: Arc::clone(&snapshot.generation),
        predecessor: (snapshot.number > 1)
            .then(|| Arc::from(format!("generation-{}", snapshot.number.saturating_sub(1)))),
        config_revision: snapshot.config_revision,
        admitted_servers: snapshot.servers.keys().map(ToString::to_string).collect(),
        admitted_tools,
        capture: snapshot.capture.clone(),
    }
}

fn descriptor_digests(tools: &[RegisteredTool]) -> Vec<String> {
    let mut digests = tools
        .iter()
        .map(|tool| {
            let descriptor = tool.descriptor();
            let mut hash = Sha256::new();
            hash.update(descriptor.name.as_bytes());
            hash.update([0]);
            hash.update(descriptor.description.as_bytes());
            hash.update([0]);
            hash.update(descriptor.parameters.to_string().as_bytes());
            format!("sha256:{:x}", hash.finalize())
        })
        .collect::<Vec<_>>();
    digests.sort();
    digests
}

fn stable_capture_fingerprint(
    servers: &BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
) -> Result<String, McpRegistryError> {
    let mut fingerprint = StableCaptureHasher::new();
    fingerprint.count("server-count", servers.len());
    for (name, server) in servers {
        fingerprint.field("server", name.as_str().as_bytes());
        fingerprint.field("candidate-identity", server.identity.as_str().as_bytes());
        fingerprint.provenance(&server.provenance);
        fingerprint.server_admission(&server.admission)?;
        fingerprint.field("capability-tools", &[u8::from(server.capabilities.tools)]);
        fingerprint.field(
            "capability-resources",
            &[u8::from(server.capabilities.resources)],
        );
        fingerprint.field(
            "capability-prompts",
            &[u8::from(server.capabilities.prompts)],
        );
        fingerprint.optional_str("cache-source-revision", server.cache.source_revision());
        fingerprint.field(
            "collapsed-bootstrap",
            &[u8::from(server.collapsed_bootstrap.is_some())],
        );

        let mut descriptors = server.descriptors.iter().collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        fingerprint.count("descriptor-count", descriptors.len());
        for descriptor in descriptors {
            fingerprint.field("descriptor-name", descriptor.name.as_bytes());
            fingerprint.field("descriptor-description", descriptor.description.as_bytes());
            fingerprint.json("descriptor-parameters", &descriptor.parameters);
        }

        let mut definitions = server.tools.iter().collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name().cmp(right.name()));
        fingerprint.count("operation-count", definitions.len());
        for definition in definitions {
            fingerprint.field("operation-name", definition.name().as_bytes());
            fingerprint.field(
                "operation-schema",
                definition.admission_schema_fingerprint().as_bytes(),
            );
        }

        let mut resources = server.resources.iter().collect::<Vec<_>>();
        resources.sort_by(|left, right| left.uri.cmp(&right.uri));
        fingerprint.count("resource-count", resources.len());
        for resource in resources {
            let value = serde_json::to_value(resource)
                .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            fingerprint.json("resource", &value);
        }

        let mut prompts = server.prompts.iter().collect::<Vec<_>>();
        prompts.sort_by(|left, right| left.name.cmp(&right.name));
        fingerprint.count("prompt-count", prompts.len());
        for prompt in prompts {
            let value = serde_json::to_value(prompt)
                .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            fingerprint.json("prompt", &value);
        }
    }
    Ok(fingerprint.finish())
}

struct StableCaptureHasher(Sha256);

impl StableCaptureHasher {
    fn new() -> Self {
        let mut hash = Sha256::new();
        hash.update(b"ego:mcp-registry-capture:v1");
        Self(hash)
    }

    fn field(&mut self, name: &str, value: &[u8]) {
        self.bytes(name.as_bytes());
        self.bytes(value);
    }

    fn bytes(&mut self, value: &[u8]) {
        self.0.update(value.len().to_string().as_bytes());
        self.0.update([0]);
        self.0.update(value);
    }

    fn count(&mut self, name: &str, value: usize) {
        self.field(name, value.to_string().as_bytes());
    }

    fn optional_str(&mut self, name: &str, value: Option<&str>) {
        self.field(name, &[u8::from(value.is_some())]);
        if let Some(value) = value {
            self.field(name, value.as_bytes());
        }
    }

    fn provenance(&mut self, provenance: &McpConfigProvenance) {
        match provenance {
            McpConfigProvenance::Layer { source } => {
                self.field("provenance-kind", b"layer");
                self.field("provenance-source", source.to_string().as_bytes());
            }
            McpConfigProvenance::Profile { source, name } => {
                self.field("provenance-kind", b"profile");
                self.field("provenance-source", source.to_string().as_bytes());
                self.field("provenance-profile", name.as_bytes());
            }
        }
    }

    fn server_admission(&mut self, admission: &McpServerAdmission) -> Result<(), McpRegistryError> {
        match admission.config() {
            McpServerConfig::Stdio(config) => {
                self.field("transport", b"stdio");
                self.field("command", config.command.as_bytes());
                self.count("argument-count", config.args.len());
                for argument in &config.args {
                    self.field("argument", argument.as_bytes());
                }
                self.field("cwd-present", &[u8::from(config.cwd.is_some())]);
                if let Some(cwd) = &config.cwd {
                    self.field("cwd", cwd.as_os_str().as_encoded_bytes());
                }
                let mut inherited = config
                    .inherit_env
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>();
                inherited.sort();
                self.count("inherit-env-count", inherited.len());
                for name in inherited {
                    self.field("inherit-env", name.as_bytes());
                }
                let environment = admission.stdio_environment().ok_or_else(|| {
                    McpRegistryError::InvalidCandidate(
                        "admitted MCP stdio server has no stdio material".to_owned(),
                    )
                })?;
                self.count("environment-count", environment.len());
                for binding in environment {
                    let binding = binding
                        .explicit_binding()
                        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
                    self.field("environment-name", binding.name().to_string().as_bytes());
                    self.field(
                        "environment-value",
                        binding.expose_value().expose_secret_bytes(),
                    );
                }
            }
            McpServerConfig::Http(config) => {
                self.field("transport", b"http");
                self.field("http-profile", config.profile.as_str().as_bytes());
                self.field("url", config.url.to_string().as_bytes());
                self.field(
                    "http-auth",
                    match config.auth {
                        ego_core::config::McpHttpAuth::None => b"none",
                        ego_core::config::McpHttpAuth::Oauth => b"oauth",
                    },
                );
                self.field(
                    "allow-insecure-loopback",
                    &[u8::from(config.allow_insecure_loopback)],
                );
                let headers = admission.http_headers().ok_or_else(|| {
                    McpRegistryError::InvalidCandidate(
                        "admitted MCP HTTP server has no HTTP material".to_owned(),
                    )
                })?;
                self.count("header-count", headers.len());
                for header in headers {
                    self.field("header-name", header.name().as_bytes());
                    self.field("header-value", header.value().expose_secret_bytes());
                }
            }
            McpServerConfig::Acp { server_id } => {
                self.field("transport", b"acp");
                self.field("server-id", server_id.as_bytes());
            }
        }
        Ok(())
    }

    fn json(&mut self, name: &str, value: &serde_json::Value) {
        self.bytes(name.as_bytes());
        self.json_value(value);
    }

    fn json_value(&mut self, value: &serde_json::Value) {
        match value {
            serde_json::Value::Null => self.field("json-type", b"null"),
            serde_json::Value::Bool(value) => {
                self.field("json-type", b"boolean");
                self.field("json-boolean", &[u8::from(*value)]);
            }
            serde_json::Value::Number(value) => {
                self.field("json-type", b"number");
                self.field("json-number", value.to_string().as_bytes());
            }
            serde_json::Value::String(value) => {
                self.field("json-type", b"string");
                self.field("json-string", value.as_bytes());
            }
            serde_json::Value::Array(values) => {
                self.field("json-type", b"array");
                self.count("json-length", values.len());
                for value in values {
                    self.json_value(value);
                }
            }
            serde_json::Value::Object(values) => {
                self.field("json-type", b"object");
                self.count("json-length", values.len());
                let mut fields = values.iter().collect::<Vec<_>>();
                fields.sort_by_key(|(name, _)| (*name).clone());
                for (name, value) in fields {
                    self.field("json-field", name.as_bytes());
                    self.json_value(value);
                }
            }
        }
    }

    fn finish(self) -> String {
        format!("sha256:{:x}", self.0.finalize())
    }
}

fn validate_candidate_contents(
    capabilities: &McpCapabilities,
    tools: &[GeneratorToolDescriptor],
    resources: &[Resource],
    prompts: &[Prompt],
) -> Result<(), McpRegistryError> {
    if (!tools.is_empty() && !capabilities.has_tools())
        || (!resources.is_empty() && !capabilities.has_resources())
        || (!prompts.is_empty() && !capabilities.has_prompts())
    {
        return Err(McpRegistryError::InvalidCandidate(
            "MCP candidate content exceeds its advertised capabilities".to_owned(),
        ));
    }
    let mut tool_names = std::collections::BTreeSet::new();
    for tool in tools {
        if tool.name.trim().is_empty()
            || !tool.parameters.is_object()
            || !tool_names.insert(tool.name.as_str())
        {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate contains an invalid or duplicate tool".to_owned(),
            ));
        }
    }
    let mut resource_uris = std::collections::BTreeSet::new();
    for resource in resources {
        if resource.uri.trim().is_empty()
            || resource.name.trim().is_empty()
            || !resource_uris.insert(resource.uri.as_str())
        {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate contains an invalid or duplicate resource".to_owned(),
            ));
        }
    }
    let mut prompt_names = std::collections::BTreeSet::new();
    for prompt in prompts {
        if prompt.name.trim().is_empty()
            || !prompt_names.insert(prompt.name.as_str())
            || prompt.arguments.as_ref().is_some_and(|arguments| {
                let mut names = std::collections::BTreeSet::new();
                arguments.iter().any(|argument| {
                    argument.name.trim().is_empty() || !names.insert(&argument.name)
                })
            })
        {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate contains an invalid or duplicate prompt".to_owned(),
            ));
        }
    }
    Ok(())
}

fn capture_contexts(
    capture: &str,
    servers: &BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
) -> Result<McpCapturedContextSource, McpRegistryError> {
    let mut operations = Vec::new();
    for (server_name, server) in servers {
        for resource in server.resources.iter() {
            let descriptor = descriptor_fingerprint(resource)?;
            let revision = server
                .cache
                .source_revision()
                .unwrap_or(&descriptor)
                .to_owned();
            let entry = ContextEntry::resource(
                server_name.to_string(),
                resource.uri.clone(),
                resource.name.clone(),
                ContextSensitivity::Restricted,
                CacheScope::Private,
                revision,
            )
            .and_then(|entry| entry.with_admission_capture(capture))
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            let subject = McpSubject::Resource {
                server: server_name.to_string(),
                uri: resource.uri.clone(),
            };
            let admission = McpAdmissionFingerprint::new(
                capture,
                format!("{server_name}/{}", resource.uri),
                descriptor,
            )
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            operations.push(McpCapturedContextOperation {
                entry,
                subject,
                operation: McpOperationKind::ReadResource,
                admission,
                server_admission: Arc::clone(&server.admission),
                factory: Arc::clone(&server.factory),
            });
        }
        for prompt in server.prompts.iter() {
            let descriptor = descriptor_fingerprint(prompt)?;
            let revision = server
                .cache
                .source_revision()
                .unwrap_or(&descriptor)
                .to_owned();
            let arguments = prompt
                .arguments
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|argument| argument.name.clone())
                .collect();
            let entry = ContextEntry::prompt(
                server_name.to_string(),
                prompt.name.clone(),
                prompt.description.clone().unwrap_or_default(),
                arguments,
                ContextSensitivity::Restricted,
                CacheScope::Private,
                revision,
            )
            .and_then(|entry| entry.with_admission_capture(capture))
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            let subject = McpSubject::Prompt {
                server: server_name.to_string(),
                name: prompt.name.clone(),
            };
            let admission = McpAdmissionFingerprint::new(
                capture,
                format!("{server_name}/{}", prompt.name),
                descriptor,
            )
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            operations.push(McpCapturedContextOperation {
                entry,
                subject,
                operation: McpOperationKind::GetPrompt,
                admission,
                server_admission: Arc::clone(&server.admission),
                factory: Arc::clone(&server.factory),
            });
        }
    }
    McpCapturedContextSource::new(operations)
}

fn descriptor_fingerprint(value: &impl serde::Serialize) -> Result<String, McpRegistryError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod collapsed_mcp_red {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;
    use rmcp::model::Prompt;
    use serde_json::{Value, json};

    use ego_core::config::{McpConfigProvenance, McpServerConfig, Source};
    use ego_core::generator::types::ToolDescriptor;
    use ego_core::mcp::ToolExecution;
    use ego_core::permit::{CanonPath, GuardedIo, Op, PreparedCall, ToolError};
    use ego_core::turn::{McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError};

    use super::*;

    const SERVER: &str = "collapsed";
    fn descriptor(name: &str, schema: Value) -> ToolDescriptor {
        ToolDescriptor {
            name: name.to_owned(),
            description: format!("{name} fixture"),
            parameters: schema,
        }
    }

    fn management() -> Vec<ToolDescriptor> {
        vec![
            descriptor(
                "search_tools",
                json!({
                    "type":"object",
                    "properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1}},
                    "required":["query"],
                    "additionalProperties":false
                }),
            ),
            descriptor(
                "get_tool_schema",
                json!({
                    "type":"object",
                    "properties":{"tool_name":{"type":"string"}},
                    "required":["tool_name"],
                    "additionalProperties":false
                }),
            ),
            descriptor(
                "call_tool",
                json!({
                    "type":"object",
                    "properties":{"tool_name":{"type":"string"},"arguments":{"type":"object"}},
                    "required":["tool_name","arguments"],
                    "additionalProperties":false
                }),
            ),
        ]
    }

    fn admission(name: &str) -> Arc<McpServerAdmission> {
        Arc::new(
            McpServerAdmission::new(
                name.parse().expect("server name"),
                McpServerConfig::stdio(format!("mcp-{name}"), Vec::new(), None, Vec::new())
                    .expect("stdio config"),
                crate::McpTransportMaterial::stdio(Vec::new()),
            )
            .expect("admission"),
        )
    }

    #[derive(Default)]
    struct Factory;

    impl McpOperationFactory for Factory {
        fn prepare(
            &self,
            _admission: Arc<McpServerAdmission>,
            _operation: crate::McpOperation,
        ) -> Result<PreparedCall, ToolError> {
            Ok(PreparedCall::new(
                "collapsed-test",
                Op::Read {
                    path: CanonPath::new("/fixture"),
                },
            ))
        }

        fn execute<'a, 'io>(
            &'a self,
            _admission: Arc<McpServerAdmission>,
            _call: PreparedCall,
            _io: &'a dyn GuardedIo<'io>,
            _generation: Arc<str>,
        ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
        where
            'io: 'a,
        {
            Box::pin(async {
                Ok(ToolExecution::Complete(
                    ego_core::permit::ToolResult::builtin("collapsed-test"),
                ))
            })
        }
    }

    enum Outcome {
        Candidate(McpAdmittedCandidate),
    }

    #[derive(Default)]
    struct Admission {
        outcomes: Mutex<VecDeque<Outcome>>,
    }

    impl Admission {
        fn push(&self, outcome: Outcome) {
            self.outcomes.lock().expect("outcomes").push_back(outcome);
        }
    }

    impl McpCandidateAdmission for Admission {
        fn admit(
            &self,
            _request: McpCandidateRequest,
        ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
            let outcome = self.outcomes.lock().expect("outcomes").pop_front();
            Box::pin(async move {
                match outcome.expect("candidate outcome") {
                    Outcome::Candidate(candidate) => Ok(candidate),
                }
            })
        }
    }

    #[derive(Default)]
    struct Journal {
        events: Mutex<Vec<McpRegistryEvent>>,
        revision: Mutex<u64>,
    }

    impl McpRegistryJournal for Journal {
        fn record_private_diagnostic(
            &self,
            _server: &str,
            _cause: &str,
        ) -> Result<(), McpRegistryJournalError> {
            Ok(())
        }

        fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
            self.events.lock().expect("events").push(event);
            Ok(())
        }

        fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
            Ok(self.events.lock().expect("events").clone())
        }

        fn begin_refresh(
            &self,
            request_id: String,
            trigger: String,
            server_scope: Option<String>,
        ) -> Result<u64, McpRegistryJournalError> {
            let mut revision = self.revision.lock().expect("revision");
            *revision += 1;
            self.events
                .lock()
                .expect("events")
                .push(McpRegistryEvent::RefreshRequested {
                    request_id,
                    trigger,
                    server_scope,
                    config_revision: *revision,
                });
            Ok(*revision)
        }
    }

    fn candidate(
        server_admission: Arc<McpServerAdmission>,
        catalogue: Option<McpCollapsedCatalogue>,
    ) -> McpAdmittedCandidate {
        McpAdmittedCandidate::new(
            Arc::clone(&server_admission),
            McpCandidateIdentity::validated(server_admission.name().as_str()),
            McpCandidateInventory::new(
                McpCapabilities::tools(),
                None,
                management(),
                Vec::new(),
                Vec::<Prompt>::new(),
                McpCacheMetadata::default(),
                catalogue,
            ),
            Arc::new(Factory),
        )
        .expect("candidate")
    }

    fn refresh(admission: &Arc<McpServerAdmission>) -> McpRegistryRefresh {
        McpRegistryRefresh::new(
            vec![McpRegistryCandidate::ready(
                Arc::clone(admission),
                McpConfigProvenance::Layer {
                    source: Source::User,
                },
            )],
            RegistryTrigger::Reload,
            RegistryScope::All,
        )
        .expect("refresh")
    }

    fn remove_refresh(admission: &Arc<McpServerAdmission>) -> McpRegistryRefresh {
        McpRegistryRefresh::new(
            Vec::new(),
            RegistryTrigger::Remove,
            RegistryScope::Server(admission.name().clone()),
        )
        .expect("remove refresh")
    }

    async fn bootstrap_named(
        name: &str,
    ) -> (
        Arc<McpRegistryController>,
        Arc<McpServerAdmission>,
        McpRegistryPublication,
    ) {
        let server_admission = admission(name);
        let candidate_admission = Arc::new(Admission::default());
        candidate_admission.push(Outcome::Candidate(candidate(
            Arc::clone(&server_admission),
            None,
        )));
        let journal = Arc::new(Journal::default());
        let controller = McpRegistryController::new(
            Arc::clone(&journal) as Arc<dyn McpRegistryJournal>,
            Arc::clone(&candidate_admission) as Arc<dyn McpCandidateAdmission>,
        )
        .expect("controller");
        let publication = controller
            .reconcile(refresh(&server_admission))
            .await
            .expect("bootstrap");
        (controller, server_admission, publication)
    }

    async fn bootstrap() -> (
        Arc<McpRegistryController>,
        Arc<McpServerAdmission>,
        McpRegistryPublication,
    ) {
        bootstrap_named(SERVER).await
    }

    fn fingerprint_for(server: &str, capture: &McpRegistryCapture) -> McpAdmissionFingerprint {
        let lookup_schema = management()
            .into_iter()
            .find(|tool| tool.name == "get_tool_schema")
            .expect("get_tool_schema descriptor");
        let lookup_schema = crate::dynamic::schema_fingerprint(&lookup_schema.parameters)
            .expect("lookup schema fingerprint");
        McpAdmissionFingerprint::new(
            capture.stable_fingerprint(),
            format!("{server}/get_tool_schema"),
            lookup_schema,
        )
        .expect("lookup fingerprint")
    }

    fn fingerprint(capture: &McpRegistryCapture) -> McpAdmissionFingerprint {
        fingerprint_for(SERVER, capture)
    }

    fn schema_definition(name: &str, schema: Value) -> Value {
        json!({
            "name": name,
            "description": format!("{name} definition"),
            "inputSchema": schema
        })
    }

    fn command(
        capture: &McpRegistryCapture,
        generation: &str,
        requested: &str,
        structured_content: Value,
    ) -> McpToolSchemaCommand {
        McpToolSchemaCommand::new(
            capture.servers()[0].clone(),
            generation.to_owned(),
            fingerprint(capture),
            requested.to_owned(),
            structured_content,
        )
    }

    fn weather_schema() -> Value {
        json!({
            "type":"object",
            "properties":{"city":{"type":"string"}},
            "required":["city"],
            "additionalProperties":false
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn collapsed_bootstrap_publishes_management_without_call_tool() {
        let bootstrap_contract =
            McpCollapsedBootstrap::new(management()).expect("collapsed bootstrap");
        assert_eq!(
            bootstrap_contract.tool_names().collect::<Vec<_>>(),
            vec!["get_tool_schema", "search_tools"]
        );
        let (controller, _admission, publication) = bootstrap().await;
        assert_eq!(publication.generation(), "generation-1");
        assert_eq!(
            publication.admitted_tools(),
            &["get_tool_schema".to_owned(), "search_tools".to_owned()]
        );
        assert!(
            !controller
                .capture()
                .expect("capture")
                .tools()
                .iter()
                .any(|tool| tool.descriptor().name == "call_tool")
        );
    }

    // Catches: Tool annotations prevent a real registry schema lookup from publishing.
    #[tokio::test(flavor = "current_thread")]
    async fn exact_schema_lookup_with_annotations_publishes_n_plus_one_and_preserves_old_capture() {
        let (controller, _admission, first) = bootstrap().await;
        let old_capture = first.capture().clone();
        let mut definition = schema_definition("weather", weather_schema());
        definition["annotations"] = json!({"readOnlyHint":true});
        let command = command(first.capture(), "generation-1", "weather", definition);
        let second = controller
            .reconcile_tool_schema(command)
            .await
            .expect("schema reconciliation");
        assert_eq!(second.generation(), "generation-2");
        assert_eq!(second.predecessor(), Some("generation-1"));
        assert!(
            second
                .admitted_tools()
                .iter()
                .any(|name| name == "call_tool")
        );
        assert!(
            !old_capture
                .tools()
                .iter()
                .any(|tool| tool.descriptor().name == "call_tool")
        );
        assert!(
            second
                .capture()
                .servers()
                .iter()
                .all(|server| server.admission().name().as_str() == SERVER)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_exact_schema_replaces_and_merges_only_requested_tools_deterministically() {
        let (controller, _admission, first) = bootstrap().await;
        let first = controller
            .reconcile_tool_schema(command(
                first.capture(),
                "generation-1",
                "weather",
                schema_definition("weather", weather_schema()),
            ))
            .await
            .expect("first schema lookup");
        let second = controller
            .reconcile_tool_schema(command(
                first.capture(),
                "generation-2",
                "calendar",
                schema_definition(
                    "calendar",
                    json!({"type":"object","additionalProperties":false}),
                ),
            ))
            .await
            .expect("second schema lookup");
        let third = controller
            .reconcile_tool_schema(command(
                second.capture(),
                "generation-3",
                "weather",
                schema_definition(
                    "weather",
                    json!({
                        "type":"object",
                        "properties":{"city":{"type":"string"},"units":{"type":"string"}},
                        "required":["city","units"],
                        "additionalProperties":false
                    }),
                ),
            ))
            .await
            .expect("replacement schema lookup");
        assert_eq!(first.generation(), "generation-2");
        assert_eq!(second.generation(), "generation-3");
        assert_eq!(third.generation(), "generation-4");
        let snapshot = controller.current_snapshot().expect("snapshot");
        let server = snapshot
            .servers
            .get(&SERVER.parse().expect("server name"))
            .expect("server");
        let execute = server
            .tools
            .iter()
            .find(|tool| tool.name() == "call_tool")
            .expect("call_tool wrapper");
        let catalogue = execute.collapsed_catalogue().expect("catalogue");
        assert_eq!(
            catalogue.tool_names().collect::<Vec<_>>(),
            vec!["call_tool", "calendar", "weather"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_or_invalid_schema_is_rejected_without_publication() {
        let cases = [
            ("empty definition", "weather", json!({}), "generation-1"),
            (
                "stale generation",
                "weather",
                schema_definition("weather", weather_schema()),
                "generation-0",
            ),
            (
                "missing requested",
                "calendar",
                schema_definition("weather", weather_schema()),
                "generation-1",
            ),
            (
                "duplicate descriptor",
                "weather",
                json!({"name":"weather","description":"weather definition","inputSchema":weather_schema(),"extra":true}),
                "generation-1",
            ),
            (
                "management descriptor",
                "search_tools",
                schema_definition("search_tools", json!({"type":"object"})),
                "generation-1",
            ),
            (
                "lookup management descriptor",
                "get_tool_schema",
                schema_definition("get_tool_schema", json!({"type":"object"})),
                "generation-1",
            ),
            (
                "dispatch management descriptor",
                "call_tool",
                schema_definition("call_tool", json!({"type":"object"})),
                "generation-1",
            ),
            (
                "unexpected descriptor",
                "weather",
                schema_definition("calendar", json!({"type":"object"})),
                "generation-1",
            ),
            (
                "invalid schema",
                "weather",
                schema_definition("weather", json!({"type":7})),
                "generation-1",
            ),
            (
                "protocol error",
                "weather",
                json!({"isError":true,"content":[{"type":"text","text":"failed"}]}),
                "generation-1",
            ),
        ];
        for (label, requested, content, generation) in cases {
            let (controller, _admission, first) = bootstrap().await;
            let before = controller.publication();
            let command = command(first.capture(), generation, requested, content);
            assert!(
                controller.reconcile_tool_schema(command).await.is_err(),
                "{label} was accepted"
            );
            assert_eq!(
                controller.publication().generation(),
                before.generation(),
                "{label}"
            );
            assert_eq!(
                controller.publication().admitted_tools(),
                before.admitted_tools(),
                "{label}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_server_and_lookup_fingerprint_are_rejected_without_publication() {
        let (controller, _admission, first) = bootstrap().await;
        let (_other_controller, _other_admission, other) = bootstrap_named("other").await;
        let stale = McpToolSchemaCommand::new(
            other.capture().servers()[0].clone(),
            "generation-1".to_owned(),
            fingerprint_for("other", other.capture()),
            "weather".to_owned(),
            schema_definition("weather", weather_schema()),
        );
        assert!(controller.reconcile_tool_schema(stale).await.is_err());
        let before = controller.publication();
        let stale = McpToolSchemaCommand::new(
            first.capture().servers()[0].clone(),
            "generation-1".to_owned(),
            McpAdmissionFingerprint::new(
                "generation-1",
                format!("{SERVER}/get_tool_schema"),
                "sha256:stale",
            )
            .expect("stale fingerprint"),
            "weather".to_owned(),
            schema_definition("weather", weather_schema()),
        );
        assert!(controller.reconcile_tool_schema(stale).await.is_err());
        assert_eq!(controller.publication().generation(), before.generation());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn removal_withdraws_collapsed_tools_while_invalid_refresh_keeps_last_good() {
        let (controller, admission, first) = bootstrap().await;
        let before = controller.publication();
        let invalid = command(
            first.capture(),
            "generation-1",
            "weather",
            json!({"name":"weather","description":"weather definition","inputSchema":{"type":7}}),
        );
        assert!(controller.reconcile_tool_schema(invalid).await.is_err());
        assert_eq!(controller.publication().generation(), before.generation());
        let removed = controller
            .reconcile(remove_refresh(&admission))
            .await
            .expect("remove refresh");
        assert!(removed.admitted_tools().is_empty());
        assert!(removed.capture().tools().is_empty());
    }
}
