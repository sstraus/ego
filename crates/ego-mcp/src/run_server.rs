use std::collections::BTreeSet;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use ego_core::ceiling::{Budget, Ceilings};
use ego_core::compact::{CompactionCommand, CompactionReceipt, CompactionTransitionRequest};
use ego_core::config::{Mode, ModelRef};
use ego_core::event::id::SessionId;
use ego_core::event::time::Clock;
use ego_core::generator::types::{ThinkingLevel, ToolDescriptor};
use ego_core::run::{
    CancelRequest, ContinueRequest, EventSinkFactory, IdSource, RequestDigest, RequestId,
    RunCommandOutcome, RunController, RunError, RunExecution, RunExecutionBindings, RunExecutor,
    RunId, RunRecoveryOutcome, RunRoot, RunSelections, RunSnapshot, RunStatus, StartRequest,
};
use ego_core::store::session_store::SessionStore;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{
    McpError, McpFailure, McpFuture, McpServerCallOutcome, McpServerPort,
    McpServerResourceContents, McpServerResourceDescriptor, McpServerToolCall,
};

pub trait McpRunRootResolver: Send + Sync {
    fn resolve(&self, raw: &Path) -> Result<RunRoot, McpError>;
}

pub trait McpRunExecutionSpawner: Send + Sync {
    fn spawn(
        &self,
        future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<(), McpError>;
}

pub trait McpRunBindingsBuilder: Send + Sync {
    fn build_sink(
        &self,
        session_id: SessionId,
        request_state: Option<&str>,
    ) -> Result<Arc<dyn EventSinkFactory>, McpError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRunResourceRequest {
    Summary { run_id: RunId },
    Artifact { run_id: RunId, artifact_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SanitizedMcpRunResource {
    Text {
        mime_type: Arc<str>,
        text: Arc<str>,
    },
    Blob {
        mime_type: Arc<str>,
        bytes: Arc<[u8]>,
    },
}

pub trait McpRunResourceReader: Send + Sync {
    fn read<'a>(
        &'a self,
        request: McpRunResourceRequest,
    ) -> McpFuture<'a, Result<SanitizedMcpRunResource, McpError>>;
}

pub struct McpRunServerPorts {
    root_resolver: Arc<dyn McpRunRootResolver>,
    execution_spawner: Arc<dyn McpRunExecutionSpawner>,
    bindings_builder: Arc<dyn McpRunBindingsBuilder>,
    resource_reader: Arc<dyn McpRunResourceReader>,
    compaction: Option<Arc<dyn CompactionCommand>>,
}

impl McpRunServerPorts {
    pub fn new(
        root_resolver: Arc<dyn McpRunRootResolver>,
        execution_spawner: Arc<dyn McpRunExecutionSpawner>,
        bindings_builder: Arc<dyn McpRunBindingsBuilder>,
        resource_reader: Arc<dyn McpRunResourceReader>,
    ) -> Self {
        Self {
            root_resolver,
            execution_spawner,
            bindings_builder,
            resource_reader,
            compaction: None,
        }
    }

    pub fn with_compaction(mut self, compaction: Arc<dyn CompactionCommand>) -> Self {
        self.compaction = Some(compaction);
        self
    }
}

pub struct McpRunSelectionPolicy {
    allowed_roots: BTreeSet<PathBuf>,
    allowed_models: BTreeSet<String>,
    max_thinking: ThinkingLevel,
    allowed_modes: BTreeSet<String>,
    ceilings: Ceilings,
    allowed_profiles: BTreeSet<String>,
}

impl McpRunSelectionPolicy {
    pub fn new(
        allowed_roots: BTreeSet<RunRoot>,
        allowed_models: BTreeSet<ModelRef>,
        max_thinking: ThinkingLevel,
        allowed_modes: BTreeSet<Mode>,
        ceilings: Ceilings,
        allowed_profiles: BTreeSet<String>,
    ) -> Result<Self, McpError> {
        let allowed_roots = allowed_roots
            .into_iter()
            .map(|root| normalize_absolute(root.as_path()))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let allowed_models = normalize_names(allowed_models.into_iter().map(|model| model.0))?;
        let allowed_modes = allowed_modes
            .into_iter()
            .map(|mode| mode.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let allowed_profiles = normalize_names(allowed_profiles)?;
        if allowed_roots.is_empty() || allowed_models.is_empty() || allowed_modes.is_empty() {
            return Err(invalid("invalid empty MCP run selection policy"));
        }
        if ceilings.money.is_none() && ceilings.tokens.is_none() {
            return Err(invalid(
                "invalid MCP run selection policy without a budget ceiling",
            ));
        }
        Ok(Self {
            allowed_roots,
            allowed_models,
            max_thinking,
            allowed_modes,
            ceilings,
            allowed_profiles,
        })
    }

    fn validate(&self, request: &mut RunToolRequest) -> Result<(), McpError> {
        request.model = request.model.take().map(normalize_name).transpose()?;
        request.profile = request.profile.take().map(normalize_name).transpose()?;
        if request
            .model
            .as_ref()
            .is_some_and(|model| !self.allowed_models.contains(model))
        {
            return Err(authority("requested model is not allowed"));
        }
        if request
            .thinking
            .is_some_and(|level| level > self.max_thinking)
        {
            return Err(authority(
                "requested thinking exceeds the configured ceiling",
            ));
        }
        if request
            .mode
            .is_some_and(|mode| !self.allowed_modes.contains(mode.as_str()))
        {
            return Err(authority("requested mode is not allowed"));
        }
        if request
            .budget
            .as_ref()
            .is_some_and(|budget| !budget_within(budget, &self.ceilings))
        {
            return Err(authority("requested budget exceeds the configured ceiling"));
        }
        if request
            .profile
            .as_ref()
            .is_some_and(|profile| !self.allowed_profiles.contains(profile))
        {
            return Err(authority("requested profile is not allowed"));
        }
        Ok(())
    }
}

pub struct McpRunServerConfig {
    policy: McpRunSelectionPolicy,
    max_concurrent_runs: NonZeroUsize,
    max_resource_bytes: NonZeroUsize,
}

impl McpRunServerConfig {
    pub fn new(
        policy: McpRunSelectionPolicy,
        max_concurrent_runs: NonZeroUsize,
        max_resource_bytes: NonZeroUsize,
    ) -> Result<Self, McpError> {
        Ok(Self {
            policy,
            max_concurrent_runs,
            max_resource_bytes,
        })
    }

    pub fn policy(&self) -> &McpRunSelectionPolicy {
        &self.policy
    }

    pub const fn max_concurrent_runs(&self) -> NonZeroUsize {
        self.max_concurrent_runs
    }

    pub const fn max_resource_bytes(&self) -> NonZeroUsize {
        self.max_resource_bytes
    }
}

pub struct McpRunServerFactory<S, E, C, I> {
    controller: Arc<RunController<S, E, C, I>>,
    config: Arc<McpRunServerConfig>,
    ports: Arc<McpRunServerPorts>,
}

impl<S, E, C, I> McpRunServerFactory<S, E, C, I> {
    pub fn new(
        controller: Arc<RunController<S, E, C, I>>,
        config: Arc<McpRunServerConfig>,
        ports: McpRunServerPorts,
    ) -> Self {
        Self {
            controller,
            config,
            ports: Arc::new(ports),
        }
    }

    pub fn new_connection(&self) -> McpRunServer<S, E, C, I> {
        McpRunServer {
            controller: Arc::clone(&self.controller),
            config: Arc::clone(&self.config),
            ports: Arc::clone(&self.ports),
            visible_runs: Mutex::new(BTreeSet::new()),
        }
    }
}

pub struct McpRunServer<S, E, C, I> {
    controller: Arc<RunController<S, E, C, I>>,
    config: Arc<McpRunServerConfig>,
    ports: Arc<McpRunServerPorts>,
    visible_runs: Mutex<BTreeSet<RunId>>,
}

impl<S, E, C, I> McpRunServer<S, E, C, I>
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    async fn run(&self, call: &McpServerToolCall) -> Result<RunSnapshot, McpError> {
        let mut request: RunToolRequest = decode(call.arguments())?;
        self.config.policy.validate(&mut request)?;
        let resolved_root = self.ports.root_resolver.resolve(Path::new(&request.root))?;
        let normalized_root = normalize_absolute(resolved_root.as_path())?;
        if !self.config.policy.allowed_roots.contains(&normalized_root) {
            return Err(authority("requested root is not allowed"));
        }
        let root = RunRoot::new(normalized_root).map_err(run_error)?;
        let request_id = parse_request_id(&request.request_id)?;
        let bindings = self.bindings(RequestId::session_id(request_id), call.request_state())?;
        let digest = request_digest(call.arguments())?;
        let selections = RunSelections::new(
            request.model.map(ModelRef),
            request.thinking,
            request.mode,
            request.budget,
            request.profile,
        );
        let start = StartRequest::new(root, request_id, digest, selections, request.prompt);
        match self
            .controller
            .start(start, bindings.clone())
            .await
            .map_err(run_error)?
        {
            RunCommandOutcome::Admitted(admission) => {
                let snapshot = admission.snapshot().clone();
                self.schedule(admission.into_execution())?;
                self.grant(snapshot.run_id());
                Ok(snapshot)
            }
            RunCommandOutcome::Replayed(observation) => {
                let snapshot = observation.snapshot().clone();
                if self.is_visible(snapshot.run_id()) {
                    return Ok(snapshot);
                }
                if matches!(
                    snapshot.status(),
                    RunStatus::Running | RunStatus::Cancelling
                ) {
                    match self
                        .controller
                        .recover(snapshot.run_id(), bindings)
                        .await
                        .map_err(run_error)?
                    {
                        RunRecoveryOutcome::Reattached(reattachment) => {
                            let snapshot = reattachment.snapshot().clone();
                            self.schedule(reattachment.into_execution())?;
                            self.grant(snapshot.run_id());
                            Ok(snapshot)
                        }
                        RunRecoveryOutcome::Observed(observed) => {
                            let snapshot = observed.snapshot().clone();
                            self.grant(snapshot.run_id());
                            Ok(snapshot)
                        }
                    }
                } else {
                    self.grant(snapshot.run_id());
                    Ok(snapshot)
                }
            }
        }
    }

    async fn continue_run(&self, call: &McpServerToolCall) -> Result<RunSnapshot, McpError> {
        let request: ContinueToolRequest = decode(call.arguments())?;
        let run_id = parse_run_id(&request.run_id)?;
        self.require_visible(run_id)?;
        let bindings = self.bindings(run_id.session_id(), call.request_state())?;
        let command = ContinueRequest::new(
            run_id,
            parse_request_id(&request.request_id)?,
            request_digest(call.arguments())?,
            ego_core::run::RunRevision::new(request.expected_revision),
            request.prompt,
        );
        match self
            .controller
            .continue_run(command, bindings)
            .await
            .map_err(run_error)?
        {
            RunCommandOutcome::Admitted(admission) => {
                let snapshot = admission.snapshot().clone();
                self.schedule(admission.into_execution())?;
                Ok(snapshot)
            }
            RunCommandOutcome::Replayed(observation) => Ok(observation.snapshot().clone()),
        }
    }

    async fn cancel(&self, arguments: &Map<String, Value>) -> Result<RunSnapshot, McpError> {
        let request: CancelToolRequest = decode(arguments)?;
        let run_id = parse_run_id(&request.run_id)?;
        self.require_visible(run_id)?;
        self.controller
            .cancel(CancelRequest::new(
                run_id,
                parse_request_id(&request.request_id)?,
                request_digest(arguments)?,
                ego_core::run::RunRevision::new(request.expected_revision),
            ))
            .await
            .map_err(run_error)
    }

    async fn status(&self, arguments: &Map<String, Value>) -> Result<RunSnapshot, McpError> {
        let request: StatusToolRequest = decode(arguments)?;
        let run_id = parse_run_id(&request.run_id)?;
        self.require_visible(run_id)?;
        self.controller.status(run_id).await.map_err(run_error)
    }

    async fn compact(
        &self,
        arguments: &Map<String, Value>,
    ) -> Result<McpServerCallOutcome, McpError> {
        let request: CompactToolRequest = decode(arguments)?;
        let request_id = parse_request_id(&request.request_id)?;
        if request_id.uuid().is_nil() {
            return Err(invalid("invalid nil MCP compaction request id"));
        }
        let run_id = parse_run_id(&request.run_id)?;
        let result = self
            .compact_visible(
                request_id,
                run_id,
                ego_core::run::RunRevision::new(request.expected_revision),
            )
            .await;
        match result {
            Ok(receipt) => {
                let text = serde_json::to_string(&McpCompactResponse::from(receipt))?;
                Ok(McpServerCallOutcome::success(
                    ego_core::permit::ToolResult::builtin(text),
                ))
            }
            Err(error) => {
                let text = serde_json::to_string(&json!({
                    "v": 1,
                    "isError": true,
                    "error": error.to_string(),
                }))?;
                Ok(McpServerCallOutcome::error(
                    ego_core::permit::ToolResult::builtin(text),
                ))
            }
        }
    }

    async fn compact_visible(
        &self,
        request_id: RequestId,
        run_id: RunId,
        expected_revision: ego_core::run::RunRevision,
    ) -> Result<CompactionReceipt, McpError> {
        self.require_visible(run_id)?;
        let snapshot = self.controller.status(run_id).await.map_err(run_error)?;
        if snapshot.revision() != expected_revision {
            return Err(authority("compaction expected_revision is stale"));
        }
        if !matches!(
            snapshot.settlement(),
            Some(ego_core::run::RunSettlement::ContextOverflow)
        ) {
            return Err(authority(
                "compaction requires a settled context_overflow run",
            ));
        }
        let compaction = self
            .ports
            .compaction
            .as_ref()
            .ok_or_else(|| authority("compaction is unavailable in this server composition"))?;
        compaction
            .compact(CompactionTransitionRequest {
                request_id,
                source: run_id.session_id(),
            })
            .await
            .map_err(|error| McpError::new(McpFailure::Protocol, error.to_string()))
    }

    fn bindings(
        &self,
        session_id: SessionId,
        request_state: Option<&str>,
    ) -> Result<RunExecutionBindings, McpError> {
        Ok(RunExecutionBindings::new(
            self.ports
                .bindings_builder
                .build_sink(session_id, request_state)?,
            None,
        ))
    }

    fn schedule(&self, execution: RunExecution) -> Result<(), McpError> {
        let controller = Arc::clone(&self.controller);
        self.ports.execution_spawner.spawn(Box::pin(async move {
            let _ = controller.execute(execution).await;
        }))
    }

    fn grant(&self, run_id: RunId) {
        self.visible_runs
            .lock()
            .expect("MCP run visibility")
            .insert(run_id);
    }

    fn is_visible(&self, run_id: RunId) -> bool {
        self.visible_runs
            .lock()
            .expect("MCP run visibility")
            .contains(&run_id)
    }

    fn require_visible(&self, run_id: RunId) -> Result<(), McpError> {
        self.is_visible(run_id)
            .then_some(())
            .ok_or_else(|| absent("run is absent from this MCP connection"))
    }

    async fn resource(&self, uri: &str) -> Result<McpServerResourceContents, McpError> {
        if uri == "ego://capabilities" {
            let text = self.capabilities()?;
            self.enforce_resource_limit(text.len())?;
            return Ok(McpServerResourceContents {
                uri: uri.to_owned(),
                mime_type: "application/json".to_owned(),
                text,
                blob: None,
            });
        }
        let request = parse_resource_uri(uri)?;
        let run_id = match &request {
            McpRunResourceRequest::Summary { run_id }
            | McpRunResourceRequest::Artifact { run_id, .. } => *run_id,
        };
        self.require_visible(run_id)?;
        let sanitized = self.ports.resource_reader.read(request).await?;
        match sanitized {
            SanitizedMcpRunResource::Text { mime_type, text } => {
                self.enforce_resource_limit(text.len())?;
                Ok(McpServerResourceContents {
                    uri: uri.to_owned(),
                    mime_type: mime_type.to_string(),
                    text: text.to_string(),
                    blob: None,
                })
            }
            SanitizedMcpRunResource::Blob { mime_type, bytes } => {
                self.enforce_resource_limit(bytes.len())?;
                Ok(McpServerResourceContents {
                    uri: uri.to_owned(),
                    mime_type: mime_type.to_string(),
                    text: String::new(),
                    blob: Some(base64(&bytes)),
                })
            }
        }
    }

    fn enforce_resource_limit(&self, bytes: usize) -> Result<(), McpError> {
        if bytes > self.config.max_resource_bytes.get() {
            return Err(authority("sanitized MCP resource exceeds the byte ceiling"));
        }
        Ok(())
    }

    fn capabilities(&self) -> Result<String, McpError> {
        let policy = &self.config.policy;
        let roots = policy
            .allowed_roots
            .iter()
            .map(|root| {
                root.to_str()
                    .expect("MCP policy roots are validated as Unicode")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        let models = policy.allowed_models.iter().cloned().collect::<Vec<_>>();
        let modes = policy.allowed_modes.iter().cloned().collect::<Vec<_>>();
        let profiles = policy.allowed_profiles.iter().cloned().collect::<Vec<_>>();
        let tools = if self.ports.compaction.is_some() {
            vec!["run", "continue", "cancel", "status", "compact"]
        } else {
            vec!["run", "continue", "cancel", "status"]
        };
        serde_json::to_string(&json!({
            "tools": tools,
            "resources": [
                "ego://capabilities",
                "ego://runs/{run_id}/summary",
                "ego://runs/{run_id}/artifacts/{artifact_id}"
            ],
            "selection_policy": {
                "allowed_roots": roots,
                "allowed_models": models,
                "max_thinking": policy.max_thinking.as_str(),
                "allowed_modes": modes,
                "ceilings": {
                    "money": policy.ceilings.money,
                    "tokens": policy.ceilings.tokens
                },
                "allowed_profiles": profiles
            },
            "max_concurrent_runs": self.config.max_concurrent_runs.get(),
            "max_resource_bytes": self.config.max_resource_bytes.get()
        }))
        .map_err(Into::into)
    }
}

impl<S, E, C, I> McpServerPort for McpRunServer<S, E, C, I>
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    fn list_tools<'a>(&'a self) -> McpFuture<'a, Vec<ToolDescriptor>> {
        Box::pin(async { tool_descriptors(self.ports.compaction.is_some()) })
    }

    fn call_tool<'a>(
        &'a self,
        call: McpServerToolCall,
    ) -> McpFuture<'a, Result<ego_core::permit::ToolResult, McpError>> {
        Box::pin(async move {
            self.call_outcome(call)
                .await
                .map(McpServerCallOutcome::into_result)
        })
    }

    fn call_tool_outcome<'a>(
        &'a self,
        call: McpServerToolCall,
    ) -> McpFuture<'a, Result<McpServerCallOutcome, McpError>> {
        Box::pin(async move { self.call_outcome(call).await })
    }

    fn list_resources<'a>(&'a self) -> McpFuture<'a, Vec<McpServerResourceDescriptor>> {
        Box::pin(async {
            vec![
                McpServerResourceDescriptor {
                    uri: "ego://capabilities".to_owned(),
                    name: "ego.capabilities".to_owned(),
                    description: "Sanitized MCP run server capabilities".to_owned(),
                    mime_type: Some("application/json".to_owned()),
                },
                McpServerResourceDescriptor {
                    uri: "ego://runs/{run_id}/summary".to_owned(),
                    name: "ego.run.summary".to_owned(),
                    description: "Sanitized run summary".to_owned(),
                    mime_type: Some("application/json".to_owned()),
                },
                McpServerResourceDescriptor {
                    uri: "ego://runs/{run_id}/artifacts/{artifact_id}".to_owned(),
                    name: "ego.run.artifact".to_owned(),
                    description: "Sanitized run artifact".to_owned(),
                    mime_type: None,
                },
            ]
        })
    }

    fn read_resource<'a>(
        &'a self,
        uri: &'a str,
    ) -> McpFuture<'a, Result<McpServerResourceContents, McpError>> {
        Box::pin(async move { self.resource(uri).await })
    }
}

impl<S, E, C, I> McpRunServer<S, E, C, I>
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    async fn call_outcome(
        &self,
        call: McpServerToolCall,
    ) -> Result<McpServerCallOutcome, McpError> {
        if call.name() == "compact" {
            return self.compact(call.arguments()).await;
        }
        let result = match call.name() {
            "run" => self.run(&call).await,
            "continue" => self.continue_run(&call).await,
            "cancel" => self.cancel(call.arguments()).await,
            "status" => self.status(call.arguments()).await,
            _ => Err(invalid("invalid MCP run tool name")),
        }?;
        let text = serialize_run_snapshot(&result)?;
        Ok(McpServerCallOutcome::success(
            ego_core::permit::ToolResult::builtin(text),
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunToolRequest {
    request_id: String,
    root: String,
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking: Option<ThinkingLevel>,
    #[serde(default)]
    mode: Option<Mode>,
    #[serde(default)]
    budget: Option<Budget>,
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContinueToolRequest {
    request_id: String,
    run_id: String,
    expected_revision: u64,
    prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelToolRequest {
    request_id: String,
    run_id: String,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusToolRequest {
    run_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactToolRequest {
    request_id: String,
    run_id: String,
    expected_revision: u64,
}

#[derive(serde::Serialize)]
struct McpCompactResponse {
    v: u32,
    source_session_id: SessionId,
    source_seq: ego_core::event::id::Seq,
    target_session_id: SessionId,
    publication: ego_core::session::TargetPublication,
    successor_start_request_id: RequestId,
}

impl From<CompactionReceipt> for McpCompactResponse {
    fn from(receipt: CompactionReceipt) -> Self {
        Self {
            v: 1,
            source_session_id: receipt.source,
            source_seq: receipt.source_seq,
            target_session_id: receipt.target,
            publication: receipt.publication,
            successor_start_request_id: receipt.successor_start_request_id,
        }
    }
}

#[derive(serde::Serialize)]
struct McpRunSnapshotResponse {
    run_id: String,
    root: String,
    revision: u64,
    status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    settlement: Option<ego_core::run::RunSettlement>,
}

impl TryFrom<&RunSnapshot> for McpRunSnapshotResponse {
    type Error = McpError;

    fn try_from(snapshot: &RunSnapshot) -> Result<Self, Self::Error> {
        let root = snapshot.root();
        let root = root
            .as_path()
            .to_str()
            .ok_or_else(|| invalid("invalid non-Unicode MCP run root"))?
            .to_owned();
        Ok(Self {
            run_id: snapshot.run_id().to_string(),
            root,
            revision: snapshot.revision().get(),
            status: snapshot.status(),
            settlement: snapshot.settlement().cloned(),
        })
    }
}

/// Encode one controller snapshot through the public MCP projection.
pub fn serialize_run_snapshot(snapshot: &RunSnapshot) -> Result<String, McpError> {
    serde_json::to_string(&McpRunSnapshotResponse::try_from(snapshot)?).map_err(McpError::from)
}

fn decode<T>(arguments: &Map<String, Value>) -> Result<T, McpError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(Value::Object(arguments.clone()))
        .map_err(|error| invalid(format!("invalid MCP run request: {error}")))
}

fn parse_request_id(raw: &str) -> Result<RequestId, McpError> {
    raw.parse()
        .map_err(|_| invalid("invalid MCP run request id"))
}

fn parse_run_id(raw: &str) -> Result<RunId, McpError> {
    raw.parse().map_err(|_| invalid("invalid MCP run id"))
}

fn request_digest(request: &impl serde::Serialize) -> Result<RequestDigest, McpError> {
    let bytes = serde_json::to_vec(request)?;
    let digest = format!("{:x}", Sha256::digest(bytes));
    RequestDigest::parse(&digest).map_err(run_error)
}

fn normalize_names(values: impl IntoIterator<Item = String>) -> Result<BTreeSet<String>, McpError> {
    values.into_iter().map(normalize_name).collect()
}

fn normalize_name(value: String) -> Result<String, McpError> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return Err(invalid("invalid empty MCP run selection"));
    }
    Ok(normalized.to_owned())
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, McpError> {
    if !path.is_absolute() {
        return Err(invalid("invalid non-absolute MCP run root"));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid("invalid MCP run root traversal"));
                }
            }
        }
    }
    if normalized.to_str().is_none() {
        return Err(invalid("invalid non-Unicode MCP run root"));
    }
    Ok(normalized)
}

fn budget_within(requested: &Budget, ceilings: &Ceilings) -> bool {
    match requested {
        Budget::Tokens(requested) => ceilings.tokens.is_some_and(|maximum| *requested <= maximum),
        Budget::Money(requested) => ceilings.money.is_some_and(|maximum| {
            requested.currency == maximum.currency && requested.amount <= maximum.amount
        }),
    }
}

fn parse_resource_uri(uri: &str) -> Result<McpRunResourceRequest, McpError> {
    let tail = uri
        .strip_prefix("ego://runs/")
        .ok_or_else(|| absent("MCP run resource is absent"))?;
    if let Some((run_id, artifact_id)) = tail.split_once("/artifacts/") {
        if artifact_id.is_empty() || artifact_id.contains('/') {
            return Err(absent("MCP run artifact is absent"));
        }
        return Ok(McpRunResourceRequest::Artifact {
            run_id: parse_run_id(run_id).map_err(|_| absent("MCP run resource is absent"))?,
            artifact_id: artifact_id.to_owned(),
        });
    }
    let run_id = tail
        .strip_suffix("/summary")
        .ok_or_else(|| absent("MCP run resource is absent"))?;
    if run_id.is_empty() || run_id.contains('/') {
        return Err(absent("MCP run resource is absent"));
    }
    Ok(McpRunResourceRequest::Summary {
        run_id: parse_run_id(run_id).map_err(|_| absent("MCP run resource is absent"))?,
    })
}

fn tool_descriptors(compaction: bool) -> Vec<ToolDescriptor> {
    let mut tools = vec![
        descriptor(
            "run",
            "Start or exactly replay an ego run",
            json!({
                "type":"object", "additionalProperties":false,
                "required":["request_id","root","prompt"],
                "properties":{
                    "request_id":{"type":"string","format":"uuid"},
                    "root":{"type":"string"}, "prompt":{"type":"string"},
                    "model":{"type":"string"},
                    "thinking":{"enum":["off","minimal","low","medium","high","xhigh","max"]},
                    "mode":{"enum":["plan","default","auto","edits","yolo"]},
                    "budget":{"type":"string"}, "profile":{"type":"string"}
                }
            }),
        ),
        descriptor(
            "continue",
            "Continue a visible settled ego run",
            mutation_schema(true),
        ),
        descriptor("cancel", "Cancel a visible ego run", mutation_schema(false)),
        descriptor(
            "status",
            "Read the status of a visible ego run",
            json!({
                "type":"object", "additionalProperties":false,
                "required":["run_id"], "properties":{"run_id":{"type":"string"}}
            }),
        ),
    ];
    if compaction {
        tools.push(descriptor(
            "compact",
            "Compact a visible context-overflow run into its deterministic successor",
            mutation_schema(false),
        ));
    }
    tools
}

fn mutation_schema(prompt: bool) -> Value {
    let mut required = vec!["request_id", "run_id", "expected_revision"];
    let mut properties = json!({
        "request_id":{"type":"string","format":"uuid"},
        "run_id":{"type":"string"}, "expected_revision":{"type":"integer","minimum":0}
    });
    if prompt {
        required.push("prompt");
        properties
            .as_object_mut()
            .expect("schema properties")
            .insert("prompt".to_owned(), json!({"type":"string"}));
    }
    json!({
        "type":"object", "additionalProperties":false,
        "required":required, "properties":properties
    })
}

fn descriptor(name: &str, description: &str, parameters: Value) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        encoded.push(ALPHABET[((word >> 18) & 63) as usize] as char);
        encoded.push(ALPHABET[((word >> 12) & 63) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((word >> 6) & 63) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(word & 63) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

fn run_error(error: RunError) -> McpError {
    if matches!(&error, RunError::RequestIdMismatch { .. }) {
        return McpError::new(
            McpFailure::Protocol,
            format!("request digest does not match the retained command: {error}"),
        );
    }
    let failure = match &error {
        RunError::NotFound { .. } => McpFailure::Absent,
        RunError::InvalidRequestDigest | RunError::InvalidRoot { .. } => McpFailure::InvalidValue,
        _ => McpFailure::Protocol,
    };
    McpError::new(failure, error.to_string())
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::InvalidValue, message)
}

fn authority(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Authority, message)
}

fn absent(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Absent, message)
}
