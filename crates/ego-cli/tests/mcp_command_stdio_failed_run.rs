//! A run that fails after a stdio MCP call still reaps that call's child (regression case).
#![cfg(unix)]

mod support;

use support::{Ego, Plan, Reply, Stub, tool_call};

// After stdin closes the server lingers, so only an explicit kill ends it.
const SERVER: &str = r#"#!/bin/sh
printf 'pid %s\n' "$$" >> "$1"
while IFS= read -r line; do
  case "$line" in *'"id"'*) ;; *) continue ;; esac
  id=${line#*\"id\":}
  id=${id%%,*}
  case "$line" in
    *server/discover*) result='{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/list*) result='{"resultType":"complete","tools":[{"name":"ping","description":"Ping.","inputSchema":{"type":"object","additionalProperties":false}}],"nextCursor":null,"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/call*) result='{"resultType":"complete","content":[{"type":"text","text":"pong"}],"isError":false}' ;;
    *) continue ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
sleep 30
"#;

/// Catches reaping the retained child only on the success path of the run.
#[test]
fn stdio_child_is_reaped_when_the_run_fails_after_a_call() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("first", "ping", "{}")),
            Reply::Status(400),
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
    ego.file("AGENTS.md", "# MCP failed run fixture\n");
    let log = ego.home_path().join("stdio-events");
    let server = ego.file("mcp-server.sh", SERVER);
    let config = serde_json::json!({
        "name": "fixture",
        "transport": "stdio",
        "command": "/bin/sh",
        "args": [server.to_string_lossy(), log.to_string_lossy()],
        "inherit_env": []
    });
    let selector = ego.file("mcp.json", &config.to_string());
    let finished = ego
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .run_pty("ping then fail");
    assert!(
        finished.stdout.contains("HTTP 400"),
        "the provider failure never surfaced: {}",
        finished.stdout
    );
    let events = std::fs::read_to_string(&log).expect("events");
    let pids: Vec<_> = events
        .lines()
        .filter_map(|line| line.strip_prefix("pid "))
        .collect();
    assert!(!pids.is_empty(), "the server never started: {events}");
    for pid in pids {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("PID probe")
            .success();
        assert!(!alive, "child {pid} outlived the failed run: {events}");
    }
}
