//! Adversarial contracts for the session-scoped stdio MCP scope (story
//! regression_case) through the packaged binary and real stdio children.

#![cfg(unix)]

mod support;

use std::time::{Duration, Instant};

use support::{AcpHost, Ego, Places};

fn host_with_sandbox(places: &Places, sandbox: &str) -> AcpHost {
    std::fs::write(
        places.home_path().join("config.toml"),
        format!(
            "[profile.acp]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = '{sandbox}'\nmode = 'default'\n"
        ),
    )
    .expect("isolated profile configuration");
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    host
}

fn cwd(places: &Places) -> &str {
    places.workspace_path().to_str().expect("UTF-8 root")
}

const WORKING_SERVER: &str = r#"import json, os, sys
with open(sys.argv[1], "a", encoding="utf-8") as starts:
    starts.write(f"{os.getpid()}\n")
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    if request["method"] == "server/discover":
        result = {"resultType": "complete", "supportedVersions": ["2026-07-28"], "capabilities": {}, "ttlMs": 0, "cacheScope": "private"}
    else:
        result = {"resultType": "complete", "tools": [], "resources": [], "prompts": [], "nextCursor": None, "ttlMs": 0, "cacheScope": "private"}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

fn pid_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn supplied(server: &std::path::Path, marker: &std::path::Path) -> serde_json::Value {
    serde_json::json!([{
        "name": "writing",
        "command": "/bin/sh",
        "args": [server, marker],
        "env": []
    }])
}

/// Whether the supplied server managed to write into the workspace while the
/// given session method discovered it.
fn discovery_wrote(sandbox: &str, method: &str) -> bool {
    let places = Places::new();
    let mut host = host_with_sandbox(&places, sandbox);
    let marker = places
        .workspace_path()
        .join(format!("wrote-{}.txt", method.replace('/', "-")));
    let server = places.file(
        "writing-mcp.sh",
        "printf 'x' > \"$1\" 2>/dev/null\nexit 1\n",
    );
    let source = host.new_session(cwd(&places));
    let servers = supplied(&server, &marker);
    let params = match method {
        "session/new" => serde_json::json!({"cwd": cwd(&places), "mcpServers": servers}),
        _ => serde_json::json!({"sessionId": source, "cwd": cwd(&places), "mcpServers": servers}),
    };
    host.request(method, params);
    host.finish();
    marker.exists()
}

/// Catches: stdio discovery confined on `session/load` only, so `new`,
/// `resume` or `fork` still start the supplied command unconfined. The
/// sandbox=off control proves the server does run when nothing confines it.
#[test]
fn stdio_discovery_is_confined_on_new_resume_and_fork() {
    for method in ["session/new", "session/resume", "session/fork"] {
        assert!(
            discovery_wrote("off", method),
            "{method}: control failed, the server never ran, so confinement proves nothing"
        );
        assert!(
            !discovery_wrote("ro", method),
            "{method}: the server wrote into a read-only session's workspace during discovery"
        );
    }
}

/// Catches: a refused admission leaving a listed, server-less session behind
/// (new) or an unexplained fork child (fork).
#[test]
fn refused_stdio_admission_leaves_no_listed_session() {
    let places = Places::new();
    let mut host = host_with_sandbox(&places, "workspace");
    let server = places.file("failing-mcp.sh", "exit 1\n");
    let marker = places.workspace_path().join("unused.txt");
    let source = host.new_session(cwd(&places));
    let list = |host: &mut AcpHost| -> Vec<String> {
        let response = host.request("session/list", serde_json::json!({"cwd": cwd(&places)}));
        response["result"]["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("no sessions: {response}"))
            .iter()
            .map(|session| session["sessionId"].as_str().expect("id").to_owned())
            .collect()
    };
    let before = list(&mut host);
    assert_eq!(before, vec![source.clone()]);
    let servers = supplied(&server, &marker);

    let new = host.request(
        "session/new",
        serde_json::json!({"cwd": cwd(&places), "mcpServers": servers}),
    );
    assert!(new["error"].is_object(), "{new}");
    assert_eq!(
        list(&mut host),
        before,
        "a refused session/new stayed listed"
    );

    let fork = host.request(
        "session/fork",
        serde_json::json!({"sessionId": source, "cwd": cwd(&places), "mcpServers": servers}),
    );
    assert!(fork["error"].is_object(), "{fork}");
    assert_eq!(
        list(&mut host),
        before,
        "a refused session/fork left a child"
    );
    host.finish();
}

/// Catches: a sandbox pick that differs from the one the stdio server was
/// admitted under being accepted, while the same pick on a server-less session
/// stays free.
#[test]
fn sandbox_pick_after_stdio_admission_is_refused_but_free_without_servers() {
    let places = Places::new();
    let mut host = host_with_sandbox(&places, "ro");
    let starts = places.workspace_path().join("starts.txt");
    let server = places.file("working-mcp.py", WORKING_SERVER);
    let pick = |host: &mut AcpHost, session: &str, value: &str| {
        host.request(
            "session/set_config_option",
            serde_json::json!({"sessionId": session, "configId": "sandbox", "value": value}),
        )
    };

    let bare = host.new_session(cwd(&places));
    let free = pick(&mut host, &bare, "workspace");
    assert!(free["result"].is_object(), "control pick failed: {free}");

    let opened = host.request(
        "session/new",
        serde_json::json!({"cwd": cwd(&places), "mcpServers": [{
            "name": "working", "command": "/usr/bin/python3",
            "args": [server, starts], "env": []
        }]}),
    );
    let session = opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("the working server was refused: {opened}"))
        .to_owned();
    let refused = pick(&mut host, &session, "workspace");
    assert!(refused["error"].is_object(), "{refused}");
    host.finish();
}

/// Catches: a stdio child that survives `session/close` (the confined scope
/// is not cancelled), left running after its session is gone.
#[test]
fn closing_a_session_leaves_no_stdio_child_running() {
    let places = Places::new();
    let mut host = host_with_sandbox(&places, "off");
    let starts = places.workspace_path().join("starts.txt");
    let server = places.file("working-mcp.py", WORKING_SERVER);
    let opened = host.request(
        "session/new",
        serde_json::json!({"cwd": cwd(&places), "mcpServers": [{
            "name": "working", "command": "/usr/bin/python3",
            "args": [server, starts], "env": []
        }]}),
    );
    let session = opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("the working server was refused: {opened}"))
        .to_owned();
    let closed = host.request("session/close", serde_json::json!({"sessionId": session}));
    assert!(closed["result"].is_object(), "{closed}");

    let recorded = std::fs::read_to_string(&starts).expect("the server started");
    let pids: Vec<&str> = recorded.lines().collect();
    assert!(!pids.is_empty(), "no stdio child was ever started");
    let deadline = Instant::now() + Duration::from_secs(5);
    while pids.iter().any(|pid| pid_alive(pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let survivors: Vec<&&str> = pids.iter().filter(|pid| pid_alive(pid)).collect();
    host.finish();
    assert!(
        survivors.is_empty(),
        "stdio children outlived session/close: {survivors:?}"
    );
}
