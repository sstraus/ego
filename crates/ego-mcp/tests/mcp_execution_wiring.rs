//! Registry captures retain fresh MCP operation factories and validate dynamic
//! inner calls before factory execution.

use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::future::BoxFuture;
use serde_json::{Value, json};

use ego_core::config::{
    McpConfigProvenance, McpServerConfig, ResolvedMcpConfig, ResolvedMcpEntry, Source, Trust,
};
use ego_core::generator::types::ToolDescriptor;
use ego_core::mcp::ToolExecution;
use ego_core::permit::{
    ForeignToolId, ForeignTransportTarget, GuardedIo, HttpHeader, HttpRequest, HttpRoute,
    HttpScheme, NetworkTarget, Op, PreparedCall, ToolError, ToolResult,
};
use ego_core::turn::{McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError};
use ego_mcp::{
    McpAdmittedCandidate, McpCacheMetadata, McpCandidateAdmission, McpCandidateError,
    McpCandidateIdentity, McpCandidateInventory, McpCapabilities, McpCollapsedCatalogue, McpFuture,
    McpOperation, McpOperationFactory, McpRegistryCandidate, McpRegistryController,
    McpRegistryRefresh, McpServerAdmission, McpTransportMaterial, RegistryScope, RegistryTrigger,
};

#[derive(Default)]
struct FakeFactory {
    prepares: Mutex<Vec<usize>>,
    executions: AtomicUsize,
}

impl McpOperationFactory for FakeFactory {
    fn prepare(
        &self,
        _: Arc<McpServerAdmission>,
        operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        let invocation_count = operation.invocations().len();
        self.prepares
            .lock()
            .expect("prepares")
            .push(invocation_count);
        if invocation_count == 2 {
            let [(outer, outer_digest), (inner, inner_digest)] = operation.invocations() else {
                unreachable!("the count was checked above")
            };
            assert_eq!(
                outer,
                &ForeignToolId::new("alpha", "call_tool").expect("outer")
            );
            assert_eq!(
                inner,
                &ForeignToolId::new("alpha", "weather").expect("inner")
            );
            let request = HttpRequest::new(
                "POST",
                "/mcp",
                vec![HttpHeader::new("accept", b"application/json".to_vec()).expect("header")],
                br#"{"jsonrpc":"2.0","method":"tools/call"}"#.to_vec(),
            )
            .expect("request");
            let route = HttpRoute::new(
                HttpScheme::Https,
                NetworkTarget::new("mcp.example", 443).expect("target"),
            )
            .expect("route");
            return Ok(PreparedCall::foreign_dispatch(
                outer.clone(),
                outer_digest.clone(),
                inner.clone(),
                inner_digest.clone(),
                ForeignTransportTarget::http(route),
                &request,
            ));
        }
        Ok(PreparedCall::new(
            "mcp-operation",
            vec![Op::Read {
                path: ego_core::permit::CanonPath::new("/fixture"),
            }],
        ))
    }

    fn execute<'a, 'io>(
        &'a self,
        _: Arc<McpServerAdmission>,
        _: PreparedCall,
        _: &'a dyn GuardedIo<'io>,
        _: Arc<str>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ToolExecution::Complete(ToolResult::builtin("fixture"))) })
    }
}

struct NoIo;
impl<'io> GuardedIo<'io> for NoIo {
    fn write(
        &self,
        _: &ego_core::permit::CanonPath,
        _: &[u8],
    ) -> Result<(), ego_core::permit::IoDenied> {
        unreachable!("fixture factory does not use I/O")
    }
    fn read(&self, _: &ego_core::permit::CanonPath) -> Result<Vec<u8>, ego_core::permit::IoDenied> {
        unreachable!("fixture factory does not use I/O")
    }
    fn list(
        &self,
        _: &ego_core::permit::ScanRequest,
    ) -> Result<ego_core::permit::Scan, ego_core::permit::IoDenied> {
        unreachable!("fixture factory does not use I/O")
    }
    fn search(
        &self,
        _: &ego_core::permit::ScanRequest,
        _: &dyn ego_core::permit::LineMatcher,
    ) -> Result<ego_core::permit::Found, ego_core::permit::IoDenied> {
        unreachable!("fixture factory does not use I/O")
    }
    fn exec<'a>(
        &'a self,
        _: &'a ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ego_core::permit::ProcessError>> {
        unreachable!("fixture factory does not use I/O")
    }
    fn read_evidence<'a>(
        &'a self,
        _: &'a ego_core::store::evidence::EvidenceRead,
    ) -> BoxFuture<'a, Result<ego_core::store::evidence::EvidencePage, ToolError>> {
        unreachable!("fixture factory does not use I/O")
    }
    fn guarded_http(
        &self,
        _: PreparedCall,
        _: ego_core::permit::HttpRequest,
    ) -> BoxFuture<
        'static,
        Result<ego_core::permit::OneHopResponse, ego_core::permit::TransportError>,
    > {
        unreachable!("fixture factory does not use I/O")
    }
    fn claim_mcp_http(
        &self,
        _: PreparedCall,
    ) -> Result<ego_core::permit::McpHttpClaim, ego_core::permit::TransportError> {
        unreachable!("fixture factory does not use I/O")
    }
    fn guarded_duplex(
        &self,
        _: PreparedCall,
    ) -> BoxFuture<'static, Result<ego_core::permit::DuplexHandle, ego_core::permit::TransportError>>
    {
        unreachable!("fixture factory does not use I/O")
    }
    fn store_evidence(
        &self,
        _: &[u8],
    ) -> Result<ego_core::store::evidence::EvidenceRef, ToolError> {
        unreachable!("fixture factory does not use I/O")
    }
}

fn descriptor(name: &str, parameters: Value) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        description: "fixture".into(),
        parameters,
    }
}
fn config() -> ego_core::config::ResolvedMcpConfig {
    let layer = ego_core::config::ConfigLayer::parse(
        Source::User,
        "[mcp.servers.alpha]\ntransport='stdio'\ncommand='mcp-fixture'\n",
    )
    .expect("layer");
    ego_core::config::resolve(&[layer], Trust::Trusted, None)
        .expect("config")
        .mcp
}
fn materialize_admissions(
    config: &ResolvedMcpConfig,
) -> Vec<(Arc<McpServerAdmission>, McpConfigProvenance)> {
    config
        .entries()
        .iter()
        .filter_map(|(name, entry)| match entry {
            ResolvedMcpEntry::Configured { server, provenance } => {
                let material = match server {
                    McpServerConfig::Stdio(_) => McpTransportMaterial::stdio(Vec::new()),
                    McpServerConfig::Http(_) => McpTransportMaterial::http(Vec::new()),
                };
                Some((
                    Arc::new(
                        McpServerAdmission::new(name.clone(), server.clone(), material)
                            .expect("admission"),
                    ),
                    provenance.clone(),
                ))
            }
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .collect()
}

fn alpha_admission() -> (Arc<McpServerAdmission>, McpConfigProvenance) {
    materialize_admissions(&config())
        .into_iter()
        .next()
        .expect("alpha admission")
}

fn refresh_for(
    admission: &Arc<McpServerAdmission>,
    provenance: &McpConfigProvenance,
    trigger: RegistryTrigger,
) -> McpRegistryRefresh {
    McpRegistryRefresh::new(
        vec![McpRegistryCandidate::ready(
            Arc::clone(admission),
            provenance.clone(),
        )],
        trigger,
        RegistryScope::All,
    )
    .expect("refresh")
}

fn empty_refresh(trigger: RegistryTrigger) -> McpRegistryRefresh {
    McpRegistryRefresh::new(Vec::new(), trigger, RegistryScope::All).expect("empty refresh")
}

fn static_candidate(
    admission: Arc<McpServerAdmission>,
    factory: Arc<FakeFactory>,
) -> McpAdmittedCandidate {
    McpAdmittedCandidate::new(
        admission,
        McpCandidateIdentity::validated("alpha"),
        McpCandidateInventory::new(
            McpCapabilities::tools(),
            vec![descriptor(
                "weather",
                json!({"type":"object","additionalProperties":false}),
            )],
            Vec::new(),
            Vec::new(),
            McpCacheMetadata::default(),
            None,
        ),
        factory,
    )
    .expect("candidate")
}
fn dynamic_candidate(
    admission: Arc<McpServerAdmission>,
    factory: Arc<FakeFactory>,
) -> McpAdmittedCandidate {
    let wrapper = json!({"type":"object","properties":{"tool_name":{"type":"string"},"arguments":{"type":"object"}},"required":["tool_name","arguments"],"additionalProperties":false});
    let inner = descriptor(
        "weather",
        json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}),
    );
    let catalogue = McpCollapsedCatalogue::new(vec![inner]).expect("catalogue");
    McpAdmittedCandidate::new(
        admission,
        McpCandidateIdentity::validated("alpha"),
        McpCandidateInventory::new(
            McpCapabilities::tools(),
            vec![
                descriptor(
                    "search_tools",
                    json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1}},"required":["query"],"additionalProperties":false}),
                ),
                descriptor(
                    "get_tool_schema",
                    json!({"type":"object","properties":{"tool_name":{"type":"string"}},"required":["tool_name"],"additionalProperties":false}),
                ),
                descriptor("call_tool", wrapper),
            ],
            Vec::new(),
            Vec::new(),
            McpCacheMetadata::default(),
            Some(catalogue),
        ),
        factory,
    )
    .expect("candidate")
}

#[derive(Default)]
struct FakeJournal {
    events: Mutex<Vec<McpRegistryEvent>>,
    epoch: Mutex<u64>,
}
impl McpRegistryJournal for FakeJournal {
    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        self.events.lock().expect("events").push(event);
        Ok(())
    }
    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        Ok(self.events.lock().expect("events").clone())
    }
    fn begin_refresh(
        &self,
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
    ) -> Result<u64, McpRegistryJournalError> {
        let mut epoch = self.epoch.lock().expect("epoch");
        *epoch += 1;
        self.append(McpRegistryEvent::RefreshRequested {
            request_id,
            trigger,
            server_scope,
            config_revision: *epoch,
        })?;
        Ok(*epoch)
    }
}
struct FakeAdmission {
    queue: Mutex<VecDeque<McpAdmittedCandidate>>,
}
impl FakeAdmission {
    fn one(candidate: McpAdmittedCandidate) -> Self {
        Self {
            queue: Mutex::new(VecDeque::from([candidate])),
        }
    }
}
impl McpCandidateAdmission for FakeAdmission {
    fn admit(
        &self,
        _: ego_mcp::McpCandidateRequest,
    ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
        let candidate = self.queue.lock().expect("queue").pop_front();
        Box::pin(async move {
            candidate.ok_or_else(|| McpCandidateError::Invalid {
                cause: "no candidate".into(),
            })
        })
    }
}

#[test]
fn published_capture_retains_old_operation_until_capture_drop() {
    let factory = Arc::new(FakeFactory::default());
    let (admission, provenance) = alpha_admission();
    let controller = McpRegistryController::new(
        Arc::new(FakeJournal::default()),
        Arc::new(FakeAdmission::one(static_candidate(
            Arc::clone(&admission),
            Arc::clone(&factory),
        ))),
    )
    .expect("controller");
    let first = futures::executor::block_on(controller.reconcile(refresh_for(
        &admission,
        &provenance,
        RegistryTrigger::Add,
    )))
    .expect("G1");
    let capture = first.capture().clone();
    let tool = capture
        .tools()
        .iter()
        .find(|tool| tool.descriptor().name == "weather")
        .cloned()
        .expect("weather");
    futures::executor::block_on(controller.reconcile(empty_refresh(RegistryTrigger::Remove)))
        .expect("G2");
    let fs = ego_core::permit::fake::FakeFs::default();
    let ctx = ego_core::permit::PrepareCtx {
        cwd: ego_core::permit::CanonPath::new("/fixture"),
        resolver: &fs,
    };
    let call = tool.prepare(json!({}), &ctx).expect("prepare");
    futures::executor::block_on(tool.execute(call, &NoIo)).expect("execute");
    assert_eq!(factory.executions.load(Ordering::SeqCst), 1);
    assert!(
        controller
            .publication()
            .capture()
            .tools()
            .iter()
            .all(|tool| tool.descriptor().name != "weather")
    );
    drop(tool);
    drop(capture);
}

#[test]
fn dynamic_capture_validates_inner_schema_and_executes_one_selected_call() {
    let factory = Arc::new(FakeFactory::default());
    let (admission, provenance) = alpha_admission();
    let controller = McpRegistryController::new(
        Arc::new(FakeJournal::default()),
        Arc::new(FakeAdmission::one(dynamic_candidate(
            Arc::clone(&admission),
            Arc::clone(&factory),
        ))),
    )
    .expect("controller");
    let publication = futures::executor::block_on(controller.reconcile(refresh_for(
        &admission,
        &provenance,
        RegistryTrigger::Add,
    )))
    .expect("G1");
    let tool = publication
        .capture()
        .tools()
        .iter()
        .find(|tool| tool.descriptor().name == "call_tool")
        .expect("call_tool");
    let fs = ego_core::permit::fake::FakeFs::default();
    let ctx = ego_core::permit::PrepareCtx {
        cwd: ego_core::permit::CanonPath::new("/fixture"),
        resolver: &fs,
    };
    let call = tool
        .prepare(
            json!({"tool_name":"weather","arguments":{"city":"Rome"}}),
            &ctx,
        )
        .expect("valid");
    assert_eq!(factory.prepares.lock().expect("prepares").as_slice(), &[2]);
    assert!(
        tool.prepare(json!({"tool_name":"weather","arguments":{"city":7}}), &ctx)
            .is_err()
    );
    for management in ["search_tools", "get_tool_schema", "call_tool"] {
        assert!(
            tool.prepare(json!({"tool_name":management,"arguments":{}}), &ctx)
                .is_err(),
            "management tool {management} cannot recurse through call_tool"
        );
    }
    futures::executor::block_on(tool.execute(call, &NoIo)).expect("execute");
    assert_eq!(factory.executions.load(Ordering::SeqCst), 1);
}

#[test]
fn captured_operation_is_empty_after_successor_removes_server() {
    let factory = Arc::new(FakeFactory::default());
    let (admission, provenance) = alpha_admission();
    let controller = McpRegistryController::new(
        Arc::new(FakeJournal::default()),
        Arc::new(FakeAdmission::one(static_candidate(
            Arc::clone(&admission),
            factory,
        ))),
    )
    .expect("controller");
    futures::executor::block_on(controller.reconcile(refresh_for(
        &admission,
        &provenance,
        RegistryTrigger::Add,
    )))
    .expect("G1");
    futures::executor::block_on(controller.reconcile(empty_refresh(RegistryTrigger::Reload)))
        .expect("G2");
    assert!(
        controller
            .publication()
            .capture()
            .tools()
            .iter()
            .all(|tool| tool.descriptor().name != "weather")
    );
}
