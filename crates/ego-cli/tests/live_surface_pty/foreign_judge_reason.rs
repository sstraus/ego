use super::*;

const JUDGE_REASON: &str = "the ping tool is outside the task and its effects are unknown";

/// A stdio MCP server named `fixture` that serves one `ping` tool.
const SERVER: &str = r#"#!/bin/sh
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
"#;

// Catches: a foreign tool prompt that says unknown risk with no reason, while
// the judge's escalation reason stays in the session log only.
#[test]
fn a_judge_escalated_foreign_tool_prompt_shows_the_judge_reason() {
    let verdict = serde_json::json!({"verdict": "escalate", "reason": JUDGE_REASON}).to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("ping-call", "ping", "{}")),
            Reply::Sse(text(&verdict)),
            Reply::Sse(text(TOOL_TURN_DONE)),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing();
    ego.home_file("config.toml", "[permissions]\njudge = \"ollama/qwen3\"\n");
    let server = ego.file("mcp-server.sh", SERVER);
    let selector = ego.file(
        "mcp.json",
        &serde_json::json!({
            "name": "fixture",
            "transport": "stdio",
            "command": "/bin/sh",
            "args": [server.to_string_lossy()],
            "inherit_env": []
        })
        .to_string(),
    );
    let ego = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--mode",
            "auto",
            "--sandbox",
            "off",
        ])
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .env("TERM", "xterm-256color");
    let mut session = modal_session_on(ego, stub);
    session.press(b"call ping\r");
    session.wait("[d] details");

    let screen = session.screen();
    let first = screen
        .iter()
        .rposition(|line| line.starts_with("Allow "))
        .unwrap_or_else(|| panic!("no headline row: {screen:#?}"));
    let modal = screen[first..].join("\n");
    assert!(
        modal.contains(&format!("Judge: {JUDGE_REASON}")),
        "the judge reason is not on the prompt:\n{modal}"
    );
    assert!(
        screen[first].contains("fixture") && screen[first].contains("ping"),
        "the headline must name the server and the tool:\n{modal}"
    );
    assert!(modal.contains("Risk: unknown - "), "{modal}");

    session.press(b"y");
    session.wait(TOOL_TURN_DONE);
    let (generations, _) = session.finish();
    assert_eq!(generations.len(), 3, "main, judge, main after the call");
}
