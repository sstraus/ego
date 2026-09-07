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
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ego_core::compact::{CompactionCommand, CompactionReceipt, CompactionTransitionRequest};
use ego_core::control::{ControlError, SessionControls};
use ego_core::event::id::SessionId;
use ego_core::event::time::Clock;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::generator::types::{Image, ImageMediaType};
use ego_core::mcp::McpRuntimeBinding;
use ego_core::run::{
    CancelKey, ContinueRequest, EventSinkFactory, IdSource, RequestDigest, RequestId,
    RunCommandOutcome, RunController, RunError, RunExecution, RunExecutionBindings, RunExecutor,
    RunPrompt, RunRecoveryOutcome, RunRoot, RunSelections, RunSettlement, RunSnapshot, RunStatus,
    RunSubscription, StartRequest,
};
use ego_core::session_lifecycle::{
    HoldState, LifecycleError, SessionAttachment, SessionCursor, SessionLifecycle,
    SessionLifecycleStore, SessionPage,
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
pub struct AcpRunBindingsBuilder {
    controls: Option<Arc<dyn SessionControls>>,
}

impl AcpRunBindingsBuilder {
    /// Bind the connection's operator seat, if it owns one.
    ///
    /// The same seat the router answers `session/set_config_option` from, so
    /// what a host was told it picked is what the next generation reads.
    pub fn with_controls(controls: Option<Arc<dyn SessionControls>>) -> Self {
        Self { controls }
    }
}

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
        let elicitor =
            AcpQuestionAdapter::live(session.clone(), capabilities, connection.clone()).elicitor();
        // The connected client is this turn's only human. Installing the seat
        // here, beside the elicitor, is what stops an escalation from ending
        // as an unanswerable internal error the host cannot act on.
        let human =
            crate::permission::AcpPermissionAdapter::live(session.clone(), connection.clone())
                .approver();
        let bindings = ego_core::run::RunExecutionBindings::new(
            Arc::new(AcpSinkFactory {
                connection,
                session,
            }),
            Some(elicitor),
        )
        .with_human(human);
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
    // Built entry by entry rather than as one literal, so an extension is
    // advertised exactly when the port that serves it exists. A host that
    // negotiated a build with lifecycle and no compaction is told about
    // `_ego/pause` and not `_ego/compact`, which is the truth; a single
    // literal gated on one flag would have made the two stand or fall
    // together and one of the two claims would always be a lie.
    let mut ego = serde_json::Map::new();
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
        // An internal invariant failed. This used to answer `cancelled`
        // alongside the line above, which told a host its own cancellation
        // had succeeded when in fact this agent had broken. It is an error,
        // for the same reason `Uncertain` below is one.
        Some(RunSettlement::Interrupted) => {
            Err("the turn was interrupted by an internal failure and did not run".to_string())
        }
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

    fn pause(&self, id: SessionId, request: RequestId) -> LifecycleFuture<'_, HoldState> {
        Box::pin(SessionLifecycle::pause(self, id, request))
    }

    fn release(&self, id: SessionId, request: RequestId) -> LifecycleFuture<'_, HoldState> {
        Box::pin(SessionLifecycle::release(self, id, request))
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

/// The generation controls this session offers, or nothing.
///
/// Absent controls stay absent rather than becoming an empty picker, which is
/// the same fallback rule `docs/06-acp.md` §3 applies to usage and cost. A
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

fn control_error(error: &ControlError) -> Error {
    match error {
        // Both name a value the caller sent, so both are the caller's
        // parameters being wrong rather than ego failing.
        ControlError::Unavailable { .. } | ControlError::Refused { .. } => {
            Error::invalid_params().data(error.to_string())
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
        LifecycleError::LeaseHeld
        | LifecycleError::Active
        | LifecycleError::Conflict
        | LifecycleError::EffectUncertain => ErrorCode::Other(SESSION_BUSY),
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
fn session_info(meta: SessionMeta) -> v1::SessionInfo {
    v1::SessionInfo::new(meta.id.to_string(), meta.workspace_root)
        .title(meta.title)
        .updated_at(meta.updated.to_rfc3339())
}

fn emit_info(
    connection: &ConnectionTo<Client>,
    id: &v1::SessionId,
    meta: &SessionMeta,
) -> Result<(), Error> {
    connection.send_notification(v1::SessionNotification::new(
        id.clone(),
        v1::SessionUpdate::SessionInfoUpdate(
            v1::SessionInfoUpdate::new()
                .title(meta.title.clone())
                .updated_at(meta.updated.to_rfc3339()),
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
        None,
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
                let controls = controls.clone();
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
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    let start_request = RequestId::from_uuid(ids.next_uuid());
                    let session = RequestId::session_id(start_request);
                    let id = v1::SessionId::new(session.to_string());
                    if let Some(lifecycle) = &lifecycle
                        && let Err(error) =
                            lifecycle.create_with_id(session, root.as_path().to_path_buf())
                    {
                        return responder.respond_with_error(lifecycle_error(&error));
                    }
                    let attachment_root = root.clone();
                    let _ = sessions.open(id.clone(), root, Some(start_request), None, mcp);
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
                    // The response first, the announcement after it. A
                    // `session_info_update` names a session by an id the client
                    // learns from *this* response, so a client that indexes
                    // sessions by the response — the ordering `project.rs`
                    // already documents — drops any update that precedes it.
                    // Everything able to fail the request has happened above;
                    // answering first therefore loses nothing.
                    responder.respond(
                        v1::NewSessionResponse::new(id.clone())
                            .config_options(config_options(controls.as_ref(), session)),
                    )?;
                    match announcement {
                        Some(meta) => emit_info(&cx, &id, &meta),
                        None => Ok(()),
                    }
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
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    let cursor = match request.cursor.map(SessionCursor::parse).transpose() {
                        Ok(cursor) => cursor,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    match lifecycle.list(cursor.as_ref(), root.as_ref().map(RunRoot::as_path)) {
                        Ok(page) => responder.respond(
                            v1::ListSessionsResponse::new(
                                page.items.into_iter().map(session_info).collect(),
                            )
                            .next_cursor(page.next_cursor.map(|cursor| cursor.to_string())),
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
                let controller = Arc::clone(&controller);
                let roots = Arc::clone(&roots);
                let ids = Arc::clone(&ids);
                let mcp_admitter = Arc::clone(&mcp_admitter);
                let spawner = Arc::clone(&spawner);
                let bindings_builder = Arc::clone(&bindings_builder);
                let client_capabilities = Arc::clone(&client_capabilities);
                let controls = controls.clone();
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
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    let attachment = match lifecycle.load(id, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    let mcp = match admit_mcp(mcp_admitter.as_ref(), request.mcp_servers).await {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(refused_params(error)),
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
                        Err(error) => return responder.respond_with_error(Error::internal_error().data(error)),
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
                    // Answer before replaying, for the reason `session/new`
                    // does: every notification below names this session, and a
                    // client that indexes by the response has not seen the id
                    // yet. The whole replay, not just the trailing info update,
                    // would be dropped.
                    responder.respond(
                        v1::LoadSessionResponse::new()
                            .config_options(config_options(controls.as_ref(), id)),
                    )?;
                    emit_attachment(&cx, &request.session_id, &attachment)
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
                let controls = controls.clone();
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
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    let attachment = match lifecycle.resume(id, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    let mcp = match admit_mcp(mcp_admitter.as_ref(), request.mcp_servers).await {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(refused_params(error)),
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
                        Err(error) => return responder.respond_with_error(Error::internal_error().data(error)),
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
                    responder.respond(
                        v1::ResumeSessionResponse::new()
                            .config_options(config_options(controls.as_ref(), id)),
                    )?;
                    emit_attachment(&cx, &request.session_id, &attachment)
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
                let controls = controls.clone();
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
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    if let Err(error) = lifecycle.resume(source, root.as_path()).await {
                        return responder.respond_with_error(lifecycle_error(&error));
                    }
                    // Admission precedes publication of the child so an MCP
                    // refusal cannot leave an unexplained durable fork. If
                    // the later lifecycle transition refuses, this temporary
                    // connection authority is dropped without attachment.
                    let mcp = match admit_mcp(mcp_admitter.as_ref(), request.mcp_servers).await {
                        Ok(mcp) => mcp,
                        Err(error) => return responder.respond_with_error(refused_params(error)),
                    };
                    let child = match lifecycle.fork(source).await {
                        Ok(child) => child.id,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    let attachment = match lifecycle.resume(child, root.as_path()).await {
                        Ok(attachment) => attachment,
                        Err(error) => return responder.respond_with_error(lifecycle_error(&error)),
                    };
                    let child_id = v1::SessionId::new(child.to_string());
                    let _ = sessions.open(
                        child_id.clone(),
                        root,
                        Some(RequestId::from_uuid(ids.next_uuid())),
                        None,
                        mcp,
                    );
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
                    emit_info(&cx, &child_id, &attachment.meta)
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
                        Err(error) => responder.respond_with_error(lifecycle_error(&error)),
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
                    // Derived, not copied: this value is written into the
                    // durable run transaction, so serializing the request into
                    // it would persist the prompt a second time -- and, once a
                    // block can carry image bytes, would write those bytes into
                    // the event stream that
                    // `docs/decisions/2026-08-29-images-are-session-blobs-
                    // before-they-are-wire-parts.md` keeps them out of.
                    let digest = match RequestDigest::of(&request) {
                        Ok(digest) => digest,
                        Err(error) => {
                            return responder
                                .respond_with_error(Error::invalid_params().data(error.to_string()));
                        }
                    };
                    let correlation =
                        match sessions.begin(&id, ids.as_ref(), &command, digest.as_str()) {
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
                    let prompt = match prompt_content(&request.prompt) {
                        Ok(prompt) => prompt,
                        Err(refusal) => {
                            sessions.refuse(&id, correlation.generation);
                            return responder.respond_with_error(
                                Error::invalid_params().data(refusal),
                            );
                        }
                    };
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
                                        prompt,
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
                                        prompt,
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
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                move |request: AcpHoldRequest<false>,
                      responder: Responder<serde_json::Value>,
                      _cx| {
                    let lifecycle = lifecycle.clone();
                    async move { hold(lifecycle.as_deref(), &request, responder).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let lifecycle = lifecycle.clone();
                move |request: AcpHoldRequest<true>,
                      responder: Responder<serde_json::Value>,
                      _cx| {
                    let lifecycle = lifecycle.clone();
                    async move { hold(lifecycle.as_deref(), &request, responder).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let controls = controls.clone();
                async move |request: v1::SetSessionConfigOptionRequest, responder, _cx| {
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
                        Err(error) => return responder.respond_with_error(error),
                    };
                    match controls.stage(session, change) {
                        Ok(state) => responder.respond(v1::SetSessionConfigOptionResponse::new(
                            crate::controls::options(&state),
                        )),
                        Err(error) => responder.respond_with_error(control_error(&error)),
                    }
                }
            },
            on_receive_request!(),
        )
}
