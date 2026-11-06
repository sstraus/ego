//! Explicit retry intent through real ACP requests and an owned MCP process.
use crate::support;

use support::{AcpHost, Ego, Places};

// Catches: repairing a server still leaves an explicit retry blocked by cooldown.
#[test]
fn explicit_mcp_retry_after_repair_ignores_cooldown() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n",
    ).expect("profile");
    let starts = places.workspace_path().join("starts");
    let repaired = places.workspace_path().join("repaired");
    let server = places.file("repairable.py", r#"import json, os, sys
with open(sys.argv[1], "a") as starts:
    starts.write("start\n")
for line in sys.stdin:
    request = json.loads(line)
    if not os.path.exists(sys.argv[2]):
        sys.exit(1)
    if "id" not in request:
        continue
    if request["method"] == "server/discover":
        result = {"resultType": "complete", "supportedVersions": ["2026-07-28"], "capabilities": {}, "ttlMs": 0, "cacheScope": "private"}
    else:
        result = {"resultType": "complete", "tools": [], "resources": [], "prompts": [], "nextCursor": None, "ttlMs": 0, "cacheScope": "private"}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#);
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("root"));
    let load = |explicit: bool| {
        serde_json::json!({
            "sessionId": session, "cwd": places.workspace_path(),
            "mcpServers": [{"name": "repairable", "command": "/usr/bin/python3", "args": [server, starts, repaired], "env": []}],
            "_meta": {"ego": {"mcpRetry": explicit}}
        })
    };
    let failed = host.request("session/load", load(false));
    assert!(failed["error"].is_object(), "{failed}");
    std::fs::write(&repaired, "ready").expect("repair server");
    let automatic = host.request("session/load", load(false));
    assert!(
        automatic["error"]["data"]
            .as_str()
            .is_some_and(|s| s.contains("retry later")),
        "{automatic}"
    );
    let retry = host.request("session/load", load(true));
    assert!(
        retry["result"].is_object(),
        "explicit retry must admit the repaired server: {retry}"
    );
    host.finish();
    assert_eq!(
        std::fs::read_to_string(starts).expect("starts"),
        "start\nstart\n"
    );
}

// Catches: concurrent failures or subsequent automatic requests start a child storm.
#[test]
fn automatic_mcp_retry_storm_stays_backed_off_and_coalesced() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n",
    ).expect("profile");
    let starts = places.workspace_path().join("starts");
    let server = places.file(
        "failing.py",
        r#"import sys, time
sys.stdin.readline()
with open(sys.argv[1], "a") as starts:
    starts.write("start\n")
time.sleep(1)
sys.exit(1)
"#,
    );
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("root"));
    let load = |explicit: bool| {
        serde_json::json!({
            "sessionId": session, "cwd": places.workspace_path(),
            "mcpServers": [{"name": "failing", "command": "/usr/bin/python3", "args": [server, starts], "env": []}],
            "_meta": {"ego": {"mcpRetry": explicit}}
        })
    };
    for explicit in [false, true] {
        let ids: Vec<_> = (0..8)
            .map(|_| host.send_request("session/load", load(explicit)))
            .collect();
        let mut replies = std::collections::HashSet::new();
        while replies.len() < ids.len() {
            let frame = host.read_frame();
            if let Some(id) = frame["id"].as_u64() {
                assert!(ids.contains(&id), "unexpected response: {frame}");
                assert!(frame["error"].is_object(), "{frame}");
                assert!(replies.insert(id), "duplicate response: {frame}");
            }
        }
        for _ in 0..8 {
            let retry = host.request("session/load", load(false));
            assert!(
                retry["error"]["data"]
                    .as_str()
                    .is_some_and(|s| s.contains("retry later")),
                "{retry}"
            );
        }
    }
    host.finish();
    assert_eq!(
        std::fs::read_to_string(starts).expect("starts"),
        "start\nstart\n"
    );
}
