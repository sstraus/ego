//! RED contract for regression case guarded HTTP and one-child duplex authority.

use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::future::{self, BoxFuture};
use futures::io::{AsyncRead, AsyncWrite};

use ego_core::config::{ConfigLayer, Mode, Ruleset, Source};
use ego_core::permit::{
    CanonPath, DuplexHandle, DuplexSession, DuplexSpawner, EnvPolicy, Fs, FsError, GuardedEffects,
    HttpRequest, NetworkTarget, OneHop, PathResolver, PolicyEngine, PreparedCall, ProcessError,
    ProcessExecutor, Resolver, Scan, ScanRequest, SpawnSpec, Subject, ToolResult, TransportError,
};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};

fn rules(allow: &[&str], deny: &[&str]) -> Ruleset {
    let render = |values: &[&str]| {
        values
            .iter()
            .map(|value| format!("{value:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    ConfigLayer::parse(
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
    .expect("permissions")
    .map(|permissions| Ruleset::from_layer(permissions, Source::Project))
    .expect("permissions")
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

fn tool(server: &str, name: &str, args: &str) -> ego_core::permit::ForeignToolId {
    ego_core::permit::ForeignToolId::new(server, name).expect("foreign identity")
}

fn http_call(server: &str, name: &str, args: &str, target: &str) -> PreparedCall {
    PreparedCall::foreign_http(
        tool(server, name, args),
        digest(args),
        ego_core::permit::ForeignTransportTarget::http("https", target).expect("HTTP target"),
    )
}

fn stdio_call(server: &str, argv: Vec<OsString>) -> PreparedCall {
    let spec = SpawnSpec::new(argv, None, None, EnvPolicy::Inherited).expect("spawn spec");
    PreparedCall::foreign_stdio(
        tool(server, "execute", "args-a"),
        digest("args-a"),
        server,
        spec,
    )
}

#[derive(Clone, Default)]
struct FakeResolver {
    answer: Arc<Mutex<Option<ego_core::permit::DnsResolution>>>,
    calls: Arc<Mutex<usize>>,
}
impl FakeResolver {
    fn one(target: NetworkTarget, ip: &str) -> Self {
        Self {
            answer: Arc::new(Mutex::new(Some(
                ego_core::permit::DnsResolution::new(target, vec![ip.parse().expect("IP")])
                    .expect("DNS"),
            ))),
            calls: Arc::new(Mutex::new(0)),
        }
    }
    fn calls(&self) -> usize {
        *self.calls.lock().expect("calls")
    }
}
impl Resolver for FakeResolver {
    fn resolve(
        &self,
        _target: NetworkTarget,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, TransportError>> {
        *self.calls.lock().expect("calls") += 1;
        Box::pin(future::ready(
            self.answer
                .lock()
                .expect("answer")
                .take()
                .ok_or_else(|| TransportError::Resolution("DNS evidence exhausted".into())),
        ))
    }
}

#[derive(Clone, Default)]
struct FakeOneHop {
    calls: Arc<Mutex<usize>>,
}
impl OneHop for FakeOneHop {
    fn execute(
        &self,
        _connect: ego_core::permit::ResolvedConnect,
        _request: HttpRequest,
    ) -> BoxFuture<'static, Result<ego_core::permit::OneHopResponse, TransportError>> {
        *self.calls.lock().expect("calls") += 1;
        Box::pin(future::ready(Ok(
            ego_core::permit::OneHopResponse::response(
                200,
                vec![],
                vec![],
                ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent,
            ),
        )))
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
        mut self: std::pin::Pin<&mut Self>,
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
fn permission_requires_outer_and_inner_opaque_plus_exact_http_or_spawn_authority() {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)", "net(api.example.test:443)"], &[]),
    );
    assert!(matches!(
        policy.classify(http_call("srv", "search", "args-a", "api.example.test:443")),
        ego_core::permit::Classification::Settled(Ok(_))
    ));
    let outer = PreparedCall::speakeasy(
        tool("router", "execute_tool", "outer"),
        digest("outer"),
        tool("srv", "search", "inner"),
        digest("inner"),
        ego_core::permit::ForeignTransportTarget::http("https", "api.example.test:443")
            .expect("target"),
    );
    assert!(matches!(
        policy.classify(outer),
        ego_core::permit::Classification::Ask(_)
    ));
    let denied = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(
            &["tool(srv/search)", "net(api.example.test:443)"],
            &["tool(srv/search)"],
        ),
    );
    assert!(matches!(
        denied.classify(http_call("srv", "search", "args-a", "api.example.test:443")),
        ego_core::permit::Classification::Settled(Err(_))
    ));
}

#[test]
fn guarded_http_consumes_one_dns_answer_and_rejects_reuse_as_authority_spent() {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(srv/search)", "net(api.example.test:443)"], &[]),
    );
    let authorized =
        match policy.classify(http_call("srv", "search", "args-a", "api.example.test:443")) {
            ego_core::permit::Classification::Settled(Ok(value)) => value,
            other => panic!("expected authorization, got {other:?}"),
        };
    let (call, permit) = authorized.into_parts();
    let resolver = FakeResolver::one(
        NetworkTarget::parse("api.example.test:443").expect("target"),
        "192.0.2.10",
    );
    let one_hop = FakeOneHop::default();
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        &resolver,
        &one_hop,
        &FakeSpawner::default(),
    );
    let request = HttpRequest::get("/").expect("request");
    let first = futures::executor::block_on(guarded.guarded_http(call.clone(), request.clone()))
        .expect("first hop");
    assert_eq!(first.status(), 200);
    assert_eq!(resolver.calls(), 1);
    let second = futures::executor::block_on(guarded.guarded_http(call, request))
        .expect_err("spent authority");
    assert!(matches!(second, TransportError::AuthoritySpent));
    assert_eq!(resolver.calls(), 1);
}

#[test]
fn guarded_duplex_returns_one_child_capability_and_exact_spawn_spec() {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        rules(&["tool(server/execute)", "exec(server --mcp)"], &[]),
    );
    let authorized =
        match policy.classify(stdio_call("server", vec!["server".into(), "--mcp".into()])) {
            ego_core::permit::Classification::Settled(Ok(value)) => value,
            other => panic!("expected authorization, got {other:?}"),
        };
    let (call, permit) = authorized.into_parts();
    let spawner = FakeSpawner::default();
    let guarded = GuardedEffects::new(
        permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        &FakeResolver::default(),
        &FakeOneHop::default(),
        &spawner,
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
    let interrupt_guarded = GuardedEffects::new(
        interrupt_permit,
        &MemoryFs,
        &NoProcess,
        &NoEvidence,
        &FakeResolver::default(),
        &FakeOneHop::default(),
        &spawner,
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
