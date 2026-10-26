//! Final-contract tests for one immutable admitted MCP registry.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use ego_core::config::{
    ConfigLayer, McpConfigProvenance, ResolvedMcpConfig, ResolvedMcpEntry, Source, Trust, resolve,
};
use ego_core::config::{McpHttpAuth, McpServerConfig, McpServerName};
use ego_core::mcp::ToolExecution;
use ego_core::permit::{CanonPath, Op, PreparedCall, ToolError};
use ego_core::suspension::McpCapturePin;
use ego_core::turn::{McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError};
use ego_mcp::{
    McpAdmittedCandidate, McpCandidateAdmission, McpCandidateError, McpCandidateInventory,
    McpCandidateRequest, McpFailure, McpOperation, McpOperationFactory, McpRegistryCandidate,
    McpRegistryController, McpRegistryError, McpRegistryPublication, McpRegistryRefresh,
    McpServerAdmission, McpTransportMaterial, RegistryScope, RegistryTrigger,
};
use futures::future::BoxFuture;
use serde_json::json;

const SERVER: &str = "alpha";

#[derive(Default)]
struct JournalState {
    events: Vec<McpRegistryEvent>,
    next_epoch: u64,
    fail_begin: bool,
    fail_append: bool,
    private_diagnostics: Vec<(String, String)>,
    fail_private: bool,
}
#[derive(Default)]
struct FakeJournal {
    state: Mutex<JournalState>,
}
impl FakeJournal {
    fn events(&self) -> Vec<McpRegistryEvent> {
        self.state.lock().expect("journal state").events.clone()
    }
    fn fail_begin(&self) {
        self.state.lock().expect("journal state").fail_begin = true;
    }
}
impl McpRegistryJournal for FakeJournal {
    fn record_private_diagnostic(
        &self,
        server: &str,
        cause: &str,
    ) -> Result<(), McpRegistryJournalError> {
        let mut state = self.state.lock().expect("journal state");
        if state.fail_private {
            return Err(McpRegistryJournalError::new(
                ego_core::turn::McpRegistryJournalOperation::PrivateDiagnostic,
                ego_core::turn::McpRegistryJournalErrorKind::Unavailable,
            ));
        }
        state
            .private_diagnostics
            .push((server.to_owned(), cause.to_owned()));
        Ok(())
    }
    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        let mut state = self.state.lock().expect("journal state");
        if state.fail_append {
            return Err(McpRegistryJournalError::new(
                ego_core::turn::McpRegistryJournalOperation::Append,
                ego_core::turn::McpRegistryJournalErrorKind::Unavailable,
            ));
        }
        state.events.push(event);
        Ok(())
    }
    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        Ok(self.events())
    }
    fn begin_refresh(
        &self,
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
    ) -> Result<u64, McpRegistryJournalError> {
        let mut state = self.state.lock().expect("journal state");
        if state.fail_begin {
            return Err(McpRegistryJournalError::new(
                ego_core::turn::McpRegistryJournalOperation::BeginRefresh,
                ego_core::turn::McpRegistryJournalErrorKind::Unavailable,
            ));
        }
        state.next_epoch = state
            .next_epoch
            .checked_add(1)
            .expect("fixture epoch overflow");
        let epoch = state.next_epoch;
        state.events.push(McpRegistryEvent::RefreshRequested {
            request_id,
            trigger,
            server_scope,
            config_revision: epoch,
        });
        Ok(epoch)
    }
}

#[derive(Clone)]
enum Outcome {
    Candidate(CandidateFixture),
    Invalid(String),
    AuthorizationRequired {
        issuer: String,
        protected_resource: String,
        scopes: Vec<String>,
    },
}

#[derive(Clone)]
struct CandidateFixture {
    name: String,
    tools: Vec<Arc<CountingTool>>,
    instructions: Option<String>,
}
#[derive(Default)]
struct AdmissionState {
    queue: VecDeque<Outcome>,
    requests: Vec<McpCandidateRequest>,
    active: usize,
    max_active: usize,
}
struct FakeAdmission {
    state: Arc<Mutex<AdmissionState>>,
    gate: Arc<Mutex<Option<Arc<Notify>>>>,
}
impl FakeAdmission {
    fn push(&self, outcome: Outcome) {
        self.state
            .lock()
            .expect("admission state")
            .queue
            .push_back(outcome);
    }
    fn requests(&self) -> Vec<McpCandidateRequest> {
        self.state.lock().expect("admission state").requests.clone()
    }
    fn block_next(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.gate.lock().expect("admission gate") = Some(Arc::clone(&gate));
        gate
    }
}
impl McpCandidateAdmission for FakeAdmission {
    fn admit(
        &self,
        request: McpCandidateRequest,
    ) -> ego_mcp::McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
        let state = Arc::clone(&self.state);
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            let outcome = {
                let mut state = state.lock().expect("admission state");
                let candidate_admission = request.admission();
                state.requests.push(request);
                state.active += 1;
                state.max_active = state.max_active.max(state.active);
                let gate = gate.lock().expect("admission gate").take();
                let outcome = state.queue.pop_front().expect("admission outcome");
                (gate, outcome, candidate_admission)
            };
            if let Some(gate) = outcome.0 {
                gate.notified().await;
            }
            let outcome = {
                let mut state = state.lock().expect("admission state");
                state.active -= 1;
                (outcome.1, outcome.2)
            };
            match outcome {
                (Outcome::Candidate(fixture), admission) => {
                    assert_eq!(fixture.name, admission.name().as_str());
                    Ok(admitted_candidate_saying(
                        admission,
                        fixture.tools,
                        fixture.instructions.as_deref(),
                    ))
                }
                (Outcome::Invalid(cause), _) => Err(McpCandidateError::Invalid { cause }),
                (
                    Outcome::AuthorizationRequired {
                        issuer,
                        protected_resource,
                        scopes,
                    },
                    _,
                ) => Err(McpCandidateError::AuthorizationRequired {
                    issuer,
                    protected_resource,
                    scopes,
                }),
            }
        })
    }
}

struct CountingTool {
    name: String,
    dropped: Arc<AtomicUsize>,
    parameters: serde_json::Value,
}
impl CountingTool {
    fn new(name: &str, dropped: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            dropped,
            parameters: json!({"type":"object","additionalProperties":false}),
        })
    }

    fn with_parameters(
        name: &str,
        dropped: Arc<AtomicUsize>,
        parameters: serde_json::Value,
    ) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            dropped,
            parameters,
        })
    }
}
impl Drop for CountingTool {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl CountingTool {
    fn generator_descriptor(&self) -> ego_core::generator::types::ToolDescriptor {
        ego_core::generator::types::ToolDescriptor {
            name: self.name.clone(),
            description: "counting MCP test tool".to_owned(),
            parameters: self.parameters.clone(),
        }
    }
}

struct CountingFactory {
    tools: Vec<Arc<CountingTool>>,
}

impl McpOperationFactory for CountingFactory {
    fn prepare(
        &self,
        _admission: Arc<McpServerAdmission>,
        _operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall::new(
            "counted",
            Op::Read {
                path: CanonPath::new("/fixture"),
            },
        ))
    }

    fn execute<'a, 'io>(
        &'a self,
        _admission: Arc<McpServerAdmission>,
        _call: PreparedCall,
        _io: &'a dyn ego_core::permit::GuardedIo<'io>,
        _generation: Arc<str>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        let _retained = self.tools.len();
        Box::pin(async {
            Ok(ToolExecution::Complete(
                ego_core::permit::ToolResult::builtin("counted"),
            ))
        })
    }
}

fn config() -> ego_core::config::ResolvedMcpConfig {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.alpha]\ntransport = 'stdio'\ncommand = 'mcp-alpha'\nargs = ['--current']\n",
    )
    .expect("valid MCP layer");
    resolve(&[layer], Trust::Trusted, None)
        .expect("resolved MCP config")
        .mcp
}
fn materialize_admissions(
    config: &ResolvedMcpConfig,
) -> Vec<(Arc<McpServerAdmission>, McpConfigProvenance)> {
    config
        .entries()
        .iter()
        .filter_map(|(name, entry)| match entry {
            ResolvedMcpEntry::Configured {
                server, provenance, ..
            } => {
                let material = match server {
                    McpServerConfig::Stdio(config) => McpTransportMaterial::stdio(
                        config
                            .inherit_env
                            .iter()
                            .map(|name| {
                                ego_mcp::McpEnvironmentBinding::from_native(
                                    name.clone(),
                                    std::ffi::OsString::from(format!("fixture-{}", name.as_str())),
                                )
                                .expect("fixture environment material")
                            })
                            .collect(),
                    ),
                    McpServerConfig::Http(_) => McpTransportMaterial::http(Vec::new()),
                    McpServerConfig::Acp { .. } => McpTransportMaterial::acp(),
                };
                Some((
                    Arc::new(
                        McpServerAdmission::new(name.clone(), server.clone(), material)
                            .expect("configured admission"),
                    ),
                    provenance.clone(),
                ))
            }
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .collect()
}

fn admissions(config: ResolvedMcpConfig) -> Vec<McpRegistryCandidate> {
    materialize_admissions(&config)
        .into_iter()
        .map(|(admission, provenance)| McpRegistryCandidate::ready(admission, provenance))
        .collect()
}

fn refresh(trigger: RegistryTrigger) -> McpRegistryRefresh {
    McpRegistryRefresh::new(admissions(config()), trigger, RegistryScope::All)
        .expect("registry refresh")
}
fn empty_config() -> ego_core::config::ResolvedMcpConfig {
    let layer = ConfigLayer::parse(Source::User, "").expect("empty valid MCP layer");
    resolve(&[layer], Trust::Trusted, None)
        .expect("resolved empty MCP config")
        .mcp
}
fn config_two() -> ego_core::config::ResolvedMcpConfig {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.zeta]\ntransport = 'stdio'\ncommand = 'mcp-zeta'\n\n[mcp.servers.alpha]\ntransport = 'stdio'\ncommand = 'mcp-alpha'\n",
    )
    .expect("valid MCP layer");
    resolve(&[layer], Trust::Trusted, None)
        .expect("resolved MCP config")
        .mcp
}

fn config_changed() -> ego_core::config::ResolvedMcpConfig {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.alpha]\ntransport = 'stdio'\ncommand = 'mcp-alpha'\nargs = ['--changed']\n",
    )
    .expect("valid changed MCP layer");
    resolve(&[layer], Trust::Trusted, None)
        .expect("resolved changed MCP config")
        .mcp
}

fn http_config() -> ego_core::config::ResolvedMcpConfig {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.alpha]\ntransport = 'http'\nurl = 'https://mcp-alpha.example.test/mcp'\n",
    )
    .expect("valid HTTP MCP layer");
    resolve(&[layer], Trust::Trusted, None)
        .expect("resolved HTTP MCP config")
        .mcp
}
fn refresh_with(
    config: ego_core::config::ResolvedMcpConfig,
    trigger: RegistryTrigger,
) -> McpRegistryRefresh {
    McpRegistryRefresh::new(admissions(config), trigger, RegistryScope::All)
        .expect("registry refresh")
}
fn refresh_zeta(trigger: RegistryTrigger) -> McpRegistryRefresh {
    McpRegistryRefresh::new(
        admissions(config_two()),
        trigger,
        RegistryScope::Server("zeta".parse().expect("server name")),
    )
    .expect("registry refresh")
}

fn fixture_admission(outcomes: Vec<Outcome>) -> Arc<FakeAdmission> {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    for outcome in outcomes {
        admission.push(outcome);
    }
    admission
}

async fn one_publication(
    config: ResolvedMcpConfig,
    tools: Vec<Arc<CountingTool>>,
) -> (Arc<McpRegistryController>, McpRegistryPublication) {
    let admission = fixture_admission(vec![Outcome::Candidate(candidate(SERVER, tools))]);
    let (registry, _) = controller(admission);
    let publication = registry
        .reconcile(refresh_with(config, RegistryTrigger::Add))
        .await
        .expect("fixture publication");
    (registry, publication)
}

fn alpha_admission() -> (Arc<McpServerAdmission>, McpConfigProvenance) {
    materialize_admissions(&config())
        .into_iter()
        .next()
        .expect("alpha admission")
}

fn rejected(name: &str, config: McpServerConfig, failure: McpFailure) -> McpRegistryCandidate {
    McpRegistryCandidate::rejected(
        name.parse::<McpServerName>().expect("server name"),
        config,
        McpConfigProvenance::Layer {
            source: Source::User,
        },
        failure,
    )
}
fn candidate(name: &str, tools: Vec<Arc<CountingTool>>) -> CandidateFixture {
    CandidateFixture {
        name: name.to_owned(),
        tools,
        instructions: None,
    }
}
/// A candidate whose server describes itself at `initialize`.
fn candidate_saying(
    name: &str,
    tools: Vec<Arc<CountingTool>>,
    instructions: &str,
) -> CandidateFixture {
    CandidateFixture {
        name: name.to_owned(),
        tools,
        instructions: Some(instructions.to_owned()),
    }
}
fn admitted_candidate_saying(
    admission: Arc<McpServerAdmission>,
    tools: Vec<Arc<CountingTool>>,
    instructions: Option<&str>,
) -> McpAdmittedCandidate {
    let descriptors = tools
        .iter()
        .map(|tool| tool.generator_descriptor())
        .collect();
    let factory = Arc::new(CountingFactory { tools });
    McpAdmittedCandidate::new(
        Arc::clone(&admission),
        ego_mcp::McpCandidateIdentity::validated(admission.name().as_str()),
        McpCandidateInventory::new(
            ego_mcp::McpCapabilities::tools(),
            instructions.map(str::to_owned),
            descriptors,
            Vec::new(),
            Vec::new(),
            ego_mcp::McpCacheMetadata::default(),
            None,
        ),
        factory,
    )
    .expect("validated candidate")
}
fn controller(admission: Arc<FakeAdmission>) -> (Arc<McpRegistryController>, Arc<FakeJournal>) {
    let journal = Arc::new(FakeJournal::default());
    let registry = McpRegistryController::new(
        Arc::clone(&journal) as Arc<dyn McpRegistryJournal>,
        Arc::clone(&admission) as Arc<dyn McpCandidateAdmission>,
    )
    .expect("registry controller");
    (registry, journal)
}

#[tokio::test(flavor = "current_thread")]
async fn every_management_and_notification_trigger_enters_reconcile() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    for _ in 0..10 {
        admission.push(Outcome::Candidate(candidate(
            SERVER,
            vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
        )));
    }
    let (registry, journal) = controller(admission);
    for trigger in [
        RegistryTrigger::Add,
        RegistryTrigger::Remove,
        RegistryTrigger::Login,
        RegistryTrigger::Reload,
        RegistryTrigger::ConfigChanged,
        RegistryTrigger::AuthorizationSucceeded,
        RegistryTrigger::TtlExpired,
        RegistryTrigger::ToolsListChanged,
        RegistryTrigger::Rescan,
        RegistryTrigger::Recovery,
    ] {
        registry
            .reconcile(refresh(trigger))
            .await
            .expect("reconcile trigger");
    }
    assert_eq!(
        journal
            .events()
            .iter()
            .filter(|event| matches!(event, McpRegistryEvent::RefreshRequested { .. }))
            .count(),
        10
    );
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_candidate_keeps_predecessor_until_whole_successor_publication() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate(
        SERVER,
        vec![
            CountingTool::new("z_tool", Arc::clone(&dropped)),
            CountingTool::new("a_tool", Arc::clone(&dropped)),
        ],
    )));
    let (registry, _) = controller(Arc::clone(&admission));
    registry
        .reconcile(refresh(RegistryTrigger::Reload))
        .await
        .expect("first publication");
    let first = registry.publication();
    assert_eq!(first.admitted_tools(), ["a_tool", "z_tool"]);
    let first_status = registry.status();
    let server_name = SERVER.parse().expect("server name");
    let admitted_server = first_status.server(&server_name).expect("admitted status");
    assert!(matches!(
        admitted_server.state(),
        ego_mcp::McpServerState::Admitted
    ));
    assert_eq!(admitted_server.name(), &server_name);
    assert_eq!(admitted_server.generation(), Some(first.generation()));
    assert!(admitted_server.capabilities().is_some());
    let _ = admitted_server.cache_source_revision();
    assert_eq!(
        admitted_server.cache_freshness(),
        ego_mcp::McpCacheFreshness::Uncached
    );
    assert!(admitted_server.authorization().is_none());
    assert!(admitted_server.diagnostic().is_none());
    admission.push(Outcome::Invalid("duplicate tool identity".to_owned()));
    let failed = registry
        .reconcile(refresh(RegistryTrigger::ToolsListChanged))
        .await
        .expect("per-server failure publishes retained successor status");
    assert_ne!(failed.generation(), first.generation());
    assert_eq!(failed.predecessor(), Some(first.generation()));
    assert_eq!(failed.admitted_tools(), ["a_tool", "z_tool"]);
    let failed_status = registry.status();
    assert!(failed_status.is_degraded());
    assert_eq!(failed_status.servers().len(), 1);
    assert!(failed_status.server(&server_name).is_some());
    let degraded_server = failed_status.server(&server_name).expect("degraded status");
    assert!(matches!(
        degraded_server.state(),
        ego_mcp::McpServerState::Degraded
    ));
    assert_eq!(degraded_server.generation(), Some(first.generation()));
    let degraded_diagnostic = degraded_server.diagnostic().expect("degraded diagnostic");
    assert_eq!(degraded_diagnostic.failure(), McpFailure::InvalidValue);
    assert!(!degraded_diagnostic.message().is_empty());
    assert!(!failed_status.mcp_errors().is_empty());
    assert!(
        failed_status
            .mcp_errors()
            .iter()
            .all(|error| !error.contains("access_token") && !error.contains("client_secret"))
    );
    admission.push(Outcome::Candidate(candidate(
        SERVER,
        vec![CountingTool::new("successor_tool", Arc::clone(&dropped))],
    )));
    let release = admission.block_next();
    let pending_registry = Arc::clone(&registry);
    let pending = tokio::spawn(async move {
        pending_registry
            .reconcile(refresh(RegistryTrigger::Recovery))
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(registry.publication().generation(), failed.generation());
    assert_eq!(
        registry.publication().admitted_tools(),
        ["a_tool", "z_tool"]
    );
    release.notify_one();
    pending
        .await
        .expect("reconcile task")
        .expect("successor publication");
    assert_ne!(registry.publication().generation(), first.generation());
    assert_eq!(registry.publication().admitted_tools(), ["successor_tool"]);
    assert!(admission.state.lock().expect("admission state").max_active <= 1);
}

#[tokio::test(flavor = "current_thread")]
async fn old_capture_retains_exact_arc_until_capture_drop_after_removal() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate(
        SERVER,
        vec![CountingTool::new("old_tool", Arc::clone(&dropped))],
    )));
    let (registry, _) = controller(Arc::clone(&admission));
    registry
        .reconcile(refresh(RegistryTrigger::Add))
        .await
        .expect("initial publication");
    let old_capture = registry.admitted_tools().capture().expect("old capture");
    registry
        .reconcile(refresh_with(empty_config(), RegistryTrigger::Remove))
        .await
        .expect("removal publication");
    assert_eq!(old_capture.generation(), "generation-1");
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    assert_ne!(
        registry.publication().generation(),
        old_capture.generation()
    );
    assert!(registry.publication().admitted_tools().is_empty());
    assert_eq!(admission.requests().len(), 1);
    drop(old_capture);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn new_invalid_server_exposes_no_tool_and_typed_auth_is_sanitized() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    let raw_backend_error = "backend-secret=do-not-publish";
    admission.push(Outcome::Invalid(raw_backend_error.to_owned()));
    let (registry, journal) = controller(Arc::clone(&admission));
    let invalid_publication = registry
        .reconcile(refresh(RegistryTrigger::Add))
        .await
        .expect("cold per-server failure publishes status");
    assert_eq!(invalid_publication.generation(), "generation-1");
    assert!(invalid_publication.admitted_tools().is_empty());
    let absent = registry.status();
    let absent_name = SERVER.parse().expect("server name");
    let absent_server = absent.server(&absent_name).expect("absent status");
    assert!(matches!(
        absent_server.state(),
        ego_mcp::McpServerState::Absent
    ));
    assert_eq!(
        absent_server.cache_freshness(),
        ego_mcp::McpCacheFreshness::Uncached
    );
    assert!(absent_server.diagnostic().is_some());
    assert_eq!(absent_server.name(), &absent_name);
    assert!(absent_server.generation().is_none());
    assert!(absent_server.capabilities().is_none());
    assert!(absent_server.cache_source_revision().is_none());
    assert!(absent_server.authorization().is_none());
    let diagnostic = absent_server.diagnostic().expect("diagnostic");
    assert_eq!(diagnostic.failure(), McpFailure::InvalidValue);
    assert!(!diagnostic.message().is_empty());
    let errors = absent.mcp_errors();
    assert!(
        errors
            .iter()
            .all(|error| !error.contains("do-not-publish") && !error.contains(raw_backend_error))
    );
    assert_eq!(
        errors,
        ["MCP server 'alpha': server configuration is invalid; inspect with 'ego mcp ls'"]
    );
    admission.push(Outcome::AuthorizationRequired {
        issuer: "https://issuer.example".to_owned(),
        protected_resource: "https://mcp.example".to_owned(),
        scopes: vec!["tools".to_owned()],
    });
    let auth_publication = registry
        .reconcile(refresh(RegistryTrigger::Login))
        .await
        .expect("authorization-required status publishes");
    assert_ne!(
        auth_publication.generation(),
        invalid_publication.generation()
    );
    assert_eq!(
        auth_publication.predecessor(),
        Some(invalid_publication.generation())
    );
    assert!(auth_publication.admitted_tools().is_empty());
    let auth_status = registry.status();
    let auth_server = auth_status.server(&absent_name).expect("auth status");
    assert!(matches!(
        auth_server.state(),
        ego_mcp::McpServerState::AuthorizationRequired
    ));
    let authorization = auth_server.authorization().expect("authorization facts");
    assert_eq!(auth_server.name(), &absent_name);
    assert!(auth_server.generation().is_none());
    assert!(auth_server.capabilities().is_none());
    assert!(auth_server.cache_source_revision().is_none());
    assert_eq!(
        auth_server.cache_freshness(),
        ego_mcp::McpCacheFreshness::Uncached
    );
    assert_eq!(authorization.issuer(), "https://issuer.example");
    assert_eq!(authorization.protected_resource(), "https://mcp.example");
    assert_eq!(authorization.scopes(), ["tools".to_owned()]);
    assert_eq!(authorization.request_correlation(), "request-2");
    assert_eq!(
        auth_server.diagnostic().expect("auth diagnostic").failure(),
        McpFailure::Authorization
    );
    // Catches: the authorization notice sends a REPL user out to the shell or
    // leaves a placeholder instead of the configured server name.
    assert_eq!(
        auth_server.diagnostic().unwrap().message(),
        "authorization is required; run '/mcp login alpha'"
    );
    let events = journal.events();
    assert!(events.iter().any(|event| matches!(event,
        McpRegistryEvent::AuthorizationRequired { server, issuer, protected_resource, scopes, request_correlation }
            if server == SERVER && issuer == "https://issuer.example" && protected_resource == "https://mcp.example"
                && scopes == &["tools".to_owned()] && request_correlation == "request-2"
    )));
    let serialized = format!("{:?}", events);
    assert!(!serialized.contains("access_token"));
    assert!(!serialized.contains("client_secret"));
}

#[tokio::test(flavor = "current_thread")]
async fn admission_requests_are_complete_and_refresh_admission_is_async() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate(
        SERVER,
        vec![CountingTool::new(
            "alpha_tool",
            Arc::new(AtomicUsize::new(0)),
        )],
    )));
    let (registry, _) = controller(Arc::clone(&admission));
    registry
        .reconcile(refresh(RegistryTrigger::Reload))
        .await
        .expect("reconcile");
    let requests = admission.requests();
    assert_eq!(requests.len(), 1);
    let request = requests.first().expect("recorded request");
    assert_eq!(request.name().as_str(), SERVER);
    assert_eq!(
        request.config(),
        config()
            .configured(&SERVER.parse().expect("server name"))
            .expect("server config")
    );
    assert!(matches!(
        request.provenance(),
        McpConfigProvenance::Layer {
            source: Source::User
        }
    ));
}

/// Catches: duplicate stdio tool names losing their private cause and reporting only
/// a generic publication failure, including on refresh and optional-log failure.
#[tokio::test(flavor = "current_thread")]
async fn duplicate_stdio_tool_names_report_conflict_and_retain_private_cause() {
    const MESSAGE: &str =
        "MCP tool names conflict across servers; attach only one server for each tool name";
    const PRIVATE_TOOL: &str = "private-tool-name";
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.design-test]\ntransport = 'stdio'\ncommand = '/usr/bin/python3'\nargs = ['tap.py', 'ego']\ninherit_env = ['TUIC_SOCKET', 'TUIC_SESSION']\n\n[mcp.servers.tuic]\ntransport = 'stdio'\ncommand = 'tuic-bridge'\n",
    )
    .expect("same stdio candidate shape with enabled configured bridge");
    let config = resolve(&[layer], Trust::Trusted, None).unwrap().mcp;
    for (retain_predecessor, fail_private) in [(false, false), (true, false), (true, true)] {
        let admission = fixture_admission(Vec::new());
        let (registry, journal) = controller(Arc::clone(&admission));
        if retain_predecessor {
            // Distinct names remain valid before a conflicting refresh.
            for (server, tool) in [("design-test", "unique_one"), ("tuic", "unique_two")] {
                admission.push(Outcome::Candidate(candidate(
                    server,
                    vec![CountingTool::new(tool, Arc::new(AtomicUsize::new(0)))],
                )));
            }
            registry
                .reconcile(refresh_with(config.clone(), RegistryTrigger::Add))
                .await
                .unwrap();
            assert_eq!(
                registry.publication().admitted_tools(),
                ["unique_one", "unique_two"]
            );
        }
        let predecessor = registry.publication();
        journal.state.lock().unwrap().fail_private = fail_private;
        for server in ["design-test", "tuic"] {
            admission.push(Outcome::Candidate(candidate(
                server,
                vec![CountingTool::new(
                    PRIVATE_TOOL,
                    Arc::new(AtomicUsize::new(0)),
                )],
            )));
        }
        let error = registry
            .reconcile(refresh_with(config.clone(), RegistryTrigger::Reload))
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            format!("invalid MCP candidate: {MESSAGE}")
        );
        assert_eq!(
            registry.publication().generation(),
            predecessor.generation()
        );
        assert_eq!(
            registry.publication().admitted_tools(),
            predecessor.admitted_tools()
        );
        for server in registry.status().servers() {
            assert_eq!(server.diagnostic().unwrap().message(), MESSAGE);
            assert_eq!(
                server.state(),
                if retain_predecessor {
                    ego_mcp::McpServerState::Degraded
                } else {
                    ego_mcp::McpServerState::Absent
                }
            );
        }
        let events = journal.events();
        assert!(
            matches!(events.last(), Some(McpRegistryEvent::RefreshFailed { cause, .. }) if cause == MESSAGE)
        );
        assert!(!format!("{events:?}").contains(PRIVATE_TOOL));
        assert!(!format!("{:?}", registry.status().mcp_errors()).contains(PRIVATE_TOOL));
        let state = journal.state.lock().unwrap();
        if fail_private {
            assert!(state.private_diagnostics.is_empty());
        } else {
            assert_eq!(state.private_diagnostics.len(), 2);
            assert!(
                state
                    .private_diagnostics
                    .iter()
                    .all(|(_, cause)| cause.contains(PRIVATE_TOOL))
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn journal_failure_prevents_controller_publication() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate(
        SERVER,
        vec![CountingTool::new(
            "never_admitted",
            Arc::new(AtomicUsize::new(0)),
        )],
    )));
    let journal = Arc::new(FakeJournal::default());
    journal.fail_begin();
    let registry = McpRegistryController::new(
        Arc::clone(&journal) as Arc<dyn McpRegistryJournal>,
        Arc::clone(&admission) as Arc<dyn McpCandidateAdmission>,
    )
    .expect("registry controller");
    let before = registry.publication();
    let result: Result<_, McpRegistryError> =
        registry.reconcile(refresh(RegistryTrigger::Recovery)).await;
    assert!(result.is_err());
    assert_eq!(registry.publication().generation(), before.generation());
    assert!(admission.requests().is_empty());
    assert!(
        !journal
            .events()
            .iter()
            .any(|event| matches!(event, McpRegistryEvent::SnapshotPublished { .. }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn one_live_admission_arc_reaches_request_capture_and_status() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate(
        SERVER,
        vec![CountingTool::new(
            "alpha_tool",
            Arc::new(AtomicUsize::new(0)),
        )],
    )));
    let (registry, _) = controller(Arc::clone(&admission));
    let publication = registry
        .reconcile(refresh(RegistryTrigger::Add))
        .await
        .expect("publication");

    let request_admission = admission
        .requests()
        .first()
        .expect("candidate request")
        .admission();
    let captured_admission = publication
        .capture()
        .servers()
        .first()
        .expect("captured server")
        .admission();
    assert!(
        std::ptr::eq(request_admission.as_ref(), captured_admission),
        "request and capture did not retain the same live admission authority"
    );

    let status = registry.status();
    assert_eq!(status.servers().len(), 1);
    let server_name = SERVER.parse().expect("server name");
    assert!(status.server(&server_name).is_some());
    assert!(!status.is_degraded());
    assert!(status.mcp_errors().is_empty());
    assert!(format!("{:?}", status.servers()).contains("Admitted"));
}

#[tokio::test(flavor = "current_thread")]
async fn status_servers_are_canonicalized_by_name_across_mixed_outcomes() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate(
        "alpha",
        vec![CountingTool::new(
            "alpha_tool",
            Arc::new(AtomicUsize::new(0)),
        )],
    )));
    let (registry, _) = controller(Arc::clone(&admission));
    let alpha_publication = registry
        .reconcile(refresh(RegistryTrigger::Add))
        .await
        .expect("alpha publication");
    admission.push(Outcome::Invalid(
        "zeta-backend-secret=zeta-value".to_owned(),
    ));
    let zeta_publication = registry
        .reconcile(refresh_zeta(RegistryTrigger::Reload))
        .await
        .expect("per-server failure publishes mixed status");
    assert_ne!(
        zeta_publication.generation(),
        alpha_publication.generation()
    );
    assert_eq!(
        zeta_publication.predecessor(),
        Some(alpha_publication.generation())
    );
    assert_eq!(zeta_publication.admitted_servers(), ["alpha".to_owned()]);

    let status = registry.status();
    let names: Vec<_> = status
        .servers()
        .iter()
        .map(|server| server.name().to_string())
        .collect();
    assert_eq!(names, ["alpha", "zeta"]);
    assert!(status.is_degraded());
    assert!(matches!(
        status
            .server(&"alpha".parse().expect("server name"))
            .expect("alpha status")
            .state(),
        ego_mcp::McpServerState::Admitted
    ));
    let zeta = status
        .server(&"zeta".parse().expect("server name"))
        .expect("zeta status");
    assert!(matches!(zeta.state(), ego_mcp::McpServerState::Absent));
    assert!(zeta.diagnostic().is_some());
    let errors = status.mcp_errors();
    assert!(errors.iter().all(|error| !error.contains("zeta-value")));
    assert_eq!(
        errors,
        ["MCP server 'zeta': server configuration is invalid; inspect with 'ego mcp ls'"]
    );
}

fn server_config(name: &str) -> McpServerConfig {
    McpServerConfig::stdio(format!("mcp-{name}"), Vec::new(), None, Vec::new())
        .expect("server config")
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_materialization_candidate_does_not_abort_valid_admission() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate(
        SERVER,
        vec![CountingTool::new(
            "alpha_tool",
            Arc::new(AtomicUsize::new(0)),
        )],
    )));
    let (alpha, provenance) = alpha_admission();
    let refresh = McpRegistryRefresh::new(
        vec![
            rejected("beta", server_config("beta"), McpFailure::Credential),
            McpRegistryCandidate::ready(alpha, provenance),
        ],
        RegistryTrigger::ConfigChanged,
        RegistryScope::All,
    )
    .expect("mixed refresh");
    let (registry, _) = controller(admission);

    let publication = registry
        .reconcile(refresh)
        .await
        .expect("valid server publishes");
    assert_eq!(publication.admitted_servers(), [SERVER.to_owned()]);
    assert_eq!(publication.admitted_tools(), ["alpha_tool".to_owned()]);
    let status = registry.status();
    assert!(matches!(
        status
            .server(&SERVER.parse().expect("server name"))
            .expect("valid status")
            .state(),
        ego_mcp::McpServerState::Admitted
    ));
    let rejected_status = status
        .server(&"beta".parse().expect("server name"))
        .expect("rejected status");
    assert!(matches!(
        rejected_status.state(),
        ego_mcp::McpServerState::Absent
    ));
    assert_eq!(rejected_status.generation(), None);
    assert!(rejected_status.authorization().is_none());
    assert_eq!(
        rejected_status
            .diagnostic()
            .expect("rejection diagnostic")
            .failure(),
        McpFailure::Credential
    );
    let errors = status.mcp_errors();
    assert_eq!(errors.len(), 1);
    // Catches: missing credentials still demand a guessed CLI strategy.
    assert_eq!(
        errors[0],
        "MCP server 'beta': server credential is unavailable; run '/mcp login beta'"
    );
    assert!(!errors[0].contains("access_token"));
    assert!(!errors[0].contains("client_secret"));
}

#[tokio::test(flavor = "current_thread")]
async fn cold_rejected_candidates_are_absent_with_sorted_sanitized_errors() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    let refresh = McpRegistryRefresh::new(
        vec![
            rejected("zeta", server_config("zeta"), McpFailure::Credential),
            rejected("alpha", server_config("alpha"), McpFailure::InvalidValue),
        ],
        RegistryTrigger::Reload,
        RegistryScope::All,
    )
    .expect("rejected refresh");
    let (registry, _) = controller(admission);

    let publication = registry
        .reconcile(refresh)
        .await
        .expect("rejections publish status");
    assert!(publication.admitted_servers().is_empty());
    let status = registry.status();
    let names: Vec<_> = status
        .servers()
        .iter()
        .map(|server| server.name().to_string())
        .collect();
    assert_eq!(names, ["alpha", "zeta"]);
    for name in ["alpha", "zeta"] {
        let name = name.parse().expect("server name");
        let server = status.server(&name).expect("cold rejection status");
        assert!(matches!(server.state(), ego_mcp::McpServerState::Absent));
        assert!(server.generation().is_none());
        assert!(server.authorization().is_none());
        assert!(server.diagnostic().is_some());
    }
    let errors = status.mcp_errors();
    assert_eq!(errors.len(), 2);
    assert!(errors[0].contains("alpha"));
    assert!(errors[1].contains("zeta"));
    assert!(!errors.iter().any(|error| {
        [
            "access_token",
            "client_secret",
            "issuer",
            "resource",
            "token",
        ]
        .iter()
        .any(|secret| error.contains(secret))
    }));
    assert_eq!(errors, registry.status().mcp_errors());
}

#[tokio::test(flavor = "current_thread")]
async fn later_materialization_rejection_retains_last_good_slice_as_degraded() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate(
        SERVER,
        vec![CountingTool::new(
            "last_good",
            Arc::new(AtomicUsize::new(0)),
        )],
    )));
    let (alpha, provenance) = alpha_admission();
    let (registry, _) = controller(admission);
    let first = registry
        .reconcile(
            McpRegistryRefresh::new(
                vec![McpRegistryCandidate::ready(
                    Arc::clone(&alpha),
                    provenance.clone(),
                )],
                RegistryTrigger::Add,
                RegistryScope::All,
            )
            .expect("initial refresh"),
        )
        .await
        .expect("initial publication");
    let second = registry
        .reconcile(
            McpRegistryRefresh::new(
                vec![rejected(
                    SERVER,
                    alpha.config().clone(),
                    McpFailure::Credential,
                )],
                RegistryTrigger::ConfigChanged,
                RegistryScope::All,
            )
            .expect("rejected refresh"),
        )
        .await
        .expect("retained publication");

    assert_ne!(second.generation(), first.generation());
    assert_eq!(second.admitted_servers(), [SERVER.to_owned()]);
    assert_eq!(second.admitted_tools(), ["last_good".to_owned()]);
    let status = registry.status();
    let server = status
        .server(&SERVER.parse().expect("server name"))
        .expect("retained status");
    assert!(matches!(server.state(), ego_mcp::McpServerState::Degraded));
    assert_eq!(server.generation(), Some(first.generation()));
    assert_eq!(
        server.diagnostic().expect("degraded diagnostic").failure(),
        McpFailure::Credential
    );
    assert!(
        status
            .mcp_errors()
            .iter()
            .all(|error| { !error.contains("access_token") && !error.contains("client_secret") })
    );
}

#[test]
fn refresh_sorts_complete_candidates_and_rejects_duplicate_names() {
    let alpha = rejected("alpha", server_config("alpha"), McpFailure::InvalidValue);
    assert_eq!(alpha.name().as_str(), "alpha");
    assert_eq!(alpha.config(), &server_config("alpha"));
    assert!(matches!(
        alpha.provenance(),
        McpConfigProvenance::Layer {
            source: Source::User
        }
    ));
    let unsorted = McpRegistryRefresh::new(
        vec![
            rejected("zeta", server_config("zeta"), McpFailure::Credential),
            alpha,
        ],
        RegistryTrigger::Reload,
        RegistryScope::All,
    )
    .expect("sorted refresh");
    let names: Vec<_> = unsorted
        .candidates()
        .iter()
        .map(|candidate| candidate.name().to_string())
        .collect();
    assert_eq!(names, ["alpha", "zeta"]);

    assert!(
        McpRegistryRefresh::new(
            vec![
                rejected("alpha", server_config("alpha"), McpFailure::Credential),
                rejected("alpha", server_config("alpha"), McpFailure::Credential),
            ],
            RegistryTrigger::Reload,
            RegistryScope::All,
        )
        .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn oauth_without_cimd_is_authority_absent_without_fabricated_authorization() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    let oauth_config = McpServerConfig::http(
        "https://mcp.example.test/mcp".parse().expect("HTTPS URL"),
        McpHttpAuth::Oauth,
        false,
    )
    .expect("OAuth HTTP config");
    let refresh = McpRegistryRefresh::new(
        vec![rejected("oauth", oauth_config, McpFailure::Authority)],
        RegistryTrigger::Login,
        RegistryScope::All,
    )
    .expect("OAuth refresh");
    let (registry, _) = controller(admission);

    registry
        .reconcile(refresh)
        .await
        .expect("OAuth rejection status");
    let status = registry.status();
    let server = status
        .server(&"oauth".parse().expect("server name"))
        .expect("OAuth status");
    assert!(matches!(server.state(), ego_mcp::McpServerState::Absent));
    assert_eq!(server.generation(), None);
    assert!(server.authorization().is_none());
    assert_eq!(
        server.diagnostic().expect("authority diagnostic").failure(),
        McpFailure::Authority
    );
    let errors = status.mcp_errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("oauth"));
    assert!(!errors.iter().any(|error| {
        [
            "issuer",
            "protected_resource",
            "resource",
            "access_token",
            "token",
        ]
        .iter()
        .any(|secret| error.contains(secret))
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn identical_authority_has_stable_capture_identity_across_publication_history() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let first_admission = fixture_admission(vec![
        Outcome::Candidate(candidate(
            SERVER,
            vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
        )),
        Outcome::Candidate(candidate(
            SERVER,
            vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
        )),
    ]);
    let (first_registry, _) = controller(first_admission);
    first_registry
        .reconcile(refresh(RegistryTrigger::Add))
        .await
        .expect("first publication");
    let first = first_registry
        .reconcile(refresh(RegistryTrigger::Reload))
        .await
        .expect("second publication");

    let (second_registry, second) = one_publication(
        config(),
        vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
    )
    .await;

    assert_ne!(first.generation(), second.generation());
    assert_ne!(
        first.capture().stable_fingerprint(),
        first.capture().generation(),
        "stable identity is not the process-local publication generation"
    );
    assert_eq!(
        first.capture().stable_fingerprint(),
        second.capture().stable_fingerprint(),
        "identical authority must reproduce its content-addressed capture identity"
    );
    assert_eq!(
        second_registry.publication().generation(),
        second.generation()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn equivalent_capture_resolution_survives_newer_pin_release_then_fails_closed_after_all_release()
 {
    let dropped = Arc::new(AtomicUsize::new(0));
    let admission = fixture_admission(vec![
        Outcome::Candidate(candidate(
            SERVER,
            vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
        )),
        Outcome::Candidate(candidate(
            SERVER,
            vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
        )),
    ]);
    let (registry, _) = controller(admission);
    let first = registry
        .reconcile(refresh(RegistryTrigger::Add))
        .await
        .expect("first publication");
    let old_capture = first.capture().clone();
    let old_generation = old_capture.generation().to_owned();
    let stable = old_capture.stable_fingerprint().to_owned();

    let newer = registry
        .reconcile(refresh(RegistryTrigger::Reload))
        .await
        .expect("equivalent successor publication");
    let newer_capture = newer.capture().clone();
    assert_eq!(newer_capture.stable_fingerprint(), stable);
    assert_ne!(newer_capture.generation(), old_generation);

    // Removing the newer publication drops its current strong pin.  The
    // older explicitly pinned equivalent remains the exact resolver result.
    drop(newer_capture);
    drop(newer);
    registry
        .reconcile(refresh_with(empty_config(), RegistryTrigger::Remove))
        .await
        .expect("remove current equivalent publication");
    let resolved = registry
        .resolve_capture(&stable)
        .expect("older equivalent remains resolvable while pinned");
    assert_eq!(resolved.generation(), old_generation);
    assert_eq!(resolved.stable_fingerprint(), stable);
    drop(resolved);
    drop(first);
    drop(old_capture);
    assert!(registry.resolve_capture(&stable).is_err());
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn every_authority_relevant_capture_drift_changes_stable_identity() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let (_, baseline) = one_publication(
        config(),
        vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
    )
    .await;
    let (_, config_drift) = one_publication(
        config_changed(),
        vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
    )
    .await;
    let (_, transport_drift) = one_publication(
        http_config(),
        vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
    )
    .await;
    let (_, operation_and_inventory_drift) = one_publication(
        config(),
        vec![CountingTool::new("renamed_tool", Arc::clone(&dropped))],
    )
    .await;
    let (_, schema_drift) = one_publication(
        config(),
        vec![CountingTool::with_parameters(
            "alpha_tool",
            Arc::clone(&dropped),
            json!({
                "type": "object",
                "properties": {"mode": {"type": "string"}},
                "required": ["mode"],
                "additionalProperties": false
            }),
        )],
    )
    .await;
    let stable = baseline.capture().stable_fingerprint();

    assert_ne!(stable, config_drift.capture().stable_fingerprint());
    assert_ne!(stable, transport_drift.capture().stable_fingerprint());
    assert_ne!(
        stable,
        operation_and_inventory_drift.capture().stable_fingerprint()
    );
    assert_ne!(stable, schema_drift.capture().stable_fingerprint());
}

#[tokio::test(flavor = "current_thread")]
async fn controller_resolves_the_exact_historical_capture_and_fails_closed_unknown_identity() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let admission = fixture_admission(vec![
        Outcome::Candidate(candidate(
            SERVER,
            vec![CountingTool::new("old_tool", Arc::clone(&dropped))],
        )),
        Outcome::Candidate(candidate(
            SERVER,
            vec![CountingTool::new("current_tool", Arc::clone(&dropped))],
        )),
    ]);
    let (registry, _) = controller(admission);
    let old = registry
        .reconcile(refresh(RegistryTrigger::Add))
        .await
        .expect("old publication");
    let old_fingerprint = old.capture().stable_fingerprint().to_owned();
    let current = registry
        .reconcile(refresh(RegistryTrigger::Reload))
        .await
        .expect("current publication");
    assert_ne!(old.generation(), current.generation());
    assert_ne!(
        old.capture().stable_fingerprint(),
        current.capture().stable_fingerprint()
    );

    let resolved = registry
        .resolve_capture(&old_fingerprint)
        .expect("historical capture remains addressable by stable identity");
    assert_eq!(resolved.generation(), old.capture().generation());
    assert_eq!(resolved.stable_fingerprint(), old_fingerprint);
    assert_eq!(resolved.tools()[0].descriptor().name, "old_tool");
    assert!(registry.resolve_capture("sha256:unknown-capture").is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn fresh_controller_reconstructs_matching_capture_but_not_drift_or_removal() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let (_, original) = one_publication(
        config(),
        vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
    )
    .await;
    let stable = original.capture().stable_fingerprint().to_owned();

    let (fresh, matching) = one_publication(
        config(),
        vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
    )
    .await;
    assert_eq!(matching.capture().stable_fingerprint(), stable);
    let resolved = fresh
        .resolve_capture(&stable)
        .expect("full re-admission reproduces the durable capture identity");
    assert_eq!(resolved.stable_fingerprint(), stable);

    let (drifted, _) = one_publication(
        config_changed(),
        vec![CountingTool::new("alpha_tool", Arc::clone(&dropped))],
    )
    .await;
    assert!(drifted.resolve_capture(&stable).is_err());

    let (removed, _) = one_publication(empty_config(), Vec::new()).await;
    assert!(removed.resolve_capture(&stable).is_err());
}

fn config_three() -> ego_core::config::ResolvedMcpConfig {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.zulu]\ntransport = 'stdio'\ncommand = 'mcp-zulu'\n\n         [mcp.servers.alpha]\ntransport = 'stdio'\ncommand = 'mcp-alpha'\n\n         [mcp.servers.mike]\ntransport = 'stdio'\ncommand = 'mcp-mike'\n",
    )
    .expect("valid MCP layer");
    resolve(&[layer], Trust::Trusted, None)
        .expect("resolved MCP config")
        .mcp
}

// Catches: a refresh or a new invocation reuses stale server instructions from
// the machine journal, or mutates an already admitted generation's instructions.
#[tokio::test(flavor = "current_thread")]
async fn refreshed_and_reopened_registries_read_new_instructions_without_mutating_old_captures() {
    let admission = fixture_admission(vec![
        Outcome::Candidate(candidate_saying(SERVER, Vec::new(), "first guidance")),
        Outcome::Candidate(candidate_saying(SERVER, Vec::new(), "refreshed guidance")),
        Outcome::Candidate(candidate_saying(SERVER, Vec::new(), "reopened guidance")),
    ]);
    let (registry, journal) = controller(Arc::clone(&admission));
    let first = registry
        .reconcile(refresh(RegistryTrigger::ConfigChanged))
        .await
        .expect("first admission");
    let second = registry
        .reconcile(refresh(RegistryTrigger::Reload))
        .await
        .expect("refreshed admission");
    assert_eq!(first.capture().instructions()[0].text(), "first guidance");
    assert_eq!(
        second.capture().instructions()[0].text(),
        "refreshed guidance"
    );
    assert_eq!(admission.requests().len(), 2);

    // The CLI constructs another controller over the same machine journal for
    // another invocation, even when it opens the same durable conversation.
    let reopened = McpRegistryController::new(
        journal as Arc<dyn McpRegistryJournal>,
        Arc::clone(&admission) as Arc<dyn McpCandidateAdmission>,
    )
    .expect("reopened registry");
    let third = reopened
        .reconcile(refresh(RegistryTrigger::ConfigChanged))
        .await
        .expect("new invocation admission");
    assert_eq!(
        third.capture().instructions()[0].text(),
        "reopened guidance"
    );
    assert_eq!(admission.requests().len(), 3);
    assert_eq!(first.capture().instructions()[0].text(), "first guidance");
    assert_eq!(
        second.capture().instructions()[0].text(),
        "refreshed guidance"
    );
}

/// A server's `instructions` reach the capture, trimmed and bounded, in
/// ascending server-name order — and a server that said nothing adds no entry.
///
/// The capture keeps its servers in a `BTreeMap`, so today the order happens
/// incidentally. A documented composition order that nothing checks is one
/// that stops being true the first time the container changes, which is why
/// this asserts the whole ordered list rather than membership.
#[tokio::test(flavor = "current_thread")]
async fn server_instructions_reach_the_capture_in_server_name_order() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Candidate(candidate_saying(
        "alpha",
        vec![CountingTool::new(
            "alpha_tool",
            Arc::new(AtomicUsize::new(0)),
        )],
        "  Prefer alpha_tool for live lookups.  ",
    )));
    admission.push(Outcome::Candidate(candidate(
        "mike",
        vec![CountingTool::new(
            "mike_tool",
            Arc::new(AtomicUsize::new(0)),
        )],
    )));
    admission.push(Outcome::Candidate(candidate_saying(
        "zulu",
        vec![CountingTool::new(
            "zulu_tool",
            Arc::new(AtomicUsize::new(0)),
        )],
        "Prefer zulu_tool for archival lookups.",
    )));
    let (registry, _) = controller(Arc::clone(&admission));
    let publication = registry
        .reconcile(
            McpRegistryRefresh::new(
                admissions(config_three()),
                RegistryTrigger::Add,
                RegistryScope::All,
            )
            .expect("registry refresh"),
        )
        .await
        .expect("publication");
    assert_eq!(
        publication.admitted_servers(),
        ["alpha".to_owned(), "mike".to_owned(), "zulu".to_owned()]
    );

    let instructions = publication.capture().instructions();
    let seen: Vec<_> = instructions
        .iter()
        .map(|entry| (entry.server(), entry.text()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("alpha", "Prefer alpha_tool for live lookups."),
            ("zulu", "Prefer zulu_tool for archival lookups."),
        ],
        "instructions must be name-ordered, trimmed, and absent for a silent server"
    );
    assert!(
        instructions.iter().all(|entry| !entry.truncated()),
        "nothing this short was cut"
    );
}

// Catches: arbitrary backend text crossing the public registry boundary even when
// a heuristic masker does not recognize its credential syntax (critic rounds 7-9).
#[tokio::test(flavor = "current_thread")]
async fn allowlisted_diagnostics_never_publish_round_seven_to_nine_backend_text() {
    let mut causes: Vec<String> = [
        "password=Pass(word)LEAKA end",
        "pwd=(abc)LEAKB end",
        r#"config token: Wrapper { inner: "LEAKC" } rejected"#,
        r#"password=Mutex { data: "LEAKD", poisoned: false, .. } end"#,
        r#"{ "msg": "bad token=abc", "name": "x-api-key", "note": "}", "value": "LEAKE" }"#,
        r#"Header { name: "x-api-key", meta: Meta(token=abc), value: "LEAKF" }"#,
        r#"Header { name: Some("x-api-key"), value: Some("LEAKG") }"#,
        r#"Header { name: Qualified("tenant", "x-api-key"), value: "LEAKWRAP" }"#,
        r#"{"name":["scope","token"],"value":"LEAKWRAP"}"#,
        r#"{"value":"LEAKBEFORE","name":"x-api-key"}"#,
        "Authorization\nBearer LEAKBARE",
        "ｔｏｋｅｎ=LEAKCONFUSABLE",
        "tоken=LEAKCONFUSABLE",
        r#"headers: [["x-api-key","LEAKL1"]] rejected"#,
        "headers [['Authorization', 'Basic LEAKL2']]",
        r#"["token", "LEAKL3"]"#,
        r#"{"header_name":"x-api-key","header_value":"LEAKN1"}"#,
        r#"{"headerName":"authorization","headerValue":"LEAKN2"}"#,
        "header.name=x-api-key header.value=LEAKN3",
        "spawn failed: mcp-server --api-key LEAKF1 --port 7",
        "exec: tool --password LEAKF2",
        "exec: tool --token LEAKF3",
        "connect postgres://user:LEAKU1/x@db/app failed",
        "connect postgres://user:LEAKU2?x@db/app failed",
        "connect postgres://user:LEAKU3#x@db/app failed",
        "connect postgres://user:a@LEAKU4/b@db/app failed",
        "auth Bearer: LEAKB1",
        "auth bearer=LEAKB2",
        "cfg[[token]]=LEAKP1",
        "cfg (token)=LEAKP2",
        "cfg <token>: LEAKP3",
        "plain backend text without any credential marker",
        "\n\u{1b}[31m raw backend text \u{0}",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for joiner in ['\u{200d}', '\u{200c}', '\u{fe0f}'] {
        causes.extend([
            format!("tok\u{0435}{joiner}n=LEAKJ1 end"),
            format!("pas\u{0455}{joiner}word: LEAKJ2"),
            format!("{{\"name\":\"x-api-k\u{0435}{joiner}y\",\"value\":\"LEAKJ3\"}}"),
        ]);
    }
    for mark in ['\u{20dd}', '\u{0338}', '\u{0489}'] {
        causes.extend([
            format!("to{mark}ken=LEAKM1 end"),
            format!("pass{mark}word: LEAKM2 end"),
        ]);
    }
    for name in [
        "\u{0422}oken",
        "t\u{043e}ken",
        "tok\u{0435}n",
        "p\u{0430}ssword",
        "\u{0455}ecret",
        "passw\u{043e}r\u{0501}",
        "ke\u{0443}",
        "\u{041a}ey",
        "pass\u{051d}d",
        "author\u{0456}zat\u{0456}on",
        "author\u{0406}zat\u{0406}on",
        "author\u{0130}zat\u{0130}on",
        "author\u{0131}zat\u{0131}on",
    ] {
        causes.push(format!("{name}=LEAKCONFUSABLE end"));
    }
    let mut cases: Vec<(String, String)> = causes
        .into_iter()
        .enumerate()
        .map(|(index, cause)| (format!("base-{index:02}"), cause))
        .collect();
    cases.extend(legacy_masker_causes());
    cases.push((
        "retained:status_servers_are_canonicalized_by_name_across_mixed_outcomes".to_owned(),
        "zeta-backend-secret=zeta-value".to_owned(),
    ));
    let mut leaks = Vec::new();
    for (index, (case, cause)) in cases.into_iter().enumerate() {
        let admission = Arc::new(FakeAdmission {
            state: Arc::new(Mutex::new(AdmissionState::default())),
            gate: Arc::new(Mutex::new(None)),
        });
        admission.push(Outcome::Invalid(cause.clone()));
        let (registry, journal) = controller(admission);
        registry
            .reconcile(refresh(RegistryTrigger::Add))
            .await
            .expect("publication");
        assert_eq!(
            journal.state.lock().expect("journal").private_diagnostics,
            [("alpha".to_owned(), cause)]
        );
        let status = registry.status();
        let server = status.servers().first().expect("status");
        let diagnostic = server.diagnostic().expect("diagnostic");
        if status.mcp_errors()
            != ["MCP server 'alpha': server configuration is invalid; inspect with 'ego mcp ls'"]
            || diagnostic.message() != "server configuration is invalid; inspect with 'ego mcp ls'"
            || diagnostic.failure() != McpFailure::InvalidValue
        {
            // Report the corpus index, never echo the raw cause into test diagnostics.
            leaks.push((index, case));
        }
        for event in journal.events() {
            if let McpRegistryEvent::RefreshFailed { cause, .. } = event {
                assert_eq!(
                    cause,
                    "server configuration is invalid; inspect with 'ego mcp ls'"
                );
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "raw backend detail published for corpus cases {leaks:?}"
    );
}

// Catches: an optional raw-log failure skipping RefreshFailed machine evidence
// or preventing the fixed failed-server status from being published.
#[tokio::test(flavor = "current_thread")]
async fn private_log_failure_still_journals_refresh_failed_and_keeps_fixed_status() {
    let admission = Arc::new(FakeAdmission {
        state: Arc::new(Mutex::new(AdmissionState::default())),
        gate: Arc::new(Mutex::new(None)),
    });
    admission.push(Outcome::Invalid("--api-key PRIVATE-RAW".to_owned()));
    let (registry, journal) = controller(admission);
    journal.state.lock().expect("journal").fail_private = true;
    registry
        .reconcile(refresh(RegistryTrigger::Add))
        .await
        .expect("best-effort private log");
    assert_eq!(
        registry.status().mcp_errors(),
        ["MCP server 'alpha': server configuration is invalid; inspect with 'ego mcp ls'"]
    );
    assert!(
        journal
            .state
            .lock()
            .expect("journal")
            .private_diagnostics
            .is_empty()
    );
    let events = journal.events();
    let failed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            McpRegistryEvent::RefreshFailed { cause, .. } => Some(cause.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        failed,
        ["server configuration is invalid; inspect with 'ego mcp ls'"]
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, McpRegistryEvent::SnapshotPublished { .. }))
    );
    assert!(!format!("{events:?}").contains("PRIVATE-RAW"));
}

fn legacy_masker_causes() -> Vec<(String, String)> {
    let mut causes = Vec::new();
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::multibyte_cause_is_cut_on_a_character_boundary_at_the_bound".to_owned(), value.to_owned()));
        };
        let cause = "😀".repeat(300);
        capture(&cause);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::cause_at_the_bound_is_kept_whole_and_one_past_it_loses_one_character".to_owned(), value.to_owned()));
        };
        let exact = "x".repeat(256);
        capture(&exact);
        let over = "x".repeat(256 + 1);
        capture(&over);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::ascii_control_characters_never_reach_the_diagnostic".to_owned(), value.to_owned()));
        };
        capture("bad\nforged: line\r\u{1b}[31mred\u{0}\u{7f}\u{85}end");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::unicode_line_breaks_and_bidi_overrides_never_reach_the_diagnostic".to_owned(), value.to_owned()));
        };
        capture("a\u{2028}b\u{2029}c\u{202e}d\u{2066}e\u{200b}f");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::empty_or_blank_cause_adds_no_dangling_separator".to_owned(), value.to_owned()));
        };
        for cause in ["", "   ", "\n\n\n", "\t\r\n"] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::leading_control_flood_does_not_consume_the_bound".to_owned(), value.to_owned()));
        };
        let cause = format!("{}resources/list missing envelope", "\n".repeat(300));
        {
            capture(&cause);
            capture(&cause);
        };
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::candidate_detail_critic::secret_values_are_masked_in_every_form"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        for (cause, _secret) in [
            ("backend-secret=do-not-publish", "do-not-publish"),
            ("bad api_key=abc123 retry", "abc123"),
            ("TOKEN=Zz9 failed", "Zz9"),
            ("GET /x?client_secret=s3&mode=1", "s3"),
            (
                "auth failed: Authorization: Bearer eyJhbGci.payload.sig",
                "eyJhbGci",
            ),
            ("authorization: BEARER abc", "abc"),
            ("key=\"two words\" end", "two words"),
        ] {
            capture(cause);
        }
        capture("backend-secret=do-not-publish");
        capture("x?a=1&api_key=k&b=2");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::candidate_detail_critic::non_secret_detail_is_kept_verbatim"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        capture("bearer token required");
        for cause in [
            "duplicate tool identity",
            "mode=fast retries=3",
            "empty key=",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::secret_straddling_the_cut_never_leaks_a_prefix".to_owned(), value.to_owned()));
        };
        let cause = format!("{} api_key=SUPERSECRETVALUE", "a".repeat(240));
        capture(&cause);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::unterminated_private_key_header_masks_to_the_end".to_owned(), value.to_owned()));
        };
        capture("bad -----BEGIN RSA PRIVATE KEY MIIEvLEAK");
        capture("see -----BEGIN CERTIFICATE----- ok");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::candidate_detail_critic::joiners_after_non_ascii_text_are_kept_but_never_split_a_secret_name".to_owned(), value.to_owned()));
        };
        capture("failed 👨\u{200d}👩 ok");
        capture("می\u{200c}خواهد");
        capture("❤\u{fe0f} ok");
        capture("to\u{200d}ken=LEAK");
        capture("é\u{200d}token=LEAK");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::json_quoted_token_value_is_hidden".to_owned(),
                value.to_owned(),
            ));
        };
        capture(r#"bad body {"token":"abc123XYZ"}"#);
        capture(r#"bad body {"api_key": "abc123XYZ"}"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::colon_header_style_secret_is_hidden"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        capture("sent x-api-key: abc123XYZ failed");
        capture("token: abc123XYZ rejected");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::password_values_are_hidden".to_owned(),
                value.to_owned(),
            ));
        };
        capture("login password=hunter2 failed");
        capture("login passwd=hunter2 failed");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::url_userinfo_password_is_hidden".to_owned(),
                value.to_owned(),
            ));
        };
        capture("cannot reach https://svc:hunter2@host.example/mcp");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::blanks_around_equals_do_not_defeat_the_mask"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        capture("bad api_key = abc123XYZ end");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::non_bearer_authorization_schemes_are_hidden"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        capture("Authorization: Basic dXNlcjpwYXNz");
        capture("Authorization: Token abc123XYZ");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic::authorization_without_blank_before_scheme_is_hidden".to_owned(), value.to_owned()));
        };
        capture("Authorization:Bearer abc123XYZ");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::json_quoted_authorization_bearer_is_hidden"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        capture(r#"headers {"Authorization": "Bearer abc123XYZ"}"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic::escaped_quote_inside_quoted_value_does_not_end_the_mask".to_owned(), value.to_owned()));
        };
        capture(r#"token="ab\"TAILSECRET" end"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic::scheme_word_after_equals_does_not_shield_the_credential".to_owned(), value.to_owned()));
        };
        capture("token=Bearer abc123XYZ");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic::zero_width_char_inside_the_name_does_not_defeat_the_mask".to_owned(), value.to_owned()));
        };
        capture("to\u{200b}ken=abc123XYZ");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::pem_private_key_body_is_hidden".to_owned(),
                value.to_owned(),
            ));
        };
        capture("bad -----BEGIN PRIVATE KEY----- MIIEvQIBADANBgkq -----END PRIVATE KEY-----");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic::semicolon_inside_secret_value_does_not_leak_the_tail".to_owned(), value.to_owned()));
        };
        capture("token=ab;cdTAIL");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic::bearer_secret_straddling_the_cut_never_leaks_a_prefix".to_owned(), value.to_owned()));
        };
        let cause = format!(
            "{} Authorization: Bearer SUPERSECRETVALUE",
            "a ".repeat(120)
        );
        capture(&cause);
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic::repeated_secrets_are_all_masked".to_owned(),
                value.to_owned(),
            ));
        };
        capture("a api_key=ONE b token=TWO c Authorization: Bearer THREE");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_two::delimiter_characters_inside_an_unquoted_secret_do_not_leak_the_tail".to_owned(), value.to_owned()));
        };
        for cause in [
            "token=ab,cdTAIL",
            "token=ab&cdTAIL",
            "token=ab)cdTAIL",
            "token=ab}cdTAIL",
            "token=ab]cdTAIL",
            "Authorization: Bearer ab,cdTAIL",
            "password=p@ss)cdTAIL",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic_round_two::pgp_private_key_block_is_hidden"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        capture(
            "bad -----BEGIN PGP PRIVATE KEY BLOCK----- lQOYBGBODYSECRET -----END PGP PRIVATE KEY BLOCK-----",
        );
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_two::url_userinfo_without_password_is_hidden".to_owned(), value.to_owned()));
        };
        capture("clone https://ghp_ABCDEF123456@github.com/x failed");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_two::bidi_mark_or_nul_inside_the_name_does_not_defeat_the_mask".to_owned(), value.to_owned()));
        };
        capture("to\u{200e}ken=abc123XYZ");
        capture("tok\u{0}en=abc123XYZ");
        capture("to\u{2063}ken=abc123XYZ");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_two::sigv4_authorization_header_is_hidden".to_owned(), value.to_owned()));
        };
        capture(
            "Authorization: AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/20260101/x, Signature=deadbeef0123",
        );
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_two::colon_after_a_key_word_in_prose_is_masked_by_design".to_owned(), value.to_owned()));
        };
        capture("duplicate key: command");
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic_round_two::hostile_long_input_stays_bounded"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        for unit in [
            "-----BEGIN ",
            "a://",
            "key:",
            "token=\"\\",
            "k=",
            "authorization: a ",
        ] {
            capture(&unit.repeat(40_000));
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_three::json_encoded_inside_a_string_still_hides_the_value".to_owned(), value.to_owned()));
        };
        capture(r#"body {\"token\":\"LEAKESC\"} rejected"#);
        capture(r#"body {\"api_key\": \"LEAKESC\"}"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_three::digest_params_with_spaces_inside_quotes_do_not_leak_later_params".to_owned(), value.to_owned()));
        };
        capture(r#"Authorization: Digest username="u", realm="my realm", response="LEAKRESP""#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_three::invisible_marks_missing_from_the_drop_list_do_not_split_the_name".to_owned(), value.to_owned()));
        };
        for mark in ['\u{034f}', '\u{fe0f}', '\u{e0041}', '\u{180e}', '\u{3164}'] {
            let cause = format!("to{mark}ken=LEAKINV done");
            capture(&cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_three::bracketed_and_tuple_forms_are_hidden".to_owned(), value.to_owned()));
        };
        capture("params[token]=LEAKBR&a=1");
        capture(r#"header ("x-api-key", "LEAKTUP") rejected"#);
        capture(r#"Header("authorization", "Bearer LEAKTUP")"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_three::quoted_bearer_credential_with_a_space_does_not_leak_its_tail".to_owned(), value.to_owned()));
        };
        capture(r#"Authorization: Bearer "abc LEAKSP" rejected"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_three::unterminated_private_key_header_does_not_publish_the_body".to_owned(), value.to_owned()));
        };
        capture("-----BEGIN PRIVATE KEY MIIEvQIBADANBgkqLEAKBODY");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_three::urls_without_userinfo_are_kept_verbatim".to_owned(), value.to_owned()));
        };
        for cause in [
            "GET http://localhost:8080/mcp failed",
            "see https://example.com/a@b now",
            "see https://example.com?x=a@b now",
            "connect tcp://127.0.0.1:9 refused",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::json_error_bodies_whose_message_ends_in_a_secret_word_are_kept".to_owned(), value.to_owned()));
        };
        for cause in [
            r#"{"error":"invalid token","code":401}"#,
            r#"{"message":"missing api key","code":400}"#,
            r#"{"field":"api_key","message":"required"}"#,
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::string_lists_that_name_a_secret_word_are_kept".to_owned(), value.to_owned()));
        };
        for cause in [
            r#"missing properties ["key", "name"]"#,
            r#"allowed headers ["authorization", "content-type"]"#,
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::name_value_header_records_hide_the_value".to_owned(), value.to_owned()));
        };
        capture(r#"{"name":"x-api-key","value":"LEAKREC"}"#);
        capture(r#"Header { name: "x-api-key", value: "LEAKREC" }"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::json_escaped_twice_hides_the_value".to_owned(), value.to_owned()));
        };
        capture(r#"outer {\\\"token\\\":\\\"LEAKDEEP\\\"} rejected"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::escaped_quote_inside_an_escaped_value_does_not_end_it".to_owned(), value.to_owned()));
        };
        capture(r#"body {\"password\":\"pa\\\"LEAKTAIL\"} rejected"#);
        capture(r#"{\"authorization\":\"Digest username=\\\"u\\\", response=\\\"LEAKDIG\\\"\"}"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::pretty_printed_object_under_a_secret_name_is_hidden".to_owned(), value.to_owned()));
        };
        capture(r#"{ "password": { "value": "LEAKNEST" } }"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::invisible_table_range_boundaries_are_all_dropped".to_owned(), value.to_owned()));
        };
        for mark in [
            '\u{00ad}',
            '\u{0605}',
            '\u{06dd}',
            '\u{070f}',
            '\u{0891}',
            '\u{08e2}',
            '\u{1160}',
            '\u{17b4}',
            '\u{180b}',
            '\u{180f}',
            '\u{200b}',
            '\u{200f}',
            '\u{202a}',
            '\u{202e}',
            '\u{2060}',
            '\u{2064}',
            '\u{2066}',
            '\u{206f}',
            '\u{fe00}',
            '\u{feff}',
            '\u{ffa0}',
            '\u{fff9}',
            '\u{fffb}',
            '\u{110bd}',
            '\u{110cd}',
            '\u{13430}',
            '\u{1343f}',
            '\u{1bca0}',
            '\u{1bca3}',
            '\u{1d173}',
            '\u{1d17a}',
            '\u{e0001}',
            '\u{e0100}',
            '\u{e01ef}',
        ] {
            let cause = format!("to{mark}ken=LEAKMARK done");
            capture(&cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::private_key_headers_cut_at_every_point_hide_the_body".to_owned(), value.to_owned()));
        };
        for cause in [
            "-----BEGIN PRIVATE KEY",
            "-----BEGIN PRIVATE KEY--",
            "-----BEGIN PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY----- LEAKPEM",
            "x -----BEGIN OPENSSH PRIVATE KEY----- LEAKPEM -----END",
        ] {
            capture(cause);
            {
                capture(cause);
                capture(cause);
            };
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_four::separator_followed_only_by_blanks_masks_nothing_and_does_not_panic".to_owned(), value.to_owned()));
        };
        capture("key= ");
        capture("token: ");
        capture("key= ");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_five::long_inputs_are_masked_in_linear_time".to_owned(), value.to_owned()));
        };
        let cases = [
            ("unclosed objects", "name:{".repeat(10_000)),
            ("unclosed arrays", "name:[".repeat(10_000)),
            (
                "nested objects",
                format!("{}1{}", r#"{"name":"#.repeat(12_000), "}".repeat(12_000)),
            ),
            ("blank key pairs", "key= ".repeat(50_000)),
            (
                "authorization chain",
                "authorization: Bearer ".repeat(20_000),
            ),
            ("tuple heads", r#"("token", "#.repeat(20_000)),
        ];
        for (_label, cause) in cases {
            capture(&cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_five::every_prefix_and_suffix_of_a_dense_sample_is_masked_without_panicking".to_owned(), value.to_owned()));
        };
        let sample = format!(
            "\u{e9}\u{200d}{}",
            r#"{\"name\":\"x-api-key\",\"value\":\\\"a\\\\\"b\",\"token\":{\"a\":[\"b\"]},("key", 'v')} Authorization: Digest a="b c", d=\"e\" name: token value: [x {y 'z \\\\"#
        );
        let cuts = sample
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(sample.len()));
        for cut in cuts {
            capture(&sample[..cut]);
            capture(&sample[cut..]);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_five::name_value_record_with_an_auth_scheme_word_as_name_hides_the_value".to_owned(), value.to_owned()));
        };
        for cause in [
            "param name=token value=LEAKREC",
            "param name: token value: LEAKREC",
            "param name=bearer value=LEAKREC",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_five::key_value_records_hide_the_value_member".to_owned(), value.to_owned()));
        };
        for cause in [
            r#"{"key":"API_TOKEN","value":"LEAKKV"}"#,
            r#"{"Key":"db_password","Value":"LEAKKV"}"#,
            r#"[{"Key":"Environment","Value":"prod"},{"Key":"api_key","Value":"LEAKKV"}]"#,
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_five::pretty_printed_debug_tuples_hide_the_value".to_owned(), value.to_owned()));
        };
        for cause in [
            "(\n    \"x-api-key\",\n    \"LEAKPT\",\n)",
            "Header(\n    \"authorization\",\n    \"Bearer LEAKPT\",\n)",
            "[\n    (\n        \"x-api-key\",\n        \"LEAKPT\",\n    ),\n]",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_five::python_bytes_tuples_hide_the_value".to_owned(), value.to_owned()));
        };
        for cause in [
            r#"[(b'x-api-key', b'LEAKBT')]"#,
            r#"[(b'authorization', b'Bearer LEAKBT')]"#,
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:registry::secret_mask_critic_round_five::debug_tuples_hide_the_value"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        for cause in [
            r#"Some(("x-api-key", "LEAKT"))"#,
            r#"[("accept", "*/*"), ("x-api-key", "LEAKT")]"#,
            r#"(\"x-api-key\", \"LEAKT\")"#,
            r#"[('Authorization', 'Bearer LEAKT')]"#,
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_five::ordinary_pairs_and_records_without_a_secret_name_are_kept".to_owned(), value.to_owned()));
        };
        for cause in [
            "duplicate tool identity: search",
            "mode=fast retries=3",
            r#"{"name":"mode","value":"fast"}"#,
            r#"{"name":"theme","value":"dark","extra":{"value":1}}"#,
            "name: web value: 42",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_six::record_marker_survives_brackets_inside_unmasked_strings".to_owned(), value.to_owned()));
        };
        for (cause, _secret) in [
            (
                r#"{"name":"x-api-key","hint":"(","value":"LEAKH1"}"#,
                "LEAKH1",
            ),
            (
                r#"{"name":"authorization","note":"see )","value":"Bearer LEAKH2"}"#,
                "LEAKH2",
            ),
            (
                r#"Header { name: "x-api-key", hint: "}", value: "LEAKH3" }"#,
                "LEAKH3",
            ),
            (
                r#"{"name":"x-api-key","hint":":)","value":"LEAKH4"}"#,
                "LEAKH4",
            ),
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_six::prefixed_secret_name_arms_the_value_member".to_owned(), value.to_owned()));
        };
        for (cause, _secret) in [
            (r#"{'name': b'x-api-key', 'value': b'LEAKP1'}"#, "LEAKP1"),
            (r#"{'name': u'x-api-key', 'value': u'LEAKP2'}"#, "LEAKP2"),
            (r#"Header(name=b'x-api-key', value=b'LEAKP3')"#, "LEAKP3"),
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_six::two_letter_prefixes_and_a_blank_before_the_prefix_still_open_a_tuple".to_owned(), value.to_owned()));
        };
        for (cause, _secret) in [
            (r#"[(rb'x-api-key', rb'LEAKT1')]"#, "LEAKT1"),
            (r#"[(Rb"authorization", Rb"Bearer LEAKT2")]"#, "LEAKT2"),
            (r#"[( b'x-api-key', b'LEAKT3' )]"#, "LEAKT3"),
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_six::every_new_rule_is_linear_on_100k_adversarial_chars".to_owned(), value.to_owned()));
        };
        let n = 100_000;
        let cases = [
            (
                "name then backslash run",
                format!("name:{}", "\\".repeat(n)),
            ),
            (
                "key separators each followed by a backslash",
                "key:\\".repeat(n / 5),
            ),
            (
                "plain quote around backslash pairs",
                format!("password=\"{}\"", "\\\\".repeat(n / 2)),
            ),
            (
                "escaped quote runs never closed",
                format!("password=\\\"{}", "\\\\\\\"".repeat(n / 4)),
            ),
            (
                "records armed and consumed",
                r#"{"name":"token","value":1}"#.repeat(n / 26),
            ),
            (
                "records armed and never closed",
                r#"{"name":"token","#.repeat(n / 16),
            ),
            ("unquoted records", "name=token value=".repeat(n / 17)),
            ("spaced prefixed tuple heads", "( b'a', ".repeat(n / 8)),
            ("prefixed secret tuple heads", "(b'token', ".repeat(n / 11)),
            (
                "long prefixed name before a comma",
                format!("(b'{}', x", "a".repeat(n)),
            ),
            (
                "bracket value with escaped quote runs",
                format!("token=[{}", "\\\"".repeat(n / 2)),
            ),
            (
                "bracket value with both quote kinds",
                format!("token=[{}", "\"'".repeat(n / 2)),
            ),
            (
                "bracket value with backslash runs in a string",
                format!("token=[\"{}\"]", "\\\\".repeat(n / 2)),
            ),
            ("string prefixes", "password=bb'".repeat(n / 12)),
            ("nested secret brackets", "token=[".repeat(n / 7)),
        ];
        for (_label, cause) in cases {
            capture(&cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_seven::text_glued_to_the_closing_paren_of_a_secret_is_hidden".to_owned(), value.to_owned()));
        };
        capture("password=Pass(word)LEAKA end");
        capture("pwd=(abc)LEAKB end");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_seven::struct_literal_wrapped_secret_values_are_hidden".to_owned(), value.to_owned()));
        };
        capture(r#"config token: Wrapper { inner: "LEAKC" } rejected"#);
        capture(r#"password=Mutex { data: "LEAKD", poisoned: false, .. } end"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_seven::record_marker_survives_a_masked_closing_quote_before_it".to_owned(), value.to_owned()));
        };
        capture(
            r#"{ "msg": "bad token=abc", "name": "x-api-key", "note": "}", "value": "LEAKE" }"#,
        );
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_seven::record_marker_survives_a_masked_closing_bracket_between_name_and_value".to_owned(), value.to_owned()));
        };
        capture(r#"Header { name: "x-api-key", meta: Meta(token=abc), value: "LEAKF" }"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_seven::call_wrapped_record_name_arms_the_value_member".to_owned(), value.to_owned()));
        };
        capture(r#"Header { name: Some("x-api-key"), value: Some("LEAKG") }"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_seven::round_seven_adversarial_input_stays_linear".to_owned(), value.to_owned()));
        };
        let n = 100_000;
        let cases = [
            ("empty calls", "token=Some() ".repeat(n / 13)),
            ("open calls", "token=Some(".repeat(n / 11)),
            ("call words", "token=aaaaaaaa ".repeat(n / 15)),
            ("escaped runs", format!("token=[\"{}\"]", "\\".repeat(n))),
            ("even runs", "token=[\"a\\\\\" ".repeat(n / 12)),
            ("armed records", r#"name: b"x-api-key" "#.repeat(n / 19)),
            ("struct bodies", "token: W { ".repeat(n / 11)),
        ];
        for (_label, cause) in cases {
            capture(&cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::a_joiner_after_a_foreign_letter_never_splits_a_secret_name".to_owned(), value.to_owned()));
        };
        for joiner in ['\u{200d}', '\u{200c}', '\u{fe0f}'] {
            capture(&format!("tok\u{0435}{joiner}n=LEAKJ1 end"));
            capture(&format!("pas\u{0455}{joiner}word: LEAKJ2"));
            capture(&format!(
                "{{\"name\":\"x-api-k\u{0435}{joiner}y\",\"value\":\"LEAKJ3\"}}"
            ));
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::non_composing_combining_marks_never_split_a_secret_name".to_owned(), value.to_owned()));
        };
        for mark in ['\u{20dd}', '\u{0338}', '\u{0489}'] {
            capture(&format!("to{mark}ken=LEAKM1 end"));
            capture(&format!("pass{mark}word: LEAKM2 end"));
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::confusable_letters_of_every_suffix_are_folded".to_owned(), value.to_owned()));
        };
        for (name, secret) in [
            ("\u{0422}oken", "LEAKC1"),
            ("t\u{043e}ken", "LEAKC2"),
            ("tok\u{0435}n", "LEAKC3"),
            ("p\u{0430}ssword", "LEAKC4"),
            ("\u{0455}ecret", "LEAKC5"),
            ("passw\u{043e}r\u{0501}", "LEAKC6"),
            ("ke\u{0443}", "LEAKC7"),
            ("\u{041a}ey", "LEAKC8"),
            ("pass\u{051d}d", "LEAKC9"),
            ("author\u{0456}zat\u{0456}on", "LEAKC10"),
            ("author\u{0406}zat\u{0406}on", "LEAKC11"),
            ("author\u{0130}zat\u{0130}on", "LEAKC12"),
            ("author\u{0131}zat\u{0131}on", "LEAKC13"),
        ] {
            capture(&format!("{name}={secret} end"));
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::a_pair_in_a_plain_list_hides_its_value".to_owned(), value.to_owned()));
        };
        capture(r#"headers: [["x-api-key","LEAKL1"]] rejected"#);
        capture("headers [['Authorization', 'Basic LEAKL2']]");
        capture(r#"["token", "LEAKL3"]"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::a_record_whose_name_field_is_prefixed_hides_the_line".to_owned(), value.to_owned()));
        };
        capture(r#"{"header_name":"x-api-key","header_value":"LEAKN1"}"#);
        capture(r#"{"headerName":"authorization","headerValue":"LEAKN2"}"#);
        capture("header.name=x-api-key header.value=LEAKN3");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::a_command_line_flag_hides_its_blank_separated_value".to_owned(), value.to_owned()));
        };
        capture("spawn failed: mcp-server --api-key LEAKF1 --port 7");
        capture("exec: tool --password LEAKF2");
        capture("exec: tool --token LEAKF3");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::url_userinfo_with_a_slash_query_or_hash_in_the_password_is_masked".to_owned(), value.to_owned()));
        };
        capture("connect postgres://user:LEAKU1/x@db/app failed");
        capture("connect postgres://user:LEAKU2?x@db/app failed");
        capture("connect postgres://user:LEAKU3#x@db/app failed");
        capture("connect postgres://user:a@LEAKU4/b@db/app failed");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::a_scheme_word_followed_by_a_separator_hides_its_value".to_owned(), value.to_owned()));
        };
        capture("auth Bearer: LEAKB1");
        capture("auth bearer=LEAKB2");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:registry::secret_mask_critic_round_nine::a_bracketed_or_parenthesised_name_hides_its_value".to_owned(), value.to_owned()));
        };
        capture("cfg[[token]]=LEAKP1");
        capture("cfg (token)=LEAKP2");
        capture("cfg <token>: LEAKP3");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:secret_mask_critic_round_eight::multiline_secret_values_do_not_resume_publication".to_owned(), value.to_owned()));
        };
        for cause in [
            "token:\nLEAKMULTI",
            "password=<<EOF\nLEAKMULTI\nEOF\nmode=fast",
            "client_secret=first\\\nLEAKMULTI",
            "{\"token\": {\n\"value\":\"LEAKMULTI\"\n}}",
            "token=-----BEGIN PRIVATE KEY-----\nLEAKMULTI\n-----END PRIVATE KEY-----",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:secret_mask_critic_round_eight::prefixed_sensitive_names_mask_the_entire_suffix".to_owned(), value.to_owned()));
        };
        for cause in [
            "X-ApI-KeY=first LEAKPREFIX",
            "AWS_SECRET_ACCESS_KEY: first LEAKPREFIX",
            "api key = first LEAKPREFIX",
            "client.secret=first LEAKPREFIX",
            "DATABASE_URL=postgresql://alice:LEAKPREFIX@db/app",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:secret_mask_critic_round_eight::comma_bearing_sensitive_name_wrappers_do_not_disarm_the_mask".to_owned(), value.to_owned()));
        };
        for cause in [
            r#"Header { name: Qualified("tenant", "x-api-key"), value: "LEAKWRAP" }"#,
            r#"{"name":["scope","token"],"value":"LEAKWRAP"}"#,
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:secret_mask_critic_round_eight::quoted_bracket_sensitive_keys_do_not_publish_the_value".to_owned(), value.to_owned()));
        };
        for cause in [
            r#"headers["x-api-key"]=LEAKBRACKET"#,
            r#"params['token']=LEAKBRACKET"#,
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:secret_mask_critic_round_eight::value_before_sensitive_record_name_does_not_publish_the_secret".to_owned(), value.to_owned()));
        };
        capture(r#"{"value":"LEAKBEFORE","name":"x-api-key"}"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:secret_mask_critic_round_eight::bare_sensitive_key_on_the_previous_line_does_not_publish_the_value".to_owned(), value.to_owned()));
        };
        capture("Authorization\nBearer LEAKBARE");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:secret_mask_critic_round_eight::confusable_sensitive_key_does_not_publish_the_value".to_owned(), value.to_owned()));
        };
        for cause in ["ｔｏｋｅｎ=LEAKCONFUSABLE", "tоken=LEAKCONFUSABLE"] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::standalone_auth_schemes_mask_the_remaining_public_diagnostic".to_owned(), value.to_owned()));
        };
        for cause in [
            "Authorization\nBearer LEAKSCHEME",
            "failed Basic LEAKSCHEME done",
            "failed tOkEn LEAKSCHEME done",
            "failed Digest response=LEAKSCHEME done",
        ] {
            capture(cause);
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::compatibility_and_confusable_names_mask_public_credentials".to_owned(), value.to_owned()));
        };
        for cause in [
            "ｔｏｋｅｎ＝LEAKUNICODE",
            "𝐭𝐨𝐤𝐞𝐧=LEAKUNICODE",
            "tоken=LEAKUNICODE",
            "tοken=LEAKUNICODE",
            "ｎａｍｅ: Qualified(\"scope\", \"tоken\"), value: LEAKUNICODE",
            "{\"value\":\"LEAKUNICODE\",\"nаme\":\"token\"}",
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::json_escaped_inside_a_string_masks_the_remaining_line".to_owned(), value.to_owned()));
        };
        capture(r#"body {\"token\":\"LEAK\"} rejected"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::digest_quoted_params_with_blanks_are_masked_to_the_last_one".to_owned(), value.to_owned()));
        };
        capture(r#"Authorization: Digest username="u", realm="my realm", response="LEAK" end"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::every_default_ignorable_or_format_mark_is_dropped_before_matching".to_owned(), value.to_owned()));
        };
        for mark in [
            '\u{034f}',
            '\u{fe0f}',
            '\u{e0041}',
            '\u{180e}',
            '\u{3164}',
            '\u{0600}',
            '\u{115f}',
        ] {
            capture(&format!("to{mark}ken=LEAK done"));
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:published_secret_mask_regressions::bracketed_and_tuple_names_are_masked"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        capture("params[token]=LEAK&a=1");
        capture(r#"header ("x-api-key", "LEAK") rejected"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::quoted_credential_with_a_blank_after_a_scheme_is_masked_whole".to_owned(), value.to_owned()));
        };
        capture(r#"Authorization: Bearer "abc LEAK" rejected"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::a_blank_after_a_sensitive_key_never_preserves_the_next_pair".to_owned(), value.to_owned()));
        };
        capture("empty key= mode=fast");
        capture("token: YWJjZA== end");
        capture("api_key = LEAK end");
        capture("password=first SECOND mode=fast");
        capture("token=first\nSECOND mode=fast");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::name_value_records_mask_the_value_member".to_owned(), value.to_owned()));
        };
        capture(r#"{"name":"x-api-key","value":"LEAK","k":1}"#);
        capture(r#"Header { name: "x-api-key", value: "LEAK" }"#);
        capture(r#"{"name":"content-type","value":"json"}"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::escaped_json_of_any_depth_masks_the_remaining_line".to_owned(), value.to_owned()));
        };
        capture(r#"o {\\\"token\\\":\\\"LEAK\\\"} end"#);
        capture(r#"o {\"password\":\"pa\\\"LEAK\"} end"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::nested_object_or_array_under_a_secret_name_is_masked_whole".to_owned(), value.to_owned()));
        };
        capture(r#"{ "password": { "value": "LEAK", "b": ["x"] } } ok"#);
        capture(r#"token=["a","}"] ok"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::call_wrapped_secret_values_mask_the_remaining_line".to_owned(), value.to_owned()));
        };
        capture(r#"token: Some("a LEAK") rejected"#);
        capture(r#"password=Secret(("x", ")")) end"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push((
                "legacy:published_secret_mask_regressions::base64_padding_in_value_is_fully_masked"
                    .to_owned(),
                value.to_owned(),
            ));
        };
        capture("token=YWJjZA== done");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::an_apparently_empty_value_masks_the_next_pair".to_owned(), value.to_owned()));
        };
        capture("empty key= mode=fast");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::name_value_record_hides_the_value_and_later_records".to_owned(), value.to_owned()));
        };
        let cause = r#"[{"name":"token","value":"LEAKR1"},{"name":"mode","value":"fast"}]"#;
        capture(cause);
        capture(cause);
        let escaped = r#"{\\\"name\\\":\\\"x-api-key\\\",\\\"value\\\":\\\"LEAKR2\\\"}"#;
        capture(escaped);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::a_sensitive_name_masks_later_records_even_without_its_value".to_owned(), value.to_owned()));
        };
        capture(r#"[{"name":"token","type":"string"},{"id":7,"value":"public"}]"#);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::brackets_inside_json_strings_of_an_escaped_nested_value_do_not_close_it".to_owned(), value.to_owned()));
        };
        let cause = r#"{\"token\":{\"a\":\"x}y\",\"b\":\"LEAKBR\"},\"user\":\"bob\"} done"#;
        capture(cause);
        capture(cause);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::brackets_inside_single_quoted_strings_of_a_nested_value_do_not_close_it".to_owned(), value.to_owned()));
        };
        let cause = r#"{'token': {'a': 'x}y', 'b': 'LEAKSQ'}, 'user': 'bob'} done"#;
        capture(cause);
        capture(cause);
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::nested_secret_values_and_the_text_after_them_are_masked".to_owned(), value.to_owned()));
        };
        for cause in [
            r#"{"token":{"a":{"b":"LEAKN1"},"c":["LEAKN2","LEAKN3"]},"user":"bob"} done"#,
            r#"{"token":{"a":"x}y","b":"LEAKN4"},"user":"bob"} done"#,
            r#"{\"token\":{\"a\":{\"b\":\"LEAKN5\"}},\"user\":\"bob\"} done"#,
        ] {
            capture(cause);
        }
        capture("token: {truncated LEAKN6");
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::embedded_escaped_quotes_stay_inside_the_secret_at_every_depth".to_owned(), value.to_owned()));
        };
        for cause in [
            r#"password="LEAKA\"LEAKB" user=bob"#,
            r#"password='LEAKA\'LEAKB' user=bob"#,
            r#"{\"password\":\"LEAKA\\\"LEAKB\",\"user\":\"bob\"}"#,
            r#"{\\\"password\\\":\\\"LEAKA\\\\\\\"LEAKB\\\",\\\"user\\\":\\\"bob\\\"}"#,
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::bracket_value_with_a_string_ending_in_an_escaped_backslash_is_masked_whole".to_owned(), value.to_owned()));
        };
        for (cause, _secret) in [
            (
                r#"token: {"dir":"C:\\","note":"}","pw":"LEAKB1"} done"#,
                "LEAKB1",
            ),
            (
                r#"token: {'dir': 'C:\\', 'note': ']', 'pw': 'LEAKB2'} done"#,
                "LEAKB2",
            ),
        ] {
            capture(cause);
        }
    }
    {
        let mut capture = |value: &str| {
            causes.push(("legacy:published_secret_mask_regressions::a_sensitive_record_name_masks_nested_members_and_its_value".to_owned(), value.to_owned()));
        };
        capture(r#"{"name":"x-api-key","schema":{"value":"public"},"value":"LEAK"}"#);
    }
    causes
}

// Catches: OAuth publication requires static headers, or capture identity omits
// the issuer/account binding and can resolve authority for another credential.
#[tokio::test(flavor = "current_thread")]
async fn oauth_publication_hashes_bound_identity_without_reading_rotating_tokens() {
    use ego_core::credentials::{
        CredentialKey, CredentialRemoval, CredentialStore, CredentialStoreError, CredentialTarget,
        SecretMaterial,
    };
    struct NoCredentialRead;
    impl CredentialStore for NoCredentialRead {
        fn get(&self, _: &CredentialKey) -> Result<Option<SecretMaterial>, CredentialStoreError> {
            panic!("capture must not read a token")
        }
        fn put(&self, _: &CredentialKey, _: SecretMaterial) -> Result<(), CredentialStoreError> {
            panic!("capture must not write a token")
        }
        fn delete(&self, _: &CredentialKey) -> Result<CredentialRemoval, CredentialStoreError> {
            panic!("capture must not delete a token")
        }
    }
    let admission = fixture_admission(
        (0..4)
            .map(|_| {
                Outcome::Candidate(candidate(
                    SERVER,
                    vec![CountingTool::new(
                        "alpha_tool",
                        Arc::new(AtomicUsize::new(0)),
                    )],
                ))
            })
            .collect(),
    );
    let (registry, _) = controller(admission);
    let http = ego_core::config::McpHttpConfig::new(
        "https://mcp.example.test/mcp".parse::<url::Url>().unwrap(),
        McpHttpAuth::Oauth,
        false,
    )
    .unwrap();
    let mut fingerprints = Vec::new();
    for (issuer, account) in [
        ("https://issuer.example.test/", None),
        ("https://issuer.example.test/", None),
        ("https://issuer.example.test/", Some("other-account")),
        ("https://other.example.test/", None),
    ] {
        let key = CredentialKey::new(
            issuer.into(),
            CredentialTarget::ProtectedResource(http.url.to_string()),
            account.map(str::to_owned),
        )
        .unwrap();
        let session = Arc::new(
            ego_mcp::McpOAuthSession::new(
                SERVER.parse().unwrap(),
                http.clone(),
                key,
                Arc::new(NoCredentialRead),
            )
            .unwrap(),
        );
        let live = Arc::new(
            McpServerAdmission::new(
                SERVER.parse().unwrap(),
                McpServerConfig::Http(http.clone()),
                McpTransportMaterial::oauth(session),
            )
            .unwrap(),
        );
        let publication = registry
            .reconcile(
                McpRegistryRefresh::new(
                    vec![McpRegistryCandidate::ready(
                        live,
                        McpConfigProvenance::Layer {
                            source: Source::User,
                        },
                    )],
                    RegistryTrigger::AuthorizationSucceeded,
                    RegistryScope::All,
                )
                .unwrap(),
            )
            .await
            .expect("OAuth server must publish without static header material");
        assert_eq!(publication.admitted_tools(), ["alpha_tool"]);
        fingerprints.push(publication.capture().stable_fingerprint().to_owned());
    }
    assert_eq!(
        fingerprints[0], fingerprints[1],
        "same binding survives a new OAuth session"
    );
    assert_ne!(
        fingerprints[0], fingerprints[2],
        "account changes authority"
    );
    assert_ne!(fingerprints[0], fingerprints[3], "issuer changes authority");
}
