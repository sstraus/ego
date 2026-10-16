//! A pinned socket owned by the run, never by a provider wire mapping.
use super::*;
use async_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use ego_core::permit::{PreparedConnect, WebSocketSession};
use tokio::sync::{mpsc, oneshot};

const MAX_MESSAGE: usize = 16 * 1024 * 1024;

enum Command {
    Send(
        Vec<u8>,
        Instant,
        oneshot::Sender<Result<(), TransportError>>,
    ),
    Receive(
        Instant,
        oneshot::Sender<Result<Option<Vec<u8>>, TransportError>>,
    ),
}

struct Session {
    target: NetworkTarget,
    path: String,
    headers: Vec<HttpHeader>,
    cancel: CancelToken,
    run_deadline: Option<Instant>,
    sender: mpsc::Sender<Command>,
    _task: AbortOnDrop<()>,
}

pub(super) fn open(
    transport: &OneShotHttpTransport,
    connect: ResolvedConnect,
    request: HttpRequest,
    deadline: Instant,
) -> BoxFuture<'static, Result<Box<dyn WebSocketSession>, TransportError>> {
    let cancel = transport.cancel.clone();
    let run_deadline = transport.run_deadline;
    let offline = transport.confinement.offline();
    let mut task = AbortOnDrop(runtime_handle().spawn(async move {
        if offline && !offline_connect_allowed(&connect) {
            return Err(definitely_unsent(TransportFailure::Offline));
        }
        check_scope(&cancel, run_deadline, deadline)?;
        if request.method() != "POST"
            || &ego_core::permit::HttpRequestDigest::of(&request) != connect.request_digest()
        {
            return Err(definitely_unsent(TransportFailure::AuthorityMismatch));
        }
        let target = connect.target().clone();
        let scheme = match connect.route().scheme() {
            HttpScheme::Http => "ws",
            HttpScheme::Https => "wss",
        };
        let authority = if target.host().contains(':') {
            format!("[{}]:{}", target.host(), target.port())
        } else {
            format!("{}:{}", target.host(), target.port())
        };
        let mut handshake = format!("{scheme}://{authority}{}", request.path())
            .into_client_request()
            .map_err(|_| definitely_unsent(TransportFailure::InvalidRequest("invalid WebSocket target".into())))?;
        for header in request.headers() {
            let name = async_tungstenite::tungstenite::http::HeaderName::from_bytes(header.name().as_bytes())
                .map_err(|_| definitely_unsent(TransportFailure::AuthorityMismatch))?;
            let value = async_tungstenite::tungstenite::http::HeaderValue::from_bytes(header.value())
                .map_err(|_| definitely_unsent(TransportFailure::AuthorityMismatch))?;
            handshake.headers_mut().insert(name, value);
        }
        // Connect only to the admitted address; TLS still authenticates the logical host.
        let opening = async {
            let tcp = tokio::net::TcpStream::connect(SocketAddr::new(connect.selected(), target.port()))
                .await.map_err(|_| definitely_unsent(TransportFailure::Connect("WebSocket connection failed".into())))?;
            let config = async_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
            async_tungstenite::tokio::client_async_tls_with_config(handshake, tcp, Some(config))
                .await.map_err(|_| definitely_unsent(TransportFailure::Connect("WebSocket handshake failed".into())))
        };
        let (mut socket, _) = tokio::select! {
            result = opening => result?,
            failure = scope_end(cancel.clone(), run_deadline, deadline) => return Err(definitely_unsent(failure)),
        };
        let (sender, mut receiver) = mpsc::channel(1);
        let lifetime_cancel = cancel.clone();
        let owner = AbortOnDrop(runtime_handle().spawn(async move {
            let commands = async {
                while let Some(command) = receiver.recv().await {
                    match command {
                        Command::Send(bytes, deadline, reply) => {
                            if let Err(error) = check_scope(&lifetime_cancel, run_deadline, deadline) {
                                let _ = reply.send(Err(error));
                                return;
                            }
                            let result = tokio::select! {
                                result = socket.send(Message::Text(String::from_utf8(bytes).expect("validated UTF-8").into())) => result.map_err(|_| possibly_sent(TransportFailure::Exchange("WebSocket send failed".into()))),
                                _ = deadline_elapsed(deadline) => Err(possibly_sent(TransportFailure::TimedOut)),
                            };
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed { return; }
                        }
                        Command::Receive(deadline, reply) => {
                            let result = tokio::select! {
                                result = async {
                                    loop {
                                        match socket.next().await {
                                            Some(Ok(Message::Text(text))) => return Ok(Some(text.as_bytes().to_vec())),
                                            Some(Ok(Message::Close(_))) | None => return Ok(None),
                                            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                                            _ => return Err(possibly_sent(TransportFailure::ResponseBody("invalid WebSocket message".into()))),
                                        }
                                    }
                                } => result,
                                _ = deadline_elapsed(deadline) => Err(possibly_sent(TransportFailure::TimedOut)),
                            };
                            let ended = !matches!(&result, Ok(Some(_)));
                            let _ = reply.send(result);
                            if ended { return; }
                        }
                    }
                }
            };
            // This select also covers idle time and blocked channel/socket operations.
            tokio::select! {
                _ = commands => {},
                _ = run_end(lifetime_cancel.clone(), run_deadline) => {},
            }
        }));
        Ok(Box::new(Session {
            target, path: request.path().to_owned(), headers: request.headers().to_vec(),
            cancel, run_deadline, sender, _task: owner,
        }) as Box<dyn WebSocketSession>)
    }));
    Box::pin(async move {
        (&mut task.0).await.map_err(|_| {
            definitely_unsent(TransportFailure::Connect("WebSocket task failed".into()))
        })?
    })
}

fn check_scope(
    cancel: &CancelToken,
    run: Option<Instant>,
    deadline: Instant,
) -> Result<(), TransportError> {
    if cancel.is_cancelled() {
        return Err(definitely_unsent(cancellation_failure(cancel)));
    }
    if deadline <= Instant::now() || run.is_some_and(|end| end <= Instant::now()) {
        return Err(definitely_unsent(TransportFailure::TimedOut));
    }
    Ok(())
}

async fn run_end(cancel: CancelToken, run: Option<Instant>) {
    loop {
        if cancel.is_cancelled() || run.is_some_and(|end| end <= Instant::now()) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

async fn scope_end(
    cancel: CancelToken,
    run: Option<Instant>,
    deadline: Instant,
) -> TransportFailure {
    let deadline = run.map_or(deadline, |run| run.min(deadline));
    loop {
        if let Err(error) = check_scope(&cancel, run, deadline) {
            return error.failure().clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

impl WebSocketSession for Session {
    fn send(
        &mut self,
        connect: PreparedConnect,
        request: HttpRequest,
        deadline: Instant,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            check_scope(&self.cancel, self.run_deadline, deadline)?;
            if connect.target() != &self.target
                || !connect.matches(&request)
                || request.path() != self.path
                || request.headers() != self.headers
                || request.method() != "POST"
                || request.body().len() > MAX_MESSAGE
                || std::str::from_utf8(request.body()).is_err()
            {
                return Err(definitely_unsent(TransportFailure::AuthorityMismatch));
            }
            let (reply, answer) = oneshot::channel();
            self.sender
                .send(Command::Send(request.body().to_vec(), deadline, reply))
                .await
                .map_err(|_| {
                    definitely_unsent(TransportFailure::Connect("WebSocket closed".into()))
                })?;
            answer.await.map_err(|_| {
                possibly_sent(if self.cancel.is_cancelled() {
                    cancellation_failure(&self.cancel)
                } else {
                    TransportFailure::TimedOut
                })
            })?
        })
    }

    fn receive(
        &mut self,
        deadline: Instant,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move {
            check_scope(&self.cancel, self.run_deadline, deadline)
                .map_err(|error| possibly_sent(error.failure().clone()))?;
            let (reply, answer) = oneshot::channel();
            self.sender
                .send(Command::Receive(deadline, reply))
                .await
                .map_err(|_| {
                    possibly_sent(TransportFailure::ResponseBody("WebSocket closed".into()))
                })?;
            answer.await.map_err(|_| {
                possibly_sent(if self.cancel.is_cancelled() {
                    cancellation_failure(&self.cancel)
                } else {
                    TransportFailure::TimedOut
                })
            })?
        })
    }
}
