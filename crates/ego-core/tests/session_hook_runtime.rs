//! RED contracts for the borrow-scoped shared session hook runtime.

use std::sync::Arc;

use ego_core::event::hook::HookLifecycleDispatcher;
use ego_core::hook::SessionHookRuntime;
use ego_core::mcp::{ForeignQuestionPort, McpRuntimeBinding, ToolExecution};
use ego_core::run::{EventSinkFactory, RunExecutionBindings};
use ego_core::sink::{EventSink, NullSink};
use ego_core::suspension::ResumeFloor;
use ego_core::turn::{SharedJournal, ToolEffectPorts};
use futures::future::BoxFuture;

struct RejectingPort;

impl ForeignQuestionPort for RejectingPort {
    fn resume<'a>(
        &'a self,
        _continuation_id: &'a str,
        _canonical_json: &'a str,
    ) -> BoxFuture<'a, Result<ToolExecution, String>> {
        Box::pin(async { Err("runtime-bound port was not invoked".to_owned()) })
    }
}

#[derive(Default)]
struct RuntimeBinding;

impl McpRuntimeBinding for RuntimeBinding {
    fn foreign_question_port<'a>(
        &self,
        runtime: SessionHookRuntime<'a>,
        _floor: ResumeFloor,
        _effects: ToolEffectPorts<'a>,
    ) -> Arc<dyn ForeignQuestionPort + 'a> {
        let _ = runtime;
        Arc::new(RejectingPort)
    }
}

fn assert_borrow_scoped<'a>(runtime: &'a SessionHookRuntime<'a>) -> &'a SessionHookRuntime<'a> {
    runtime
}

#[test]
fn runtime_binds_one_journal_and_dispatcher_without_static_session_state() {
    fn keep_runtime<'a>(
        runtime: SessionHookRuntime<'a>,
        journal: SharedJournal<'a>,
        dispatcher: HookLifecycleDispatcher,
    ) -> (SessionHookRuntime<'a>, SharedJournal<'a>, HookLifecycleDispatcher) {
        let _ = assert_borrow_scoped(&runtime);
        (runtime, journal, dispatcher)
    }

    let _ = keep_runtime;
    fn assert_send<T: Send>() {}
    assert_send::<SessionHookRuntime<'static>>();
}

#[test]
fn guarded_mcp_resume_accepts_runtime_and_keeps_execution_bindings_cloneable() {
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

    let binding: Arc<dyn McpRuntimeBinding> = Arc::new(RuntimeBinding);
    let bindings = RunExecutionBindings::new(Arc::new(SinkFactory), None);
    let cloned = bindings.clone();
    assert!(Arc::ptr_eq(&Arc::clone(&binding), &binding));
    let _ = (cloned, binding);
}
