//! Serialized MCP reconciliation and immutable admitted snapshots.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use ego_core::config::{
    McpConfigProvenance, McpServerConfig, McpServerName, ResolvedMcpConfig, ResolvedMcpEntry,
};
use ego_core::generator::types::ToolDescriptor as GeneratorToolDescriptor;
use ego_core::mcp::{
    CacheScope, ContextEntry, ContextError, ContextSensitivity, ContextSnapshot, ContextSource,
    McpAdmissionFingerprint, McpOperationKind, McpSubject,
};
use ego_core::permit::Tool;
use ego_core::turn::{
    AdmittedToolSource, CapturedToolCatalogue, McpRegistryEvent, McpRegistryJournal,
    McpRegistryJournalError, RegisteredTool, ToolCatalogueError,
};
use rmcp::model::{Prompt, Resource};
use sha2::{Digest, Sha256};

use crate::dynamic::McpToolDefinition;
use crate::{McpDynamicCatalogue, McpDynamicFamily, McpFuture, McpOperationFactory};

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
pub struct McpRegistryRefresh {
    config: ResolvedMcpConfig,
    trigger: RegistryTrigger,
    scope: RegistryScope,
}

impl McpRegistryRefresh {
    pub fn new(config: ResolvedMcpConfig, trigger: RegistryTrigger, scope: RegistryScope) -> Self {
        Self {
            config,
            trigger,
            scope,
        }
    }

    pub fn config(&self) -> &ResolvedMcpConfig {
        &self.config
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
    name: McpServerName,
    server: McpServerConfig,
    provenance: McpConfigProvenance,
}

impl McpCandidateRequest {
    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn server(&self) -> &McpServerConfig {
        &self.server
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
    fn cause(&self) -> String {
        match self {
            Self::Invalid { cause } => cause.clone(),
            Self::AuthorizationRequired { .. } => "authorization required".to_owned(),
        }
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
    tools: Vec<GeneratorToolDescriptor>,
    resources: Vec<Resource>,
    prompts: Vec<Prompt>,
    cache: McpCacheMetadata,
    dynamic: Option<McpDynamicCatalogue>,
}

impl McpCandidateInventory {
    pub fn new(
        capabilities: McpCapabilities,
        tools: Vec<GeneratorToolDescriptor>,
        resources: Vec<Resource>,
        prompts: Vec<Prompt>,
        cache: McpCacheMetadata,
        dynamic: Option<McpDynamicCatalogue>,
    ) -> Self {
        Self {
            capabilities,
            tools,
            resources,
            prompts,
            cache,
            dynamic,
        }
    }
}

#[derive(Clone)]
pub struct McpAdmittedCandidate {
    name: Arc<str>,
    identity: McpCandidateIdentity,
    capabilities: McpCapabilities,
    config: Arc<McpServerConfig>,
    tools: Arc<[McpToolDefinition]>,
    resources: Arc<[Resource]>,
    prompts: Arc<[Prompt]>,
    cache: McpCacheMetadata,
    dynamic_family: Option<McpDynamicFamily>,
    factory: Arc<dyn McpOperationFactory>,
}

impl McpAdmittedCandidate {
    pub fn new(
        name: String,
        identity: McpCandidateIdentity,
        config: McpServerConfig,
        inventory: McpCandidateInventory,
        factory: Arc<dyn McpOperationFactory>,
    ) -> Result<Self, McpRegistryError> {
        if name.is_empty() || identity.as_str().is_empty() || name != identity.as_str() {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate identity does not match its configured name".to_owned(),
            ));
        }
        config
            .validate()
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        let McpCandidateInventory {
            capabilities,
            tools,
            resources,
            prompts,
            cache,
            dynamic,
        } = inventory;
        validate_candidate_contents(&capabilities, &tools, &resources, &prompts)?;
        let dynamic_family = McpDynamicFamily::detect(&tools)
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        match (&dynamic_family, &dynamic) {
            (Some(detected), Some(catalogue)) if detected == catalogue.family() => {}
            (None, None) => {}
            (Some(_), None) => {
                return Err(McpRegistryError::InvalidCandidate(
                    "dynamic MCP family has no admitted inner catalogue".to_owned(),
                ));
            }
            _ => {
                return Err(McpRegistryError::InvalidCandidate(
                    "dynamic MCP catalogue does not match the detected family".to_owned(),
                ));
            }
        }
        let mut definitions = Vec::new();
        for descriptor in tools {
            let definition = if let Some(catalogue) = &dynamic {
                if catalogue
                    .family()
                    .management_names()
                    .contains(&descriptor.name.as_str())
                {
                    if descriptor.name == "execute_tool" {
                        Some(McpToolDefinition::dynamic(descriptor, catalogue.clone()))
                    } else {
                        None
                    }
                } else {
                    Some(McpToolDefinition::direct(descriptor))
                }
            } else {
                Some(McpToolDefinition::direct(descriptor))
            };
            if let Some(definition) = definition {
                definitions.push(
                    definition
                        .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?,
                );
            }
        }
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
            name: Arc::from(name),
            identity,
            capabilities,
            config: Arc::new(config),
            tools: definitions.into(),
            resources: resources.into(),
            prompts: prompts.into(),
            cache,
            dynamic_family,
            factory,
        })
    }
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
    identity: McpCandidateIdentity,
    provenance: McpConfigProvenance,
    capabilities: McpCapabilities,
    config: Arc<McpServerConfig>,
    tools: Arc<[McpToolDefinition]>,
    resources: Arc<[Resource]>,
    prompts: Arc<[Prompt]>,
    cache: McpCacheMetadata,
    dynamic_family: Option<McpDynamicFamily>,
    factory: Arc<dyn McpOperationFactory>,
}

impl AdmittedServerSlice {
    fn from_candidate(candidate: McpAdmittedCandidate, provenance: McpConfigProvenance) -> Self {
        Self {
            identity: candidate.identity,
            provenance,
            capabilities: candidate.capabilities,
            config: candidate.config,
            tools: candidate.tools,
            resources: candidate.resources,
            prompts: candidate.prompts,
            cache: candidate.cache,
            dynamic_family: candidate.dynamic_family,
            factory: candidate.factory,
        }
    }
}

#[derive(Clone)]
pub struct McpCapturedServer {
    identity: McpCandidateIdentity,
    provenance: McpConfigProvenance,
    capabilities: McpCapabilities,
    config: Arc<McpServerConfig>,
    cache: McpCacheMetadata,
    dynamic_family: Option<McpDynamicFamily>,
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

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    pub fn cache(&self) -> &McpCacheMetadata {
        &self.cache
    }

    pub fn dynamic_family(&self) -> Option<&McpDynamicFamily> {
        self.dynamic_family.as_ref()
    }

    pub fn operation_factory(&self) -> Arc<dyn McpOperationFactory> {
        Arc::clone(&self.factory)
    }
}

#[derive(Clone)]
pub struct McpCapturedContextOperation {
    entry: ContextEntry,
    subject: McpSubject,
    operation: McpOperationKind,
    admission: McpAdmissionFingerprint,
    config: Arc<McpServerConfig>,
    factory: Arc<dyn McpOperationFactory>,
}

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

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    pub fn operation_factory(&self) -> Arc<dyn McpOperationFactory> {
        Arc::clone(&self.factory)
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
    generation: Arc<str>,
    tools: Arc<[Arc<dyn Tool>]>,
    contexts: Arc<McpCapturedContextSource>,
    servers: Arc<[McpCapturedServer]>,
}

impl McpRegistryCapture {
    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    pub fn context_source(&self) -> Arc<dyn ContextSource> {
        Arc::clone(&self.contexts) as Arc<dyn ContextSource>
    }

    pub fn context_operations(&self) -> &[McpCapturedContextOperation] {
        self.contexts.operations()
    }

    pub fn servers(&self) -> &[McpCapturedServer] {
        &self.servers
    }
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
                generation: Arc::from("generation-0"),
                tools: Arc::from(Vec::<Arc<dyn Tool>>::new()),
                contexts,
                servers: Arc::from(Vec::<McpCapturedServer>::new()),
            },
        }
    }
}

#[derive(Clone, Default)]
pub struct McpRegistryStatus {
    admitted: Vec<String>,
    degraded: Vec<String>,
    absent: Vec<String>,
}

impl McpRegistryStatus {
    pub fn admitted(&self) -> &[String] {
        &self.admitted
    }

    pub fn degraded(&self) -> &[String] {
        &self.degraded
    }

    pub fn absent(&self) -> &[String] {
        &self.absent
    }

    pub fn is_degraded(&self) -> bool {
        !self.degraded.is_empty()
    }
}

struct PublishedState {
    snapshot: Arc<AdmittedRegistrySnapshot>,
    status: McpRegistryStatus,
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
        CapturedToolCatalogue::new(
            snapshot.generation.to_string(),
            snapshot.tools.iter().cloned().collect(),
        )
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
            state: Arc::new(RwLock::new(PublishedState {
                snapshot: Arc::new(AdmittedRegistrySnapshot::empty()),
                status: McpRegistryStatus::default(),
            })),
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
        let predecessor = self.current_snapshot()?;
        let mut successor = if matches!(&refresh.scope, RegistryScope::All) {
            BTreeMap::new()
        } else {
            predecessor.servers.clone()
        };

        for (name, entry) in refresh.config.entries() {
            if !refresh.scope.includes(name) {
                continue;
            }
            match entry {
                ResolvedMcpEntry::Tombstone { .. } => {
                    successor.remove(name);
                }
                ResolvedMcpEntry::Configured { server, provenance } => {
                    let request = McpCandidateRequest {
                        config_revision,
                        name: name.clone(),
                        server: server.clone(),
                        provenance: provenance.clone(),
                    };
                    match self.admission.admit(request).await {
                        Ok(candidate) => {
                            if candidate.name.as_ref() != name.as_str()
                                || candidate.config.as_ref() != server
                            {
                                return self.fail_refresh(
                                    &request_id,
                                    name,
                                    McpCandidateError::Invalid {
                                        cause:
                                            "candidate identity or config changed during admission"
                                                .to_owned(),
                                    },
                                    &predecessor,
                                );
                            }
                            successor.insert(
                                name.clone(),
                                Arc::new(AdmittedServerSlice::from_candidate(
                                    candidate,
                                    provenance.clone(),
                                )),
                            );
                        }
                        Err(error) => {
                            return self.fail_refresh(&request_id, name, error, &predecessor);
                        }
                    }
                }
            }
        }

        if let RegistryScope::Server(name) = &refresh.scope
            && refresh.config.entry(name).is_none()
        {
            successor.remove(name);
        }

        self.publish(&request_id, config_revision, predecessor, successor)
    }

    pub fn publication(&self) -> McpRegistryPublication {
        self.current_snapshot()
            .map(|snapshot| publication_of(&snapshot))
            .unwrap_or_else(|_| publication_of(&Arc::new(AdmittedRegistrySnapshot::empty())))
    }

    pub fn status(&self) -> McpRegistryStatus {
        self.state.read().map_or_else(
            |_| McpRegistryStatus::default(),
            |state| state.status.clone(),
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

    fn current_snapshot(&self) -> Result<Arc<AdmittedRegistrySnapshot>, McpRegistryError> {
        self.state
            .read()
            .map(|state| Arc::clone(&state.snapshot))
            .map_err(|_| McpRegistryError::PublicationUnavailable)
    }

    fn fail_refresh(
        &self,
        request_id: &str,
        server: &McpServerName,
        error: McpCandidateError,
        predecessor: &Arc<AdmittedRegistrySnapshot>,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        if let McpCandidateError::AuthorizationRequired {
            issuer,
            protected_resource,
            scopes,
        } = &error
        {
            self.journal
                .append(McpRegistryEvent::AuthorizationRequired {
                    server: server.to_string(),
                    issuer: issuer.clone(),
                    protected_resource: protected_resource.clone(),
                    scopes: scopes.clone(),
                    request_correlation: request_id.to_owned(),
                })?;
        }
        let retained = predecessor
            .servers
            .contains_key(server)
            .then(|| predecessor.generation.to_string());
        self.journal.append(McpRegistryEvent::RefreshFailed {
            request_id: request_id.to_owned(),
            cause: error.cause(),
            retained_generation: retained.clone(),
        })?;
        let mut state = self
            .state
            .write()
            .map_err(|_| McpRegistryError::PublicationUnavailable)?;
        if retained.is_some() {
            if !state
                .status
                .degraded
                .iter()
                .any(|name| name == server.as_str())
            {
                state.status.degraded.push(server.to_string());
                state.status.degraded.sort();
            }
        } else if !state
            .status
            .absent
            .iter()
            .any(|name| name == server.as_str())
        {
            state.status.absent.push(server.to_string());
            state.status.absent.sort();
        }
        Err(McpRegistryError::Candidate {
            server: server.to_string(),
            cause: error.cause(),
        })
    }

    fn publish(
        &self,
        request_id: &str,
        config_revision: u64,
        predecessor: Arc<AdmittedRegistrySnapshot>,
        servers: BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
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
        let mut effect_tools = Vec::<Arc<dyn Tool>>::new();
        for (name, server) in &servers {
            for definition in server.tools.iter() {
                let tool = match definition.capture(
                    name.as_str(),
                    &generation,
                    Arc::clone(&server.config),
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
                effect_tools.push(tool);
            }
        }
        effect_tools.sort_by_key(|tool| tool.descriptor().name);
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
        let contexts = match capture_contexts(&generation, &servers) {
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
                identity: server.identity.clone(),
                provenance: server.provenance.clone(),
                capabilities: server.capabilities.clone(),
                config: Arc::clone(&server.config),
                cache: server.cache.clone(),
                dynamic_family: server.dynamic_family.clone(),
                factory: Arc::clone(&server.factory),
            })
            .collect::<Vec<_>>();
        let capture = McpRegistryCapture {
            generation: Arc::clone(&generation),
            tools: effect_tools.into(),
            contexts,
            servers: captured_servers.into(),
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
        state.snapshot = snapshot;
        state.status = McpRegistryStatus {
            admitted: publication.admitted_servers.clone(),
            degraded: Vec::new(),
            absent: Vec::new(),
        };
        Ok(publication)
    }

    fn fail_publication<'a>(
        &self,
        request_id: &str,
        cause: String,
        predecessor: &Arc<AdmittedRegistrySnapshot>,
        attempted_servers: impl Iterator<Item = &'a McpServerName>,
    ) -> Result<McpRegistryPublication, McpRegistryError> {
        let retained_generation =
            (predecessor.number > 0).then(|| predecessor.generation.to_string());
        self.journal.append(McpRegistryEvent::RefreshFailed {
            request_id: request_id.to_owned(),
            cause: cause.clone(),
            retained_generation,
        })?;

        let attempted_servers = attempted_servers
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let mut state = self
            .state
            .write()
            .map_err(|_| McpRegistryError::PublicationUnavailable)?;
        state.status.admitted = predecessor
            .servers
            .keys()
            .map(ToString::to_string)
            .collect();
        state.status.degraded = state.status.admitted.clone();
        state.status.absent = attempted_servers
            .into_iter()
            .filter(|name| {
                !predecessor
                    .servers
                    .keys()
                    .any(|server| server.as_str() == name.as_str())
            })
            .collect();
        Err(McpRegistryError::InvalidCandidate(cause))
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
    generation: &str,
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
            .and_then(|entry| entry.with_admission_generation(generation))
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            let subject = McpSubject::Resource {
                server: server_name.to_string(),
                uri: resource.uri.clone(),
            };
            let admission = McpAdmissionFingerprint::new(
                generation,
                format!("{server_name}/{}", resource.uri),
                descriptor,
            )
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            operations.push(McpCapturedContextOperation {
                entry,
                subject,
                operation: McpOperationKind::ReadResource,
                admission,
                config: Arc::clone(&server.config),
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
            .and_then(|entry| entry.with_admission_generation(generation))
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            let subject = McpSubject::Prompt {
                server: server_name.to_string(),
                name: prompt.name.clone(),
            };
            let admission = McpAdmissionFingerprint::new(
                generation,
                format!("{server_name}/{}", prompt.name),
                descriptor,
            )
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
            operations.push(McpCapturedContextOperation {
                entry,
                subject,
                operation: McpOperationKind::GetPrompt,
                admission,
                config: Arc::clone(&server.config),
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
