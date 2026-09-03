//! RED contract for the single live MCP admission authority.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ego_core::config::{McpConfigProvenance, McpHttpAuth, McpServerConfig, McpServerName, Source};
use ego_core::credentials::SecretMaterial;
use ego_core::generator::types::ToolDescriptor;
use ego_core::mcp::ToolExecution;
use ego_core::permit::{CanonPath, GuardedIo, Op, PreparedCall, ToolError};
use ego_core::turn::{McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError};
use ego_mcp::{
    McpAdmittedCandidate, McpCacheMetadata, McpCandidateAdmission, McpCandidateError,
    McpCandidateIdentity, McpCandidateInventory, McpCapabilities, McpCollapsedBootstrap,
    McpCollapsedCatalogue, McpEnvironmentBinding, McpHeaderBinding, McpOperation,
    McpOperationFactory, McpRegistryCandidate, McpRegistryController, McpRegistryRefresh,
    McpServerAdmission, McpTransportMaterial, RegistryScope, RegistryTrigger,
};
use futures::future::BoxFuture;
use serde_json::{Value, json};

#[test]
fn admission_retains_acp_transport_values_but_debug_never_reveals_them() {
    let name: McpServerName = "files".parse().expect("valid server name");
    let config = McpServerConfig::stdio("/bin/true".into(), vec![], None, vec![])
        .expect("valid stdio config");
    let env = McpEnvironmentBinding::new(
        "TOKEN".parse().expect("valid environment name"),
        SecretMaterial::new(b"session-secret".to_vec()),
    );
    let header = McpHeaderBinding::new(
        "Authorization",
        SecretMaterial::new(b"Bearer session-secret".to_vec()),
    )
    .expect("valid header");
    let admission = McpServerAdmission::new(
        name.clone(),
        config.clone(),
        McpTransportMaterial::stdio(vec![env]),
    )
    .expect("admission");
    assert_eq!(admission.name(), &name);
    let debug = format!("{admission:?}");
    assert!(!debug.contains("session-secret"));

    let http_config = McpServerConfig::http(
        "https://example.test/mcp".parse().expect("valid URL"),
        McpHttpAuth::None,
        false,
    )
    .expect("valid HTTP config");
    let http = McpServerAdmission::new(name, http_config, McpTransportMaterial::http(vec![header]))
        .expect("HTTP admission");
    assert!(matches!(http.config(), McpServerConfig::Http(_)));
    let debug = format!("{http:?}");
    assert!(!debug.contains("Bearer session-secret"));
}

#[test]
fn admission_rejects_duplicate_case_insensitive_transport_names() {
    let name: McpServerName = "files".parse().expect("valid server name");
    let config = McpServerConfig::stdio("/bin/true".into(), vec![], None, vec![])
        .expect("valid stdio config");
    let first = McpEnvironmentBinding::new(
        "TOKEN".parse().expect("valid environment name"),
        SecretMaterial::new(b"one".to_vec()),
    );
    let second = McpEnvironmentBinding::new(
        "token".parse().expect("valid environment name"),
        SecretMaterial::new(b"two".to_vec()),
    );
    assert!(
        McpServerAdmission::new(
            name,
            config,
            McpTransportMaterial::stdio(vec![first, second]),
        )
        .is_err()
    );
}

#[test]
fn header_debug_redacts_secret_material() {
    let header = McpHeaderBinding::new(
        "Authorization",
        SecretMaterial::new(b"Bearer top-secret".to_vec()),
    )
    .expect("valid header");
    assert!(!format!("{header:?}").contains("top-secret"));
}

#[test]
fn transport_material_validates_header_names_and_preserves_binding_names() {
    let env = McpEnvironmentBinding::new(
        "API_TOKEN".parse().expect("valid environment name"),
        SecretMaterial::new(b"environment-top-secret".to_vec()),
    );
    assert_eq!(env.name().as_str(), "API_TOKEN");
    assert!(!format!("{env:?}").contains("environment-top-secret"));

    for name in ["", "bad header", "content_length", "x\r\n-injected"] {
        assert!(
            McpHeaderBinding::new(name, SecretMaterial::new(b"value".to_vec())).is_err(),
            "invalid header name {name:?} was accepted"
        );
    }
}

#[test]
fn refresh_admits_live_values_and_rejects_duplicate_server_names() {
    use ego_core::config::{McpConfigProvenance, Source};
    use ego_mcp::{McpRegistryRefresh, RegistryScope, RegistryTrigger};

    let make = |name: &str| {
        Arc::new(
            McpServerAdmission::new(
                name.parse().expect("valid server name"),
                McpServerConfig::stdio(name.to_owned(), Vec::new(), None, Vec::new())
                    .expect("valid stdio config"),
                McpTransportMaterial::stdio(Vec::new()),
            )
            .expect("valid admission"),
        )
    };
    let provenance = McpConfigProvenance::Layer {
        source: Source::User,
    };
    let alpha = make("alpha");
    let beta = make("beta");
    assert!(
        McpRegistryRefresh::new(
            vec![
                McpRegistryCandidate::ready(Arc::clone(&alpha), provenance.clone()),
                McpRegistryCandidate::ready(beta, provenance.clone()),
            ],
            RegistryTrigger::Reload,
            RegistryScope::All,
        )
        .is_ok()
    );
    assert!(
        McpRegistryRefresh::new(
            vec![
                McpRegistryCandidate::ready(alpha.clone(), provenance.clone()),
                McpRegistryCandidate::ready(alpha, provenance),
            ],
            RegistryTrigger::Reload,
            RegistryScope::All,
        )
        .is_err()
    );
}

const COLLAPSED_WRAPPER: &str = r#"{"type":"object","properties":{"tool_name":{"type":"string"},"arguments":{"type":"object"}},"required":["tool_name","arguments"],"additionalProperties":false}"#;

fn descriptor(name: &str, parameters: Value) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: "dynamic fixture".to_owned(),
        parameters,
    }
}

fn collapsed_management() -> Vec<ToolDescriptor> {
    vec![
        descriptor(
            "search_tools",
            json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1}},"required":["query"],"additionalProperties":false}),
        ),
        descriptor(
            "get_tool_schema",
            json!({"type":"object","properties":{"tool_name":{"type":"string"}},"required":["tool_name"],"additionalProperties":false}),
        ),
        descriptor(
            "call_tool",
            serde_json::from_str(COLLAPSED_WRAPPER).expect("wrapper"),
        ),
    ]
}

fn dynamic_admission() -> Arc<McpServerAdmission> {
    Arc::new(
        McpServerAdmission::new(
            "dynamic".parse().expect("server name"),
            McpServerConfig::stdio("mcp-dynamic".to_owned(), Vec::new(), None, Vec::new())
                .expect("stdio config"),
            McpTransportMaterial::stdio(Vec::new()),
        )
        .expect("admission"),
    )
}

#[derive(Default)]
struct DynamicJournal {
    events: Mutex<Vec<McpRegistryEvent>>,
    revision: Mutex<u64>,
}

impl McpRegistryJournal for DynamicJournal {
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
        let mut revision = self.revision.lock().expect("revision");
        *revision += 1;
        self.events
            .lock()
            .expect("events")
            .push(McpRegistryEvent::RefreshRequested {
                request_id,
                trigger,
                server_scope,
                config_revision: *revision,
            });
        Ok(*revision)
    }
}

#[derive(Default)]
struct DynamicFactory;

impl McpOperationFactory for DynamicFactory {
    fn prepare(
        &self,
        _admission: Arc<McpServerAdmission>,
        _operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall::new(
            "dynamic",
            Op::Read {
                path: CanonPath::new("/fixture"),
            },
        ))
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
            Ok(ToolExecution::Complete(
                ego_core::permit::ToolResult::builtin("dynamic"),
            ))
        })
    }
}

enum DynamicOutcome {
    Candidate(McpAdmittedCandidate),
}

#[derive(Default)]
struct DynamicAdmission {
    outcomes: Mutex<VecDeque<DynamicOutcome>>,
}

impl DynamicAdmission {
    fn push(&self, outcome: DynamicOutcome) {
        self.outcomes.lock().expect("outcomes").push_back(outcome);
    }
}

impl McpCandidateAdmission for DynamicAdmission {
    fn admit(
        &self,
        _request: ego_mcp::McpCandidateRequest,
    ) -> ego_mcp::McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
        let outcome = self.outcomes.lock().expect("outcomes").pop_front();
        Box::pin(async move {
            match outcome.expect("admission outcome") {
                DynamicOutcome::Candidate(candidate) => Ok(candidate),
            }
        })
    }
}

fn dynamic_candidate(
    admission: Arc<McpServerAdmission>,
    catalogue: Option<McpCollapsedCatalogue>,
) -> McpAdmittedCandidate {
    McpAdmittedCandidate::new(
        Arc::clone(&admission),
        McpCandidateIdentity::validated("dynamic"),
        McpCandidateInventory::new(
            McpCapabilities::tools(),
            collapsed_management(),
            Vec::new(),
            Vec::new(),
            McpCacheMetadata::default(),
            catalogue,
        ),
        Arc::new(DynamicFactory),
    )
    .expect("dynamic candidate")
}

fn dynamic_refresh(admission: &Arc<McpServerAdmission>) -> McpRegistryRefresh {
    McpRegistryRefresh::new(
        vec![McpRegistryCandidate::ready(
            Arc::clone(admission),
            McpConfigProvenance::Layer {
                source: Source::User,
            },
        )],
        RegistryTrigger::Reload,
        RegistryScope::All,
    )
    .expect("dynamic refresh")
}

#[tokio::test(flavor = "current_thread")]
async fn collapsed_family_without_schema_publishes_management_only_bootstrap() {
    let bootstrap = McpCollapsedBootstrap::new(collapsed_management()).expect("bootstrap");
    assert_eq!(
        bootstrap.tool_names().collect::<Vec<_>>(),
        vec!["get_tool_schema", "search_tools"]
    );
    assert!(!bootstrap.tool_names().any(|name| name == "call_tool"));
    let admission = dynamic_admission();
    let queue = Arc::new(DynamicAdmission::default());
    queue.push(DynamicOutcome::Candidate(dynamic_candidate(
        Arc::clone(&admission),
        None,
    )));
    let journal = Arc::new(DynamicJournal::default());
    let registry = McpRegistryController::new(
        Arc::clone(&journal) as Arc<dyn McpRegistryJournal>,
        Arc::clone(&queue) as Arc<dyn McpCandidateAdmission>,
    )
    .expect("controller");

    let publication = registry
        .reconcile(dynamic_refresh(&admission))
        .await
        .expect("management bootstrap");
    assert_eq!(publication.generation(), "generation-1");
    assert_eq!(
        publication.admitted_tools(),
        &["get_tool_schema".to_owned(), "search_tools".to_owned()]
    );
    assert!(
        !publication
            .admitted_tools()
            .iter()
            .any(|name| name == "call_tool")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn described_collapsed_catalogue_publishes_next_generation_and_keeps_old_capture() {
    let admission = dynamic_admission();
    let queue = Arc::new(DynamicAdmission::default());
    queue.push(DynamicOutcome::Candidate(dynamic_candidate(
        Arc::clone(&admission),
        None,
    )));
    let old_schema = json!({
        "type":"object",
        "properties":{"city":{"type":"string"}},
        "required":["city"],
        "additionalProperties":false
    });
    let catalogue = McpCollapsedCatalogue::new(vec![descriptor("weather", old_schema.clone())])
        .expect("described catalogue");
    queue.push(DynamicOutcome::Candidate(dynamic_candidate(
        Arc::clone(&admission),
        Some(catalogue),
    )));
    let journal = Arc::new(DynamicJournal::default());
    let registry = McpRegistryController::new(
        Arc::clone(&journal) as Arc<dyn McpRegistryJournal>,
        Arc::clone(&queue) as Arc<dyn McpCandidateAdmission>,
    )
    .expect("controller");

    let first = registry
        .reconcile(dynamic_refresh(&admission))
        .await
        .expect("bootstrap");
    let old_capture = first.capture().clone();
    let second = registry
        .reconcile(dynamic_refresh(&admission))
        .await
        .expect("described publication");
    assert_eq!(second.generation(), "generation-2");
    assert_eq!(second.predecessor(), Some("generation-1"));
    assert!(
        second
            .admitted_tools()
            .iter()
            .any(|name| name == "call_tool")
    );
    assert!(
        !old_capture
            .tools()
            .iter()
            .any(|tool| tool.descriptor().name == "call_tool")
    );
    assert!(
        second
            .capture()
            .servers()
            .iter()
            .all(|server| std::ptr::eq(server.admission(), admission.as_ref()))
    );
}

#[test]
fn collapsed_schema_validation_rejects_invalid_catalogue_without_a_partial_value() {
    let invalid = [
        vec![descriptor("weather", json!({"type": 7}))],
        vec![
            descriptor(
                "weather",
                json!({"type":"object","additionalProperties":false}),
            ),
            descriptor(
                "weather",
                json!({"type":"object","additionalProperties":false}),
            ),
        ],
        vec![descriptor(
            "call_tool",
            json!({"type":"object","additionalProperties":false}),
        )],
    ];
    for descriptors in invalid {
        assert!(McpCollapsedCatalogue::new(descriptors).is_err());
    }
    assert!(McpCollapsedCatalogue::new(Vec::new()).is_err());
}
