//! `ego acp`: production composition for the inbound ACP adapter.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Stdio};
use agent_client_protocol_schema::v1;
use ego_acp::auth::Credential;
use ego_acp::serve::{
    AcpRunBindingsBuilder, McpAdmissionContext, McpSessionAdmissionError,
    McpSessionAdmissionFuture, McpSessionAdmitter, SessionConfinementAdmission,
    SessionMcpAdmission, SessionTrustAdmission,
};
use ego_core::config::{
    McpConfigProvenance, McpEnvironmentName, McpHttpAuth, McpServerConfig, McpServerName,
    ProfileName, Source, UserConfigTarget,
};
use ego_core::control::SessionControls;
use ego_core::credentials::SecretMaterial;
use ego_core::endpoint::Admitted;
use ego_core::event::domain::CancelCause;
use ego_core::evidence::EvidenceAdmissionService;
use ego_core::mcp::McpRuntimeBinding;
use ego_core::permit::{CanonPath, Confinement, ExecObservation, RunConfinement, SpawnSpec};
use ego_core::run::RunRoot;
use ego_core::run::{RunController, RunRetention, RunTermination};
use ego_core::session_lifecycle::{SessionLifecycle, SessionRunLifecycle};
use ego_core::suspension::ContinuationJournal;
use ego_core::turn::{AdmittedToolSource, CancelToken, CapturedToolCatalogue, ToolCatalogueError};
use ego_mcp::{
    MCP_DEFAULT_MAX_SSE_EVENT_SIZE, MCP_DEFAULT_OPERATION_TIMEOUT, McpAcpEffects, McpDuplexEffects,
    McpDuplexEffectsFactory, McpEnvironmentBinding, McpError, McpFailure, McpHeaderBinding,
    McpHttpEffects, McpRegistryCandidate, McpRegistryRefresh, McpRegistryRuntime,
    McpServerAdmission, McpTransportMaterial, RegistryScope, RegistryTrigger,
};
use ego_tools::clock::SystemClock;
use ego_tools::credentials::{OsGenerationPolicyCaptureStore, SessionResumeKey};
use ego_tools::exec::{ExecLimits, RunEffectPorts};
use ego_tools::mcp::{RunMcpDuplexEffectsFactory, RunMcpHttpEffects};
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::OsIo;
use futures::future::BoxFuture;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::acp_confinement::CliSessionConfinementAdmission;
use crate::acp_controls::AcpSessionControls;
use crate::acp_permission::CliAcpPermissionRules;
use crate::acp_trust::CliSessionTrustAdmission;
use crate::cli::AcpArgs;
use crate::confinement_diagnostics::Reporter;
use crate::host_ports::{CanonicalRoots, TokioExecutionSpawner, UuidIds};
use crate::provider_http::{CommandHttpScope, ProviderHttpAdapter};
use crate::{run, session};

/// The confined effect scope one session's stdio MCP processes run in. It is
/// cancelled when the session closes, so no process outlives its session.
struct SessionMcpScope {
    confinement: Confinement,
    cancel: CancelToken,
    _effects: CommandHttpScope,
    stdio: Arc<ego_mcp::McpStdioSession>,
}

impl SessionMcpScope {
    async fn open(
        root: &RunRoot,
        session: ego_core::event::id::SessionId,
        confinement: Confinement,
        observation: Option<&ExecObservation>,
    ) -> Result<Self, McpSessionAdmissionError> {
        let cancel = CancelToken::new();
        let run_confinement =
            RunConfinement::new(confinement, false, CanonPath::new(root.as_path()))
                .map_err(|error| safe_mcp_error(error.to_string()))?;
        let effects =
            CommandHttpScope::confined(run_confinement, cancel.clone(), None, observation)
                .await
                .map_err(|error| {
                    safe_mcp_error(format!(
                        "the confined MCP effect scope could not be created: {error}"
                    ))
                })?;
        let stdio = ego_mcp::McpStdioSession::new(
            session,
            Arc::new(RunMcpDuplexEffectsFactory::new(
                effects.ports().duplex_handle(),
            )),
            effects.ports().process_handle(),
            confinement,
        );
        Ok(Self {
            confinement,
            cancel,
            stdio,
            _effects: effects,
        })
    }

    fn stdio_management(&self) -> Arc<dyn McpDuplexEffectsFactory> {
        self.stdio.clone()
    }
}

impl Drop for SessionMcpScope {
    fn drop(&mut self) {
        self.cancel.cancel(CancelCause::Host);
    }
}

/// The stdio side of a session that supplied no stdio server: every attempt
/// to start a process is refused.
struct NoStdioEffects;

impl McpDuplexEffectsFactory for NoStdioEffects {
    fn create(
        &self,
        _server: McpServerName,
        _spec: SpawnSpec,
    ) -> Result<Box<dyn McpDuplexEffects>, McpError> {
        Err(McpError::new(
            McpFailure::Authority,
            "this session did not supply a stdio MCP server",
        ))
    }
}

struct AcpSessionMcpAdmission {
    runtime: Arc<McpRegistryRuntime>,
    scope: Option<SessionMcpScope>,
    _storage: tempfile::TempDir,
    admitted_tools: Arc<dyn AdmittedToolSource>,
    mcp_errors: Vec<String>,
    acp_effects: Option<Arc<CliAcpMcpEffects>>,
}

struct AcpContextToolSource {
    inner: Arc<dyn AdmittedToolSource>,
    effects: Arc<CliAcpMcpEffects>,
}

impl AdmittedToolSource for AcpContextToolSource {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        self.inner.capture()
    }

    fn context_notes(&self) -> Vec<String> {
        self.effects.context_notes()
    }
}

impl AcpSessionMcpAdmission {
    fn from_runtime(
        runtime: Arc<McpRegistryRuntime>,
        storage: tempfile::TempDir,
        acp_effects: Option<Arc<CliAcpMcpEffects>>,
        scope: Option<SessionMcpScope>,
    ) -> Self {
        let controller = runtime.controller();
        let admitted_tools: Arc<dyn AdmittedToolSource> = match acp_effects.as_ref() {
            Some(effects) => Arc::new(AcpContextToolSource {
                inner: controller.admitted_tools(),
                effects: Arc::clone(effects),
            }),
            None => controller.admitted_tools(),
        };
        let mcp_errors = controller.status().mcp_errors();
        Self {
            runtime,
            scope,
            _storage: storage,
            admitted_tools,
            mcp_errors,
            acp_effects,
        }
    }
}

impl SessionMcpAdmission for AcpSessionMcpAdmission {
    fn admitted_tools(&self) -> Arc<dyn AdmittedToolSource> {
        Arc::clone(&self.admitted_tools)
    }

    fn mcp_errors(&self) -> &[String] {
        &self.mcp_errors
    }

    fn mcp_runtime(&self) -> Arc<dyn McpRuntimeBinding> {
        self.runtime.clone()
    }

    fn pinned_confinement(&self) -> Option<Confinement> {
        self.scope.as_ref().map(|scope| scope.confinement)
    }

    fn cancel_active(&self) {
        if let Some(effects) = &self.acp_effects {
            effects.cancel_active();
        }
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(effects) = &self.acp_effects {
                effects.close_all().await;
            }
            if let Some(scope) = &self.scope {
                scope.cancel.cancel(CancelCause::Host);
                scope.stdio.close().await;
            }
        })
    }
}

struct AcpMcpSessionAdmitter {
    private_diagnostic_root: PathBuf,
    http_management: Arc<dyn McpHttpEffects>,
    max_sse_event_size: usize,
    timeout: Duration,
    failed_admissions: Arc<Mutex<HashMap<[u8; 32], AdmissionRetry>>>,
    connections: Arc<Mutex<HashMap<String, Weak<CliAcpMcpEffects>>>>,
}

struct CliAcpMcpEffects {
    connection: ConnectionTo<Client>,
    active: Arc<Mutex<Vec<String>>>,
    cancelled: watch::Sender<u64>,
    workspace: Mutex<Option<String>>,
    inbox: Mutex<Option<String>>,
    self_ref: Weak<CliAcpMcpEffects>,
    connections: Arc<Mutex<HashMap<String, Weak<CliAcpMcpEffects>>>>,
    refreshes: Mutex<HashMap<String, RefreshSlot>>,
}

#[derive(Default)]
struct RefreshSlot {
    in_flight: bool,
    pending: bool,
}

fn drain_active_connections(active: &Mutex<Vec<String>>) -> Vec<String> {
    match active.lock() {
        Ok(mut active) => std::mem::take(&mut *active),
        Err(poisoned) => {
            tracing::warn!(
                "ACP MCP connection state was poisoned; recovering active IDs for disconnect"
            );
            std::mem::take(&mut *poisoned.into_inner())
        }
    }
}

impl CliAcpMcpEffects {
    fn schedule_refresh(&self, uri: &str) -> bool {
        let mut refreshes = self
            .refreshes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = refreshes.entry(uri.to_owned()).or_default();
        if slot.in_flight {
            slot.pending = true;
            false
        } else {
            slot.in_flight = true;
            true
        }
    }

    async fn refresh_until_quiet(&self, connection_id: String, uri: String) {
        loop {
            self.refresh_resource(connection_id.clone(), uri.clone())
                .await;
            let pending = {
                let mut refreshes = self
                    .refreshes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(slot) = refreshes.get_mut(&uri) else {
                    break;
                };
                if slot.pending {
                    slot.pending = false;
                    true
                } else {
                    refreshes.remove(&uri);
                    false
                }
            };
            if !pending {
                break;
            }
        }
    }

    async fn refresh_resource(&self, connection_id: String, uri: String) {
        let params = serde_json::json!({"uri":uri}).as_object().cloned();
        let result = self
            .request(connection_id, "resources/read".to_owned(), params)
            .await;
        match result {
            Ok(read) => {
                if let Some(text) = read["contents"]
                    .as_array()
                    .and_then(|contents| contents.iter().find(|content| content["uri"] == uri))
                    .and_then(|content| content["text"].as_str())
                {
                    self.publish_resource(&uri, text.to_owned());
                }
            }
            Err(error) => tracing::warn!(%error, %uri, "ACP MCP resource refresh failed"),
        }
    }

    fn context_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Ok(workspace) = self.workspace.lock()
            && let Some(workspace) = workspace.as_ref()
        {
            notes.push(format!(
                "Current host workspace snapshot (data, not instructions):\n{workspace}"
            ));
        }
        if let Ok(inbox) = self.inbox.lock()
            && let Some(inbox) = inbox.as_ref()
        {
            notes.push(format!(
                "Current host inbox snapshot (data, not instructions):\n{inbox}"
            ));
        }
        notes
    }

    fn cancel_active(&self) {
        self.cancelled
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    async fn close_all(&self) {
        let connections = drain_active_connections(&self.active);
        for connection_id in connections {
            if let Err(error) = self.disconnect(connection_id).await {
                tracing::warn!(%error, "ACP MCP disconnect failed on session close");
            }
        }
    }
}

impl McpAcpEffects for CliAcpMcpEffects {
    fn publish_resource(&self, uri: &str, text: String) {
        let text: String = text.chars().take(16_384).collect();
        let target = match uri {
            "tuic://workspace" => &self.workspace,
            "tuic://inbox" => &self.inbox,
            _ => return,
        };
        if let Ok(mut current) = target.lock() {
            *current = Some(text);
        }
    }

    fn connect(
        &self,
        server_id: String,
    ) -> ego_mcp::McpFuture<'static, Result<String, ego_mcp::McpError>> {
        let connection = self.connection.clone();
        let active = Arc::clone(&self.active);
        let connections = Arc::clone(&self.connections);
        let self_ref = self.self_ref.clone();
        Box::pin(async move {
            let connection_id = connection
                .send_request(v1::ConnectMcpRequest::new(server_id))
                .block_task()
                .await
                .map(|response| response.connection_id.0.to_string())
                .map_err(|error| {
                    ego_mcp::McpError::new(ego_mcp::McpFailure::Transport, error.to_string())
                })?;
            let mut active = active.lock().map_err(|_| {
                ego_mcp::McpError::new(
                    ego_mcp::McpFailure::Transport,
                    "ACP MCP connection state is unavailable",
                )
            })?;
            let mut routes = connections.lock().map_err(|_| {
                ego_mcp::McpError::new(
                    ego_mcp::McpFailure::Transport,
                    "ACP MCP connection routes are unavailable",
                )
            })?;
            if routes.get(&connection_id).and_then(Weak::upgrade).is_some() {
                return Err(ego_mcp::McpError::new(
                    ego_mcp::McpFailure::Transport,
                    "ACP MCP host reused an active connection ID",
                ));
            }
            routes.insert(connection_id.clone(), self_ref);
            active.push(connection_id.clone());
            Ok(connection_id)
        })
    }

    fn request(
        &self,
        connection_id: String,
        method: String,
        params: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> ego_mcp::McpFuture<'static, Result<serde_json::Value, ego_mcp::McpError>> {
        let connection = self.connection.clone();
        let mut cancelled = self.cancelled.subscribe();
        Box::pin(async move {
            let request = v1::MessageMcpRequest::new(connection_id, method).params(params);
            let sent = connection.send_request(request);
            let response = tokio::select! {
                response = sent.block_task() => response,
                _ = cancelled.changed() => {
                    return Err(ego_mcp::McpError::new(ego_mcp::McpFailure::Cancelled, "ACP MCP request was cancelled"));
                }
            }
                .map_err(|error| ego_mcp::McpError::new(ego_mcp::McpFailure::Transport, error.to_string()))?;
            serde_json::from_str(response.0.get()).map_err(Into::into)
        })
    }

    fn notify(
        &self,
        connection_id: String,
        method: String,
        params: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(), ego_mcp::McpError> {
        self.connection
            .send_notification(
                v1::MessageMcpNotification::new(connection_id, method).params(params),
            )
            .map_err(|error| {
                ego_mcp::McpError::new(ego_mcp::McpFailure::Transport, error.to_string())
            })
    }

    fn disconnect(
        &self,
        connection_id: String,
    ) -> ego_mcp::McpFuture<'static, Result<(), ego_mcp::McpError>> {
        let connection = self.connection.clone();
        let active = Arc::clone(&self.active);
        let connections = Arc::clone(&self.connections);
        let self_ref = self.self_ref.clone();
        Box::pin(async move {
            if let Ok(mut active) = active.lock() {
                active.retain(|id| id != &connection_id);
            }
            if let Ok(mut routes) = connections.lock()
                && routes
                    .get(&connection_id)
                    .is_some_and(|owner| owner.ptr_eq(&self_ref))
            {
                routes.remove(&connection_id);
            }
            connection
                .send_request(v1::DisconnectMcpRequest::new(connection_id))
                .block_task()
                .await
                .map(|_| ())
                .map_err(|error| {
                    ego_mcp::McpError::new(ego_mcp::McpFailure::Transport, error.to_string())
                })
        })
    }
}

const MAX_FAILED_ADMISSIONS: usize = 64;
const INITIAL_ADMISSION_BACKOFF: Duration = Duration::from_secs(60);
const MAX_ADMISSION_BACKOFF: Duration = Duration::from_secs(15 * 60);

struct AdmissionRetry {
    failures: u32,
    last_error: Option<String>,
    retry_after: Instant,
    active: Option<watch::Receiver<Option<Result<(), McpSessionAdmissionError>>>>,
}

struct AdmissionAttempt {
    key: [u8; 32],
    state: Arc<Mutex<HashMap<[u8; 32], AdmissionRetry>>>,
    completion: watch::Sender<Option<Result<(), McpSessionAdmissionError>>>,
    finished: bool,
}

enum AdmissionReservation {
    Wait(watch::Receiver<Option<Result<(), McpSessionAdmissionError>>>),
    Start(watch::Sender<Option<Result<(), McpSessionAdmissionError>>>),
}

impl AdmissionAttempt {
    async fn begin(
        state: Arc<Mutex<HashMap<[u8; 32], AdmissionRetry>>>,
        key: [u8; 32],
    ) -> Result<Self, McpSessionAdmissionError> {
        loop {
            let reservation = {
                let mut retries = state
                    .lock()
                    .map_err(|_| safe_mcp_error("ACP MCP admission state could not be read"))?;
                if !retries.contains_key(&key) && retries.len() >= MAX_FAILED_ADMISSIONS {
                    let evict = retries
                        .iter()
                        .filter(|(_, retry)| retry.active.is_none())
                        .min_by_key(|(_, retry)| retry.retry_after)
                        .map(|(key, _)| *key);
                    let Some(evict) = evict else {
                        return Err(safe_mcp_error(
                            "too many MCP admissions are in progress; retry later",
                        ));
                    };
                    retries.remove(&evict);
                }
                let now = Instant::now();
                let retry = retries.entry(key).or_insert_with(|| AdmissionRetry {
                    failures: 0,
                    last_error: None,
                    retry_after: now,
                    active: None,
                });
                if let Some(active) = retry.active.clone() {
                    AdmissionReservation::Wait(active)
                } else if now < retry.retry_after {
                    return Err(safe_mcp_error(format!(
                        "{}; retry later",
                        retry
                            .last_error
                            .as_deref()
                            .unwrap_or("the supplied MCP servers could not be admitted")
                    )));
                } else {
                    let (completion, active) = watch::channel(None);
                    retry.active = Some(active);
                    AdmissionReservation::Start(completion)
                }
            };
            match reservation {
                AdmissionReservation::Wait(mut active) => loop {
                    if let Some(result) = active.borrow_and_update().clone() {
                        result?;
                        break;
                    }
                    active.changed().await.map_err(|_| {
                        safe_mcp_error("the concurrent MCP admission ended without a result")
                    })?;
                },
                AdmissionReservation::Start(completion) => {
                    return Ok(Self {
                        key,
                        state,
                        completion,
                        finished: false,
                    });
                }
            }
        }
    }

    fn finish(mut self, result: Result<(), McpSessionAdmissionError>) {
        self.record(result);
        self.finished = true;
    }

    fn record(&self, result: Result<(), McpSessionAdmissionError>) {
        let Ok(mut retries) = self.state.lock() else {
            let _ = self.completion.send(Some(Err(safe_mcp_error(
                "ACP MCP admission state could not be read",
            ))));
            return;
        };
        if result.is_ok() {
            retries.remove(&self.key);
        } else if let Some(retry) = retries.get_mut(&self.key) {
            retry.active = None;
            retry.failures = retry.failures.saturating_add(1);
            retry.last_error = result.as_ref().err().map(ToString::to_string);
            let multiplier = 1_u64 << retry.failures.saturating_sub(1).min(4);
            let backoff = INITIAL_ADMISSION_BACKOFF
                .saturating_mul(multiplier as u32)
                .min(MAX_ADMISSION_BACKOFF);
            retry.retry_after = Instant::now() + backoff;
            tracing::warn!(
                attempt = retry.failures,
                retry_after_seconds = backoff.as_secs(),
                "ACP MCP admission failed; delaying another attempt for the same server selection"
            );
        }
        drop(retries);
        let _ = self.completion.send(Some(result));
    }
}

impl Drop for AdmissionAttempt {
    fn drop(&mut self) {
        if !self.finished {
            self.record(Err(safe_mcp_error("ACP MCP admission was interrupted")));
        }
    }
}

impl AcpMcpSessionAdmitter {
    fn new(
        private_diagnostic_root: PathBuf,
        http_management: Arc<dyn McpHttpEffects>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            private_diagnostic_root,
            http_management,
            max_sse_event_size,
            timeout,
            failed_admissions: Arc::new(Mutex::new(HashMap::new())),
            connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl McpSessionAdmitter for AcpMcpSessionAdmitter {
    fn mcp_notification(
        &self,
        notification: v1::MessageMcpNotification,
    ) -> Option<BoxFuture<'static, ()>> {
        let value = match serde_json::to_value(notification) {
            Ok(value) => value,
            Err(_) => return None,
        };
        let method = value["method"].as_str();
        let uri = value["params"]["uri"].as_str();
        let connection_id = value["connectionId"].as_str();
        if method != Some("notifications/resources/updated") {
            return None;
        }
        let Some(uri @ ("tuic://workspace" | "tuic://inbox")) = uri else {
            return None;
        };
        let connection_id = connection_id?;
        let effects = self
            .connections
            .lock()
            .ok()
            .and_then(|routes| routes.get(connection_id).and_then(Weak::upgrade));
        let connection_id = connection_id.to_owned();
        let uri = uri.to_owned();
        let effects = effects.filter(|effects| effects.schedule_refresh(&uri))?;
        Some(Box::pin(async move {
            effects.refresh_until_quiet(connection_id, uri).await;
        }))
    }

    fn admit(
        &self,
        servers: Vec<v1::McpServer>,
        connection: Option<ConnectionTo<Client>>,
        context: McpAdmissionContext,
    ) -> McpSessionAdmissionFuture {
        if servers.is_empty() {
            return Box::pin(async { Ok(None) });
        }
        let private_diagnostic_root = self.private_diagnostic_root.clone();
        let http_management = Arc::clone(&self.http_management);
        let max_sse_event_size = self.max_sse_event_size;
        let timeout = self.timeout;
        let failed_admissions = Arc::clone(&self.failed_admissions);
        let connections = Arc::clone(&self.connections);
        Box::pin(async move {
            let serialized = serde_json::to_vec(&servers)
                .map_err(|_| safe_mcp_error("the supplied MCP servers could not be identified"))?;
            let key: [u8; 32] = Sha256::digest(&serialized).into();
            let has_stdio = servers
                .iter()
                .any(|server| matches!(server, v1::McpServer::Stdio(_)));
            let admissions = map_mcp_servers(servers)?;
            let acp_effects = connection.map(|connection| {
                Arc::new_cyclic(|self_ref| CliAcpMcpEffects {
                    connection,
                    active: Arc::new(Mutex::new(Vec::new())),
                    cancelled: watch::channel(0).0,
                    workspace: Mutex::new(None),
                    inbox: Mutex::new(None),
                    self_ref: self_ref.clone(),
                    connections: Arc::clone(&connections),
                    refreshes: Mutex::new(HashMap::new()),
                })
            });
            let acp_management = acp_effects
                .as_ref()
                .map(|effects| Arc::clone(effects) as Arc<dyn McpAcpEffects>);
            let attempt = AdmissionAttempt::begin(failed_admissions, key).await?;
            let cleanup_effects = acp_effects.clone();
            let result = async {
                let refresh = McpRegistryRefresh::new(
                    admissions,
                    RegistryTrigger::ConfigChanged,
                    RegistryScope::All,
                )
                .map_err(|_| safe_mcp_error("the supplied MCP servers are invalid"))?;
                let storage = tempfile::tempdir()
                    .map_err(|_| safe_mcp_error("ACP MCP session storage could not be created"))?;
                let registry_root = storage.path().join("registry");
                let continuation_root = storage.path().join("continuations");
                let registry_journal =
                    Arc::new(JsonlMcpRegistryJournal::with_private_diagnostic_root(
                        registry_root,
                        private_diagnostic_root,
                        Box::new(OsIo),
                    ));
                let continuation_journal: Arc<dyn ContinuationJournal> = Arc::new(
                    JsonlContinuationJournal::open(continuation_root).map_err(|_| {
                        safe_mcp_error("ACP MCP continuation storage could not be opened")
                    })?,
                );
                // Stdio discovery starts processes, so it runs in a scope
                // confined as the session's own pick, never the online one
                // the HTTP management shares.
                // A session with no stdio server starts no process, so it
                // neither measures nor pins a confinement.
                let scope = if has_stdio {
                    let pick = context.confinement.as_ref();
                    Some(
                        SessionMcpScope::open(
                            &context.root,
                            context.session,
                            pick.map_or(Confinement::Off, |pick| pick.level),
                            pick.and_then(|pick| pick.observation.as_ref()),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                let stdio_management = scope.as_ref().map_or_else(
                    || Arc::new(NoStdioEffects) as Arc<dyn McpDuplexEffectsFactory>,
                    SessionMcpScope::stdio_management,
                );
                let runtime = Arc::new(
                    McpRegistryRuntime::new_with_acp(
                        registry_journal,
                        continuation_journal,
                        Arc::new(OsGenerationPolicyCaptureStore::default()),
                        http_management,
                        stdio_management,
                        Arc::new(SessionResumeKey::generate().map_err(|_| {
                            safe_mcp_error("ACP MCP resume key could not be generated")
                        })?),
                        Arc::new(SystemClock),
                        max_sse_event_size,
                        timeout,
                        acp_management,
                    )
                    .map_err(|_| safe_mcp_error("ACP MCP runtime could not be created"))?,
                );
                runtime
                    .controller()
                    .reconcile(refresh)
                    .await
                    .map_err(|error| {
                        safe_mcp_error(format!(
                            "the supplied MCP servers could not be admitted: {error}"
                        ))
                    })?;
                let mcp_errors = runtime.controller().status().mcp_errors();
                if !mcp_errors.is_empty() {
                    return Err(safe_mcp_error(format!(
                        "the supplied MCP servers could not be admitted: {}",
                        mcp_errors.join("; ")
                    )));
                }
                Ok(Some(Arc::new(AcpSessionMcpAdmission::from_runtime(
                    runtime,
                    storage,
                    acp_effects,
                    scope,
                )) as Arc<dyn SessionMcpAdmission>))
            }
            .await;
            if result.is_err()
                && let Some(effects) = cleanup_effects
            {
                effects.close_all().await;
            }
            attempt.finish(result.as_ref().map(|_| ()).map_err(Clone::clone));
            result
        })
    }
}

fn map_mcp_servers(
    servers: Vec<v1::McpServer>,
) -> Result<Vec<McpRegistryCandidate>, McpSessionAdmissionError> {
    servers
        .into_iter()
        .map(|server| {
            let (name, config, material) = match server {
                v1::McpServer::Stdio(server) => {
                    let name = server_name(&server.name)?;
                    if !server.command.is_absolute() {
                        return Err(safe_mcp_error(
                            "an ACP stdio MCP command must be an absolute path",
                        ));
                    }
                    let command = server.command.into_os_string().into_string().map_err(|_| {
                        safe_mcp_error("an ACP stdio MCP command is not valid Unicode")
                    })?;
                    let mut environment_names = Vec::with_capacity(server.env.len());
                    let mut environment = Vec::with_capacity(server.env.len());
                    for binding in server.env {
                        let name = binding
                            .name
                            .parse::<McpEnvironmentName>()
                            .map_err(|error| safe_mcp_error(error.to_string()))?;
                        environment_names.push(name.clone());
                        environment.push(McpEnvironmentBinding::new(
                            name,
                            SecretMaterial::new(binding.value.into_bytes()),
                        ));
                    }
                    let config =
                        McpServerConfig::stdio(command, server.args, None, environment_names)
                            .map_err(|error| safe_mcp_error(error.to_string()))?;
                    (name, config, McpTransportMaterial::stdio(environment))
                }
                v1::McpServer::Http(server) => {
                    let name = server_name(&server.name)?;
                    let url = server.url.parse::<ego_core::config::McpHttpUrl>().map_err(
                        |error: ego_core::config::McpValueError| safe_mcp_error(error.to_string()),
                    )?;
                    let allow_insecure_loopback = url.is_exact_plaintext_loopback();
                    let config = McpServerConfig::http(
                        url.as_url().clone(),
                        McpHttpAuth::None,
                        allow_insecure_loopback,
                    )
                    .map_err(|error| safe_mcp_error(error.to_string()))?;
                    let headers = server
                        .headers
                        .into_iter()
                        .map(|header| {
                            McpHeaderBinding::new(
                                header.name,
                                SecretMaterial::new(header.value.into_bytes()),
                            )
                            .map_err(|_| safe_mcp_error("an ACP MCP HTTP header is invalid"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    (name, config, McpTransportMaterial::http(headers))
                }
                v1::McpServer::Sse(_) => {
                    return Err(safe_mcp_error("this build does not support MCP over SSE"));
                }
                v1::McpServer::Acp(server) => {
                    let name = server_name(&server.name)?;
                    let config = McpServerConfig::Acp {
                        server_id: server.server_id.0.to_string(),
                    };
                    (name, config, McpTransportMaterial::acp())
                }
                _ => {
                    return Err(safe_mcp_error(
                        "this build does not support the supplied MCP transport",
                    ));
                }
            };
            let admission = McpServerAdmission::new(name, config, material)
                .map_err(|_| safe_mcp_error("the supplied MCP server is invalid"))?;
            Ok(McpRegistryCandidate::ready(
                Arc::new(admission),
                McpConfigProvenance::Layer {
                    source: Source::Flags,
                },
            ))
        })
        .collect()
}

fn server_name(name: &str) -> Result<McpServerName, McpSessionAdmissionError> {
    name.parse()
        .map_err(|error: ego_core::config::McpValueError| safe_mcp_error(error.to_string()))
}

fn safe_mcp_error(message: impl Into<String>) -> McpSessionAdmissionError {
    McpSessionAdmissionError::new(message)
}

struct AcpListener {
    admitted: Admitted,
    bound: ego_listen::Bound,
    credential: Option<Credential>,
    gate: ego_listen::ConnectionGate,
}

async fn prepare_listener(args: &AcpArgs) -> Result<Option<AcpListener>, RunTermination> {
    let Some(listen) = args.listen.as_deref() else {
        return Ok(None);
    };
    let admitted = crate::cli::admit_endpoint(listen, args.allow_public_bind)
        .map_err(|error| failed(error.to_string()))?;
    let credential = if ego_listen::credential_required(&admitted) {
        Some(
            Credential::from_env(|name| std::env::var(name).ok())
                .map_err(|error| failed(error.to_string()))?,
        )
    } else {
        None
    };
    let limits = ego_listen::ConnectionLimits::new(
        args.max_connections,
        args.connection_lifetime,
        args.max_ingress_bytes,
    )
    .map_err(|error| failed(format!("the ACP listener limits are invalid: {error}")))?;
    let gate = ego_listen::ConnectionGate::new(limits);
    let bound = ego_listen::bind(&admitted)
        .await
        .map_err(|error| failed(format!("the ACP endpoint could not be opened: {error}")))?;
    Ok(Some(AcpListener {
        admitted,
        bound,
        credential,
        gate,
    }))
}

fn failed(message: String) -> RunTermination {
    RunTermination::Failed { message }
}

fn confinement_ceiling(
    args: &AcpArgs,
    configured: Option<Confinement>,
    endpoint: Option<&Admitted>,
) -> Confinement {
    let selected = args.sandbox.or(configured).unwrap_or_default();
    endpoint.map_or(selected, |endpoint| endpoint.cap_confinement(selected))
}

/// Serve the protocol on stdio or an explicitly selected endpoint.
pub async fn serve(args: &AcpArgs, evidence: Arc<EvidenceAdmissionService>) -> RunTermination {
    let listener = match prepare_listener(args).await {
        Ok(listener) => listener,
        Err(termination) => return termination,
    };
    let admitted = listener.as_ref().map(|listener| &listener.admitted);
    let root = match run::workspace(args.directory.as_ref()) {
        Ok(root) => root,
        Err(error) => {
            return RunTermination::UsageError {
                message: error.to_string(),
            };
        }
    };
    let store_root = match session::store_root(args.session_dir.as_ref()) {
        Ok(store_root) => run::rooted_path(&root, store_root),
        Err(error) => {
            return RunTermination::UsageError {
                message: error.to_string(),
            };
        }
    };
    let private_diagnostic_root = store_root.join("mcp-private-diagnostics");
    let store = Arc::new(session::jsonl(store_root, root.clone()));
    let executor = Arc::new(run::CliRunExecutor::for_acp(
        args.session_dir.clone(),
        args.shape.clone(),
        Arc::clone(&store),
        Arc::clone(&evidence),
    ));
    let compaction: Arc<dyn ego_core::compact::CompactionCommand> =
        Arc::new(run::CliCompactionCommand::for_protocol(
            root.clone(),
            args.session_dir.clone(),
            Arc::clone(&store),
            evidence,
        ));
    let ids = Arc::new(UuidIds);
    let clock = Arc::new(SystemClock);
    let controller = Arc::new(RunController::new(
        Arc::clone(&store),
        executor,
        Arc::clone(&clock),
        Arc::clone(&ids),
        RunRetention::new(NonZeroUsize::new(64).expect("non-zero retention"))
            .expect("valid retention"),
    ));
    let run_lifecycle: Arc<dyn SessionRunLifecycle> = controller.clone();
    let lifecycle = Arc::new(SessionLifecycle::new(
        Arc::clone(&store),
        run_lifecycle,
        clock,
    ));

    let mcp_cancel = CancelToken::new();
    let mcp_effects = match CommandHttpScope::online(mcp_cancel.clone()).await {
        Ok(effects) => effects,
        Err(error) => {
            return RunTermination::Failed {
                message: format!("the ACP MCP effect scope could not be created: {error}"),
            };
        }
    };
    let http_management: Arc<dyn McpHttpEffects> =
        Arc::new(RunMcpHttpEffects::new(Arc::clone(mcp_effects.ports())));
    let mcp_admitter: Arc<dyn McpSessionAdmitter> = Arc::new(AcpMcpSessionAdmitter::new(
        private_diagnostic_root,
        http_management,
        MCP_DEFAULT_MAX_SSE_EVENT_SIZE,
        MCP_DEFAULT_OPERATION_TIMEOUT,
    ));

    // The connected host is this connection's operator seat, so it gets the
    // picker a terminal seat gets. `run.rs` builds the catalogue only where
    // there is a human, and the ACP client is one. Discovery reuses the HTTP
    // scope this server already opened rather than a second egress: an `ego
    // acp` invocation is online by construction, so this adds no reach it did
    // not already have.
    let (controls, ceiling, rule_target, coordinator, orchestration_mode) =
        match run::protocol_effective(root.as_path(), args.session_dir.clone(), args.shape.clone())
        {
            Ok(effective) => {
                crate::config::report_discarded(root.as_path(), &effective, &crate::output::Stderr);
                let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(mcp_effects.ports())))
                    as Arc<dyn ego_ai::http::Http>;
                let catalogue = match crate::models::build_catalogue_snapshot(
                    &crate::cli::ModelsArgs {
                        provider: None,
                        refresh: false,
                        json: false,
                    },
                    &http,
                )
                .await
                {
                    Ok(catalogue) => Some(catalogue),
                    // A picker that lists only the configured model is worse
                    // than one that lists every model; a connection that
                    // refuses to start is worse than both.
                    Err(error) => {
                        tracing::warn!(?error, "the ACP model catalogue is unavailable");
                        None
                    }
                };
                // The flag wins over the configuration, and the configuration
                // over the default. A host that was started at `--sandbox ro`
                // is capped there whatever any layer says, which is the whole
                // reason the flag exists: the layer an operator would have
                // reached for is the one an untrusted headless resolution
                // discards.
                let ceiling = confinement_ceiling(args, effective.sandbox, admitted);
                let rule_target = match effective.active_profile.as_ref() {
                    Some(profile) => match ProfileName::parse(&profile.name) {
                        Ok(name) => UserConfigTarget::Profile(name),
                        Err(error) => {
                            return RunTermination::UsageError {
                                message: format!(
                                    "the ACP profile cannot target user rules: {error}"
                                ),
                            };
                        }
                    },
                    None => UserConfigTarget::Base,
                };
                (
                    Arc::new(AcpSessionControls::new(&effective, ceiling, catalogue)),
                    ceiling,
                    rule_target,
                    effective
                        .active_profile
                        .as_ref()
                        .is_some_and(|profile| profile.name == "coordinator"),
                    effective.orchestration,
                )
            }
            Err(error) => {
                return RunTermination::UsageError {
                    message: format!(
                        "the ACP confinement configuration could not be resolved: {error}"
                    ),
                };
            }
        };

    let reporter = Arc::new(Reporter::new(Arc::new(crate::output::Stderr)));
    if listener.is_some() {
        let policy = RunConfinement::new(ceiling, false, CanonPath::new(root.as_path()))
            .expect("the resolved ACP process root is absolute");
        let reporter = Arc::clone(&reporter);
        let cancel = mcp_cancel.clone();
        tokio::spawn(async move {
            reporter.report(
                RunEffectPorts::diagnostic_capabilities(&policy, cancel, ExecLimits::default())
                    .await,
            );
        });
    }

    // The first session of a connection should find its sandbox already
    // measured: start every offered measurement now, while the host is still
    // initializing, instead of when its first `session/new` arrives.
    {
        let cancel = mcp_cancel.clone();
        let warm_root = root.as_path().to_path_buf();
        tokio::spawn(async move {
            if let Err(error) =
                crate::acp_confinement::warm_process_cache(ceiling, cancel, &warm_root).await
            {
                tracing::debug!(%error, "the ACP confinement warm-up did not finish");
            }
        });
    }

    let control_port: Arc<dyn SessionControls> = controls.clone();
    let permission_rules: Option<Arc<dyn ego_acp::permission::PermissionRuleWriter>> =
        crate::config::ego_home().map(|home| {
            Arc::new(CliAcpPermissionRules::new(
                home.join("config.toml"),
                rule_target,
            )) as Arc<dyn ego_acp::permission::PermissionRuleWriter>
        });
    let sandbox_grants: Option<Arc<dyn ego_acp::sandbox::SandboxGrantStore>> =
        crate::config::ego_home().map(|home| {
            Arc::new(crate::acp_sandbox_grants::FileSandboxGrants::new(home))
                as Arc<dyn ego_acp::sandbox::SandboxGrantStore>
        });
    let confinement_port: Arc<dyn SessionConfinementAdmission> = Arc::new(
        CliSessionConfinementAdmission::with_reporter(
            Arc::clone(&store),
            controls,
            ceiling,
            mcp_cancel.clone(),
            Arc::clone(&reporter),
        )
        .with_shape(args.shape.clone()),
    );

    // One lister for the whole connection: it holds no session state, and
    // the answer it helps ask is carried by the run selection rather than by
    // anything this adapter remembers.
    let trust = CliSessionTrustAdmission::new(crate::config::ego_home());
    let trust = if args.shape.profile.is_some()
        && args.shape.profile_source == Some(ego_core::config::Source::Flags)
    {
        trust.with_host_profile()
    } else {
        trust
    };
    let trust_port: Arc<dyn SessionTrustAdmission> = Arc::new(trust);

    let notices = ego_acp::serve::AcpNotices::default();
    let _observer = match crate::orchestration_observer::start(
        coordinator,
        orchestration_mode,
        &root,
        notices.clone(),
        Arc::clone(&store),
    ) {
        Ok(observer) => observer,
        Err(error) => {
            return failed(format!(
                "the orchestration observer could not start: {error}"
            ));
        }
    };
    let roots = Arc::new(CanonicalRoots { base: root });
    let spawner = Arc::new(TokioExecutionSpawner);
    let factory = move || {
        ego_acp::serve::durable_agent_with_compaction(
            Arc::clone(&controller),
            Arc::clone(&lifecycle),
            roots.clone(),
            spawner.clone(),
            ids.clone(),
            Arc::new(
                AcpRunBindingsBuilder::with_controls(Some(Arc::clone(&control_port)))
                    .with_permission_rules(permission_rules.clone())
                    .with_sandbox_store(sandbox_grants.clone()),
            ),
            Arc::clone(&mcp_admitter),
            Arc::clone(&compaction),
            Some(Arc::clone(&control_port)),
            Some(Arc::clone(&confinement_port)),
            Some(Arc::clone(&trust_port)),
            notices.clone(),
        )
    };
    let outcome = match listener {
        Some(listener) => ego_acp::serve::serve_on(
            listener.bound,
            &listener.admitted,
            listener.credential,
            listener.gate,
            factory,
            std::future::pending(),
        )
        .await
        .map_err(|error| format!("the ACP listener ended badly: {error}")),
        None => factory()
            .connect_to(Stdio::new())
            .await
            .map_err(|error| format!("the ACP connection ended badly: {error}")),
    };
    mcp_cancel.cancel(CancelCause::Host);
    drop(mcp_effects);
    match outcome {
        Ok(()) => RunTermination::Done,
        Err(message) => RunTermination::Failed { message },
    }
}

#[cfg(test)]
mod confinement_ceiling_tests {
    use super::{confinement_ceiling, map_mcp_servers};
    use crate::cli::{self, AcpArgs, Invocation};
    use ego_core::endpoint::Admitted;
    use ego_core::permit::Confinement;

    fn args(sandbox: Option<Confinement>) -> AcpArgs {
        let mut argv = vec!["ego".to_owned(), "acp".to_owned()];
        if let Some(sandbox) = sandbox {
            argv.extend(["--sandbox".to_owned(), sandbox.as_str().to_owned()]);
        }
        match cli::parse(argv).expect("the ACP command line parses") {
            Invocation::Acp(args) => *args,
            _ => panic!("the test command is not `ego acp`"),
        }
    }

    #[test]
    fn resolves_flag_then_config_then_default_and_caps_only_admitted_public() {
        let local = Admitted::Local {
            name: "regression case.sock".to_owned(),
        };
        let loopback = Admitted::Loopback("127.0.0.1:8080".parse().unwrap());
        let public = Admitted::Public("0.0.0.0:8080".parse().unwrap());

        assert_eq!(
            confinement_ceiling(&args(Some(Confinement::Ro)), Some(Confinement::Off), None),
            Confinement::Ro,
            "the CLI flag wins over configuration"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Ro), None),
            Confinement::Ro,
            "configuration wins when the CLI does not name a ceiling"
        );
        assert_eq!(
            confinement_ceiling(&args(None), None, None),
            Confinement::Workspace,
            "the default applies when neither flag nor configuration names a ceiling"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Off), Some(&public)),
            Confinement::Workspace,
            "an admitted public endpoint caps unconfined ACP at workspace"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Off), None),
            Confinement::Off,
            "stdio has no endpoint cap"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Off), Some(&local)),
            Confinement::Off,
            "a local endpoint adds no confinement cap"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Off), Some(&loopback)),
            Confinement::Off,
            "a loopback endpoint adds no confinement cap"
        );
    }

    #[test]
    fn maps_only_exact_loopback_plaintext_http_servers() {
        let server = |url: &str| {
            serde_json::from_value(serde_json::json!({
                "type": "http",
                "name": "local",
                "url": url,
                "headers": []
            }))
            .expect("the ACP HTTP server fixture parses")
        };

        for url in [
            "http://localhost:8787/mcp",
            "http://127.0.0.1:8787/mcp",
            "http://[::1]:8787/mcp",
            "https://mcp.example.test/mcp",
        ] {
            assert!(
                map_mcp_servers(vec![server(url)]).is_ok(),
                "valid ACP MCP server {url} was refused"
            );
        }
        let error = match map_mcp_servers(vec![server("http://192.0.2.1:8787/mcp")]) {
            Ok(_) => panic!("non-loopback plaintext HTTP was accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "plaintext HTTP requires allow_insecure_loopback = true"
        );
    }
}

#[cfg(test)]
mod poisoned_active_connections_tests {
    use std::sync::{Arc, Mutex};

    use super::drain_active_connections;

    #[test]
    fn poisoned_active_lock_still_yields_every_connection_for_disconnect() {
        let active = Arc::new(Mutex::new(vec!["first".to_owned(), "second".to_owned()]));
        let poison = Arc::clone(&active);
        let thread = std::thread::spawn(move || {
            let _guard = poison.lock().expect("initial lock");
            panic!("poison the lock after recording active connections");
        });
        assert!(thread.join().is_err());
        assert_eq!(drain_active_connections(&active), ["first", "second"]);
        assert!(active.lock().unwrap_err().into_inner().is_empty());
    }
}

#[cfg(test)]
mod admission_concurrency_tests {
    use std::sync::Arc;

    use ego_acp::serve::{McpAdmissionContext, McpSessionAdmitter};
    use ego_core::event::id::SessionId;
    use ego_core::run::RunRoot;
    use ego_core::turn::CancelToken;
    use ego_tools::mcp::RunMcpHttpEffects;

    use super::{
        AcpMcpSessionAdmitter, CommandHttpScope, MCP_DEFAULT_MAX_SSE_EVENT_SIZE,
        MCP_DEFAULT_OPERATION_TIMEOUT,
    };

    #[tokio::test]
    async fn two_simultaneous_sessions_with_the_same_working_server_both_admit() {
        let directory = tempfile::tempdir().expect("test files");
        let script = directory.path().join("working-mcp.py");
        std::fs::write(
            &script,
            r#"import json, sys, time
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    if request["method"] == "server/discover":
        time.sleep(3)
        result = {"resultType": "complete", "supportedVersions": ["2026-07-28"], "capabilities": {}, "ttlMs": 0, "cacheScope": "private"}
    else:
        result = {"resultType": "complete", "tools": [], "resources": [], "prompts": [], "nextCursor": None, "ttlMs": 0, "cacheScope": "private"}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#,
        )
        .expect("working stdio server");
        let server = serde_json::json!({
            "name": "working",
            "command": "/usr/bin/python3",
            "args": [script],
            "env": []
        });
        let effects = CommandHttpScope::online(CancelToken::new())
            .await
            .expect("MCP effects");
        let http = Arc::new(RunMcpHttpEffects::new(Arc::clone(effects.ports())));
        let admitter = AcpMcpSessionAdmitter::new(
            directory.path().join("private-diagnostics"),
            http,
            MCP_DEFAULT_MAX_SSE_EVENT_SIZE,
            MCP_DEFAULT_OPERATION_TIMEOUT,
        );
        let context = || McpAdmissionContext {
            session: SessionId::new(),
            root: RunRoot::new(std::env::current_dir().expect("workspace")).expect("root"),
            confinement: None,
        };
        let selection = || vec![serde_json::from_value(server.clone()).expect("ACP server")];
        let (first, second) = tokio::join!(
            admitter.admit(selection(), None, context()),
            admitter.admit(selection(), None, context())
        );
        if let Err(error) = first {
            panic!("first session: {error}");
        }
        if let Err(error) = second {
            panic!("second session: {error}");
        }
    }
}
