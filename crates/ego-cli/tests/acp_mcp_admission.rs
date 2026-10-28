//! ACP MCP admission through the packaged binary and a real stdio child.

mod support;

use support::{AcpHost, Ego, Places};

/// Catches: reversing the admission backoff comparison restarts a failed child immediately.
#[test]
fn repeated_failed_acp_admission_does_not_restart_the_same_stdio_server_immediately() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n",
    )
    .expect("isolated profile configuration");
    let starts = places.workspace_path().join("mcp-starts.txt");
    let server = places.file(
        "failing-mcp.sh",
        "IFS= read -r request || exit 1\nprintf '%s\\n' \"$request\" >> \"$1\"\nexit 1\n",
    );
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let load = || {
        serde_json::json!({
            "sessionId": session,
            "cwd": places.workspace_path(),
            "mcpServers": [{
                "name": "failing",
                "command": "/bin/sh",
                "args": [server, starts],
                "env": []
            }]
        })
    };
    let first = host.request("session/load", load());
    let second = host.request("session/load", load());
    assert!(first["error"].is_object(), "{first}");
    assert!(second["error"].is_object(), "{second}");
    assert!(
        second["error"]["data"]
            .as_str()
            .is_some_and(|message| message.contains("retry")),
        "the refused retry must be explained to the host: {second}"
    );
    for refusal in [&first, &second] {
        assert!(
            refusal["error"]["data"]
                .as_str()
                .is_some_and(|message| message.contains("MCP server 'failing'")),
            "the refusal must name the server that failed: {refusal}"
        );
    }
    host.finish();
    let requests = std::fs::read_to_string(starts).expect("the fake server started");
    assert_eq!(
        requests.lines().count(),
        1,
        "identical failed loads restarted the stdio server: {requests}"
    );
    assert!(
        requests.contains("\"method\":\"server/discover\""),
        "{requests}"
    );
}

#[test]
fn changed_acp_mcp_selection_can_retry_after_a_different_selection_failed() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n",
    )
    .expect("isolated profile configuration");
    let starts = places.workspace_path().join("mcp-starts.txt");
    let server = places.file(
        "failing-mcp.sh",
        "IFS= read -r request || exit 1\nprintf '%s\n' \"$request\" >> \"$1\"\nexit 1\n",
    );
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    for version in ["original", "changed"] {
        let response = host.request(
            "session/load",
            serde_json::json!({
                "sessionId": session,
                "cwd": places.workspace_path(),
                "mcpServers": [{
                    "name": "failing",
                    "command": "/bin/sh",
                    "args": [server, starts, version],
                    "env": []
                }]
            }),
        );
        assert!(response["error"].is_object(), "{response}");
    }
    host.finish();
    let requests = std::fs::read_to_string(starts).expect("both server versions started");
    assert_eq!(
        requests.lines().count(),
        2,
        "a changed server selection must get its own discovery: {requests}"
    );
}

#[test]
fn concurrent_identical_acp_loads_each_admit_their_own_session() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n",
    )
    .expect("isolated profile configuration");
    let starts = places.workspace_path().join("mcp-starts.txt");
    let server = places.file(
        "working-mcp.py",
        r#"import json, os, sys, time
with open(sys.argv[1], "a", encoding="utf-8") as starts:
    starts.write(f"{os.getpid()}\n")
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    if request["method"] == "server/discover":
        time.sleep(3)
        result = {"resultType": "complete", "supportedVersions": ["2026-07-28"], "capabilities": {}, "ttlMs": 0, "cacheScope": "private"}
    else:
        result = {"resultType": "complete", "tools": [], "resources": [], "prompts": [], "nextCursor": None, "ttlMs": 0, "cacheScope": "private"}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#,
    );
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let sessions = [host.new_session(cwd), host.new_session(cwd)];
    let ids = sessions.map(|session| {
        host.send_request(
            "session/load",
            serde_json::json!({
                "sessionId": session,
                "cwd": cwd,
                "mcpServers": [{
                    "name": "working",
                    "command": "/usr/bin/python3",
                    "args": [server, starts],
                    "env": []
                }]
            }),
        )
    });
    let mut replies = std::collections::HashMap::new();
    while replies.len() < ids.len() {
        let frame = host.read_frame();
        if let Some(id) = frame["id"].as_u64() {
            replies.insert(id, frame);
        }
    }
    for id in ids {
        let reply = &replies[&id];
        assert!(reply["result"].is_object(), "both tabs must load: {reply}");
    }
    host.finish();
    let starts = std::fs::read_to_string(starts).expect("working server starts recorded");
    assert_eq!(starts.lines().count(), 2, "one child per session: {starts}");
}

/// Catches: stdio discovery that ignores the session's sandbox pick and so
/// lets a supplied server write into the workspace of a read-only session.
/// The server answers discovery, so the load only succeeds if it really ran
/// under the sandbox; a server that never started fails the first assertion
/// instead of passing the second vacuously.
#[test]
fn stdio_discovery_runs_under_the_session_sandbox() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'ro'\nmode = 'default'\n",
    )
    .expect("isolated profile configuration");
    let marker = places.workspace_path().join("discovery-wrote-here.txt");
    let server = places.file(
        "answering-mcp.sh",
        r#"printf 'x' > "$1" 2>/dev/null
while IFS= read -r line; do
  case "$line" in *'"id"'*) ;; *) continue ;; esac
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *server/discover*) result='{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"private"}' ;;
    *) result='{"resultType":"complete","tools":[],"resources":[],"prompts":[],"nextCursor":null,"ttlMs":0,"cacheScope":"private"}' ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#,
    );
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let response = host.request(
        "session/load",
        serde_json::json!({
            "sessionId": session,
            "cwd": places.workspace_path(),
            "mcpServers": [{
                "name": "answering",
                "command": "/bin/sh",
                "args": [server, marker],
                "env": []
            }]
        }),
    );
    host.finish();
    assert!(
        response["result"].is_object(),
        "the server never ran and answered discovery under the sandbox: {response}"
    );
    assert!(
        !marker.exists(),
        "the server wrote into a read-only session's workspace during discovery"
    );
}

/// Catches: an unknown MCP transport dropped by the protocol library and
/// admitted as an empty selection, so the real admitter opens a session the
/// host believes carries a server.
#[test]
fn unknown_mcp_transport_is_refused_before_a_session_exists() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n",
    )
    .expect("isolated profile configuration");
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let response = host.request(
        "session/new",
        serde_json::json!({
            "cwd": cwd,
            "mcpServers": [{"type": "future-transport", "name": "future", "url": "https://mcp.example.test/mcp"}]
        }),
    );
    let listed = host.request("session/list", serde_json::json!({"cwd": cwd}));
    host.finish();
    assert!(response["error"].is_object(), "{response}");
    assert_eq!(listed["result"]["sessions"], serde_json::json!([]));
}

/// Catches: applying capacity eviction to an existing key erases its backoff,
/// while skipping eviction for a new key retains failed selections indefinitely.
#[test]
fn failed_acp_admission_preserves_backoff_at_capacity_and_evicts_oldest_for_new_selection() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n",
    )
    .expect("isolated profile configuration");
    let starts = places.workspace_path().join("mcp-starts.txt");
    let server = places.file(
        "failing-mcp.sh",
        "IFS= read -r request || exit 1\nprintf '%s\\n' \"$2\" >> \"$1\"\nexit 1\n",
    );
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let load = |selection: usize| {
        serde_json::json!({
            "sessionId": session,
            "cwd": places.workspace_path(),
            "mcpServers": [{
                "name": "failing",
                "command": "/bin/sh",
                "args": [server, starts, selection.to_string()],
                "env": []
            }]
        })
    };
    let began = std::time::Instant::now();
    for selection in 0..64 {
        let response = host.request("session/load", load(selection));
        assert!(response["error"].is_object(), "{response}");
    }
    assert!(
        began.elapsed() < std::time::Duration::from_secs(60),
        "fill must finish inside the initial backoff"
    );
    let repeated = host.request("session/load", load(0));
    assert!(
        repeated["error"]["data"]
            .as_str()
            .is_some_and(|message| message.contains("retry later")),
        "an existing failed selection must keep its backoff at capacity: {repeated}"
    );
    let requests = std::fs::read_to_string(&starts).expect("failed starts recorded");
    assert_eq!(
        requests.lines().count(),
        64,
        "a full cache must not restart an existing selection"
    );

    let added = host.request("session/load", load(64));
    assert!(added["error"].is_object(), "{added}");
    let evicted = host.request("session/load", load(0));
    assert!(evicted["error"].is_object(), "{evicted}");
    host.finish();
    let requests = std::fs::read_to_string(starts).expect("evicted selection starts again");
    let selections: Vec<_> = requests.lines().collect();
    let expected: Vec<_> = (0..65)
        .chain(std::iter::once(0))
        .map(|selection| selection.to_string())
        .collect();
    assert_eq!(
        selections, expected,
        "a new selection must evict the oldest inactive failure"
    );
}
