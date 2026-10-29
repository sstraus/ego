//! Contracts for process-level ExecCaps diagnostics.
//!
//! Boot measurement is a diagnostic side channel. It does not mint session
//! authority, create session evidence, or prevent a listener from serving.
//! The ACP case deliberately uses the official typed client and WebSocket
//! adapter used by `listener_composition.rs`, never stdio framing.

mod support;

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{Agent, Channel, Client, ConnectTo, Error, TransportFrame};
use agent_client_protocol_schema::{ProtocolVersion, v1};
use async_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use ego_core::endpoint::{Admitted, PublicBind, Requested, admit};
use futures::StreamExt;
use support::{Ego, Running};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::Notify;
use tokio::task::JoinSet;

const ACP_TOKEN: &str = "listener-test-acp-token";
const A2A_TOKEN: &str = "listener-test-a2a-token";
const EMPTY_CAPABILITIES_WARNING: &str = "no effective confinement";

struct ChildGuard {
    running: Option<Running>,
}

impl ChildGuard {
    fn start(ego: Ego) -> Self {
        Self {
            running: Some(ego.spawn()),
        }
    }

    fn running(&mut self) -> &mut Running {
        self.running
            .as_mut()
            .expect("the child guard owns its process")
    }

    fn finish(mut self) -> support::Finished {
        let mut running = self
            .running
            .take()
            .expect("the child guard owns its process");
        if running.is_running() {
            let _ = running.child.kill();
        }
        running.finish()
    }

    fn panic_with_output(&mut self, reason: &str) -> ! {
        let mut running = self
            .running
            .take()
            .expect("the child guard owns its process");
        if running.is_running() {
            let _ = running.child.kill();
        }
        let finished = running.finish();
        panic!(
            "{reason}; child exit={:?}; child stderr:\n{}",
            finished.code, finished.stderr
        );
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut running) = self.running.take() else {
            return;
        };
        if running.is_running() {
            let _ = running.child.kill();
        }
        let _ = running.child.wait();
    }
}

fn free_tcp_address() -> SocketAddr {
    TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("an ephemeral TCP port is available")
        .local_addr()
        .expect("the ephemeral TCP listener has an address")
}

async fn wait_for_tcp(child: &mut ChildGuard, address: SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return;
        }
        if !child.running().is_running() {
            child.panic_with_output(&format!("the listener exited before binding {address}"));
        }
        if tokio::time::Instant::now() >= deadline {
            child.panic_with_output(&format!("the listener did not bind {address}"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct SessionClient {
    workspace: String,
    session_id: Arc<Mutex<Option<String>>>,
    options: Arc<Mutex<Option<Vec<v1::SessionConfigOption>>>>,
    ready: Arc<Notify>,
}

impl ConnectTo<Agent> for SessionClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        let options = Arc::clone(&self.options);
        let update_session = Arc::new(Mutex::new(None::<String>));
        let update_session_for_handler = Arc::clone(&update_session);
        let update_ready = Arc::new(Notify::new());
        let update_ready_for_handler = Arc::clone(&update_ready);
        Client
            .builder()
            .on_receive_notification(
                async move |notification: v1::SessionNotification, _cx| {
                    if let v1::SessionUpdate::ConfigOptionUpdate(update) = notification.update
                        && update
                            .config_options
                            .iter()
                            .any(|option| option.id.0.as_ref() == "sandbox")
                    {
                        *options.lock().expect("session options mutex") =
                            Some(update.config_options);
                        *update_session_for_handler
                            .lock()
                            .expect("update session mutex") =
                            Some(notification.session_id.to_string());
                        update_ready_for_handler.notify_one();
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, async move |connection| {
                let initialized = connection
                    .send_request(
                        v1::InitializeRequest::new(ProtocolVersion::V1)
                            .client_info(v1::Implementation::new("ego-listener-test-cli", "0")),
                    )
                    .block_task()
                    .await?;
                assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
                let session = connection
                    .send_request(v1::NewSessionRequest::new(self.workspace))
                    .block_task()
                    .await?;
                let session_id = session.session_id.to_string();
                *self.session_id.lock().expect("session id mutex") = Some(session_id.clone());
                tokio::time::timeout(
                    support::ACP_MEASURED_OPTIONS_DEADLINE,
                    update_ready.notified(),
                )
                .await
                .map_err(|_| {
                    Error::internal_error().data("measured ACP config update did not arrive")
                })?;
                if update_session
                    .lock()
                    .expect("update session mutex")
                    .as_deref()
                    != Some(session_id.as_str())
                {
                    return Err(Error::internal_error().data("config update named another session"));
                }
                self.ready.notify_one();
                Ok(())
            })
            .await
    }
}

/// Drive an official ACP client over the official WebSocket transport until
/// initialize and session/new are complete, then close the physical socket.
async fn initialize_websocket(address: &str, client: SessionClient) -> Result<(), Error> {
    let mut request = format!("ws://{address}/acp")
        .into_client_request()
        .map_err(|error| Error::internal_error().data(error.to_string()))?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer listener-test-acp-token"),
    );
    let (websocket, response) = async_tungstenite::tokio::connect_async(request)
        .await
        .map_err(|error| Error::internal_error().data(error.to_string()))?;
    assert_eq!(response.status().as_u16(), 101);

    let (mut websocket_tx, mut websocket_rx) = websocket.split();
    let (client_transport, wire_transport) = Channel::duplex();
    let ready = Arc::clone(&client.ready);
    let close_notify = Arc::new(Notify::new());
    let close_sent = Arc::new(AtomicBool::new(false));
    let agent_client = client.connect_to(client_transport);
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
                _ = close_notify_for_writer.notified() => break,
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

    let first_completion = tokio::select! {
        biased;
        _ = ready.notified() => None,
        completion = tasks.join_next() => completion,
    };
    close_notify.notify_one();
    let mut completions = Vec::new();
    if let Some(completion) = first_completion {
        completions.push(completion);
    }
    let drained = tokio::time::timeout(Duration::from_secs(1), async {
        while task_state.load(Ordering::SeqCst) & 0b011 != 0b011 {
            let Some(completion) = tasks.join_next().await else {
                break;
            };
            completions.push(completion);
        }
    })
    .await;
    let completed = task_state.load(Ordering::SeqCst);
    if drained.is_err() || completed & 0b011 != 0b011 {
        tasks.abort_all();
        while let Some(completion) = tasks.join_next().await {
            completions.push(completion);
        }
        return Err(Error::internal_error().data(format!(
            "the ACP WebSocket client did not shut down after transport close: {completed}"
        )));
    }
    if !close_sent.load(Ordering::SeqCst) {
        tasks.abort_all();
        while let Some(completion) = tasks.join_next().await {
            completions.push(completion);
        }
        return Err(Error::internal_error().data("the ACP WebSocket writer did not send Close"));
    }
    let reader_was_pending = completed & 0b100 == 0;
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
    first_error.map_or(Ok(()), Err)
}

async fn agent_card(address: SocketAddr) -> String {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("the A2A listener accepts peers");
    let request = format!(
        "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {A2A_TOKEN}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the A2A listener reads an HTTP request");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
        .await
        .expect("the A2A response has a bounded read timeout")
        .expect("the A2A response could not be read");
    String::from_utf8(response).expect("the A2A response is UTF-8")
}

fn warning_count(stderr: &str) -> usize {
    stderr.matches(EMPTY_CAPABILITIES_WARNING).count()
}

fn assert_durable_observation(home: &Path, session_id: &str) {
    let path = home.join("sessions").join(session_id).join("events.jsonl");
    let events = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "durable session events missing at {}: {error}",
            path.display()
        )
    });
    assert!(
        events.contains("ExecutionMeasured"),
        "observation was not durable: {events}"
    );
}

#[tokio::test]
async fn listener_acp_boot_diagnostic_continues_into_durable_session_controls() {
    let ego = Ego::nowhere();
    let home = ego.home_path().to_owned();
    let workspace = ego.workspace_path().to_owned();
    let address = free_tcp_address();
    let address_text = address.to_string();
    let mut child = ChildGuard::start(
        ego.subcommand("acp")
            .args(["--listen", address_text.as_str(), "--sandbox", "off"])
            .env("EGO_ACP_TOKEN", ACP_TOKEN),
    );
    wait_for_tcp(&mut child, address).await;

    let session_id = Arc::new(Mutex::new(None));
    let options = Arc::new(Mutex::new(None));
    if let Err(error) = initialize_websocket(
        &address.to_string(),
        SessionClient {
            workspace: workspace.to_string_lossy().into_owned(),
            session_id: Arc::clone(&session_id),
            options: Arc::clone(&options),
            ready: Arc::new(Notify::new()),
        },
    )
    .await
    {
        child.panic_with_output(&format!(
            "the official ACP WebSocket client did not complete session/new: {error}"
        ));
    }

    let finished = child.finish();
    let session_id = session_id
        .lock()
        .expect("session id mutex")
        .take()
        .expect("session/new returned an id");
    let options = options
        .lock()
        .expect("session options mutex")
        .take()
        .expect("session/new returned controls");
    assert!(
        options
            .iter()
            .any(|option| option.id.0.as_ref() == "sandbox"),
        "session controls were not seeded after confinement observation: {options:?}"
    );
    assert_eq!(
        warning_count(&finished.stderr),
        1,
        "empty boot capabilities must warn once: {}",
        finished.stderr
    );
    assert_durable_observation(&home, &session_id);
}

#[tokio::test]
async fn a2a_discovery_boot_continues_after_diagnostic_without_session_artifacts() {
    let ego = Ego::nowhere();
    let home = ego.home_path().to_owned();
    let address = free_tcp_address();
    let address_text = address.to_string();
    let mut child = ChildGuard::start(
        ego.subcommand("a2a")
            .args(["--listen", address_text.as_str(), "--sandbox", "off"])
            .env("EGO_A2A_TOKEN", A2A_TOKEN),
    );
    wait_for_tcp(&mut child, address).await;

    let card = agent_card(address).await;
    assert!(
        card.starts_with("HTTP/1.1 200") && card.contains("agent"),
        "discovery-only A2A did not serve its card: {card}"
    );
    let finished = child.finish();
    assert_eq!(
        warning_count(&finished.stderr),
        1,
        "empty discovery boot capabilities must warn once: {}",
        finished.stderr
    );
    assert!(
        !home.join("sessions").exists(),
        "discovery-only boot created session evidence"
    );
}

/// The shared inbound seam is selected once by endpoint kind, then either host
/// protocol composes above it. Keep both real-process probes in one assertion so
/// separate ACP/A2A loopback tests cannot drift into proving unrelated kinds.
#[tokio::test]
async fn the_same_loopback_endpoint_kind_serves_acp_and_a2a() {
    let address = free_tcp_address();
    let admitted = admit(Requested::Tcp(address), PublicBind::Refused)
        .expect("the shared test endpoint is admitted");
    assert_eq!(
        admitted,
        Admitted::Loopback(address),
        "the cross-protocol contract must exercise one loopback endpoint kind"
    );
    let listen = admitted.to_string();

    let acp_ego = Ego::nowhere();
    let workspace = acp_ego.workspace_path().to_string_lossy().into_owned();
    let mut acp = ChildGuard::start(
        acp_ego
            .subcommand("acp")
            .args(["--listen", listen.as_str()])
            .env("EGO_ACP_TOKEN", ACP_TOKEN),
    );
    wait_for_tcp(&mut acp, address).await;
    if let Err(error) = initialize_websocket(
        &listen,
        SessionClient {
            workspace,
            session_id: Arc::new(Mutex::new(None)),
            options: Arc::new(Mutex::new(None)),
            ready: Arc::new(Notify::new()),
        },
    )
    .await
    {
        acp.panic_with_output(&format!(
            "ACP did not serve over the admitted loopback endpoint kind: {error}"
        ));
    }
    let _ = acp.finish();

    let a2a_ego = Ego::nowhere();
    let mut a2a = ChildGuard::start(
        a2a_ego
            .subcommand("a2a")
            .args(["--listen", listen.as_str()])
            .env("EGO_A2A_TOKEN", A2A_TOKEN),
    );
    wait_for_tcp(&mut a2a, address).await;
    let card = agent_card(address).await;
    assert!(
        card.starts_with("HTTP/1.1 200") && card.contains("agent"),
        "A2A serves over the same admitted loopback endpoint kind: {card}"
    );
    let _ = a2a.finish();
}
