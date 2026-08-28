//! RED runtime contract for guarded Streamable HTTP cancellation.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ego_core::config::{McpHttpAuth, McpHttpConfig, McpHttpUrl};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{HttpHeader, HttpRequest, TransportError, TransportFailure};
use ego_mcp::{
    MCP_PROTOCOL_REVISION, McpClient, McpClientOptions, McpError, McpFuture, McpHttpEffects,
    McpHttpEnd, McpHttpResponse, McpHttpSettlement, McpRequestId, PreparedMcpHttpCall,
};

struct LocalPeer {
    listener: TcpListener,
    accepted: Arc<AtomicBool>,
    headers_sent: Arc<AtomicBool>,
    first_event_sent: Arc<AtomicBool>,
    first_body_poll: Arc<AtomicBool>,
    peer_dropped: Arc<AtomicBool>,
    body_dropped: Arc<AtomicBool>,
    active_tasks: Arc<AtomicUsize>,
}

impl LocalPeer {
    fn bind() -> Arc<Self> {
        Arc::new(Self {
            listener: TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener"),
            accepted: Arc::new(AtomicBool::new(false)),
            headers_sent: Arc::new(AtomicBool::new(false)),
            first_event_sent: Arc::new(AtomicBool::new(false)),
            first_body_poll: Arc::new(AtomicBool::new(false)),
            peer_dropped: Arc::new(AtomicBool::new(false)),
            body_dropped: Arc::new(AtomicBool::new(false)),
            active_tasks: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn url(&self) -> url::Url {
        format!(
            "http://127.0.0.1:{}/mcp",
            self.listener.local_addr().expect("peer address").port()
        )
        .parse()
        .expect("peer URL")
    }

    fn config(&self) -> McpHttpConfig {
        McpHttpConfig::new(McpHttpUrl::from(self.url()), McpHttpAuth::None, true)
            .expect("loopback HTTP config")
    }

    fn wait_for_accept(&self) {
        while !self.accepted.load(Ordering::Acquire) {
            thread::yield_now();
        }
    }

    fn read_request_headers(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).expect("read peer request");
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                return request;
            }
        }
    }

    fn serve_before_headers(self: Arc<Self>) {
        self.active_tasks.fetch_add(1, Ordering::AcqRel);
        let result = self.listener.accept();
        if let Ok((mut stream, _)) = result {
            self.accepted.store(true, Ordering::Release);
            let _ = Self::read_request_headers(&mut stream);
            let mut request_tail = Vec::new();
            let _ = stream.read_to_end(&mut request_tail);
        }
        self.peer_dropped.store(true, Ordering::Release);
        self.active_tasks.fetch_sub(1, Ordering::AcqRel);
    }

    fn serve_after_one_event(self: Arc<Self>) {
        self.active_tasks.fetch_add(1, Ordering::AcqRel);
        if let Ok((mut stream, _)) = self.listener.accept() {
            self.accepted.store(true, Ordering::Release);
            let _ = Self::read_request_headers(&mut stream);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"resultType\":\"complete\",\"supportedVersions\":[\"2026-07-28\"],\"capabilities\":{},\"ttlMs\":0,\"cacheScope\":\"private\"}}\n\n",
            );
            self.headers_sent.store(true, Ordering::Release);
            self.first_event_sent.store(true, Ordering::Release);
            let mut byte = [0_u8; 1];
            let _ = stream.read(&mut byte);
        }
        self.peer_dropped.store(true, Ordering::Release);
        self.active_tasks.fetch_sub(1, Ordering::AcqRel);
    }

    fn serve_discover_and_list(self: Arc<Self>, session_header_on_list: bool) {
        const DISCOVER: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"resultType\":\"complete\",\"supportedVersions\":[\"2026-07-28\"],\"capabilities\":{\"tools\":{\"listChanged\":false},\"resources\":{\"subscribe\":false,\"listChanged\":false},\"prompts\":{\"listChanged\":false}},\"ttlMs\":0,\"cacheScope\":\"private\",\"_meta\":{\"io.modelcontextprotocol/serverInfo\":{\"name\":\"remote\",\"version\":\"1.0.0\"}}}}\n\n";
        const LIST_TOOLS: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"resultType\":\"complete\",\"tools\":[],\"nextCursor\":null,\"ttlMs\":0,\"cacheScope\":\"private\"}}\n\n";
        self.active_tasks.fetch_add(1, Ordering::AcqRel);
        for (index, response) in [DISCOVER, LIST_TOOLS].into_iter().enumerate() {
            if let Ok((mut stream, _)) = self.listener.accept() {
                self.accepted.store(true, Ordering::Release);
                let _ = Self::read_request_headers(&mut stream);
                if index == 1 && session_header_on_list {
                    let mut response_with_session = response[..response
                        .windows(b"\r\n\r\n".len())
                        .position(|window| window == b"\r\n\r\n")
                        .expect("response headers")]
                        .to_vec();
                    response_with_session.extend_from_slice(
                        b"\r\nMcp-Session-Id: forbidden-session\r\n\r\n",
                    );
                    response_with_session.extend_from_slice(
                        &response[response
                            .windows(b"\r\n\r\n".len())
                            .position(|window| window == b"\r\n\r\n")
                            .expect("response headers")
                            + b"\r\n\r\n".len()..],
                    );
                    let _ = stream.write_all(&response_with_session);
                } else {
                    let _ = stream.write_all(response);
                }
            }
        }
        self.peer_dropped.store(true, Ordering::Release);
        self.active_tasks.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PeerBroker {
    peer: Arc<LocalPeer>,
    calls: Mutex<Vec<McpRequestId>>,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    responses: Arc<Mutex<Vec<Vec<u8>>>>,
    active: Arc<Mutex<Option<TcpStream>>>,
    cancelled: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    cancel_delivery: Mutex<Option<DeliveryCertainty>>,
    timeout_request: Mutex<Option<McpRequestId>>,
    timeout_delivery: Mutex<Option<DeliveryCertainty>>,
    complete_bodies: AtomicBool,
}

struct BodyDropProbe(Arc<AtomicBool>);

impl Drop for BodyDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

enum BodyState {
    First(Vec<u8>, BodyDropProbe),
    Complete(Vec<u8>, BodyDropProbe),
    Pending(BodyDropProbe),
}

impl PeerBroker {
    fn new(peer: Arc<LocalPeer>) -> Arc<Self> {
        Arc::new(Self {
            peer,
            calls: Mutex::new(Vec::new()),
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(Vec::new())),
            active: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            timed_out: Arc::new(AtomicBool::new(false)),
            cancel_delivery: Mutex::new(None),
            timeout_request: Mutex::new(None),
            timeout_delivery: Mutex::new(None),
            complete_bodies: AtomicBool::new(false),
        })
    }

    fn complete_bodies(&self) {
        self.complete_bodies.store(true, Ordering::Release);
    }

    fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().expect("wire requests").clone()
    }

    fn responses(&self) -> Vec<Vec<u8>> {
        self.responses.lock().expect("wire responses").clone()
    }

    fn connect(&self, call: &PreparedMcpHttpCall) -> TcpStream {
        let address = self.peer.listener.local_addr().expect("peer address");
        let stream = TcpStream::connect(address).expect("connect local peer");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        self.calls
            .lock()
            .expect("request ids")
            .push(call.request_id().clone());
        *self.active.lock().expect("active stream") =
            Some(stream.try_clone().expect("clone stream"));
        stream
    }

    fn request_bytes(call: &PreparedMcpHttpCall) -> Vec<u8> {
        let request: &HttpRequest = call.request();
        let mut bytes =
            format!("{} {} HTTP/1.1\r\n", request.method(), request.path()).into_bytes();
        for header in request.headers() {
            bytes.extend_from_slice(header.name().as_bytes());
            bytes.extend_from_slice(b": ");
            bytes.extend_from_slice(header.value());
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(request.body());
        bytes
    }

    fn response_headers(bytes: &[u8]) -> Vec<HttpHeader> {
        let end = bytes
            .windows(b"\r\n\r\n".len())
            .position(|window| window == b"\r\n\r\n")
            .expect("response headers");
        String::from_utf8_lossy(&bytes[..end])
            .split("\r\n")
            .skip(1)
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some(
                    HttpHeader::new(name.trim(), value.trim().as_bytes().to_vec())
                        .expect("peer response header"),
                )
            })
            .collect()
    }
}

impl McpHttpEffects for PeerBroker {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        let peer = Arc::clone(&self.peer);
        let active = Arc::clone(&self.active);
        let responses = Arc::clone(&self.responses);
        let complete_body = self.complete_bodies.load(Ordering::Acquire);
        let request = Self::request_bytes(&call);
        self.requests
            .lock()
            .expect("wire requests")
            .push(request.clone());
        let mut stream = self.connect(&call);
        Box::pin(async move {
            let (response_headers, first_event) = tokio::task::spawn_blocking(move || {
                stream.write_all(&request)?;
                let mut response_headers = Vec::new();
                loop {
                    let mut byte = [0_u8; 1];
                    stream.read_exact(&mut byte)?;
                    response_headers.push(byte[0]);
                    if response_headers.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let mut first_event = Vec::new();
                loop {
                    let mut byte = [0_u8; 1];
                    stream.read_exact(&mut byte)?;
                    first_event.push(byte[0]);
                    if first_event.ends_with(b"\n\n") {
                        break;
                    }
                }
                Ok::<_, std::io::Error>((response_headers, first_event))
            })
            .await
            .map_err(|error| {
                McpError::from(TransportError::definitely_unsent(
                    TransportFailure::Exchange(error.to_string()),
                ))
            })?
            .map_err(|error| {
                McpError::from(TransportError::possibly_sent(TransportFailure::Exchange(
                    error.to_string(),
                )))
            })?;
            let headers = Self::response_headers(&response_headers);
            let mut response = response_headers;
            response.extend_from_slice(&first_event);
            responses.lock().expect("wire responses").push(response);
            *active.lock().expect("active stream") = None;
            let body_dropped = Arc::clone(&peer.body_dropped);
            let first_body_poll = Arc::clone(&peer.first_body_poll);
            let body = futures::stream::unfold(
                Some(if complete_body {
                    BodyState::Complete(first_event, BodyDropProbe(body_dropped))
                } else {
                    BodyState::First(first_event, BodyDropProbe(body_dropped))
                }),
                move |state| {
                    let first_body_poll = Arc::clone(&first_body_poll);
                    async move {
                        match state? {
                            BodyState::First(chunk, probe) => {
                                first_body_poll.store(true, Ordering::Release);
                                futures::future::pending::<()>().await;
                                Some((
                                    Ok::<Vec<u8>, TransportError>(chunk),
                                    Some(BodyState::Pending(probe)),
                                ))
                            }
                            BodyState::Complete(chunk, _probe) => Some((
                                Ok::<Vec<u8>, TransportError>(chunk),
                                None,
                            )),
                            BodyState::Pending(probe) => {
                                futures::future::pending::<()>().await;
                                Some((
                                    Ok::<Vec<u8>, TransportError>(Vec::new()),
                                    Some(BodyState::Pending(probe)),
                                ))
                            }
                        }
                    }
                },
            );
            McpHttpResponse::response(
                200,
                headers,
                Box::pin(body),
                DeliveryCertainty::PossiblySent,
            )
        })
    }

    fn cancel(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let active = self.active.lock().expect("active stream").take();
        let delivery = if self.peer.first_event_sent.load(Ordering::Acquire) {
            DeliveryCertainty::PossiblySent
        } else {
            DeliveryCertainty::DefinitelyUnsent
        };
        *self.cancel_delivery.lock().expect("cancel delivery") = Some(delivery);
        self.cancelled.store(true, Ordering::Release);
        Box::pin(async move {
            if let Some(stream) = active {
                let _ = stream.shutdown(Shutdown::Both);
            }
            McpHttpSettlement::new(request_id, McpHttpEnd::Cancelled, delivery)
        })
    }

    fn settle(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let active = self.active.lock().expect("active stream").take();
        Box::pin(async move {
            if let Some(stream) = active {
                let _ = stream.shutdown(Shutdown::Both);
            }
            McpHttpSettlement::new(
                request_id,
                McpHttpEnd::Completed,
                DeliveryCertainty::PossiblySent,
            )
        })
    }

    fn timeout(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let active = self.active.lock().expect("active stream").take();
        let delivery = if self.peer.first_event_sent.load(Ordering::Acquire) {
            DeliveryCertainty::PossiblySent
        } else {
            DeliveryCertainty::DefinitelyUnsent
        };
        *self.timeout_request.lock().expect("timeout request") = Some(request_id.clone());
        *self.timeout_delivery.lock().expect("timeout delivery") = Some(delivery);
        self.timed_out.store(true, Ordering::Release);
        Box::pin(async move {
            if let Some(stream) = active {
                let _ = stream.shutdown(Shutdown::Both);
            }
            McpHttpSettlement::new(request_id, McpHttpEnd::TimedOut, delivery)
        })
    }
}

fn options() -> McpClientOptions {
    McpClientOptions::new(64 * 1024, Instant::now() + Duration::from_secs(30))
        .expect("runtime options")
}

fn wire_body(request: &[u8]) -> &[u8] {
    let separator = request
        .windows(b"\r\n\r\n".len())
        .position(|window| window == b"\r\n\r\n")
        .expect("request headers");
    &request[separator + b"\r\n\r\n".len()..]
}

fn assert_current_request(request: &[u8], method: &str) {
    let separator = request
        .windows(b"\r\n\r\n".len())
        .position(|window| window == b"\r\n\r\n")
        .expect("request headers");
    let head = String::from_utf8_lossy(&request[..separator]).to_ascii_lowercase();
    assert!(head.starts_with("post /mcp http/1.1\r\n"));
    assert!(head.contains(&format!("mcp-method: {method}\r\n")));
    let protocol_headers = head
        .split("\r\n")
        .filter(|line| line.starts_with("mcp-protocol-version:"))
        .collect::<Vec<_>>();
    assert_eq!(
        protocol_headers,
        vec!["mcp-protocol-version: 2026-07-28"],
        "exactly one current MCP protocol-version header is required"
    );
    assert!(!head.contains("mcp-session-id"));
    assert!(!head.contains("last-event-id"));
    assert!(!head.starts_with("get "));

    let body: serde_json::Value =
        serde_json::from_slice(wire_body(request)).expect("JSON-RPC request body");
    assert_eq!(body["method"], method);
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
        "ego"
    );
    assert!(body["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["version"]
        .as_str()
        .is_some_and(|version| !version.is_empty()));
    assert!(body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object());

    let lowered = String::from_utf8_lossy(request).to_ascii_lowercase();
    for forbidden in [
        "initialize",
        "initialized",
        "notifications/cancelled",
        "mcp-session-id",
        "last-event-id",
        "reinitialize",
    ] {
        assert!(!lowered.contains(forbidden), "forbidden MCP bytes: {forbidden}");
    }
}

fn assert_current_response(response: &[u8], session_header: bool) {
    let lowered = String::from_utf8_lossy(response).to_ascii_lowercase();
    assert!(lowered.starts_with("http/1.1 200 ok\r\n"));
    assert!(lowered.contains("content-type: text/event-stream"));
    assert_eq!(lowered.contains("mcp-session-id:"), session_header);
    assert!(!lowered.contains("last-event-id:"));
    for forbidden in [
        "initialize",
        "initialized",
        "notifications/cancelled",
        "last-event-id",
        "reinitialize",
    ] {
        assert!(!lowered.contains(forbidden), "forbidden MCP bytes: {forbidden}");
    }
}

#[test]
fn real_peer_cancel_before_headers_releases_peer_and_reports_definitely_unsent() {
    let peer = LocalPeer::bind();
    let broker = PeerBroker::new(Arc::clone(&peer));
    let server = Arc::clone(&peer);
    let thread = thread::spawn(move || server.serve_before_headers());
    let mut client = McpClient::connect_http(
        "runtime-before".parse().expect("server name"),
        peer.config(),
        options(),
        Arc::clone(&broker) as Arc<dyn McpHttpEffects>,
    )
    .expect("client");
    let cancellation = client.cancellation();
    let task = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(client.discover())
    });
    peer.wait_for_accept();
    cancellation.cancel();
    assert!(task.join().expect("discover thread").is_err());
    thread.join().expect("peer thread");
    assert!(peer.peer_dropped.load(Ordering::Acquire));
    assert_eq!(peer.active_tasks.load(Ordering::Acquire), 0);
    assert!(broker.cancelled.load(Ordering::Acquire));
    assert!(!peer.headers_sent.load(Ordering::Acquire));
    assert_eq!(
        *broker.cancel_delivery.lock().expect("cancel delivery"),
        Some(DeliveryCertainty::DefinitelyUnsent)
    );
    let requests = broker.requests();
    let responses = broker.responses();
    assert_eq!(requests.len(), 1);
    assert_current_request(&requests[0], "server/discover");
    assert!(responses.is_empty(), "no peer response arrived before headers");
}

#[test]
fn real_peer_cancel_after_one_sse_event_closes_body_and_preserves_possible_delivery() {
    let peer = LocalPeer::bind();
    let broker = PeerBroker::new(Arc::clone(&peer));
    let server = Arc::clone(&peer);
    let thread = thread::spawn(move || server.serve_after_one_event());
    let mut client = McpClient::connect_http(
        "runtime-after".parse().expect("server name"),
        peer.config(),
        options(),
        Arc::clone(&broker) as Arc<dyn McpHttpEffects>,
    )
    .expect("client");
    let cancellation = client.cancellation();
    let task = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(client.discover())
    });
    while !peer.first_body_poll.load(Ordering::Acquire) {
        thread::yield_now();
    }
    cancellation.cancel();
    assert!(task.join().expect("discover thread").is_err());
    thread.join().expect("peer thread");
    assert!(peer.body_dropped.load(Ordering::Acquire));
    assert!(peer.peer_dropped.load(Ordering::Acquire));
    assert_eq!(peer.active_tasks.load(Ordering::Acquire), 0);
    assert_eq!(broker.calls.lock().expect("request ids").len(), 1);
    assert_eq!(
        *broker.cancel_delivery.lock().expect("cancel delivery"),
        Some(DeliveryCertainty::PossiblySent)
    );
    let requests = broker.requests();
    let responses = broker.responses();
    assert_eq!(requests.len(), 1);
    assert_eq!(responses.len(), 1);
    assert_current_request(&requests[0], "server/discover");
    assert_current_response(&responses[0], false);
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_cancel_and_deadline_share_the_request_owned_token_and_settlement_certainty() {
    let peer = LocalPeer::bind();
    let broker = PeerBroker::new(Arc::clone(&peer));
    let server = Arc::clone(&peer);
    let thread = thread::spawn(move || server.serve_before_headers());
    let mut client = McpClient::connect_http(
        "runtime-token".parse().expect("server name"),
        peer.config(),
        McpClientOptions::new(64 * 1024, Instant::now() + Duration::from_millis(10))
            .expect("short deadline"),
        Arc::clone(&broker) as Arc<dyn McpHttpEffects>,
    )
    .expect("client");
    let result = client.discover().await;
    assert!(result.is_err());
    thread.join().expect("peer thread");
    let request_id = broker.calls.lock().expect("request ids")[0].clone();
    assert_eq!(
        *broker.timeout_request.lock().expect("timeout request"),
        Some(request_id.clone())
    );
    assert!(broker.timed_out.load(Ordering::Acquire));
    assert_eq!(
        *broker.timeout_delivery.lock().expect("timeout delivery"),
        Some(DeliveryCertainty::DefinitelyUnsent)
    );
    assert!(peer.peer_dropped.load(Ordering::Acquire));
    assert_eq!(peer.active_tasks.load(Ordering::Acquire), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_recorder_rejects_forbidden_mcp_bytes_and_requires_current_metadata() {
    let peer = LocalPeer::bind();
    let broker = PeerBroker::new(Arc::clone(&peer));
    broker.complete_bodies();
    let server = Arc::clone(&peer);
    let thread = thread::spawn(move || server.serve_discover_and_list(false));
    let mut client = McpClient::connect_http(
        "runtime-wire".parse().expect("server name"),
        peer.config(),
        options(),
        Arc::clone(&broker) as Arc<dyn McpHttpEffects>,
    )
    .expect("client");
    client.discover().await.expect("discover");
    client.list_tools().await.expect("tools/list");
    assert_eq!(MCP_PROTOCOL_REVISION, "2026-07-28");
    let requests = broker.requests();
    let responses = broker.responses();
    assert_eq!(requests.len(), 2, "Discover/list must not retry or reinitialize");
    assert_eq!(responses.len(), 2, "each logical POST has one peer response");
    assert_current_request(&requests[0], "server/discover");
    assert_current_request(&requests[1], "tools/list");
    assert_current_response(&responses[0], false);
    assert_current_response(&responses[1], false);
    thread.join().expect("peer thread");
    assert!(peer.peer_dropped.load(Ordering::Acquire));
    assert_eq!(peer.active_tasks.load(Ordering::Acquire), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_recorder_rejects_a_server_session_header_without_retrying() {
    let peer = LocalPeer::bind();
    let broker = PeerBroker::new(Arc::clone(&peer));
    broker.complete_bodies();
    let server = Arc::clone(&peer);
    let thread = thread::spawn(move || server.serve_discover_and_list(true));
    let mut client = McpClient::connect_http(
        "runtime-session-header"
            .parse()
            .expect("server name"),
        peer.config(),
        options(),
        Arc::clone(&broker) as Arc<dyn McpHttpEffects>,
    )
    .expect("client");

    client.discover().await.expect("discover");
    assert!(client.list_tools().await.is_err());
    let requests = broker.requests();
    let responses = broker.responses();
    assert_eq!(requests.len(), 2, "session rejection must not retry");
    assert_eq!(responses.len(), 2);
    assert_current_request(&requests[0], "server/discover");
    assert_current_request(&requests[1], "tools/list");
    assert_current_response(&responses[0], false);
    assert_current_response(&responses[1], true);
    thread.join().expect("peer thread");
    assert!(peer.peer_dropped.load(Ordering::Acquire));
    assert_eq!(peer.active_tasks.load(Ordering::Acquire), 0);
}
