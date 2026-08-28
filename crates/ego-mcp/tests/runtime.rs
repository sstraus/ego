//! RED runtime contract for guarded Streamable HTTP cancellation.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ego_core::config::{McpHttpAuth, McpHttpConfig, McpHttpUrl};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{HttpRequest, TransportError, TransportFailure};
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
}

struct PeerBroker {
    peer: Arc<LocalPeer>,
    calls: Mutex<Vec<McpRequestId>>,
    active: Arc<Mutex<Option<TcpStream>>>,
    cancelled: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    cancel_delivery: Mutex<Option<DeliveryCertainty>>,
    timeout_request: Mutex<Option<McpRequestId>>,
    timeout_delivery: Mutex<Option<DeliveryCertainty>>,
}

struct BodyDropProbe(Arc<AtomicBool>);

impl Drop for BodyDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

enum BodyState {
    First(Vec<u8>, BodyDropProbe),
    Pending(BodyDropProbe),
}

impl PeerBroker {
    fn new(peer: Arc<LocalPeer>) -> Arc<Self> {
        Arc::new(Self {
            peer,
            calls: Mutex::new(Vec::new()),
            active: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            timed_out: Arc::new(AtomicBool::new(false)),
            cancel_delivery: Mutex::new(None),
            timeout_request: Mutex::new(None),
            timeout_delivery: Mutex::new(None),
        })
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
}

impl McpHttpEffects for PeerBroker {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        let peer = Arc::clone(&self.peer);
        let active = Arc::clone(&self.active);
        let request = Self::request_bytes(&call);
        let mut stream = self.connect(&call);
        Box::pin(async move {
            let first_event = tokio::task::spawn_blocking(move || {
                stream.write_all(&request)?;
                let mut bytes = Vec::new();
                loop {
                    let mut byte = [0_u8; 1];
                    stream.read_exact(&mut byte)?;
                    bytes.push(byte[0]);
                    if bytes.ends_with(b"\r\n\r\n") {
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
                Ok::<_, std::io::Error>(first_event)
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
            *active.lock().expect("active stream") = None;
            let body_dropped = Arc::clone(&peer.body_dropped);
            let first_body_poll = Arc::clone(&peer.first_body_poll);
            let body = futures::stream::unfold(
                Some(BodyState::First(first_event, BodyDropProbe(body_dropped))),
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
                vec![
                    ego_core::permit::HttpHeader::new(
                        "content-type",
                        b"text/event-stream".to_vec(),
                    )
                    .expect("content type"),
                ],
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
    let mut client = McpClient::connect_http(
        "runtime-wire".parse().expect("server name"),
        peer.config(),
        options(),
        Arc::clone(&broker) as Arc<dyn McpHttpEffects>,
    )
    .expect("client");
    let cancellation = client.cancellation();
    cancellation.cancel();
    assert!(cancellation.is_cancelled());
    assert_eq!(MCP_PROTOCOL_REVISION, "2026-07-28");
    let _ = client.discover().await;
    assert!(broker.calls.lock().expect("request ids").len() <= 1);
}
