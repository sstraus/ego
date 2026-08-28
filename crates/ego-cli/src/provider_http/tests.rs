//! RED contract for the CLI-owned provider HTTP adapter.
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
use ego_core::budget::{BudgetGate, Metering};
use ego_core::cost::CurrencyCode;
use ego_core::event::id::{GenerationId, SessionId};
use ego_core::generator::attempt::GenerationAttempt;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
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

type ResolverObservation = (NetworkTarget, Option<Instant>);
type SharedResolverLog = Arc<Mutex<Vec<ResolverObservation>>>;

#[derive(Clone, Default)]
struct RecordingResolver {
    deadlines: SharedResolverLog,
}

impl Resolver for RecordingResolver {
    fn resolve(
        &self,
        target: NetworkTarget,
        deadline: Option<Instant>,
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
        schema_version: 1,
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
    let (ports, _) = RunEffectPorts::for_run(config).await.expect("effect ports");
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
    let generator = WireGenerator::new(row.clone(), http, credential(&row));
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
            stream.shutdown(Shutdown::Both).expect("HTTP close");
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
    let generator = WireGenerator::new(provider, adapter, provider_credential);
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
    assert!(observed.iter().all(|(_, deadline)| deadline.is_some()));
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
