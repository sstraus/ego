//! The HTTP client side of MCP Tasks (SEP-2663).
//!
//! The server frames are serialized by `rmcp` 3.1.4 itself, the protocol
//! implementation the specification ships with, so a field rename there fails
//! these tests instead of silently desynchronizing ego.

use std::sync::Arc;

use ego_mcp::McpTaskState;
use ego_mcp::{McpClient, McpClientOptions, McpHttpEffects, McpHttpResponse, McpManualOutcome};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, CreateTaskResult, DetailedTask,
    GetTaskResult, Task, TaskAckResult, TaskPayload, TaskStatus,
};
use serde_json::{Value, json};

use crate::protocol::{FakeHttp, http_config, options};

fn frame(id: u64, result: impl serde::Serialize) -> McpHttpResponse {
    let body = json!({"jsonrpc":"2.0","id":id,"result":result});
    McpHttpResponse::json(200, serde_json::to_vec(&body).expect("frame"))
}

fn discover_reply() -> McpHttpResponse {
    McpHttpResponse::json(
        200,
        br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false},"extensions":{"io.modelcontextprotocol/tasks":{}}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"remote","version":"1.0.0"}}}}"#,
    )
}

fn task(id: &str) -> Task {
    Task::new(
        id,
        TaskStatus::Working,
        "2026-11-05T10:00:00Z",
        "2026-11-05T10:00:00Z",
    )
    .with_poll_interval_ms(1500)
}

/// A client that discovers, then makes one operation against queued replies.
async fn connect(tasks: bool, replies: Vec<McpHttpResponse>) -> (McpClient, Arc<FakeHttp>) {
    let http = Arc::new(FakeHttp::new());
    http.push(discover_reply());
    for reply in replies {
        http.push(reply);
    }
    let options: McpClientOptions = options().with_tasks(tasks);
    let mut client = McpClient::connect_http(
        "remote".parse().expect("server name"),
        http_config(),
        options,
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
    )
    .expect("HTTP client");
    client.discover().await.expect("discover");
    (client, http)
}

fn body(http: &FakeHttp, index: usize) -> Value {
    serde_json::from_slice(&http.seen()[index].body).expect("request body")
}

fn header(http: &FakeHttp, index: usize, name: &str) -> Option<Vec<u8>> {
    http.seen()[index]
        .headers
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value.clone())
}

// Catches: ego declares the Tasks extension without the user's opt-in, or always.
#[tokio::test(flavor = "current_thread")]
async fn the_extension_is_declared_only_when_the_options_ask_for_it() {
    for (tasks, declared) in [(false, false), (true, true)] {
        let (_client, http) = connect(tasks, Vec::new()).await;
        let capabilities = &body(&http, 0)["params"]["_meta"];
        let text = capabilities.to_string();
        assert_eq!(
            text.contains("io.modelcontextprotocol/tasks"),
            declared,
            "{text}"
        );
    }
}

// Catches: a task answer is accepted from a session that never declared the
// extension, or is refused after the user opted in.
#[tokio::test(flavor = "current_thread")]
async fn a_task_answer_needs_the_declared_extension() {
    let created = || frame(1, CreateTaskResult::new(task("task-1")));
    let (mut client, _) = connect(true, vec![created()]).await;
    let outcome = client
        .call_tool_once(CallToolRequestParams::new("slow"))
        .await
        .expect("a declared session accepts a task");
    assert!(
        matches!(outcome, McpManualOutcome::TaskCreated(task) if task.task_id == "task-1"
            && task.poll_interval_ms == Some(1500))
    );
    let (mut client, _) = connect(false, vec![created()]).await;
    let refused = client
        .call_tool_once(CallToolRequestParams::new("slow"))
        .await;
    assert!(refused.is_err(), "an undeclared session refuses a task");
}

// Catches: a poll sends another method, task id or routing header than tasks/get for the id.
#[tokio::test(flavor = "current_thread")]
async fn a_poll_sends_tasks_get_for_the_recorded_id_and_maps_every_status() {
    let result = CallToolResult::success(vec![ContentBlock::text("sunny")]);
    let result = serde_json::to_value(&result).expect("result");
    let cases: Vec<(TaskPayload, McpTaskState)> = vec![
        (TaskPayload::Working, McpTaskState::Working),
        (
            TaskPayload::Completed {
                result: result.as_object().expect("object").clone(),
            },
            McpTaskState::Completed(Box::new(
                serde_json::from_value(result.clone()).expect("typed result"),
            )),
        ),
        (TaskPayload::Cancelled, McpTaskState::Cancelled),
    ];
    for (payload, expected) in cases {
        let reply = frame(
            1,
            GetTaskResult::new(DetailedTask::new(task("task-1"), payload)),
        );
        let (mut client, http) = connect(true, vec![reply]).await;
        let state = client.get_task_once("task-1").await.expect("poll");
        assert_eq!(state, expected);
        let request = body(&http, 1);
        assert_eq!(request["method"], "tasks/get");
        assert_eq!(request["params"]["taskId"], "task-1");
        // The guarded transport admits a selected request only with this metadata.
        let meta = &request["params"]["_meta"];
        assert!(
            meta["progressToken"].is_u64() || meta["progressToken"].is_string(),
            "{request}"
        );
        assert!(
            meta["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                ["io.modelcontextprotocol/tasks"]
                .is_object(),
            "{request}"
        );
        assert_eq!(
            header(&http, 1, "mcp-method").as_deref(),
            Some(&b"tasks/get"[..])
        );
    }
}

// Catches: a poll answered for another task id is taken as this task's result.
#[tokio::test(flavor = "current_thread")]
async fn a_poll_answered_for_another_task_is_refused() {
    let reply = frame(
        1,
        GetTaskResult::new(DetailedTask::new(task("task-2"), TaskPayload::Working)),
    );
    let (mut client, _) = connect(true, vec![reply]).await;
    assert!(client.get_task_once("task-1").await.is_err());
}

// Catches: a cancel sends another method or id, or the client polls without the declaration.
#[tokio::test(flavor = "current_thread")]
async fn a_cancel_sends_tasks_cancel_and_an_undeclared_client_sends_nothing() {
    let (mut client, http) = connect(true, vec![frame(1, TaskAckResult::new())]).await;
    client.cancel_task_once("task-1").await.expect("cancel");
    let request = body(&http, 1);
    assert_eq!(request["method"], "tasks/cancel");
    assert_eq!(request["params"]["taskId"], "task-1");

    let (mut client, http) = connect(false, Vec::new()).await;
    assert!(client.get_task_once("task-1").await.is_err());
    let (mut client2, _) = connect(false, Vec::new()).await;
    assert!(client2.cancel_task_once("task-1").await.is_err());
    assert_eq!(http.seen().len(), 1, "only discover reached the transport");
}
