//! RED contract for one explicit MCP runtime binding per outer session.

use std::sync::Arc;

use ego_core::hook::SessionHookRuntime;
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
    ) -> BoxFuture<'a, Result<ToolExecution, String>> {
        Box::pin(async { Err("session-bound port was not invoked".to_owned()) })
    }
}

#[derive(Default)]
struct TestMcpRuntime;

impl McpRuntimeBinding for TestMcpRuntime {
    fn foreign_question_port<'a>(
        &self,
        _runtime: SessionHookRuntime<'a>,
        _floor: ResumeFloor,
        _effects: ToolEffectPorts<'a>,
    ) -> Arc<dyn ForeignQuestionPort + 'a> {
        Arc::new(RejectingPort)
    }
}

fn attach_borrow_scoped_port<'a>(
    turn: ego_core::turn::TurnLoop<'a>,
    port: Arc<dyn ForeignQuestionPort + 'a>,
) -> ego_core::turn::TurnLoop<'a> {
    turn.with_foreign_question_port(port)
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
    let _ = attach_borrow_scoped_port;
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
