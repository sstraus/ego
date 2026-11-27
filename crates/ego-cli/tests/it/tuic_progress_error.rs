#![cfg(unix)]

use crate::support;

use support::{Ego, Plan, Reply, Stub, text, tool_call};

// TUIC answers a refused `progress` call with `isError: true` and the error
// body (mcp_transport.rs tool_result_is_error, format_tool_call_result). The
// body is the one in the b-019 transcript (2026-11-13, ego 0.4.11).
const SERVER: &str = r#"
import json
import sys

progress = {
    "name": "progress",
    "description": "Report progress",
    "inputSchema": {
        "type": "object",
        "properties": {"type": {"type": "string"}, "text": {"type": "string"}},
        "required": ["type", "text"],
        "additionalProperties": False,
    },
}

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "server/discover":
        result = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {"listChanged": False}},
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "tuic", "version": "1"}},
        }
    elif method == "tools/list":
        result = {
            "resultType": "complete",
            "tools": [progress],
            "nextCursor": None,
            "ttlMs": 0,
            "cacheScope": "private",
        }
    elif method == "tools/call":
        assert request["params"]["name"] == "progress"
        assert request["params"]["arguments"]["type"] == "blocked"
        body = {"error": "project_required: register an authoritative project before reporting progress"}
        result = {
            "resultType": "complete",
            "content": [{"type": "text", "text": json.dumps(body, separators=(",", ":"))}],
            "isError": True,
        }
    else:
        raise AssertionError(method)
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

/// Catches: a TUIC error answer to a blocked progress report counts as a user
/// wait, so the turn ends Completed with no final message (story 1100-e872).
#[test]
fn refused_blocked_progress_returns_the_error_to_the_model() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "blocked-report",
                "progress",
                r#"{"type":"blocked","text":"The new token is rejected."}"#,
            )),
            Reply::Sse(text("FINAL-AFTER-REFUSED-PROGRESS")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
        "--output",
        "jsonl",
    ]);
    let server = ego.file("tuic-server.py", SERVER);
    let selector = ego.file(
        "mcp.json",
        &serde_json::json!({
            "name": "tuic",
            "transport": "stdio",
            "command": "/usr/bin/python3",
            "args": [server],
            "inherit_env": []
        })
        .to_string(),
    );
    let finished = ego
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .arg("Rotate the token and report")
        .run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        2,
        "the refused report must return to the model: {finished:?}"
    );
    let tool_message = generations[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "blocked-report")
        .expect("progress result")
        .clone();
    assert!(
        tool_message["content"]
            .as_str()
            .is_some_and(|content| content.contains("project_required")),
        "{tool_message:?}"
    );
    assert!(
        finished.stdout.contains("FINAL-AFTER-REFUSED-PROGRESS"),
        "{finished:?}"
    );
    assert!(
        finished
            .stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|event| event["type"] == "item_completed"
                && event["kind"] == "tool"
                && event["ok"] == false),
        "the error result must be journaled as a failed call: {finished:?}"
    );
}
