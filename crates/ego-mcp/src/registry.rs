//! Serialized MCP reconciliation and immutable admitted snapshots.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use ego_core::config::{
    McpConfigProvenance, McpServerConfig, McpServerName, ResolvedMcpConfig, ResolvedMcpEntry,
};
use ego_core::turn::{
    AdmittedToolSource, CapturedToolCatalogue, McpRegistryEvent, McpRegistryJournal,
    McpRegistryJournalError, RegisteredTool, ToolCatalogueError,
};
use rmcp::model::{Prompt, Resource};
use sha2::{Digest, Sha256};

use crate::McpFuture;

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
    pub fn tools() -> Self {
        Self {
            tools: true,
            resources: false,
            prompts: false,
        }
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
pub struct McpAdmittedCandidate {
    name: Arc<str>,
    identity: McpCandidateIdentity,
    capabilities: McpCapabilities,
    tools: Arc<[RegisteredTool]>,
    resources: Arc<[Resource]>,
    prompts: Arc<[Prompt]>,
    cache: McpCacheMetadata,
}

impl McpAdmittedCandidate {
    pub fn new(
        name: String,
        identity: McpCandidateIdentity,
        capabilities: McpCapabilities,
        tools: Vec<RegisteredTool>,
        resources: Vec<Resource>,
        prompts: Vec<Prompt>,
        cache: McpCacheMetadata,
    ) -> Result<Self, McpRegistryError> {
        if name.is_empty() || identity.as_str().is_empty() || name != identity.as_str() {
            return Err(McpRegistryError::InvalidCandidate(
                "MCP candidate identity does not match its configured name".to_owned(),
            ));
        }
        CapturedToolCatalogue::new("candidate", tools.clone())
            .map_err(|error| McpRegistryError::InvalidCandidate(error.to_string()))?;
        Ok(Self {
            name: Arc::from(name),
            identity,
            capabilities,
            tools: tools.into(),
            resources: resources.into(),
            prompts: prompts.into(),
            cache,
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
    _identity: McpCandidateIdentity,
    _provenance: McpConfigProvenance,
    _capabilities: McpCapabilities,
    tools: Arc<[RegisteredTool]>,
    _resources: Arc<[Resource]>,
    _prompts: Arc<[Prompt]>,
    _cache: McpCacheMetadata,
}

impl AdmittedServerSlice {
    fn from_candidate(candidate: McpAdmittedCandidate, provenance: McpConfigProvenance) -> Self {
        Self {
            _identity: candidate.identity,
            _provenance: provenance,
            _capabilities: candidate.capabilities,
            tools: candidate.tools,
            _resources: candidate.resources,
            _prompts: candidate.prompts,
            _cache: candidate.cache,
        }
    }
}

struct AdmittedRegistrySnapshot {
    number: u64,
    generation: Arc<str>,
    config_revision: u64,
    servers: BTreeMap<McpServerName, Arc<AdmittedServerSlice>>,
    tools: Arc<[RegisteredTool]>,
}

impl AdmittedRegistrySnapshot {
    fn empty() -> Self {
        Self {
            number: 0,
            generation: Arc::from("generation-0"),
            config_revision: 0,
            servers: BTreeMap::new(),
            tools: Arc::from(Vec::<RegisteredTool>::new()),
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
                            if candidate.name.as_ref() != name.as_str() {
                                return self.fail_refresh(
                                    &request_id,
                                    name,
                                    McpCandidateError::Invalid {
                                        cause: "candidate identity changed during admission"
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
        let mut tools = servers
            .values()
            .flat_map(|server| server.tools.iter().cloned())
            .collect::<Vec<_>>();
        tools.sort_by_key(|tool| tool.descriptor().name);
        if let Err(error) = CapturedToolCatalogue::new(generation.to_string(), tools.clone()) {
            return self.fail_publication(
                request_id,
                error.to_string(),
                &predecessor,
                servers.keys(),
            );
        }
        let snapshot = Arc::new(AdmittedRegistrySnapshot {
            number,
            generation: Arc::clone(&generation),
            config_revision,
            servers,
            tools: tools.into(),
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
