//! contract for the MCP hook adapter (M4 S5).
//!
//! Dispatch consumes one immutable captured registry generation and the
//! ordinary MCP operation path. The capture below is admitted through the
//! real registry controller; the operation result comes from its factory.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite, Cursor};
use serde_json::json;

use ego_core::config::{
    ConfigLayer, Mode, PermCfg, ResolvedMcpEntry, Ruleset, Source, Trust, resolve,
};
use ego_core::event::hook::{
    HookActivation, HookContentDigest, HookId, HookInvocationId, HookLifecycle,
    HookSourceAuthority, HookSourceLocation, HookSubject,
};
use ego_core::event::id::{EventId, SessionId, TurnId};
use ego_core::hook::{
    EffectScope, HookCausality, HookEvent, HookHandlerConfig, HookInvocationRequest,
    HookStartedEvidence,
};
use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject, ToolExecution};
use ego_core::permit::{
    CanonPath, Classification, CleanupCertainty, DnsResolution, DuplexEnd, DuplexHandle,
    DuplexSession, DuplexSpawner, EnvPolicy, ForeignToolId, GuardedEffects, GuardedIo,
    InvocationDigest, OneHop, OneHopResponse, Op, PolicyEngine, PreparedCall, ProcessError,
    ProcessExecutor, ResolvedConnect, Resolver, SpawnSpec, ToolError, ToolResult, TransportError,
    TransportFailure,
};
use ego_core::store::blob::{BlobError, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::turn::{McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError};
use ego_mcp::{
    McpAdmittedCandidate, McpCacheMetadata, McpCandidateAdmission, McpCandidateError,
    McpCandidateIdentity, McpCandidateInventory, McpCandidateRequest, McpCapabilities, McpFuture,
    McpHookAdapter, McpHookFailure, McpOperation, McpOperationFactory, McpRegistryCandidate,
    McpRegistryCapture, McpRegistryController, McpRegistryRefresh, McpServerAdmission,
    McpTransportMaterial, RegistryScope, RegistryTrigger,
};

fn request(server: &str, tool: &str, input: serde_json::Value) -> HookInvocationRequest {
    let hook = HookId::derive(
        HookSourceAuthority::Project,
        &HookSourceLocation::parse("hooks/mcp.toml").expect("hook source"),
        &HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("hook digest"),
        0,
    );
    HookInvocationRequest::new(
        HookStartedEvidence::new(
            EventId::new(),
            EventId::new(),
            hook,
            HookInvocationId::new(),
            HookActivation::Persistent,
        ),
        HookCausality::session(SessionId::new(), TurnId::new()),
        HookHandlerConfig::McpTool {
            server: server.to_owned(),
            tool: tool.to_owned(),
            input,
            timeout_seconds: Some(30),
        },
        HookEvent::turn(HookLifecycle::PreToolUse, HookSubject::Session),
        EffectScope::new(Duration::from_secs(30)),
    )
}

fn wrapped_wire(request: &HookInvocationRequest, input: &serde_json::Value) -> serde_json::Value {
    json!({
        "hook": ego_core::hook::HookEventV1::from_runtime(request)
            .expect("strict MCP hook event"),
        "input": input,
    })
}

fn admitted_call(wire: &serde_json::Value, capture: &str) -> PreparedCall {
    let subject = McpSubject::Tool {
        server: "server".to_owned(),
        name: "lookup".to_owned(),
    };
    let spec = SpawnSpec::new(
        vec![OsString::from("mcp-server")],
        None,
        Some(Duration::from_secs(30)),
        EnvPolicy::exact(Vec::new()).expect("empty admitted MCP environment"),
    )
    .expect("stdio spawn spec");
    PreparedCall::mcp_stdio(
        subject,
        McpOperationKind::CallTool,
        vec![(
            ForeignToolId::new("server", "lookup").expect("foreign tool identity"),
            InvocationDigest::from_bytes(&serde_json::to_vec(wire).expect("wrapped wire")),
        )],
        McpAdmissionFingerprint::new(capture, "server/lookup", "schema-v1")
            .expect("admission fingerprint"),
        "stdio-transport-v1",
        "server",
        spec,
        wire,
    )
    .expect("stdio prepared call")
}

fn authorize(call: PreparedCall) -> ego_core::permit::Authorized {
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Yolo,
        Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![
                    ego_core::permit::rule::Rule::load("tool(server/lookup)")
                        .expect("MCP tool rule"),
                    ego_core::permit::rule::Rule::load("exec(mcp-server *)")
                        .expect("MCP process rule"),
                ],
                deny: Vec::new(),
            },
            Source::Project,
        ),
    );
    let Classification::Settled(Ok(authorized)) = policy.classify(call) else {
        panic!("MCP hook call must be authorized by its policy");
    };
    authorized
}

macro_rules! invoke_admitted_hook {
    ($capture:expr, $request:expr, $parameters:expr, $expected_duplexes:expr $(,)?) => {{
        let request = $request;
        let parameters = $parameters;
        let wire = wrapped_wire(&request, &parameters);
        let capture = $capture;
        let prepared = admitted_call(&wire, capture.stable_fingerprint());
        assert!(
            prepared
                .ops()
                .iter()
                .any(|op| matches!(op, Op::Opaque { .. }))
        );
        assert!(
            prepared
                .ops()
                .iter()
                .any(|op| matches!(op, Op::Spawn { .. }))
        );
        let authorized = authorize(prepared.clone());
        let fs = ego_core::permit::fake::FakeFs::default();
        let process = HookProcess;
        let evidence = EmptyEvidence;
        let duplex = Arc::new(RecordingDuplexSpawner::default());
        let guarded_duplex: Arc<dyn DuplexSpawner> = duplex.clone();
        let (_, permit) = authorized.into_parts();
        let guarded = GuardedEffects::new(
            permit,
            &fs,
            &process,
            &evidence,
            Arc::new(OfflineResolver),
            Arc::new(OfflineOneHop),
            guarded_duplex,
        );
        let result = block_on(
            McpHookAdapter::new(
                capture,
                guarded,
                prepared,
                EffectScope::new(Duration::from_secs(30)),
            )
            .invoke(request),
        );
        assert_eq!(duplex.calls(), $expected_duplexes);
        result
    }};
}

#[test]
fn mcp_wraps_hook_and_input_for_one_captured_tool_and_preserves_generation() {
    let capture = capture_registry(json!({
        "structuredContent": {"version": 1, "decision": "continue"},
        "content": [{"type": "text", "text": "evidence only"}],
    }));
    let request = request("server", "lookup", json!({"key": "value"}));
    let input = json!({"key": "value"});
    let expected_wire = wrapped_wire(&request, &input);
    let result = invoke_admitted_hook!(capture, request, input, 1,);
    let settled = result.expect("typed MCP settlement");

    assert_eq!(settled.registry_generation(), "generation-1");
    assert!(matches!(settled.operation(), McpOperationKind::CallTool));
    assert_eq!(settled.wire_input(), &expected_wire);
    assert_eq!(settled.wire_input()["hook"]["version"], 1);
    assert_eq!(settled.wire_input()["input"]["key"], "value");
}

#[test]
fn mcp_accepts_current_structured_content_and_keeps_other_content_evidence_only() {
    let result = invoke_admitted_hook!(
        capture_registry(json!({
            "structuredContent": {"version": 1, "decision": "continue"},
            "content": [{"type": "text", "text": "evidence only"}],
        })),
        request("server", "lookup", json!({})),
        json!({}),
        1,
    );
    assert!(result.expect("structured MCP result").feedback().is_none());
}

#[test]
fn mcp_missing_or_malformed_structured_content_is_failure_without_text_fallback() {
    for raw in [
        json!({"content": [{"type": "text", "text": "continue"}]}),
        json!({"structuredContent": {"decision": "continue"}}),
    ] {
        let result = invoke_admitted_hook!(
            capture_registry(raw),
            request("server", "lookup", json!({})),
            json!({}),
            1,
        );
        assert_eq!(
            result.expect("settlement").failure(),
            Some(McpHookFailure::Malformed)
        );
    }
}

#[test]
fn mcp_unknown_capture_or_denied_egress_fails_before_operation() {
    let result = invoke_admitted_hook!(
        capture_registry(json!({
            "structuredContent": {"version": 1, "decision": "continue"},
            "content": [{"type": "text", "text": "evidence only"}],
        })),
        request("unknown", "lookup", json!({}),),
        json!({}),
        0,
    );
    assert_eq!(
        result.expect("settlement").failure(),
        Some(McpHookFailure::PermissionDenied)
    );
}

fn capture_registry(result: serde_json::Value) -> McpRegistryCapture {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.server]\ntransport = 'stdio'\ncommand = 'mcp-server'\n",
    )
    .expect("MCP config");
    let resolved = resolve(&[layer], Trust::Trusted, None)
        .expect("resolved MCP config")
        .mcp;
    let (admission, provenance) = resolved
        .entries()
        .iter()
        .find_map(|(name, entry)| match entry {
            ResolvedMcpEntry::Configured { server, provenance } => Some((
                Arc::new(
                    McpServerAdmission::new(
                        name.clone(),
                        server.clone(),
                        McpTransportMaterial::stdio(Vec::new()),
                    )
                    .expect("server admission"),
                ),
                provenance.clone(),
            )),
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .expect("configured server");
    let structured_content = result.get("structuredContent").cloned();
    let mut result = rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
        "evidence only",
    )]);
    result.structured_content = structured_content;
    let factory = Arc::new(HookFactory {
        result,
        calls: Mutex::new(Vec::new()),
    });
    let inventory = McpCandidateInventory::new(
        McpCapabilities::tools(),
        None,
        vec![ego_core::generator::types::ToolDescriptor {
            name: "lookup".to_owned(),
            description: "hook fixture".to_owned(),
            parameters: lookup_schema(),
        }],
        Vec::new(),
        Vec::new(),
        McpCacheMetadata::default(),
        None,
    );
    let candidate = McpAdmittedCandidate::new(
        Arc::clone(&admission),
        McpCandidateIdentity::validated("server"),
        inventory,
        factory,
    )
    .expect("admitted candidate");
    let admission_port = Arc::new(OneCandidate {
        candidate: Mutex::new(Some(candidate)),
    });
    let journal = Arc::new(HookJournal::default());
    let registry = McpRegistryController::new(
        Arc::clone(&journal) as Arc<dyn McpRegistryJournal>,
        Arc::clone(&admission_port) as Arc<dyn McpCandidateAdmission>,
    )
    .expect("registry controller");
    let refresh = McpRegistryRefresh::new(
        vec![McpRegistryCandidate::ready(admission, provenance)],
        RegistryTrigger::Add,
        RegistryScope::All,
    )
    .expect("registry refresh");
    block_on(registry.reconcile(refresh)).expect("admitted registry publication");
    registry.capture().expect("immutable registry capture")
}

fn lookup_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["hook", "input"],
        "properties": {
            "hook": {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "version",
                    "hook_id",
                    "invocation_id",
                    "activation",
                    "event",
                    "phase",
                    "subject"
                ],
                "properties": {
                    "version": {"const": 1},
                    "hook_id": {"type": "string"},
                    "invocation_id": {"type": "string"},
                    "activation": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind"],
                        "properties": {
                            "kind": {"enum": ["persistent", "skill"]},
                            "invocation": {"type": "string"}
                        }
                    },
                    "event": {"type": "string"},
                    "phase": {"enum": ["pre_effect", "observer"]},
                    "subject": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["scope"],
                        "properties": {
                            "scope": {"enum": ["session", "turn", "item"]},
                            "turn": {"type": "string"},
                            "item": {"type": "string"}
                        }
                    },
                    "selector": {"type": "string"}
                }
            },
            "input": {"type": "object"}
        }
    })
}

#[derive(Default)]
struct HookJournal {
    events: Mutex<Vec<McpRegistryEvent>>,
}

impl McpRegistryJournal for HookJournal {
    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        self.events.lock().expect("journal").push(event);
        Ok(())
    }

    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        Ok(self.events.lock().expect("journal").clone())
    }

    fn begin_refresh(
        &self,
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
    ) -> Result<u64, McpRegistryJournalError> {
        self.events
            .lock()
            .expect("journal")
            .push(McpRegistryEvent::RefreshRequested {
                request_id,
                trigger,
                server_scope,
                config_revision: 1,
            });
        Ok(1)
    }
}

struct OneCandidate {
    candidate: Mutex<Option<McpAdmittedCandidate>>,
}

impl McpCandidateAdmission for OneCandidate {
    fn admit(
        &self,
        _request: McpCandidateRequest,
    ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
        let candidate = self.candidate.lock().expect("candidate").take();
        Box::pin(async move {
            candidate.ok_or_else(|| McpCandidateError::Invalid {
                cause: "candidate reused".to_owned(),
            })
        })
    }
}

struct HookFactory {
    result: rmcp::model::CallToolResult,
    calls: Mutex<Vec<(PreparedCall, EffectScope)>>,
}

struct EmptyBlobStore;

impl BlobStore for EmptyBlobStore {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        Err(BlobError::Limit { limit: 0 })
    }

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Err(BlobError::NotFound {
            reference: reference.id.clone(),
        })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        Box::pin(futures::future::ready(Err(BlobError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }

    fn collect_garbage(&self, _reachable: &[BlobRef]) -> Result<usize, BlobError> {
        Ok(0)
    }
}

static EMPTY_BLOBS: EmptyBlobStore = EmptyBlobStore;

struct EmptyEvidence;

impl EvidenceStore for EmptyEvidence {
    fn blob_store(&self) -> &dyn BlobStore {
        &EMPTY_BLOBS
    }

    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Err(EvidenceError::Limit { limit: 0 })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(futures::future::ready(Err(EvidenceError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }
}

struct HookProcess;

impl ProcessExecutor for HookProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(futures::future::ready(Err(ProcessError::Invariant(
            "MCP hook process must use execute_hook".to_owned(),
        ))))
    }
}

struct OfflineResolver;

impl Resolver for OfflineResolver {
    fn resolve(
        &self,
        _target: ego_core::permit::NetworkTarget,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        Box::pin(futures::future::ready(Err(
            TransportError::definitely_unsent(TransportFailure::Offline),
        )))
    }
}

struct OfflineOneHop;

impl OneHop for OfflineOneHop {
    fn begin_call(&self) -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }
    fn execute(
        &self,
        _connect: ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(futures::future::ready(Err(
            TransportError::definitely_unsent(TransportFailure::Offline),
        )))
    }
}

#[derive(Clone, Default)]
struct RecordingDuplexSpawner {
    calls: Arc<Mutex<Vec<SpawnSpec>>>,
}

impl RecordingDuplexSpawner {
    fn calls(&self) -> usize {
        self.calls.lock().expect("duplex calls").len()
    }
}

impl DuplexSpawner for RecordingDuplexSpawner {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        self.calls.lock().expect("duplex calls").push(spec);
        Box::pin(futures::future::ready(Ok(
            Box::new(MemoryDuplex::default()) as DuplexHandle,
        )))
    }
}

#[derive(Default)]
struct MemoryDuplex {
    input: Cursor<Vec<u8>>,
    output: Cursor<Vec<u8>>,
}

impl AsyncRead for MemoryDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.input).poll_read(cx, bytes)
    }
}

impl AsyncWrite for MemoryDuplex {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.output).poll_write(cx, bytes)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.output).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.output).poll_close(cx)
    }
}

impl DuplexSession for MemoryDuplex {
    fn delivery(&self) -> ego_core::generator::attempt::DeliveryCertainty {
        ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent
    }

    fn settle(self: Box<Self>) -> BoxFuture<'static, ego_core::permit::DuplexSettlement> {
        Box::pin(futures::future::ready(
            ego_core::permit::DuplexSettlement::new(
                DuplexEnd::Exited { code: 0 },
                CleanupCertainty::Complete,
                ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent,
                Err(EvidenceError::Limit { limit: 0 }),
            ),
        ))
    }

    fn interrupt(self: Box<Self>) -> BoxFuture<'static, ego_core::permit::DuplexSettlement> {
        Box::pin(futures::future::ready(
            ego_core::permit::DuplexSettlement::new(
                DuplexEnd::Interrupted,
                CleanupCertainty::Complete,
                ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent,
                Err(EvidenceError::Limit { limit: 0 }),
            ),
        ))
    }
}

impl McpOperationFactory for HookFactory {
    fn prepare(
        &self,
        admission: Arc<McpServerAdmission>,
        operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        if operation.operation() != McpOperationKind::CallTool {
            return Err(ToolError::InvalidParams(
                "hook requires a tool call".to_owned(),
            ));
        }
        let subject = McpSubject::Tool {
            server: admission.name().to_string(),
            name: "lookup".to_owned(),
        };
        let parameters = operation.parameters().clone();
        let spec = SpawnSpec::new(
            vec![OsString::from("mcp-server")],
            None,
            Some(Duration::from_secs(30)),
            EnvPolicy::exact(Vec::new()).expect("empty admitted MCP environment"),
        )
        .map_err(|error| ToolError::InvalidParams(error.to_string()))?;
        PreparedCall::mcp_stdio(
            subject,
            McpOperationKind::CallTool,
            vec![(
                ForeignToolId::new("server", "lookup")
                    .map_err(|error| ToolError::InvalidParams(error.to_string()))?,
                InvocationDigest::from_bytes(
                    &serde_json::to_vec(&parameters)
                        .map_err(|error| ToolError::InvalidParams(error.to_string()))?,
                ),
            )],
            McpAdmissionFingerprint::new("generation-1", "server/lookup", "schema-v1")
                .map_err(|error| ToolError::InvalidParams(error.to_string()))?,
            "stdio-transport-v1",
            admission.name().as_str(),
            spec,
            &parameters,
        )
        .map_err(|error| ToolError::InvalidParams(error.to_string()))
    }

    fn execute<'a, 'io>(
        &'a self,
        _admission: Arc<McpServerAdmission>,
        _call: PreparedCall,
        _io: &'a dyn GuardedIo<'io>,
        _generation: Arc<str>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async {
            Err(ToolError::InvalidParams(
                "ordinary MCP result is not a hook result".to_owned(),
            ))
        })
    }

    fn execute_hook<'a, 'io>(
        &'a self,
        _admission: Arc<McpServerAdmission>,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
        scope: EffectScope,
    ) -> McpFuture<'a, Result<ego_mcp::McpHookExecution, ToolError>>
    where
        'io: 'a,
    {
        self.calls
            .lock()
            .expect("hook calls")
            .push((call.clone(), scope));
        let result = self.result.clone();
        Box::pin(async move {
            let duplex = io
                .guarded_duplex(call)
                .await
                .map_err(|error| ToolError::InvalidParams(error.to_string()))?;
            let _settlement = duplex.settle().await;
            let evidence_bytes = serde_json::to_vec(&result)
                .map_err(|error| ToolError::InvalidParams(error.to_string()))?;
            Ok(ego_mcp::McpHookExecution::new(
                result,
                ego_core::store::evidence::EvidenceRef::new(
                    ego_core::store::evidence::EvidenceId::parse(ego_core::digest::sha256_hex(
                        &evidence_bytes,
                    ))
                    .expect("MCP hook evidence id"),
                    evidence_bytes.len() as u64,
                ),
                ego_core::generator::attempt::DeliveryCertainty::DefinitelySent,
            ))
        })
    }
}
