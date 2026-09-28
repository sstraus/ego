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
