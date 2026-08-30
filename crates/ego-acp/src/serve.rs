//! The inbound ACP adapter over core's rooted run controller.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::negotiate::{SERVED, negotiate};
use crate::project::stop_reason_v1;
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
use ego_core::run::{
    CancelKey, ContinueRequest, EventSinkFactory, IdSource, RequestDigest, RequestId,
    RunCommandOutcome, RunController, RunExecutor, RunRoot, RunSelections, RunSettlement,
    RunSnapshot, StartRequest,
};
use ego_core::sink::EventSink;
use ego_core::store::session_store::SessionStore;
use ego_core::turn::AdmittedToolSource;
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

fn capabilities(compaction: bool) -> v1::AgentCapabilities {
    let capabilities = v1::AgentCapabilities::new()
        .load_session(false)
        .mcp_capabilities(v1::McpCapabilities::new().http(true).sse(false));
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
    root: RunRoot,
    start_request: RequestId,
    run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
    busy: bool,
    active_cancel: Option<CancelKey>,
    mcp: Option<Arc<dyn SessionMcpAdmission>>,
}

struct PromptCorrelation {
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

#[derive(Clone, Default)]
struct Sessions(Arc<Mutex<HashMap<v1::SessionId, Registered>>>);

impl Sessions {
    fn open(
        &self,
        id: v1::SessionId,
        root: RunRoot,
        start_request: RequestId,
        mcp: Option<Arc<dyn SessionMcpAdmission>>,
    ) {
        self.lock().insert(
            id,
            Registered {
                root,
                start_request,
                run: None,
                busy: false,
                active_cancel: None,
                mcp,
            },
        );
    }

    fn begin(&self, id: &v1::SessionId, ids: &dyn IdSource) -> BeginPrompt {
        let mut sessions = self.lock();
        let Some(session) = sessions.get_mut(id) else {
            return BeginPrompt::Unknown;
        };
        if session.busy {
            return BeginPrompt::Busy;
        }
        session.busy = true;
        let request_id = if session.run.is_none() {
            session.start_request
        } else {
            RequestId::from_uuid(ids.next_uuid())
        };
        BeginPrompt::Ready(PromptCorrelation {
            root: session.root.clone(),
            request_id,
            run: session.run,
            mcp: session.mcp.as_ref().map(Arc::clone),
        })
    }

    fn install_admission(
        &self,
        id: &v1::SessionId,
        run_id: ego_core::run::RunId,
        revision: ego_core::run::RunRevision,
        key: CancelKey,
    ) {
        if let Some(session) = self.lock().get_mut(id) {
            session.run = Some((run_id, revision));
            session.active_cancel = Some(key);
        }
    }

    fn finish(&self, id: &v1::SessionId, snapshot: &RunSnapshot) {
        if let Some(session) = self.lock().get_mut(id) {
            session.run = Some((snapshot.run_id(), snapshot.revision()));
            session.active_cancel = None;
            session.busy = false;
        }
    }

    fn refuse(&self, id: &v1::SessionId) {
        if let Some(session) = self.lock().get_mut(id) {
            session.active_cancel = None;
            session.busy = false;
        }
    }

    fn cancel_key(&self, id: &v1::SessionId) -> Option<CancelKey> {
        self.lock()
            .get(id)
            .and_then(|session| session.active_cancel.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<v1::SessionId, Registered>> {
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
    ))
}

fn v1_agent<S, E, C, I>(
    controller: Arc<RunController<S, E, C, I>>,
    roots: Arc<dyn RootResolver>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
    mcp_admitter: Arc<dyn McpSessionAdmitter>,
    compaction: Option<Arc<dyn CompactionCommand>>,
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
                            .agent_capabilities(capabilities(advertises_compaction))
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
                async move |request: v1::NewSessionRequest, responder, _cx| {
                    if let Err(error) = validate_mcp_servers(&request.mcp_servers) {
                        return responder.respond_with_error(
                            Error::invalid_params().data(error.to_string()),
                        );
                    }
                    let root = match roots.resolve(&request.cwd) {
                        Ok(root) => root,
                        Err(error) => {
                            return responder.respond_with_error(
                                Error::invalid_params().data(error.to_string()),
                            );
                        }
                    };
                    let supplied_mcp = !request.mcp_servers.is_empty();
                    let mcp = match mcp_admitter.admit(request.mcp_servers).await {
                        Ok(Some(admission)) if supplied_mcp => Some(admission),
                        Ok(Some(_)) => {
                            return responder.respond_with_error(Error::invalid_params().data(
                                "an empty ACP MCP selection unexpectedly produced live authority",
                            ));
                        }
                        Ok(None) if !supplied_mcp => None,
                        Ok(None) => {
                            return responder.respond_with_error(Error::invalid_params().data(
                                "the supplied MCP servers did not produce live session authority",
                            ));
                        }
                        Err(error) => {
                            return responder.respond_with_error(
                                Error::invalid_params().data(error.to_string()),
                            );
                        }
                    };
                    let start_request = RequestId::from_uuid(ids.next_uuid());
                    let session = RequestId::session_id(start_request);
                    let id = v1::SessionId::new(session.to_string());
                    sessions.open(id.clone(), root, start_request, mcp);
                    responder.respond(v1::NewSessionResponse::new(id))
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
                    let correlation = match sessions.begin(&id, ids.as_ref()) {
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
                    let digest = serde_json::to_string(&request)
                        .expect("ACP request serialization is infallible");
                    let text = match prompt_text(&request.prompt) {
                        Ok(text) => text,
                        Err(refusal) => {
                            sessions.refuse(&id);
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
                        ),
                        None => bindings,
                    };
                    let outcome = match correlation.run {
                        None => {
                            controller
                                .start(
                                    StartRequest::new(
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
                            sessions.refuse(&id);
                            return responder.respond_with_error(
                                Error::internal_error().data(error.to_string()),
                            );
                        }
                    };
                    if let Some(execution) = execution {
                        let controller = Arc::clone(&controller);
                        spawner.spawn(Box::pin(async move {
                            if let Err(error) = controller.execute(execution).await {
                                tracing::warn!(%error, "admitted ACP execution failed");
                            }
                        }));
                    }
                    let sessions = sessions.clone();
                    cx.spawn(async move {
                        let snapshot = subscription.await;
                        sessions.finish(&id, &snapshot);
                        match prompt_result(&snapshot) {
                            Ok(stop_reason) => {
                                responder.respond(v1::PromptResponse::new(stop_reason))
                            }
                            Err(message) => {
                                responder.respond_with_error(Error::new(
                                    i32::from(ErrorCode::InternalError),
                                    message,
                                ))
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
