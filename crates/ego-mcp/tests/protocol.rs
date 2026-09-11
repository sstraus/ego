//! contract for the current MCP 2026-07-28 client and guarded transports.
//!
//! The effects below are deterministic protocol-neutral fakes. They record
//! the exact request handed to the guarded boundary; they do not implement
//! MCP framing, OAuth, or a second transport. A production implementation
//! must make these observations through the public `ego-mcp` spine while
//! keeping request identity and authority private to that spine.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ego_core::config::{McpHttpAuth, McpHttpConfig, McpHttpUrl, McpServerName};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{
    CleanupCertainty, DuplexEnd, DuplexSession, DuplexSettlement, HttpHeader, TransportError,
};
use ego_mcp::{
    MCP_PROTOCOL_REVISION, McpClient, McpClientOptions, McpDuplexEffects, McpError, McpFuture,
    McpHttpBody, McpHttpEffects, McpHttpEnd, McpHttpPurpose, McpHttpResponse, McpHttpSettlement,
    McpRequestId, McpServerPort, McpServerToolCall, PreparedMcpHttpCall, serve_stdio,
};
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite, Cursor};
use rmcp::model::CallToolRequestParams;

#[derive(Clone, Debug)]
struct SeenHttp {
    request_id_value: McpRequestId,
    request_id: String,
    purpose: McpHttpPurpose,
    delivery: DeliveryCertainty,
    method: String,
    path: String,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

struct FakeHttp {
    seen: Mutex<Vec<SeenHttp>>,
    replies: Mutex<VecDeque<McpHttpResponse>>,
}

impl FakeHttp {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            replies: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, response: McpHttpResponse) {
        self.replies
            .lock()
            .expect("reply queue")
            .push_back(response);
    }

    fn seen(&self) -> Vec<SeenHttp> {
        self.seen.lock().expect("request log").clone()
    }
}

impl McpHttpEffects for FakeHttp {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        let reply = self.replies.lock().expect("reply queue").pop_front();
        let delivery = reply.as_ref().map_or(
            DeliveryCertainty::DefinitelyUnsent,
            McpHttpResponse::delivery,
        );
        self.seen.lock().expect("request log").push(SeenHttp {
            request_id_value: call.request_id().clone(),
            request_id: format!("{:?}", call.request_id()),
            purpose: call.purpose(),
            delivery,
            method: call.request().method().to_owned(),
            path: call.request().path().to_owned(),
            headers: call
                .request()
                .headers()
                .iter()
                .map(|header| (header.name().to_owned(), header.value().to_vec()))
                .collect(),
            body: call.request().body().to_vec(),
        });
        Box::pin(async move { Ok(reply.expect("fake response was queued")) })
    }

    fn cancel(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        Box::pin(async move {
            McpHttpSettlement::new(
                request_id,
                McpHttpEnd::Cancelled,
                DeliveryCertainty::DefinitelyUnsent,
            )
        })
    }

    fn timeout(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        Box::pin(async move {
            McpHttpSettlement::new(
                request_id,
                McpHttpEnd::TimedOut,
                DeliveryCertainty::DefinitelyUnsent,
            )
        })
    }

    fn settle(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        Box::pin(async move {
            McpHttpSettlement::new(
                request_id,
                McpHttpEnd::Completed,
                DeliveryCertainty::PossiblySent,
            )
        })
    }
}

struct FakeDuplex {
    opened: Mutex<Vec<String>>,
}

impl McpDuplexEffects for FakeDuplex {
    fn open(
        self: Box<Self>,
        server: McpServerName,
    ) -> McpFuture<'static, Result<ego_core::permit::DuplexHandle, McpError>> {
        self.opened
            .lock()
            .expect("duplex log")
            .push(server.to_string());
        Box::pin(async move {
            Ok(Box::new(ScriptedDuplex::for_discover()) as ego_core::permit::DuplexHandle)
        })
    }
}

struct MemoryDuplex {
    inner: Cursor<Vec<u8>>,
    output: Arc<Mutex<Vec<u8>>>,
    evidence: Arc<Mutex<Vec<&'static str>>>,
}

type DuplexEvidence = Arc<Mutex<Vec<&'static str>>>;
type DuplexOutput = Arc<Mutex<Vec<u8>>>;

impl MemoryDuplex {
    fn with_evidence() -> (Self, DuplexEvidence) {
        let (duplex, evidence, _) = Self::with_input(Vec::new());
        (duplex, evidence)
    }

    fn with_input(input: Vec<u8>) -> (Self, DuplexEvidence, DuplexOutput) {
        let evidence = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner: Cursor::new(input),
                output: Arc::clone(&output),
                evidence: Arc::clone(&evidence),
            },
            evidence,
            output,
        )
    }
}

impl AsyncRead for MemoryDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, bytes)
    }
}

impl AsyncWrite for MemoryDuplex {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.output
            .lock()
            .expect("duplex output")
            .extend_from_slice(bytes);
        std::task::Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_close(cx)
    }
}

impl DuplexSession for MemoryDuplex {
    fn delivery(&self) -> DeliveryCertainty {
        DeliveryCertainty::DefinitelyUnsent
    }

    fn settle(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        self.evidence
            .lock()
            .expect("duplex evidence")
            .push("settle");
        Box::pin(async {
            DuplexSettlement::new(
                DuplexEnd::Exited { code: 0 },
                CleanupCertainty::Complete,
                DeliveryCertainty::DefinitelyUnsent,
                Ok(ego_core::permit::ToolResult::builtin("settled")),
            )
        })
    }

    fn interrupt(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        self.evidence
            .lock()
            .expect("duplex evidence")
            .push("interrupt");
        Box::pin(async {
            DuplexSettlement::new(
                DuplexEnd::Interrupted,
                CleanupCertainty::Complete,
                DeliveryCertainty::DefinitelyUnsent,
                Ok(ego_core::permit::ToolResult::builtin("interrupted")),
            )
        })
    }
}

struct ScriptedDuplex {
    input: Cursor<Vec<u8>>,
    output: Cursor<Vec<u8>>,
}

impl ScriptedDuplex {
    fn for_discover() -> Self {
        Self {
            input: Cursor::new(
                b"{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"resultType\":\"complete\",\"supportedVersions\":[\"2026-07-28\"],\"capabilities\":{\"tools\":{\"listChanged\":false}},\"ttlMs\":0,\"cacheScope\":\"private\",\"_meta\":{\"io.modelcontextprotocol/serverInfo\":{\"name\":\"local\",\"version\":\"1.0\"}}}}\n".to_vec(),
            ),
            output: Cursor::new(Vec::new()),
        }
    }
}

impl AsyncRead for ScriptedDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.input).poll_read(cx, bytes)
    }
}

impl AsyncWrite for ScriptedDuplex {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.output).poll_write(cx, bytes)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.output).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.output).poll_close(cx)
    }
}

impl DuplexSession for ScriptedDuplex {
    fn delivery(&self) -> DeliveryCertainty {
        DeliveryCertainty::DefinitelyUnsent
    }

    fn settle(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        Box::pin(async {
            DuplexSettlement::new(
                DuplexEnd::Exited { code: 0 },
                CleanupCertainty::Complete,
                DeliveryCertainty::DefinitelyUnsent,
                Ok(ego_core::permit::ToolResult::builtin("settled")),
            )
        })
    }

    fn interrupt(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        Box::pin(async {
            DuplexSettlement::new(
                DuplexEnd::Interrupted,
                CleanupCertainty::Complete,
                DeliveryCertainty::DefinitelyUnsent,
                Ok(ego_core::permit::ToolResult::builtin("interrupted")),
            )
        })
    }
}

struct ServerPort;

fn invalid_server_error() -> McpError {
    match McpServerToolCall::new(String::new(), serde_json::Map::new(), None) {
        Ok(_) => panic!("invalid fixture call unexpectedly succeeded"),
        Err(error) => error,
    }
}

impl McpServerPort for ServerPort {
    fn list_tools<'a>(&'a self) -> McpFuture<'a, Vec<ego_core::generator::types::ToolDescriptor>> {
        Box::pin(async { Vec::new() })
    }

    fn call_tool<'a>(
        &'a self,
        _call: McpServerToolCall,
    ) -> McpFuture<'a, Result<ego_core::permit::ToolResult, McpError>> {
        Box::pin(async { Ok(ego_core::permit::ToolResult::builtin("server result")) })
    }

    fn list_resources<'a>(&'a self) -> McpFuture<'a, Vec<ego_mcp::McpServerResourceDescriptor>> {
        Box::pin(async { Vec::new() })
    }

    fn read_resource<'a>(
        &'a self,
        _uri: &'a str,
    ) -> McpFuture<'a, Result<ego_mcp::McpServerResourceContents, McpError>> {
        Box::pin(async { Err(invalid_server_error()) })
    }
}

fn options() -> McpClientOptions {
    McpClientOptions::new(64 * 1024, Instant::now() + Duration::from_secs(30))
        .expect("valid client options")
}

fn http_config() -> McpHttpConfig {
    McpHttpConfig::new(
        McpHttpUrl::from(
            "https://mcp.example.test/mcp"
                .parse::<url::Url>()
                .expect("MCP URL"),
        ),
        McpHttpAuth::None,
        false,
    )
    .expect("HTTP config")
}

async fn wait_for_flag(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("worker flag");
}

impl FakeDuplex {
    fn new() -> Self {
        Self {
            opened: Mutex::new(Vec::new()),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn selects_only_current_discover_and_required_request_metadata() {
    let http = Arc::new(FakeHttp::new());
    http.push(McpHttpResponse::json(
        200,
        br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false},"prompts":{"listChanged":false}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"remote","version":"1.0.0"}}}}"#,
    ));
    let mut client = McpClient::connect_http(
        "remote".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");

    client.discover().await.expect("discover succeeds");
    let seen = http.seen();
    assert_eq!(MCP_PROTOCOL_REVISION, "2026-07-28");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].purpose, McpHttpPurpose::Protocol);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/mcp");
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(name, value)| name == "mcp-method" && value == b"server/discover")
    );
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(name, value)| name == "mcp-protocol-version" && value == b"2026-07-28")
    );
    assert!(!seen[0].headers.iter().any(|(name, _)| name == "mcp-name"));
    let body: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("JSON body");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["method"], "server/discover");
    assert!(
        body["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["version"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
        "ego"
    );
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"],
        serde_json::json!({
            "elicitation": {
                "form": {"schemaValidation": true}
            }
        }),
        "the client advertises exactly the optional input mode it implements"
    );
    assert!(
        !seen[0]
            .headers
            .iter()
            .any(|(name, _)| name == "mcp-session-id")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn current_client_rejects_legacy_only_discovery_without_handshake_fallback() {
    let http = Arc::new(FakeHttp::new());
    http.push(McpHttpResponse::json(
        200,
        br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2025-11-25","2025-06-18","2025-03-26"],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"legacy-only","version":"1.0.0"}}}}"#,
    ));
    let mut client = McpClient::connect_http(
        "legacy-only".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");

    let error = client
        .discover()
        .await
        .expect_err("legacy-only discovery must fail closed");
    assert!(error.to_string().contains("no compatible protocol version"));
    let seen = http.seen();
    assert_eq!(seen.len(), 1, "version refusal must not retry");
    assert!(
        !seen[0]
            .body
            .windows(b"initialize".len())
            .any(|window| window == b"initialize"),
        "modern refusal must not fall back to initialize"
    );
    assert!(
        !seen[0]
            .headers
            .iter()
            .any(|(name, _)| name == "mcp-session-id"),
        "modern refusal must not establish a session"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn named_tool_call_carries_current_routing_and_request_metadata() {
    let http = Arc::new(FakeHttp::new());
    http.push(McpHttpResponse::json(
        200,
        br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"remote","version":"1.0.0"}}}}"#,
    ));
    http.push(McpHttpResponse::json(
        200,
        br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"ok"}],"isError":false}}"#,
    ));
    let mut client = McpClient::connect_http(
        "remote".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");

    client.discover().await.expect("discover");
    client
        .call_tool_once(CallToolRequestParams::new("search_tools"))
        .await
        .expect("current tool call");

    let seen = http.seen();
    assert_eq!(seen.len(), 2);
    let call = &seen[1];
    assert!(
        call.headers
            .iter()
            .any(|(name, value)| name == "mcp-protocol-version" && value == b"2026-07-28")
    );
    assert!(
        call.headers
            .iter()
            .any(|(name, value)| name == "mcp-method" && value == b"tools/call")
    );
    assert!(
        call.headers
            .iter()
            .any(|(name, value)| name == "mcp-name" && value == b"search_tools")
    );
    assert!(
        !call
            .headers
            .iter()
            .any(|(name, _)| name == "mcp-session-id")
    );
    let body: serde_json::Value = serde_json::from_slice(&call.body).expect("tool request body");
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], "search_tools");
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
        "ego"
    );
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"],
        serde_json::json!({
            "elicitation": {
                "form": {"schemaValidation": true}
            }
        }),
        "every modern request repeats the exact request-scoped capabilities"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn raw_http_json_does_not_repair_an_incomplete_mcp_envelope() {
    let http = Arc::new(FakeHttp::new());
    http.push(McpHttpResponse::json(
        200,
        br#"{"result":{"capabilities":{}}}"#,
    ));
    let mut client = McpClient::connect_http(
        "remote".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");

    assert!(client.discover().await.is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn each_post_consumes_fresh_call_and_never_retries_or_reinitializes() {
    let http = Arc::new(FakeHttp::new());
    http.push(McpHttpResponse::json(
        500,
        br#"{"jsonrpc":"2.0","id":0,"error":{"code":-32000,"message":"first"}}"#,
    ));
    let mut client = McpClient::connect_http(
        "remote".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");
    let result = client.discover().await;
    assert!(result.is_err());
    let seen = http.seen();
    assert_eq!(seen.len(), 1, "one failed POST must not retry");
    assert!(
        seen.iter()
            .all(|request| request.purpose == McpHttpPurpose::Protocol)
    );
    assert!(!seen.iter().any(|request| {
        request
            .body
            .windows(b"initialize".len())
            .any(|window| window == b"initialize")
            || request
                .headers
                .iter()
                .any(|(name, _)| name == "mcp-session-id")
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn every_logical_post_receives_distinct_request_owned_authority() {
    let http = Arc::new(FakeHttp::new());
    http.push(McpHttpResponse::json(
        200,
        br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false},"prompts":{"listChanged":false}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"remote","version":"1.0.0"}}}}"#,
    ));
    http.push(McpHttpResponse::json(
        200,
        br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","tools":[],"nextCursor":null,"ttlMs":0,"cacheScope":"private"}}"#,
    ));
    let mut client = McpClient::connect_http(
        "remote".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");
    client.discover().await.expect("discover");
    client.list_tools().await.expect("tools/list");
    let seen = http.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].request_id, seen[1].request_id);
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_pending_http_effect_closes_request_before_headers_without_sdk_cancel_path() {
    struct DropAwareHttp {
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }
    impl McpHttpEffects for DropAwareHttp {
        fn execute(
            &self,
            _call: PreparedMcpHttpCall,
        ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
            self.started.store(true, Ordering::SeqCst);
            let dropped = Arc::clone(&self.dropped);
            Box::pin(async move {
                struct Guard(Arc<AtomicBool>);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                let _guard = Guard(dropped);
                futures::future::pending().await
            })
        }

        fn cancel(
            &self,
            request_id: McpRequestId,
        ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
            Box::pin(async move {
                McpHttpSettlement::new(
                    request_id,
                    McpHttpEnd::Cancelled,
                    DeliveryCertainty::DefinitelyUnsent,
                )
            })
        }

        fn timeout(
            &self,
            request_id: McpRequestId,
        ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
            Box::pin(async move {
                McpHttpSettlement::new(
                    request_id,
                    McpHttpEnd::TimedOut,
                    DeliveryCertainty::DefinitelyUnsent,
                )
            })
        }

        fn settle(
            &self,
            request_id: McpRequestId,
        ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
            Box::pin(async move {
                McpHttpSettlement::new(
                    request_id,
                    McpHttpEnd::Completed,
                    DeliveryCertainty::PossiblySent,
                )
            })
        }
    }

    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let http = Arc::new(DropAwareHttp {
        started: Arc::clone(&started),
        dropped: Arc::clone(&dropped),
    });
    let mut client = McpClient::connect_http(
        "remote".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");
    let mut request = Box::pin(client.discover());
    tokio::select! {
        _ = wait_for_flag(&started) => {}
        outcome = &mut request => panic!("discovery future finished before adapter start: {outcome:?}"),
    }
    drop(request);
    wait_for_flag(&dropped).await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_stream_after_first_sse_event_preserves_possible_delivery_and_closes_body() {
    struct DropProbe(Arc<AtomicBool>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let yielded = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let probe = DropProbe(Arc::clone(&closed));
    let yielded_for_stream = Arc::clone(&yielded);
    let body: McpHttpBody = Box::pin(futures::stream::unfold(
        Some((false, probe)),
        move |state| {
            let yielded = Arc::clone(&yielded_for_stream);
            async move {
                let (sent, probe) = state?;
                if sent {
                    futures::future::pending::<()>().await;
                    None
                } else {
                    yielded.store(true, Ordering::SeqCst);
                    Some((
                        Ok::<Vec<u8>, TransportError>(
                            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"resultType\":\"complete\",\"supportedVersions\":[\"2026-07-28\"],\"capabilities\":{\"tools\":{\"listChanged\":false},\"resources\":{\"subscribe\":false,\"listChanged\":false},\"prompts\":{\"listChanged\":false}},\"ttlMs\":0,\"cacheScope\":\"private\",\"_meta\":{\"io.modelcontextprotocol/serverInfo\":{\"name\":\"remote\",\"version\":\"1.0.0\"}}}}\n\n".to_vec(),
                        ),
                        Some((true, probe)),
                    ))
                }
            }
        },
    ));
    let response = McpHttpResponse::response(
        200,
        vec![HttpHeader::new("content-type", b"text/event-stream".to_vec()).expect("header")],
        body,
        DeliveryCertainty::PossiblySent,
    )
    .expect("stream response");
    let http = Arc::new(FakeHttp::new());
    http.push(response);
    let mut client = McpClient::connect_http(
        "remote".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");
    let mut request = Box::pin(client.discover());
    let waker = futures::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    assert!(matches!(
        std::future::Future::poll(request.as_mut(), &mut context),
        std::task::Poll::Pending
    ));
    wait_for_flag(&yielded).await;
    assert_eq!(http.seen()[0].delivery, DeliveryCertainty::PossiblySent);
    drop(request);
    wait_for_flag(&closed).await;
}

#[tokio::test(flavor = "current_thread")]
async fn stdio_is_a_guarded_duplex_and_server_lifecycle_has_no_raw_child_escape() {
    let duplex = FakeDuplex::new();
    let mut client = McpClient::connect_stdio(
        "local".parse().expect("server name"),
        options(),
        Box::new(duplex),
    )
    .expect("stdio client");
    client.discover().await.expect("stdio discover");
    let settlement = client.settle().await.expect("stdio settlement");
    let duplex = settlement.duplex().expect("duplex settlement");
    assert_eq!(duplex.cleanup(), &CleanupCertainty::Complete);
}

#[test]
fn consuming_a_real_duplex_interrupt_reports_complete_cleanup() {
    let (duplex, evidence) = MemoryDuplex::with_evidence();
    let settlement = futures::executor::block_on(Box::new(duplex).interrupt());
    assert_eq!(settlement.cleanup(), &CleanupCertainty::Complete);
    assert_eq!(
        evidence.lock().expect("duplex evidence").as_slice(),
        &["interrupt"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn server_adapter_owns_rmcp_and_exposes_only_scoped_port_values() {
    let port: Arc<dyn McpServerPort> = Arc::new(ServerPort);
    let (duplex, evidence) = MemoryDuplex::with_evidence();
    let duplex: ego_core::permit::DuplexHandle = Box::new(duplex);
    let settlement = serve_stdio(port, duplex)
        .await
        .expect("server EOF has a truthful settlement");
    assert_eq!(settlement.cleanup(), &CleanupCertainty::Complete);
    assert_eq!(
        evidence.lock().expect("duplex evidence").as_slice(),
        &["settle"]
    );
}

async fn initialize_server(version: &str) -> (serde_json::Value, DuplexEvidence) {
    let mut request = format!(
        r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{{}},"clientInfo":{{"name":"same-client-for-every-version","version":"1.0"}}}}}}"#,
    )
    .into_bytes();
    request.push(b'\n');
    let (duplex, evidence, output) = MemoryDuplex::with_input(request);
    let port: Arc<dyn McpServerPort> = Arc::new(ServerPort);
    let duplex: ego_core::permit::DuplexHandle = Box::new(duplex);

    serve_stdio(port, duplex)
        .await
        .expect("server EOF has a truthful settlement");

    let response = serde_json::from_slice(&output.lock().expect("duplex output"))
        .expect("JSON initialize response");
    (response, evidence)
}

#[tokio::test(flavor = "current_thread")]
async fn server_echoes_each_supported_handshake_revision_independent_of_client_identity() {
    for version in ["2025-03-26", "2025-06-18", "2025-11-25"] {
        let (response, evidence) = initialize_server(version).await;
        assert_eq!(
            response["result"]["protocolVersion"], version,
            "the requested supported wire revision must be negotiated exactly"
        );
        assert!(response["result"]["capabilities"]["tools"].is_object());
        assert!(response["result"]["capabilities"]["resources"].is_object());
        for unsupported in ["sampling", "roots", "elicitation", "tasks"] {
            assert!(
                response["result"]["capabilities"]
                    .get(unsupported)
                    .is_none(),
                "optional capability {unsupported} was not admitted"
            );
        }
        assert_eq!(
            evidence.lock().expect("duplex evidence").as_slice(),
            &["settle"]
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn server_counteroffers_latest_handshake_revision_for_modern_initialize() {
    let (response, evidence) = initialize_server("2026-07-28").await;
    assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(
        evidence.lock().expect("duplex evidence").as_slice(),
        &["settle"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn raw_sse_bound_is_enforced_across_split_chunks_before_parsing() {
    let body: McpHttpBody = Box::pin(futures::stream::iter(vec![
        Ok::<Vec<u8>, TransportError>(b"event: message\ndata: {\"jsonrpc\"".to_vec()),
        Ok::<Vec<u8>, TransportError>(b":\"2.0\",\"id\":0}\n\n".to_vec()),
    ]));
    let response = McpHttpResponse::response(
        200,
        vec![HttpHeader::new("content-type", b"text/event-stream".to_vec()).expect("header")],
        body,
        DeliveryCertainty::PossiblySent,
    )
    .expect("response");
    let http = Arc::new(FakeHttp::new());
    http.push(response);
    let mut client = McpClient::connect_http(
        "bounded".parse().expect("server name"),
        http_config(),
        McpClientOptions::new(8, Instant::now() + Duration::from_secs(30)).expect("options"),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("client");
    assert!(client.discover().await.is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_settlement_retain_the_exact_request_owned_identity() {
    let http = Arc::new(FakeHttp::new());
    http.push(McpHttpResponse::json(
        200,
        br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"private"}}"#,
    ));
    let mut client = McpClient::connect_http(
        "settlement".parse().expect("server name"),
        http_config(),
        options(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("client");
    let cancellation = client.cancellation();
    assert!(!cancellation.is_cancelled());
    client.discover().await.expect("discover");
    let request_id = http
        .seen()
        .first()
        .expect("request")
        .request_id_value
        .clone();
    cancellation.cancel();
    assert!(cancellation.is_cancelled());
    let settlement = http.cancel(request_id.clone()).await.expect("cancel");
    assert_eq!(settlement.request_id(), &request_id);
    assert_eq!(settlement.end(), McpHttpEnd::Cancelled);
    assert_eq!(settlement.delivery(), DeliveryCertainty::DefinitelyUnsent);
    let client_settlement = client.settle().await.expect("client settlement");
    assert!(!client_settlement.http().is_empty());
    assert!(client_settlement.request(&request_id).is_some());
}
