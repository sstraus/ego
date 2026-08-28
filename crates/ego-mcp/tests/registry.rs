//! Final-contract RED tests for one immutable admitted MCP registry.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use ego_core::config::McpServerConfig;
use ego_core::config::{ConfigLayer, McpConfigProvenance, Source, Trust, resolve};
use ego_core::mcp::ToolExecution;
use ego_core::permit::{PreparedCall, ToolError};
use ego_core::turn::{McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError};
use ego_mcp::{
    McpAdmittedCandidate, McpCandidateAdmission, McpCandidateError, McpCandidateInventory,
    McpCandidateRequest, McpOperation, McpOperationFactory, McpRegistryController,
    McpRegistryError, McpRegistryRefresh, RegistryScope, RegistryTrigger,
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
    Candidate(McpAdmittedCandidate),
    Invalid(String),
    AuthorizationRequired {
        issuer: String,
        protected_resource: String,
        scopes: Vec<String>,
    },
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
                state.requests.push(request);
                state.active += 1;
                state.max_active = state.max_active.max(state.active);
                let gate = gate.lock().expect("admission gate").take();
                let outcome = state.queue.pop_front().expect("admission outcome");
                (gate, outcome)
            };
            if let Some(gate) = outcome.0 {
                gate.notified().await;
            }
            let outcome = {
                let mut state = state.lock().expect("admission state");
                state.active -= 1;
                outcome.1
            };
            match outcome {
                Outcome::Candidate(candidate) => Ok(candidate),
                Outcome::Invalid(cause) => Err(McpCandidateError::Invalid { cause }),
                Outcome::AuthorizationRequired {
                    issuer,
                    protected_resource,
                    scopes,
                } => Err(McpCandidateError::AuthorizationRequired {
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
}
impl CountingTool {
    fn new(name: &str, dropped: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            dropped,
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
            parameters: json!({"type":"object","additionalProperties":false}),
        }
    }
}

struct CountingFactory {
    tools: Vec<Arc<CountingTool>>,
}

impl McpOperationFactory for CountingFactory {
    fn prepare(
        &self,
        _config: &McpServerConfig,
        _operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall::new("counted", Vec::new()))
    }

    fn execute<'a>(
        &'a self,
        _config: &'a McpServerConfig,
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
fn refresh(trigger: RegistryTrigger) -> McpRegistryRefresh {
    McpRegistryRefresh::new(config(), trigger, RegistryScope::All)
}
fn empty_config() -> ego_core::config::ResolvedMcpConfig {
    let layer = ConfigLayer::parse(Source::User, "").expect("empty valid MCP layer");
    resolve(&[layer], Trust::Trusted, None)
        .expect("resolved empty MCP config")
        .mcp
}
fn refresh_with(
    config: ego_core::config::ResolvedMcpConfig,
    trigger: RegistryTrigger,
) -> McpRegistryRefresh {
    McpRegistryRefresh::new(config, trigger, RegistryScope::All)
}
fn candidate(name: &str, tools: Vec<Arc<CountingTool>>) -> McpAdmittedCandidate {
    let descriptors = tools
        .iter()
        .map(|tool| tool.generator_descriptor())
        .collect();
    let factory = Arc::new(CountingFactory { tools });
    McpAdmittedCandidate::new(
        name.to_owned(),
        ego_mcp::McpCandidateIdentity::validated(name),
        McpServerConfig::stdio(
            "mcp-alpha".to_owned(),
            vec!["--current".to_owned()],
            None,
            Vec::new(),
        )
        .expect("stdio candidate config"),
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
    admission.push(Outcome::Invalid("duplicate tool identity".to_owned()));
    assert!(
        registry
            .reconcile(refresh(RegistryTrigger::ToolsListChanged))
            .await
            .is_err()
    );
    let failed = registry.publication();
    assert_eq!(failed.generation(), first.generation());
    assert_eq!(failed.admitted_tools(), ["a_tool", "z_tool"]);
    assert!(registry.status().is_degraded());
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
    assert_eq!(registry.publication().generation(), first.generation());
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
    admission.push(Outcome::Invalid("schema rejected".to_owned()));
    let (registry, journal) = controller(Arc::clone(&admission));
    assert!(
        registry
            .reconcile(refresh(RegistryTrigger::Add))
            .await
            .is_err()
    );
    assert!(registry.publication().admitted_tools().is_empty());
    assert!(registry.status().admitted().is_empty());
    admission.push(Outcome::AuthorizationRequired {
        issuer: "https://issuer.example".to_owned(),
        protected_resource: "https://mcp.example".to_owned(),
        scopes: vec!["tools".to_owned()],
    });
    assert!(
        registry
            .reconcile(refresh(RegistryTrigger::Login))
            .await
            .is_err()
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
    assert_eq!(request.name(), SERVER);
    assert_eq!(
        request.server(),
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
