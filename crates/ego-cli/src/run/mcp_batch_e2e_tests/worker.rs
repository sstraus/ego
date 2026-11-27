use super::*;

const TEST: &str = concat!(
    module_path!(),
    "::headless_worker_receives_mcp_lookup_without_the_batch_tool"
);

// Catches: a worker inheriting the headless parent's batch descriptor even
// though it may call the same admitted MCP tools individually.
#[test]
fn headless_worker_receives_mcp_lookup_without_the_batch_tool() {
    const WORKER_PROMPT: &str = "Look up the worker-only key.";
    if std::env::var_os(CHILD).is_none() {
        let task = json!({"name":"Worker lookup", "prompt":WORKER_PROMPT});
        let stub = Stub::start_concurrent(Plan {
            completions: vec![
                Reply::Sse(support::tool_call("delegate", "task", &task.to_string())),
                Reply::Sse(support::text("parent finished")),
            ],
            prompt_completions: vec![(
                WORKER_PROMPT.to_owned(),
                vec![
                    Reply::Sse(support::tool_call(
                        "worker-lookup",
                        "lookup",
                        r#"{"key":"worker"}"#,
                    )),
                    Reply::Sse(support::text("worker finished")),
                ],
            )],
            ..Plan::default()
        });
        let fixture = StdioFixture::new("measure", Duration::ZERO);
        fixture.child(&stub, TEST, "worker");
        let calls = read_dispatches(&fixture.workspace.path().join("dispatch.log"));
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0]["key"], "worker");
        let requests = stub.generations();
        let offered = |request: &Value, name: &str| {
            request["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == name))
        };
        assert!(offered(&requests[0], "mcp_batch"));
        let workers: Vec<_> = requests
            .iter()
            .filter(|request| {
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "user" && message["content"] == WORKER_PROMPT)
            })
            .collect();
        assert_eq!(workers.len(), 2, "{requests:?}");
        for request in workers {
            assert!(offered(request, "lookup"));
            assert!(!offered(request, "mcp_batch"));
        }
        return;
    }
    super::headless_driver::TEST_MCP_BATCH.store(true, Ordering::SeqCst);
    let crate::cli::Invocation::Interactive(mut args) = crate::cli::parse(fixture_args()).unwrap()
    else {
        panic!("expected launch arguments");
    };
    args.prompt = Some("Delegate the lookup to a worker.".to_owned());
    args.no_hooks = true;
    let ended = runtime().block_on(run_with_registry(
        *args,
        |_| &NullSink,
        CancelToken::new(),
        Arc::new(EvidenceAdmissionService::new()),
        None,
    ));
    assert_eq!(ended.termination.exit_code(), 0, "{:?}", ended.termination);
}
