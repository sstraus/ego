//! RED contract for command and HTTP hook adapters (M4 S5).
//!
//! Adapters delegate effects to existing process and guarded HTTP owners.
//! Assertions observe prepared calls, typed owner results, and delivery
//! certainty rather than adapter-local flags.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::executor::block_on;
use futures::future::BoxFuture;

use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::event::hook::{
    HookActivation, HookContentDigest, HookId, HookInvocationId, HookLifecycle,
    HookSourceAuthority, HookSourceLocation, HookSubject,
};
use ego_core::event::id::{EventId, SessionId, TurnId};
use ego_core::hook::{
    EffectScope, HookCausality, HookEvent, HookHandlerConfig, HookHandlerRequestV1,
    HookInvocationRequest, HookStartedEvidence,
};
use ego_core::permit::{
    CanonPath, Classification, DnsResolution, EnvPolicy, GuardedEffects, HttpHeader, HttpRequest,
    HttpRoute, HttpScheme, NetworkTarget, OneHop, OneHopResponse, Op, PolicyEngine, PreparedCall,
    ProcessError, ProcessExecutor, ResolvedConnect, Resolver, SpawnSpec, StreamProvenance,
    ToolContentBlock, ToolResult, TransportError,
};
use ego_tools::hook::{CommandHookAdapter, HookAdapterResult, HttpHookAdapter};
use ego_tools::store::SessionEvidenceStore;

#[derive(Default)]
struct ProcessCapture {
    calls: std::sync::Mutex<Vec<SpawnSpec>>,
    inputs: Mutex<Vec<Vec<u8>>>,
    scopes: Mutex<Vec<EffectScope>>,
    outputs: Mutex<Vec<ToolResult>>,
    result: std::sync::Mutex<Option<Result<ToolResult, ProcessError>>>,
}

impl ProcessExecutor for ProcessCapture {
    fn execute<'a>(
        &'a self,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        self.calls.lock().unwrap().push(spec.clone());
        let result = self.next_result();
        self.record_output(&result);
        Box::pin(std::future::ready(result))
    }

    fn execute_handler<'a>(
        &'a self,
        spec: &'a SpawnSpec,
        input: &'a [u8],
        scope: EffectScope,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        self.calls.lock().unwrap().push(spec.clone());
        self.inputs.lock().unwrap().push(input.to_vec());
        self.scopes.lock().unwrap().push(scope);
        let result = self.next_result();
        self.record_output(&result);
        Box::pin(std::future::ready(result))
    }
}

impl ProcessCapture {
    fn next_result(&self) -> Result<ToolResult, ProcessError> {
        self.result.lock().unwrap().take().unwrap_or_else(|| {
            Ok(ToolResult::new(vec![ToolContentBlock::process(
                StreamProvenance::Stdout,
                br#"{"version":1,"decision":"continue"}"#,
                4096,
                ego_core::store::evidence::EvidenceRef::new(
                    ego_core::store::evidence::EvidenceId::parse(ego_core::digest::sha256_hex(
                        br#"{"version":1,"decision":"continue"}"#,
                    ))
                    .expect("handler result evidence id"),
                    br#"{"version":1,"decision":"continue"}"#.len() as u64,
                ),
            )]))
        })
    }

    fn record_output(&self, result: &Result<ToolResult, ProcessError>) {
        if let Ok(output) = result {
            self.outputs.lock().unwrap().push(output.clone());
        }
    }

    fn failing(error: ProcessError) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
            scopes: Mutex::new(Vec::new()),
            outputs: Mutex::new(Vec::new()),
            result: std::sync::Mutex::new(Some(Err(error))),
        }
    }

    fn inputs(&self) -> Vec<Vec<u8>> {
        self.inputs.lock().unwrap().clone()
    }

    fn scopes(&self) -> Vec<EffectScope> {
        self.scopes.lock().unwrap().clone()
    }

    fn outputs(&self) -> Vec<ToolResult> {
        self.outputs.lock().unwrap().clone()
    }
}

fn command_request(args: Option<Vec<String>>, shell: Option<&str>) -> HookInvocationRequest {
    let hook = HookId::derive(
        HookSourceAuthority::Project,
        &HookSourceLocation::parse("hooks/command.toml").expect("hook source"),
        &HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("hook digest"),
        0,
    );
    let invocation = HookInvocationId::new();
    HookInvocationRequest::new(
        HookStartedEvidence::new(
            EventId::new(),
            EventId::new(),
            hook,
            invocation,
            HookActivation::Persistent,
        ),
        HookCausality::session(SessionId::new(), TurnId::new()),
        HookHandlerConfig::Command {
            command: "/usr/bin/check".to_owned(),
            args,
            shell: shell.map(str::to_owned),
            timeout_seconds: Some(3),
        },
        HookEvent::tool(
            HookLifecycle::PreToolUse,
            HookSubject::Session,
            "exec(git status)",
        ),
        EffectScope::new(Duration::from_secs(3)),
    )
}

fn http_request(url: &str, authorization: &str) -> HookInvocationRequest {
    let hook = HookId::derive(
        HookSourceAuthority::Project,
        &HookSourceLocation::parse("hooks/http.toml").expect("hook source"),
        &HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("hook digest"),
        0,
    );
    let invocation = HookInvocationId::new();
    HookInvocationRequest::new(
        HookStartedEvidence::new(
            EventId::new(),
            EventId::new(),
            hook,
            invocation,
            HookActivation::Persistent,
        ),
        HookCausality::session(SessionId::new(), TurnId::new()),
        HookHandlerConfig::Http {
            url: url.to_owned(),
            headers: vec![("Authorization".to_owned(), authorization.to_owned())],
            allowed_env_vars: Some(vec!["HOOK_TOKEN".to_owned()]),
            timeout_seconds: Some(30),
        },
        HookEvent::session(HookLifecycle::UserPromptSubmit),
        EffectScope::new(Duration::from_secs(30)),
    )
}

fn admitted_http() -> (
    PreparedCall,
    HttpRequest,
    tempfile::TempDir,
    SessionEvidenceStore,
) {
    let hook_id = HookId::derive(
        HookSourceAuthority::Project,
        &HookSourceLocation::parse("hooks/http.toml").expect("hook location"),
        &HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("hook digest"),
        0,
    );
    let secret = ego_core::credentials::SecretMaterial::new(b"opaque-token".to_vec());
    let request = HttpRequest::new(
        "POST",
        "/check",
        vec![HttpHeader::secret("authorization", secret).expect("secret header")],
        br#"{"version":1,"decision":"continue"}"#.to_vec(),
    )
    .expect("admitted HTTP request");
    let target = NetworkTarget::new("hooks.example.test", 443).expect("target");
    let route = HttpRoute::new(HttpScheme::Https, target).expect("route");
    let prepared = PreparedCall::hook_http(hook_id, route, &request, vec!["HOOK_TOKEN".to_owned()]);
    let root = tempfile::tempdir().expect("evidence root");
    let evidence = SessionEvidenceStore::open(
        root.path(),
        ego_core::event::id::SessionId::new(),
        1024 * 1024,
    )
    .expect("evidence store");
    (prepared, request, root, evidence)
}

fn policy_for_http(mode: Mode) -> PolicyEngine {
    PolicyEngine::new(
        CanonPath::new("/workspace"),
        mode,
        Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![
                    ego_core::permit::rule::Rule::load("net(hooks.example.test:443)")
                        .expect("network rule"),
                ],
                deny: Vec::new(),
            },
            Source::Project,
        ),
    )
}

fn command_call(args: Option<Vec<String>>, shell: Option<&str>) -> PreparedCall {
    let argv = match (shell, args) {
        (Some(shell), None) => vec![
            OsString::from(shell),
            OsString::from("-c"),
            OsString::from("/usr/bin/check"),
        ],
        (None, Some(args)) => std::iter::once(OsString::from("/usr/bin/check"))
            .chain(args.into_iter().map(OsString::from))
            .collect(),
        (None, None) => vec![OsString::from("/usr/bin/check")],
        (Some(_), Some(_)) => Vec::new(),
    };
    let spec = SpawnSpec::new(argv, None, None, EnvPolicy::Inherited).expect("command spec");
    PreparedCall::new("hook_command", vec![Op::Spawn { spec }])
}

fn invoke_command(
    process: &ProcessCapture,
    prepared: PreparedCall,
    request: HookInvocationRequest,
    expected_delivery: ego_core::generator::attempt::DeliveryCertainty,
) {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Yolo,
        Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![
                    ego_core::permit::rule::Rule::load("exec(/usr/bin/check *)")
                        .expect("command rule"),
                    ego_core::permit::rule::Rule::load("exec(bash *)").expect("shell rule"),
                ],
                deny: Vec::new(),
            },
            Source::Project,
        ),
    );
    let Classification::Settled(Ok(authorized)) = policy.classify(prepared.clone()) else {
        panic!("hook command must be admitted by its policy");
    };
    let root = tempfile::tempdir().expect("evidence root");
    let evidence = SessionEvidenceStore::open(
        root.path(),
        ego_core::event::id::SessionId::new(),
        1024 * 1024,
    )
    .expect("evidence store");
    let fs = ego_core::permit::fake::FakeFs::default();
    let (_, permit) = authorized.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &fs,
        process,
        &evidence,
        Arc::new(TestResolver),
        Arc::new(TestOneHop),
        Arc::new(TestDuplex),
    );
    let settled = block_on(CommandHookAdapter::new(guarded, prepared).invoke(request))
        .expect("typed command settlement");
    assert!(matches!(&settled, HookAdapterResult::Settled(_)));
    assert_eq!(settled.delivery_certainty(), expected_delivery);
}

#[test]
fn command_uses_process_executor_and_guarded_effects_for_exact_argv() {
    let process = Arc::new(ProcessCapture::default());
    let request = command_request(Some(vec!["--exact".to_owned(), "argv".to_owned()]), None);
    let expected_input = serde_json::to_vec(
        &HookHandlerRequestV1::from_runtime(&request).expect("strict handler request"),
    )
    .expect("handler request JSON");
    let lower_deadline = Instant::now();
    invoke_command(
        &process,
        command_call(Some(vec!["--exact".to_owned(), "argv".to_owned()]), None),
        request,
        ego_core::generator::attempt::DeliveryCertainty::DefinitelySent,
    );
    let upper_deadline = Instant::now() + Duration::from_secs(3);
    let spawn = &process.calls.lock().unwrap()[0];

    assert_eq!(spawn.argv(), ["/usr/bin/check", "--exact", "argv"]);
    assert_eq!(process.inputs(), vec![expected_input]);
    let output = &process.outputs()[0];
    assert_eq!(output.content.len(), 1);
    assert_eq!(output.content[0].evidence_reference().unwrap().bytes, 35);
    let scope = &process.scopes()[0];
    assert!(scope.deadline() >= lower_deadline);
    assert!(scope.deadline() <= upper_deadline);
}

#[test]
fn command_absent_args_is_shell_success_but_present_empty_args_conflicts_with_shell() {
    let process = Arc::new(ProcessCapture::default());
    invoke_command(
        &process,
        command_call(None, Some("bash")),
        command_request(None, Some("bash")),
        ego_core::generator::attempt::DeliveryCertainty::DefinitelySent,
    );
    assert_eq!(
        process.calls.lock().unwrap()[0].argv(),
        ["bash", "-c", "/usr/bin/check"]
    );

    assert!(CommandHookAdapter::prepare(command_request(Some(Vec::new()), Some("bash"))).is_err());
}

#[test]
fn command_normalizes_pre_effect_and_observer_failures_with_delivery_certainty() {
    for (error, certainty) in [
        (
            ProcessError::Timeout {
                timeout: Duration::from_secs(3),
            },
            ego_tools::hook::DeliveryCertainty::PossiblySent,
        ),
        (
            ProcessError::Cancelled,
            ego_tools::hook::DeliveryCertainty::PossiblySent,
        ),
        (
            ProcessError::Backend("backend".to_owned()),
            ego_tools::hook::DeliveryCertainty::DefinitelyUnsent,
        ),
    ] {
        let process = ProcessCapture::failing(error);
        invoke_command(
            &process,
            command_call(None, None),
            command_request(None, None),
            certainty,
        );
    }
}

#[test]
fn http_uses_guarded_http_prepared_call_with_connect_and_secret_egress() {
    let (prepared, admitted, _root, evidence) = admitted_http();
    assert_eq!(admitted.method(), "POST");
    assert_eq!(admitted.path(), "/check");
    assert_eq!(admitted.headers().len(), 1);
    assert!(admitted.headers()[0].is_secret());
    assert_eq!(admitted.headers()[0].name(), "authorization");
    assert_eq!(admitted.body(), br#"{"version":1,"decision":"continue"}"#);
    assert!(
        prepared
            .ops
            .iter()
            .any(|op| matches!(op, Op::Connect { .. }))
    );
    assert!(
        prepared
            .ops
            .iter()
            .any(|op| op.name().to_string() == "secret_egress")
    );
    let policy = policy_for_http(Mode::Yolo);
    let Classification::Settled(Ok(authorized)) = policy.classify(prepared.clone()) else {
        panic!("yolo policy must settle the admitted hook HTTP call");
    };
    let fs = ego_core::permit::fake::FakeFs::default();
    let process = ProcessCapture::default();
    let (_, permit) = authorized.into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &fs,
        &process,
        &evidence,
        Arc::new(TestResolver),
        Arc::new(TestOneHop),
        Arc::new(TestDuplex),
    );
    let result = block_on(
        HttpHookAdapter::new(guarded, prepared.clone(), admitted).invoke(http_request(
            "https://hooks.example.test/check",
            "${HOOK_TOKEN}",
        )),
    );
    let settled = result.expect("typed HTTP settlement");
    assert!(matches!(settled, HookAdapterResult::Settled(_)));
}

#[test]
fn http_rejects_url_tokens_and_missing_environment_before_preparation() {
    assert!(
        HttpHookAdapter::prepare(http_request("https://${HOOK_TOKEN}/check", "literal",)).is_err()
    );
    assert!(
        HttpHookAdapter::prepare(http_request(
            "https://hooks.example.test/check",
            "${MISSING}",
        ))
        .is_err()
    );
}

#[test]
fn secret_egress_is_a_one_shot_policy_decision_and_offline_is_definitely_unsent() {
    let (prepared, _request, _root, _evidence) = admitted_http();
    let Classification::Ask(pending) = policy_for_http(Mode::Default).classify(prepared) else {
        panic!("secret egress must remain a one-shot approval question");
    };
    assert!(
        pending
            .request()
            .ops()
            .iter()
            .any(|op| op.name().to_string() == "secret_egress")
    );
    assert!(
        pending
            .request()
            .ops()
            .iter()
            .any(|op| matches!(op, Op::Connect { .. }))
    );
}

#[test]
fn guarded_http_offline_refusal_is_definitely_unsent_before_one_hop() {
    let target = NetworkTarget::new("hooks.example.test", 443).expect("target");
    let route =
        ego_core::permit::HttpRoute::new(ego_core::permit::HttpScheme::Https, target.clone())
            .expect("route");
    let request = ego_core::permit::HttpRequest::get("/check").expect("request");
    let connect = ego_core::permit::PreparedConnect::new(target, &request);
    let result = block_on(
        ego_core::permit::GuardedHttp::new(Arc::new(OfflineResolver), Arc::new(TestOneHop))
            .execute_admitted(route, connect, request, None),
    );
    let error = result.expect_err("offline hook HTTP must be refused");
    assert_eq!(
        error.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent
    );
    assert!(matches!(
        error.failure(),
        ego_core::permit::TransportFailure::Offline
    ));
}

struct TestResolver;
struct TestOneHop;
struct TestDuplex;
struct OfflineResolver;

impl Resolver for OfflineResolver {
    fn resolve(
        &self,
        _target: NetworkTarget,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            ego_core::permit::TransportFailure::Offline,
        ))))
    }
}

impl ego_core::permit::DuplexSpawner for TestDuplex {
    fn spawn(
        &self,
        _spec: SpawnSpec,
    ) -> BoxFuture<'static, Result<ego_core::permit::DuplexHandle, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            ego_core::permit::TransportFailure::Offline,
        ))))
    }
}

impl Resolver for TestResolver {
    fn resolve(
        &self,
        target: NetworkTarget,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        Box::pin(std::future::ready(Ok(DnsResolution::new(
            target,
            vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
        )
        .expect("loopback resolution"))))
    }
}

impl OneHop for TestOneHop {
    fn execute(
        &self,
        _connect: ResolvedConnect,
        _request: HttpRequest,
        _deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(std::future::ready(Ok(OneHopResponse::response(
            200,
            Vec::new(),
            br#"{"version":1,"decision":"continue"}"#.to_vec(),
        ))))
    }
}
