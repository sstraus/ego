#![cfg(unix)]

mod support;

use support::{Ego, Plan, Reply, Stub, text, tool_call};

#[test]
fn permitted_current_http_mcp_tool_reaches_server_in_pty() {
    let stub = Stub::start_mcp_tool(Plan {
        completions: vec![
            Reply::Sse(tool_call("search-call", "search_tools", "{}")),
            Reply::Sse(text("HTTP-SEARCH-COMPLETE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# HTTP MCP PTY fixture\n");
    let config = serde_json::json!({
        "name": "fixture",
        "transport": "http",
        "url": format!("http://{}/mcp", stub.address),
        "auth": "none",
        "allow_insecure_loopback": true
    });
    let selector = ego.file("mcp.json", &config.to_string());

    let finished = ego
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .run_pty("search tools");

    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    assert!(
        finished.stdout.contains("HTTP-SEARCH-COMPLETE"),
        "{}",
        finished.stdout
    );
    let requests = stub.requests();
    let calls = requests
        .iter()
        .filter(|request| request.path == "/mcp")
        .filter(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body["method"] == "tools/call")
        })
        .count();
    assert_eq!(
        calls, 1,
        "the selected HTTP request was not sent once; terminal: {}; requests: {requests:?}",
        finished.stdout
    );
    assert!(
        stub.generations()
            .iter()
            .any(|generation| generation.to_string().contains("lookup-complete")),
        "the provider did not receive the HTTP server's result"
    );
}
