//! A command run keeps one stdio MCP child across tool calls (regression).
#![cfg(unix)]

use crate::support;

use support::{Ego, Plan, Reply, Stub, text, tool_call};

// Like tuic-bridge, the server binds an identity to its own process: `register`
// sets it, `whoami` answers from it. A fresh child per call loses the binding.
const SERVER: &str = r#"#!/bin/sh
printf 'pid %s\n' "$$" >> "$1"
bound=no
while IFS= read -r line; do
  case "$line" in *'"id"'*) ;; *) continue ;; esac
  id=${line#*\"id\":}
  id=${id%%,*}
  case "$line" in
    *server/discover*) result='{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/list*) result='{"resultType":"complete","tools":[{"name":"register","description":"Bind the caller.","inputSchema":{"type":"object","additionalProperties":false}},{"name":"whoami","description":"Report the binding.","inputSchema":{"type":"object","additionalProperties":false}}],"nextCursor":null,"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/call*)
      case "$line" in
        *'"register"'*) bound=yes; msg=registered ;;
        *) if [ "$bound" = yes ]; then msg=BOUND; else msg=UNBOUND; fi ;;
      esac
      result='{"resultType":"complete","content":[{"type":"text","text":"'$msg'"}],"isError":false}' ;;
    *) continue ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;

/// Catches a fresh stdio child per tool call, which drops a binding made by the previous call.
#[test]
fn consecutive_stdio_calls_in_one_run_share_one_server_process() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("first", "register", "{}")),
            Reply::Sse(tool_call("second", "whoami", "{}")),
            Reply::Sse(text("DONE")),
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
    ego.file("AGENTS.md", "# MCP binding fixture\n");
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
        .run_pty("register then whoami");
    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    let generations = stub.generations();
    let last = generations
        .last()
        .expect("provider generations")
        .to_string();
    assert!(
        last.contains("BOUND") && !last.contains("UNBOUND"),
        "binding lost across calls (children: {:?}): {last}",
        std::fs::read_to_string(&log)
    );
    // Catches a retained child outliving the run that owns it.
    let events = std::fs::read_to_string(&log).expect("events");
    for pid in events.lines().filter_map(|line| line.strip_prefix("pid ")) {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("PID probe")
            .success();
        assert!(!alive, "child {pid} outlived the run: {events}");
    }
}
