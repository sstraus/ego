//! RED contract for one explicit MCP runtime binding per outer session.

use std::sync::Arc;

use ego_core::event::hook::{HookFailure, HookFailureKind};
use ego_core::hook::{
    HookCausality, HookFuture, HookInvocationPort, HookInvocationRequest, SessionHookRuntime,
};
use ego_core::mcp::{ForeignQuestionPort, McpRuntimeBinding, ToolExecution};
use ego_core::run::{EventSinkFactory, RunExecutionBindings};
use ego_core::sink::{EventSink, NullSink};
use ego_core::suspension::ResumeFloor;
use ego_core::turn::ToolEffectPorts;
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
        Box::pin(async { Err("session-bound port was not invoked".to_owned()) })
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
        Err("composition fixture has no pending foreign policy".to_owned())
    }

    fn bind_pending_foreign_continuation(
        &self,
        _continuation_id: &str,
        _snapshot: Arc<ego_core::turn::GenerationPolicySnapshot>,
    ) -> Result<(), String> {
        Err("composition fixture cannot bind a pending continuation".to_owned())
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

#[test]
fn execution_bindings_retain_one_clone_stable_runtime_and_factory_contract() {
    let runtime: Arc<dyn McpRuntimeBinding> = Arc::new(TestMcpRuntime);
    let bindings = RunExecutionBindings::new(Arc::new(SinkFactory), None).with_mcp(
        Arc::new(EmptyAdmittedTools),
        vec!["safe MCP diagnostic".to_owned()],
        Arc::clone(&runtime),
    );
    let cloned = bindings.clone();

    assert!(Arc::ptr_eq(
        &runtime,
        &cloned.mcp_runtime().expect("runtime binding retained")
    ));
    assert_eq!(cloned.mcp_errors(), ["safe MCP diagnostic"]);
}

#[derive(Debug, Default)]
struct EmptyAdmittedTools;

impl ego_core::turn::AdmittedToolSource for EmptyAdmittedTools {
    fn capture(
        &self,
    ) -> Result<ego_core::turn::CapturedToolCatalogue, ego_core::turn::ToolCatalogueError> {
        ego_core::turn::CapturedToolCatalogue::new("session-composition", Vec::new())
    }
}
