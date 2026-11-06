#![cfg(unix)]

use crate::support;

use std::fs::{self, OpenOptions};

use support::{Ego, Plan, Reply, Stub, text, tool_call};

#[test]
fn complete_stdio_mcp_call_finishes_in_pty_while_resume_key_is_locked() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("ping-call", "ping", "{}")),
            Reply::Sse(text("PING-COMPLETE")),
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
    ego.file("AGENTS.md", "# MCP PTY fixture\n");
    let server = ego.file(
        "mcp-server.sh",
        r#"#!/bin/sh
while IFS= read -r line; do
  id=${line#*\"id\":}
  id=${id%%,*}
  case "$line" in
    *server/discover*) result='{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"pty-ping","version":"1"}}}' ;;
    *tools/list*) result='{"resultType":"complete","tools":[{"name":"ping","description":"Return a fixed pong token.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}],"nextCursor":null,"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/call*) result='{"resultType":"complete","content":[{"type":"text","text":"pong"}],"isError":false}' ;;
    *) continue ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#,
    );
    let config = serde_json::json!({
        "name": "fixture",
        "transport": "stdio",
        "command": "/bin/sh",
        "args": [server.to_string_lossy()],
        "inherit_env": []
    });
    let selector = ego.file("mcp.json", &config.to_string());
    let lock_path = ego.home_path().join("mcp/mcp-resume-key.lock");
    fs::create_dir_all(lock_path.parent().expect("lock parent")).expect("create MCP home");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .expect("resume lock file");
    let mut lock = fd_lock::RwLock::new(file);
    let guard = lock
        .write()
        .expect("hold resume key lock in another process");

    let finished = ego
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .run_pty("call ping");
    drop(guard);

    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    assert!(
        finished.stdout.contains("PING-COMPLETE"),
        "{}",
        finished.stdout
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 2, "{generations:?}");
    assert!(
        generations[1].to_string().contains("pong"),
        "the provider did not receive the server's fixed result: {}",
        generations[1]
    );
}
