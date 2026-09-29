//! Contracts for CLI-owned ACP listener composition.
//!
//! The lower crates prove endpoint admission, listener lifecycle and ACP
//! transport behavior in isolation. This suite keeps the composition root
//! honest: the real `ego` process must select the transport, order startup
//! refusals, load only the credential its endpoint requires, and carry the
//! endpoint ceiling into the ACP session controls.

mod support;

use std::net::{SocketAddr, TcpListener};
#[cfg(unix)]
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use agent_client_protocol::ByteStreams;
use agent_client_protocol::{Agent, Channel, Client, ConnectTo, Error, TransportFrame};
use agent_client_protocol_schema::{ProtocolVersion, v1};
use async_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use futures::StreamExt;
use support::{Ego, Running};
use tokio::sync::Notify;
use tokio::task::JoinSet;
#[cfg(unix)]
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

const ACP_TOKEN: &str = "regression case-token";

/// A process guard that cannot leak a resident listener when an assertion
/// fails. The test still talks to the child through the public binary seam;
/// this only bounds cleanup after a failure.
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
            .expect("the child guard still owns its process")
    }

    fn finish(mut self) -> support::Finished {
        let mut running = self
            .running
            .take()
            .expect("the child guard still owns its process");
        if running.is_running() {
            let _ = running.child.kill();
        }
        running.finish()
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

/// Refusals must be observable without allowing a broken listener to hang the
/// test process indefinitely.
fn expect_exit(mut child: ChildGuard) -> support::Finished {
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.running().is_running() {
        assert!(
            Instant::now() < deadline,
            "the ACP startup refusal did not terminate within five seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
    child.finish()
}

fn acp_refusal<const N: usize>(args: [&str; N]) -> support::Finished {
    expect_exit(ChildGuard::start(
        Ego::nowhere().subcommand("acp").args(args),
    ))
}

#[cfg(unix)]
fn wait_for_path(child: &mut ChildGuard, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if path.exists() {
            return;
        }
        assert!(
            child.running().is_running(),
            "the ACP process exited before creating its local endpoint {}",
            path.display()
        );
        assert!(
            Instant::now() < deadline,
            "the ACP process did not create its local endpoint {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

async fn wait_for_tcp(child: &mut ChildGuard, address: SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return;
        }
        assert!(
            child.running().is_running(),
            "the ACP process exited before binding {address}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "the ACP process did not bind {address}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn acp_without_listen_remains_a_stdio_process() {
    let running = Ego::nowhere().subcommand("acp").spawn_conversing();
    let mut host = support::AcpHost::adopt(running);
    let initialized = host.initialize();
    assert!(
        initialized.get("result").is_some(),
        "`ego acp` without `--listen` did not answer ACP on stdout: {initialized}"
    );
    let stderr = host.finish();
    assert!(
        stderr.is_empty() || !stderr.contains("EGO_ACP_TOKEN"),
        "stdio ACP unexpectedly attempted network bearer authentication: {stderr}"
    );
}

#[test]
fn a_public_bind_is_refused_before_a_missing_acp_token() {
    let finished = acp_refusal(["--listen", "0.0.0.0:0"]);
    assert_ne!(
        finished.code,
        Some(0),
        "public bind was accepted: {}",
        finished.stderr
    );
    assert!(
        finished.stderr.contains("--allow-public-bind"),
        "the endpoint admission refusal must name its remedy: {}",
        finished.stderr
    );
    assert!(
        !finished.stderr.contains("EGO_ACP_TOKEN"),
        "credential loading ran before public endpoint admission: {}",
        finished.stderr
    );
}

#[test]
fn a_loopback_bind_refuses_startup_without_the_acp_token() {
    let finished = acp_refusal(["--listen", "127.0.0.1:0"]);
    assert_ne!(finished.code, Some(0), "loopback started without a token");
    assert!(
        finished.stderr.contains("EGO_ACP_TOKEN"),
        "the loopback refusal must name the ACP credential variable: {}",
        finished.stderr
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_local_endpoint_starts_and_answers_acp_without_reading_a_bearer_token() {
    use std::os::unix::fs::PermissionsExt as _;
    use tokio::net::UnixStream;

    let ego = Ego::nowhere();
    let mut permissions = std::fs::metadata(ego.workspace_path())
        .expect("the local ACP workspace exists")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(ego.workspace_path(), permissions)
        .expect("the local ACP workspace can be made private");
    let socket = ego.workspace_path().join("acp.sock");
    let socket_name = socket.to_string_lossy().into_owned();
    let mut child = ChildGuard::start(
        ego.subcommand("acp")
            .args(["--listen", socket_name.as_str()]),
    );
    wait_for_path(&mut child, &socket);

    let stream = UnixStream::connect(&socket)
        .await
        .expect("the local ACP socket accepts peers");
    let (reader, writer) = tokio::io::split(stream);
    let transport = ByteStreams::new(writer.compat_write(), reader.compat());
    let ready = Arc::new(Notify::new());
    let mut client = tokio::spawn(
        InitializingClient {
            ready: Arc::clone(&ready),
        }
        .connect_to(transport),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            biased;
            _ = ready.notified() => {}
            result = &mut client => {
                let result = result.expect("the local ACP client task joins");
                panic!("local ACP initialization failed without a bearer token: {result:?}");
            }
        }
    })
    .await
    .expect("the local ACP server answers initialize within five seconds");
    client.abort();
    let _ = client.await;
}

#[cfg(unix)]
struct InitializingClient {
    ready: Arc<Notify>,
}

#[cfg(unix)]
impl ConnectTo<Agent> for InitializingClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        Client
            .builder()
            .connect_with(agent, async move |connection| {
                let response = connection
                    .send_request(
                        v1::InitializeRequest::new(ProtocolVersion::V1)
                            .client_info(v1::Implementation::new("ego-regression case-local", "0")),
                    )
                    .block_task()
                    .await?;
                assert_eq!(response.protocol_version, ProtocolVersion::V1);
                self.ready.notify_one();
                Ok(())
            })
            .await
    }
}

struct SessionClient {
    workspace: String,
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
                            .client_info(v1::Implementation::new("ego-regression case", "0")),
                    )
                    .block_task()
                    .await?;
                assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
                let session = connection
                    .send_request(v1::NewSessionRequest::new(self.workspace))
                    .block_task()
                    .await?;
                let session_id = session.session_id.to_string();
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

/// The CLI integration uses the same official ACP client/channel and
/// async-tungstenite adapter as `ego-acp`'s listener transport contract. The
/// test observes a real process and keeps the wire lifecycle bounded without
/// introducing a second WebSocket implementation.
async fn initialize_websocket(address: &str, client: SessionClient) -> Result<(), Error> {
    let mut request = format!("ws://{address}/acp")
        .into_client_request()
        .map_err(|error| Error::internal_error().data(error.to_string()))?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer regression case-token"),
    );
    let (websocket, response) = async_tungstenite::tokio::connect_async(request)
        .await
        .map_err(|error| Error::internal_error().data(error.to_string()))?;
    assert_eq!(response.status().as_u16(), 101);

    let (mut websocket_tx, mut websocket_rx) = websocket.split();
    let (client_transport, wire_transport) = Channel::duplex();
    let ready = Arc::clone(&client.ready);
    let close_notify = Arc::new(Notify::new());
    let close_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
    // Keep the SDK foreground future alive after `session/new`, then close
    // the physical WebSocket only after the real client handshake completes.
    // This is the same bounded lifecycle as the ACP transport contract test.
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
            Error::internal_error()
                .data("the ACP WebSocket closed before session creation completed")
        }));
    }
    first_error.map_or(Ok(()), Err)
}

fn free_tcp_address() -> SocketAddr {
    TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("an ephemeral TCP port is available")
        .local_addr()
        .expect("the ephemeral TCP listener has an address")
}

#[tokio::test]
async fn a_public_ceiling_is_carried_into_the_real_acp_session_controls() {
    let loopback = free_tcp_address();
    let public = format!("0.0.0.0:{}", loopback.port());
    let ego = Ego::nowhere();
    let workspace = ego.workspace_path().to_string_lossy().into_owned();
    let mut child = ChildGuard::start(
        ego.subcommand("acp")
            .args([
                "--listen",
                public.as_str(),
                "--allow-public-bind",
                "--sandbox",
                "off",
            ])
            .env("EGO_ACP_TOKEN", ACP_TOKEN),
    );
    wait_for_tcp(&mut child, loopback).await;

    let options = Arc::new(Mutex::new(None));
    let client = SessionClient {
        workspace,
        options: Arc::clone(&options),
        ready: Arc::new(Notify::new()),
    };
    if let Err(error) = initialize_websocket(&loopback.to_string(), client).await {
        let finished = child.finish();
        panic!(
            "the real CLI ACP WebSocket did not complete initialize and session/new: {error}; child stderr: {}",
            finished.stderr
        );
    }
    let options = options
        .lock()
        .expect("session options mutex")
        .take()
        .expect("ACP session/new publishes the session controls");
    let sandbox = options
        .iter()
        .find(|option| option.id.0.as_ref() == "sandbox")
        .expect("ACP session/new publishes the sandbox control");
    let v1::SessionConfigKind::Select(select) = &sandbox.kind else {
        panic!("the sandbox session control is not a select: {sandbox:?}");
    };
    assert_eq!(
        select.current_value.0.as_ref(),
        "workspace",
        "a public endpoint must cap `--sandbox off` at workspace: {sandbox:?}"
    );
    let values = match &select.options {
        v1::SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| option.value.0.to_string())
            .collect::<Vec<_>>(),
        other => panic!("the sandbox control is unexpectedly grouped: {other:?}"),
    };
    assert!(
        values
            .iter()
            .all(|value| matches!(value.as_str(), "ro" | "workspace")),
        "the public endpoint exposed a value outside its workspace ceiling: {sandbox:?}"
    );
    assert!(
        !values.iter().any(|value| value == "off"),
        "the public endpoint exposed the unbounded `off` control: {sandbox:?}"
    );
}
