//! Contract tests for the CLI-owned provider HTTP adapter.
//!
//! These tests stop at the CLI boundary: provider authority is prepared by a
//! real `WireGenerator`, while the adapter must spend it through one run's
//! `RunEffectPorts`. Core and tools own the lower-level authority and socket
//! semantics; this file proves that the CLI composes those seams correctly.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::ProviderHttpAdapter;
use ego_ai::generator::WireGenerator;
use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_ai::provider::{Credentials, MissingCredential, ProviderRow, ResolvedCredential, Secret};
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_core::budget::{BudgetGate, Metering};
use ego_core::cost::CurrencyCode;
use ego_core::event::id::{GenerationId, SessionId};
use ego_core::generator::attempt::GenerationAttempt;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::port::{AiEvent, Generator};
use ego_core::generator::types::{
    Conversation, GenRequest, Message, Role, StopReason, ThinkingLevel,
};
use ego_core::permit::{
    DnsResolution, HttpHeader, NetworkTarget, OneHopResponse, Resolver, TransportError,
    TransportFailure,
};
use ego_core::turn::CancelToken;
use ego_tools::exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts};
use ego_tools::store::SessionEvidenceStore;
use futures::StreamExt;
use futures::future::BoxFuture;
use url::Url;

const LOGICAL_HOST: &str = "provider.test";
const SECOND_HOST: &str = "other-provider.test";
const BODY: &[u8] = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

type ResolverObservation = (NetworkTarget, Instant);
type SharedResolverLog = Arc<Mutex<Vec<ResolverObservation>>>;

#[derive(Clone, Default)]
struct RecordingResolver {
    deadlines: SharedResolverLog,
}

impl Resolver for RecordingResolver {
    fn resolve(
        &self,
        target: NetworkTarget,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        self.deadlines
            .lock()
            .expect("resolver recording lock")
            .push((target.clone(), deadline));
        Box::pin(futures::future::ready(
            DnsResolution::new(target, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]).map_err(|_| {
                TransportError::definitely_unsent(TransportFailure::Resolution(
                    "invalid resolver fixture".to_owned(),
                ))
            }),
        ))
    }
}

struct TestCredentials;

impl Credentials for TestCredentials {
    fn secret(&self, var: &str) -> Result<Secret, MissingCredential> {
        if var == "OPENAI_API_KEY" {
            Ok(Secret::from("test-key"))
        } else {
            Err(MissingCredential {
                var: var.to_owned(),
            })
        }
    }
}

fn provider_row(host: &str, port: u16) -> ProviderRow {
    ProviderRow::openai_at(
        Url::parse(&format!("http://{host}:{port}/v1/")).expect("provider base URL"),
    )
}

fn credential(row: &ProviderRow) -> ResolvedCredential {
    row.credential
        .resolve(&TestCredentials)
        .expect("test credential")
}

fn request() -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "test-model".to_owned(),
        messages: Conversation::new(vec![Message::text(Role::User, "hello")])
            .expect("valid conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(16),
        parallel_tool_calls: false,
    }
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(8_192),
        max_output_tokens: Some(128),
        max_tools: Some(8),
        supports_parallel_tool_calls: false,
        reasoning_carryover: ReasoningCarryover::Unsupported,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

fn attempt(gate: &mut BudgetGate, request: &GenRequest, metering: &Metering) -> GenerationAttempt {
    let reservation = gate
        .reserve(metering, None, request, &capabilities())
        .expect("unlimited test budget");
    GenerationAttempt::new(
        GenerationId::new(),
        reservation,
        metering,
        None,
        request,
        &capabilities(),
    )
    .expect("attempt matches reservation")
}

struct PortFixture {
    ports: Arc<RunEffectPorts>,
    _evidence_root: tempfile::TempDir,
}

async fn run_ports(resolver: RecordingResolver) -> PortFixture {
    let workspace = std::env::current_dir().expect("workspace");
    let confinement = ego_core::permit::RunConfinement::new(
        ego_core::permit::Confinement::Off,
        false,
        ego_core::permit::CanonPath::new(&workspace),
    )
    .expect("online confinement");
    let evidence_root = tempfile::tempdir().expect("evidence directory");
    let evidence = SessionEvidenceStore::open(evidence_root.path(), SessionId::new(), 1024 * 1024)
        .expect("evidence store");
    let config = ExecRunConfig::new(
        confinement,
        evidence,
        CancelToken::new(),
        ExecLimits::default(),
        HttpCallTimeout::new(Duration::from_secs(5)).expect("call timeout"),
        None,
        Arc::new(resolver),
    );
    let (ports, _, _) = RunEffectPorts::for_run(config).await.expect("effect ports");
    PortFixture {
        ports: Arc::new(ports),
        _evidence_root: evidence_root,
    }
}

struct CaptureHttp {
    calls: Arc<Mutex<Vec<PreparedHttpCall>>>,
}

impl Http for CaptureHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(CaptureHttpCall {
            calls: Arc::clone(&self.calls),
        })
    }
}

struct CaptureHttpCall {
    calls: Arc<Mutex<Vec<PreparedHttpCall>>>,
}

impl HttpCall for CaptureHttpCall {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        self.calls.lock().expect("capture lock").push(call);
        Box::pin(futures::future::ready(Ok(OneHopResponse::response(
            200,
            vec![HttpHeader::new("content-type", b"text/event-stream".to_vec()).expect("header")],
            BODY.to_vec(),
        ))))
    }
}

async fn capture_prepared(row: ProviderRow) -> PreparedHttpCall {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let http = Arc::new(CaptureHttp {
        calls: Arc::clone(&calls),
    }) as Arc<dyn Http>;
    let generator = WireGenerator::new(row.clone(), &OpenAiCompletions, http, credential(&row));
    let req = request();
    let metering = ego_ai::catalogue::unmetered();
    let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD"));
    let _: Vec<AiEvent> = generator
        .generate(req.clone(), attempt(&mut gate, &req, &metering))
        .collect()
        .await;
    calls
        .lock()
        .expect("capture lock")
        .pop()
        .expect("prepared call")
}

fn response(status: u16, body: &[u8], extra: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(body.iter().copied())
    .collect()
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("server read timeout");
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let count = stream.read(&mut buffer).expect("request bytes");
        assert!(count > 0, "client closed before request headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).expect("request body");
        assert!(count > 0, "client closed before request body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    bytes
}

fn spawn_server(responses: Vec<Vec<u8>>) -> (u16, mpsc::Receiver<Vec<u8>>, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback listener");
    let port = listener.local_addr().expect("listener address").port();
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("HTTP connection");
            let request = read_request(&mut stream);
            sender.send(request).expect("request observation");
            stream.write_all(&response).expect("HTTP response");
            stream.flush().expect("HTTP flush");
            // A client that has already read its response and hung up makes
            // macOS answer the close with NotConnected; that is a finished
            // exchange, not a fixture failure.
            match stream.shutdown(Shutdown::Both) {
                Err(error) if error.kind() == std::io::ErrorKind::NotConnected => {}
                closed => closed.expect("HTTP close"),
            }
        }
    });
    (port, receiver, thread)
}

#[tokio::test]
async fn one_logical_call_uses_one_deadline_across_a_redirect() {
    let responses = vec![
        response(307, b"", "Location: /final\r\n"),
        response(200, BODY, "Content-Type: text/event-stream\r\n"),
    ];
    let (port, requests, server) = spawn_server(responses);
    let resolver = RecordingResolver::default();
    let deadlines = Arc::clone(&resolver.deadlines);
    let ports = run_ports(resolver).await;
    let adapter = Arc::new(ProviderHttpAdapter::new(Arc::clone(&ports.ports))) as Arc<dyn Http>;
    let provider = provider_row(LOGICAL_HOST, port);
    let provider_credential = credential(&provider);
    let generator = WireGenerator::new(provider, &OpenAiCompletions, adapter, provider_credential);
    let req = request();
    let metering = ego_ai::catalogue::unmetered();
    let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD"));
    let events: Vec<AiEvent> = generator
        .generate(req.clone(), attempt(&mut gate, &req, &metering))
        .collect()
        .await;

    assert!(
        matches!(events.last(), Some(AiEvent::Stop(StopReason::Stop))),
        "{events:?}"
    );
    let observed = deadlines.lock().expect("resolver recording lock");
    assert_eq!(observed.len(), 2, "one DNS effect per admitted hop");
    assert!(
        observed
            .iter()
            .all(|(_, deadline)| *deadline > Instant::now())
    );
    assert_eq!(
        observed[0].1, observed[1].1,
        "one logical deadline across redirect"
    );
    let first = requests
        .recv_timeout(Duration::from_secs(2))
        .expect("first request");
    let second = requests
        .recv_timeout(Duration::from_secs(2))
        .expect("redirect request");
    assert!(
        first
            .windows(b"/v1/chat/completions".len())
            .any(|window| window == b"/v1/chat/completions")
    );
    assert!(
        second
            .windows(b"/final".len())
            .any(|window| window == b"/final")
    );
    server.join().expect("server thread");
}

#[tokio::test]
async fn one_call_handle_rejects_a_changed_profile_before_a_second_effect() {
    let (port, requests, server) = spawn_server(vec![response(200, b"ok", "")]);
    let resolver = RecordingResolver::default();
    let deadlines = Arc::clone(&resolver.deadlines);
    let ports = run_ports(resolver).await;
    let adapter = ProviderHttpAdapter::new(Arc::clone(&ports.ports));
    let first = capture_prepared(provider_row(LOGICAL_HOST, port)).await;
    let second = capture_prepared(provider_row(SECOND_HOST, port)).await;
    let mut call = adapter.begin_call();
    let _ = call
        .execute(first)
        .await
        .expect("first profile is admitted");
    let error = call
        .execute(second)
        .await
        .expect_err("profile substitution refused");
    assert!(matches!(
        error.failure(),
        TransportFailure::AuthorityMismatch
    ));
    assert_eq!(
        error.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent
    );
    assert_eq!(deadlines.lock().expect("resolver recording lock").len(), 1);
    requests
        .recv_timeout(Duration::from_secs(2))
        .expect("first request");
    assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
    server.join().expect("server thread");
}

#[tokio::test]
async fn codex_websocket_tool_result_uses_incremental_input_through_run_authority() {
    use ego_ai::wire::openai_codex_responses::OpenAiCodexResponses;
    use ego_core::generator::types::{ContentBlock, PromptCache};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = async_tungstenite::tokio::accept_async(tcp).await.unwrap();
        let first: serde_json::Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(first["type"], "response.create");
        assert_eq!(first["input"].as_array().unwrap().len(), 1);
        let tool = serde_json::json!({"type":"function_call","id":"fc1","call_id":"c1","name":"read","arguments":"{}"});
        for event in [
            serde_json::json!({"type":"response.output_item.added","item":tool}),
            serde_json::json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[tool]}}),
        ] {
            socket
                .send(async_tungstenite::tungstenite::Message::Text(
                    event.to_string().into(),
                ))
                .await
                .unwrap();
        }
        let second: serde_json::Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(second["previous_response_id"], "resp_1");
        assert_eq!(
            second["input"],
            serde_json::json!([{"type":"function_call_output","call_id":"c1","output":"ok"}])
        );
        let event = serde_json::json!({"type":"response.completed","response":{"id":"resp_2","status":"completed","output":[]}});
        socket
            .send(async_tungstenite::tungstenite::Message::Text(
                event.to_string().into(),
            ))
            .await
            .unwrap();
    });
    let resolver = RecordingResolver::default();
    let observations = resolver.deadlines.clone();
    let ports = run_ports(resolver).await;
    let adapter = Arc::new(ProviderHttpAdapter::new(ports.ports.clone())) as Arc<dyn Http>;
    let row = provider_row(LOGICAL_HOST, port);
    let credential = credential(&row);
    let generator = WireGenerator::new(row, &OpenAiCodexResponses, adapter, credential);
    let mut req = request();
    let id = SessionId::new();
    req.cache = Some(PromptCache {
        session_id: id,
        lineage_root: id,
        retention: CacheRetention::Short,
        stable_messages: 0,
    });
    let metering = ego_ai::catalogue::unmetered();
    let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").unwrap());
    let events: Vec<_> = generator
        .generate(req.clone(), attempt(&mut gate, &req, &metering))
        .collect()
        .await;
    assert!(
        matches!(events.last(), Some(AiEvent::Stop(StopReason::ToolUse))),
        "{events:?}"
    );
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "hello"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                call_id: "c1".into(),
                name: "read".into(),
                args: "{}".into(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "c1".into(),
                output: "ok".into(),
                is_error: false,
            }],
        },
    ])
    .unwrap();
    let events: Vec<_> = generator
        .generate(req.clone(), attempt(&mut gate, &req, &metering))
        .collect()
        .await;
    assert!(
        matches!(events.last(), Some(AiEvent::Stop(StopReason::Stop))),
        "{events:?}"
    );
    assert_eq!(
        observations.lock().unwrap().len(),
        1,
        "reuse the admitted socket without new DNS"
    );
    server.await.unwrap();
}

mod codex_real_recovery {
    use super::*;
    use ego_ai::http::HttpWebSocket;
    use ego_ai::wire::openai_codex_responses::OpenAiCodexResponses;
    use ego_core::budget::SharedBudgetGate;
    use ego_core::config::{Mode, ModelRef, Ruleset};
    use ego_core::event::domain::ExitReason;
    use ego_core::event::time::Clock;
    use ego_core::evidence::{EvidenceAdmissionService, RunInvocationId};
    use ego_core::generation_model_source::{
        GenerationModelError, GenerationModelRequest, GenerationModelSource, GenerationProfile,
    };
    use ego_core::generator::attempt::DeliveryCertainty;
    use ego_core::permit::CanonPath;
    use ego_core::public::PublicEvent;
    use ego_core::sink::CollectingSink;
    use ego_core::state::SessionState;
    use ego_core::turn::{
        GenerationRuntime, RetryPause, Session, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
    };
    use ego_tools::clock::SystemClock;
    use ego_tools::store::{JsonlStore, LeaseInputs, OsIo};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Observation only: all admission, bytes, errors and delivery come from the
    // real CLI adapter and ego-tools transport; no fault is injected here.
    struct ObservedHttp {
        real: ProviderHttpAdapter,
        opens: AtomicUsize,
        http_calls: AtomicUsize,
        trace: Arc<Mutex<Vec<String>>>,
    }
    struct ObservedSocket {
        real: Box<dyn HttpWebSocket>,
        trace: Arc<Mutex<Vec<String>>>,
    }
    impl Http for ObservedHttp {
        fn begin_call(&self) -> Box<dyn HttpCall> {
            self.http_calls.fetch_add(1, Ordering::SeqCst);
            self.real.begin_call()
        }
        fn open_websocket(
            &self,
            call: PreparedHttpCall,
        ) -> BoxFuture<'static, Result<Box<dyn HttpWebSocket>, TransportError>> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            let opened = self.real.open_websocket(call);
            let trace = self.trace.clone();
            Box::pin(async move {
                Ok(Box::new(ObservedSocket {
                    real: opened.await?,
                    trace,
                }) as Box<dyn HttpWebSocket>)
            })
        }
    }
    impl HttpWebSocket for ObservedSocket {
        fn send(&mut self, call: PreparedHttpCall) -> BoxFuture<'_, Result<(), TransportError>> {
            Box::pin(async move {
                let result = self.real.send(call).await;
                self.trace.lock().unwrap().push(match &result {
                    Ok(()) => "send:ok".into(),
                    Err(error) => {
                        assert_eq!(
                            error.delivery(),
                            DeliveryCertainty::PossiblySent,
                            "real idle close must not authorize an in-attempt replay"
                        );
                        format!("send:{:?}", error.delivery())
                    }
                });
                result
            })
        }
        fn receive(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
            Box::pin(async move {
                let result = self.real.receive().await;
                match &result {
                    Ok(None) => self.trace.lock().unwrap().push("receive:eof".into()),
                    Err(error) => {
                        assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
                        self.trace
                            .lock()
                            .unwrap()
                            .push(format!("receive:{:?}", error.delivery()));
                    }
                    _ => {}
                }
                result
            })
        }
    }
    struct Source(Arc<WireGenerator>);
    impl GenerationModelSource for Source {
        fn resolve<'a>(
            &'a self,
            _: GenerationModelRequest,
        ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
            Box::pin(async move {
                Ok(GenerationProfile {
                    output_limit: None,
                    identity: ModelRef("openai-codex/test-model".into()),
                    request_model: "test-model".into(),
                    thinking: ThinkingLevel::Off,
                    generator: self.0.clone(),
                    capabilities: capabilities(),
                    metering: ego_ai::catalogue::unmetered(),
                    rates: None,
                })
            })
        }
    }
    struct Pause;
    impl RetryPause for Pause {
        fn pause(&self, delay: Duration) -> BoxFuture<'static, ()> {
            Box::pin(tokio::time::sleep(delay))
        }
    }

    async fn exercise_peer_drop(idle_close: bool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            use async_tungstenite::tungstenite::Message as WsMessage;
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = async_tungstenite::tokio::accept_async(tcp).await.unwrap();
            let first: Value =
                serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            socket.send(WsMessage::Text(json!({"type":"response.completed", "response":{"id":"resp_old", "status":"completed", "output":[]}}).to_string().into())).await.unwrap();
            let incremental = if idle_close {
                socket.send(WsMessage::Close(None)).await.unwrap();
                None
            } else {
                let request: Value =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(request["previous_response_id"], "resp_old");
                // A real frame with no visible output, then TCP loss without
                // a terminal response. It must go to the turn retry owner.
                socket
                    .send(WsMessage::Text(
                        json!({"type":"response.created", "response":{"id":"resp_dropped"}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                Some(request)
            };
            drop(socket);
            closed_tx.send(()).unwrap();
            let (tcp, _) = listener.accept().await.unwrap();
            let mut recovered = async_tungstenite::tokio::accept_async(tcp).await.unwrap();
            let full: Value =
                serde_json::from_str(recovered.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert!(
                full.get("previous_response_id").is_none(),
                "lost socket ID leaked"
            );
            let input = full["input"].as_array().unwrap();
            assert!(
                input.starts_with(first["input"].as_array().unwrap()),
                "recovery lost original input"
            );
            assert!(input.len() > first["input"].as_array().unwrap().len());
            assert!(full.to_string().contains("second turn"));
            recovered
                .send(WsMessage::Text(
                    json!({"type":"response.output_text.delta", "delta":"OK"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            recovered.send(WsMessage::Text(json!({"type":"response.completed", "response":{"id":"resp_final", "status":"completed", "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"OK"}]}]}}).to_string().into())).await.unwrap();
            incremental.is_some()
        });
        let ports = run_ports(RecordingResolver::default()).await;
        let observed = Arc::new(ObservedHttp {
            real: ProviderHttpAdapter::new(ports.ports.clone()),
            opens: AtomicUsize::new(0),
            http_calls: AtomicUsize::new(0),
            trace: Default::default(),
        });
        let row = provider_row(LOGICAL_HOST, port);
        let generator = Arc::new(WireGenerator::new(
            row.clone(),
            &OpenAiCodexResponses,
            observed.clone(),
            credential(&row),
        ));
        let root = tempfile::tempdir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let store = JsonlStore::new(root.path(), &cwd, Box::new(OsIo));
        let id = SessionId::new();
        let clock = SystemClock;
        let opened = store
            .open_writer(
                &id,
                LeaseInputs {
                    pid: std::process::id(),
                    now: clock.now(),
                    stale_after: Duration::from_secs(60),
                    is_alive: &|_| true,
                },
            )
            .unwrap();
        let evidence = SessionEvidenceStore::open(root.path(), id, 1024 * 1024).unwrap();
        let sink = CollectingSink::new();
        let policy = crate::repl::policy_controls::CliGenerationPolicySource::new(
            crate::repl::policy_controls::GenerationPolicySelection::new(
                Mode::Yolo,
                Ruleset::default(),
            ),
            CanonPath::new(&cwd),
            None,
        );
        let admission = Arc::new(EvidenceAdmissionService::new());
        let invocation = RunInvocationId::new();
        admission
            .admit_run_context(
                &opened.writer,
                ego_core::evidence::RunContextEvidence::new(
                    id,
                    invocation,
                    ego_core::run::RunId::new(id, invocation.uuid()),
                    ego_core::evidence::ConfigProvenance::default(),
                ),
            )
            .unwrap();
        let runtime = GenerationRuntime {
            source: Arc::new(Source(generator)),
            policy_source: Arc::new(policy),
            gate: SharedBudgetGate::new(BudgetGate::unlimited(CurrencyCode::new("USD").unwrap())),
            config: TurnConfig {
                cache: CacheRetention::Short,
                system: None,
                identity: None,
                token_ceiling: None,
                retry: 1,
                fallbacks: vec![],
            },
            admission,
            invocation,
            retry_pause: Arc::new(Pause),
        };
        let effects = ToolEffectPorts::new(
            &ego_tools::fs::OsFs,
            ports.ports.process(),
            &evidence,
            ports.ports.resolver_handle(),
            ports.ports.one_hop_handle(),
            ports.ports.duplex_handle(),
        );
        let tools = Tools::new(&[], effects, CanonPath::new(&cwd));
        let mut turn = TurnLoop::new(
            Session {
                id,
                state: SessionState::default(),
                store: &opened.writer,
                evidence: &opened.writer,
                clock: &clock,
                sink: &sink,
                cancel: CancelToken::new(),
            },
            runtime,
            tools,
        )
        .unwrap();
        let first = turn.run("first turn").await.unwrap();
        assert_eq!(first.exit_reason, ExitReason::Completed);
        if idle_close {
            closed_rx.await.unwrap();
        }
        let second = turn.run("second turn").await.unwrap();
        assert_eq!(second.exit_reason, ExitReason::Completed, "{second:?}");
        assert!(second.ok);
        let retries = sink
            .events()
            .iter()
            .filter(|e| matches!(e, PublicEvent::ProviderRetry { .. }))
            .count();
        assert_eq!(
            retries, 1,
            "must recover through policy, not inside a possibly-sent attempt"
        );
        assert_eq!(observed.opens.load(Ordering::SeqCst), 2);
        assert_eq!(observed.http_calls.load(Ordering::SeqCst), 0);
        assert_eq!(server.await.unwrap(), !idle_close);
        let trace = observed.trace.lock().unwrap().clone();
        assert_eq!(
            trace
                .iter()
                .filter(|event| event.starts_with("send:"))
                .count(),
            3,
            "one initial request, one failed reused request, one policy retry"
        );
        let state = turn.into_state().unwrap();
        assert!(state.live_attempts.is_empty(), "every attempt must settle");
        assert_eq!(state.settled_attempts.len(), 3);
    }

    // Catches: a real peer-closed idle socket or no-output midstream drop ends
    // the turn, is replayed inside one possibly-sent attempt, or leaks its ID.
    #[tokio::test]
    async fn codex_real_transport_idle_close_and_midstream_drop_finish_via_policy_retry() {
        tokio::time::timeout(Duration::from_secs(25), async {
            for idle_close in [true, false] {
                exercise_peer_drop(idle_close).await;
            }
        })
        .await
        .expect("bounded real WebSocket recovery");
    }
}

mod proxy_tests;
