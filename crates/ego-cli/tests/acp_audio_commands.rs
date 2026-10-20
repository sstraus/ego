//! Real public dispatch must not drop audio on text-only custom commands.
#![cfg(unix)]
mod support;
use serde_json::json;
use support::{AcpHost, Ego};

const FACTS: &str = r#"
[model."openai/gpt-audio-1.5".capabilities]
schema_version = 6
max_context_tokens = 128000
max_output_tokens = 16384
supports_parallel_tool_calls = false
reasoning_carryover = "unsupported"
reasoning_efforts = { only = ["off"] }
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "none"
transport = "sse"
image_input = "unsupported"
audio_input = "supported_wav"
"#;
// Owned local server, not a recorded external response. Prompt execution always
// errors, so the pre-fix RED cannot proceed to any model generation.
const SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in *'"id"'*) ;; *) continue ;; esac
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *server/discover*) result='{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"prompts":{"listChanged":false}},"ttlMs":0,"cacheScope":"private"}' ;;
    *prompts/list*) result='{"resultType":"complete","prompts":[{"name":"review","description":"Review fixture","arguments":[]}],"nextCursor":null,"ttlMs":0,"cacheScope":"private"}' ;;
    *prompts/get*) printf called > "$1"; printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"fixture stops before generation"}}\n' "$id"; continue ;;
    *) continue ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;

fn configured() -> Ego {
    let ego = Ego::nowhere();
    ego.home_file("models.toml", FACTS);
    ego.home_file(
        "config.toml",
        "model = 'openai/gpt-audio-1.5'\nthinking = 'off'\nsandbox = 'off'\nmode = 'yolo'\n",
    );
    ego
}
fn host(ego: &Ego) -> AcpHost {
    AcpHost::adopt(
        Ego::at(ego.home_path(), ego.workspace_path(), None)
            .subcommand("acp")
            .env("OPENAI_API_KEY", "invalid-fixture-key")
            .spawn_conversing(),
    )
}
fn audio() -> String {
    "UklGRmQBAABXQVZFZm10IBAAAAABAAEAgD4AAAB9AAACABAAZGF0YUABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==".to_owned()
}

fn prompt(host: &mut AcpHost, session: &str, command: &str) -> serde_json::Value {
    host.request(
        "session/prompt",
        json!({"sessionId":session,"prompt":[
            {"type":"text","text":command}, {"type":"audio","mimeType":"audio/wav","data":audio()}
        ]}),
    )
}

// Catches custom skill dispatch accepting audio and executing without the clip.
#[test]
fn custom_skill_audio_refuses_before_command_execution() {
    let ego = configured();
    ego.home_file(
        "commands/review.md",
        "Review !`touch audio-command-executed; exit 7`",
    );
    let mut host = host(&ego);
    host.initialize();
    let session = host.new_session(ego.workspace_path().to_str().unwrap());
    let response = prompt(&mut host, &session, "/review");
    let stderr = host.finish();
    assert!(
        response
            .to_string()
            .contains("custom commands do not accept"),
        "{response} {stderr}"
    );
    assert!(response.to_string().contains("audio"), "{response}");
    assert!(!ego.workspace_path().join("audio-command-executed").exists());
}

// Catches MCP context dispatch accepting audio and fetching only the text prompt.
#[test]
fn mcp_context_audio_refuses_before_prompt_execution() {
    let ego = configured();
    let script = ego.file("prompt-server.sh", SERVER);
    let marker = ego.workspace_path().join("audio-mcp-executed");
    let mut host = host(&ego);
    host.initialize();
    let opened = host.request(
        "session/new",
        json!({"cwd":ego.workspace_path(),"mcpServers":[
            {"name":"fixture","command":"/bin/sh","args":[script,marker],"env":[]}
        ]}),
    );
    let session = opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"))
        .to_owned();
    let response = prompt(&mut host, &session, "/mcp__fixture__review");
    let stderr = host.finish();
    assert!(
        response
            .to_string()
            .contains("custom commands do not accept"),
        "{response} {stderr}"
    );
    assert!(response.to_string().contains("audio"), "{response}");
    assert!(!marker.exists());
}
