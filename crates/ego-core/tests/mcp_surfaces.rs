//! RED contract for the core-owned MCP execution boundaries.

use std::sync::Arc;

use ego_core::credentials::SecretMaterial;
use ego_core::mcp::McpRuntimeBinding;
use ego_core::permit::{EnvName, EnvPolicy, HttpHeader, HttpRequest, HttpRequestDigest};
use ego_core::run::{EventSinkFactory, RunExecutionBindings};
use ego_core::sink::{EventSink, NullSink};
use ego_core::turn::{AdmittedToolSource, CapturedToolCatalogue, ToolCatalogueError};

#[derive(Debug, Default)]
struct SinkFactory;

impl EventSinkFactory for SinkFactory {
    fn create(
        &self,
        _capabilities: &ego_core::generator::capabilities::ModelCapabilities,
    ) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

#[derive(Debug, Default)]
struct EmptyMcpSource;

impl AdmittedToolSource for EmptyMcpSource {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        CapturedToolCatalogue::new("test-generation", Vec::new())
    }
}

#[derive(Debug, Default)]
struct UnusedMcpRuntime;

impl McpRuntimeBinding for UnusedMcpRuntime {
    fn foreign_question_port<'a>(
        &self,
        _dispatcher: ego_core::hook::HookLifecycleDispatcher,
        _floor: ego_core::suspension::ResumeFloor,
        _effects: ego_core::turn::ToolEffectPorts<'a>,
    ) -> Arc<dyn ego_core::mcp::ForeignQuestionPort + 'a> {
        panic!("this runtime-only binding is not exercised by the surface test")
    }
}

#[test]
fn explicit_environment_material_is_named_exposable_and_redacted() {
    let secret = "runtime-env-secret";
    let binding = ego_core::permit::ExplicitEnvBinding::new(
        EnvName::new("MCP_TOKEN").expect("valid environment name"),
        SecretMaterial::new(secret.as_bytes().to_vec()),
    );

    assert_eq!(binding.name().to_string(), "MCP_TOKEN");
    assert_eq!(
        binding.expose_value().expose_secret_bytes(),
        secret.as_bytes()
    );
    assert!(!format!("{binding:?}").contains(secret));

    let policy = EnvPolicy::exact(vec![binding]).expect("valid exact environment policy");
    assert_eq!(policy.names().len(), 1);
}

#[test]
fn secret_http_headers_are_redacted_and_digest_binds_secret_authority() {
    let first = HttpRequest::new(
        "POST",
        "/mcp",
        vec![
            HttpHeader::secret(
                "Authorization",
                SecretMaterial::new(b"Bearer first-secret".to_vec()),
            )
            .expect("valid secret header"),
        ],
        b"{}".to_vec(),
    )
    .expect("valid request");
    let second = HttpRequest::new(
        "POST",
        "/mcp",
        vec![
            HttpHeader::secret(
                "Authorization",
                SecretMaterial::new(b"Bearer second-secret".to_vec()),
            )
            .expect("valid secret header"),
        ],
        b"{}".to_vec(),
    )
    .expect("valid request");

    assert_eq!(first.headers()[0].name(), "authorization");
    assert!(!format!("{:?}", first.headers()[0]).contains("first-secret"));
    // Secret bytes are runtime authority, not fingerprint identity. A safe
    // digest may bind the header name and request shape, but never the secret.
    assert_eq!(
        HttpRequestDigest::of(&first),
        HttpRequestDigest::of(&second)
    );
    let public_change = HttpRequest::new(
        "POST",
        "/mcp",
        vec![HttpHeader::new("x-mcp-revision", b"two".to_vec()).expect("valid header")],
        b"{}".to_vec(),
    )
    .expect("valid request");
    assert_ne!(
        HttpRequestDigest::of(&first),
        HttpRequestDigest::of(&public_change)
    );
    assert!(!format!("{:?}", HttpRequestDigest::of(&first)).contains("first-secret"));
}

#[test]
fn execution_bindings_keep_optional_live_mcp_source_and_safe_errors() {
    let empty = RunExecutionBindings::new(Arc::new(SinkFactory), None);
    assert!(empty.admitted_tools().is_none());
    assert!(empty.mcp_errors().is_empty());

    let source: Arc<dyn AdmittedToolSource> = Arc::new(EmptyMcpSource);
    let bindings = empty.with_mcp(
        Arc::clone(&source),
        vec!["server 'files' is unavailable".to_owned()],
        Arc::new(UnusedMcpRuntime),
    );
    assert!(bindings.admitted_tools().is_some());
    assert_eq!(
        bindings.mcp_errors(),
        ["server 'files' is unavailable".to_owned()]
    );
}

#[test]
fn existing_run_selection_model_has_no_live_mcp_attachment_shape() {
    let selections = ego_core::run::RunSelections::empty();
    let encoded = serde_json::to_string(&selections).expect("run selections serialize");
    assert!(!encoded.contains("mcp"));
}
