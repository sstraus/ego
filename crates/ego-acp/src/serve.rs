//! The inbound ACP adapter over core's rooted run controller.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::io;
use std::num::NonZeroUsize;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::negotiate::{SERVED, negotiate};
use crate::project::{
    AcpNotice, project_goal_plan_v1, project_notice_v1, project_replay_v1, stop_reason_v1,
};
use crate::question::{AcpQuestionAdapter, CapabilityCapture};
use agent_client_protocol::{
    Agent, AgentProtocolRouter, ByteStreams, Client, ConnectTo, ConnectionTo, Error, ErrorCode,
    JsonRpcMessage, JsonRpcRequest, Responder, UntypedMessage, on_receive_notification,
    on_receive_request,
};
use agent_client_protocol_http::{AcpHttpServer, CorsOptions, ServerOptions};
use agent_client_protocol_schema::v1;
use axum::extract::{Request as AxumRequest, State};
use axum::http::header::{AUTHORIZATION, CONNECTION, UPGRADE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response as AxumResponse};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ego_core::compact::{CompactionCommand, CompactionReceipt, CompactionTransitionRequest};
use ego_core::config::Trust;
use ego_core::control::{ControlError, SessionControlChange, SessionControlState, SessionControls};
use ego_core::endpoint::{Admitted, Authentication};
use ego_core::event::id::ItemId;
use ego_core::event::id::SessionId;
use ego_core::event::time::Clock;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::generator::types::{Image, ImageMediaType};
use ego_core::mcp::McpRuntimeBinding;
use ego_core::permit::{Confinement, ExecObservation};
use ego_core::question::{
    Elicitor, QuestionAnswer, QuestionMode, QuestionOption, QuestionOutcome, QuestionRequest,
};
use ego_core::run::{
    CancelKey, ContinueRequest, EventSinkFactory, IdSource, RequestDigest, RequestId,
    RunCommandOutcome, RunController, RunError, RunExecution, RunExecutionBindings, RunExecutor,
    RunPrompt, RunRecoveryOutcome, RunRoot, RunSelections, RunSettlement, RunSnapshot, RunStatus,
    RunSubscription, StartRequest,
};
use ego_core::session_lifecycle::{
    HoldState, LifecycleError, SessionAttachment, SessionCursor, SessionLifecycle,
    SessionLifecycleStore, SessionLineageInfo, SessionPage,
};
use ego_core::sink::EventSink;
use ego_core::store::session_store::{SessionMeta, SessionProfiles, SessionStore, StoreError};
use ego_core::turn::{AdmittedToolSource, HostClient};
use ego_listen::{Bound, ConnectionGate, GuardedAccepted, Peer, serve_connections};
use futures::channel::oneshot;
use futures::future::Either;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serde::{Deserialize, Serialize};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

#[path = "prompt_turn.rs"]
mod prompt_turn;
#[path = "resume.rs"]
mod resume;
use resume::{PromptHandler, PromptReply, ResumeQueue};

/// The live ACP delivery boundary for notices produced outside a model turn.
#[derive(Clone, Default)]
pub struct AcpNotices {
    connections: Arc<Mutex<HashMap<v1::SessionId, NoticeConnection>>>,
    recorder: Arc<Mutex<Option<Arc<NoticeRecorder>>>>,
}

type NoticeRecorder = dyn Fn(
        ego_core::event::id::SessionId,
        ego_core::store::session_store::NoticeDelivery,
    ) -> Result<(), String>
    + Send
    + Sync;

struct NoticeConnection {
    owner: Arc<()>,
    connection: ConnectionTo<Client>,
    closed_watcher: tokio::task::AbortHandle,
    resume: ResumeQueue,
    resume_worker: tokio::task::AbortHandle,
}

impl Drop for NoticeConnection {
    fn drop(&mut self) {
        self.closed_watcher.abort();
        self.resume_worker.abort();
    }
}

impl AcpNotices {
    /// Bind the session journal owned by the host's composition root.
    pub fn set_delivery_recorder(&self, recorder: Arc<NoticeRecorder>) {
        *self.recorder.lock().expect("notice recorder lock poisoned") = Some(recorder);
    }
    pub fn connected(&self, session: &v1::SessionId) -> bool {
        self.connections
            .lock()
            .expect("ACP notice connection lock poisoned")
            .get(session)
            .is_some_and(|seat| !seat.connection.is_incoming_closed())
    }

    /// Caller reserves durability before enqueueing. A disconnected seat refuses work.
    pub fn resume(
        &self,
        session: &v1::SessionId,
        id: String,
        prompt: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, Error> {
        let connections = self
            .connections
            .lock()
            .expect("ACP notice connection lock poisoned");
        let seat = connections
            .get(session)
            .filter(|seat| !seat.connection.is_incoming_closed())
            .ok_or_else(|| Error::from(ErrorCode::ResourceNotFound))?;
        Ok(seat.resume.push(id, prompt))
    }

    fn register(
        &self,
        session: v1::SessionId,
        connection: ConnectionTo<Client>,
        sessions: Sessions,
        handler: PromptHandler,
    ) {
        let (resume, resume_worker) =
            ResumeQueue::start(session.clone(), connection.clone(), sessions, handler);
        let owner = Arc::new(());
        let notices = self.clone();
        let watched_session = session.clone();
        let watched_owner = Arc::clone(&owner);
        let watched_connection = connection.clone();
        let closed_watcher = tokio::spawn(async move {
            watched_connection.incoming_closed().await;
            notices.unregister_if_owner(&watched_session, &watched_owner);
        })
        .abort_handle();
        let previous = self
            .connections
            .lock()
            .expect("ACP notice connection lock poisoned")
            .insert(
                session.clone(),
                NoticeConnection {
                    owner: Arc::clone(&owner),
                    connection: connection.clone(),
                    closed_watcher,
                    resume,
                    resume_worker,
                },
            );
        if let Some(previous) = previous {
            previous.closed_watcher.abort();
        }
        if connection.is_incoming_closed() {
            self.unregister_if_owner(&session, &owner);
        }
    }

    fn unregister(&self, session: &v1::SessionId) {
        let removed = self
            .connections
            .lock()
            .expect("ACP notice connection lock poisoned")
            .remove(session);
        if let Some(removed) = removed {
            removed.closed_watcher.abort();
        }
    }

    fn unregister_if_owner(&self, session: &v1::SessionId, owner: &Arc<()>) {
        let removed = {
            let mut connections = self
                .connections
                .lock()
                .expect("ACP notice connection lock poisoned");
            if connections
                .get(session)
                .is_some_and(|current| Arc::ptr_eq(&current.owner, owner))
            {
                connections.remove(session)
            } else {
                None
            }
        };
        if let Some(removed) = removed {
            removed.closed_watcher.abort();
        }
    }

    /// Publish a normal ACP update even when no `session/prompt` is active.
    pub fn publish(&self, session: &v1::SessionId, notice: &AcpNotice) -> Result<(), Error> {
        let mut connections = self
            .connections
            .lock()
            .expect("ACP notice connection lock poisoned");
        let Some(registered) = connections.get(session) else {
            return Err(Error::from(ErrorCode::ResourceNotFound));
        };
        if registered.connection.is_incoming_closed() {
            if let Some(removed) = connections.remove(session) {
                removed.closed_watcher.abort();
            }
            return Err(Error::from(ErrorCode::ResourceNotFound));
        }
        let recorder = self
            .recorder
            .lock()
            .expect("notice recorder lock poisoned")
            .clone()
            .ok_or_else(|| {
                Error::internal_error().data("notice delivery journal is not configured")
            })?;
        let session_id = session
            .to_string()
            .parse()
            .map_err(|_| Error::invalid_params().data("invalid notice session id"))?;
        let sent = registered
            .connection
            .send_notification(v1::SessionNotification::new(
                session.clone(),
                project_notice_v1(notice),
            ));
        if sent.is_err()
            && let Some(removed) = connections.remove(session)
        {
            removed.closed_watcher.abort();
        }
        sent?;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| Error::internal_error().data(error.to_string()))?;
        let delivered_at = ego_core::event::time::Timestamp::from_millis(
            u64::try_from(at.as_millis())
                .map_err(|error| Error::internal_error().data(error.to_string()))?,
        );
        recorder(
            session_id,
            ego_core::store::session_store::NoticeDelivery {
                card_id: notice.id.clone(),
                source_path: match &notice.action {
                    crate::project::NoticeAction::OpenResult { path } => Some(path.clone()),
                    _ => None,
                },
                delivered_at,
            },
        )
        .map_err(|error| {
            Error::internal_error().data(format!(
                "notice was queued but delivery journal failed: {error}"
            ))
        })
    }
}

use crate::auth::{Credential, TOKEN_VAR};

const ACP_PATH: &str = "/acp";

/// Serve ACP on one endpoint already opened by the shared listener.
///
/// Local endpoints use the SDK's newline-delimited [`ByteStreams`] directly
/// after operating-system peer authentication. TCP endpoints use the official
/// HTTP/WebSocket adapter, with bearer and WebSocket admission outside its
/// router so no protocol component exists for a refused request.
pub async fn serve_on<F, C, Shutdown>(
    bound: Bound,
    admitted: &Admitted,
    credential: Option<Credential>,
    gate: ConnectionGate,
    factory: F,
    shutdown: Shutdown,
) -> io::Result<()>
where
    F: Fn() -> C + Send + Sync + 'static,
    C: ConnectTo<Client>,
    Shutdown: Future<Output = ()> + Send,
{
    match admitted.authentication() {
        Authentication::PeerIdentity => serve_local(bound, gate, factory, shutdown).await,
        Authentication::BearerCredential => {
            let credential = credential.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{admitted} requires an ACP bearer credential before serving: set {TOKEN_VAR}"
                    ),
                )
            })?;
            serve_network(bound, gate, credential, factory, shutdown).await
        }
    }
}

async fn serve_local<F, C, Shutdown>(
    bound: Bound,
    gate: ConnectionGate,
    factory: F,
    shutdown: Shutdown,
) -> io::Result<()>
where
    F: Fn() -> C + Send + Sync + 'static,
    C: ConnectTo<Client>,
    Shutdown: Future<Output = ()> + Send,
{
    let factory = Arc::new(factory);
    serve_connections(
        bound,
        gate,
        move |accepted: GuardedAccepted| {
            let factory = Arc::clone(&factory);
            async move {
                if accepted.peer != Peer::CurrentUser {
                    tracing::warn!(
                        target: "ego::acp",
                        "an ACP local connection lacked operating-system peer authentication"
                    );
                    return;
                }
                let (incoming, outgoing) = tokio::io::split(accepted.stream);
                let transport = ByteStreams::new(outgoing.compat_write(), incoming.compat());
                if let Err(error) = factory().connect_to(transport).await {
                    tracing::debug!(%error, "an ACP local connection ended badly");
                }
            }
        },
        shutdown,
    )
    .await
}

#[derive(Clone)]
struct NetworkAdmission {
    credential: Arc<Credential>,
}

async fn admit_network_request(
    State(admission): State<NetworkAdmission>,
    request: AxumRequest,
    next: Next,
) -> AxumResponse {
    if !presents_exactly_one_credential(request.headers(), &admission.credential) {
        return unauthorized();
    }
    if request.uri().path() != ACP_PATH {
        return StatusCode::NOT_FOUND.into_response();
    }
    if request.method() != Method::GET
        || !header_contains_token(request.headers(), CONNECTION, b"upgrade")
        || !header_contains_token(request.headers(), UPGRADE, b"websocket")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    next.run(request).await
}

fn presents_exactly_one_credential(headers: &HeaderMap, credential: &Credential) -> bool {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    values.next().is_none() && credential.admits(value.as_bytes())
}

fn header_contains_token(headers: &HeaderMap, name: HeaderName, expected: &[u8]) -> bool {
    headers.get_all(name).iter().any(|value| {
        value
            .as_bytes()
            .split(|byte| *byte == b',')
            .map(trim_ascii_whitespace)
            .any(|token| token.eq_ignore_ascii_case(expected))
    })
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn unauthorized() -> AxumResponse {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

async fn serve_network<F, C, Shutdown>(
    bound: Bound,
    gate: ConnectionGate,
    credential: Credential,
    factory: F,
    shutdown: Shutdown,
) -> io::Result<()>
where
    F: Fn() -> C + Send + Sync + 'static,
    C: ConnectTo<Client>,
    Shutdown: Future<Output = ()> + Send,
{
    let router = AcpHttpServer::new(factory)
        .with_options(ServerOptions {
            path: ACP_PATH.to_owned(),
            cors: CorsOptions::disabled(),
            health_endpoint: false,
        })
        .into_router()
        .layer(middleware::from_fn_with_state(
            NetworkAdmission {
                credential: Arc::new(credential),
            },
            admit_network_request,
        ));

    serve_connections(
        bound,
        gate,
        move |accepted: GuardedAccepted| {
            let service = TowerToHyperService::new(router.clone());
            async move {
                if let Err(error) = http1::Builder::new()
                    .serve_connection(TokioIo::new(accepted.stream), service)
                    .with_upgrades()
                    .await
                {
                    tracing::debug!(%error, "an ACP WebSocket connection ended badly");
                }
            }
        },
        shutdown,
    )
    .await
}

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
    /// The confinement this session's MCP processes were started under, when
    /// the admission owns any. A later pick of another sandbox is refused: a
    /// server admitted under one confinement cannot be moved into another.
    fn pinned_confinement(&self) -> Option<Confinement> {
        None
    }
    fn cancel_active(&self) {}
    fn close(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

pub type McpSessionAdmissionFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError>>
            + Send
            + 'static,
    >,
>;

/// The sandbox pick a session's MCP servers are admitted under, with the
/// observation measured for it. The observation is absent only where the
/// connection has no confinement admission to measure it.
#[derive(Clone)]
pub struct McpConfinement {
    pub level: Confinement,
    pub observation: Option<ExecObservation>,
}

/// What a session's MCP admission may rely on. Servers are admitted after the
/// session exists and its confinement is measured, never before: admission
/// starts their processes.
#[derive(Clone)]
pub struct McpAdmissionContext {
    pub session: SessionId,
    pub root: RunRoot,
    /// Absent where the connection has no session controls.
    pub confinement: Option<McpConfinement>,
}

/// Consumer-owned boundary that atomically admits one ACP session's servers.
pub trait McpSessionAdmitter: Send + Sync {
    fn admit(
        &self,
        servers: Vec<v1::McpServer>,
        connection: Option<ConnectionTo<Client>>,
        context: McpAdmissionContext,
    ) -> McpSessionAdmissionFuture;

    fn mcp_notification(
        &self,
        _notification: v1::MessageMcpNotification,
    ) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        None
    }
}

/// Completion of one session's confinement measurement and evidence write.
pub type SessionConfinementFuture =
    Pin<Box<dyn Future<Output = Result<(), ControlError>> + Send + 'static>>;

/// Consumer-owned boundary for measuring and pinning one ACP session's sandbox.
///
/// Initialization is asynchronous because the outer adapter probes a process
/// backend and commits evidence. Observation lookup reads the immutable
/// snapshot installed before this session's options were published.
pub trait SessionConfinementAdmission: Send + Sync {
    /// Resolve optional host profiles before the session starts any effects.
    fn configure_profiles(
        &self,
        _session: SessionId,
        _root: &RunRoot,
        _profile: Option<&str>,
        _ceiling: Option<&str>,
    ) -> Result<serde_json::Value, ControlError> {
        Err(ControlError::Refused {
            control: "profile",
            why: "session profiles are not supported".into(),
        })
    }

    /// Validate and pin per-session prompt selections before creation.
    fn configure_prompt(
        &self,
        _session: SessionId,
        _options: ego_core::config::SessionPromptOptions,
    ) -> Result<(), ControlError> {
        Err(ControlError::Refused {
            control: "systemPrompt",
            why: "session prompt selections are not supported".into(),
        })
    }

    /// Carry admitted profile authority through the durable run boundary.
    fn profile_selections(
        &self,
        _session: SessionId,
        selections: RunSelections,
    ) -> Result<RunSelections, ControlError> {
        Ok(selections)
    }

    fn initialize(&self, session: SessionId, root: RunRoot) -> SessionConfinementFuture;

    fn observation(
        &self,
        session: SessionId,
        confinement: Confinement,
    ) -> Result<ExecObservation, ControlError>;
}

/// The CLI owns the ledger and the executable surface; ACP owns elicitation.
pub enum SessionTrustStatus {
    Empty,
    Remembered(Trust),
    Question {
        content: String,
        fingerprint: String,
    },
}

pub trait SessionTrustAdmission: Send + Sync {
    /// The adapter decides whether the process has a host-selected profile.
    /// Default ports never accept client self-attestation.
    fn attest_trusted(&self, _root: &RunRoot) {}

    fn status(&self, root: &RunRoot) -> SessionTrustStatus;
    /// False means executable authority changed while the question was open.
    fn record_answer(&self, root: &RunRoot, fingerprint: &str, decision: Trust) -> bool;
}

/// Builds live execution bindings from the connection that accepted a prompt.
pub trait RunBindingsBuilder: Send + Sync {
    fn build(
        &self,
        connection: ConnectionTo<Client>,
        session: v1::SessionId,
        capabilities: CapabilityCapture,
        root: &RunRoot,
        session_approval: Arc<AtomicBool>,
    ) -> ego_core::run::RunExecutionBindings;

    /// The session was closed, deleted, loaded or resumed: whatever a person
    /// allowed for "this session" ends with it, like the exec session approval.
    fn end_session(&self, _session: &v1::SessionId) {}
}

/// Production ACP bindings: live updates plus the existing question adapter.
#[derive(Debug, Default)]
pub struct AcpRunBindingsBuilder {
    controls: Option<Arc<dyn SessionControls>>,
    permission_rules: Option<Arc<dyn crate::permission::PermissionRuleWriter>>,
    sandbox_store: Option<Arc<dyn crate::sandbox::SandboxGrantStore>>,
    /// One list per session: a session answer outlives the prompt that asked
    /// and ends with the session (`end_session`). Keyed by the wire session id.
    sandbox_sessions:
        std::sync::Mutex<std::collections::HashMap<String, crate::sandbox::SessionSandboxGrants>>,
}

impl AcpRunBindingsBuilder {
    /// Bind the connection's operator seat, if it owns one.
    ///
    /// The same seat the router answers `session/set_config_option` from, so
    /// what a host was told it picked is what the next generation reads.
    pub fn with_controls(controls: Option<Arc<dyn SessionControls>>) -> Self {
        Self {
            controls,
            permission_rules: None,
            sandbox_store: None,
            sandbox_sessions: std::sync::Mutex::default(),
        }
    }

    /// Keep "allow always" sandbox answers for the workspace in `store`.
    pub fn with_sandbox_store(
        mut self,
        store: Option<Arc<dyn crate::sandbox::SandboxGrantStore>>,
    ) -> Self {
        self.sandbox_store = store;
        self
    }

    pub fn with_permission_rules(
        mut self,
        rules: Option<Arc<dyn crate::permission::PermissionRuleWriter>>,
    ) -> Self {
        self.permission_rules = rules;
        self
    }
}

struct AcpSinkFactory {
    connection: ConnectionTo<Client>,
    session: v1::SessionId,
    root: RunRoot,
}

impl EventSinkFactory for AcpSinkFactory {
    fn execution_policy_notice(&self, message: &str) -> Result<(), String> {
        self.connection
            .send_notification(v1::SessionNotification::new(
                self.session.clone(),
                v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new(
                    message.to_owned().into(),
                )),
            ))
            .map_err(|error| error.to_string())
    }

    fn create(&self, capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(crate::sink::AcpSink::new(
            self.connection.clone(),
            self.session.clone(),
            capabilities.clone(),
            self.root.as_path().to_path_buf(),
        ))
    }
}

impl RunBindingsBuilder for AcpRunBindingsBuilder {
    fn end_session(&self, session: &v1::SessionId) {
        self.sandbox_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session.0.as_ref());
    }

    fn build(
        &self,
        connection: ConnectionTo<Client>,
        session: v1::SessionId,
        capabilities: CapabilityCapture,
        root: &RunRoot,
        session_approval: Arc<AtomicBool>,
    ) -> ego_core::run::RunExecutionBindings {
        let elicitor =
            AcpQuestionAdapter::live(session.clone(), capabilities, connection.clone()).elicitor();
        // The connected client is this turn's only human. Installing the seat
        // here, beside the elicitor, is what stops an escalation from ending
        // as an unanswerable internal error the host cannot act on.
        let human =
            crate::permission::AcpPermissionAdapter::live(session.clone(), connection.clone())
                .with_rule_writer(self.permission_rules.clone())
                .with_session_approval(session_approval)
                .approver();
        let session_grants = Arc::clone(
            self.sandbox_sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(session.0.to_string())
                .or_default(),
        );
        let sandbox_grants = crate::sandbox::AcpSandboxGrants::new(
            session.clone(),
            connection.clone(),
            root.as_path().to_path_buf(),
            self.sandbox_store.clone(),
            session_grants,
        );
        let bindings = ego_core::run::RunExecutionBindings::new(
            Arc::new(AcpSinkFactory {
                connection,
                session,
                root: root.clone(),
            }),
            Some(elicitor),
        )
        .with_human(human)
        .with_sandbox_grants(Arc::new(sandbox_grants));
        match &self.controls {
            Some(controls) => bindings.with_controls(Arc::clone(controls)),
            None => bindings,
        }
    }
}

/// What `durable_agent_with_compaction` puts in its `initialize` answer.
///
/// Exposed so a local surface can say what this build advertises without
/// connecting to it. regression case is why: TUICommander read the pause extension
/// as two separately versioned entries while this crate publishes one `hold`
/// naming both methods, and because nothing local could report the shape, the
/// only way to find the mismatch was to run a second implementation and watch
/// it fail.
///
/// `true, true` is a fact of the signature it mirrors and not a guess:
/// [`durable_agent_with_compaction`] takes `compaction` and `lifecycle` as
/// plain `Arc`s rather than `Option`s, so it passes `Some` for both on every
/// path there is.
pub fn durable_advertisement() -> v1::AgentCapabilities {
    capabilities(true, true)
}

fn capabilities(compaction: bool, lifecycle: bool) -> v1::AgentCapabilities {
    let capabilities = v1::AgentCapabilities::new()
        .load_session(lifecycle)
        // `image` is a statement about this protocol surface: an image block
        // sent here is decoded, bounded and published like an `--attach` would
        // be. It is deliberately not a promise about the model a session ends
        // up on, which `initialize` cannot know — no session exists yet, and a
        // session can change model mid-flight. A model that takes no pictures
        // still refuses, with its own name in the sentence.
        .prompt_capabilities(
            v1::PromptCapabilities::new()
                .image(true)
                .audio(false)
                .embedded_context(false),
        )
        .mcp_capabilities(v1::McpCapabilities::new().http(true).sse(false).acp(true));
    let capabilities = if lifecycle {
        capabilities.session_capabilities(
            v1::SessionCapabilities::new()
                .list(v1::SessionListCapabilities::new())
                .delete(v1::SessionDeleteCapabilities::new())
                .fork(
                    v1::SessionForkCapabilities::new().meta(serde_json::Map::from_iter([(
                        "ego".to_owned(),
                        serde_json::json!({"atMessage": true}),
                    )])),
                )
                .resume(v1::SessionResumeCapabilities::new())
                .close(v1::SessionCloseCapabilities::new()),
        )
    } else {
        capabilities
    };
    // Built entry by entry rather than as one literal, so an extension is
    // advertised exactly when the port that serves it exists. A host that
    // negotiated a build with lifecycle and no compaction is told about
    // `_ego/pause` and not `_ego/compact`, which is the truth; a single
    // literal gated on one flag would have made the two stand or fall
    // together and one of the two claims would always be a lie.
    let mut ego = serde_json::Map::new();
    ego.insert(
        "steer".to_owned(),
        serde_json::json!({"version": 1, "method": "_ego/steer", "contentTypes": ["text"]}),
    );
    if compaction {
        ego.insert(
            "compact".to_owned(),
            serde_json::json!({"version": 1, "method": "_ego/compact"}),
        );
    }
    if lifecycle {
        ego.insert(
            "hold".to_owned(),
            serde_json::json!({
                "version": 1,
                "pause": "_ego/pause",
                "resume": "_ego/resume",
            }),
        );
    }
    if ego.is_empty() {
        return capabilities;
    }
    capabilities.meta(serde_json::Map::from_iter([(
        "ego".to_owned(),
        serde_json::Value::Object(ego),
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

/// Submit text to the currently running turn's durable FIFO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcpSteerRequest {
    v: u32,
    session_id: String,
    prompt: Vec<v1::ContentBlock>,
}

impl JsonRpcMessage for AcpSteerRequest {
    fn matches_method(method: &str) -> bool {
        method == "_ego/steer"
    }
    fn method(&self) -> &str {
        "_ego/steer"
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

impl JsonRpcRequest for AcpSteerRequest {
    type Response = serde_json::Value;
}

/// One `_ego/pause` or `_ego/resume`.
///
/// One type for both because they carry the same three fields and differ
/// only in which method names them. [`JsonRpcMessage::matches_method`] is
/// what separates the two, and a second identical struct would be a second
/// place to keep the versioning rule in step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcpHoldRequest<const RELEASE: bool> {
    v: u32,
    session_id: String,
    request_id: String,
}

const fn hold_method(release: bool) -> &'static str {
    if release { "_ego/resume" } else { "_ego/pause" }
}

impl<const RELEASE: bool> JsonRpcMessage for AcpHoldRequest<RELEASE> {
    fn matches_method(method: &str) -> bool {
        method == hold_method(RELEASE)
    }

    fn method(&self) -> &str {
        hold_method(RELEASE)
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

impl<const RELEASE: bool> JsonRpcRequest for AcpHoldRequest<RELEASE> {
    type Response = serde_json::Value;
}

/// What a hold request found or established.
///
/// The `state` is the whole answer and it is deliberately not a boolean: see
/// [`HoldState`] for why `pending` and `paused` must stay apart on the wire.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcpHoldResponse {
    v: u32,
    session_id: SessionId,
    request_id: RequestId,
    state: HoldState,
}

struct Registered {
    generation: RegistrationGeneration,
    root: RunRoot,
    start_request: Option<RequestId>,
    answered: VecDeque<AnsweredCommand>,
    run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
    busy: bool,
    steering: Option<ego_core::turn::InteractiveIngress>,
    withdrawal: Option<Withdrawal>,
    active_cancel: Option<CancelKey>,
    cancel_requested: bool,
    mcp: Option<Arc<dyn SessionMcpAdmission>>,
    session_approval: Arc<AtomicBool>,
}

/// A `session/cancel` that arrives while a prompt is accepted but the run it
/// names has not been admitted yet.
///
/// There is no [`CancelKey`] in that window and there cannot be one: the key is
/// minted by admission. The flag is the whole point — a cancellation must be
/// remembered even when nobody is waiting on it yet, because the task that
/// will admit the run may not have reached the question, or may be inside
/// `start` and past the point where it could have declined to begin. The
/// channel only *wakes* a waiter; it is the flag, read under the registration
/// lock, that decides.
struct Withdrawal {
    raised: bool,
    wake: Option<oneshot::Sender<()>>,
}

/// What one `session/cancel` found in flight under a session id.
enum Cancellation {
    /// A run is admitted, so the controller owns the cancellation and the key
    /// names it.
    Admitted(CancelKey, Option<Arc<dyn SessionMcpAdmission>>),
    /// A prompt is accepted and not yet admitted: the withdrawal is recorded
    /// against the registration and the prompt task answers it.
    Withdrawn,
    /// Nothing is in flight under this id.
    Nothing,
}

/// A prompt one session accepted, and the command that prompt named.
/// Re-delivering the same protocol request with the same body names that
/// command again, so the run replays its retained receipt instead of admitting
/// a second turn. Both halves matter: JSON-RPC lets a client reuse a request
/// identity it has already been answered on, and a reused identity carrying a
/// different body is new work, not a re-delivery.
struct AnsweredCommand {
    request: String,
    digest: String,
    id: RequestId,
}

struct PromptCorrelation {
    generation: RegistrationGeneration,
    root: RunRoot,
    request_id: RequestId,
    run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
    /// Resolves when this prompt is withdrawn before its run is admitted, and
    /// also when the registration holding the other half is dropped: a session
    /// that closed mid-question is not one to start a run for either.
    withdrawn: oneshot::Receiver<()>,
    mcp: Option<Arc<dyn SessionMcpAdmission>>,
    session_approval: Arc<AtomicBool>,
}

enum BeginPrompt {
    Ready(PromptCorrelation),
    Busy,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RegistrationGeneration(u64);

struct SessionRegistrations {
    next_generation: u64,
    sessions: HashMap<v1::SessionId, Registered>,
    /// How many answered commands one session remembers. Read from the
    /// controller's retention so this window never outruns it: remembering a
    /// command whose receipt the run has already pruned would turn a
    /// re-delivery into a refusal, which is worse than admitting it as new.
    remembered: NonZeroUsize,
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

#[derive(Clone)]
struct Sessions(Arc<Mutex<SessionRegistrations>>, Arc<tokio::sync::Notify>);

impl Sessions {
    fn new(remembered: NonZeroUsize) -> Self {
        Self(
            Arc::new(Mutex::new(SessionRegistrations {
                next_generation: 0,
                sessions: HashMap::new(),
                remembered,
            })),
            Arc::default(),
        )
    }

    fn open(
        &self,
        id: v1::SessionId,
        root: RunRoot,
        start_request: Option<RequestId>,
        run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
        mcp: Option<Arc<dyn SessionMcpAdmission>>,
        session_approval: Arc<AtomicBool>,
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
                answered: VecDeque::new(),
                run,
                busy: false,
                steering: None,
                withdrawal: None,
                active_cancel: None,
                cancel_requested: false,
                mcp,
                session_approval,
            },
        );
        Some(generation)
    }

    /// Serialize the confinement pick with first-prompt admission.
    fn stage_confinement(
        &self,
        id: &v1::SessionId,
        session: SessionId,
        controls: &dyn SessionControls,
        change: SessionControlChange,
    ) -> Result<SessionControlState, Error> {
        let registrations = self.lock();
        let registered = registrations.sessions.get(id).ok_or_else(|| {
            refused_params(format!("there is no open session {id}: create one first"))
        })?;
        if registered.busy || registered.run.is_some() {
            return Err(refused_params(
                "this session has already started preparing a run: pick the sandbox \
                 before the first prompt, or start a new session",
            ));
        }
        if let (Some(pinned), SessionControlChange::Sandbox(pick)) = (
            registered
                .mcp
                .as_ref()
                .and_then(|mcp| mcp.pinned_confinement()),
            &change,
        ) && pinned != *pick
        {
            return Err(refused_params(format!(
                "this session's MCP servers already run under the `{pinned}` sandbox: \
                 pick the sandbox when the session is created, or start a new session"
            )));
        }
        // Keep the registration lock until the synchronous pick has landed.
        // `begin` cannot publish busy and read a different selection between
        // the eligibility check and this write.
        controls
            .stage(session, change)
            .map_err(|error| control_error(&error))
    }

    async fn wait_idle(&self, id: &v1::SessionId) {
        loop {
            let idle = self.1.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if !self.is_busy(id) {
                return;
            }
            idle.await;
        }
    }
    async fn begin_prompt(
        &self,
        id: &v1::SessionId,
        ids: &dyn IdSource,
        command: &str,
        digest: &str,
        resume: bool,
    ) -> BeginPrompt {
        loop {
            if resume {
                self.wait_idle(id).await;
            }
            let begun = self.begin(id, ids, command, digest);
            if resume && matches!(begun, BeginPrompt::Busy) {
                continue;
            }
            return begun;
        }
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
        let remembered = sessions.remembered;
        let session = sessions
            .sessions
            .get_mut(id)
            .expect("the checked ACP session remains registered");
        session.generation = generation;
        session.busy = true;
        session.cancel_requested = false;
        // Minted here, under the lock that publishes `busy`, so there is no
        // instant in which this session is accepting a prompt and a
        // `session/cancel` has nowhere to land.
        let (wake, withdrawn) = oneshot::channel();
        session.withdrawal = Some(Withdrawal {
            raised: false,
            wake: Some(wake),
        });
        let replayed = session
            .answered
            .iter()
            .find(|answered| answered.request == command && answered.digest == digest)
            .map(|answered| answered.id);
        let request_id = match replayed {
            Some(id) => id,
            None if session.run.is_none() => session
                .start_request
                .unwrap_or_else(|| RequestId::from_uuid(ids.next_uuid())),
            None => RequestId::from_uuid(ids.next_uuid()),
        };
        if replayed.is_none() {
            // A re-delivery keeps its place in the queue rather than moving to
            // the back: it produces no new admission, so the run's own pruning
            // order — oldest admission first — is unchanged, and this window
            // has to age the same way or it will outlive the receipt it names.
            session.answered.push_back(AnsweredCommand {
                request: command.to_owned(),
                digest: digest.to_owned(),
                id: request_id,
            });
            while session.answered.len() > remembered.get() {
                session.answered.pop_front();
            }
        }
        BeginPrompt::Ready(PromptCorrelation {
            generation,
            root: session.root.clone(),
            request_id,
            run: session.run,
            withdrawn,
            mcp: session.mcp.as_ref().map(Arc::clone),
            session_approval: Arc::clone(&session.session_approval),
        })
    }

    /// Install the admitted run's cancel key, and say whether this prompt was
    /// already withdrawn.
    ///
    /// Both halves happen under one lock, and that is the contract: a
    /// `session/cancel` that arrives while `start` is writing its transaction
    /// finds no key to name, so it leaves the flag instead, and this is the
    /// only place that can read the flag and publish the key in the same
    /// instant. Split in two, the window between them would swallow exactly
    /// that cancellation.
    fn install_admission(
        &self,
        id: &v1::SessionId,
        generation: RegistrationGeneration,
        run_id: ego_core::run::RunId,
        revision: ego_core::run::RunRevision,
        key: CancelKey,
    ) -> bool {
        let mut sessions = self.lock();
        let Some(session) = sessions
            .sessions
            .get_mut(id)
            .filter(|session| session.generation == generation)
        else {
            return false;
        };
        session.run = Some((run_id, revision));
        session.active_cancel = Some(key);
        session.busy = true;
        let withdrawn = session
            .withdrawal
            .as_ref()
            .is_some_and(|withdrawal| withdrawal.raised);
        // The key answers every later cancellation, so the flag has served its
        // purpose and staying set would make a second prompt read this one's.
        session.withdrawal = None;
        withdrawn
    }

    /// Record a cancellation against whatever this session has in flight.
    fn cancel(&self, id: &v1::SessionId) -> Cancellation {
        let mut sessions = self.lock();
        let Some(session) = sessions.sessions.get_mut(id) else {
            return Cancellation::Nothing;
        };
        if let Some(key) = session.active_cancel.clone() {
            session.cancel_requested = true;
            return Cancellation::Admitted(key, session.mcp.as_ref().map(Arc::clone));
        }
        let Some(withdrawal) = session.withdrawal.as_mut() else {
            return Cancellation::Nothing;
        };
        withdrawal.raised = true;
        if let Some(wake) = withdrawal.wake.take() {
            // A closed receiver means the prompt task is past the point where
            // it waits; the flag above is what it reads then.
            let _ = wake.send(());
        }
        Cancellation::Withdrawn
    }

    fn finish(
        &self,
        id: &v1::SessionId,
        generation: RegistrationGeneration,
        snapshot: &RunSnapshot,
    ) -> bool {
        if let Some(session) = self
            .lock()
            .sessions
            .get_mut(id)
            .filter(|session| session.generation == generation)
        {
            let cancel_requested = session.cancel_requested;
            session.run = Some((snapshot.run_id(), snapshot.revision()));
            session.withdrawal = None;
            session.active_cancel = None;
            session.busy = false;
            session.steering = None;
            self.1.notify_waiters();
            session.cancel_requested = false;
            return cancel_requested;
        }
        false
    }

    fn refuse(&self, id: &v1::SessionId, generation: RegistrationGeneration) {
        if let Some(session) = self
            .lock()
            .sessions
            .get_mut(id)
            .filter(|session| session.generation == generation)
        {
            session.withdrawal = None;
            session.active_cancel = None;
            session.busy = false;
            session.steering = None;
            self.1.notify_waiters();
        }
    }

    fn steering(
        &self,
        id: &v1::SessionId,
    ) -> Result<Option<ego_core::turn::InteractiveIngress>, Error> {
        let registrations = self.lock();
        let session = registrations
            .sessions
            .get(id)
            .ok_or_else(|| refused_params("unknown steering sessionId"))?;
        Ok(session.steering.clone())
    }

    fn install_steering(
        &self,
        id: &v1::SessionId,
        generation: RegistrationGeneration,
        ingress: ego_core::turn::InteractiveIngress,
    ) {
        if let Some(session) = self
            .lock()
            .sessions
            .get_mut(id)
            .filter(|session| session.generation == generation && session.busy)
        {
            session.steering = Some(ingress);
        }
    }

    fn is_busy(&self, id: &v1::SessionId) -> bool {
        self.lock()
            .sessions
            .get(id)
            .is_some_and(|session| session.busy)
    }

    fn close(&self, id: &v1::SessionId) -> Option<Arc<dyn SessionMcpAdmission>> {
        let removed = self
            .lock()
            .sessions
            .remove(id)
            .and_then(|session| session.mcp);
        self.1.notify_waiters();
        removed
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
                let allow_insecure_loopback = url.is_exact_plaintext_loopback();
                ego_core::config::McpServerConfig::http(
                    url.as_url().clone(),
                    ego_core::config::McpHttpAuth::None,
                    allow_insecure_loopback,
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
            v1::McpServer::Acp(server) => {
                if server.server_id.0.is_empty() {
                    return Err(McpSessionAdmissionError::new(
                        "an ACP MCP server id must not be empty",
                    ));
                }
                &server.name
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
    connection: Option<ConnectionTo<Client>>,
    context: McpAdmissionContext,
) -> Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError> {
    validate_mcp_servers(&servers)?;
    let supplied = !servers.is_empty();
    match admitter.admit(servers, connection, context).await {
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

/// What the client said about itself in `initialize`, kept for every run the
/// connection starts.
#[derive(Debug, Clone, Default)]
struct ClientInit {
    capabilities: CapabilityCapture,
    host: Option<HostClient>,
}

fn session_bindings(
    builder: &dyn RunBindingsBuilder,
    connection: ConnectionTo<Client>,
    session: v1::SessionId,
    client: &ClientInit,
    root: &RunRoot,
    mcp: Option<&Arc<dyn SessionMcpAdmission>>,
    session_approval: Arc<AtomicBool>,
) -> RunExecutionBindings {
    let bindings = builder.build(
        connection,
        session,
        client.capabilities,
        root,
        session_approval,
    );
    let bindings = match &client.host {
        Some(host) => bindings.with_host(host.clone()),
        None => bindings,
    };
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

/// Read one client's blocks into the prompt this build can run.
///
/// Text and resource links flatten into one string, and images become the
/// validated [`Image`] the rest of the system already speaks — the same value
/// `--attach` produces, so what arrives over ACP and what arrives from a path
/// are admitted, bounded and published by one set of rules rather than two.
///
/// **The blocks are not kept interleaved**, because the conversation model they
/// are read into is `{ text, images }`: that is the shape of `PromptQueued` and
/// `UserPrompt`, fixed by `docs/decisions/2026-08-29-images-are-session-blobs-
/// before-they-are-wire-parts.md`, and it predates this reader. A client that
/// sandwiches text around a picture gets its text joined in order and its
/// images in order; it does not get the sandwich back.
fn prompt_content(blocks: &[v1::ContentBlock]) -> Result<RunPrompt, String> {
    let mut parts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            v1::ContentBlock::Text(text) => parts.push(text.text.clone()),
            v1::ContentBlock::ResourceLink(link) => parts.push(link.uri.clone()),
            v1::ContentBlock::Image(image) => images.push(admit_image(image, images.len() + 1)?),
            v1::ContentBlock::Audio(_) => return Err(unsupported_block("audio")),
            v1::ContentBlock::Resource(_) => {
                return Err(unsupported_block("an embedded resource"));
            }
            _ => return Err(unsupported_block("a content block this build cannot read")),
        }
    }
    Ok(RunPrompt::new(parts.join("\n"), images))
}

/// One image block, decoded and proved against its own declared media type.
///
/// Two claims are checked and neither is taken on the client's word. The media
/// type has to name an encoding this build admits at all, and then
/// [`Image::new`] decodes the payload *as* that encoding — so a PNG announced
/// as a JPEG fails here rather than reaching a provider that would reject the
/// whole turn, and the declared width and height a client might have sent are
/// never the ones ego records.
///
/// The count and the byte ceilings are deliberately not enforced here. They
/// belong to the selected model as well as the product, and no model is
/// resolved yet at this point in the request; the executor applies both before
/// anything is published. What this does is refuse content that could never be
/// admitted by any model, as early as it can be recognised.
fn admit_image(image: &v1::ImageContent, index: usize) -> Result<Image, String> {
    let refuse = |reason: String| format!("image {index} in the prompt {reason}");
    let media_type = match image.mime_type.as_str() {
        "image/jpeg" => ImageMediaType::Jpeg,
        "image/png" => ImageMediaType::Png,
        "image/gif" => ImageMediaType::Gif,
        "image/webp" => ImageMediaType::Webp,
        other => {
            return Err(refuse(format!(
                "declares `{other}`, and this build admits only image/jpeg, \
                 image/png, image/gif and image/webp"
            )));
        }
    };
    let bytes = BASE64
        .decode(image.data.as_bytes())
        .map_err(|error| refuse(format!("is not valid base64: {error}")))?;
    Image::new(bytes, media_type).map_err(|error| {
        refuse(format!(
            "does not decode as the `{}` it declares: {error}",
            image.mime_type
        ))
    })
}

fn unsupported_block(what: &str) -> String {
    format!(
        "the prompt carries {what}, and this build's conversation model holds \
         only text, images, tool calls and tool results — so there is nothing \
         for it to be sent in. `initialize` advertises no such prompt capability"
    )
}

/// Serve one `_ego/pause` or `_ego/resume`.
///
/// One function for both, because the only thing that differs between them
/// is which lifecycle call the validated request is handed to. Splitting it
/// would duplicate the version check, both id parses and the error mapping —
/// four rules that must not drift apart between the pause path and the
/// resume path.
async fn hold<const RELEASE: bool>(
    lifecycle: Option<&dyn LifecycleOperations>,
    unadmitted: &UnadmittedSessions,
    request: &AcpHoldRequest<RELEASE>,
    responder: Responder<serde_json::Value>,
) -> Result<(), Error> {
    // Absent lifecycle is unavailable, not partially emulated: the same rule
    // that makes an unadvertised lifecycle method answer `method_not_found`
    // rather than pretend.
    let Some(lifecycle) = lifecycle else {
        return responder.respond_with_error(Error::method_not_found());
    };
    let method = hold_method(RELEASE);
    if request.v != 1 {
        return responder.respond_with_error(
            Error::invalid_params().data(format!("unsupported {method} version")),
        );
    }
    let session = match request.session_id.parse::<SessionId>() {
        Ok(session) if !session.uuid().is_nil() => session,
        _ => {
            return responder.respond_with_error(
                Error::invalid_params().data(format!("invalid {method} sessionId")),
            );
        }
    };
    let request_id = match request.request_id.parse::<RequestId>() {
        Ok(request_id) if !request_id.uuid().is_nil() => request_id,
        _ => {
            return responder.respond_with_error(
                Error::invalid_params().data(format!("invalid {method} requestId")),
            );
        }
    };
    if let Err(error) = unadmitted.refuse(session) {
        return responder.respond_with_error(error);
    }
    let outcome = if RELEASE {
        lifecycle.release(session, request_id).await
    } else {
        lifecycle.pause(session, request_id).await
    };
    let state = match outcome {
        Ok(state) => state,
        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
    };
    let response = AcpHoldResponse {
        v: 1,
        session_id: session,
        request_id,
        state,
    };
    match serde_json::to_value(response) {
        Ok(response) => responder.respond(response),
        Err(error) => responder.respond_with_error(Error::internal_error().data(error.to_string())),
    }
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
        Some(RunSettlement::Paused) => stop_reason_v1(&ego_core::run::RunTermination::Paused),
        // A cancellation, however early it arrived, is what the host asked
        // for and answers `cancelled`.
        Some(RunSettlement::Cancelled { .. }) | Some(RunSettlement::CancelledBeforeStart) => {
            stop_reason_v1(&ego_core::run::RunTermination::Signalled(
                ego_core::run::Signal::Int,
            ))
        }
        // A known turn failure. This used to answer `cancelled`
        // alongside the line above, which told a host its own cancellation
        // had succeeded when in fact this agent had broken. It is an error,
        // for the same reason `Uncertain` below is one.
        Some(settlement @ RunSettlement::Interrupted { .. }) => {
            Err(failure_message("the turn failed", settlement))
        }
        Some(RunSettlement::PermissionRequired { action, by }) => {
            stop_reason_v1(&ego_core::run::RunTermination::PermissionRequired {
                action: action.clone(),
                by: by.clone(),
            })
        }
        Some(settlement @ RunSettlement::Uncertain { .. }) => Err(failure_message(
            "the run outcome is uncertain and requires explicit recovery",
            settlement,
        )),
        None => Err("the admitted turn ended without a settlement".to_string()),
    }
}

/// The sentence a failed turn answers with: what kind of failure it was, and —
/// when the settlement kept one — why.
///
/// The kind is stated first and unconditionally so the answer is the same
/// shape whether or not a diagnostic survived. That matters most on a replay:
/// a retry of a request answered before this build ran finds no diagnostic on
/// the receipt, and must still say something a host can act on rather than
/// answering a bare cause with no category.
fn failure_message(kind: &str, settlement: &RunSettlement) -> String {
    match settlement.diagnostic() {
        Some(diagnostic) => format!("{kind}: {diagnostic}"),
        None => kind.to_owned(),
    }
}

type LifecycleFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, LifecycleError>> + Send + 'a>>;

/// Consumer-owned semantic port used only to compose optional ACP lifecycle
/// capability handlers without coupling the legacy router constructor to a
/// concrete outer store. It contains no ACP DTOs and owns no durable policy.
trait LifecycleOperations: Send + Sync {
    fn create_with_id(
        &self,
        id: SessionId,
        root: std::path::PathBuf,
        profiles: SessionProfiles,
    ) -> Result<(), LifecycleError>;
    fn list(
        &self,
        cursor: Option<&SessionCursor>,
        root: Option<&Path>,
    ) -> Result<SessionPage, LifecycleError>;
    fn hold(&self, id: SessionId) -> Result<HoldState, LifecycleError>;
    fn meta(&self, id: SessionId) -> Result<SessionMeta, LifecycleError>;
    fn profiles(&self, id: SessionId) -> Result<SessionProfiles, LifecycleError>;
    fn goal(&self, id: SessionId) -> Result<Option<ego_core::goal::GoalRecord>, LifecycleError>;
    fn lineage_info(&self, id: SessionId) -> Result<Option<SessionLineageInfo>, LifecycleError>;
    fn load<'a>(&'a self, id: SessionId, root: &'a Path) -> LifecycleFuture<'a, SessionAttachment>;
    fn resume<'a>(
        &'a self,
        id: SessionId,
        root: &'a Path,
    ) -> LifecycleFuture<'a, SessionAttachment>;
    fn fork(
        &self,
        id: SessionId,
        target: SessionId,
        message: Option<ItemId>,
    ) -> LifecycleFuture<'_, ego_core::session_lifecycle::CreatedSession>;
    fn delete(&self, id: SessionId) -> LifecycleFuture<'_, ()>;
    /// Hide a session this server refused to open, durably and without its
    /// writer lease.
    fn quarantine(&self, id: SessionId) -> Result<(), LifecycleError>;
    fn close(&self, id: SessionId) -> LifecycleFuture<'_, ()>;
    fn pause(&self, id: SessionId, request: RequestId) -> LifecycleFuture<'_, HoldState>;
    /// `release` and not `resume`, which this trait already uses for
    /// `session/resume`. The wire name stays `_ego/resume`.
    fn release(&self, id: SessionId, request: RequestId) -> LifecycleFuture<'_, HoldState>;
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
        profiles: SessionProfiles,
    ) -> Result<(), LifecycleError> {
        SessionLifecycle::create_with_profiles(self, id, root, profiles).map(|_| ())
    }

    fn list(
        &self,
        cursor: Option<&SessionCursor>,
        root: Option<&Path>,
    ) -> Result<SessionPage, LifecycleError> {
        SessionLifecycle::list(self, cursor, SESSION_PAGE_SIZE, root)
    }

    fn hold(&self, id: SessionId) -> Result<HoldState, LifecycleError> {
        SessionLifecycle::hold(self, id)
    }

    fn meta(&self, id: SessionId) -> Result<SessionMeta, LifecycleError> {
        SessionLifecycle::meta(self, id)
    }

    fn profiles(&self, id: SessionId) -> Result<SessionProfiles, LifecycleError> {
        SessionLifecycle::profiles(self, id)
    }

    fn goal(&self, id: SessionId) -> Result<Option<ego_core::goal::GoalRecord>, LifecycleError> {
        SessionLifecycle::goal(self, id)
    }

    fn lineage_info(&self, id: SessionId) -> Result<Option<SessionLineageInfo>, LifecycleError> {
        SessionLifecycle::lineage_info(self, id)
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

    fn pause(&self, id: SessionId, request: RequestId) -> LifecycleFuture<'_, HoldState> {
        Box::pin(SessionLifecycle::pause(self, id, request))
    }

    fn release(&self, id: SessionId, request: RequestId) -> LifecycleFuture<'_, HoldState> {
        Box::pin(SessionLifecycle::release(self, id, request))
    }

    fn fork(
        &self,
        id: SessionId,
        target: SessionId,
        message: Option<ItemId>,
    ) -> LifecycleFuture<'_, ego_core::session_lifecycle::CreatedSession> {
        Box::pin(async move {
            match message {
                Some(message) => self.fork_at_as(id, message, target).await,
                None => self.fork_as(id, target).await,
            }
        })
    }

    fn delete(&self, id: SessionId) -> LifecycleFuture<'_, ()> {
        Box::pin(async move { SessionLifecycle::delete(self, id).await.map(|_| ()) })
    }

    fn quarantine(&self, id: SessionId) -> Result<(), LifecycleError> {
        SessionLifecycle::quarantine(self, id)
    }

    fn close(&self, id: SessionId) -> LifecycleFuture<'_, ()> {
        Box::pin(async move { SessionLifecycle::close(self, id).await.map(|_| ()) })
    }
}

/// The generation controls this session offers, or nothing.
///
/// Absent controls stay absent rather than becoming an empty picker, which is
/// the same fallback rule `ARCHITECTURE.md` §3 applies to usage and cost. A
/// seat that cannot be read is a warning and an omission, not a refusal to
/// open the session: creating a session has never depended on the picker.
fn config_options(
    controls: Option<&Arc<dyn SessionControls>>,
    session: SessionId,
) -> Option<Vec<v1::SessionConfigOption>> {
    match controls?.state(session) {
        Ok(state) => Some(crate::controls::options(&state)),
        Err(error) => {
            tracing::warn!(%error, "the session config options could not be read");
            None
        }
    }
}

/// Establish the session's measured offer before any option can expose it.
async fn initialize_confinement(
    admission: Option<&Arc<dyn SessionConfinementAdmission>>,
    session: SessionId,
    root: &RunRoot,
) -> Result<(), Error> {
    let Some(admission) = admission else {
        return Ok(());
    };
    admission
        .initialize(session, root.clone())
        .await
        .map_err(|error| control_error(&error))
}

/// Recover creation authority and run selections, following durable ancestry
/// for successors that have not started their own run yet.
async fn restore_session_profiles<S, E, C, I>(
    controller: &RunController<S, E, C, I>,
    lifecycle: &dyn LifecycleOperations,
    admission: Option<&Arc<dyn SessionConfinementAdmission>>,
    source: SessionId,
    target: SessionId,
    root: &RunRoot,
) -> Result<(), Error>
where
    S: SessionStore,
    E: RunExecutor,
    C: Clock,
    I: IdSource,
{
    let Some(admission) = admission else {
        return Ok(());
    };
    let mut current = source;
    let mut visited = std::collections::HashSet::new();
    let mut selected = RunSelections::empty();
    let mut prompt_options = None;
    loop {
        if !visited.insert(current) {
            return Err(Error::internal_error().data("cyclic session profile lineage"));
        }
        let profiles = lifecycle
            .profiles(current)
            .map_err(|error| lifecycle_error(&error))?;
        let mut selections = match controller.status_by_session(current).await {
            Ok(snapshot) => snapshot.selections().clone(),
            Err(RunError::Store(StoreError::NotFound)) => admission
                .profile_selections(current, RunSelections::empty())
                .map_err(|error| control_error(&error))?,
            Err(error) => return Err(Error::internal_error().data(error.to_string())),
        };
        if prompt_options.is_none() {
            prompt_options = selections
                .prompt_options()
                .cloned()
                .or(profiles.prompt_options);
        }
        if let Some(profile) = profiles.profile {
            selections = selections.with_profile_name(profile);
        }
        if let Some(ceiling) = profiles.ceiling_profile {
            selections = selections.with_ceiling_profile(ceiling);
        }
        if selected.profile().is_none() {
            selected = selections.clone();
        }
        if let Some(ceiling) = selections.ceiling_profile() {
            selected = selected.with_ceiling_profile(ceiling);
            break;
        }
        let Some(lineage) = lifecycle
            .lineage_info(current)
            .map_err(|error| lifecycle_error(&error))?
        else {
            break;
        };
        current = match lineage.lineage {
            ego_core::session::SessionLineage::Fork { source, .. }
            | ego_core::session::SessionLineage::Compaction { source, .. } => source,
        };
    }
    if let Some(options) = prompt_options {
        admission
            .configure_prompt(target, options)
            .map_err(|error| control_error(&error))?;
    }
    if selected.profile().is_some() || selected.ceiling_profile().is_some() {
        admission
            .configure_profiles(target, root, selected.profile(), selected.ceiling_profile())
            .map_err(|error| control_error(&error))?;
    }
    Ok(())
}

/// The confinement this session's seat holds, as a run selection.
///
/// Read on the prompt that *starts* the run and nowhere else. Unlike model,
/// effort and mode — which the seat re-applies at every generation boundary —
/// confinement is admitted once: the MCP transports this run may open, the
/// effect scope it runs inside and its child authority ceiling are all derived
/// from it during preparation, and none of them can be re-decided afterwards.
///
/// Carry the effective value even when the host has not made an explicit pick:
/// the process ceiling is authority, while `chosen` only records UI intent.
fn selected_confinement(
    controls: Option<&Arc<dyn SessionControls>>,
    admission: Option<&Arc<dyn SessionConfinementAdmission>>,
    session: SessionId,
) -> Result<RunSelections, Error> {
    let Some(controls) = controls else {
        return Ok(RunSelections::empty());
    };
    let state = controls
        .state(session)
        .map_err(|error| control_error(&error))?;
    let sandbox = state.sandbox.current.ok_or_else(|| {
        control_error(&ControlError::Internal(
            "the ACP sandbox control has no current confinement".to_owned(),
        ))
    })?;
    let selections = RunSelections::empty().with_sandbox(sandbox);
    let Some(admission) = admission else {
        return Ok(selections);
    };
    let observation = admission
        .observation(session, sandbox)
        .map_err(|error| control_error(&error))?;
    admission
        .profile_selections(
            session,
            selections.with_sandbox_observation(sandbox, observation),
        )
        .map_err(|error| control_error(&error))
}

/// Parse our optional extension strictly: malformed ceilings never become absent.
fn session_profiles(meta: Option<&v1::Meta>) -> Result<Option<SessionProfiles>, Error> {
    let Some(ego) = meta.and_then(|meta| meta.get("ego")) else {
        return Ok(None);
    };
    if !ego.is_object() {
        return Err(Error::invalid_params().data("_meta.ego must be an object"));
    }
    let parse = |key: &str| -> Result<Option<String>, Error> {
        match ego.get(key) {
            None => Ok(None),
            Some(serde_json::Value::String(name)) => ego_core::config::ProfileName::parse(name)
                .map(|_| Some(name.clone()))
                .map_err(|error| Error::invalid_params().data(error.to_string())),
            Some(_) => {
                Err(Error::invalid_params().data(format!("_meta.ego.{key} must be a profile name")))
            }
        }
    };
    let profile = parse("profile")?;
    let ceiling = parse("ceilingProfile")?;
    Ok(
        (profile.is_some() || ceiling.is_some()).then_some(SessionProfiles {
            profile,
            ceiling_profile: ceiling,
            prompt_options: None,
        }),
    )
}

/// Decode only prompt-owned fields; profile and trust fields have separate owners.
fn session_prompt(
    meta: Option<&v1::Meta>,
) -> Result<Option<ego_core::config::SessionPromptOptions>, Error> {
    let Some(ego) = meta.and_then(|meta| meta.get("ego")) else {
        return Ok(None);
    };
    let mut fields = serde_json::Map::new();
    for key in ["systemPrompt", "bare"] {
        if let Some(value) = ego.get(key) {
            if value.is_null() {
                return Err(
                    Error::invalid_params().data(format!("_meta.ego.{key} must not be null"))
                );
            }
            fields.insert(key.to_owned(), value.clone());
        }
    }
    if fields.is_empty() {
        return Ok(None);
    }
    serde_json::from_value(serde_json::Value::Object(fields))
        .map(Some)
        .map_err(|error| Error::invalid_params().data(format!("_meta.ego: {error}")))
}

/// A session request whose MCP selection arrived whole.
///
/// The protocol library drops a server whose transport it does not know and
/// hands over the rest, so an unknown transport would be admitted as an empty
/// selection. Counting the raw entries against the parsed ones refuses the
/// request before any session exists.
#[derive(Debug, Clone)]
struct McpChecked<R>(R);

impl<R: JsonRpcMessage> JsonRpcMessage for McpChecked<R> {
    fn matches_method(method: &str) -> bool {
        R::matches_method(method)
    }

    fn method(&self) -> &str {
        self.0.method()
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, Error> {
        self.0.to_untyped_message()
    }

    fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, Error> {
        let request = R::parse_message(method, params)?;
        let raw = serde_json::to_value(params)
            .map_err(|error| Error::internal_error().data(error.to_string()))?;
        let supplied = raw
            .get("mcpServers")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        let parsed = request
            .to_untyped_message()?
            .params
            .get("mcpServers")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        if supplied != parsed {
            return Err(refused_params(
                "an MCP server uses a transport this agent does not know",
            ));
        }
        Ok(Self(request))
    }
}

impl<R: JsonRpcRequest> JsonRpcRequest for McpChecked<R> {
    type Response = R::Response;
}

/// Sessions a request has created in the store and not yet finished opening.
///
/// Measuring a session's confinement needs it to exist, and its servers are
/// admitted only after that. Until admission settles the session is hidden:
/// not listed, not loadable, not forkable. A host sees it only once it is whole.
#[derive(Clone, Default)]
struct UnadmittedSessions(Arc<Mutex<HashSet<SessionId>>>);

impl UnadmittedSessions {
    fn hold(&self, session: SessionId) -> UnadmittedSession {
        self.lock().insert(session);
        UnadmittedSession {
            sessions: self.clone(),
            session,
            hidden_for_good: false,
        }
    }

    fn contains(&self, session: SessionId) -> bool {
        self.lock().contains(&session)
    }

    fn refuse(&self, session: SessionId) -> Result<(), Error> {
        if self.contains(session) {
            return Err(lifecycle_error(&LifecycleError::UnknownSession));
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<SessionId>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One hidden session. Dropping it publishes the session; `discard` takes it
/// back out of the store instead.
struct UnadmittedSession {
    sessions: UnadmittedSessions,
    session: SessionId,
    hidden_for_good: bool,
}

impl UnadmittedSession {
    /// Delete the session this request created and cannot finish opening. A
    /// session that cannot be deleted (another writer holds it) is quarantined
    /// in the store, so every connection and every later server hides it; only
    /// if that fails too does it stay hidden for the life of this server. The
    /// request fails either way; durable exclusion requires quarantine to succeed.
    async fn discard(
        mut self,
        lifecycle: Option<&dyn LifecycleOperations>,
        refusal: Error,
    ) -> Error {
        let Some(lifecycle) = lifecycle else {
            return refusal;
        };
        match lifecycle.delete(self.session).await {
            Ok(_) => refusal,
            Err(error) => {
                if let Err(quarantine) = lifecycle.quarantine(self.session) {
                    // DEFERRED (2026-10-04): maintainer accepts failed quarantine publication
                    // plus a writer-held deletion lease. No durable write can hide the
                    // refused id; ACP stdio reconnect starts a new process and may expose it.
                    self.hidden_for_good = true;
                    tracing::error!(%quarantine, session = %self.session, "a refused session could not be quarantined");
                }
                tracing::error!(%error, session = %self.session, "a refused session could not be deleted");
                Error::internal_error().data(format!(
                    "{}; the session it created could not be removed: {error}",
                    refusal.data.as_ref().map_or_else(
                        || refusal.message.clone(),
                        |data| data
                            .as_str()
                            .map_or_else(|| data.to_string(), str::to_owned),
                    )
                ))
            }
        }
    }
}

impl Drop for UnadmittedSession {
    fn drop(&mut self) {
        if !self.hidden_for_good {
            self.sessions.lock().remove(&self.session);
        }
    }
}

/// Whether admitting these servers starts a local process.
fn spawns_process(servers: &[v1::McpServer]) -> bool {
    servers
        .iter()
        .any(|server| matches!(server, v1::McpServer::Stdio(_)))
}

/// Admit the MCP servers a session request supplied.
///
/// The caller has already measured the session's confinement: admission starts
/// the servers' processes, and they start under the pick this reads.
async fn admit_session_mcp(
    admitter: &dyn McpSessionAdmitter,
    servers: Vec<v1::McpServer>,
    connection: ConnectionTo<Client>,
    controls: Option<&Arc<dyn SessionControls>>,
    admission: Option<&Arc<dyn SessionConfinementAdmission>>,
    session: SessionId,
    root: &RunRoot,
) -> Result<Option<Arc<dyn SessionMcpAdmission>>, Error> {
    validate_mcp_servers(&servers).map_err(refused_params)?;
    if servers.is_empty() {
        return Ok(None);
    }
    let confinement = match controls.filter(|_| spawns_process(&servers)) {
        Some(controls) => {
            let state = controls
                .state(session)
                .map_err(|error| control_error(&error))?;
            let level = state.sandbox.current.ok_or_else(|| {
                control_error(&ControlError::Internal(
                    "the ACP sandbox control has no current confinement".to_owned(),
                ))
            })?;
            let observation = admission
                .map(|admission| admission.observation(session, level))
                .transpose()
                .map_err(|error| control_error(&error))?;
            Some(McpConfinement { level, observation })
        }
        None => None,
    };
    let context = McpAdmissionContext {
        session,
        root: root.clone(),
        confinement,
    };
    admit_mcp(admitter, servers, Some(connection), context)
        .await
        .map_err(refused_params)
}

/// The option ids the trust form offers. They are ego's, not the host's: the
/// host renders labels and returns one of these two strings, so a host that
/// invents a third answer is refused rather than guessed at.
const TRUST_GRANT: &str = "trusted";
const TRUST_REFUSE: &str = "untrusted";

/// A remembered answer, a new question, or no trustable content.
///
/// Every condition here is synchronous, and that is the point: the caller uses
/// a `Some` to mean "this prompt is about to wait on a person". A prompt that
/// waits on nobody must keep behaving exactly as it did before trust existed,
/// down to where a `session/cancel` lands on it, so the three ways there is no
/// question are answered before anything is awaited.
///
/// A remembered answer is used even without form elicitation. A new answer
/// needs a host that advertises a form.
enum TrustChoice {
    None,
    Remembered(Trust),
    Question(QuestionRequest, String),
}

fn trust_question(
    admission: Option<&Arc<dyn SessionTrustAdmission>>,
    capabilities: CapabilityCapture,
    root: &RunRoot,
) -> TrustChoice {
    let Some(admission) = admission else {
        return TrustChoice::None;
    };
    let (trustable, fingerprint) = match admission.status(root) {
        SessionTrustStatus::Empty => return TrustChoice::None,
        SessionTrustStatus::Remembered(decision) => return TrustChoice::Remembered(decision),
        SessionTrustStatus::Question {
            content,
            fingerprint,
        } => (content, fingerprint),
    };
    if !capabilities.form_available() {
        return TrustChoice::None;
    }
    TrustChoice::Question(
        QuestionRequest::new(
            format!(
                "Trust the workspace at {}?\n\nTrusting it lets this session read its project \
             configuration, its AGENTS.md, its project MCP servers, its skills and its hooks, \
             and run the programs those name. Review:\n{trustable}",
                root.as_path().display()
            ),
            QuestionMode::Single,
            vec![
                QuestionOption::new(TRUST_GRANT, "Trust this workspace"),
                QuestionOption::new(TRUST_REFUSE, "Keep this session bare"),
            ],
            false,
        ),
        fingerprint,
    )
}

/// Put one trust question to the host and read the posture out of the answer.
///
/// Asked here, on the prompt that *starts* the run, for the reason
/// [`selected_confinement`] gives one function up: trust is admitted once, and
/// the config layers, the instruction file, the project MCP servers, the
/// skills and the hooks are all derived from it during preparation. Asked at
/// `session/new` it would be a form raised before the person had asked for
/// anything; asked after preparation it would answer a settled question.
///
/// Explicit answers are written to the same per-root ledger the terminal
/// uses. A matching remembered answer is applied before this function runs.
///
/// Every way of not answering — a decline, a cancel, a host with no form, a
/// transport failure — leaves the session exactly as untrusted as it was
/// before this function existed. Only an explicit `trusted` grants anything.
async fn selected_trust(
    request: QuestionRequest,
    elicitor: AcpQuestionAdapter,
    selections: RunSelections,
    admission: &dyn SessionTrustAdmission,
    root: &RunRoot,
    fingerprint: &str,
) -> RunSelections {
    let question = ItemId::new();
    let outcome = match elicitor.elicit(question, request).await {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(%error, outcome = "failed", "the workspace trust question was not answered");
            return selections;
        }
    };
    // Four outcomes, four branches, and no default that swallows one: a
    // refusal is a person's decision and a cancel is a question nobody
    // answered, and collapsing them would make the two indistinguishable to
    // the host that produced them.
    match outcome {
        QuestionOutcome::Answered(QuestionAnswer::Single(answer)) if answer == TRUST_GRANT => {
            if !admission.record_answer(root, fingerprint, Trust::Trusted) {
                tracing::warn!(
                    "the workspace executable surface changed while the trust form was open"
                );
                return selections;
            }
            tracing::info!(
                outcome = "trusted",
                "the host trusted this workspace for this session"
            );
            selections.with_trust(Trust::Trusted)
        }
        QuestionOutcome::Answered(QuestionAnswer::Single(_)) => {
            if !admission.record_answer(root, fingerprint, Trust::Untrusted) {
                tracing::warn!(
                    "the workspace executable surface changed while the trust form was open"
                );
                return selections;
            }
            tracing::info!(outcome = "untrusted", "the host kept this session bare");
            selections.with_trust(Trust::Untrusted)
        }
        // `validate_for` already refused every answer outside the grammar, so
        // reaching here would mean the adapter admitted a shape it offered no
        // option for. Untrusted, and said out loud.
        QuestionOutcome::Answered(other) => {
            tracing::warn!(
                ?other,
                outcome = "malformed",
                "the workspace trust answer was not one of the offered options"
            );
            selections
        }
        QuestionOutcome::Declined => {
            tracing::info!(
                outcome = "declined",
                "the host refused the workspace trust question"
            );
            selections.with_trust(Trust::Untrusted)
        }
        QuestionOutcome::Cancelled => {
            tracing::info!(
                outcome = "cancelled",
                "the workspace trust question was withdrawn unanswered"
            );
            selections
        }
        QuestionOutcome::Unavailable => {
            tracing::info!(
                outcome = "unavailable",
                "this host cannot render the workspace trust form"
            );
            selections
        }
    }
}

fn control_error(error: &ControlError) -> Error {
    match error {
        // Both name a value the caller sent, so both are the caller's
        // parameters being wrong rather than ego failing.
        ControlError::Unavailable { .. }
        | ControlError::Refused { .. }
        | ControlError::MissingCapabilities { .. } => {
            Error::new(i32::from(ErrorCode::InvalidParams), error.to_string())
        }
        ControlError::Internal(message) => Error::internal_error().data(message.clone()),
    }
}

fn parse_session(id: &v1::SessionId) -> Result<SessionId, Error> {
    id.to_string()
        .parse::<SessionId>()
        .map_err(|_| Error::invalid_params().data(format!("invalid ACP session id {id}")))
}

/// A session was momentarily owned by another writer and the same request may
/// be sent again.
///
/// ACP names no busy code, and none of the ones it does name is true here:
/// the params were valid, the method exists, the session exists, nothing was
/// cancelled, and ego did not fail. `-32010` is inside JSON-RPC's
/// implementation-defined server range and clear of every code ACP assigns
/// (`-32000` auth, `-32002` not found). It is part of the `_ego` extension
/// contract and is documented with it; a host must not have to match on the
/// wording of `data` to learn a retry is safe.
const SESSION_BUSY: i32 = -32010;
const SESSION_HELD: i32 = -32011;

/// What a lifecycle refusal is, in the only vocabulary a host acts on.
///
/// Exhaustive on purpose. The blanket `impl Display` this replaced answered
/// `invalid_params` to every one of these, which told a host its request was
/// malformed when the session was missing, when another writer held it for the
/// length of an append, and when ego itself had failed — three different things
/// to do, given one answer that is right for none of them. Matching the enum
/// means a variant added later cannot inherit a verdict nobody chose for it.
fn lifecycle_error(error: &LifecycleError) -> Error {
    let code = match error {
        // The id was well-formed and names nothing, or names something that is
        // gone. A caller cannot fix either by sending better parameters.
        LifecycleError::UnknownSession | LifecycleError::Deleted => ErrorCode::ResourceNotFound,
        // Transient by construction: another writer has the session, or moved
        // it under this request. Retrying is what these are for, and both
        // methods are idempotent so retrying is safe.
        LifecycleError::LeaseHeld | LifecycleError::Active | LifecycleError::Conflict => {
            ErrorCode::Other(SESSION_BUSY)
        }
        LifecycleError::EffectUncertain => {
            return Error::new(
                ErrorCode::InternalError.into(),
                "the session's last run has an uncertain effect; close it with `session/close` and start a new chat with `session/new`",
            );
        }
        // ego failed. Not the caller's parameters, and not something a retry
        // is promised to fix.
        LifecycleError::Store(_)
        | LifecycleError::Replay(_)
        | LifecycleError::Ancestry(_)
        | LifecycleError::Evidence(_)
        | LifecycleError::Run(_)
        | LifecycleError::NotPublished(_) => ErrorCode::InternalError,
        // What the caller asked for, refused on its own terms.
        LifecycleError::InvalidCursor
        | LifecycleError::InvalidLimit
        | LifecycleError::InvalidText
        | LifecycleError::RootMismatch { .. }
        | LifecycleError::NotPaused { .. }
        | LifecycleError::Goal(_)
        | LifecycleError::Fork(_) => ErrorCode::InvalidParams,
    };
    Error::from(code).data(error.to_string())
}

/// A request refused for what it carried, before any session was reached.
///
/// Root resolution and MCP admission both reject something the caller sent, so
/// `invalid_params` is the truth for them — but they are not lifecycle errors
/// and must not share a mapper with one, or the next correction to either will
/// silently move the other.
fn refused_params(error: impl fmt::Display) -> Error {
    Error::invalid_params().data(error.to_string())
}

/// One session as the host lists and labels it.
///
/// `updated_at` is `SessionMeta::updated` — the observation time of the last
/// event appended to that session — spelled RFC 3339 because that is what the
/// field is defined to carry. It is not decoration: `session/list` returns
/// sessions in no protocol-defined order, so recency is the only thing a host
/// can sort a session picker by, and an agent that omits it hands back a list
/// nobody can order.
fn session_meta(
    state: HoldState,
    summary: Option<&str>,
    lineage: Option<SessionLineageInfo>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut ego = serde_json::json!({"hold": {"state": state}});
    if let Some(summary) = summary {
        ego["summary"] = serde_json::Value::String(summary.to_owned());
    }
    if let Some(info) = lineage {
        use ego_core::session::SessionLineage;
        let (kind, source) = match info.lineage {
            SessionLineage::Fork { source, .. } => ("fork", source),
            SessionLineage::Compaction { source, .. } => ("compaction", source),
        };
        let mut lineage = serde_json::json!({"kind": kind, "sourceSessionId": source,
            "rootSessionId": info.root, "sourceDeleted": info.source_deleted});
        if let Some(message) = info.at_message {
            lineage["atMessageId"] = serde_json::json!(message)
        }
        ego["lineage"] = lineage;
    }
    serde_json::Map::from_iter([("ego".to_owned(), ego)])
}

fn session_info(
    meta: SessionMeta,
    hold: HoldState,
    lineage: Option<SessionLineageInfo>,
) -> v1::SessionInfo {
    let extension = session_meta(hold, meta.summary.as_deref(), lineage);
    v1::SessionInfo::new(meta.id.to_string(), meta.workspace_root)
        .title(meta.title)
        .updated_at(meta.updated.to_rfc3339())
        .meta(extension)
}

fn emit_info(
    connection: &ConnectionTo<Client>,
    id: &v1::SessionId,
    meta: &SessionMeta,
    lifecycle: Option<&dyn LifecycleOperations>,
) -> Result<(), Error> {
    let lineage = lifecycle
        .map(|lifecycle| lifecycle.lineage_info(meta.id))
        .transpose()
        .map_err(|error| lifecycle_error(&error))?
        .flatten();
    let hold = lifecycle
        .map(|lifecycle| lifecycle.hold(meta.id))
        .transpose()
        .map_err(|error| lifecycle_error(&error))?
        .unwrap_or(HoldState::Running);
    connection.send_notification(v1::SessionNotification::new(
        id.clone(),
        v1::SessionUpdate::SessionInfoUpdate(
            v1::SessionInfoUpdate::new()
                .title(meta.title.clone())
                .updated_at(meta.updated.to_rfc3339())
                .meta(session_meta(hold, meta.summary.as_deref(), lineage)),
        ),
    ))?;
    if let Some(lifecycle) = lifecycle {
        let goal = lifecycle
            .goal(meta.id)
            .map_err(|error| lifecycle_error(&error))?;
        if let Some(plan) = project_goal_plan_v1(goal.as_ref()) {
            connection.send_notification(v1::SessionNotification::new(id.clone(), plan))?;
        }
    }
    Ok(())
}

fn emit_config_options(
    connection: &ConnectionTo<Client>,
    id: &v1::SessionId,
    controls: Option<&Arc<dyn SessionControls>>,
    session: SessionId,
) -> Result<(), Error> {
    let Some(options) = config_options(controls, session) else {
        return Ok(());
    };
    connection.send_notification(v1::SessionNotification::new(
        id.clone(),
        v1::SessionUpdate::ConfigOptionUpdate(v1::ConfigOptionUpdate::new(options)),
    ))
}

fn emit_attachment(
    connection: &ConnectionTo<Client>,
    id: &v1::SessionId,
    attachment: &SessionAttachment,
    lifecycle: Option<&dyn LifecycleOperations>,
) -> Result<(), Error> {
    for update in project_replay_v1(
        &attachment.replay,
        attachment.context_window,
        &attachment.root,
    ) {
        connection.send_notification(v1::SessionNotification::new(id.clone(), update))?;
    }
    emit_info(connection, id, &attachment.meta, lifecycle)
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
        None,
        None,
        None,
        AcpNotices::default(),
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
        None,
        None,
        None,
        AcpNotices::default(),
    ))
}

/// Build the production ACP router with the versioned compaction extension.
#[allow(clippy::too_many_arguments)]
pub fn agent_with_compaction<S, E, C, I>(
    controller: Arc<RunController<S, E, C, I>>,
    roots: Arc<dyn RootResolver>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
    mcp_admitter: Arc<dyn McpSessionAdmitter>,
    compaction: Arc<dyn CompactionCommand>,
    controls: Option<Arc<dyn SessionControls>>,
    confinement: Option<Arc<dyn SessionConfinementAdmission>>,
    trust: Option<Arc<dyn SessionTrustAdmission>>,
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
        controls,
        confinement,
        trust,
        AcpNotices::default(),
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
    controls: Option<Arc<dyn SessionControls>>,
    confinement: Option<Arc<dyn SessionConfinementAdmission>>,
    trust: Option<Arc<dyn SessionTrustAdmission>>,
    notices: AcpNotices,
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
        controls,
        confinement,
        trust,
        notices,
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
    controls: Option<Arc<dyn SessionControls>>,
    confinement: Option<Arc<dyn SessionConfinementAdmission>>,
    trust: Option<Arc<dyn SessionTrustAdmission>>,
    notices: AcpNotices,
) -> impl agent_client_protocol::ConnectTo<Client>
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    let sessions = Sessions::new(controller.retention().completed_receipts());
    let unadmitted = UnadmittedSessions::default();
    let client_capabilities = Arc::new(Mutex::new(ClientInit::default()));
    let advertises_compaction = compaction.is_some();
    let advertises_lifecycle = lifecycle.is_some();
    let prompt_handler = prompt_turn::prompt_handler(
        sessions.clone(),
        unadmitted.clone(),
        Arc::clone(&controller),
        Arc::clone(&spawner),
        Arc::clone(&ids),
        Arc::clone(&bindings_builder),
        Arc::clone(&client_capabilities),
        controls.clone(),
        confinement.clone(),
        trust.clone(),
        lifecycle.clone(),
    );
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
                    {
                        let mut client = client_capabilities
                            .lock()
                            .expect("initialize capability lock poisoned");
                        client
                            .capabilities
                            .capture_initialize(&request.client_capabilities);
                        client.host = request.client_info.as_ref().map(|info| HostClient {
                            name: info.name.clone(),
                            version: info.version.clone(),
                        });
                    }
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
                let unadmitted = unadmitted.clone();
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                let lifecycle = lifecycle.clone();
                let controls = controls.clone();
                let confinement = confinement.clone();
                let notices = notices.clone();
                let prompt_handler = Arc::clone(&prompt_handler);
                let trust = trust.clone();
                async move |McpChecked(request): McpChecked<v1::NewSessionRequest>, responder, cx: ConnectionTo<Client>| {
                    let sessions = sessions.clone();
                    let unadmitted = unadmitted.clone();
                    let roots = Arc::clone(&roots);
                    let ids = Arc::clone(&ids);
                    let mcp_admitter = Arc::clone(&mcp_admitter);
                    let lifecycle = lifecycle.clone();
                    let controls = controls.clone();
                    let confinement = confinement.clone();
                    let notices = notices.clone();
                let prompt_handler = Arc::clone(&prompt_handler);
                    let trust = trust.clone();
                    let dispatcher = cx.clone();
                    dispatcher.spawn(async move {
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
                    if let Err(error) = validate_mcp_servers(&request.mcp_servers) {
                        return responder.respond_with_error(refused_params(error));
                    }
                    let start_request = RequestId::from_uuid(ids.next_uuid());
                    let session = RequestId::session_id(start_request);
                    let id = v1::SessionId::new(session.to_string());
                    let profiles = match session_profiles(request.meta.as_ref()) {
                        Ok(profiles) => profiles,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let prompt_options = match session_prompt(request.meta.as_ref()) {
                        Ok(options) => options,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    if let Some(options) = prompt_options.clone() {
                        match confinement.as_ref() {
                            Some(admission) => {
                                if let Err(error) = admission.configure_prompt(session, options) {
                                    return responder.respond_with_error(control_error(&error));
                                }
                            }
                            None => return responder.respond_with_error(Error::invalid_params().data("session prompt selections are not supported")),
                        }
                    }
                    let mut creation_profiles = profiles.clone().unwrap_or_default();
                    creation_profiles.prompt_options = prompt_options;
                    let profile_meta = match profiles {
                        Some(SessionProfiles { profile, ceiling_profile, .. }) => match confinement.as_ref() {
                            Some(admission) => match admission.configure_profiles(session, &root, profile.as_deref(), ceiling_profile.as_deref()) {
                                Ok(meta) => Some(serde_json::Map::from_iter([("ego".to_owned(), meta)])),
                                Err(error) => return responder.respond_with_error(control_error(&error)),
                            },
                            None => return responder.respond_with_error(Error::invalid_params().data("session profiles are not supported")),
                        },
                        None => None,
                    };
                    // Hidden before it exists so no list or load can see it
                    // between creation and admission.
                    let hidden = unadmitted.hold(session);
                    if let Some(lifecycle) = &lifecycle
                        && let Err(error) =
                            lifecycle.create_with_id(session, root.as_path().to_path_buf(), creation_profiles)
                    {
                        return responder.respond_with_error(lifecycle_error(&error));
                    }
                    // Supplied servers start processes, which start under the
                    // session's measured confinement: measure first, admit
                    // after, and take the session back if either refuses so a
                    // refused request leaves no empty session behind.
                    // Only a server that starts a process needs the measurement
                    // before admission; the others keep the deferred offer.
                    let admits_mcp = !request.mcp_servers.is_empty();
                    let measures_first = spawns_process(&request.mcp_servers);
                    let mcp = if admits_mcp {
                        let measured = if measures_first {
                            initialize_confinement(confinement.as_ref(), session, &root).await
                        } else {
                            Ok(())
                        };
                        let admitted = match measured {
                            Ok(()) => admit_session_mcp(
                                mcp_admitter.as_ref(),
                                request.mcp_servers,
                                cx.clone(),
                                controls.as_ref(),
                                confinement.as_ref(),
                                session,
                                &root,
                            )
                            .await,
                            Err(error) => Err(error),
                        };
                        match admitted {
                            Ok(mcp) => mcp,
                            Err(error) => {
                                let error = hidden.discard(lifecycle.as_deref(), error).await;
                                return responder.respond_with_error(error);
                            }
                        }
                    } else {
                        None
                    };
                    let pending_measurement = (!measures_first)
                        .then(|| {
                            confinement
                                .as_ref()
                                .map(|admission| admission.initialize(session, root.clone()))
                        })
                        .flatten();
                    let attachment_root = root.clone();
                    let _ = sessions.open(
                        id.clone(),
                        root,
                        Some(start_request),
                        None,
                        mcp,
                        Arc::new(AtomicBool::new(false)),
                    );
                    let announcement = if let Some(lifecycle) = &lifecycle {
                        match lifecycle.resume(session, attachment_root.as_path()).await {
                            Ok(attachment) => Some(attachment.meta),
                            Err(error) => {
                                return responder.respond_with_error(lifecycle_error(&error));
                            }
                        }
                    } else {
                        None
                    };
                    if request.meta.as_ref()
                        .and_then(|meta| meta.get("ego"))
                        .and_then(|ego| ego.get("trust"))
                        .and_then(serde_json::Value::as_str) == Some(TRUST_GRANT)
                        && let Some(admission) = &trust
                    {
                        admission.attest_trusted(&attachment_root);
                    }
                    drop(hidden);
                    // The response first, the announcement after it. A
                    // `session_info_update` names a session by an id the client
                    // learns from *this* response, so a client that indexes
                    // sessions by the response — the ordering `project.rs`
                    // already documents — drops any update that precedes it.
                    // Measurement now runs after the response. Its complete
                    // offer arrives as a later config_option_update, while
                    // the first prompt awaits the same admission.
                    responder.respond(
                        v1::NewSessionResponse::new(id.clone())
                            .meta(profile_meta)
                            .config_options(
                                pending_measurement
                                    .is_none()
                                    .then(|| config_options(controls.as_ref(), session))
                                    .flatten(),
                            ),
                    )?;
                    notices.register(id.clone(), cx.clone(), sessions.clone(), Arc::clone(&prompt_handler));
                    if let Some(meta) = announcement {
                        emit_info(&cx, &id, &meta, lifecycle.as_deref())?;
                    }
                    if let Some(pending) = pending_measurement {
                        let connection = cx.clone();
                        let controls = controls.clone();
                        if let Err(error) = cx.spawn(async move {
                            match pending.await {
                                Ok(()) => {
                                    if let Err(error) = emit_config_options(
                                        &connection,
                                        &id,
                                        controls.as_ref(),
                                        session,
                                    ) {
                                        tracing::warn!(%error, "measured ACP config options could not be delivered");
                                    }
                                }
                                Err(error) => tracing::warn!(%error, "ACP confinement measurement failed after session/new"),
                            }
                            Ok(())
                        }) {
                            tracing::warn!(%error, "ACP confinement update task could not start");
                        }
                    }
                    Ok(())
                    })
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let roots = Arc::clone(&roots);
                let unadmitted = unadmitted.clone();
                async move |request: v1::ListSessionsRequest, responder, _cx| {
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let root = match request.cwd.as_deref().map(|cwd| roots.resolve(cwd)).transpose() {
                        Ok(root) => root,
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    let mut cursor = match request.cursor.map(SessionCursor::parse).transpose() {
                        Ok(cursor) => cursor,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    // A hidden session is filtered out of its page, so keep
                    // reading pages until the response holds as many rows as
                    // a full page, and the cursor always names the last row
                    // read: no visible row is skipped or left behind.
                    let mut visible = Vec::new();
                    let mut wanted = None;
                    let next_cursor = loop {
                        let page = match lifecycle.list(cursor.as_ref(), root.as_ref().map(RunRoot::as_path)) {
                            Ok(page) => page,
                            Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                        };
                        let wanted = *wanted.get_or_insert(page.items.len());
                        visible.extend(page.items.into_iter().filter(|meta| !unadmitted.contains(meta.id)));
                        match page.next_cursor {
                            Some(next) if visible.len() < wanted => cursor = Some(next),
                            next => break next,
                        }
                    };
                    let items = visible.into_iter().map(|meta| {
                        let lineage = lifecycle.lineage_info(meta.id)?;
                        lifecycle.hold(meta.id).map(|hold| session_info(meta, hold, lineage))
                    }).collect::<Result<Vec<_>, _>>();
                    match items {
                        Ok(items) => responder.respond(
                            v1::ListSessionsResponse::new(items)
                                .next_cursor(next_cursor.map(|cursor| cursor.to_string())),
                        ),
                        Err(error) => responder.respond_with_error(lifecycle_error(&error)),
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                let unadmitted = unadmitted.clone();
                let controller = Arc::clone(&controller);
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                let spawner = Arc::clone(&spawner);
                let bindings_builder = Arc::clone(&bindings_builder);
                let client_capabilities = Arc::clone(&client_capabilities);
                let controls = controls.clone();
                let confinement = confinement.clone();
                let notices = notices.clone();
                let prompt_handler = Arc::clone(&prompt_handler);
                async move |McpChecked(request): McpChecked<v1::LoadSessionRequest>, responder, cx: ConnectionTo<Client>| {
                    let lifecycle = lifecycle.clone();
                    let sessions = sessions.clone();
                    let unadmitted = unadmitted.clone();
                    let controller = Arc::clone(&controller);
                    let roots = Arc::clone(&roots);
                    let ids = Arc::clone(&ids);
                    let mcp_admitter = Arc::clone(&mcp_admitter);
                    let spawner = Arc::clone(&spawner);
                    let bindings_builder = Arc::clone(&bindings_builder);
                    let client_capabilities = Arc::clone(&client_capabilities);
                    let controls = controls.clone();
                    let confinement = confinement.clone();
                    let notices = notices.clone();
                let prompt_handler = Arc::clone(&prompt_handler);
                    let dispatcher = cx.clone();
                    dispatcher.spawn(async move {
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
                    if let Err(error) = unadmitted.refuse(id) {
                        return responder.respond_with_error(error);
                    }
                    if sessions.is_busy(&request.session_id) {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!("session {} already has a turn running", request.session_id),
                        ));
                    }
                    let root = match roots.resolve(&request.cwd) {
                        Ok(root) => root,
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    let attachment = match lifecycle.load(id, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    let hold = match lifecycle.hold(id) {
                        Ok(hold) => hold,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    if let Err(error) = restore_session_profiles(controller.as_ref(), lifecycle.as_ref(), confinement.as_ref(), id, id, &root).await {
                        return responder.respond_with_error(error);
                    }
                    if let Err(error) =
                        initialize_confinement(confinement.as_ref(), id, &root).await
                    {
                        return responder.respond_with_error(error);
                    }
                    let mcp = match admit_session_mcp(
                        mcp_admitter.as_ref(),
                        request.mcp_servers,
                        cx.clone(),
                        controls.as_ref(),
                        confinement.as_ref(),
                        id,
                        &root,
                    )
                    .await
                    {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let client = client_capabilities
                        .lock()
                        .expect("initialize capability lock poisoned")
                        .clone();
                    let session_approval = Arc::new(AtomicBool::new(false));
                    bindings_builder.end_session(&request.session_id);
                    let bindings = session_bindings(
                        bindings_builder.as_ref(),
                        cx.clone(),
                        request.session_id.clone(),
                        &client,
                        &root,
                        mcp.as_ref(),
                        Arc::clone(&session_approval),
                    );
                    let recovered = match recover_current_run(controller.as_ref(), id, bindings).await {
                        Ok(run) => run,
                        Err(error) => return responder.respond_with_error(Error::internal_error().data(error)),
                    };
                    let Some(generation) = sessions.open(
                        request.session_id.clone(),
                        root,
                        recovered.run.is_none().then(|| RequestId::from_uuid(ids.next_uuid())),
                        recovered.run,
                        mcp,
                        session_approval,
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
                    // Answer before replaying, for the reason `session/new`
                    // does: every notification below names this session, and a
                    // client that indexes by the response has not seen the id
                    // yet. The whole replay, not just the trailing info update,
                    // would be dropped.
                    responder.respond(
                        v1::LoadSessionResponse::new()
                            .config_options(config_options(controls.as_ref(), id))
                            .meta(session_meta(hold, attachment.meta.summary.as_deref(), lifecycle.lineage_info(id).map_err(|error| lifecycle_error(&error))?)),
                    )?;
                    notices.register(request.session_id.clone(), cx.clone(), sessions.clone(), Arc::clone(&prompt_handler));
                    emit_attachment(&cx, &request.session_id, &attachment, Some(lifecycle.as_ref()))
                    })
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                let unadmitted = unadmitted.clone();
                let controller = Arc::clone(&controller);
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                let spawner = Arc::clone(&spawner);
                let bindings_builder = Arc::clone(&bindings_builder);
                let client_capabilities = Arc::clone(&client_capabilities);
                let controls = controls.clone();
                let confinement = confinement.clone();
                let notices = notices.clone();
                let prompt_handler = Arc::clone(&prompt_handler);
                async move |McpChecked(request): McpChecked<v1::ResumeSessionRequest>, responder, cx: ConnectionTo<Client>| {
                    let lifecycle = lifecycle.clone();
                    let sessions = sessions.clone();
                    let unadmitted = unadmitted.clone();
                    let controller = Arc::clone(&controller);
                    let roots = Arc::clone(&roots);
                    let ids = Arc::clone(&ids);
                    let mcp_admitter = Arc::clone(&mcp_admitter);
                    let spawner = Arc::clone(&spawner);
                    let bindings_builder = Arc::clone(&bindings_builder);
                    let client_capabilities = Arc::clone(&client_capabilities);
                    let controls = controls.clone();
                    let confinement = confinement.clone();
                    let notices = notices.clone();
                let prompt_handler = Arc::clone(&prompt_handler);
                    let dispatcher = cx.clone();
                    dispatcher.spawn(async move {
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
                    if let Err(error) = unadmitted.refuse(id) {
                        return responder.respond_with_error(error);
                    }
                    if sessions.is_busy(&request.session_id) {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!("session {} already has a turn running", request.session_id),
                        ));
                    }
                    let root = match roots.resolve(&request.cwd) {
                        Ok(root) => root,
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    let attachment = match lifecycle.resume(id, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    if let Err(error) = restore_session_profiles(controller.as_ref(), lifecycle.as_ref(), confinement.as_ref(), id, id, &root).await {
                        return responder.respond_with_error(error);
                    }
                    if let Err(error) =
                        initialize_confinement(confinement.as_ref(), id, &root).await
                    {
                        return responder.respond_with_error(error);
                    }
                    let mcp = match admit_session_mcp(
                        mcp_admitter.as_ref(),
                        request.mcp_servers,
                        cx.clone(),
                        controls.as_ref(),
                        confinement.as_ref(),
                        id,
                        &root,
                    )
                    .await
                    {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let client = client_capabilities
                        .lock()
                        .expect("initialize capability lock poisoned")
                        .clone();
                    let session_approval = Arc::new(AtomicBool::new(false));
                    bindings_builder.end_session(&request.session_id);
                    let bindings = session_bindings(
                        bindings_builder.as_ref(),
                        cx.clone(),
                        request.session_id.clone(),
                        &client,
                        &root,
                        mcp.as_ref(),
                        Arc::clone(&session_approval),
                    );
                    let recovered = match recover_current_run(controller.as_ref(), id, bindings).await {
                        Ok(run) => run,
                        Err(error) => return responder.respond_with_error(Error::internal_error().data(error)),
                    };
                    let Some(generation) = sessions.open(
                        request.session_id.clone(),
                        root,
                        recovered.run.is_none().then(|| RequestId::from_uuid(ids.next_uuid())),
                        recovered.run,
                        mcp,
                        session_approval,
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
                    responder.respond(
                        v1::ResumeSessionResponse::new()
                            .config_options(config_options(controls.as_ref(), id)),
                    )?;
                    notices.register(request.session_id.clone(), cx.clone(), sessions.clone(), Arc::clone(&prompt_handler));
                    emit_attachment(&cx, &request.session_id, &attachment, Some(lifecycle.as_ref()))
                    })
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let controller = Arc::clone(&controller);
                let sessions = sessions.clone();
                let unadmitted = unadmitted.clone();
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                let controls = controls.clone();
                let confinement = confinement.clone();
                let notices = notices.clone();
                let prompt_handler = Arc::clone(&prompt_handler);
                async move |McpChecked(request): McpChecked<v1::ForkSessionRequest>, responder, cx: ConnectionTo<Client>| {
                    let lifecycle = lifecycle.clone();
                    let controller = Arc::clone(&controller);
                    let sessions = sessions.clone();
                    let unadmitted = unadmitted.clone();
                    let roots = Arc::clone(&roots);
                    let ids = Arc::clone(&ids);
                    let mcp_admitter = Arc::clone(&mcp_admitter);
                    let controls = controls.clone();
                    let confinement = confinement.clone();
                    let notices = notices.clone();
                let prompt_handler = Arc::clone(&prompt_handler);
                    let dispatcher = cx.clone();
                    dispatcher.spawn(async move {
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
                    if let Err(error) = unadmitted.refuse(source) {
                        return responder.respond_with_error(error);
                    }
                    let root = match roots.resolve(&request.cwd) {
                        Ok(root) => root,
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    if let Err(error) = lifecycle.resume(source, root.as_path()).await {
                        return responder.respond_with_error(lifecycle_error(&error));
                    }
                    if let Err(error) = validate_mcp_servers(&request.mcp_servers) {
                        return responder.respond_with_error(refused_params(error));
                    }
                    let at_message = match request.meta.as_ref().and_then(|meta| meta.get("ego")) {
                        Some(ego) => match ego.get("atMessageId") {
                            Some(value) => match serde_json::from_value::<ItemId>(value.clone()) {
                                Ok(message) => Some(message),
                                Err(_) => return responder.respond_with_error(Error::invalid_params().data("atMessageId must be a message UUID")),
                            },
                            None => None,
                        },
                        None => None,
                    };
                    // Hidden before the fork publishes it, so no list or load
                    // can see the child between publication and admission.
                    let child = SessionId::new();
                    let hidden = unadmitted.hold(child);
                    let child = match lifecycle.fork(source, child, at_message).await {
                        Ok(child) => child.id,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    let attachment = match lifecycle.resume(child, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => {
                            let error = hidden
                                .discard(Some(lifecycle.as_ref()), lifecycle_error(&error))
                                .await;
                            return responder.respond_with_error(error);
                        }
                    };
                    // The child's processes start under the child's measured
                    // confinement, so the child exists first. An MCP refusal
                    // takes it back rather than leaving an unexplained fork.
                    if let Err(error) = restore_session_profiles(controller.as_ref(), lifecycle.as_ref(), confinement.as_ref(), source, child, &root).await {
                        let error = hidden.discard(Some(lifecycle.as_ref()), error).await;
                        return responder.respond_with_error(error);
                    }
                    let admitted = match initialize_confinement(confinement.as_ref(), child, &root).await {
                        Ok(()) => admit_session_mcp(
                            mcp_admitter.as_ref(),
                            request.mcp_servers,
                            cx.clone(),
                            controls.as_ref(),
                            confinement.as_ref(),
                            child,
                            &root,
                        )
                        .await,
                        Err(error) => Err(error),
                    };
                    let mcp = match admitted {
                        Ok(mcp) => mcp,
                        Err(error) => {
                            let error = hidden.discard(Some(lifecycle.as_ref()), error).await;
                            return responder.respond_with_error(error);
                        }
                    };
                    let child_id = v1::SessionId::new(child.to_string());
                    let _ = sessions.open(
                        child_id.clone(),
                        root,
                        Some(RequestId::from_uuid(ids.next_uuid())),
                        None,
                        mcp,
                        Arc::new(AtomicBool::new(false)),
                    );
                    drop(hidden);
                    // The child's id reaches the client here and nowhere else,
                    // so its own announcement cannot precede this. The failure
                    // compensation that used to close the child is gone with
                    // the failure it compensated: once the response is out the
                    // child is announced, and the only way `emit_info` fails
                    // now is a connection that can no longer carry either
                    // message.
                    responder.respond(
                        v1::ForkSessionResponse::new(child_id.clone())
                            .config_options(config_options(controls.as_ref(), child)),
                    )?;
                    notices.register(child_id.clone(), cx.clone(), sessions.clone(), Arc::clone(&prompt_handler));
                    emit_info(&cx, &child_id, &attachment.meta, Some(lifecycle.as_ref()))
                    })
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                let bindings_builder = Arc::clone(&bindings_builder);
                let unadmitted = unadmitted.clone();
                let notices = notices.clone();
                async move |request: v1::DeleteSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let lifecycle = lifecycle.clone();
                    let sessions = sessions.clone();
                    let bindings_builder = Arc::clone(&bindings_builder);
                    let unadmitted = unadmitted.clone();
                    let notices = notices.clone();
                    let dispatcher = cx.clone();
                    dispatcher.spawn(async move {
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let id = match parse_session(&request.session_id) {
                        Ok(id) => id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    if let Err(error) = unadmitted.refuse(id) {
                        return responder.respond_with_error(error);
                    }
                    match lifecycle.delete(id).await {
                        Ok(()) => {
                            if let Some(mcp) = sessions.close(&request.session_id) {
                                mcp.close().await;
                            }
                            bindings_builder.end_session(&request.session_id);
                            notices.unregister(&request.session_id);
                            responder.respond(v1::DeleteSessionResponse::new())
                        }
                        Err(error) => responder.respond_with_error(lifecycle_error(&error)),
                    }
                    })
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let sessions = sessions.clone();
                let bindings_builder = Arc::clone(&bindings_builder);
                let unadmitted = unadmitted.clone();
                let notices = notices.clone();
                async move |request: v1::CloseSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let lifecycle = lifecycle.clone();
                    let sessions = sessions.clone();
                    let bindings_builder = Arc::clone(&bindings_builder);
                    let unadmitted = unadmitted.clone();
                    let notices = notices.clone();
                    let dispatcher = cx.clone();
                    dispatcher.spawn(async move {
                    let Some(lifecycle) = &lifecycle else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let id = match parse_session(&request.session_id) {
                        Ok(id) => id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    if let Err(error) = unadmitted.refuse(id) {
                        return responder.respond_with_error(error);
                    }
                    match lifecycle.close(id).await {
                        Ok(()) => {
                            if let Some(mcp) = sessions.close(&request.session_id) {
                                mcp.close().await;
                            }
                            bindings_builder.end_session(&request.session_id);
                            notices.unregister(&request.session_id);
                            responder.respond(v1::CloseSessionResponse::new())
                        }
                        Err(error) => responder.respond_with_error(lifecycle_error(&error)),
                    }
                    })
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let mcp_admitter = Arc::clone(&mcp_admitter);
                async move |notification: v1::MessageMcpNotification, cx: ConnectionTo<Client>| {
                    if let Some(refresh) = mcp_admitter.mcp_notification(notification) {
                        cx.spawn(async move {
                            refresh.await;
                            Ok(())
                        })
                    } else {
                        Ok(())
                    }
                }
            },
            on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let sessions = sessions.clone();
                let controller = Arc::clone(&controller);
                async move |notification: v1::CancelNotification, _cx| {
                    match sessions.cancel(&notification.session_id) {
                        Cancellation::Admitted(key, mcp) => {
                            if let Err(error) = controller.cancel_admitted(&key).await {
                                tracing::warn!(%error, "ACP cancellation could not be applied");
                            }
                            // The run must observe cancellation before a transport
                            // failure can finish its tool and start another generation.
                            if let Some(mcp) = mcp {
                                mcp.cancel_active();
                            }
                        }
                        // Recorded, not applied: there is no run to cancel
                        // yet. The prompt task that is about to admit one
                        // reads it and answers `cancelled` instead.
                        Cancellation::Withdrawn => tracing::info!(
                            "an ACP cancellation arrived before the run it names was admitted"
                        ),
                        Cancellation::Nothing => {}
                    }
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let handler = Arc::clone(&prompt_handler);
                async move |request: v1::PromptRequest, responder, cx: ConnectionTo<Client>| {
                    handler(request, PromptReply::Wire(responder), cx).await
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                move |request: AcpSteerRequest, responder: Responder<serde_json::Value>, cx: ConnectionTo<Client>| {
                    let sessions = sessions.clone();
                    async move {
                        if request.v != 1 {
                            return responder.respond_with_error(refused_params("unsupported _ego/steer version"));
                        }
                        let id = v1::SessionId::new(request.session_id.clone());
                        if let Err(error) = parse_session(&id) { return responder.respond_with_error(error); }
                        let mut parts = Vec::new();
                        for block in request.prompt {
                            match block {
                                v1::ContentBlock::Text(text) => parts.push(text.text),
                                _ => return responder.respond(serde_json::json!({"v":1,"state":"rejected","reason":"steering supports text blocks only"})),
                            }
                        }
                        let text = parts.join("\n");
                        if text.trim().is_empty() {
                            return responder.respond(serde_json::json!({"v":1,"state":"rejected","reason":"steering text is empty"}));
                        }
                        let ingress = match sessions.steering(&id) {
                            Ok(Some(ingress)) => ingress,
                            Ok(None) => return responder.respond(serde_json::json!({"v":1,"state":"not-busy"})),
                            Err(error) => return responder.respond_with_error(error),
                        };
                        // Durable admission awaits the running coordinator. Keep the
                        // connection free to process permissions and cancellation.
                        cx.spawn(async move {
                            let response = match ingress.submit(text).await {
                                Ok(_) => serde_json::json!({"v":1,"state":"accepted"}),
                                Err(ego_core::turn::ControlError::Closed) => serde_json::json!({"v":1,"state":"not-busy"}),
                                Err(error) => serde_json::json!({"v":1,"state":"rejected","reason":error.to_string()}),
                            };
                            responder.respond(response)
                        })?;
                        Ok(())
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let compaction = compaction.clone();
                let controller = Arc::clone(&controller);
                let lifecycle = lifecycle.clone();
                let confinement = confinement.clone();
                let unadmitted = unadmitted.clone();
                move |request: AcpCompactRequest,
                      responder: Responder<serde_json::Value>,
                      _cx| {
                    let compaction = compaction.clone();
                    let controller = Arc::clone(&controller);
                    let lifecycle = lifecycle.clone();
                    let confinement = confinement.clone();
                    let unadmitted = unadmitted.clone();
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
                    if let Err(error) = unadmitted.refuse(source) {
                        return responder.respond_with_error(error);
                    }
                    let profile_root = if let Some(lifecycle) = &lifecycle {
                        let meta = match lifecycle.meta(source) {
                            Ok(meta) => meta,
                            Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                        };
                        let root = match RunRoot::new(meta.workspace_root) {
                            Ok(root) => root,
                            Err(error) => return responder.respond_with_error(Error::internal_error().data(error.to_string())),
                        };
                        if let Err(error) = restore_session_profiles(controller.as_ref(), lifecycle.as_ref(), confinement.as_ref(), source, source, &root).await {
                            return responder.respond_with_error(error);
                        }
                        Some(root)
                    } else { None };
                    match compaction
                        .compact(CompactionTransitionRequest { request_id, source })
                        .await
                    {
                        Ok(receipt) => {
                            if let (Some(lifecycle), Some(root)) = (&lifecycle, &profile_root)
                                && let Err(error) = restore_session_profiles(controller.as_ref(), lifecycle.as_ref(), confinement.as_ref(), source, receipt.target, root).await {
                                return responder.respond_with_error(error);
                            }
                            match serde_json::to_value(AcpCompactResponse::from(receipt)) {
                                Ok(response) => responder.respond(response),
                                Err(error) => responder.respond_with_error(Error::internal_error().data(error.to_string())),
                            }
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
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let unadmitted = unadmitted.clone();
                move |request: AcpHoldRequest<false>,
                      responder: Responder<serde_json::Value>,
                      _cx| {
                    let lifecycle = lifecycle.clone();
                    let unadmitted = unadmitted.clone();
                    async move { hold(lifecycle.as_deref(), &unadmitted, &request, responder).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                let unadmitted = unadmitted.clone();
                move |request: AcpHoldRequest<true>,
                      responder: Responder<serde_json::Value>,
                      _cx| {
                    let lifecycle = lifecycle.clone();
                    let unadmitted = unadmitted.clone();
                    async move { hold(lifecycle.as_deref(), &unadmitted, &request, responder).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let controls = controls.clone();
                let sessions = sessions.clone();
                let unadmitted = unadmitted.clone();
                let confinement = confinement.clone();
                async move |request: v1::SetSessionConfigOptionRequest, responder, cx: ConnectionTo<Client>| {
                    // Absent controls are unavailable, not partially emulated:
                    // the same rule that makes an unadvertised lifecycle method
                    // answer `method_not_found` rather than pretend.
                    let Some(controls) = &controls else {
                        return responder.respond_with_error(Error::method_not_found());
                    };
                    let session = match parse_session(&request.session_id) {
                        Ok(session) => session,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    if let Err(error) = unadmitted.refuse(session) {
                        return responder.respond_with_error(error);
                    }
                    // Read the offer, resolve against it, then stage. Reading
                    // first is what makes "a value ego never offered" and "a
                    // control ego never advertised" the same refusal instead of
                    // two rules that can drift apart.
                    let state = match controls.state(session) {
                        Ok(state) => state,
                        Err(error) => {
                            return responder.respond_with_error(control_error(&error));
                        }
                    };
                    let change = match crate::controls::change(&state, &request) {
                        Ok(change) => change,
                        Err(error) => {
                            if let (Some(admission), Some(candidate)) = (
                                confinement.as_ref(),
                                crate::controls::requested_confinement(&request),
                            ) && let Err(refusal) =
                                admission.observation(session, candidate)
                            {
                                return responder
                                    .respond_with_error(control_error(&refusal));
                            }
                            return responder.respond_with_error(error);
                        }
                    };
                    let Some(change) = change else {
                        return responder.respond(v1::SetSessionConfigOptionResponse::new(
                            crate::controls::options(&state),
                        ));
                    };
                    // Refused rather than staged. Staging it would leave the
                    // picker showing a confinement the run does not have, and
                    // a host reading its own selection back would be told the
                    // change landed when nothing downstream had moved.
                    let mode_changed = matches!(change, SessionControlChange::Mode(_));
                    let staged = if matches!(change, SessionControlChange::Sandbox(_)) {
                        sessions.stage_confinement(
                            &request.session_id, session, controls.as_ref(), change,
                        )
                    } else {
                        controls.stage(session, change).map_err(|error| control_error(&error))
                    };
                    match staged {
                        Ok(state) => {
                            let options = crate::controls::options(&state);
                            responder.respond(v1::SetSessionConfigOptionResponse::new(options.clone()))?;
                            cx.send_notification(v1::SessionNotification::new(request.session_id.clone(),
                                v1::SessionUpdate::ConfigOptionUpdate(v1::ConfigOptionUpdate::new(options))))?;
                            if mode_changed && let Some(mode) = state.mode.current {
                                cx.send_notification(v1::SessionNotification::new(request.session_id,
                                    v1::SessionUpdate::CurrentModeUpdate(v1::CurrentModeUpdate::new(mode.as_str()))))?;
                            }
                            Ok(())
                        },
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            on_receive_request!(),
        )
}
