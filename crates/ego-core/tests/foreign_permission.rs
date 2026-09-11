//! contract for regression case guarded HTTP and one-child duplex authority.

use std::ffi::OsString;
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use futures::StreamExt;
use futures::future::{self, BoxFuture};
use futures::io::{AsyncRead, AsyncWrite};

use ego_core::config::{ConfigLayer, Mode, Ruleset, Source};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject};
use ego_core::permit::{
    CanonPath, DuplexHandle, DuplexSession, DuplexSpawner, EnvPolicy, Fs, FsError, GuardedEffects,
    GuardedHttp, HttpHeader, HttpRequest, HttpRequestDigest, HttpRoute, HttpScheme, NetworkTarget,
    OneHop, OneHopPayload, OneHopResponse, PathResolver, PolicyEngine, PreparedCall,
    PreparedConnect, ProcessError, ProcessExecutor, Resolver, Scan, ScanRequest, SpawnSpec,
    Subject, ToolResult, TransportError, TransportFailure,
};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};

mod common;

fn rules(allow: &[&str], deny: &[&str]) -> Ruleset {
    let render = |values: &[&str]| {
        values
            .iter()
            .map(|value| format!("{value:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let permissions = ConfigLayer::parse(
        Source::Project,
        &format!(
            "[permissions]\nallow=[{}]\ndeny=[{}]",
            render(allow),
            render(deny)
        ),
    )
    .expect("permission fixture")
    .value
    .permissions
    .expect("permissions");
    Ruleset::from_layer(permissions, Source::Project)
}

fn digest(value: &str) -> ego_core::permit::InvocationDigest {
    let hex = match value {
        "args-a" => "0000000000000000000000000000000000000000000000000000000000000001",
        "args-b" => "0000000000000000000000000000000000000000000000000000000000000002",
        "outer" => "0000000000000000000000000000000000000000000000000000000000000003",
        "inner" => "0000000000000000000000000000000000000000000000000000000000000004",
        _ => "0000000000000000000000000000000000000000000000000000000000000005",
    };
    ego_core::permit::InvocationDigest::parse(&format!("sha256:{hex}")).expect("canonical digest")
}

fn tool(server: &str, name: &str) -> ego_core::permit::ForeignToolId {
    ego_core::permit::ForeignToolId::new(server, name).expect("foreign identity")
}

fn request() -> HttpRequest {
    HttpRequest::new(
        "GET",
        "/",
        vec![HttpHeader::new("x-trace", b"one".to_vec()).expect("header")],
        b"body".to_vec(),
    )
    .expect("request")
}

fn route(target: &str) -> HttpRoute {
    HttpRoute::new(
        HttpScheme::Https,
        NetworkTarget::parse(target).expect("network target"),
    )
    .expect("route")
}

fn http_call(
    server: &str,
    name: &str,
    args: &str,
    target: &str,
    req: &HttpRequest,
) -> PreparedCall {
    let route = route(target);
    PreparedCall::foreign_http(
        tool(server, name),
        digest(args),
        ego_core::permit::ForeignTransportTarget::http(route),
        req,
    )
}

fn stdio_call(server: &str, argv: Vec<OsString>) -> PreparedCall {
    let spec = SpawnSpec::new(argv, None, None, EnvPolicy::Inherited).expect("spawn spec");
    PreparedCall::foreign_stdio(tool(server, "execute"), digest("args-a"), server, spec)
}

fn mcp_call(subject: McpSubject, parameters: serde_json::Value) -> PreparedCall {
    PreparedCall::mcp(
        subject,
        McpOperationKind::CallTool,
        vec![(tool("srv", "search"), digest("args-a"))],
        McpAdmissionFingerprint::new("capture-a", "search", "schema-a").expect("admission"),
        "profile-a",
        ego_core::permit::ForeignTransportTarget::http(route("api.example.test:443")),
        &parameters,
    )
    .expect("MCP call")
}

#[derive(Clone, Default)]
struct FakeResolver {
    answer: Arc<Mutex<Option<ego_core::permit::DnsResolution>>>,
    calls: Arc<Mutex<usize>>,
    deadlines: Arc<Mutex<Vec<Instant>>>,
}
impl FakeResolver {
    fn one(target: NetworkTarget, ip: &str) -> Self {
        Self {
            answer: Arc::new(Mutex::new(Some(
                ego_core::permit::DnsResolution::new(target, vec![ip.parse().expect("IP")])
                    .expect("DNS"),
            ))),
            calls: Arc::new(Mutex::new(0)),
            deadlines: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn calls(&self) -> usize {
        *self.calls.lock().expect("calls")
    }

    fn deadlines(&self) -> Vec<Instant> {
        self.deadlines.lock().expect("deadlines").clone()
    }
}
impl Resolver for FakeResolver {
    fn resolve(
        &self,
        _target: NetworkTarget,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, TransportError>> {
        *self.calls.lock().expect("calls") += 1;
        self.deadlines.lock().expect("deadlines").push(deadline);
        Box::pin(future::ready(
            self.answer.lock().expect("answer").take().ok_or_else(|| {
                TransportError::definitely_unsent(TransportFailure::Resolution(
                    "DNS evidence exhausted".into(),
                ))
            }),
        ))
    }
}

type ObservedConnect = (
    HttpRoute,
    NetworkTarget,
    std::net::IpAddr,
    HttpRequestDigest,
);

#[derive(Clone, Default)]
struct FakeOneHop {
    calls: Arc<Mutex<usize>>,
    observed: Arc<Mutex<Option<ObservedConnect>>>,
    failure: Arc<Mutex<Option<TransportError>>>,
    deadlines: Arc<Mutex<Vec<Instant>>>,
}
impl OneHop for FakeOneHop {
    fn admit_mcp(&self) -> Result<(), TransportError> {
        Ok(())
    }

    fn begin_call(&self) -> Instant {
        Instant::now() + std::time::Duration::from_secs(60)
    }
    fn execute(
        &self,
        connect: ego_core::permit::ResolvedConnect,
        request: HttpRequest,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<ego_core::permit::OneHopResponse, TransportError>> {
        *self.calls.lock().expect("calls") += 1;
        self.deadlines.lock().expect("deadlines").push(deadline);
        assert_eq!(connect.request_digest(), &HttpRequestDigest::of(&request));
        *self.observed.lock().expect("observed") = Some((
            connect.route().clone(),
            connect.target().clone(),
            connect.selected(),
            connect.request_digest().clone(),
        ));
        if let Some(error) = self.failure.lock().expect("failure").take() {
            return Box::pin(future::ready(Err(error)));
        }
        Box::pin(future::ready(Ok(
            ego_core::permit::OneHopResponse::response(200, vec![], vec![]),
        )))
    }
}

impl FakeOneHop {
    fn deadlines(&self) -> Vec<Instant> {
        self.deadlines.lock().expect("deadlines").clone()
    }
}

#[derive(Default)]
struct FakeSession {
    input: Mutex<Vec<u8>>,
    output: Mutex<Vec<u8>>,
    delivery: Mutex<ego_core::generator::attempt::DeliveryCertainty>,
}
impl AsyncRead for FakeSession {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut input = self.input.lock().expect("input");
        let count = input.len().min(buf.len());
        buf[..count].copy_from_slice(&input[..count]);
        input.drain(..count);
        Poll::Ready(Ok(count))
    }
}
impl AsyncWrite for FakeSession {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.output.lock().expect("output").extend_from_slice(buf);
        *self.delivery.lock().expect("delivery") =
            ego_core::generator::attempt::DeliveryCertainty::PossiblySent;
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
impl DuplexSession for FakeSession {
    fn delivery(&self) -> ego_core::generator::attempt::DeliveryCertainty {
        *self.delivery.lock().expect("delivery")
    }
    fn settle(self: Box<Self>) -> BoxFuture<'static, ego_core::permit::DuplexSettlement> {
        let delivery = self.delivery();
        Box::pin(future::ready(ego_core::permit::DuplexSettlement::new(
            ego_core::permit::DuplexEnd::Exited { code: 0 },
            ego_core::permit::CleanupCertainty::Complete,
            delivery,
            Err(EvidenceError::Limit { limit: 0 }),
        )))
    }
    fn interrupt(self: Box<Self>) -> BoxFuture<'static, ego_core::permit::DuplexSettlement> {
        let delivery = self.delivery();
        Box::pin(future::ready(ego_core::permit::DuplexSettlement::new(
            ego_core::permit::DuplexEnd::Interrupted,
            ego_core::permit::CleanupCertainty::Complete,
            delivery,
            Err(EvidenceError::Limit { limit: 0 }),
        )))
    }
}

#[derive(Clone, Default)]
struct FakeSpawner {
    specs: Arc<Mutex<Vec<SpawnSpec>>>,
}
impl DuplexSpawner for FakeSpawner {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        self.specs.lock().expect("spawn specs").push(spec);
        Box::pin(future::ready(Ok(Box::new(FakeSession {
            input: Mutex::new(b"child-bytes".to_vec()),
            ..Default::default()
        }) as DuplexHandle)))
    }
}

#[derive(Default)]
struct MemoryFs;
impl PathResolver for MemoryFs {
    fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError> {
        Ok(CanonPath::new(path.to_path_buf()))
    }
}
impl Fs for MemoryFs {
    fn read(&self, _: &CanonPath) -> Result<Vec<u8>, FsError> {
        Err(FsError::NotFound {
            path: CanonPath::new("missing"),
        })
    }
    fn write(&self, _: &CanonPath, _: &[u8]) -> Result<(), FsError> {
        Ok(())
    }
    fn scan(&self, _: &ScanRequest) -> Result<Scan, FsError> {
        Ok(Scan::new(vec![], false))
    }
}
struct NoProcess;
impl ProcessExecutor for NoProcess {
    fn execute<'a>(&'a self, _: &'a SpawnSpec) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(future::ready(Err(ProcessError::Invariant(
            "not admitted".into(),
        ))))
    }
}
struct NoEvidence;
impl EvidenceStore for NoEvidence {
    fn blob_store(&self) -> &dyn ego_core::store::blob::BlobStore {
        &common::EMPTY_BLOB_STORE
    }

    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Err(EvidenceError::Limit { limit: 0 })
    }
    fn read_page<'a>(
        &'a self,
        _: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(future::ready(Err(EvidenceError::Limit { limit: 0 })))
    }
}

#[test]
fn foreign_tool_allow_covers_http_but_collapsed_dispatch_still_needs_outer_tool() {
    let req = request();
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)"], &[]),
    );
    assert!(matches!(
        policy.classify(http_call(
            "srv",
            "search",
            "args-a",
            "api.example.test:443",
            &req
        )),
        ego_core::permit::Classification::Settled(Ok(_))
    ));
    let outer = PreparedCall::foreign_dispatch(
        tool("router", "call_tool"),
        digest("outer"),
        tool("srv", "search"),
        digest("inner"),
        ego_core::permit::ForeignTransportTarget::http(route("api.example.test:443")),
        &req,
    );
    assert!(matches!(
        policy.classify(outer),
        ego_core::permit::Classification::Ask(_)
    ));
    let denied = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)"], &["tool(srv/search)"]),
    );
    assert!(matches!(
        denied.classify(http_call(
            "srv",
            "search",
            "args-a",
            "api.example.test:443",
            &req
        )),
        ego_core::permit::Classification::Settled(Err(_))
    ));
}

#[test]
fn missing_foreign_tool_authority_asks_even_when_transport_is_allowed() {
    let req = request();
    let http_policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["net(api.example.test:443)"], &[]),
    );
    assert!(matches!(
        http_policy.classify(http_call(
            "srv",
            "search",
            "args-a",
            "api.example.test:443",
            &req
        )),
        ego_core::permit::Classification::Ask(_)
    ));

    let stdio_policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["exec(server --mcp)"], &[]),
    );
    assert!(matches!(
        stdio_policy.classify(stdio_call("server", vec!["server".into(), "--mcp".into()])),
        ego_core::permit::Classification::Ask(_)
    ));
}

#[test]
fn foreign_tool_allow_covers_stdio_without_exec_allowance() {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(server/execute)"], &[]),
    );

    assert!(matches!(
        policy.classify(stdio_call(
            "server",
            vec![OsString::from("server"), OsString::from("--mcp")],
        )),
        ego_core::permit::Classification::Settled(Ok(_))
    ));
}

#[test]
fn guarded_mcp_rejects_changed_subject_without_spending_authority() {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)", "net(api.example.test:443)"], &[]),
    );
    let authorized = match policy.classify(mcp_call(
        McpSubject::Tool {
            server: "srv".into(),
            name: "search".into(),
        },
        serde_json::json!({"name":"search","arguments":{"q":"one"}}),
    )) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("{other:?}"),
    };
    let (call, permit) = authorized.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        Arc::new(FakeResolver::default()),
        Arc::new(FakeOneHop::default()),
        Arc::new(FakeSpawner::default()),
    );
    let changed = mcp_call(
        McpSubject::Tool {
            server: "srv".into(),
            name: "other".into(),
        },
        serde_json::json!({"name":"search","arguments":{"q":"one"}}),
    );
    let error = match guarded.claim_mcp_http(changed) {
        Ok(_) => panic!("changed subject was accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error.failure(),
        TransportFailure::AuthorityMismatch
    ));
    guarded
        .claim_mcp_http(call)
        .expect("original remains usable");
}

#[test]
fn guarded_mcp_rejects_changed_parameters_without_spending_authority() {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)", "net(api.example.test:443)"], &[]),
    );
    let authorized = match policy.classify(mcp_call(
        McpSubject::Tool {
            server: "srv".into(),
            name: "search".into(),
        },
        serde_json::json!({"name":"search","arguments":{"q":"one"}}),
    )) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("{other:?}"),
    };
    let (call, permit) = authorized.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        Arc::new(FakeResolver::default()),
        Arc::new(FakeOneHop::default()),
        Arc::new(FakeSpawner::default()),
    );
    let changed = mcp_call(
        McpSubject::Tool {
            server: "srv".into(),
            name: "search".into(),
        },
        serde_json::json!({"name":"search","arguments":{"q":"two"}}),
    );
    let error = match guarded.claim_mcp_http(changed) {
        Ok(_) => panic!("changed parameters were accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error.failure(),
        TransportFailure::AuthorityMismatch
    ));
    guarded
        .claim_mcp_http(call)
        .expect("original remains usable");
}

#[test]
fn explicit_matching_transport_denies_override_foreign_tool_allow() {
    let req = request();
    let denied_http = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)"], &["net(api.example.test:443)"]),
    );
    assert!(matches!(
        denied_http.classify(http_call(
            "srv",
            "search",
            "args-a",
            "api.example.test:443",
            &req
        )),
        ego_core::permit::Classification::Settled(Err(_))
    ));

    let denied_stdio = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(server/execute)"], &["exec(server --mcp)"]),
    );
    assert!(matches!(
        denied_stdio.classify(stdio_call("server", vec!["server".into(), "--mcp".into()])),
        ego_core::permit::Classification::Settled(Err(_))
    ));
}

#[test]
fn native_spawn_same_executable_is_not_covered_by_foreign_tool_allow() {
    let spec = SpawnSpec::new(
        vec![OsString::from("server"), OsString::from("--mcp")],
        None,
        None,
        EnvPolicy::Inherited,
    )
    .expect("spawn spec");
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(server/execute)"], &[]),
    );
    assert!(matches!(
        policy.classify(PreparedCall::new(
            "native",
            ego_core::permit::Op::Spawn { spec },
        )),
        ego_core::permit::Classification::Ask(_)
    ));
}

#[test]
fn guarded_http_consumes_one_dns_answer_and_rejects_reuse_as_authority_spent() {
    let req = request();
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)"], &[]),
    );
    let authorized = match policy.classify(http_call(
        "srv",
        "search",
        "args-a",
        "api.example.test:443",
        &req,
    )) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("expected authorization, got {other:?}"),
    };
    let (call, permit) = authorized.into_parts();
    let resolver = Arc::new(FakeResolver::one(
        NetworkTarget::parse("api.example.test:443").expect("target"),
        "192.0.2.10",
    ));
    let one_hop = Arc::new(FakeOneHop::default());
    let spawner = Arc::new(FakeSpawner::default());
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        resolver.clone(),
        one_hop.clone(),
        spawner.clone(),
    );
    let first = futures::executor::block_on(guarded.guarded_http(call.clone(), req.clone()))
        .expect("first hop");
    assert_eq!(first.status(), 200);
    assert_eq!(
        first.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::PossiblySent
    );
    assert_eq!(resolver.calls(), 1);
    let first_deadline = resolver.deadlines()[0];
    assert!(first_deadline > Instant::now());
    assert_eq!(one_hop.deadlines(), vec![first_deadline]);
    let second =
        futures::executor::block_on(guarded.guarded_http(call, req)).expect_err("spent authority");
    assert!(matches!(second.failure(), TransportFailure::AuthoritySpent));
    assert_eq!(
        second.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent
    );
    assert_eq!(resolver.calls(), 1);
    let observed = one_hop
        .observed
        .lock()
        .expect("observed")
        .clone()
        .expect("connect");
    assert_eq!(observed.0, route("api.example.test:443"));
    assert_eq!(
        observed.1,
        NetworkTarget::parse("api.example.test:443").expect("target")
    );
    assert_eq!(
        observed.2,
        "192.0.2.10".parse::<std::net::IpAddr>().expect("IP")
    );
    assert_eq!(observed.3, HttpRequestDigest::of(&request()));
}

#[test]
fn guarded_http_passes_one_deadline_to_dns_and_hop_and_skips_dns_for_literal() {
    let target = NetworkTarget::parse("api.example.test:443").expect("target");
    let named_route = route("api.example.test:443");
    let req = request();
    let connect = PreparedConnect::new(target.clone(), &req);
    let resolver = Arc::new(FakeResolver::one(target, "192.0.2.10"));
    let one_hop = Arc::new(FakeOneHop::default());
    let guarded = GuardedHttp::new(resolver.clone(), one_hop.clone());
    let deadline = Instant::now() + std::time::Duration::from_secs(30);
    futures::executor::block_on(guarded.execute_admitted(named_route, connect, req, deadline))
        .expect("named target HTTP");
    assert_eq!(resolver.deadlines(), vec![deadline]);
    assert_eq!(one_hop.deadlines(), vec![deadline]);

    let literal_target = NetworkTarget::parse("127.0.0.1:443").expect("literal target");
    let literal_request = HttpRequest::get("/").expect("literal request");
    let literal_connect = PreparedConnect::new(literal_target, &literal_request);
    let literal_resolver = Arc::new(FakeResolver::default());
    let literal_one_hop = Arc::new(FakeOneHop::default());
    let literal_guarded = GuardedHttp::new(literal_resolver.clone(), literal_one_hop.clone());
    futures::executor::block_on(literal_guarded.execute_admitted(
        route("127.0.0.1:443"),
        literal_connect,
        literal_request,
        deadline,
    ))
    .expect("literal target HTTP");
    assert_eq!(literal_resolver.calls(), 0);
    assert!(literal_resolver.deadlines().is_empty());
    assert_eq!(literal_one_hop.deadlines(), vec![deadline]);
}

#[test]
fn http_request_digest_changes_for_every_wire_significant_component() {
    let base = HttpRequest::new(
        "GET",
        "/",
        vec![
            HttpHeader::new("x-first", b"one".to_vec()).expect("header"),
            HttpHeader::new("x-second", b"two".to_vec()).expect("header"),
        ],
        b"body".to_vec(),
    )
    .expect("request");
    let variants = [
        HttpRequest::new("POST", "/", base.headers().to_vec(), base.body().to_vec())
            .expect("method"),
        HttpRequest::new(
            "GET",
            "/?changed=1",
            base.headers().to_vec(),
            base.body().to_vec(),
        )
        .expect("path"),
        HttpRequest::new(
            "GET",
            "/",
            vec![HttpHeader::new("x-other", b"one".to_vec()).expect("header")],
            base.body().to_vec(),
        )
        .expect("header name"),
        HttpRequest::new(
            "GET",
            "/",
            vec![base.headers()[1].clone(), base.headers()[0].clone()],
            base.body().to_vec(),
        )
        .expect("header order"),
        HttpRequest::new(
            "GET",
            "/",
            vec![
                HttpHeader::new("x-trace", b"one".to_vec()).expect("header"),
                HttpHeader::new("x-trace", b"two".to_vec()).expect("header"),
            ],
            base.body().to_vec(),
        )
        .expect("repeated value"),
        HttpRequest::new("GET", "/", base.headers().to_vec(), b"changed".to_vec()).expect("body"),
    ];
    for variant in variants {
        assert_ne!(
            HttpRequestDigest::of(&base),
            HttpRequestDigest::of(&variant)
        );
    }
    let connect = PreparedConnect::new(
        NetworkTarget::parse("api.example.test:443").expect("target"),
        &base,
    );
    assert_eq!(connect.request_digest(), &HttpRequestDigest::of(&base));
    let subject = Subject::Network(NetworkTarget::parse("api.example.test:443").expect("target"));
    let rendered = serde_json::to_string(&subject).expect("subject");
    assert!(!rendered.contains("x-first"));
    let digest_debug = format!("{:?}", HttpRequestDigest::of(&base));
    assert!(!rendered.contains(&digest_debug));
}

#[test]
fn mismatched_actual_request_refuses_before_dns_without_spending_claim() {
    let prepared_request = request();
    let actual_request = HttpRequest::new(
        "POST",
        "/",
        prepared_request.headers().to_vec(),
        prepared_request.body().to_vec(),
    )
    .expect("request");
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)"], &[]),
    );
    let authorized = match policy.classify(http_call(
        "srv",
        "search",
        "args-a",
        "api.example.test:443",
        &prepared_request,
    )) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("expected authorization, got {other:?}"),
    };
    let (call, permit) = authorized.into_parts();
    let resolver = Arc::new(FakeResolver::one(
        NetworkTarget::parse("api.example.test:443").expect("target"),
        "192.0.2.10",
    ));
    let one_hop = Arc::new(FakeOneHop::default());
    let spawner = Arc::new(FakeSpawner::default());
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        resolver.clone(),
        one_hop.clone(),
        spawner.clone(),
    );
    let error = futures::executor::block_on(guarded.guarded_http(call.clone(), actual_request))
        .expect_err("digest mismatch");
    assert!(matches!(
        error.failure(),
        TransportFailure::AuthorityMismatch
    ));
    assert_eq!(
        error.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent
    );
    assert_eq!(resolver.calls(), 0);
    let response = futures::executor::block_on(guarded.guarded_http(call, prepared_request))
        .expect("matching request");
    assert_eq!(
        response.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::PossiblySent
    );
    assert_eq!(resolver.calls(), 1);
}

#[test]
fn resolution_target_mismatch_is_definitely_unsent_and_literal_skips_dns() {
    let req = request();
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)"], &[]),
    );
    let authorized = match policy.classify(http_call(
        "srv",
        "search",
        "args-a",
        "api.example.test:443",
        &req,
    )) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("expected authorization, got {other:?}"),
    };
    let (call, permit) = authorized.into_parts();
    let resolver = Arc::new(FakeResolver::one(
        NetworkTarget::parse("other.example.test:443").expect("target"),
        "192.0.2.10",
    ));
    let one_hop = Arc::new(FakeOneHop::default());
    let spawner = Arc::new(FakeSpawner::default());
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        resolver.clone(),
        one_hop.clone(),
        spawner.clone(),
    );
    let error =
        futures::executor::block_on(guarded.guarded_http(call, req)).expect_err("target mismatch");
    assert!(matches!(
        error.failure(),
        TransportFailure::AuthorityMismatch
    ));
    assert_eq!(
        error.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent
    );

    let literal = HttpRequest::get("/").expect("request");
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)"], &[]),
    );
    let authorized = match policy.classify(http_call(
        "srv",
        "search",
        "args-a",
        "127.0.0.1:443",
        &literal,
    )) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("expected authorization, got {other:?}"),
    };
    let (call, permit) = authorized.into_parts();
    let resolver = Arc::new(FakeResolver::default());
    let one_hop = Arc::new(FakeOneHop::default());
    let spawner = Arc::new(FakeSpawner::default());
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        resolver.clone(),
        one_hop.clone(),
        spawner.clone(),
    );
    futures::executor::block_on(guarded.guarded_http(call, literal)).expect("literal HTTP");
    assert_eq!(resolver.calls(), 0);
}

#[test]
fn post_effect_transport_failure_retains_possibly_sent_delivery() {
    let req = request();
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)"], &[]),
    );
    let authorized = match policy.classify(http_call(
        "srv",
        "search",
        "args-a",
        "api.example.test:443",
        &req,
    )) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("expected authorization, got {other:?}"),
    };
    let (call, permit) = authorized.into_parts();
    let resolver = Arc::new(FakeResolver::one(
        NetworkTarget::parse("api.example.test:443").expect("target"),
        "192.0.2.10",
    ));
    let one_hop = Arc::new(FakeOneHop::default());
    *one_hop.failure.lock().expect("failure") = Some(TransportError::possibly_sent(
        TransportFailure::Exchange("reset".into()),
    ));
    let spawner = Arc::new(FakeSpawner::default());
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        resolver.clone(),
        one_hop.clone(),
        spawner.clone(),
    );
    let error =
        futures::executor::block_on(guarded.guarded_http(call, req)).expect_err("exchange failure");
    assert!(matches!(error.failure(), TransportFailure::Exchange(_)));
    assert_eq!(
        error.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::PossiblySent
    );
}

#[test]
fn guarded_duplex_returns_one_child_capability_and_exact_spawn_spec() {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(server/execute)"], &[]),
    );
    let authorized =
        match policy.classify(stdio_call("server", vec!["server".into(), "--mcp".into()])) {
            ego_core::permit::Classification::Settled(Ok(value)) => value,
            other => panic!("expected authorization, got {other:?}"),
        };
    let (call, permit) = authorized.into_parts();
    let resolver = Arc::new(FakeResolver::default());
    let one_hop = Arc::new(FakeOneHop::default());
    let spawner = Arc::new(FakeSpawner::default());
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        resolver.clone(),
        one_hop.clone(),
        spawner.clone(),
    );
    let mut handle =
        futures::executor::block_on(guarded.guarded_duplex(call)).expect("child capability");
    futures::executor::block_on(futures::io::AsyncWriteExt::write_all(
        &mut handle,
        b"\0\xff\x80",
    ))
    .expect("binary write");
    futures::executor::block_on(futures::io::AsyncWriteExt::flush(&mut handle)).expect("flush");
    let mut bytes = Vec::new();
    futures::executor::block_on(futures::io::AsyncReadExt::read_to_end(
        &mut handle,
        &mut bytes,
    ))
    .expect("binary read");
    assert_eq!(bytes, b"child-bytes");
    assert_eq!(spawner.specs.lock().expect("spawn specs").len(), 1);
    let settled = futures::executor::block_on(handle.settle());
    assert!(matches!(
        settled.end(),
        ego_core::permit::DuplexEnd::Exited { code: 0 }
    ));
    assert!(matches!(
        settled.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_eq!(
        settled.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::PossiblySent
    );
    assert!(matches!(
        settled.evidence(),
        Err(EvidenceError::Limit { limit: 0 })
    ));

    let authorized =
        match policy.classify(stdio_call("server", vec!["server".into(), "--mcp".into()])) {
            ego_core::permit::Classification::Settled(Ok(value)) => value,
            other => panic!("expected second authorization, got {other:?}"),
        };
    let (interrupt_call, interrupt_permit) = authorized.into_parts();
    let interrupt_resolver = Arc::new(FakeResolver::default());
    let interrupt_one_hop = Arc::new(FakeOneHop::default());
    let interrupt_guarded = GuardedEffects::new(
        interrupt_permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        interrupt_resolver.clone(),
        interrupt_one_hop.clone(),
        spawner.clone(),
    );
    let interrupt_handle =
        futures::executor::block_on(interrupt_guarded.guarded_duplex(interrupt_call))
            .expect("second child capability");
    let interrupted = futures::executor::block_on(interrupt_handle.interrupt());
    assert!(matches!(
        interrupted.end(),
        ego_core::permit::DuplexEnd::Interrupted
    ));
    assert!(matches!(
        interrupted.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_eq!(
        interrupted.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent
    );
    assert!(matches!(
        interrupted.evidence(),
        Err(EvidenceError::Limit { limit: 0 })
    ));
}

#[test]
fn process_subject_round_trips_native_bytes_and_unknown_fields_fail_closed() {
    let spec = SpawnSpec::new(
        vec![OsString::from("server"), OsString::from("--mcp")],
        None,
        None,
        EnvPolicy::Inherited,
    )
    .expect("spec");
    let subject = Subject::Process(spec);
    let wire = serde_json::to_string(&subject).expect("subject");
    assert_eq!(
        serde_json::from_str::<Subject>(&wire).expect("round trip"),
        subject
    );
    let mut value = serde_json::to_value(subject).expect("subject");
    value["extra"] = true.into();
    assert!(serde_json::from_value::<Subject>(value).is_err());
}

#[test]
fn network_target_ipv6_literals_are_canonical_ip_targets() {
    let cases = [
        ("[::1]", Ipv6Addr::LOCALHOST),
        (
            "[::ffff:127.0.0.1]",
            Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 1),
        ),
    ];

    for (input, expected_ip) in cases {
        let target = NetworkTarget::new(input, 443).expect("IPv6 target");
        assert_eq!(target.host(), expected_ip.to_string());
        assert_eq!(target.ip(), Some(IpAddr::V6(expected_ip)));
        assert_eq!(target.to_string(), format!("[{expected_ip}]:443"));
    }
}

#[test]
fn one_hop_response_into_parts_preserves_owned_parts_and_closed_body_kind() {
    futures::executor::block_on(async {
        let headers = vec![HttpHeader::new("x-trace", b"owned".to_vec()).expect("header")];
        let (status, returned_headers, payload, delivery) = OneHopResponse::streaming_response(
            206,
            headers.clone(),
            Box::pin(futures::stream::once(futures::future::ready(Ok(
                b"stream-body".to_vec(),
            )))),
        )
        .into_parts();
        assert_eq!(status, 206);
        assert_eq!(returned_headers, headers);
        assert_eq!(delivery, DeliveryCertainty::PossiblySent);
        let OneHopPayload::Response(mut stream) = payload else {
            panic!("streaming response returned a redirect payload");
        };
        assert_eq!(
            stream.next().await.expect("body chunk").expect("body"),
            b"stream-body"
        );
        assert!(stream.next().await.is_none());

        let headers = vec![HttpHeader::new("x-trace", b"redirect".to_vec()).expect("header")];
        let (status, returned_headers, payload, delivery) =
            OneHopResponse::redirect(307, headers.clone(), b"https://example.test/next".to_vec())
                .into_parts();
        assert_eq!(status, 307);
        assert_eq!(returned_headers, headers);
        assert_eq!(delivery, DeliveryCertainty::PossiblySent);
        let OneHopPayload::Redirect(location) = payload else {
            panic!("redirect returned a streaming payload");
        };
        assert_eq!(location, b"https://example.test/next");
    });
}
