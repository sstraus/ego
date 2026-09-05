//! The inbound ACP adapter over core's rooted run controller.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::negotiate::{SERVED, negotiate};
use crate::project::{project_replay_v1, stop_reason_v1};
use crate::question::{AcpQuestionAdapter, CapabilityCapture};
use agent_client_protocol::{
    Agent, AgentProtocolRouter, Client, ConnectionTo, Error, ErrorCode, JsonRpcMessage,
    JsonRpcRequest, Responder, UntypedMessage, on_receive_notification, on_receive_request,
};
use agent_client_protocol_schema::v1;
use ego_core::compact::{CompactionCommand, CompactionReceipt, CompactionTransitionRequest};
use ego_core::event::id::SessionId;
use ego_core::event::time::Clock;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::mcp::McpRuntimeBinding;
use ego_core::run::{
    CancelKey, ContinueRequest, EventSinkFactory, IdSource, RequestDigest, RequestId,
    RunCommandOutcome, RunController, RunError, RunExecution, RunExecutionBindings, RunExecutor,
    RunRecoveryOutcome, RunRoot, RunSelections, RunSettlement, RunSnapshot, RunStatus,
    RunSubscription, StartRequest,
};
use ego_core::session_lifecycle::{
    LifecycleError, SessionAttachment, SessionCursor, SessionLifecycle, SessionLifecycleStore,
    SessionPage,
};
use ego_core::sink::EventSink;
use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
use ego_core::turn::AdmittedToolSource;
use futures::channel::oneshot;
use serde::{Deserialize, Serialize};

/// Failure to turn an untrusted protocol path into a canonical run root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootResolveError(String);

impl RootResolveError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for RootResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RootResolveError {}

/// Consumer-owned authority for resolving a raw ACP working directory.
pub trait RootResolver: Send + Sync {
    fn resolve(&self, cwd: &Path) -> Result<RunRoot, RootResolveError>;
}

/// Runtime owner for execution that must outlive the ACP connection.
pub trait ExecutionSpawner: Send + Sync {
    fn spawn(&self, execution: Pin<Box<dyn Future<Output = ()> + Send + 'static>>);
}

/// A sanitized failure from the session-scoped MCP admission boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSessionAdmissionError(String);

impl McpSessionAdmissionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for McpSessionAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for McpSessionAdmissionError {}

/// Live MCP authority retained for exactly one ACP connection session.
pub trait SessionMcpAdmission: Send + Sync {
    fn admitted_tools(&self) -> Arc<dyn AdmittedToolSource>;
    fn mcp_errors(&self) -> &[String];
    fn mcp_runtime(&self) -> Arc<dyn McpRuntimeBinding>;
}

pub type McpSessionAdmissionFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError>>
            + Send
            + 'static,
    >,
>;

/// Consumer-owned boundary that atomically admits one ACP session's servers.
pub trait McpSessionAdmitter: Send + Sync {
    fn admit(&self, servers: Vec<v1::McpServer>) -> McpSessionAdmissionFuture;
}

/// Builds live execution bindings from the connection that accepted a prompt.
pub trait RunBindingsBuilder: Send + Sync {
    fn build(
        &self,
        connection: ConnectionTo<Client>,
        session: v1::SessionId,
        capabilities: CapabilityCapture,
    ) -> ego_core::run::RunExecutionBindings;
}

/// Production ACP bindings: live updates plus the existing question adapter.
#[derive(Debug, Default)]
pub struct AcpRunBindingsBuilder;

struct AcpSinkFactory {
    connection: ConnectionTo<Client>,
    session: v1::SessionId,
}

impl EventSinkFactory for AcpSinkFactory {
    fn create(&self, capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(crate::sink::AcpSink::new(
            self.connection.clone(),
            self.session.clone(),
            capabilities.clone(),
        ))
    }
}

impl RunBindingsBuilder for AcpRunBindingsBuilder {
    fn build(
        &self,
        connection: ConnectionTo<Client>,
        session: v1::SessionId,
        capabilities: CapabilityCapture,
    ) -> ego_core::run::RunExecutionBindings {
        let elicitor = AcpQuestionAdapter::live(capabilities, connection.clone()).elicitor();
        ego_core::run::RunExecutionBindings::new(
            Arc::new(AcpSinkFactory {
                connection,
                session,
            }),
            Some(elicitor),
        )
    }
}

fn capabilities(compaction: bool, lifecycle: bool) -> v1::AgentCapabilities {
    let capabilities = v1::AgentCapabilities::new()
        .load_session(lifecycle)
        .mcp_capabilities(v1::McpCapabilities::new().http(true).sse(false));
    let capabilities = if lifecycle {
        capabilities.session_capabilities(
            v1::SessionCapabilities::new()
                .list(v1::SessionListCapabilities::new())
                .delete(v1::SessionDeleteCapabilities::new())
                .fork(v1::SessionForkCapabilities::new())
                .resume(v1::SessionResumeCapabilities::new())
                .close(v1::SessionCloseCapabilities::new()),
        )
    } else {
        capabilities
    };
    if !compaction {
        return capabilities;
    }
    capabilities.meta(serde_json::Map::from_iter([(
        "ego".to_owned(),
        serde_json::json!({
            "compact": {"version": 1, "method": "_ego/compact"}
        }),
    )]))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcpCompactRequest {
    v: u32,
    session_id: String,
    request_id: String,
}

impl JsonRpcMessage for AcpCompactRequest {
    fn matches_method(method: &str) -> bool {
        method == "_ego/compact"
    }

    fn method(&self) -> &str {
        "_ego/compact"
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, Error> {
        UntypedMessage::new(self.method(), self)
    }

    fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, Error> {
        if !Self::matches_method(method) {
            return Err(Error::method_not_found());
        }
        agent_client_protocol::util::json_cast_params(params)
    }
}

impl JsonRpcRequest for AcpCompactRequest {
    type Response = serde_json::Value;
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcpCompactResponse {
    v: u32,
    source_session_id: SessionId,
    source_seq: ego_core::event::id::Seq,
    target_session_id: SessionId,
    publication: ego_core::session::TargetPublication,
    successor_start_request_id: RequestId,
}

impl From<CompactionReceipt> for AcpCompactResponse {
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

struct Registered {
    generation: RegistrationGeneration,
    root: RunRoot,
    start_request: Option<RequestId>,
    last_command: Option<LastCommand>,
    run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
    busy: bool,
    active_cancel: Option<CancelKey>,
    mcp: Option<Arc<dyn SessionMcpAdmission>>,
}

/// The last prompt one session accepted, and the command that prompt named.
/// Re-delivering the same protocol request with the same body names that
/// command again, so the run replays its retained receipt instead of admitting
/// a second turn. Both halves matter: JSON-RPC lets a client reuse a request
/// identity it has already been answered on, and a reused identity carrying a
/// different body is new work, not a re-delivery.
struct LastCommand {
    request: String,
    digest: String,
    id: RequestId,
}

struct PromptCorrelation {
    generation: RegistrationGeneration,
    root: RunRoot,
    request_id: RequestId,
    run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
    mcp: Option<Arc<dyn SessionMcpAdmission>>,
}

enum BeginPrompt {
    Ready(PromptCorrelation),
    Busy,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RegistrationGeneration(u64);

#[derive(Default)]
struct SessionRegistrations {
    next_generation: u64,
    sessions: HashMap<v1::SessionId, Registered>,
}

impl SessionRegistrations {
    fn mint_generation(&mut self) -> RegistrationGeneration {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("an ACP connection exhausted its registration generations");
        RegistrationGeneration(self.next_generation)
    }
}

#[derive(Clone, Default)]
struct Sessions(Arc<Mutex<SessionRegistrations>>);

impl Sessions {
    fn open(
        &self,
        id: v1::SessionId,
        root: RunRoot,
        start_request: Option<RequestId>,
        run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
        mcp: Option<Arc<dyn SessionMcpAdmission>>,
    ) -> Option<RegistrationGeneration> {
        let mut sessions = self.lock();
        if sessions
            .sessions
            .get(&id)
            .is_some_and(|session| session.busy)
        {
            return None;
        }
        let generation = sessions.mint_generation();
        sessions.sessions.insert(
            id,
            Registered {
                generation,
                root,
                start_request,
                last_command: None,
                run,
                busy: false,
                active_cancel: None,
                mcp,
            },
        );
        Some(generation)
    }

    fn begin(
        &self,
        id: &v1::SessionId,
        ids: &dyn IdSource,
        command: &str,
        digest: &str,
    ) -> BeginPrompt {
        let mut sessions = self.lock();
        let Some(session) = sessions.sessions.get(id) else {
            return BeginPrompt::Unknown;
        };
        if session.busy {
            return BeginPrompt::Busy;
        }
        let generation = sessions.mint_generation();
        let session = sessions
            .sessions
            .get_mut(id)
            .expect("the checked ACP session remains registered");
        session.generation = generation;
        session.busy = true;
        let request_id = match &session.last_command {
            Some(last) if last.request == command && last.digest == digest => last.id,
            _ if session.run.is_none() => session
                .start_request
                .unwrap_or_else(|| RequestId::from_uuid(ids.next_uuid())),
            _ => RequestId::from_uuid(ids.next_uuid()),
        };
        session.last_command = Some(LastCommand {
            request: command.to_owned(),
            digest: digest.to_owned(),
            id: request_id,
        });
        BeginPrompt::Ready(PromptCorrelation {
            generation,
            root: session.root.clone(),
            request_id,
            run: session.run,
            mcp: session.mcp.as_ref().map(Arc::clone),
        })
    }

    fn install_admission(
        &self,
        id: &v1::SessionId,
        generation: RegistrationGeneration,
        run_id: ego_core::run::RunId,
        revision: ego_core::run::RunRevision,
        key: CancelKey,
    ) {
        if let Some(session) = self
            .lock()
            .sessions
            .get_mut(id)
            .filter(|session| session.generation == generation)
        {
            session.run = Some((run_id, revision));
            session.active_cancel = Some(key);
            session.busy = true;
        }
    }

    fn finish(
        &self,
        id: &v1::SessionId,
        generation: RegistrationGeneration,
        snapshot: &RunSnapshot,
    ) {
        if let Some(session) = self
            .lock()
            .sessions
            .get_mut(id)
            .filter(|session| session.generation == generation)
        {
            session.run = Some((snapshot.run_id(), snapshot.revision()));
            session.active_cancel = None;
            session.busy = false;
        }
    }

    fn refuse(&self, id: &v1::SessionId, generation: RegistrationGeneration) {
        if let Some(session) = self
            .lock()
            .sessions
            .get_mut(id)
            .filter(|session| session.generation == generation)
        {
            session.active_cancel = None;
            session.busy = false;
        }
    }

    fn cancel_key(&self, id: &v1::SessionId) -> Option<CancelKey> {
        self.lock()
            .sessions
            .get(id)
            .and_then(|session| session.active_cancel.clone())
    }

    fn is_busy(&self, id: &v1::SessionId) -> bool {
        self.lock()
            .sessions
            .get(id)
            .is_some_and(|session| session.busy)
    }

    fn close(&self, id: &v1::SessionId) {
        self.lock().sessions.remove(id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionRegistrations> {
        self.0
            .lock()
            .expect("a poisoned lock means an ACP handler panicked mid-session")
    }
}

fn validate_mcp_servers(servers: &[v1::McpServer]) -> Result<(), McpSessionAdmissionError> {
    let mut names = BTreeSet::new();
    for server in servers {
        let name = match server {
            v1::McpServer::Stdio(server) => {
                if !server.command.is_absolute() {
                    return Err(McpSessionAdmissionError::new(
                        "an ACP stdio MCP command must be an absolute path",
                    ));
                }
                let mut names = BTreeSet::new();
                for binding in &server.env {
                    binding
                        .name
                        .parse::<ego_core::config::McpEnvironmentName>()
                        .map_err(|error| McpSessionAdmissionError::new(error.to_string()))?;
                    if !names.insert(binding.name.to_ascii_lowercase()) {
                        return Err(McpSessionAdmissionError::new(
                            "an ACP stdio MCP server contains duplicate environment names",
                        ));
                    }
                }
                &server.name
            }
            v1::McpServer::Http(server) => {
                let url = server
                    .url
                    .parse::<ego_core::config::McpHttpUrl>()
                    .map_err(|error| McpSessionAdmissionError::new(error.to_string()))?;
                ego_core::config::McpServerConfig::http(
                    url.as_url().clone(),
                    ego_core::config::McpHttpAuth::None,
                    false,
                )
                .map_err(|error| McpSessionAdmissionError::new(error.to_string()))?;
                let mut names = BTreeSet::new();
                for header in &server.headers {
                    ego_core::permit::HttpHeader::secret(
                        &header.name,
                        ego_core::credentials::SecretMaterial::new(
                            header.value.as_bytes().to_vec(),
                        ),
                    )
                    .map_err(|error| McpSessionAdmissionError::new(error.to_string()))?;
                    let normalized = header.name.to_ascii_lowercase();
                    if matches!(
                        normalized.as_str(),
                        "host"
                            | "content-length"
                            | "transfer-encoding"
                            | "connection"
                            | "accept"
                            | "content-type"
                            | "mcp-session-id"
                    ) {
                        return Err(McpSessionAdmissionError::new(
                            "an ACP HTTP MCP server contains a wire-owned header",
                        ));
                    }
                    if !names.insert(normalized) {
                        return Err(McpSessionAdmissionError::new(
                            "an ACP HTTP MCP server contains duplicate header names",
                        ));
                    }
                }
                &server.name
            }
            v1::McpServer::Sse(_) => {
                return Err(McpSessionAdmissionError::new(
                    "this build does not support MCP over SSE",
                ));
            }
            _ => {
                return Err(McpSessionAdmissionError::new(
                    "this build does not support the supplied MCP transport",
                ));
            }
        };
        name.parse::<ego_core::config::McpServerName>()
            .map_err(|error| McpSessionAdmissionError::new(error.to_string()))?;
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(McpSessionAdmissionError::new(
                "an ACP session contains duplicate MCP server names",
            ));
        }
    }
    Ok(())
}

async fn admit_mcp(
    admitter: &dyn McpSessionAdmitter,
    servers: Vec<v1::McpServer>,
) -> Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError> {
    validate_mcp_servers(&servers)?;
    let supplied = !servers.is_empty();
    match admitter.admit(servers).await {
        Ok(Some(admission)) if supplied => Ok(Some(admission)),
        Ok(Some(_)) => Err(McpSessionAdmissionError::new(
            "an empty ACP MCP selection unexpectedly produced live authority",
        )),
        Ok(None) if !supplied => Ok(None),
        Ok(None) => Err(McpSessionAdmissionError::new(
            "the supplied MCP servers did not produce live session authority",
        )),
        Err(error) => Err(error),
    }
}

fn session_bindings(
    builder: &dyn RunBindingsBuilder,
    connection: ConnectionTo<Client>,
    session: v1::SessionId,
    capabilities: CapabilityCapture,
    mcp: Option<&Arc<dyn SessionMcpAdmission>>,
) -> RunExecutionBindings {
    let bindings = builder.build(connection, session, capabilities);
    match mcp {
        Some(mcp) => bindings.with_mcp(
            mcp.admitted_tools(),
            mcp.mcp_errors().to_vec(),
            mcp.mcp_runtime(),
        ),
        None => bindings,
    }
}

async fn current_run<S, E, C, I>(
    controller: &RunController<S, E, C, I>,
    session: SessionId,
) -> Result<Option<(ego_core::run::RunId, ego_core::run::RunRevision)>, RunError>
where
    S: SessionStore,
    E: RunExecutor,
    C: Clock,
    I: IdSource,
{
    match controller.status_by_session(session).await {
        Ok(snapshot) => Ok(Some((snapshot.run_id(), snapshot.revision()))),
        Err(RunError::Store(StoreError::NotFound)) => Ok(None),
        Err(error) => Err(error),
    }
}

struct RecoveredSessionRun {
    run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
    active: Option<(CancelKey, RunSubscription, RunExecution)>,
}

async fn recover_current_run<S, E, C, I>(
    controller: &RunController<S, E, C, I>,
    session: SessionId,
    bindings: RunExecutionBindings,
) -> Result<RecoveredSessionRun, String>
where
    S: SessionStore,
    E: RunExecutor,
    C: Clock,
    I: IdSource,
{
    let Some((run_id, revision)) = current_run(controller, session)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(RecoveredSessionRun {
            run: None,
            active: None,
        });
    };
    let snapshot = controller
        .status(run_id)
        .await
        .map_err(|error| error.to_string())?;
    if !matches!(
        snapshot.status(),
        RunStatus::Running | RunStatus::Cancelling
    ) {
        return Ok(RecoveredSessionRun {
            run: Some((run_id, revision)),
            active: None,
        });
    }
    match controller
        .recover(run_id, bindings)
        .await
        .map_err(|error| error.to_string())?
    {
        RunRecoveryOutcome::Reattached(reattachment) => {
            let snapshot = reattachment.snapshot().clone();
            let key = reattachment.cancel_key().clone();
            let subscription = reattachment.subscription();
            let execution = reattachment.into_execution();
            Ok(RecoveredSessionRun {
                run: Some((snapshot.run_id(), snapshot.revision())),
                active: Some((key, subscription, execution)),
            })
        }
        RunRecoveryOutcome::Observed(observation) => {
            let snapshot = observation.snapshot();
            if matches!(
                snapshot.status(),
                RunStatus::Running | RunStatus::Cancelling
            ) {
                return Err(format!(
                    "session {session} already has a live turn attached to this process"
                ));
            }
            Ok(RecoveredSessionRun {
                run: Some((snapshot.run_id(), snapshot.revision())),
                active: None,
            })
        }
    }
}

fn prompt_text(blocks: &[v1::ContentBlock]) -> Result<String, String> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            v1::ContentBlock::Text(text) => parts.push(text.text.clone()),
            v1::ContentBlock::ResourceLink(link) => parts.push(link.uri.clone()),
            v1::ContentBlock::Image(_) => return Err(unsupported_block("an image")),
            v1::ContentBlock::Audio(_) => return Err(unsupported_block("audio")),
            v1::ContentBlock::Resource(_) => {
                return Err(unsupported_block("an embedded resource"));
            }
            _ => return Err(unsupported_block("a content block this build cannot read")),
        }
    }
    Ok(parts.join("\n"))
}

fn unsupported_block(what: &str) -> String {
    format!(
        "the prompt carries {what}, and this build's conversation model holds \
         only text, tool calls and tool results — so there is nothing for it to \
         be sent in. `initialize` advertises no such prompt capability"
    )
}

fn prompt_result(snapshot: &RunSnapshot) -> Result<v1::StopReason, String> {
    match snapshot.settlement() {
        Some(RunSettlement::Completed) => stop_reason_v1(&ego_core::run::RunTermination::Done),
        Some(RunSettlement::ContextOverflow) => {
            stop_reason_v1(&ego_core::run::RunTermination::ContextOverflow {
                message: "the provider context window is exhausted; invoke _ego/compact"
                    .to_string(),
            })
        }
        Some(RunSettlement::Cancelled { .. }) | Some(RunSettlement::Interrupted) => stop_reason_v1(
            &ego_core::run::RunTermination::Signalled(ego_core::run::Signal::Int),
        ),
        Some(RunSettlement::PermissionRequired { action, by }) => {
            stop_reason_v1(&ego_core::run::RunTermination::PermissionRequired {
                action: action.clone(),
                by: by.clone(),
            })
        }
        Some(RunSettlement::Uncertain) => {
            Err("the run outcome is uncertain and requires explicit recovery".to_string())
        }
        None => Err("the admitted turn ended without a settlement".to_string()),
    }
}

type LifecycleFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, LifecycleError>> + Send + 'a>>;

/// Consumer-owned semantic port used only to compose optional ACP lifecycle
/// capability handlers without coupling the legacy router constructor to a
/// concrete outer store. It contains no ACP DTOs and owns no durable policy.
trait LifecycleOperations: Send + Sync {
    fn create_with_id(&self, id: SessionId, root: std::path::PathBuf)
    -> Result<(), LifecycleError>;
    fn list(
        &self,
        cursor: Option<&SessionCursor>,
        root: Option<&Path>,
    ) -> Result<SessionPage, LifecycleError>;
    fn load<'a>(&'a self, id: SessionId, root: &'a Path) -> LifecycleFuture<'a, SessionAttachment>;
    fn resume<'a>(
        &'a self,
        id: SessionId,
        root: &'a Path,
    ) -> LifecycleFuture<'a, SessionAttachment>;
    fn fork(
        &self,
        id: SessionId,
    ) -> LifecycleFuture<'_, ego_core::session_lifecycle::CreatedSession>;
    fn delete(&self, id: SessionId) -> LifecycleFuture<'_, ()>;
    fn close(&self, id: SessionId) -> LifecycleFuture<'_, ()>;
}

const SESSION_PAGE_SIZE: usize = 50;

impl<S> LifecycleOperations for SessionLifecycle<S>
where
    S: SessionLifecycleStore + 'static,
{
    fn create_with_id(
        &self,
        id: SessionId,
        root: std::path::PathBuf,
    ) -> Result<(), LifecycleError> {
        SessionLifecycle::create_with_id(self, id, root).map(|_| ())
    }

    fn list(
        &self,
        cursor: Option<&SessionCursor>,
        root: Option<&Path>,
    ) -> Result<SessionPage, LifecycleError> {
        SessionLifecycle::list(self, cursor, SESSION_PAGE_SIZE, root)
    }

    fn load<'a>(&'a self, id: SessionId, root: &'a Path) -> LifecycleFuture<'a, SessionAttachment> {
        Box::pin(SessionLifecycle::load(self, id, root))
    }

    fn resume<'a>(
        &'a self,
        id: SessionId,
        root: &'a Path,
    ) -> LifecycleFuture<'a, SessionAttachment> {
        Box::pin(SessionLifecycle::resume(self, id, root))
    }

    fn fork(
        &self,
        id: SessionId,
    ) -> LifecycleFuture<'_, ego_core::session_lifecycle::CreatedSession> {
        Box::pin(SessionLifecycle::fork(self, id))
    }

    fn delete(&self, id: SessionId) -> LifecycleFuture<'_, ()> {
        Box::pin(async move { SessionLifecycle::delete(self, id).await.map(|_| ()) })
    }

    fn close(&self, id: SessionId) -> LifecycleFuture<'_, ()> {
        Box::pin(async move { SessionLifecycle::close(self, id).await.map(|_| ()) })
    }
}

fn parse_session(id: &v1::SessionId) -> Result<SessionId, Error> {
    id.to_string()
        .parse::<SessionId>()
        .map_err(|_| Error::invalid_params().data(format!("invalid ACP session id {id}")))
}

fn lifecycle_error(error: impl fmt::Display) -> Error {
    Error::invalid_params().data(error.to_string())
}

fn session_info(meta: SessionMeta) -> v1::SessionInfo {
    v1::SessionInfo::new(meta.id.to_string(), meta.workspace_root).title(meta.title)
}

fn emit_info(
    connection: &ConnectionTo<Client>,
    id: &v1::SessionId,
    meta: &SessionMeta,
) -> Result<(), Error> {
    connection.send_notification(v1::SessionNotification::new(
        id.clone(),
        v1::SessionUpdate::SessionInfoUpdate(
            v1::SessionInfoUpdate::new().title(meta.title.clone()),
        ),
    ))
}

fn emit_attachment(
    connection: &ConnectionTo<Client>,
    id: &v1::SessionId,
    attachment: &SessionAttachment,
) -> Result<(), Error> {
    for update in project_replay_v1(&attachment.replay, attachment.context_window) {
        connection.send_notification(v1::SessionNotification::new(id.clone(), update))?;
    }
    emit_info(connection, id, &attachment.meta)
}

/// Build the ACP router over one concrete core controller.
pub fn agent<S, E, C, I>(
    controller: Arc<RunController<S, E, C, I>>,
    roots: Arc<dyn RootResolver>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
    mcp_admitter: Arc<dyn McpSessionAdmitter>,
) -> AgentProtocolRouter
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    Agent.protocol_router().with_v1(v1_agent(
        controller,
        roots,
        spawner,
        ids,
        bindings_builder,
        mcp_admitter,
        None,
        None,
    ))
}

/// Build the ACP router composed with the shared durable session lifecycle.
pub fn durable_agent<S, E, C, I>(
    controller: Arc<RunController<S, E, C, I>>,
    lifecycle: Arc<SessionLifecycle<S>>,
    roots: Arc<dyn RootResolver>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
    mcp_admitter: Arc<dyn McpSessionAdmitter>,
) -> AgentProtocolRouter
where
    S: SessionLifecycleStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    Agent.protocol_router().with_v1(v1_agent(
        controller,
        roots,
        spawner,
        ids,
        bindings_builder,
        mcp_admitter,
        None,
        Some(lifecycle),
    ))
}

/// Build the production ACP router with the versioned compaction extension.
pub fn agent_with_compaction<S, E, C, I>(
    controller: Arc<RunController<S, E, C, I>>,
    roots: Arc<dyn RootResolver>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
    mcp_admitter: Arc<dyn McpSessionAdmitter>,
    compaction: Arc<dyn CompactionCommand>,
) -> AgentProtocolRouter
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    Agent.protocol_router().with_v1(v1_agent(
        controller,
        roots,
        spawner,
        ids,
        bindings_builder,
        mcp_admitter,
        Some(compaction),
        None,
    ))
}

/// Build the durable ACP router with the versioned compaction extension.
#[allow(clippy::too_many_arguments)]
pub fn durable_agent_with_compaction<S, E, C, I>(
    controller: Arc<RunController<S, E, C, I>>,
    lifecycle: Arc<SessionLifecycle<S>>,
    roots: Arc<dyn RootResolver>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
    mcp_admitter: Arc<dyn McpSessionAdmitter>,
    compaction: Arc<dyn CompactionCommand>,
) -> AgentProtocolRouter
where
    S: SessionLifecycleStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    Agent.protocol_router().with_v1(v1_agent(
        controller,
        roots,
        spawner,
        ids,
        bindings_builder,
        mcp_admitter,
        Some(compaction),
        Some(lifecycle),
    ))
}

#[allow(clippy::too_many_arguments)]
fn v1_agent<S, E, C, I>(
    controller: Arc<RunController<S, E, C, I>>,
    roots: Arc<dyn RootResolver>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
    mcp_admitter: Arc<dyn McpSessionAdmitter>,
    compaction: Option<Arc<dyn CompactionCommand>>,
    lifecycle: Option<Arc<dyn LifecycleOperations>>,
) -> impl agent_client_protocol::ConnectTo<Client>
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    let sessions = Sessions::default();
    let client_capabilities = Arc::new(Mutex::new(CapabilityCapture::default()));
    let advertises_compaction = compaction.is_some();
    let advertises_lifecycle = lifecycle.is_some();
    Agent
        .builder()
        .name("ego")
        .on_receive_request(
            {
                let client_capabilities = Arc::clone(&client_capabilities);
                async move |request: v1::InitializeRequest, responder, _cx| {
                    let Some(answer) = negotiate(request.protocol_version) else {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!(
                                "this agent speaks ACP protocol version {SERVED}, and {} is below it",
                                request.protocol_version
                            ),
                        ));
                    };
                    client_capabilities
                        .lock()
                        .expect("initialize capability lock poisoned")
                        .capture_initialize(&request.client_capabilities);
                    responder.respond(
                        v1::InitializeResponse::new(answer)
                            .agent_capabilities(capabilities(
                                advertises_compaction,
                                advertises_lifecycle,
                            ))
                            .agent_info(Some(v1::Implementation::new(
                                "ego",
                                env!("CARGO_PKG_VERSION"),
                            ))),
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                let lifecycle = lifecycle.clone();
                async move |request: v1::NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                    if !request.additional_directories.is_empty() {
                        return responder.respond_with_error(Error::invalid_params().data(
                            "this agent does not support additionalDirectories",
                        ));
                    }
                    let root = match roots.resolve(&request.cwd) {
                        Ok(root) => root,
                        Err(error) => {
                            return responder.respond_with_error(
                                Error::invalid_params().data(error.to_string()),
                            );
                        }
                    };
                    let mcp = match admit_mcp(mcp_admitter.as_ref(), request.mcp_servers).await {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let start_request = RequestId::from_uuid(ids.next_uuid());
                    let session = RequestId::session_id(start_request);
                    let id = v1::SessionId::new(session.to_string());
                    if let Some(lifecycle) = &lifecycle
                        && let Err(error) =
                            lifecycle.create_with_id(session, root.as_path().to_path_buf())
                    {
                        return responder.respond_with_error(lifecycle_error(error));
                    }
                    let attachment_root = root.clone();
                    let _ = sessions.open(id.clone(), root, Some(start_request), None, mcp);
                    if let Some(lifecycle) = &lifecycle {
                        match lifecycle.resume(session, attachment_root.as_path()).await {
                            Ok(attachment) => {
                                if let Err(error) = emit_info(&cx, &id, &attachment.meta) {
                                    return responder.respond_with_error(error);
                                }
                            }
                            Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                        }
                    }
                    responder.respond(v1::NewSessionResponse::new(id))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let roots = Arc::clone(&roots);
                async move |request: v1::ListSessionsRequest, responder, _cx| {
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let root = match request.cwd.as_deref().map(|cwd| roots.resolve(cwd)).transpose() {
                        Ok(root) => root,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let cursor = match request.cursor.map(SessionCursor::parse).transpose() {
                        Ok(cursor) => cursor,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    match lifecycle.list(cursor.as_ref(), root.as_ref().map(RunRoot::as_path)) {
                        Ok(page) => responder.respond(
                            v1::ListSessionsResponse::new(
                                page.items.into_iter().map(session_info).collect(),
                            )
                            .next_cursor(page.next_cursor.map(|cursor| cursor.to_string())),
                        ),
                        Err(error) => responder.respond_with_error(lifecycle_error(error)),
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                let controller = Arc::clone(&controller);
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                let spawner = Arc::clone(&spawner);
                let bindings_builder = Arc::clone(&bindings_builder);
                let client_capabilities = Arc::clone(&client_capabilities);
                async move |request: v1::LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                    if !request.additional_directories.is_empty() {
                        return responder.respond_with_error(Error::invalid_params().data(
                            "this agent does not support additionalDirectories",
                        ));
                    }
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let id = match parse_session(&request.session_id) {
                        Ok(id) => id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    if sessions.is_busy(&request.session_id) {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!("session {} already has a turn running", request.session_id),
                        ));
                    }
                    let root = match roots.resolve(&request.cwd) {
                        Ok(root) => root,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let attachment = match lifecycle.load(id, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let mcp = match admit_mcp(mcp_admitter.as_ref(), request.mcp_servers).await {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let capabilities = *client_capabilities
                        .lock()
                        .expect("initialize capability lock poisoned");
                    let bindings = session_bindings(
                        bindings_builder.as_ref(),
                        cx.clone(),
                        request.session_id.clone(),
                        capabilities,
                        mcp.as_ref(),
                    );
                    let recovered = match recover_current_run(controller.as_ref(), id, bindings).await {
                        Ok(run) => run,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let Some(generation) = sessions.open(
                        request.session_id.clone(),
                        root,
                        recovered.run.is_none().then(|| RequestId::from_uuid(ids.next_uuid())),
                        recovered.run,
                        mcp,
                    ) else {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!("session {} already has a turn running", request.session_id),
                        ));
                    };
                    if let Some((key, subscription, execution)) = recovered.active {
                        let (run_id, revision) = recovered.run.expect("active recovery has a run");
                        sessions.install_admission(
                            &request.session_id,
                            generation,
                            run_id,
                            revision,
                            key,
                        );
                        let controller = Arc::clone(&controller);
                        spawner.spawn(Box::pin(async move {
                            if let Err(error) = controller.execute(execution).await {
                                tracing::warn!(%error, "reattached ACP execution failed");
                            }
                        }));
                        let watched = sessions.clone();
                        let watched_id = request.session_id.clone();
                        spawner.spawn(Box::pin(async move {
                            let snapshot = subscription.await;
                            watched.finish(&watched_id, generation, &snapshot);
                        }));
                    }
                    if let Err(error) = emit_attachment(&cx, &request.session_id, &attachment) {
                        return responder.respond_with_error(error);
                    }
                    responder.respond(v1::LoadSessionResponse::new())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                let controller = Arc::clone(&controller);
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                let spawner = Arc::clone(&spawner);
                let bindings_builder = Arc::clone(&bindings_builder);
                let client_capabilities = Arc::clone(&client_capabilities);
                async move |request: v1::ResumeSessionRequest, responder, cx: ConnectionTo<Client>| {
                    if !request.additional_directories.is_empty() {
                        return responder.respond_with_error(Error::invalid_params().data(
                            "this agent does not support additionalDirectories",
                        ));
                    }
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let id = match parse_session(&request.session_id) {
                        Ok(id) => id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    if sessions.is_busy(&request.session_id) {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!("session {} already has a turn running", request.session_id),
                        ));
                    }
                    let root = match roots.resolve(&request.cwd) {
                        Ok(root) => root,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let attachment = match lifecycle.resume(id, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let mcp = match admit_mcp(mcp_admitter.as_ref(), request.mcp_servers).await {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let capabilities = *client_capabilities
                        .lock()
                        .expect("initialize capability lock poisoned");
                    let bindings = session_bindings(
                        bindings_builder.as_ref(),
                        cx.clone(),
                        request.session_id.clone(),
                        capabilities,
                        mcp.as_ref(),
                    );
                    let recovered = match recover_current_run(controller.as_ref(), id, bindings).await {
                        Ok(run) => run,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let Some(generation) = sessions.open(
                        request.session_id.clone(),
                        root,
                        recovered.run.is_none().then(|| RequestId::from_uuid(ids.next_uuid())),
                        recovered.run,
                        mcp,
                    ) else {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!("session {} already has a turn running", request.session_id),
                        ));
                    };
                    if let Some((key, subscription, execution)) = recovered.active {
                        let (run_id, revision) = recovered.run.expect("active recovery has a run");
                        sessions.install_admission(
                            &request.session_id,
                            generation,
                            run_id,
                            revision,
                            key,
                        );
                        let controller = Arc::clone(&controller);
                        spawner.spawn(Box::pin(async move {
                            if let Err(error) = controller.execute(execution).await {
                                tracing::warn!(%error, "reattached ACP execution failed");
                            }
                        }));
                        let watched = sessions.clone();
                        let watched_id = request.session_id.clone();
                        spawner.spawn(Box::pin(async move {
                            let snapshot = subscription.await;
                            watched.finish(&watched_id, generation, &snapshot);
                        }));
                    }
                    if let Err(error) = emit_attachment(&cx, &request.session_id, &attachment) {
                        return responder.respond_with_error(error);
                    }
                    responder.respond(v1::ResumeSessionResponse::new())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                async move |request: v1::ForkSessionRequest, responder, cx: ConnectionTo<Client>| {
                    if !request.additional_directories.is_empty() {
                        return responder.respond_with_error(Error::invalid_params().data(
                            "this agent does not support additionalDirectories",
                        ));
                    }
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let source = match parse_session(&request.session_id) {
                        Ok(id) => id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let root = match roots.resolve(&request.cwd) {
                        Ok(root) => root,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    if let Err(error) = lifecycle.resume(source, root.as_path()).await {
                        return responder.respond_with_error(lifecycle_error(error));
                    }
                    // Admission precedes publication of the child so an MCP
                    // refusal cannot leave an unexplained durable fork. If
                    // the later lifecycle transition refuses, this temporary
                    // connection authority is dropped without attachment.
                    let mcp = match admit_mcp(mcp_admitter.as_ref(), request.mcp_servers).await {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let child = match lifecycle.fork(source).await {
                        Ok(child) => child.id,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let attachment = match lifecycle.resume(child, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => return responder.respond_with_error(lifecycle_error(error)),
                    };
                    let child_id = v1::SessionId::new(child.to_string());
                    let _ = sessions.open(
                        child_id.clone(),
                        root,
                        Some(RequestId::from_uuid(ids.next_uuid())),
                        None,
                        mcp,
                    );
                    if let Err(error) = emit_info(&cx, &child_id, &attachment.meta) {
                        sessions.close(&child_id);
                        return responder.respond_with_error(error);
                    }
                    responder.respond(v1::ForkSessionResponse::new(child_id))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                async move |request: v1::DeleteSessionRequest, responder, _cx| {
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let id = match parse_session(&request.session_id) {
                        Ok(id) => id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    match lifecycle.delete(id).await {
                        Ok(()) => {
                            sessions.close(&request.session_id);
                            responder.respond(v1::DeleteSessionResponse::new())
                        }
                        Err(error) => responder.respond_with_error(lifecycle_error(error)),
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                async move |request: v1::CloseSessionRequest, responder, _cx| {
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let id = match parse_session(&request.session_id) {
                        Ok(id) => id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    match lifecycle.close(id).await {
                        Ok(()) => {
                            sessions.close(&request.session_id);
                            responder.respond(v1::CloseSessionResponse::new())
                        }
                        Err(error) => responder.respond_with_error(lifecycle_error(error)),
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = sessions.clone();
                let controller = Arc::clone(&controller);
                async move |notification: v1::CancelNotification, _cx| {
                    if let Some(key) = sessions.cancel_key(&notification.session_id)
                        && let Err(error) = controller.cancel_admitted(&key).await
                    {
                        tracing::warn!(%error, "ACP cancellation could not be applied");
                    }
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let controller = Arc::clone(&controller);
                let spawner = Arc::clone(&spawner);
                let ids = Arc::clone(&ids);
                let bindings_builder = Arc::clone(&bindings_builder);
                let client_capabilities = Arc::clone(&client_capabilities);
                async move |request: v1::PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let id = request.session_id.clone();
                    let durable_session = match parse_session(&id) {
                        Ok(id) => id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    // Serialized, so a numeric `41` and a string `"41"` stay
                    // two different requests exactly as JSON-RPC says they are.
                    let command = serde_json::to_string(responder.id())
                        .expect("a JSON-RPC request id is serializable");
                    let digest = serde_json::to_string(&request)
                        .expect("ACP request serialization is infallible");
                    let correlation = match sessions.begin(&id, ids.as_ref(), &command, &digest) {
                        BeginPrompt::Ready(correlation) => correlation,
                        BeginPrompt::Busy => {
                            return responder.respond_with_error(Error::invalid_request().data(
                                format!(
                                    "session {id} already has a turn running, and this agent runs one turn per session: wait for the prompt in flight to answer before sending the next"
                                ),
                            ));
                        }
                        BeginPrompt::Unknown => {
                            return responder.respond_with_error(Error::invalid_params().data(
                                format!(
                                    "there is no open session {id}: create one with `session/new` first"
                                ),
                            ));
                        }
                    };
                    let text = match prompt_text(&request.prompt) {
                        Ok(text) => text,
                        Err(refusal) => {
                            sessions.refuse(&id, correlation.generation);
                            return responder.respond_with_error(
                                Error::invalid_params().data(refusal),
                            );
                        }
                    };
                    let digest = RequestDigest::parse(&digest)
                        .expect("a serialized ACP prompt request is non-empty");
                    let capabilities = *client_capabilities
                        .lock()
                        .expect("initialize capability lock poisoned");
                    let bindings = bindings_builder.build(cx.clone(), id.clone(), capabilities);
                    let bindings = match correlation.mcp.as_ref() {
                        Some(mcp) => bindings.with_mcp(
                            mcp.admitted_tools(),
                            mcp.mcp_errors().to_vec(),
                            mcp.mcp_runtime(),
                        ),
                        None => bindings,
                    };
                    let outcome = match correlation.run {
                        None => {
                            controller
                                .start(
                                    StartRequest::new(
                                        durable_session,
                                        correlation.root,
                                        correlation.request_id,
                                        digest,
                                        RunSelections::empty(),
                                        text,
                                    ),
                                    bindings,
                                )
                                .await
                        }
                        Some((run_id, revision)) => {
                            controller
                                .continue_run(
                                    ContinueRequest::new(
                                        run_id,
                                        correlation.request_id,
                                        digest,
                                        revision,
                                        text,
                                    ),
                                    bindings,
                                )
                                .await
                        }
                    };
                    let (subscription, execution) = match outcome {
                        Ok(RunCommandOutcome::Admitted(admission)) => {
                            let subscription = admission.subscription();
                            sessions.install_admission(
                                &id,
                                correlation.generation,
                                admission.run_id(),
                                admission.revision(),
                                admission.cancel_key().clone(),
                            );
                            (subscription, Some(admission.into_execution()))
                        }
                        Ok(RunCommandOutcome::Replayed(observation)) => {
                            (observation.subscription(), None)
                        }
                        Err(error) => {
                            sessions.refuse(&id, correlation.generation);
                            return responder.respond_with_error(
                                Error::internal_error().data(error.to_string()),
                            );
                        }
                    };
                    // The execution settles the admitted revision before it
                    // returns, so the awaited subscription can only report
                    // *that* the turn failed. The diagnostic that names *why*
                    // travels beside it and is answered to the same request.
                    let (diagnostic_tx, diagnostic_rx) = oneshot::channel();
                    if let Some(execution) = execution {
                        let controller = Arc::clone(&controller);
                        spawner.spawn(Box::pin(async move {
                            let outcome = controller.execute(execution).await;
                            if let Err(error) = &outcome {
                                tracing::warn!(%error, "admitted ACP execution failed");
                            }
                            let _ = diagnostic_tx.send(outcome.err().map(|error| error.to_string()));
                        }));
                    }
                    let sessions = sessions.clone();
                    let generation = correlation.generation;
                    cx.spawn(async move {
                        let snapshot = subscription.await;
                        sessions.finish(&id, generation, &snapshot);
                        match prompt_result(&snapshot) {
                            Ok(stop_reason) => {
                                responder.respond(v1::PromptResponse::new(stop_reason))
                            }
                            Err(message) => {
                                let diagnostic = diagnostic_rx
                                    .await
                                    .ok()
                                    .flatten()
                                    .unwrap_or_else(|| message.clone());
                                responder.respond_with_error(
                                    Error::new(i32::from(ErrorCode::InternalError), message)
                                        .data(diagnostic),
                                )
                            }
                        }
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let compaction = compaction.clone();
                move |request: AcpCompactRequest,
                      responder: Responder<serde_json::Value>,
                      _cx| {
                    let compaction = compaction.clone();
                    async move {
                    let Some(compaction) = compaction else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    if request.v != 1 {
                        return responder.respond_with_error(
                            Error::invalid_params().data("unsupported _ego/compact version"),
                        );
                    }
                    let source = match request.session_id.parse::<SessionId>() {
                        Ok(source) if !source.uuid().is_nil() => source,
                        _ => {
                            return responder.respond_with_error(
                                Error::invalid_params().data("invalid compaction sessionId"),
                            );
                        }
                    };
                    let request_id = match request.request_id.parse::<RequestId>() {
                        Ok(request_id) if !request_id.uuid().is_nil() => request_id,
                        _ => {
                            return responder.respond_with_error(
                                Error::invalid_params().data("invalid compaction requestId"),
                            );
                        }
                    };
                    match compaction
                        .compact(CompactionTransitionRequest { request_id, source })
                        .await
                    {
                        Ok(receipt) => match serde_json::to_value(AcpCompactResponse::from(receipt))
                        {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(
                                Error::internal_error().data(error.to_string()),
                            ),
                        },
                        Err(error) => responder.respond_with_error(
                            Error::internal_error().data(error.to_string()),
                        ),
                    }
                    }
                }
            },
            on_receive_request!(),
        )
}
