//! ACP MCP admission through the packaged binary and a real stdio child.

mod support;

use support::{AcpHost, Ego, Places};

#[test]
fn repeated_failed_acp_load_does_not_restart_the_same_stdio_server_immediately() {
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
                .is_some_and(|message| message.contains("`failing`")),
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
