//! contract for the inbound MCP `compact` tool.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use ego_core::ceiling::{Budget, Ceilings};
use ego_core::config::{Mode, ModelRef};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::{CleanupCertainty, DuplexEnd, DuplexSession, DuplexSettlement, ToolResult};
use ego_mcp::{
    McpRunSelectionPolicy, McpRunServerConfig, McpRunServerFactory, McpServerPort,
    McpServerToolCall, serve_stdio,
};
use futures::io::{AsyncRead, AsyncWrite, Cursor};
use serde_json::{Value, json};

#[path = "support/run_controller.rs"]
mod run_fixture;

fn server() -> impl McpServerPort {
    server_with_fixtures().0
}

fn server_with_fixtures() -> (
    impl McpServerPort,
    Arc<run_fixture::ExecutorFixture>,
    Arc<run_fixture::ExecutionSpawner>,
) {
    let mut ceilings = Ceilings::default();
    ceilings.apply(&"100k".parse::<Budget>().expect("token ceiling"));
    let policy = McpRunSelectionPolicy::new(
        BTreeSet::from([run_fixture::root()]),
        BTreeSet::from([ModelRef("model/allowed".to_owned())]),
        ThinkingLevel::High,
        BTreeSet::from([Mode::Default]),
        ceilings,
        BTreeSet::from(["ci".to_owned()]),
    )
    .expect("selection policy");
    let config = Arc::new(
        McpRunServerConfig::new(
            policy,
            NonZeroUsize::new(2).expect("concurrency"),
            NonZeroUsize::new(4096).expect("resource bound"),
        )
        .expect("MCP server config"),
    );
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let (ports, spawner) = run_fixture::ports_with_spawner();
    (
        McpRunServerFactory::new(controller, config, ports).new_connection(),
        executor,
        spawner,
    )
}

fn call_named(name: &str, arguments: Value) -> McpServerToolCall {
    McpServerToolCall::new(
        name.to_owned(),
        arguments.as_object().expect("compact object").clone(),
        Some("compact-NHC".to_owned()),
    )
    .expect("compact call envelope")
}

fn call(arguments: Value) -> McpServerToolCall {
    call_named("compact", arguments)
}

struct WireDuplex {
    input: Cursor<Vec<u8>>,
    output: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl AsyncRead for WireDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.input).poll_read(cx, bytes)
    }
}

impl AsyncWrite for WireDuplex {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.output
            .lock()
            .expect("wire output")
            .extend_from_slice(bytes);
        std::task::Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl DuplexSession for WireDuplex {
    fn delivery(&self) -> ego_core::generator::attempt::DeliveryCertainty {
        ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent
    }

    fn settle(self: Box<Self>) -> futures::future::BoxFuture<'static, DuplexSettlement> {
        Box::pin(async {
            DuplexSettlement::new(
                DuplexEnd::Exited { code: 0 },
                CleanupCertainty::Complete,
                ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent,
                Ok(ToolResult::builtin("settled")),
            )
        })
    }

    fn interrupt(self: Box<Self>) -> futures::future::BoxFuture<'static, DuplexSettlement> {
        Box::pin(async {
            DuplexSettlement::new(
                DuplexEnd::Interrupted,
                CleanupCertainty::Complete,
                ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent,
                Ok(ToolResult::builtin("interrupted")),
            )
        })
    }
}

async fn wire(requests: &[Value]) -> Vec<Value> {
    let input = requests
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let output = Arc::new(std::sync::Mutex::new(Vec::new()));
    let duplex = WireDuplex {
        input: Cursor::new(format!("{input}\n").into_bytes()),
        output: Arc::clone(&output),
    };
    let _settlement = serve_stdio(Arc::new(server()), Box::new(duplex))
        .await
        .expect("MCP server settlement");
    output
        .lock()
        .expect("wire output")
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("MCP wire response JSON"))
        .collect()
}

#[test]
fn the_fifth_tool_is_compact_and_exposes_the_exact_input_schema() {
    let tools = futures::executor::block_on(server().list_tools());
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["run", "continue", "cancel", "status", "compact"]
    );
    let compact = tools
        .iter()
        .find(|tool| tool.name == "compact")
        .expect("compact descriptor");
    assert_eq!(compact.parameters["additionalProperties"], false);
    assert_eq!(
        compact.parameters["required"],
        json!(["request_id", "run_id", "expected_revision"])
    );
}

#[test]
fn compact_rejects_unknown_fields_as_protocol_input_errors() {
    let result = futures::executor::block_on(server().call_tool(call(json!({
        "request_id": "00000000-0000-0000-0000-000000000068",
        "run_id": "00000000-0000-0000-0000-000000000069",
        "expected_revision": 1,
        "prompt": "not accepted",
    }))));
    assert!(result.is_err());
}

#[test]
fn compact_application_failures_are_tool_results_not_protocol_decode_errors() {
    let result = futures::executor::block_on(server().call_tool(call(json!({
        "request_id": "00000000-0000-0000-0000-000000000068",
        "run_id": "00000000-0000-0000-0000-000000000068.00000000-0000-0000-0000-000000000069",
        "expected_revision": 1,
    }))));
    let result = result.expect("application failure remains a tool result");
    let value: Value = serde_json::from_str(&result.model_text()).expect("compact error JSON");
    assert_eq!(value["isError"], true);
}

#[tokio::test]
async fn compact_application_failures_use_the_real_mcp_is_error_envelope() {
    let responses = wire(&[
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "fixture", "version": "1"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "compact",
                "arguments": {
                    "request_id": "00000000-0000-0000-0000-000000000068",
                    "run_id": "00000000-0000-0000-0000-000000000068.00000000-0000-0000-0000-000000000069",
                    "expected_revision": 1
                }
            }
        }),
    ])
    .await;
    let response = responses
        .iter()
        .find(|response| response["id"] == 2)
        .expect("compact wire response");
    assert!(response["result"].is_object(), "MCP response: {response}");
    assert_eq!(response["result"]["isError"], true);
}

#[test]
fn status_projects_context_overflow_after_the_real_settlement() {
    let (server, executor, spawner) = server_with_fixtures();
    executor.push_settlement(ego_core::run::RunSettlement::ContextOverflow);
    let run = futures::executor::block_on(server.call_tool(call_named(
        "run",
        json!({
            "request_id": "00000000-0000-0000-0000-000000000068",
            "root": run_fixture::ROOT,
            "prompt": "context overflow fixture"
        }),
    )))
    .expect("run admission");
    let run_value: Value = serde_json::from_str(&run.model_text()).expect("run snapshot JSON");
    let run_id = run_value["run_id"].as_str().expect("run id").to_owned();
    executor.allow_effects();
    for pending in spawner.take_pending() {
        futures::executor::block_on(pending);
    }
    let status = futures::executor::block_on(
        server.call_tool(call_named("status", json!({"run_id": run_id}))),
    )
    .expect("settled status");
    let status: Value = serde_json::from_str(&status.model_text()).expect("status JSON");
    assert_eq!(status["status"], "settled");
    assert_eq!(status["settlement"]["outcome"], "context_overflow");
}

#[test]
fn compact_result_carries_source_target_and_successor_identity() {
    let result = json!({
        "v": 1,
        "source_session_id": "00000000-0000-0000-0000-000000000068",
        "source_seq": 42,
        "target_session_id": "00000000-0000-0000-0000-000000000069",
        "publication": {"kind": "published_durably"},
        "successor_start_request_id": "00000000-0000-0000-0000-00000000006a"
    });
    for field in [
        "source_session_id",
        "source_seq",
        "target_session_id",
        "publication",
        "successor_start_request_id",
    ] {
        assert!(result.get(field).is_some(), "missing {field}");
    }
}

#[test]
fn published_durability_uncertain_is_successful_and_non_retryable() {
    let publication = json!({
        "kind": "published_durability_uncertain",
        "diagnostic": "parent barrier failed",
    });
    assert_eq!(publication["kind"], "published_durability_uncertain");
    assert!(publication["diagnostic"].is_string());
}
