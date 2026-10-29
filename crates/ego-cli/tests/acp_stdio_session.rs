//! Real ACP session and guarded child lifecycle.
#![cfg(unix)]
mod support;

use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text, tool_call};

// The owned test server uses the current-protocol responses already exercised
// by support::mcp_response and acp_mcp_channel. It records real PID/request effects.
const SERVER: &str = r#"#!/bin/sh
printf 'pid %s\n' "$$" >> "$1"
while IFS= read -r line; do
  case "$line" in *'"id"'*) ;; *) continue ;; esac
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *server/discover*)
      printf 'discover %s\n' "$$" >> "$1"
      result='{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/list*) result='{"resultType":"complete","tools":[{"name":"ping","description":"Return pong.","inputSchema":{"type":"object","additionalProperties":false}}],"nextCursor":null,"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/call*)
      printf 'call %s\n' "$$" >> "$1"
      if [ "$2" = crash ]; then exit 1; fi
      result='{"resultType":"complete","content":[{"type":"text","text":"pong"}],"isError":false}' ;;
    *) continue ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;

fn setup(
    places: &Places,
    stub: &Stub,
    behavior: &str,
    allowed: bool,
) -> (AcpHost, String, std::path::PathBuf, std::path::PathBuf) {
    std::fs::write(places.home_path().join("config.toml"), format!(
        "[permissions]\n{} = ['tool(fixture/ping)']\n[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n",
        if allowed { "allow" } else { "deny" },
    )).expect("isolated config");
    let log = places.workspace_path().join("stdio-events");
    let server = places.file("stdio-server.sh", SERVER);
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let opened = host.request("session/new", serde_json::json!({
        "cwd":places.workspace_path(), "mcpServers":[{"name":"fixture","command":"/bin/sh","args":[server,log,behavior],"env":[]}]
    }));
    let session = opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("admission failed: {opened}"))
        .to_owned();
    (host, session, log, server)
}

fn prompt(host: &mut AcpHost, session: &str) {
    let response = host.request(
        "session/prompt",
        serde_json::json!({"sessionId":session,"prompt":[{"type":"text","text":"Call ping"}]}),
    );
    assert!(response["result"].is_object(), "{response}");
}

fn plan() -> Plan {
    Plan {
        completions: vec![
            Reply::Sse(tool_call("first", "ping", "{}")),
            Reply::Sse(text("first done")),
            Reply::Sse(tool_call("second", "ping", "{}")),
            Reply::Sse(text("second done")),
        ],
        ..Plan::default()
    }
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("PID probe")
        .success()
}

/// Catches per-call spawn/discovery and a child surviving acknowledged session close.
#[test]
fn two_authorized_turns_reuse_one_stdio_pid_and_discovery_and_close_reaps_it() {
    let places = Places::new();
    let stub = Stub::start(plan());
    let (mut host, session, log, _) = setup(&places, &stub, "normal", true);
    for _ in 0..2 {
        prompt(&mut host, &session);
    }
    let events = std::fs::read_to_string(&log).expect("events");
    let pids: Vec<_> = events
        .lines()
        .filter_map(|line| line.strip_prefix("pid "))
        .collect();
    assert_eq!(pids.len(), 1, "per-call spawning: {events}");
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("discover "))
            .count(),
        1,
        "repeated discovery: {events}"
    );
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("call "))
            .count(),
        2,
        "missing permitted effect: {events}"
    );
    assert!(
        alive(pids[0]),
        "successful child died with the first prompt"
    );
    let closed = host.request("session/close", serde_json::json!({"sessionId":session}));
    assert!(closed["result"].is_object(), "{closed}");
    assert!(!alive(pids[0]), "close acknowledged before reap: {events}");
    host.finish();
    assert!(
        stub.generations()
            .iter()
            .filter(|generation| generation.to_string().contains("pong"))
            .count()
            >= 2,
        "provider never received the two real results"
    );
}

/// Catches a session child acting as authority for a later denied tool call.
#[test]
fn denied_stdio_call_sends_no_tool_request() {
    let places = Places::new();
    let stub = Stub::start(plan());
    let (mut host, session, log, _) = setup(&places, &stub, "normal", false);
    let response = host.request(
        "session/prompt",
        serde_json::json!({"sessionId":session,"prompt":[{"type":"text","text":"Call ping"}]}),
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("denied") && message.contains("fixture/ping")),
        "the configured denial must be surfaced: {response}"
    );
    host.finish();
    let events = std::fs::read_to_string(log).expect("events");
    assert!(
        !events.lines().any(|line| line.starts_with("call ")),
        "denied request crossed retained channel: {events}"
    );
}

/// Catches a changed executable/script identity being sent through the earlier child.
#[test]
fn executable_change_cannot_reuse_the_old_stdio_child() {
    let places = Places::new();
    let stub = Stub::start(plan());
    let (mut host, session, log, server) = setup(&places, &stub, "normal", true);
    prompt(&mut host, &session);
    std::fs::write(server, format!("{SERVER}\n# new executable identity\n"))
        .expect("change script");
    prompt(&mut host, &session);
    host.finish();
    let events = std::fs::read_to_string(log).expect("events");
    let pids: Vec<_> = events
        .lines()
        .filter_map(|line| line.strip_prefix("pid "))
        .collect();
    let calls: Vec<_> = events
        .lines()
        .filter_map(|line| line.strip_prefix("call "))
        .collect();
    assert_eq!(
        calls.first(),
        pids.first(),
        "first admitted call missing: {events}"
    );
    assert!(
        calls.len() == 1
            || (pids.len() == 2 && calls.len() == 2 && calls[1] == pids[1] && pids[0] != pids[1]),
        "stale executable received a later call: {events}"
    );
}

/// Catches automatic replay of a possibly-sent call after the child crashes.
#[test]
fn crashed_stdio_call_is_not_replayed_and_child_is_reaped() {
    let places = Places::new();
    let stub = Stub::start(plan());
    let (mut host, session, log, _) = setup(&places, &stub, "crash", true);
    prompt(&mut host, &session);
    let events = std::fs::read_to_string(log).expect("events");
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("pid "))
            .count(),
        1,
        "automatic restart: {events}"
    );
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("call "))
            .count(),
        1,
        "uncertain call was replayed: {events}"
    );
    let pid = events
        .lines()
        .find_map(|line| line.strip_prefix("pid "))
        .expect("PID");
    assert!(!alive(pid), "failed child was not reaped");
    host.finish();
}
