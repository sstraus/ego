//! Final-contract RED tests for one immutable admitted MCP registry.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use ego_core::config::{
    ConfigLayer, McpConfigProvenance, ResolvedMcpConfig, ResolvedMcpEntry, Source, Trust, resolve,
};
use ego_core::config::{McpHttpAuth, McpServerConfig, McpServerName};
use ego_core::mcp::ToolExecution;
use ego_core::permit::{PreparedCall, ToolError};
use ego_core::turn::{McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError};
use ego_mcp::{
    McpAdmittedCandidate, McpCandidateAdmission, McpCandidateError, McpCandidateInventory,
    McpCandidateRequest, McpFailure, McpOperation, McpOperationFactory, McpRegistryCandidate,
    McpRegistryController, McpRegistryError, McpRegistryRefresh, McpServerAdmission,
    McpTransportMaterial, RegistryScope, RegistryTrigger,
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
                    Ok(admitted_candidate(admission, fixture.tools))
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
        Ok(PreparedCall::new("counted", Vec::new()))
    }

    fn execute<'a>(
        &'a self,
        _admission: Arc<McpServerAdmission>,
        _call: PreparedCall,
        _io: &'a dyn ego_core::permit::GuardedIo,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>> {
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
            ResolvedMcpEntry::Configured { server, provenance } => {
                let material = match server {
                    McpServerConfig::Stdio(_) => McpTransportMaterial::stdio(Vec::new()),
                    McpServerConfig::Http(_) => McpTransportMaterial::http(Vec::new()),
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
    }
}
fn admitted_candidate(
    admission: Arc<McpServerAdmission>,
    tools: Vec<Arc<CountingTool>>,
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
    assert!(
        absent
            .mcp_errors()
            .iter()
            .all(|error| !error.contains(raw_backend_error))
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
    admission.push(Outcome::Invalid("zeta-backend-secret".to_owned()));
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
    assert!(
        status
            .mcp_errors()
            .iter()
            .all(|error| !error.contains("zeta-backend-secret"))
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
    assert!(errors[0].contains("beta"));
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
