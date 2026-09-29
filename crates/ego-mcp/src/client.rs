use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ego_core::config::{McpHttpConfig, McpServerName};
use ego_core::generator::types::ToolDescriptor;
use ego_core::permit::{DuplexSettlement, HttpHeader, HttpRequest};
use futures::StreamExt;
use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientCapabilities,
    ClientInfo, ClientJsonRpcMessage, ClientRequest, ElicitationCapability,
    FormElicitationCapability, GetPromptRequestParams, GetPromptResult, Implementation,
    ListPromptsRequest, ListResourcesRequest, ListToolsRequest, PaginatedRequestParams, Prompt,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResult, Resource, ServerCapabilities,
    ServerJsonRpcMessage, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::common::client_side_sse::NeverRetry;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpClientTransport,
    StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
};
use rmcp::{ClientLifecycleMode, RoleClient, serve_client_with_lifecycle};
use sse_stream::SseStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::http::{AdmittedMcpHttpProfile, header, path_and_query};
use crate::stdio::SharedDuplex;
use crate::{
    McpAcpEffects, McpDuplexEffects, McpError, McpFailure, McpHttpEffects, McpHttpEnd,
    McpHttpPurpose, McpHttpSettlement, McpManualOutcome, McpRequestId,
};

pub const MCP_PROTOCOL_REVISION: &str = "2026-07-28";
const MAX_JSON_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn current_client_capabilities() -> ClientCapabilities {
    ClientCapabilities::builder()
        .enable_elicitation_with(
            ElicitationCapability::new()
                .with_form(FormElicitationCapability::new().with_schema_validation(true)),
        )
        .build()
}

#[derive(Clone, Copy, Debug)]
pub struct McpClientOptions {
    max_sse_event_size: usize,
    deadline: Instant,
}

impl McpClientOptions {
    pub fn new(max_sse_event_size: usize, deadline: Instant) -> Result<Self, McpError> {
        if max_sse_event_size == 0 || deadline <= Instant::now() {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid MCP client options",
            ));
        }
        Ok(Self {
            max_sse_event_size,
            deadline,
        })
    }
}

struct ActiveHttpRequest {
    request_id: McpRequestId,
}

#[derive(Default)]
struct ClientControl {
    cancelled: AtomicBool,
    operation_claimed: AtomicBool,
    active: Mutex<Option<ActiveHttpRequest>>,
    settlements: Mutex<Vec<McpHttpSettlement>>,
}

impl ClientControl {
    fn register(
        self: &Arc<Self>,
        request_id: McpRequestId,
        effects: Arc<dyn McpHttpEffects>,
    ) -> Result<ActiveHttpGuard, McpError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(McpError::new(
                McpFailure::Cancelled,
                "MCP client was cancelled",
            ));
        }
        let mut active = self
            .active
            .lock()
            .map_err(|_| McpError::new(McpFailure::Transport, "MCP request lock poisoned"))?;
        if active.is_some() {
            return Err(McpError::new(
                McpFailure::Protocol,
                "an MCP client operation is already active",
            ));
        }
        *active = Some(ActiveHttpRequest {
            request_id: request_id.clone(),
        });
        Ok(ActiveHttpGuard {
            request_id,
            effects,
            control: Arc::clone(self),
            finished: false,
        })
    }

    fn finish(&self, request_id: &McpRequestId, settlement: McpHttpSettlement) {
        if let Ok(mut active) = self.active.lock()
            && active
                .as_ref()
                .is_some_and(|active| &active.request_id == request_id)
        {
            active.take();
        }
        self.record(settlement);
    }

    fn record(&self, settlement: McpHttpSettlement) {
        if let Ok(mut settlements) = self.settlements.lock()
            && !settlements
                .iter()
                .any(|known| known.request_id() == settlement.request_id())
        {
            settlements.push(settlement);
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn settlements(&self) -> Vec<McpHttpSettlement> {
        self.settlements
            .lock()
            .map_or_else(|_| Vec::new(), |settlements| settlements.clone())
    }
}

struct ActiveHttpGuard {
    request_id: McpRequestId,
    effects: Arc<dyn McpHttpEffects>,
    control: Arc<ClientControl>,
    finished: bool,
}

impl ActiveHttpGuard {
    async fn cancel(mut self) -> Result<(), McpError> {
        self.finished = true;
        let settlement = self.effects.cancel(self.request_id.clone()).await?;
        self.control.finish(&self.request_id, settlement);
        Ok(())
    }

    async fn timeout(mut self) -> Result<(), McpError> {
        self.finished = true;
        let settlement = self.effects.timeout(self.request_id.clone()).await?;
        if settlement.request_id() != &self.request_id || settlement.end() != McpHttpEnd::TimedOut {
            return Err(McpError::new(
                McpFailure::InvalidResponse,
                "MCP HTTP timeout returned an invalid settlement",
            ));
        }
        self.control.finish(&self.request_id, settlement);
        Ok(())
    }

    async fn settle(mut self) -> Result<(), McpError> {
        self.finished = true;
        let settlement = self.effects.settle(self.request_id.clone()).await?;
        self.control.finish(&self.request_id, settlement);
        Ok(())
    }
}

struct DiscoverDropGuard {
    control: Arc<ClientControl>,
    armed: bool,
}

impl DiscoverDropGuard {
    fn new(control: Arc<ClientControl>) -> Self {
        Self {
            control,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DiscoverDropGuard {
    fn drop(&mut self) {
        if self.armed {
            self.control.cancel();
        }
    }
}

impl Drop for ActiveHttpGuard {
    fn drop(&mut self) {
        if self.finished || self.control.cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Ok(settlement) =
            futures::executor::block_on(self.effects.settle(self.request_id.clone()))
        {
            self.control.finish(&self.request_id, settlement);
        }
    }
}

#[derive(Clone)]
pub struct McpClientCancellation {
    control: Arc<ClientControl>,
}

impl McpClientCancellation {
    pub fn cancel(&self) {
        self.control.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.control.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct EgoStreamableHttpClient {
    server: McpServerName,
    url: url::Url,
    profile: AdmittedMcpHttpProfile,
    effects: Arc<dyn McpHttpEffects>,
    options: McpClientOptions,
    control: Arc<ClientControl>,
}

impl StreamableHttpClient for EgoStreamableHttpClient {
    type Error = McpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            self.options.max_sse_event_size,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        if session_id.is_some() {
            return Err(unexpected("MCP HTTP sessions are disabled"));
        }
        if uri.as_ref() != self.url.as_str() {
            return Err(unexpected("rmcp requested an unadmitted MCP URI"));
        }
        if max_sse_event_size == 0 || max_sse_event_size != self.options.max_sse_event_size {
            return Err(unexpected("rmcp changed the admitted SSE event bound"));
        }
        if self.control.cancelled.load(Ordering::Acquire) {
            return Err(StreamableHttpError::Client(McpError::new(
                McpFailure::Cancelled,
                "MCP client was cancelled",
            )));
        }

        let body = serde_json::to_vec(&message).map_err(StreamableHttpError::Deserialize)?;
        let mut headers = Vec::with_capacity(custom_headers.len() + 3);
        headers.push(http_header(
            "accept",
            b"application/json, text/event-stream".to_vec(),
        )?);
        headers.push(http_header("content-type", b"application/json".to_vec())?);
        if let Some(token) = auth_header {
            headers.push(http_header(
                "authorization",
                format!("Bearer {token}").into_bytes(),
            )?);
        }
        for (name, value) in custom_headers {
            if name.as_str().eq_ignore_ascii_case("mcp-session-id") {
                return Err(unexpected("MCP HTTP session header is disabled"));
            }
            headers.push(http_header(name.as_str(), value.as_bytes().to_vec())?);
        }
        let request = HttpRequest::new("POST", &path_and_query(&self.url), headers, body).map_err(
            |error| {
                StreamableHttpError::Client(McpError::new(
                    McpFailure::InvalidValue,
                    error.to_string(),
                ))
            },
        )?;
        let call = self
            .profile
            .prepare(
                &self.server,
                McpHttpPurpose::Protocol,
                &self.url,
                request,
                self.options.deadline,
            )
            .map_err(StreamableHttpError::Client)?;
        let request_id = call.request_id().clone();
        let guard = self
            .control
            .register(request_id, Arc::clone(&self.effects))
            .map_err(StreamableHttpError::Client)?;
        let mut execute = self.effects.execute(call);
        let outcome = tokio::select! {
            biased;
            () = wait_cancelled(Arc::clone(&self.control)) => (None, false),
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(self.options.deadline)) => {
                (None, true)
            }
            response = &mut execute => (Some(response), false),
        };
        let response = match outcome.0 {
            Some(response) => response.map_err(StreamableHttpError::Client)?,
            None => {
                drop(execute);
                if outcome.1 {
                    guard.timeout().await.map_err(StreamableHttpError::Client)?;
                } else {
                    guard.cancel().await.map_err(StreamableHttpError::Client)?;
                }
                return Err(StreamableHttpError::Client(McpError::new(
                    if outcome.1 {
                        McpFailure::Transport
                    } else {
                        McpFailure::Cancelled
                    },
                    if outcome.1 {
                        "MCP request deadline elapsed"
                    } else {
                        "MCP client was cancelled"
                    },
                )));
            }
        };
        if response.header_value("mcp-session-id").is_some() {
            return Err(unexpected("MCP server attempted to establish a session"));
        }
        let status = response.status();
        if matches!(status, 202 | 204) {
            guard.settle().await.map_err(StreamableHttpError::Client)?;
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let content_type = response
            .header_value("content-type")
            .map(|value| String::from_utf8_lossy(value).into_owned());
        if content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            let body = response
                .into_body()
                .ok_or_else(|| unexpected("SSE response omitted its body"))?;
            return Ok(StreamableHttpPostResponse::Sse(
                bounded_sse_stream(body, max_sse_event_size, self.options.deadline, guard),
                None,
            ));
        }
        if content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("application/json"))
        {
            let bytes = response
                .into_bytes(MAX_JSON_RESPONSE_BYTES)
                .await
                .map_err(StreamableHttpError::Client)?;
            let message = serde_json::from_slice::<ServerJsonRpcMessage>(&bytes)
                .map_err(StreamableHttpError::Deserialize)?;
            guard.settle().await.map_err(StreamableHttpError::Client)?;
            return Ok(StreamableHttpPostResponse::Json(message, None));
        }
        Err(StreamableHttpError::UnexpectedContentType(content_type))
    }

    async fn delete_session(
        &self,
        _uri: Arc<str>,
        _session_id: Arc<str>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        Err(StreamableHttpError::ServerDoesNotSupportDeleteSession)
    }

    async fn get_stream(
        &self,
        _uri: Arc<str>,
        _session_id: Option<Arc<str>>,
        _last_event_id: Option<String>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<
        BoxStream<'static, Result<sse_stream::Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        Err(StreamableHttpError::ServerDoesNotSupportSse)
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        _uri: Arc<str>,
        _session_id: Option<Arc<str>>,
        _last_event_id: Option<String>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
        _max_sse_event_size: usize,
    ) -> Result<
        BoxStream<'static, Result<sse_stream::Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        Err(StreamableHttpError::ServerDoesNotSupportSse)
    }
}

fn http_header(name: &str, value: Vec<u8>) -> Result<HttpHeader, StreamableHttpError<McpError>> {
    header(name, value).map_err(StreamableHttpError::Client)
}

fn unexpected(message: &'static str) -> StreamableHttpError<McpError> {
    StreamableHttpError::UnexpectedServerResponse(message.into())
}

#[derive(Debug)]
struct SseEventSizeLimiter {
    max_size: usize,
    retained_size: usize,
    line_size: usize,
    line_is_comment: bool,
    previous_was_cr: bool,
}

impl SseEventSizeLimiter {
    fn new(max_size: usize) -> Self {
        Self {
            max_size,
            retained_size: 0,
            line_size: 0,
            line_is_comment: false,
            previous_was_cr: false,
        }
    }

    fn observe(&mut self, chunk: &[u8]) -> Result<(), std::io::Error> {
        for &byte in chunk {
            if self.previous_was_cr {
                self.previous_was_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    self.finish_line()?;
                    self.previous_was_cr = true;
                }
                b'\n' => self.finish_line()?,
                _ => {
                    if self.line_size == 0 {
                        self.line_is_comment = byte == b':';
                    }
                    self.line_size = self.line_size.saturating_add(1);
                    self.check_limit()?;
                }
            }
        }
        Ok(())
    }

    fn finish_line(&mut self) -> Result<(), std::io::Error> {
        if self.line_size == 0 {
            self.retained_size = 0;
        } else if !self.line_is_comment {
            self.retained_size = self
                .retained_size
                .saturating_add(self.line_size)
                .saturating_add(1);
        }
        self.line_size = 0;
        self.line_is_comment = false;
        self.check_limit()
    }

    fn check_limit(&self) -> Result<(), std::io::Error> {
        if self.retained_size.saturating_add(self.line_size) > self.max_size {
            Err(std::io::Error::other(format!(
                "SSE event exceeded the maximum size of {} bytes",
                self.max_size
            )))
        } else {
            Ok(())
        }
    }
}

fn bounded_sse_stream(
    body: crate::McpHttpBody,
    max_event_size: usize,
    deadline: Instant,
    guard: ActiveHttpGuard,
) -> BoxStream<'static, Result<sse_stream::Sse, SseError>> {
    struct State {
        body: crate::McpHttpBody,
        limiter: SseEventSizeLimiter,
        guard: Option<ActiveHttpGuard>,
    }
    let bytes = futures::stream::unfold(
        Some(State {
            body,
            limiter: SseEventSizeLimiter::new(max_event_size),
            guard: Some(guard),
        }),
        move |state| async move {
            let mut state = state?;
            let control = Arc::clone(&state.guard.as_ref()?.control);
            let outcome = tokio::select! {
                biased;
                () = wait_cancelled(control) => (None, false),
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    (None, true)
                }
                item = state.body.next() => (Some(item), false),
            };
            match outcome.0 {
                None => {
                    drop(state.body);
                    let guard = state.guard.take()?;
                    let termination = if outcome.1 {
                        guard.timeout().await
                    } else {
                        guard.cancel().await
                    };
                    match termination {
                        Ok(()) => None,
                        Err(error) => Some((Err(std::io::Error::other(error)), None)),
                    }
                }
                Some(item) => match item {
                    Some(Ok(chunk)) => match state.limiter.observe(&chunk) {
                        Ok(()) => Some((Ok(Cursor::new(chunk)), Some(state))),
                        Err(error) => {
                            if let Some(guard) = state.guard.take() {
                                let _ = guard.settle().await;
                            }
                            Some((Err(error), None))
                        }
                    },
                    Some(Err(error)) => {
                        if let Some(guard) = state.guard.take() {
                            let _ = guard.settle().await;
                        }
                        Some((Err(std::io::Error::other(error)), None))
                    }
                    None => {
                        let guard = state.guard.take()?;
                        match guard.settle().await {
                            Ok(()) => None,
                            Err(error) => Some((Err(std::io::Error::other(error)), None)),
                        }
                    }
                },
            }
        },
    );
    SseStream::from_bytes_stream(bytes).boxed()
}

enum PendingTransport {
    Http {
        config: McpHttpConfig,
        profile: AdmittedMcpHttpProfile,
        effects: Arc<dyn McpHttpEffects>,
    },
    Stdio {
        effects: Box<dyn McpDuplexEffects>,
    },
    Acp {
        stream: tokio::io::DuplexStream,
    },
}

struct AcpConnection {
    id: String,
    effects: Arc<dyn McpAcpEffects>,
    bridge: tokio::task::JoinHandle<()>,
}

enum ClientState {
    Pending(PendingTransport),
    Running {
        service: RunningService<RoleClient, ClientInfo>,
        duplex: Option<SharedDuplex>,
    },
    Failed {
        duplex: Option<SharedDuplex>,
    },
    Empty,
}

pub struct McpClient {
    server: McpServerName,
    options: McpClientOptions,
    control: Arc<ClientControl>,
    state: ClientState,
    acp: Option<AcpConnection>,
}

pub(crate) struct McpClientInventory {
    pub(crate) capabilities: ServerCapabilities,
    /// What the server said about itself at `initialize`, verbatim.
    pub(crate) instructions: Option<String>,
    pub(crate) tools: Vec<ToolDescriptor>,
    pub(crate) resources: Vec<Resource>,
    pub(crate) prompts: Vec<Prompt>,
    pub(crate) expires_at: Option<Instant>,
}

struct CurrentList<T> {
    items: Vec<T>,
    minimum_ttl_ms: Option<u64>,
}

impl McpClient {
    pub fn connect_http(
        server: McpServerName,
        config: McpHttpConfig,
        options: McpClientOptions,
        effects: Arc<dyn McpHttpEffects>,
    ) -> Result<Self, McpError> {
        config
            .validate()
            .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
        let profile = AdmittedMcpHttpProfile::initial(&server, config.url.as_url())?;
        Ok(Self {
            server,
            options,
            control: Arc::new(ClientControl::default()),
            state: ClientState::Pending(PendingTransport::Http {
                config,
                profile,
                effects,
            }),
            acp: None,
        })
    }

    pub fn connect_stdio(
        server: McpServerName,
        options: McpClientOptions,
        effects: Box<dyn McpDuplexEffects>,
    ) -> Result<Self, McpError> {
        Ok(Self {
            server,
            options,
            control: Arc::new(ClientControl::default()),
            state: ClientState::Pending(PendingTransport::Stdio { effects }),
            acp: None,
        })
    }

    pub fn connect_acp(
        server: McpServerName,
        options: McpClientOptions,
        connection_id: String,
        effects: Arc<dyn McpAcpEffects>,
    ) -> Result<Self, McpError> {
        if connection_id.is_empty() {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "empty ACP MCP connection id",
            ));
        }
        let (stream, host_stream) = tokio::io::duplex(64 * 1024);
        let bridge = tokio::spawn(acp_bridge(
            host_stream,
            connection_id.clone(),
            Arc::clone(&effects),
        ));
        Ok(Self {
            server,
            options,
            control: Arc::new(ClientControl::default()),
            state: ClientState::Pending(PendingTransport::Acp { stream }),
            acp: Some(AcpConnection {
                id: connection_id,
                effects,
                bridge,
            }),
        })
    }

    /// Give a retained ACP client fresh authority and a fresh deadline for one call.
    pub fn begin_acp_operation(&mut self, options: McpClientOptions) -> Result<(), McpError> {
        if self.acp.is_none() || !matches!(self.state, ClientState::Running { .. }) {
            return Err(McpError::new(
                McpFailure::Protocol,
                "ACP MCP connection is not running",
            ));
        }
        self.options = options;
        self.control = Arc::new(ClientControl::default());
        Ok(())
    }

    pub fn cancellation(&self) -> McpClientCancellation {
        McpClientCancellation {
            control: Arc::clone(&self.control),
        }
    }

    pub async fn discover(&mut self) -> Result<(), McpError> {
        if self.control.cancelled.load(Ordering::Acquire) {
            return Err(McpError::new(
                McpFailure::Cancelled,
                "MCP client was cancelled",
            ));
        }
        let state = std::mem::replace(&mut self.state, ClientState::Empty);
        let lifecycle = ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        };
        let client_info = ClientInfo::new(
            current_client_capabilities(),
            Implementation::new("ego", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28);
        match state {
            ClientState::Pending(PendingTransport::Http {
                config,
                profile,
                effects,
            }) => {
                let backend = EgoStreamableHttpClient {
                    server: self.server.clone(),
                    url: config.url.as_url().clone(),
                    profile,
                    effects,
                    options: self.options,
                    control: Arc::clone(&self.control),
                };
                let mut transport_config =
                    StreamableHttpClientTransportConfig::with_uri(config.url.as_url().as_str())
                        .max_sse_event_size(self.options.max_sse_event_size)
                        .reinit_on_expired_session(false);
                transport_config.allow_stateless = true;
                transport_config.retry_config = Arc::new(NeverRetry::default());
                let transport =
                    StreamableHttpClientTransport::with_client(backend, transport_config);
                let mut drop_guard = DiscoverDropGuard::new(Arc::clone(&self.control));
                let outcome = serve_client_with_lifecycle(client_info, transport, lifecycle).await;
                drop_guard.disarm();
                match outcome {
                    Ok(service) => {
                        self.state = ClientState::Running {
                            service,
                            duplex: None,
                        };
                        Ok(())
                    }
                    Err(error) => {
                        self.state = ClientState::Failed { duplex: None };
                        Err(McpError::new(McpFailure::Protocol, error.to_string()))
                    }
                }
            }
            ClientState::Pending(PendingTransport::Stdio { effects }) => {
                let duplex = effects.open(self.server.clone()).await?;
                let shared = SharedDuplex::new(duplex);
                let transport = shared.clone().compat();
                match serve_client_with_lifecycle(client_info, transport, lifecycle).await {
                    Ok(service) => {
                        self.state = ClientState::Running {
                            service,
                            duplex: Some(shared),
                        };
                        Ok(())
                    }
                    Err(error) => {
                        self.state = ClientState::Failed {
                            duplex: Some(shared),
                        };
                        Err(McpError::new(McpFailure::Protocol, error.to_string()))
                    }
                }
            }
            ClientState::Pending(PendingTransport::Acp { stream }) => {
                let (read, write) = tokio::io::split(stream);
                let transport =
                    rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new_client(
                        read, write,
                    );
                match serve_client_with_lifecycle(client_info, transport, lifecycle).await {
                    Ok(service) => {
                        self.state = ClientState::Running {
                            service,
                            duplex: None,
                        };
                        Ok(())
                    }
                    Err(error) => {
                        self.state = ClientState::Failed { duplex: None };
                        Err(McpError::new(McpFailure::Protocol, error.to_string()))
                    }
                }
            }
            other => {
                self.state = other;
                Err(McpError::new(
                    McpFailure::Protocol,
                    "MCP discovery may run exactly once",
                ))
            }
        }
    }

    pub async fn list_tools(&mut self) -> Result<Vec<ToolDescriptor>, McpError> {
        if self.control.cancelled.load(Ordering::Acquire) {
            return Err(McpError::new(
                McpFailure::Cancelled,
                "MCP client was cancelled",
            ));
        }
        if !matches!(&self.state, ClientState::Running { .. }) {
            return Err(McpError::new(
                McpFailure::Protocol,
                "MCP client has not discovered its server",
            ));
        }
        self.claim_operation()?;
        let options = self.options;
        let control = Arc::clone(&self.control);
        let service = self.running_service_mut()?;
        let tools = Self::list_current_tools(service, options, control).await?;
        Ok(project_tools(tools.items))
    }

    pub(crate) async fn inventory(&mut self) -> Result<McpClientInventory, McpError> {
        if self.control.cancelled.load(Ordering::Acquire) {
            return Err(McpError::new(
                McpFailure::Cancelled,
                "MCP client was cancelled",
            ));
        }
        if !matches!(&self.state, ClientState::Running { .. }) {
            return Err(McpError::new(
                McpFailure::Protocol,
                "MCP client has not discovered its server",
            ));
        }
        self.claim_operation()?;
        let options = self.options;
        let control = Arc::clone(&self.control);
        let service = self.running_service_mut()?;
        let peer = service.peer_info().ok_or_else(|| {
            McpError::new(
                McpFailure::Protocol,
                "MCP discovery returned no server capabilities",
            )
        })?;
        let capabilities = peer.capabilities.clone();
        // A server that describes what it is for and how its tools should be
        // preferred says so here and nowhere else. Dropping it left the tool
        // descriptions as the only thing a model ever learned about a foreign
        // surface.
        let instructions = peer.instructions.clone();
        let tools = if capabilities.tools.is_some() {
            Self::list_current_tools(service, options, Arc::clone(&control)).await?
        } else {
            CurrentList::empty()
        };
        let resources = if capabilities.resources.is_some() {
            Self::list_current_resources(service, options, Arc::clone(&control)).await?
        } else {
            CurrentList::empty()
        };
        let prompts = if capabilities.prompts.is_some() {
            Self::list_current_prompts(service, options, control).await?
        } else {
            CurrentList::empty()
        };
        let minimum_ttl_ms = [
            tools.minimum_ttl_ms,
            resources.minimum_ttl_ms,
            prompts.minimum_ttl_ms,
        ]
        .into_iter()
        .flatten()
        .min();
        let expires_at = minimum_ttl_ms
            .map(std::time::Duration::from_millis)
            .map(|ttl| {
                Instant::now().checked_add(ttl).ok_or_else(|| {
                    McpError::new(McpFailure::InvalidValue, "MCP cache expiry overflowed")
                })
            })
            .transpose()?;
        Ok(McpClientInventory {
            capabilities,
            instructions,
            tools: project_tools(tools.items),
            resources: resources.items,
            prompts: prompts.items,
            expires_at,
        })
    }

    /// Perform one manual tools/call round. Input-required is returned to the
    /// durable continuation owner and is never fulfilled automatically.
    pub async fn call_tool_once(
        &mut self,
        params: CallToolRequestParams,
    ) -> Result<McpManualOutcome<CallToolResult>, McpError> {
        self.claim_operation()?;
        let service = self.running_service_mut()?;
        let response = service
            .call_tool_once(params)
            .await
            .map_err(|error| McpError::new(McpFailure::Protocol, error.to_string()))?;
        McpManualOutcome::from_call(response)
    }

    /// Perform one manual resources/read round without entering rmcp's MRTR
    /// loop.
    pub async fn read_resource_once(
        &mut self,
        params: ReadResourceRequestParams,
    ) -> Result<McpManualOutcome<ReadResourceResult>, McpError> {
        self.claim_operation()?;
        let service = self.running_service_mut()?;
        let response = service
            .read_resource_once(params)
            .await
            .map_err(|error| McpError::new(McpFailure::Protocol, error.to_string()))?;
        McpManualOutcome::from_resource(response)
    }

    /// Perform one manual prompts/get round without entering rmcp's MRTR loop.
    pub async fn get_prompt_once(
        &mut self,
        params: GetPromptRequestParams,
    ) -> Result<McpManualOutcome<GetPromptResult>, McpError> {
        self.claim_operation()?;
        let service = self.running_service_mut()?;
        let response = service
            .get_prompt_once(params)
            .await
            .map_err(|error| McpError::new(McpFailure::Protocol, error.to_string()))?;
        McpManualOutcome::from_prompt(response)
    }

    fn running_service_mut(
        &mut self,
    ) -> Result<&mut RunningService<RoleClient, ClientInfo>, McpError> {
        if self.control.cancelled.load(Ordering::Acquire) {
            return Err(McpError::new(
                McpFailure::Cancelled,
                "MCP client was cancelled",
            ));
        }
        let ClientState::Running { service, .. } = &mut self.state else {
            return Err(McpError::new(
                McpFailure::Protocol,
                "MCP client has not discovered its server",
            ));
        };
        Ok(service)
    }

    fn claim_operation(&self) -> Result<(), McpError> {
        self.control
            .operation_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                McpError::new(
                    McpFailure::Authority,
                    "MCP client operation authority was already spent",
                )
            })?;
        Ok(())
    }

    async fn list_current_tools(
        service: &mut RunningService<RoleClient, ClientInfo>,
        options: McpClientOptions,
        control: Arc<ClientControl>,
    ) -> Result<CurrentList<rmcp::model::Tool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut minimum_ttl_ms = None;
        let mut cache_scope = None;
        loop {
            let request = ClientRequest::ListToolsRequest(ListToolsRequest {
                method: Default::default(),
                params: Some(PaginatedRequestParams::default().with_cursor(cursor)),
                extensions: Default::default(),
            });
            let response =
                Self::send_current_request(service, request, options, Arc::clone(&control)).await?;
            let ServerResult::ListToolsResult(result) = response else {
                return Err(McpError::new(
                    McpFailure::Protocol,
                    "MCP server returned an unexpected tools/list response",
                ));
            };
            observe_current_list_page(
                result.result_type.as_ref(),
                result.ttl_ms,
                result.cache_scope,
                &mut minimum_ttl_ms,
                &mut cache_scope,
                "tools/list",
            )?;
            tools.extend(result.tools);
            cursor = result.next_cursor;
            if cursor.is_none() {
                return Ok(CurrentList {
                    items: tools,
                    minimum_ttl_ms,
                });
            }
        }
    }

    async fn list_current_resources(
        service: &mut RunningService<RoleClient, ClientInfo>,
        options: McpClientOptions,
        control: Arc<ClientControl>,
    ) -> Result<CurrentList<Resource>, McpError> {
        let mut resources = Vec::new();
        let mut cursor = None;
        let mut minimum_ttl_ms = None;
        let mut cache_scope = None;
        loop {
            let request = ClientRequest::ListResourcesRequest(ListResourcesRequest {
                method: Default::default(),
                params: Some(PaginatedRequestParams::default().with_cursor(cursor)),
                extensions: Default::default(),
            });
            let response =
                Self::send_current_request(service, request, options, Arc::clone(&control)).await?;
            let ServerResult::ListResourcesResult(result) = response else {
                return Err(McpError::new(
                    McpFailure::Protocol,
                    "MCP server returned an unexpected resources/list response",
                ));
            };
            observe_current_list_page(
                result.result_type.as_ref(),
                result.ttl_ms,
                result.cache_scope,
                &mut minimum_ttl_ms,
                &mut cache_scope,
                "resources/list",
            )?;
            resources.extend(result.resources);
            cursor = result.next_cursor;
            if cursor.is_none() {
                return Ok(CurrentList {
                    items: resources,
                    minimum_ttl_ms,
                });
            }
        }
    }

    async fn list_current_prompts(
        service: &mut RunningService<RoleClient, ClientInfo>,
        options: McpClientOptions,
        control: Arc<ClientControl>,
    ) -> Result<CurrentList<Prompt>, McpError> {
        let mut prompts = Vec::new();
        let mut cursor = None;
        let mut minimum_ttl_ms = None;
        let mut cache_scope = None;
        loop {
            let request = ClientRequest::ListPromptsRequest(ListPromptsRequest {
                method: Default::default(),
                params: Some(PaginatedRequestParams::default().with_cursor(cursor)),
                extensions: Default::default(),
            });
            let response =
                Self::send_current_request(service, request, options, Arc::clone(&control)).await?;
            let ServerResult::ListPromptsResult(result) = response else {
                return Err(McpError::new(
                    McpFailure::Protocol,
                    "MCP server returned an unexpected prompts/list response",
                ));
            };
            observe_current_list_page(
                result.result_type.as_ref(),
                result.ttl_ms,
                result.cache_scope,
                &mut minimum_ttl_ms,
                &mut cache_scope,
                "prompts/list",
            )?;
            prompts.extend(result.prompts);
            cursor = result.next_cursor;
            if cursor.is_none() {
                return Ok(CurrentList {
                    items: prompts,
                    minimum_ttl_ms,
                });
            }
        }
    }

    async fn send_current_request(
        service: &mut RunningService<RoleClient, ClientInfo>,
        request: ClientRequest,
        options: McpClientOptions,
        control: Arc<ClientControl>,
    ) -> Result<ServerResult, McpError> {
        let handle = service
            .send_cancellable_request(
                request,
                PeerRequestOptions::with_timeout(
                    options.deadline.saturating_duration_since(Instant::now()),
                ),
            )
            .await
            .map_err(|error| McpError::new(McpFailure::Protocol, error.to_string()))?;
        let request_id = handle.id.clone();
        tokio::select! {
            response = handle.await_response() => response.map_err(|error| {
                McpError::new(McpFailure::Protocol, error.to_string())
            }),
            () = wait_cancelled(control) => {
                let _ = service.notify_cancelled(CancelledNotificationParam::new(
                    Some(request_id),
                    Some("MCP client operation cancelled".to_owned()),
                )).await;
                Err(McpError::new(McpFailure::Cancelled, "MCP client was cancelled"))
            }
        }
    }

    pub async fn settle(mut self) -> Result<McpClientSettlement, McpError> {
        let state = std::mem::replace(&mut self.state, ClientState::Empty);
        let mut duplex_settlement = None;
        match state {
            ClientState::Running {
                mut service,
                duplex,
                ..
            } => {
                let _ = service.close().await;
                if let Some(duplex) = duplex {
                    duplex_settlement = Some(if self.control.cancelled.load(Ordering::Acquire) {
                        duplex.interrupt().await?
                    } else {
                        duplex.settle().await?
                    });
                }
            }
            ClientState::Failed { duplex } => {
                if let Some(duplex) = duplex {
                    duplex_settlement = Some(if self.control.cancelled.load(Ordering::Acquire) {
                        duplex.interrupt().await?
                    } else {
                        duplex.settle().await?
                    });
                }
            }
            ClientState::Pending(_) => {
                return Err(McpError::new(
                    McpFailure::Protocol,
                    "MCP client was never discovered",
                ));
            }
            ClientState::Empty => {
                return Err(McpError::new(
                    McpFailure::Authority,
                    "MCP client was settled",
                ));
            }
        }
        if let Some(acp) = self.acp.take() {
            acp.bridge.abort();
            acp.effects.disconnect(acp.id).await?;
        }
        Ok(McpClientSettlement {
            http: self.control.settlements(),
            duplex: duplex_settlement,
        })
    }
}

async fn acp_bridge(
    stream: tokio::io::DuplexStream,
    connection_id: String,
    effects: Arc<dyn McpAcpEffects>,
) {
    let (read, write) = tokio::io::split(stream);
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let active = Arc::new(tokio::sync::Mutex::<
        HashMap<String, tokio::task::AbortHandle>,
    >::new(HashMap::new()));
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
            break;
        };
        let Some(method) = frame["method"].as_str() else {
            continue;
        };
        let params = frame["params"].as_object().cloned();
        let Some(id) = frame.get("id").cloned() else {
            if method == "notifications/cancelled" {
                if let Some(request_id) = params.as_ref().and_then(|params| params.get("requestId"))
                {
                    if let Some(request) = active.lock().await.remove(&request_id.to_string()) {
                        request.abort();
                    }
                }
                continue;
            }
            let _ = effects.notify(connection_id.clone(), method.to_owned(), params);
            continue;
        };
        let effects = Arc::clone(&effects);
        let connection_id = connection_id.clone();
        let method = method.to_owned();
        let write = Arc::clone(&write);
        let key = id.to_string();
        let mut active_requests = active.lock().await;
        let active_for_request = Arc::clone(&active);
        let request_key = key.clone();
        let request = tokio::spawn(async move {
            let frame = match effects.request(connection_id, method, params).await {
                Ok(result) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                Err(error) => serde_json::json!({
                    "jsonrpc":"2.0","id":id,
                    "error":{"code":-32603,"message":error.to_string()}
                }),
            };
            let mut write = write.lock().await;
            let _ = write.write_all(frame.to_string().as_bytes()).await;
            let _ = write.write_all(b"\n").await;
            active_for_request.lock().await.remove(&request_key);
        });
        active_requests.insert(key, request.abort_handle());
    }
}

pub struct McpClientSettlement {
    http: Vec<McpHttpSettlement>,
    duplex: Option<DuplexSettlement>,
}

impl<T> CurrentList<T> {
    fn empty() -> Self {
        Self {
            items: Vec::new(),
            minimum_ttl_ms: None,
        }
    }
}

fn observe_current_list_page(
    result_type: Option<&rmcp::model::ResultType>,
    ttl_ms: Option<u64>,
    scope: Option<rmcp::model::CacheScope>,
    minimum_ttl_ms: &mut Option<u64>,
    cache_scope: &mut Option<rmcp::model::CacheScope>,
    operation: &str,
) -> Result<(), McpError> {
    if !result_type.is_some_and(rmcp::model::ResultType::is_complete) {
        return Err(McpError::new(
            McpFailure::Protocol,
            format!("current {operation} result has no complete resultType"),
        ));
    }
    let ttl_ms = ttl_ms.ok_or_else(|| {
        McpError::new(
            McpFailure::Protocol,
            format!("current {operation} result has no ttlMs"),
        )
    })?;
    let scope = scope.ok_or_else(|| {
        McpError::new(
            McpFailure::Protocol,
            format!("current {operation} result has no cacheScope"),
        )
    })?;
    if cache_scope.as_ref().is_some_and(|known| *known != scope) {
        return Err(McpError::new(
            McpFailure::Protocol,
            format!("current {operation} pagination changed cacheScope"),
        ));
    }
    *cache_scope = Some(scope);
    *minimum_ttl_ms = Some((*minimum_ttl_ms).map_or(ttl_ms, |known| known.min(ttl_ms)));
    Ok(())
}

fn project_tools(tools: Vec<rmcp::model::Tool>) -> Vec<ToolDescriptor> {
    tools
        .into_iter()
        .map(|tool| ToolDescriptor {
            name: tool.name.into_owned(),
            description: tool
                .description
                .map_or_else(String::new, |value| value.into_owned()),
            parameters: serde_json::Value::Object((*tool.input_schema).clone()),
        })
        .collect()
}

async fn wait_cancelled(control: Arc<ClientControl>) {
    while !control.cancelled.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

impl McpClientSettlement {
    pub fn http(&self) -> &[McpHttpSettlement] {
        &self.http
    }

    pub fn request(&self, request_id: &McpRequestId) -> Option<&McpHttpSettlement> {
        self.http
            .iter()
            .find(|settlement| settlement.request_id() == request_id)
    }

    pub fn duplex(&self) -> Option<&DuplexSettlement> {
        self.duplex.as_ref()
    }
}
