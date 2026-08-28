//! RED contract for regression case OS adapters behind guarded core capabilities.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use ego_core::config::{ConfigLayer, Mode, Ruleset, Source};
use ego_core::event::domain::CancelCause;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{
    CanonPath, Confinement, DnsResolution, EnvPolicy, Fs, FsError, GuardedEffects, HttpRequest,
    HttpRoute, HttpScheme, NetworkTarget, PathResolver, PolicyEngine, PreparedCall, Resolver,
    RunConfinement, Scan, ScanRequest, SpawnSpec, StreamProvenance, ToolOutputProvenance,
    TransportError, TransportFailure,
};
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, ProcessError, RunEffectPorts, SessionEvidenceStore,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

struct ScenarioRuntime(tokio::runtime::Runtime);

impl ScenarioRuntime {
    fn new() -> Self {
        Self(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scenario runtime"),
        )
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.0.block_on(future)
    }
}

fn rules(allow: &[&str]) -> Ruleset {
    let values = allow
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let permissions =
        ConfigLayer::parse(Source::Project, &format!("[permissions]\nallow=[{values}]"))
            .expect("permission fixture")
            .value
            .permissions
            .expect("permissions");
    Ruleset::from_layer(permissions, Source::Project)
}

fn digest() -> ego_core::permit::InvocationDigest {
    ego_core::permit::InvocationDigest::parse(
        "sha256:0000000000000000000000000000000000000000000000000000000000000001",
    )
    .expect("canonical digest")
}

fn request() -> HttpRequest {
    HttpRequest::get("/").expect("request")
}

fn http_call(target: &str, request: &HttpRequest) -> PreparedCall {
    let route = HttpRoute::new(
        HttpScheme::Http,
        NetworkTarget::parse(target).expect("target"),
    )
    .expect("route");
    PreparedCall::foreign_http(
        ego_core::permit::ForeignToolId::new("srv", "search").expect("tool"),
        digest(),
        ego_core::permit::ForeignTransportTarget::http(route),
        request,
    )
}

fn stdio_call(spec: SpawnSpec) -> PreparedCall {
    PreparedCall::foreign_stdio(
        ego_core::permit::ForeignToolId::new("server", "execute").expect("tool"),
        digest(),
        "server",
        spec,
    )
}

fn authorized(call: PreparedCall, allow: &[&str]) -> ego_core::permit::Authorized {
    let policy = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Default, rules(allow));
    match policy.classify(call) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("expected authorization, got {other:?}"),
    }
}

struct RunFixture {
    runtime: ScenarioRuntime,
    _evidence_root: tempfile::TempDir,
    evidence: SessionEvidenceStore,
    ports: RunEffectPorts,
}

fn run_ports_with(
    resolver: RecordingResolver,
    offline: bool,
    cancel: ego_core::turn::CancelToken,
    http_call_timeout: HttpCallTimeout,
) -> RunFixture {
    run_ports_with_limits(
        resolver,
        offline,
        cancel,
        ExecLimits::new(1024, Duration::from_millis(250)).expect("exec limits"),
        http_call_timeout,
    )
}

fn run_ports_with_limits(
    resolver: RecordingResolver,
    offline: bool,
    cancel: ego_core::turn::CancelToken,
    exec_limits: ExecLimits,
    http_call_timeout: HttpCallTimeout,
) -> RunFixture {
    try_run_ports_with_limits(resolver, offline, cancel, exec_limits, http_call_timeout)
        .expect("run ports")
}

fn try_run_ports_with_limits(
    resolver: RecordingResolver,
    offline: bool,
    cancel: ego_core::turn::CancelToken,
    exec_limits: ExecLimits,
    http_call_timeout: HttpCallTimeout,
) -> Result<RunFixture, ProcessError> {
    try_run_ports_with_limits_and_deadline(
        resolver,
        offline,
        cancel,
        exec_limits,
        http_call_timeout,
        None,
    )
}

fn run_ports_with_deadline(
    resolver: RecordingResolver,
    offline: bool,
    cancel: ego_core::turn::CancelToken,
    http_call_timeout: HttpCallTimeout,
    run_deadline: Option<Instant>,
) -> RunFixture {
    try_run_ports_with_limits_and_deadline(
        resolver,
        offline,
        cancel,
        ExecLimits::new(1024, Duration::from_millis(250)).expect("exec limits"),
        http_call_timeout,
        run_deadline,
    )
    .expect("run ports")
}

fn try_run_ports_with_limits_and_deadline(
    resolver: RecordingResolver,
    offline: bool,
    cancel: ego_core::turn::CancelToken,
    exec_limits: ExecLimits,
    http_call_timeout: HttpCallTimeout,
    run_deadline: Option<Instant>,
) -> Result<RunFixture, ProcessError> {
    let runtime = ScenarioRuntime::new();
    let root = CanonPath::new(
        std::fs::canonicalize(std::env::current_dir().expect("cwd")).expect("canonical cwd"),
    );
    let evidence_root = tempfile::tempdir().expect("evidence root");
    let evidence = SessionEvidenceStore::open(
        evidence_root.path(),
        ego_core::event::id::SessionId::new(),
        1024 * 1024,
    )
    .expect("evidence");
    let policy = RunConfinement::new(Confinement::Off, offline, root).expect("run policy");
    let config = ExecRunConfig::new(
        policy,
        evidence.clone(),
        cancel.clone(),
        exec_limits,
        http_call_timeout,
        run_deadline,
        Arc::new(resolver),
    );
    let (ports, _) = runtime.block_on(RunEffectPorts::for_run(config))?;
    Ok(RunFixture {
        runtime,
        _evidence_root: evidence_root,
        evidence,
        ports,
    })
}

fn run_ports(resolver: RecordingResolver, offline: bool) -> RunFixture {
    run_ports_with(
        resolver,
        offline,
        ego_core::turn::CancelToken::new(),
        HttpCallTimeout::default(),
    )
}

fn try_run_ports(
    resolver: RecordingResolver,
    offline: bool,
    cancel: ego_core::turn::CancelToken,
    http_call_timeout: HttpCallTimeout,
) -> Result<RunFixture, ProcessError> {
    try_run_ports_with_limits(
        resolver,
        offline,
        cancel,
        ExecLimits::new(1024, Duration::from_millis(250)).expect("exec limits"),
        http_call_timeout,
    )
}

type RecordingAnswer = (NetworkTarget, Vec<IpAddr>);
type RecordingAnswers = Vec<RecordingAnswer>;

#[derive(Clone, Default)]
struct RecordingResolver {
    answer: Arc<Mutex<RecordingAnswers>>,
    calls: Arc<Mutex<Vec<NetworkTarget>>>,
    delay: Arc<Mutex<Option<Duration>>>,
}
impl RecordingResolver {
    fn insert(&self, target: NetworkTarget, ip: IpAddr) {
        self.answer
            .lock()
            .expect("answers")
            .push((target, vec![ip]));
    }

    fn delay(&self, duration: Duration) {
        *self.delay.lock().expect("delay") = Some(duration);
    }
}
impl Resolver for RecordingResolver {
    fn resolve(
        &self,
        target: NetworkTarget,
        _deadline: Option<Instant>,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        self.calls.lock().expect("calls").push(target.clone());
        let delay = *self.delay.lock().expect("delay");
        let answer = self
            .answer
            .lock()
            .expect("answers")
            .iter()
            .find(|(candidate, _)| *candidate == target)
            .cloned();
        Box::pin(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            answer.map_or_else(
                || {
                    Err(TransportError::definitely_unsent(
                        TransportFailure::Resolution("no answer".into()),
                    ))
                },
                |(target, addresses)| {
                    DnsResolution::new(target, addresses).map_err(|_| {
                        TransportError::definitely_unsent(TransportFailure::Resolution(
                            "fixture returned no addresses".into(),
                        ))
                    })
                },
            )
        })
    }
}

struct NoFs;
impl PathResolver for NoFs {
    fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError> {
        Ok(CanonPath::new(path.to_path_buf()))
    }
}
impl Fs for NoFs {
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
#[derive(Debug)]
struct CapturedConnection {
    peer: SocketAddr,
    request: Vec<u8>,
    request_count: usize,
}

struct SocketServer {
    address: SocketAddr,
    stop: mpsc::Sender<()>,
    captures: Arc<Mutex<Vec<CapturedConnection>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SocketServer {
    fn bind<F>(handler: F) -> Self
    where
        F: Fn(TcpStream) -> CapturedConnection + Send + 'static,
    {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("address");
        let captures = Arc::new(Mutex::new(Vec::new()));
        let captures_for_thread = Arc::clone(&captures);
        let (stop, stop_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let capture = handler(stream);
                        captures_for_thread.lock().expect("captures").push(capture);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop_rx.recv_timeout(Duration::from_millis(10)).is_ok() {
                            break;
                        }
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
        });
        Self {
            address,
            stop,
            captures,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> Vec<CapturedConnection> {
        self.stop.send(()).expect("stop socket server");
        self.thread
            .take()
            .expect("socket thread")
            .join()
            .expect("socket server");
        Arc::try_unwrap(self.captures)
            .expect("capture ownership")
            .into_inner()
            .expect("captures")
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("request timeout");
    let mut request = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(count) => {
                request.extend_from_slice(&byte[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => panic!("request: {error}"),
        }
    }
    let body_len = request
        .split(|byte| *byte == b'\n')
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let separator = line.iter().position(|byte| *byte == b':')?;
            let (name, value) = line.split_at(separator);
            name.eq_ignore_ascii_case(b"content-length")
                .then(|| std::str::from_utf8(&value[1..]).ok()?.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    let body_start = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .unwrap_or(request.len());
    let already_read = request.len() - body_start;
    if already_read < body_len {
        let mut body = vec![0_u8; body_len - already_read];
        stream.read_exact(&mut body).expect("request body");
        request.extend_from_slice(&body);
    }
    request
}

fn host_values(request: &[u8]) -> Vec<Vec<u8>> {
    request
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let separator = line.iter().position(|byte| *byte == b':')?;
            let (name, value) = line.split_at(separator);
            let value = &value[1..];
            name.eq_ignore_ascii_case(b"host")
                .then(|| value.strip_prefix(b" ").unwrap_or(value).to_vec())
        })
        .collect()
}

struct FrameScanner {
    unread: Vec<u8>,
}

impl FrameScanner {
    const MAX_BUFFER: usize = 64 * 1024;
    const READ_TIMEOUT: Duration = Duration::from_secs(1);

    fn new() -> Self {
        Self { unread: Vec::new() }
    }

    async fn read_fixed<R>(&mut self, reader: &mut R, marker: &[u8], payload_len: usize) -> Vec<u8>
    where
        R: AsyncRead + Unpin,
    {
        self.read_marker(reader, marker).await;
        self.read_payload(reader, payload_len).await
    }

    async fn read_length_prefixed<R>(&mut self, reader: &mut R, marker: &[u8]) -> Vec<u8>
    where
        R: AsyncRead + Unpin,
    {
        self.read_marker(reader, marker).await;
        let length = self.read_payload(reader, 1).await[0] as usize;
        self.read_payload(reader, length).await
    }

    async fn read_marker<R>(&mut self, reader: &mut R, marker: &[u8])
    where
        R: AsyncRead + Unpin,
    {
        assert!(!marker.is_empty(), "frame marker must not be empty");
        loop {
            if let Some(index) = self
                .unread
                .windows(marker.len())
                .position(|candidate| candidate == marker)
            {
                self.unread.drain(..index + marker.len());
                return;
            }
            self.read_more(reader).await;
        }
    }

    async fn read_payload<R>(&mut self, reader: &mut R, length: usize) -> Vec<u8>
    where
        R: AsyncRead + Unpin,
    {
        while self.unread.len() < length {
            self.read_more(reader).await;
        }
        self.unread.drain(..length).collect()
    }

    async fn drain_to_eof<R>(&mut self, reader: &mut R)
    where
        R: AsyncRead + Unpin,
    {
        self.unread.clear();
        let mut chunk = [0_u8; 1024];
        loop {
            let count = tokio::time::timeout(Self::READ_TIMEOUT, reader.read(&mut chunk))
                .await
                .expect("frame drain timeout")
                .expect("frame drain");
            if count == 0 {
                return;
            }
        }
    }

    async fn read_more<R>(&mut self, reader: &mut R)
    where
        R: AsyncRead + Unpin,
    {
        let mut chunk = [0_u8; 1024];
        let count = tokio::time::timeout(Self::READ_TIMEOUT, reader.read(&mut chunk))
            .await
            .expect("frame read timeout")
            .expect("frame read");
        assert_ne!(count, 0, "frame ended before payload");
        self.unread.extend_from_slice(&chunk[..count]);
        assert!(
            self.unread.len() <= Self::MAX_BUFFER,
            "frame scan exceeded bounded buffer"
        );
    }
}

fn response_server(status: u16, location: &'static str) -> SocketServer {
    SocketServer::bind(move |mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        write!(
            stream,
            "HTTP/1.1 {status} Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n"
        )
        .expect("headers");
        CapturedConnection {
            peer,
            request,
            request_count: 1,
        }
    })
}

#[test]
fn guarded_http_is_the_only_subject_under_test_and_redirect_needs_fresh_authority() {
    let server = response_server(302, "/next");
    let address = server.address;
    let target = NetworkTarget::new("api.example.test", address.port()).expect("network target");
    let resolver = RecordingResolver::default();
    resolver.insert(target, IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports(resolver.clone(), false);
    let request = request();
    let authorized = authorized(
        http_call(&format!("api.example.test:{}", address.port()), &request),
        &[
            "tool(srv/search)",
            &format!("net(api.example.test:{})", address.port()),
        ],
    );
    let (call, permit) = authorized.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let response = fixture
        .runtime
        .block_on(guarded.guarded_http(call, request))
        .expect("guarded HTTP");
    assert!(matches!(
        response,
        ego_core::permit::OneHopResponse::Redirect {
            status: 302,
            ref location,
            delivery: DeliveryCertainty::PossiblySent,
            ..
        } if location == b"/next"
    ));
    assert_eq!(resolver.calls.lock().expect("calls").len(), 1);
    let captures = server.finish();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].request_count, 1);
    drop(fixture);
}

fn duplex_spec(name: &str, timeout: Option<Duration>) -> (SpawnSpec, String) {
    let workspace = CanonPath::new(std::env::current_dir().expect("cwd"));
    let executable = std::env::current_exe().expect("test executable");
    let spec = SpawnSpec::new(
        vec![
            executable.clone().into_os_string(),
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from(name),
            OsString::from("--nocapture"),
            OsString::from("--test-threads=1"),
        ],
        Some(workspace.clone()),
        timeout,
        EnvPolicy::Inherited,
    )
    .expect("spec");
    let allow = format!(
        "exec({} --ignored --exact {} --nocapture --test-threads=1)",
        executable.display(),
        name
    );
    (spec, allow)
}

fn authorized_duplex(name: &str, timeout: Option<Duration>) -> ego_core::permit::Authorized {
    let (spec, allow) = duplex_spec(name, timeout);
    authorized(stdio_call(spec), &["tool(server/execute)", &allow])
}

#[test]
fn guarded_duplex_settles_binary_child_and_keeps_stdout_out_of_evidence() {
    let authorized = authorized_duplex("foreign_transport_fixture_binary", None);
    let (call, permit) = authorized.into_parts();
    let fixture = run_ports(RecordingResolver::default(), false);
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let mut handle = fixture
        .runtime
        .block_on(guarded.guarded_duplex(call))
        .expect("guarded duplex");
    assert_eq!(handle.delivery(), DeliveryCertainty::DefinitelyUnsent);
    fixture
        .runtime
        .block_on(handle.write_all(b"\0\xff\x80"))
        .expect("binary write");
    fixture.runtime.block_on(handle.flush()).expect("flush");
    let sentinel = b"EGO_FOREIGN_TRANSPORT_SENTINEL\0";
    let mut scanner = FrameScanner::new();
    let payload = fixture
        .runtime
        .block_on(scanner.read_length_prefixed(&mut handle, sentinel));
    assert_eq!(payload, b"\0\xff\x80");
    assert_eq!(handle.delivery(), DeliveryCertainty::PossiblySent);
    let settlement = fixture.runtime.block_on(handle.settle());
    let actual_end = settlement.end();
    let actual_cleanup = settlement.cleanup();
    let actual_evidence = settlement.evidence();
    assert!(
        matches!(actual_end, ego_core::permit::DuplexEnd::Exited { code: 0 }),
        "unexpected duplex settlement: end={actual_end:?}, cleanup={actual_cleanup:?}, delivery={:?}, evidence={actual_evidence:?}",
        settlement.delivery()
    );
    assert!(matches!(
        actual_cleanup,
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_eq!(settlement.delivery(), DeliveryCertainty::PossiblySent);
    let evidence = settlement.evidence().as_ref().expect("finalized evidence");
    assert_eq!(evidence.content.len(), 1, "evidence contains stderr only");
    let block = &evidence.content[0];
    assert!(matches!(
        block.provenance(),
        ToolOutputProvenance::Process {
            stream: StreamProvenance::Stderr,
            ..
        }
    ));
    assert_eq!(
        evidence.model_text(),
        "[stderr]\nfixture bounded stderr evidence\n"
    );
    assert!(
        !evidence
            .model_text()
            .contains("EGO_FOREIGN_TRANSPORT_SENTINEL")
    );
    drop(fixture);
}

#[test]
#[ignore]
fn foreign_transport_fixture_binary() {
    let mut input = [0_u8; 3];
    std::io::stdin()
        .read_exact(&mut input)
        .expect("fixture stdin");
    std::io::stderr()
        .write_all(b"fixture bounded stderr evidence\n")
        .expect("fixture stderr");
    std::io::stderr().flush().expect("fixture stderr flush");
    std::io::stdout()
        .write_all(b"EGO_FOREIGN_TRANSPORT_SENTINEL\0")
        .expect("fixture stdout");
    std::io::stdout()
        .write_all(&[input.len() as u8])
        .expect("fixture length");
    std::io::stdout().write_all(&input).expect("fixture echo");
    std::io::stdout().flush().expect("fixture flush");
    std::process::exit(0);
}

#[test]
fn guarded_duplex_interrupts_a_blocked_child_after_write() {
    let authorized = authorized_duplex("foreign_duplex_blocking_fixture", None);
    let (call, permit) = authorized.into_parts();
    let fixture = run_ports(RecordingResolver::default(), false);
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let mut handle = fixture
        .runtime
        .block_on(guarded.guarded_duplex(call))
        .expect("duplex");
    let ready_marker = b"EGO_DUPLEX_READY\0";
    let mut scanner = FrameScanner::new();
    let ready = fixture
        .runtime
        .block_on(scanner.read_fixed(&mut handle, ready_marker, 4));
    let direct = u32::from_le_bytes(ready.try_into().unwrap());
    fixture
        .runtime
        .block_on(handle.write_all(b"outbound"))
        .expect("write");
    assert_eq!(handle.delivery(), DeliveryCertainty::PossiblySent);
    let settlement = fixture.runtime.block_on(handle.interrupt());
    assert!(matches!(
        settlement.end(),
        ego_core::permit::DuplexEnd::Interrupted
    ));
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_eq!(settlement.delivery(), DeliveryCertainty::PossiblySent);
    assert!(settlement.evidence().is_ok(), "evidence finalized");
    assert!(!descendant_is_running(direct), "direct child was reaped");
    drop(fixture);
}

#[test]
#[ignore]
fn foreign_duplex_blocking_fixture() {
    std::io::stdout()
        .write_all(b"EGO_DUPLEX_READY\0")
        .expect("ready");
    std::io::stdout()
        .write_all(&std::process::id().to_le_bytes())
        .expect("direct pid");
    std::io::stdout().flush().expect("ready flush");
    std::thread::park();
}

#[test]
fn guarded_duplex_times_out_blocked_child_with_exact_timeout() {
    let timeout = Duration::from_millis(100);
    let authorized = authorized_duplex("foreign_duplex_timeout_fixture", Some(timeout));
    let (call, permit) = authorized.into_parts();
    let fixture = run_ports(RecordingResolver::default(), false);
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let mut handle = fixture
        .runtime
        .block_on(guarded.guarded_duplex(call))
        .expect("duplex");
    let ready_marker = b"EGO_DUPLEX_READY\n";
    let mut scanner = FrameScanner::new();
    let ready = fixture
        .runtime
        .block_on(scanner.read_fixed(&mut handle, ready_marker, 0));
    assert!(ready.is_empty());
    let settlement = fixture.runtime.block_on(handle.settle());
    assert!(matches!(
        settlement.end(),
        ego_core::permit::DuplexEnd::TimedOut { timeout: value } if *value == timeout
    ));
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_eq!(settlement.delivery(), DeliveryCertainty::DefinitelyUnsent);
    assert!(settlement.evidence().is_ok(), "evidence finalized");
    drop(fixture);
}

#[test]
#[ignore]
fn foreign_duplex_timeout_fixture() {
    std::io::stdout()
        .write_all(b"EGO_DUPLEX_READY\n")
        .expect("ready");
    std::io::stdout().flush().expect("ready flush");
    std::thread::park();
}

#[test]
fn guarded_duplex_cancels_blocked_child_after_write() {
    let authorized = authorized_duplex("foreign_duplex_blocking_fixture", None);
    let (call, permit) = authorized.into_parts();
    let cancel = ego_core::turn::CancelToken::new();
    let fixture = run_ports_with(
        RecordingResolver::default(),
        false,
        cancel.clone(),
        HttpCallTimeout::default(),
    );
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let mut handle = fixture
        .runtime
        .block_on(guarded.guarded_duplex(call))
        .expect("duplex");
    let ready_marker = b"EGO_DUPLEX_READY\0";
    let mut scanner = FrameScanner::new();
    let ready = fixture
        .runtime
        .block_on(scanner.read_fixed(&mut handle, ready_marker, 4));
    assert_eq!(ready.len(), 4);
    fixture
        .runtime
        .block_on(handle.write_all(b"outbound"))
        .expect("write");
    cancel.cancel(CancelCause::User);
    let settlement = fixture.runtime.block_on(handle.settle());
    assert!(matches!(
        settlement.end(),
        ego_core::permit::DuplexEnd::Cancelled
    ));
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_eq!(settlement.delivery(), DeliveryCertainty::PossiblySent);
    assert!(settlement.evidence().is_ok(), "evidence finalized");
    drop(fixture);
}

#[cfg(unix)]
#[test]
fn guarded_duplex_interrupt_cleans_a_long_lived_descendant() {
    let authorized = authorized_duplex("foreign_duplex_descendant_fixture", None);
    let (call, permit) = authorized.into_parts();
    let fixture = run_ports(RecordingResolver::default(), false);
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let mut handle = fixture
        .runtime
        .block_on(guarded.guarded_duplex(call))
        .expect("duplex");
    let marker = b"EGO_DESCENDANTS\0";
    let mut scanner = FrameScanner::new();
    let frame = fixture
        .runtime
        .block_on(scanner.read_fixed(&mut handle, marker, 8));
    let direct = u32::from_le_bytes(frame[..4].try_into().unwrap());
    let descendant = u32::from_le_bytes(frame[4..].try_into().unwrap());
    assert_ne!(direct, descendant);
    fixture
        .runtime
        .block_on(handle.write_all(b"outbound"))
        .expect("write");
    let settlement = fixture.runtime.block_on(handle.interrupt());
    assert!(matches!(
        settlement.end(),
        ego_core::permit::DuplexEnd::Interrupted
    ));
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_eq!(settlement.delivery(), DeliveryCertainty::PossiblySent);
    assert!(settlement.evidence().is_ok(), "evidence finalized");
    assert!(!descendant_is_running(descendant), "descendant was cleaned");
    assert!(!descendant_is_running(direct), "direct child was reaped");
    drop(fixture);
}

#[test]
#[ignore]
fn foreign_duplex_descendant_fixture() {
    let mut descendant = std::process::Command::new("sh")
        .args(["-c", "sleep 30"])
        .spawn()
        .expect("descendant");
    let descendant_pid = descendant.id();
    std::thread::spawn(move || {
        descendant.wait().expect("descendant wait");
    });
    let marker = b"EGO_DESCENDANTS\0";
    std::io::stdout()
        .write_all(marker)
        .expect("descendant marker");
    std::io::stdout()
        .write_all(&std::process::id().to_le_bytes())
        .expect("direct pid");
    std::io::stdout()
        .write_all(&descendant_pid.to_le_bytes())
        .expect("descendant pid");
    std::io::stdout().flush().expect("descendant flush");
    std::thread::park();
}

#[test]
fn guarded_duplex_settles_bounded_stderr_evidence_without_stdout_protocol() {
    let authorized = authorized_duplex("foreign_duplex_stderr_fixture", None);
    let (call, permit) = authorized.into_parts();
    let fixture = run_ports_with_limits(
        RecordingResolver::default(),
        false,
        ego_core::turn::CancelToken::new(),
        ExecLimits::new(32, Duration::from_millis(250)).expect("exec limits"),
        HttpCallTimeout::default(),
    );
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let mut handle = fixture
        .runtime
        .block_on(guarded.guarded_duplex(call))
        .expect("duplex");
    let marker = b"EGO_C6_STDOUT_SENTINEL\0";
    let mut scanner = FrameScanner::new();
    let payload = fixture
        .runtime
        .block_on(scanner.read_fixed(&mut handle, marker, 0));
    assert!(payload.is_empty());
    fixture.runtime.block_on(scanner.drain_to_eof(&mut handle));
    assert_eq!(handle.delivery(), DeliveryCertainty::DefinitelyUnsent);
    let settlement = fixture.runtime.block_on(handle.settle());
    assert!(
        matches!(
            settlement.end(),
            ego_core::permit::DuplexEnd::Exited { code: 0 }
        ),
        "unexpected duplex end: end={:?}; cleanup={:?}; evidence={:?}",
        settlement.end(),
        settlement.cleanup(),
        settlement.evidence()
    );
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_eq!(settlement.delivery(), DeliveryCertainty::DefinitelyUnsent);
    let evidence = settlement.evidence().as_ref().expect("finalized evidence");
    assert_eq!(evidence.content.len(), 1, "only stderr is evidence");
    let block = &evidence.content[0];
    assert!(matches!(
        block.provenance(),
        ToolOutputProvenance::Process {
            stream: StreamProvenance::Stderr,
            truncated: true,
            ..
        }
    ));
    let reference = block.evidence_reference().expect("complete evidence ref");
    assert_eq!(reference.bytes, 76);
    assert!(evidence.model_text().contains("C6_HEAD"));
    assert!(evidence.model_text().contains("C6_TAIL"));
    assert!(!evidence.model_text().contains("EGO_C6_STDOUT_SENTINEL"));
}

#[test]
#[ignore]
fn foreign_duplex_stderr_fixture() {
    std::io::stdout()
        .write_all(b"EGO_C6_STDOUT_SENTINEL\0")
        .expect("stdout sentinel");
    std::io::stderr()
        .write_all(b"C6_HEAD_123456012345678901234567890123456789012345678901234567C6_TAIL_ABCDEF")
        .expect("stderr evidence");
    std::io::stderr().flush().expect("stderr flush");
    std::io::stdout().flush().expect("stdout flush");
}

#[cfg(unix)]
fn descendant_is_running(pid: u32) -> bool {
    let pid = pid.to_string();
    if !std::process::Command::new("/bin/kill")
        .args(["-0", &pid])
        .status()
        .map(|value| value.success())
        .unwrap_or(false)
    {
        return false;
    }
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid])
        .output();
    let Ok(output) = output else { return false };
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    output.status.success() && !state.is_empty() && !state.starts_with('Z')
}

#[cfg(windows)]
fn descendant_is_running(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/NH"])
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.split_whitespace()
                    .any(|field| field == pid.to_string())
            })
        })
        .unwrap_or(false)
}

#[test]
fn one_resolution_binds_selected_ip_and_preserves_logical_host() {
    let server = response_server(204, "/next");
    let address = server.address;
    let target = NetworkTarget::new("unresolvable.example.test", address.port()).unwrap();
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports(resolver.clone(), false);
    let req = request();
    let auth = authorized(
        http_call(
            &format!("unresolvable.example.test:{}", address.port()),
            &req,
        ),
        &[
            "tool(srv/search)",
            &format!("net(unresolvable.example.test:{})", address.port()),
        ],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let response = fixture
        .runtime
        .block_on(guarded.guarded_http(call, req))
        .unwrap();
    assert_eq!(resolver.calls.lock().unwrap().as_slice(), &[target]);
    assert_eq!(response.status(), 204);
    let captures = server.finish();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].request_count, 1);
    assert!(!captures[0].request.is_empty());
    assert_eq!(
        host_values(&captures[0].request),
        vec![format!("unresolvable.example.test:{}", address.port()).into_bytes()]
    );
}

#[test]
fn proxy_environment_is_not_an_authority_source() {
    let destination = response_server(204, "/next");
    let proxy = SocketServer::bind(|stream| CapturedConnection {
        peer: stream.peer_addr().expect("proxy peer"),
        request: Vec::new(),
        request_count: 0,
    });
    let executable = std::env::current_exe().expect("test executable");
    let destination_port = destination.address.port().to_string();
    let proxy_url = format!("http://127.0.0.1:{}", proxy.address.port());
    let status = std::process::Command::new(executable)
        .args([
            "--ignored",
            "--exact",
            "foreign_transport_proxy_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("EGO_PROXY_CHILD_PORT", destination_port)
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .status()
        .expect("proxy child");
    assert!(status.success(), "proxy child status: {status}");

    let destination_port = destination.address.port();
    let destination_captures = destination.finish();
    let proxy_captures = proxy.finish();
    assert_eq!(destination_captures.len(), 1, "destination accepted once");
    assert_eq!(destination_captures[0].request_count, 1);
    assert!(!destination_captures[0].request.is_empty());
    let expected_host = format!(
        "Host: proxy-is-not-authority.example.test:{}",
        destination_port
    );
    assert_eq!(
        host_values(&destination_captures[0].request),
        vec![
            expected_host
                .strip_prefix("Host: ")
                .expect("host prefix")
                .as_bytes()
                .to_vec()
        ]
    );
    assert!(proxy_captures.is_empty(), "proxy received no connection");
}

#[test]
#[ignore]
fn foreign_transport_proxy_child() {
    let port = std::env::var("EGO_PROXY_CHILD_PORT").expect("child destination port");
    let port = port.parse().expect("child destination port number");
    let target =
        NetworkTarget::new("proxy-is-not-authority.example.test", port).expect("child target");
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports(resolver.clone(), false);
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let response = fixture
        .runtime
        .block_on(guarded.guarded_http(call, request))
        .expect("child guarded HTTP");
    assert_eq!(response.status(), 204);
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
}

#[test]
fn redirect_is_one_hop_and_does_not_touch_second_listener() {
    let (response, calls, expected_location) = {
        let second = SocketServer::bind(|stream| CapturedConnection {
            peer: stream.peer_addr().expect("second peer"),
            request: Vec::new(),
            request_count: 0,
        });
        let second_location = format!(
            "http://redirect-second.example.test:{}/next",
            second.address.port()
        );
        let first_location = second_location.clone();
        let first = SocketServer::bind(move |mut stream| {
            let peer = stream.peer_addr().expect("first peer");
            let request = read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: {first_location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("redirect response");
            CapturedConnection {
                peer,
                request,
                request_count: 1,
            }
        });
        let address = first.address;
        let target = NetworkTarget::new("redirect.example.test", address.port()).unwrap();
        let resolver = RecordingResolver::default();
        resolver.insert(target, IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let fixture = run_ports(resolver.clone(), false);
        let req = request();
        let auth = authorized(
            http_call(&format!("redirect.example.test:{}", address.port()), &req),
            &[
                "tool(srv/search)",
                &format!("net(redirect.example.test:{})", address.port()),
            ],
        );
        let (call, permit) = auth.into_parts();
        let guarded = GuardedEffects::new(
            permit,
            &NoFs,
            fixture.ports.process(),
            &fixture.evidence,
            fixture.ports.resolver_handle(),
            fixture.ports.one_hop_handle(),
            fixture.ports.duplex_handle(),
        );
        let response = fixture
            .runtime
            .block_on(guarded.guarded_http(call, req))
            .unwrap();
        let calls = resolver.calls.lock().unwrap().len();
        let captures = first.finish();
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].request_count, 1);
        assert!(!captures[0].request.is_empty());
        let second_captures = second.finish();
        assert!(
            second_captures.is_empty(),
            "redirect did not contact second listener"
        );
        (response, calls, second_location.into_bytes())
    };
    assert_eq!(calls, 1, "redirect does not trigger a second resolution");
    assert!(matches!(
        response,
        ego_core::permit::OneHopResponse::Redirect { ref location, .. }
            if location == &expected_location
    ));
}

#[test]
fn reset_never_retries_a_request() {
    let server = SocketServer::bind(move |mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        stream.shutdown(Shutdown::Both).expect("reset connection");
        CapturedConnection {
            peer,
            request,
            request_count: 1,
        }
    });
    let target = NetworkTarget::new("reset.example.test", server.address.port()).unwrap();
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports(resolver.clone(), false);
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let error = fixture
        .runtime
        .block_on(guarded.guarded_http(call, request))
        .expect_err("reset must not produce a response");
    assert!(matches!(
        error.failure(),
        TransportFailure::Exchange(_) | TransportFailure::ResponseBody(_)
    ));
    assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
    let captures = server.finish();
    assert_eq!(captures.len(), 1, "reset accepted exactly once");
    assert_eq!(
        captures[0].request_count, 1,
        "reset sent exactly one request"
    );
    assert!(!captures[0].request.is_empty());
}

#[test]
fn fresh_authorizations_use_fresh_sockets() {
    let server = SocketServer::bind(move |mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        write!(
            stream,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n"
        )
        .expect("response");
        let second_request = read_request(&mut stream);
        if !second_request.is_empty() {
            stream
                .shutdown(Shutdown::Both)
                .expect("refuse reused request");
        }
        CapturedConnection {
            peer,
            request: [request, second_request.clone()].concat(),
            request_count: 1 + usize::from(!second_request.is_empty()),
        }
    });
    let target = NetworkTarget::new("fresh.example.test", server.address.port()).unwrap();
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports(resolver.clone(), false);

    for _ in 0..2 {
        let request = request();
        let auth = authorized(
            http_call(&format!("{target}"), &request),
            &["tool(srv/search)", &format!("net({target})")],
        );
        let (call, permit) = auth.into_parts();
        let guarded = GuardedEffects::new(
            permit,
            &NoFs,
            fixture.ports.process(),
            &fixture.evidence,
            fixture.ports.resolver_handle(),
            fixture.ports.one_hop_handle(),
            fixture.ports.duplex_handle(),
        );
        let response = fixture
            .runtime
            .block_on(guarded.guarded_http(call, request))
            .expect("fresh authorized HTTP");
        assert_eq!(response.status(), 204);
    }
    assert_eq!(
        resolver.calls.lock().expect("calls").as_slice(),
        &[target.clone(), target]
    );
    let captures = server.finish();
    assert_eq!(
        captures.len(),
        2,
        "two authorizations accepted two connections"
    );
    assert_ne!(
        captures[0].peer, captures[1].peer,
        "source ports are distinct"
    );
    assert_eq!(
        captures[0].request_count, 1,
        "first connection refused reuse"
    );
    assert_eq!(captures[1].request_count, 1);
    assert!(captures.iter().all(|capture| !capture.request.is_empty()));
}

#[test]
fn response_headers_keep_raw_duplicate_order_and_body_is_streamed() {
    let first_sent = Arc::new(Mutex::new(None));
    let first_sent_for_server = Arc::clone(&first_sent);
    let release_second = Arc::new(Mutex::new(None));
    let release_second_for_server = Arc::clone(&release_second);
    let second_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let second_sent_for_server = Arc::clone(&second_sent);
    let (first_tx, first_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = SocketServer::bind(move |mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nX-Order: first\r\nX-Order: second\r\nX-Raw: ")
            .expect("response headers");
        stream.write_all(&[0x80, 0xff]).expect("raw header value");
        stream
            .write_all(b"\r\nContent-Length: 6\r\nConnection: close\r\n\r\n")
            .expect("response headers");
        stream.write_all(b"abc").expect("first body chunk");
        stream.flush().expect("first body flush");
        *first_sent_for_server.lock().expect("first sent") = Some(Instant::now());
        first_tx.send(()).expect("first body synchronization");
        let released = release_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        *release_second_for_server.lock().expect("release") = Some(released);
        second_sent_for_server.store(true, std::sync::atomic::Ordering::Release);
        stream.write_all(b"def").expect("second body chunk");
        stream.flush().expect("second body flush");
        CapturedConnection {
            peer,
            request,
            request_count: 1,
        }
    });
    let target = NetworkTarget::new("stream.example.test", server.address.port()).unwrap();
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports(resolver.clone(), false);
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    fixture.runtime.block_on(async {
        let mut response = guarded
            .guarded_http(call, request)
            .await
            .expect("streaming HTTP");
        tokio::task::spawn_blocking(move || {
            first_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("server flushed first body chunk");
        })
        .await
        .expect("first body synchronization task");
        let ego_core::permit::OneHopResponse::Response { headers, body, .. } = &mut response else {
            panic!("expected streaming response");
        };
        assert_eq!(
            headers
                .iter()
                .map(|header| (header.name(), header.value()))
                .collect::<Vec<_>>(),
            vec![
                ("x-order", b"first".as_slice()),
                ("x-order", b"second".as_slice()),
                ("x-raw", b"\x80\xff".as_slice()),
                ("content-length", b"6".as_slice()),
                ("connection", b"close".as_slice()),
            ]
        );
        assert_eq!(
            body.next()
                .await
                .expect("first body item")
                .expect("first body chunk"),
            b"abc"
        );
        assert!(!second_sent.load(std::sync::atomic::Ordering::Acquire));
        release_tx.send(()).expect("release second body chunk");
        assert_eq!(
            body.next()
                .await
                .expect("second body item")
                .expect("second body chunk"),
            b"def"
        );
        assert_eq!(body.next().await, None);
    });
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
    let captures = server.finish();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].request_count, 1);
    assert!(!captures[0].request.is_empty());
    assert!(first_sent.lock().expect("first sent").is_some());
    assert_eq!(*release_second.lock().expect("release"), Some(true));
}

#[test]
fn large_request_is_sent_without_shared_request_byte_cap() {
    let server = SocketServer::bind(|mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .expect("response");
        CapturedConnection {
            peer,
            request,
            request_count: 1,
        }
    });
    let target =
        NetworkTarget::new("large-request.example.test", server.address.port()).expect("target");
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports(resolver.clone(), false);
    let body = vec![b'x'; 2048];
    let request = HttpRequest::new("POST", "/", Vec::new(), body.clone()).expect("request");
    let authorized = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = authorized.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let response = fixture
        .runtime
        .block_on(guarded.guarded_http(call, request))
        .expect("large request");
    assert_eq!(response.status(), 200);
    assert_eq!(response.delivery(), DeliveryCertainty::PossiblySent);
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
    let captures = server.finish();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].request_count, 1);
    assert!(captures[0].request.ends_with(&body));
}

#[test]
fn large_raw_response_headers_are_not_shared_byte_limited() {
    let header_value = vec![b'x'; 256];
    let header_value_for_server = header_value.clone();
    let server = SocketServer::bind(move |mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nX-Large: ")
            .expect("response prefix");
        stream
            .write_all(&header_value_for_server)
            .expect("large response header");
        stream
            .write_all(b"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("response suffix");
        CapturedConnection {
            peer,
            request,
            request_count: 1,
        }
    });
    let target =
        NetworkTarget::new("large-header.example.test", server.address.port()).expect("target");
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports_with(
        resolver.clone(),
        false,
        ego_core::turn::CancelToken::new(),
        HttpCallTimeout::default(),
    );
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let response = fixture
        .runtime
        .block_on(guarded.guarded_http(call, request))
        .expect("large response header");
    assert_eq!(response.status(), 200);
    assert_eq!(response.delivery(), DeliveryCertainty::PossiblySent);
    let ego_core::permit::OneHopResponse::Response { headers, .. } = &response else {
        panic!("expected response");
    };
    assert_eq!(
        headers
            .iter()
            .find(|header| header.name() == "x-large")
            .expect("large header")
            .value(),
        header_value.as_slice()
    );
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
    let captures = server.finish();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].request_count, 1);
    assert!(!captures[0].request.is_empty());
}

#[test]
fn large_streamed_response_body_is_delivered_without_shared_byte_cap() {
    let first = vec![b'a'; 1024];
    let second = vec![b'b'; 1024];
    let first_for_server = first.clone();
    let second_for_server = second.clone();
    let (first_tx, first_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = SocketServer::bind(move |mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n")
            .expect("response headers");
        stream
            .write_all(&first_for_server)
            .expect("first body chunk");
        stream.flush().expect("first body flush");
        first_tx.send(()).expect("first body synchronization");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release second body chunk");
        stream
            .write_all(&second_for_server)
            .expect("second body chunk");
        stream.flush().expect("body flush");
        CapturedConnection {
            peer,
            request,
            request_count: 1,
        }
    });
    let target =
        NetworkTarget::new("large-body.example.test", server.address.port()).expect("target");
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports_with(
        resolver.clone(),
        false,
        ego_core::turn::CancelToken::new(),
        HttpCallTimeout::default(),
    );
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let mut response = fixture
        .runtime
        .block_on(guarded.guarded_http(call, request))
        .expect("large streamed body");
    assert_eq!(response.status(), 200);
    assert_eq!(response.delivery(), DeliveryCertainty::PossiblySent);
    let body = response.body().expect("streaming body");
    let chunks = fixture.runtime.block_on(async {
        tokio::task::spawn_blocking(move || {
            first_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("server flushed first body chunk");
        })
        .await
        .expect("first body synchronization task");
        let first_chunk = body
            .next()
            .await
            .expect("first body item")
            .expect("first body chunk");
        release_tx.send(()).expect("release second body chunk");
        let second_chunk = body
            .next()
            .await
            .expect("second body item")
            .expect("second body chunk");
        assert_eq!(body.next().await, None);
        vec![first_chunk, second_chunk]
    });
    let bytes = chunks.concat();
    assert_eq!(bytes, [first, second].concat());
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
    let captures = server.finish();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].request_count, 1);
    assert!(!captures[0].request.is_empty());
}

#[test]
fn absolute_hop_timeout_covers_phased_exchange() {
    let (first_tx, first_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = SocketServer::bind(move |mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")
            .expect("first response phase");
        stream.flush().expect("first response flush");
        first_tx.send(()).expect("first response synchronization");
        let _ = release_rx.recv_timeout(Duration::from_millis(250));
        stream.write_all(b"def").expect("second response phase");
        stream.flush().expect("second response flush");
        CapturedConnection {
            peer,
            request,
            request_count: 1,
        }
    });
    let target = NetworkTarget::new("timeout.example.test", server.address.port()).unwrap();
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let fixture = run_ports_with(
        resolver.clone(),
        false,
        ego_core::turn::CancelToken::new(),
        HttpCallTimeout::new(Duration::from_millis(100)).expect("HTTP call timeout"),
    );
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    fixture.runtime.block_on(async {
        let mut response = guarded
            .guarded_http(call, request)
            .await
            .expect("headers before timeout");
        tokio::task::spawn_blocking(move || {
            first_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("first body phase");
        })
        .await
        .expect("first body synchronization task");
        let body = response.body().expect("streaming body");
        assert_eq!(
            body.next()
                .await
                .expect("first body item")
                .expect("first body chunk"),
            b"abc"
        );
        let error = body
            .next()
            .await
            .expect("timeout body item")
            .expect_err("absolute hop timeout");
        assert!(matches!(error.failure(), TransportFailure::TimedOut));
        assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
    });
    let _ = release_tx.send(());
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
    let captures = server.finish();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].request_count, 1);
    assert!(!captures[0].request.is_empty());
}

#[test]
fn delayed_resolution_times_out_before_socket() {
    let server = SocketServer::bind(|stream| CapturedConnection {
        peer: stream.peer_addr().expect("peer"),
        request: Vec::new(),
        request_count: 0,
    });
    let target = NetworkTarget::new("delayed-resolution.example.test", server.address.port())
        .expect("target");
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    resolver.delay(Duration::from_millis(250));
    let fixture = run_ports_with(
        resolver.clone(),
        false,
        ego_core::turn::CancelToken::new(),
        HttpCallTimeout::new(Duration::from_millis(100)).expect("HTTP call timeout"),
    );
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let error = fixture
        .runtime
        .block_on(guarded.guarded_http(call, request))
        .expect_err("delayed resolution timeout");
    assert!(matches!(error.failure(), TransportFailure::TimedOut));
    assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
    assert!(
        server.finish().is_empty(),
        "resolver timeout opened no socket"
    );
}

#[test]
fn http_call_deadline_is_bounded_by_run_deadline() {
    let earlier = Instant::now() + Duration::from_secs(1);
    let bounded_fixture = run_ports_with_deadline(
        RecordingResolver::default(),
        false,
        ego_core::turn::CancelToken::new(),
        HttpCallTimeout::new(Duration::from_secs(30)).expect("HTTP call timeout"),
        Some(earlier),
    );
    let bounded = bounded_fixture.ports.begin_http_call();
    assert!(bounded <= earlier);

    let configured_fixture = run_ports_with_deadline(
        RecordingResolver::default(),
        false,
        ego_core::turn::CancelToken::new(),
        HttpCallTimeout::new(Duration::from_secs(30)).expect("HTTP call timeout"),
        None,
    );
    let before = Instant::now();
    let configured = configured_fixture.ports.begin_http_call();
    let after = Instant::now();
    assert!(configured >= before + Duration::from_secs(30));
    assert!(configured <= after + Duration::from_secs(30));
}

#[test]
fn cancellation_before_io_is_definitely_unsent_and_skips_dns_and_socket() {
    let server = SocketServer::bind(|stream| CapturedConnection {
        peer: stream.peer_addr().expect("peer"),
        request: Vec::new(),
        request_count: 0,
    });
    let target = NetworkTarget::new("cancel-before.example.test", server.address.port()).unwrap();
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let cancel = ego_core::turn::CancelToken::new();
    cancel.cancel(CancelCause::User);
    let fixture = run_ports_with(resolver.clone(), false, cancel, HttpCallTimeout::default());
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    let error = fixture
        .runtime
        .block_on(guarded.guarded_http(call, request))
        .expect_err("pre-cancelled HTTP");
    assert!(matches!(error.failure(), TransportFailure::Cancelled));
    assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    assert!(resolver.calls.lock().expect("calls").is_empty());
    assert!(server.finish().is_empty(), "pre-cancel opened no socket");
}

#[test]
fn cancellation_during_io_is_possibly_sent_after_one_request() {
    let (received_tx, received_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = SocketServer::bind(move |mut stream| {
        let peer = stream.peer_addr().expect("peer");
        let request = read_request(&mut stream);
        received_tx.send(()).expect("request synchronization");
        let _ = release_rx.recv_timeout(Duration::from_secs(2));
        CapturedConnection {
            peer,
            request,
            request_count: 1,
        }
    });
    let target =
        NetworkTarget::new("cancel-during.example.test", server.address.port()).expect("target");
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let cancel = ego_core::turn::CancelToken::new();
    let fixture = run_ports_with(
        resolver.clone(),
        false,
        cancel.clone(),
        HttpCallTimeout::new(Duration::from_secs(2)).expect("HTTP call timeout"),
    );
    let request = request();
    let auth = authorized(
        http_call(&format!("{target}"), &request),
        &["tool(srv/search)", &format!("net({target})")],
    );
    let (call, permit) = auth.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &NoFs,
        fixture.ports.process(),
        &fixture.evidence,
        fixture.ports.resolver_handle(),
        fixture.ports.one_hop_handle(),
        fixture.ports.duplex_handle(),
    );
    fixture.runtime.block_on(async {
        let request_future = guarded.guarded_http(call, request);
        tokio::pin!(request_future);
        let received = tokio::task::spawn_blocking(move || {
            received_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("server received request");
        });
        let error = tokio::select! {
            result = &mut request_future => result.expect_err("in-flight cancellation"),
            received_result = received => {
                received_result.expect("request synchronization task");
                cancel.cancel(CancelCause::User);
                request_future
                    .await
                    .expect_err("cancelled result")
            }
        };
        assert!(matches!(error.failure(), TransportFailure::Cancelled));
        assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
        release_tx.send(()).expect("release cancelled server");
    });
    assert_eq!(resolver.calls.lock().expect("calls").as_slice(), &[target]);
    let captures = server.finish();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].request_count, 1);
    assert!(!captures[0].request.is_empty());
}

#[test]
fn offline_http_refuses_before_dns_or_socket() {
    let server = SocketServer::bind(|stream| CapturedConnection {
        peer: stream.peer_addr().expect("peer"),
        request: Vec::new(),
        request_count: 0,
    });
    let target = NetworkTarget::new("offline.example.test", server.address.port()).unwrap();
    let resolver = RecordingResolver::default();
    resolver.insert(target.clone(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let result = try_run_ports(
        resolver.clone(),
        true,
        ego_core::turn::CancelToken::new(),
        HttpCallTimeout::default(),
    );
    #[cfg(target_os = "linux")]
    {
        match result {
            Err(error) => assert!(
                matches!(error, ProcessError::MissingCapabilities(_)),
                "offline construction failed for an unrelated reason: {error:?}"
            ),
            Ok(_) => panic!("Linux offline construction unexpectedly succeeded"),
        }
        assert!(resolver.calls.lock().expect("calls").is_empty());
        assert!(server.finish().is_empty(), "offline opened no socket");
        return;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let fixture = match result {
            Ok(fixture) => fixture,
            Err(error) => {
                assert!(
                    matches!(error, ProcessError::MissingCapabilities(_)),
                    "offline construction failed for an unrelated reason: {error:?}"
                );
                assert!(resolver.calls.lock().expect("calls").is_empty());
                assert!(server.finish().is_empty(), "offline opened no socket");
                return;
            }
        };
        let request = request();
        let auth = authorized(
            http_call(&format!("{target}"), &request),
            &["tool(srv/search)", &format!("net({target})")],
        );
        let (call, permit) = auth.into_parts();
        let guarded = GuardedEffects::new(
            permit,
            &NoFs,
            fixture.ports.process(),
            &fixture.evidence,
            fixture.ports.resolver_handle(),
            fixture.ports.one_hop_handle(),
            fixture.ports.duplex_handle(),
        );
        let error = fixture
            .runtime
            .block_on(guarded.guarded_http(call, request))
            .expect_err("offline HTTP");
        assert!(matches!(error.failure(), TransportFailure::Offline));
        assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
        assert!(resolver.calls.lock().expect("calls").is_empty());
        assert!(server.finish().is_empty(), "offline opened no socket");
    }
}
