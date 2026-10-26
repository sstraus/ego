#![cfg(unix)]

mod support;

use serde_json::Value;
use support::{Ego, Finished, Plan, Reply, Stub, text};

const SERVER: &str = r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    if request["method"] == "server/discover":
        result = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {"listChanged": False}},
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "auto", "version": "1"}},
        }
    else:
        result = {
            "resultType": "complete",
            "tools": [
                {
                    "name": "search_tools",
                    "description": "Find a tool",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}},
                        "required": ["query"],
                        "additionalProperties": False,
                    },
                }
            ],
            "nextCursor": None,
            "ttlMs": 0,
            "cacheScope": "private",
        }
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

fn offers_tool(generation: &Value, name: &str) -> bool {
    generation["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == name))
}

/// Run one prompt with a user-configured stdio server `auto`.
fn run_with_configured_server(enabled: &str, extra: &[&str]) -> (Finished, Vec<Value>) {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("AUTO-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let server = ego.file("auto-server.py", SERVER);
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.auto]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{:?}]\n{enabled}",
            server.display().to_string()
        ),
    );
    let finished = extra
        .iter()
        .fold(ego, |ego, arg| ego.arg(arg))
        .arg("hello")
        .run();
    (finished, stub.generations())
}

// Catches: attachment still gated on --mcp.
#[test]
fn configured_server_is_attached_without_the_mcp_flag() {
    let (finished, generations) = run_with_configured_server("", &[]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        generations
            .first()
            .is_some_and(|first| offers_tool(first, "search_tools")),
        "the configured server's tools were not offered: {generations:?}; {finished:?}"
    );
    assert!(
        offers_tool(&generations[0], "web_fetch"),
        "native web retrieval must coexist with discovered MCP tools: {generations:?}"
    );
    assert!(
        !finished.stderr.contains("was not connected"),
        "a healthy server drew a notice: {finished:?}"
    );
}

// Catches: the enabled flag ignored, or --mcp losing its override role.
#[test]
fn disabled_server_stays_detached_until_selected_explicitly() {
    let (finished, generations) = run_with_configured_server("enabled=false\n", &[]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        generations
            .first()
            .is_some_and(|first| !offers_tool(first, "search_tools")),
        "a disabled server was attached: {generations:?}"
    );
    let (finished, generations) = run_with_configured_server("enabled=false\n", &["--mcp", "auto"]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        generations
            .first()
            .is_some_and(|first| offers_tool(first, "search_tools")),
        "--mcp did not attach the disabled server: {generations:?}; {finished:?}"
    );
}

// Catches: --mcp NAME naming an enabled server attaching it twice.
#[test]
fn explicit_selection_of_an_enabled_server_attaches_it_once() {
    let (finished, generations) = run_with_configured_server("", &["--mcp", "auto"]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    let offered = generations[0]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter(|tool| tool["function"]["name"] == "search_tools")
        .count();
    assert_eq!(offered, 1, "{generations:?}");
}

fn run_with_remote_server(extra: &[&str]) -> Finished {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("REMOTE-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--model", "ollama/qwen3", "--sandbox", "workspace"]);
    ego.home_file(
        "config.toml",
        "[mcp.servers.remote]\ntransport='http'\nurl='https://mcp.example.test/v1'\n",
    );
    extra
        .iter()
        .fold(ego, |ego, arg| ego.arg(arg))
        .arg("hello")
        .run()
}

// Catches: one server the sandbox refuses aborts the session, or the refusal is
// silent, or admission is skipped for automatic attachment.
#[test]
fn refused_configured_server_is_reported_once_and_the_session_continues() {
    let finished = run_with_remote_server(&[]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(finished.stdout.contains("REMOTE-COMPLETE"), "{finished:?}");
    let notice =
        "MCP server 'remote' was not connected: MCP HTTP server `remote` requires --sandbox off";
    assert_eq!(finished.stderr.matches(notice).count(), 1, "{finished:?}");
}

// Catches: explicit selection losing its hard failure when admission refuses.
#[test]
fn refused_explicit_selection_stays_a_usage_error() {
    let finished = run_with_remote_server(&["--mcp", "remote"]);
    assert_ne!(finished.code, Some(0), "{finished:?}");
    assert!(
        finished.stderr.contains("requires --sandbox off"),
        "{finished:?}"
    );
}

// Catches: a server that cannot start stopping the session, or going unreported.
#[test]
fn configured_server_that_fails_to_start_is_reported_once_and_the_session_continues() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("BROKEN-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let missing = ego.workspace_path().join("missing-server");
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.missing]\ntransport='stdio'\ncommand={:?}\n",
            missing
        ),
    );
    let finished = ego.arg("hello").run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(finished.stdout.contains("BROKEN-COMPLETE"), "{finished:?}");
    assert!(!finished.stderr.contains("configured but not attached"));
    assert_eq!(
        finished.stderr.matches("MCP server 'missing'").count(),
        1,
        "{finished:?}"
    );
    assert!(
        !finished.stderr.contains(&missing.display().to_string()),
        "unsafe raw configuration diagnostic: {finished:?}"
    );
}
