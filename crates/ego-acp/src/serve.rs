//! The inbound ACP adapter over core's rooted run controller.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::negotiate::{SERVED, negotiate};
use crate::project::stop_reason_v1;
use crate::question::{AcpQuestionAdapter, CapabilityCapture};
use agent_client_protocol::{
    Agent, AgentProtocolRouter, Client, ConnectionTo, Error, ErrorCode, on_receive_notification,
    on_receive_request,
};
use agent_client_protocol_schema::v1;
use ego_core::event::time::Clock;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::run::{
    CancelKey, ContinueRequest, EventSinkFactory, IdSource, RequestDigest, RequestId,
    RunCommandOutcome, RunController, RunExecutor, RunRoot, RunSettlement, RunSnapshot,
    StartRequest,
};
use ego_core::sink::EventSink;
use ego_core::store::session_store::SessionStore;

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

fn capabilities() -> v1::AgentCapabilities {
    v1::AgentCapabilities::new()
}

struct Registered {
    root: RunRoot,
    start_request: RequestId,
    run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
    busy: bool,
    active_cancel: Option<CancelKey>,
}

struct PromptCorrelation {
    root: RunRoot,
    request_id: RequestId,
    run: Option<(ego_core::run::RunId, ego_core::run::RunRevision)>,
}

enum BeginPrompt {
    Ready(PromptCorrelation),
    Busy,
    Unknown,
}

#[derive(Clone, Default)]
struct Sessions(Arc<Mutex<HashMap<v1::SessionId, Registered>>>);

impl Sessions {
    fn open(&self, id: v1::SessionId, root: RunRoot, start_request: RequestId) {
        self.lock().insert(
            id,
            Registered {
                root,
                start_request,
                run: None,
                busy: false,
                active_cancel: None,
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
) -> AgentProtocolRouter
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    Agent
        .protocol_router()
        .with_v1(v1_agent(controller, roots, spawner, ids, bindings_builder))
}

fn v1_agent<S, E, C, I>(
    controller: Arc<RunController<S, E, C, I>>,
    roots: Arc<dyn RootResolver>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
) -> impl agent_client_protocol::ConnectTo<Client>
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    let sessions = Sessions::default();
    let client_capabilities = Arc::new(Mutex::new(CapabilityCapture::default()));
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
                            .agent_capabilities(capabilities())
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
                async move |request: v1::NewSessionRequest, responder, _cx| {
                    if !request.mcp_servers.is_empty() {
                        return responder.respond_with_error(Error::invalid_params().data(
                            "this build has no MCP client, so the servers named here would be accepted and never connected to",
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
                    let start_request = RequestId::from_uuid(ids.next_uuid());
                    let session = RequestId::session_id(start_request);
                    let id = v1::SessionId::new(session.to_string());
                    sessions.open(id.clone(), root, start_request);
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
                    let outcome = match correlation.run {
                        None => {
                            controller
                                .start(
                                    StartRequest::new(
                                        correlation.root,
                                        correlation.request_id,
                                        digest,
                                        None,
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
}
