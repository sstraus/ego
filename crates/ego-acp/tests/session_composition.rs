//! contract for one opaque live MCP binding per ACP session.

use std::sync::Arc;

use ego_acp::serve::SessionMcpAdmission;
use ego_core::event::hook::{HookFailure, HookFailureKind};
use ego_core::hook::{
    HookCausality, HookFuture, HookInvocationPort, HookInvocationRequest, SessionHookRuntime,
};
use ego_core::mcp::{ForeignQuestionPort, McpRuntimeBinding, ToolExecution};
use ego_core::run::{EventSinkFactory, RunExecutionBindings};
use ego_core::sink::{EventSink, NullSink};
use ego_core::suspension::ResumeFloor;
use ego_core::turn::{AdmittedToolSource, CapturedToolCatalogue, ToolEffectPorts};
use futures::future::BoxFuture;

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

struct RejectingPort;

impl ForeignQuestionPort for RejectingPort {
    fn resume<'a>(
        &'a self,
        _continuation_id: &'a str,
        _canonical_json: &'a str,
        _causality: HookCausality,
    ) -> BoxFuture<'a, Result<ToolExecution, String>> {
        Box::pin(async { Err("ACP test port was not invoked".to_owned()) })
    }
}

#[derive(Default)]
struct TestMcpRuntime;

struct UnusedHookPort;

impl HookInvocationPort for UnusedHookPort {
    fn invoke<'a>(&'a self, _request: HookInvocationRequest) -> HookFuture<'a> {
        Box::pin(async { Err(HookFailure::new(HookFailureKind::HandlerFailed, None)) })
    }
}

impl McpRuntimeBinding for TestMcpRuntime {
    fn load_pending_foreign_policy(
        &self,
        _continuation_id: &str,
    ) -> Result<Arc<ego_core::turn::GenerationPolicySnapshot>, String> {
        Err("ACP composition fixture has no pending foreign policy".to_owned())
    }

    fn bind_pending_foreign_continuation(
        &self,
        _continuation_id: &str,
        _snapshot: Arc<ego_core::turn::GenerationPolicySnapshot>,
    ) -> Result<(), String> {
        Err("ACP composition fixture cannot bind a pending continuation".to_owned())
    }

    fn hook_invocation_port<'a>(
        &self,
        _policy: Arc<ego_core::permit::PolicyEngine>,
        _effects: ToolEffectPorts<'a>,
    ) -> Result<Arc<dyn HookInvocationPort + 'a>, String> {
        Ok(Arc::new(UnusedHookPort))
    }

    fn foreign_question_port<'session, 'turn>(
        &self,
        _runtime: SessionHookRuntime<'session>,
        _floor: ResumeFloor,
        _effects: ToolEffectPorts<'turn>,
    ) -> Arc<dyn ForeignQuestionPort + 'turn>
    where
        'session: 'turn,
    {
        Arc::new(RejectingPort)
    }
}

#[derive(Debug, Default)]
struct EmptyAdmittedTools;

impl AdmittedToolSource for EmptyAdmittedTools {
    fn capture(&self) -> Result<CapturedToolCatalogue, ego_core::turn::ToolCatalogueError> {
        CapturedToolCatalogue::new("acp-session-composition", Vec::new())
    }
}

struct SessionAdmission {
    runtime: Arc<dyn McpRuntimeBinding>,
    source: Arc<dyn AdmittedToolSource>,
    errors: Vec<String>,
}

impl SessionMcpAdmission for SessionAdmission {
    fn admitted_tools(&self) -> Arc<dyn AdmittedToolSource> {
        Arc::clone(&self.source)
    }

    fn mcp_errors(&self) -> &[String] {
        &self.errors
    }

    fn mcp_runtime(&self) -> Arc<dyn McpRuntimeBinding> {
        Arc::clone(&self.runtime)
    }
}

fn bindings_from_admission(admission: &dyn SessionMcpAdmission) -> RunExecutionBindings {
    RunExecutionBindings::new(Arc::new(SinkFactory), None).with_mcp(
        admission.admitted_tools(),
        admission.mcp_errors().to_vec(),
        admission.mcp_runtime(),
    )
}

#[test]
fn concurrent_acp_sessions_forward_distinct_opaque_runtime_authorities() {
    let first_runtime: Arc<dyn McpRuntimeBinding> = Arc::new(TestMcpRuntime);
    let second_runtime: Arc<dyn McpRuntimeBinding> = Arc::new(TestMcpRuntime);
    let first = SessionAdmission {
        runtime: Arc::clone(&first_runtime),
        source: Arc::new(EmptyAdmittedTools),
        errors: vec!["first MCP diagnostic".to_owned()],
    };
    let second = SessionAdmission {
        runtime: Arc::clone(&second_runtime),
        source: Arc::new(EmptyAdmittedTools),
        errors: vec!["second MCP diagnostic".to_owned()],
    };

    let first_bindings = bindings_from_admission(&first);
    let second_bindings = bindings_from_admission(&second);
    let first_forwarded = first_bindings.mcp_runtime().expect("first runtime binding");
    let second_forwarded = second_bindings
        .mcp_runtime()
        .expect("second runtime binding");

    assert!(Arc::ptr_eq(&first_runtime, &first_forwarded));
    assert!(Arc::ptr_eq(&second_runtime, &second_forwarded));
    assert!(!Arc::ptr_eq(&first_forwarded, &second_forwarded));
    assert_eq!(first_bindings.mcp_errors(), ["first MCP diagnostic"]);
    assert_eq!(second_bindings.mcp_errors(), ["second MCP diagnostic"]);
}
