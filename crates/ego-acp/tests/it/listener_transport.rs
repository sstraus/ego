//! Contracts for ACP's listener-owned transport lifecycle.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[cfg(unix)]
use agent_client_protocol::ByteStreams;
use agent_client_protocol::{Agent, Channel, Client, ConnectTo, Error, TransportFrame};
use agent_client_protocol_schema::{ProtocolVersion, v1};
use async_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use ego_acp::auth::Credential;
use ego_core::endpoint::{PublicBind, Requested, admit};
use ego_listen::{ConnectionGate, ConnectionLimits, bind};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinSet;
#[cfg(unix)]
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::support;

struct Running {
    address: String,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Running {
    async fn shutdown(self) {
        self.stop.send(()).expect("the listener is still running");
        tokio::time::timeout(Duration::from_secs(1), self.task)
            .await
            .expect("the ACP listener shuts down")
            .expect("the ACP listener task joins")
            .expect("the ACP listener shuts down cleanly");
    }
}

async fn tcp_server(
    credential: Option<Credential>,
    limits: ConnectionLimits,
    factory_calls: Arc<AtomicUsize>,
) -> Running {
    let admitted = admit(
        Requested::Tcp("127.0.0.1:0".parse().unwrap()),
        PublicBind::Refused,
    )
    .unwrap();
    let bound = bind(&admitted).await.unwrap();
    let address = bound.advertised().unwrap();
    let gate = ConnectionGate::new(limits);
    let (stop, shutdown) = oneshot::channel();
    let task = tokio::spawn({
        let factory_calls = Arc::clone(&factory_calls);
        async move {
            ego_acp::serve::serve_on(
                bound,
                &admitted,
                credential,
                gate,
                move || {
                    factory_calls.fetch_add(1, Ordering::SeqCst);
                    support::agent()
                },
                async move {
                    let _ = shutdown.await;
                },
            )
            .await
        }
    });
    Running {
        address,
        stop,
        task,
    }
}

fn handshake(path: &str, authorization: Option<&str>, key: &str) -> String {
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: Upgrade\r\n\
         upgrade: websocket\r\nsec-websocket-version: 13\r\nsec-websocket-key: {key}\r\n"
    );
    if let Some(authorization) = authorization {
        request.push_str(&format!("authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    request
}

async fn response_for(address: &str, request: &str) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), response_head(&mut stream))
        .await
        .unwrap()
}

async fn response_head(stream: &mut TcpStream) -> String {
    let mut response = Vec::new();
    loop {
        let mut byte = [0; 1];
        stream.read_exact(&mut byte).await.unwrap();
        response.push(byte[0]);
        if response.ends_with(b"\r\n\r\n") {
            return String::from_utf8(response).unwrap();
        }
    }
}

async fn wait_for_count(calls: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("factory count never reached {expected}"));
}

struct InitializingClient {
    initialized: Arc<AtomicUsize>,
    ready: Arc<Notify>,
    client_name: String,
}

impl ConnectTo<Agent> for InitializingClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        Client
            .builder()
            .connect_with(agent, async move |connection| {
                let response = connection
                    .send_request(
                        v1::InitializeRequest::new(ProtocolVersion::V1)
                            .client_info(v1::Implementation::new(self.client_name, "0")),
                    )
                    .block_task()
                    .await?;
                assert_eq!(response.protocol_version, ProtocolVersion::V1);
                self.initialized.fetch_add(1, Ordering::SeqCst);
                self.ready.notify_one();
                Ok(())
            })
            .await
    }
}

#[cfg(unix)]
async fn initialize_local(
    path: &str,
    initialized: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<Result<(), Error>> {
    let stream = UnixStream::connect(path).await.unwrap();
    let (reader, writer) = tokio::io::split(stream);
    let transport = ByteStreams::new(writer.compat_write(), reader.compat());
    tokio::spawn(
        InitializingClient {
            initialized,
            ready: Arc::new(Notify::new()),
            client_name: "ego-listener-test-test".to_owned(),
        }
        .connect_to(transport),
    )
}

async fn initialize_websocket(
    address: &str,
    initialized: Arc<AtomicUsize>,
    client_name: String,
) -> Result<(), Error> {
    let mut request = format!("ws://{address}/acp")
        .into_client_request()
        .map_err(|error| Error::internal_error().data(error.to_string()))?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer acp-secret"),
    );
    let (websocket, response) = async_tungstenite::tokio::connect_async(request)
        .await
        .map_err(|error| Error::internal_error().data(error.to_string()))?;
    assert_eq!(response.status().as_u16(), 101);

    let (mut websocket_tx, mut websocket_rx) = websocket.split();
    let (client_transport, wire_transport) = Channel::duplex();
    let ready = Arc::new(Notify::new());
    let close_notify = Arc::new(Notify::new());
    let close_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent_client = InitializingClient {
        initialized,
        ready: Arc::clone(&ready),
        client_name,
    }
    .connect_to(client_transport);
    let Channel {
        rx: mut outgoing,
        tx: incoming,
    } = wire_transport;
    let close_sent_for_writer = Arc::clone(&close_sent);
    let close_notify_for_writer = Arc::clone(&close_notify);
    let writer = async move {
        loop {
            tokio::select! {
                biased;
                _ = close_notify_for_writer.notified() => {
                    break;
                }
                frame = outgoing.next() => {
                    let Some(frame) = frame else { break };
                    let text = frame.to_json()?;
                    async_tungstenite::WebSocketSender::send(
                        &mut websocket_tx,
                        Message::Text(text.into()),
                    )
                    .await
                    .map_err(|error| Error::internal_error().data(error.to_string()))?;
                }
            }
        }
        close_sent_for_writer.store(true, Ordering::SeqCst);
        async_tungstenite::WebSocketSender::send(&mut websocket_tx, Message::Close(None))
            .await
            .map_err(|error| Error::internal_error().data(error.to_string()))?;
        Ok::<(), Error>(())
    };
    let reader = async move {
        while let Some(message) = websocket_rx.next().await {
            match message.map_err(|error| Error::internal_error().data(error.to_string()))? {
                Message::Text(text) => incoming
                    .unbounded_send(TransportFrame::parse_json(text.as_ref()))
                    .map_err(|error| Error::internal_error().data(error.to_string()))?,
                Message::Binary(bytes) => {
                    let text = std::str::from_utf8(bytes.as_ref())
                        .map_err(|error| Error::internal_error().data(error.to_string()))?;
                    incoming
                        .unbounded_send(TransportFrame::parse_json(text))
                        .map_err(|error| Error::internal_error().data(error.to_string()))?;
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
                _ => {}
            }
        }
        Ok::<(), Error>(())
    };
    // The SDK's foreground future intentionally remains alive after the
    // initialize response. Drive every future concurrently, close the
    // physical WebSocket only after that response proves the real client
    // handshake. The SDK client and writer must finish; the reader is joined
    // after bounded cancellation because the peer need not send a close frame.
    let mut tasks = JoinSet::new();
    let task_state = Arc::new(AtomicUsize::new(0));
    let task_state_for_agent = Arc::clone(&task_state);
    tasks.spawn(async move {
        let result = agent_client.await;
        task_state_for_agent.fetch_or(1, Ordering::SeqCst);
        ("agent_client", result)
    });
    let task_state_for_writer = Arc::clone(&task_state);
    tasks.spawn(async move {
        let result = writer.await;
        task_state_for_writer.fetch_or(2, Ordering::SeqCst);
        ("writer", result)
    });
    let task_state_for_reader = Arc::clone(&task_state);
    tasks.spawn(async move {
        let result = reader.await;
        task_state_for_reader.fetch_or(4, Ordering::SeqCst);
        ("reader", result)
    });

    let mut initialized = false;
    let first_completion = tokio::select! {
        biased;
        _ = ready.notified() => {
            initialized = true;
            None
        }
        completion = tasks.join_next() => completion,
    };
    close_notify.notify_one();

    let mut completions = Vec::new();
    if let Some(completion) = first_completion {
        completions.push(completion);
    }
    let required_tasks_drained = tokio::time::timeout(Duration::from_secs(1), async {
        while task_state.load(Ordering::SeqCst) & 0b011 != 0b011 {
            let Some(completion) = tasks.join_next().await else {
                break;
            };
            completions.push(completion);
        }
    })
    .await;
    let completed = task_state.load(Ordering::SeqCst);
    if required_tasks_drained.is_err() || completed & 0b011 != 0b011 {
        tasks.abort_all();
        while let Some(completion) = tasks.join_next().await {
            completions.push(completion);
        }
        return Err(Error::internal_error().data(format!(
            "the ACP WebSocket client tasks did not shut down after transport close; \
             required task mask: {completed}"
        )));
    }
    if !close_sent.load(Ordering::SeqCst) {
        tasks.abort_all();
        while let Some(completion) = tasks.join_next().await {
            completions.push(completion);
        }
        return Err(Error::internal_error()
            .data("the ACP WebSocket writer completed without sending Close"));
    }
    let reader_was_pending = task_state.load(Ordering::SeqCst) & 0b100 == 0;
    tasks.abort_all();
    while let Some(completion) = tasks.join_next().await {
        completions.push(completion);
    }

    let mut first_error = None;
    for completion in completions {
        match completion {
            Ok((_task, Ok(()))) => {}
            Ok((_task, Err(error))) => {
                first_error.get_or_insert(error);
            }
            Err(error) if reader_was_pending && error.is_cancelled() => {}
            Err(error) => {
                first_error.get_or_insert_with(|| Error::internal_error().data(error.to_string()));
            }
        }
    }
    if !initialized {
        return Err(first_error.unwrap_or_else(|| {
            Error::internal_error().data("the ACP WebSocket closed before initialize completed")
        }));
    }
    first_error.map_or(Ok(()), Err)
}

#[tokio::test]
async fn tcp_authentication_rejects_missing_wrong_and_non_websocket_requests_before_factory() {
    let calls = Arc::new(AtomicUsize::new(0));
    let credential = Credential::from_env(|_| Some("acp-secret".to_owned())).unwrap();
    let server = tcp_server(
        Some(credential),
        ConnectionLimits::default(),
        Arc::clone(&calls),
    )
    .await;

    let missing = response_for(
        &server.address,
        &handshake("/unknown", None, "MDEyMzQ1Njc4OWFiY2RlZg=="),
    )
    .await;
    assert!(missing.starts_with("HTTP/1.1 401"), "{missing}");
    let wrong = response_for(
        &server.address,
        &handshake("/unknown", Some("Bearer wrong"), "YWJjZGVmZ2hpamtsbW5vcA=="),
    )
    .await;
    assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");
    let non_websocket = response_for(
        &server.address,
        "GET /acp HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer acp-secret\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert!(
        non_websocket
            .as_bytes()
            .get(9)
            .is_some_and(|code| *code == b'4'),
        "an authenticated non-WebSocket request was not refused: {non_websocket}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn authenticated_websocket_factory_is_fresh_and_the_permit_survives_http_101() {
    let calls = Arc::new(AtomicUsize::new(0));
    let credential = Credential::from_env(|_| Some("acp-secret".to_owned())).unwrap();
    let server = tcp_server(
        Some(credential),
        ConnectionLimits::new(1, Duration::from_secs(60), 4096).unwrap(),
        Arc::clone(&calls),
    )
    .await;

    let mut first = TcpStream::connect(&server.address).await.unwrap();
    first
        .write_all(
            &handshake(
                "/acp",
                Some("Bearer acp-secret"),
                "MDEyMzQ1Njc4OWFiY2RlZg==",
            )
            .into_bytes(),
        )
        .await
        .unwrap();
    let first_response = response_head(&mut first).await;
    assert!(
        first_response.starts_with("HTTP/1.1 101"),
        "{first_response}"
    );
    wait_for_count(&calls, 1).await;

    let mut second = TcpStream::connect(&server.address).await.unwrap();
    second
        .write_all(
            &handshake(
                "/acp",
                Some("Bearer acp-secret"),
                "YWJjZGVmZ2hpamtsbW5vcA==",
            )
            .into_bytes(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), response_head(&mut second))
            .await
            .is_err(),
        "the permit was released when the first HTTP task returned 101"
    );
    drop(first);
    let second_response = tokio::time::timeout(Duration::from_secs(1), response_head(&mut second))
        .await
        .unwrap();
    assert!(
        second_response.starts_with("HTTP/1.1 101"),
        "{second_response}"
    );
    wait_for_count(&calls, 2).await;
    drop(second);
    server.shutdown().await;
}

#[tokio::test]
async fn authenticated_websocket_initialize_uses_the_sdk_client() {
    let calls = Arc::new(AtomicUsize::new(0));
    let credential = Credential::from_env(|_| Some("acp-secret".to_owned())).unwrap();
    let server = tcp_server(
        Some(credential),
        ConnectionLimits::default(),
        Arc::clone(&calls),
    )
    .await;
    let initialized = Arc::new(AtomicUsize::new(0));

    tokio::time::timeout(
        Duration::from_secs(2),
        initialize_websocket(
            &server.address,
            Arc::clone(&initialized),
            "ego-listener-test-websocket-test".to_owned(),
        ),
    )
    .await
    .expect("the SDK WebSocket client initializes")
    .expect("the authenticated WebSocket initialize succeeds");
    assert_eq!(initialized.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.shutdown().await;
}

#[tokio::test]
#[cfg(unix)]
async fn local_byte_streams_initialize_and_sequential_connections_get_fresh_factories() {
    // A short name keeps the socket path under the platform sun_path limit.
    let mut path = std::env::temp_dir();
    path.push("ego-acp");
    path.push(uuid::Uuid::new_v4().simple().to_string());
    path.push("acp.sock");
    let admitted = admit(
        Requested::Local {
            name: path.display().to_string(),
        },
        PublicBind::Refused,
    )
    .unwrap();
    let bound = bind(&admitted).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 4096).unwrap());
    let (stop, shutdown) = oneshot::channel();
    let task = tokio::spawn({
        let calls = Arc::clone(&calls);
        async move {
            ego_acp::serve::serve_on(
                bound,
                &admitted,
                None,
                gate,
                move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    support::agent()
                },
                async move {
                    let _ = shutdown.await;
                },
            )
            .await
        }
    });

    let initialized = Arc::new(AtomicUsize::new(0));
    let first = initialize_local(path.to_str().unwrap(), Arc::clone(&initialized)).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while initialized.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    first.abort();
    let _ = first.await;

    let second = initialize_local(path.to_str().unwrap(), Arc::clone(&initialized)).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while initialized.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    second.abort();
    let _ = second.await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn connection_lifetime_closes_one_connection_but_the_listener_survives() {
    let calls = Arc::new(AtomicUsize::new(0));
    let credential = Credential::from_env(|_| Some("acp-secret".to_owned())).unwrap();
    let server = tcp_server(
        Some(credential),
        ConnectionLimits::new(1, Duration::from_millis(250), 4096).unwrap(),
        Arc::clone(&calls),
    )
    .await;
    let mut first = TcpStream::connect(&server.address).await.unwrap();
    first
        .write_all(
            &handshake(
                "/acp",
                Some("Bearer acp-secret"),
                "MDEyMzQ1Njc4OWFiY2RlZg==",
            )
            .into_bytes(),
        )
        .await
        .unwrap();
    assert!(response_head(&mut first).await.starts_with("HTTP/1.1 101"));
    let mut closed = [0; 1];
    let closed_result = tokio::time::timeout(Duration::from_secs(1), first.read(&mut closed))
        .await
        .unwrap();
    assert!(matches!(closed_result, Ok(0) | Err(_)));
    drop(first);
    let mut second = TcpStream::connect(&server.address).await.unwrap();
    second
        .write_all(
            &handshake(
                "/acp",
                Some("Bearer acp-secret"),
                "YWJjZGVmZ2hpamtsbW5vcA==",
            )
            .into_bytes(),
        )
        .await
        .unwrap();
    assert!(response_head(&mut second).await.starts_with("HTTP/1.1 101"));
    wait_for_count(&calls, 2).await;
    drop(second);
    server.shutdown().await;
}

#[tokio::test]
async fn ingress_shutdown_closes_only_the_exhausted_connection() {
    let calls = Arc::new(AtomicUsize::new(0));
    let credential = Credential::from_env(|_| Some("acp-secret".to_owned())).unwrap();
    let server = tcp_server(
        Some(credential),
        ConnectionLimits::new(1, Duration::from_secs(60), 1024).unwrap(),
        Arc::clone(&calls),
    )
    .await;
    let oversized_initialize = tokio::time::timeout(
        Duration::from_secs(1),
        initialize_websocket(
            &server.address,
            Arc::new(AtomicUsize::new(0)),
            "x".repeat(2048),
        ),
    )
    .await
    .expect("the oversized valid ACP frame closes the client promptly");
    assert!(
        oversized_initialize.is_err(),
        "an oversized valid masked ACP WebSocket frame bypassed the ingress budget"
    );

    let mut second = TcpStream::connect(&server.address).await.unwrap();
    second
        .write_all(
            &handshake(
                "/acp",
                Some("Bearer acp-secret"),
                "YWJjZGVmZ2hpamtsbW5vcA==",
            )
            .into_bytes(),
        )
        .await
        .unwrap();
    assert!(response_head(&mut second).await.starts_with("HTTP/1.1 101"));
    wait_for_count(&calls, 2).await;
    drop(second);
    server.shutdown().await;
}
