#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{Ego, Finished, Plan, Reply, Stub, text};

const SERVER: &str = r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    if request["method"] == "server/discover":
        result = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {"listChanged": False}},
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "auto", "version": "1"}},
        }
    else:
        result = {
            "resultType": "complete",
            "tools": [
                {
                    "name": "search_tools",
                    "description": "Find a tool",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}},
                        "required": ["query"],
                        "additionalProperties": False,
                    },
                }
            ],
            "nextCursor": None,
            "ttlMs": 0,
            "cacheScope": "private",
        }
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

fn offers_tool(generation: &Value, name: &str) -> bool {
    generation["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == name))
}

/// Run one prompt with a user-configured stdio server `auto`.
fn run_with_configured_server(enabled: &str, extra: &[&str]) -> (Finished, Vec<Value>) {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("AUTO-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let server = ego.file("auto-server.py", SERVER);
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.auto]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{:?}]\n{enabled}",
            server.display().to_string()
        ),
    );
    let finished = extra
        .iter()
        .fold(ego, |ego, arg| ego.arg(arg))
        .arg("hello")
        .run();
    (finished, stub.generations())
}

// Catches: attachment still gated on --mcp, or a non-empty --tools selection
// incorrectly suppressing MCP attachment along with --no-tools.
#[test]
fn configured_server_is_attached_without_the_mcp_flag() {
    for extra in [&[][..], &["--tools", "web_fetch"][..]] {
        let (finished, generations) = run_with_configured_server("", extra);
        assert_eq!(finished.code, Some(0), "{finished:?}");
        assert!(
            generations
                .first()
                .is_some_and(|first| offers_tool(first, "search_tools")),
            "the configured server's tools were not offered: {generations:?}; {finished:?}"
        );
        assert!(
            offers_tool(&generations[0], "web_fetch"),
            "native web retrieval must coexist with discovered MCP tools: {generations:?}"
        );
        assert!(
            !finished.stderr.contains("was not connected"),
            "a healthy server drew a notice: {finished:?}"
        );
    }
}

// Catches: the enabled flag ignored, or --mcp losing its override role.
#[test]
fn disabled_server_stays_detached_until_selected_explicitly() {
    let (finished, generations) = run_with_configured_server("enabled=false\n", &[]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        generations
            .first()
            .is_some_and(|first| !offers_tool(first, "search_tools")),
        "a disabled server was attached: {generations:?}"
    );
    let (finished, generations) = run_with_configured_server("enabled=false\n", &["--mcp", "auto"]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        generations
            .first()
            .is_some_and(|first| offers_tool(first, "search_tools")),
        "--mcp did not attach the disabled server: {generations:?}; {finished:?}"
    );
}

// Catches: --mcp NAME naming an enabled server attaching it twice.
#[test]
fn explicit_selection_of_an_enabled_server_attaches_it_once() {
    let (finished, generations) = run_with_configured_server("", &["--mcp", "auto"]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    let offered = generations[0]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter(|tool| tool["function"]["name"] == "search_tools")
        .count();
    assert_eq!(offered, 1, "{generations:?}");
}

// Story 1077-e96a. Catches: --mcp-only still attaching the enabled configured
// server next to the invocation file server, which makes a host-injected server
// collide with the owner's server that publishes the same tools.
#[test]
fn mcp_only_attaches_only_the_explicit_selection() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("ONLY-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let configured = ego.file("auto-server.py", SERVER);
    let invocation = ego.file(
        "file-server.py",
        &SERVER.replace("\"search_tools\"", "\"file_tool\""),
    );
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.auto]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{:?}]\n",
            configured.display().to_string()
        ),
    );
    let selector = ego.file(
        "mcp.json",
        &serde_json::json!({
            "name": "fixture",
            "transport": "stdio",
            "command": "/usr/bin/python3",
            "args": [invocation.display().to_string()],
            "inherit_env": [],
        })
        .to_string(),
    );
    let finished = ego
        .arg("--mcp-only")
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .arg("hello")
        .run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    let generations = stub.generations();
    let first = generations.first().expect("one generation");
    assert!(
        offers_tool(first, "file_tool"),
        "the selected file server was not attached: {generations:?}; {finished:?}"
    );
    assert!(
        !offers_tool(first, "search_tools"),
        "--mcp-only attached the configured server: {generations:?}"
    );
}

fn run_with_remote_server(extra: &[&str]) -> (Finished, Vec<Value>) {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("REMOTE-COMPLETE"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub).args(["--model", "ollama/qwen3", "--sandbox", "off", "--offline"]);
    ego.home_file(
        "config.toml",
        "[mcp.servers.remote]\ntransport='http'\nurl='https://mcp.example.test/v1'\n",
    );
    let finished = extra
        .iter()
        .fold(ego, |ego, arg| ego.arg(arg))
        .arg("hello")
        .run();
    (finished, stub.generations())
}

// Catches: one server that --offline refuses aborts the session, or the refusal is
// silent, or admission is skipped for automatic attachment.
#[test]
fn refused_configured_server_is_reported_once_and_the_session_continues() {
    let (finished, _) = run_with_remote_server(&[]);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(finished.stdout.contains("REMOTE-COMPLETE"), "{finished:?}");
    let notice = "MCP server 'remote' was not connected: MCP HTTP server `remote` is unavailable under --offline";
    assert_eq!(finished.stderr.matches(notice).count(), 1, "{finished:?}");
}

// Catches: transport admission refuses before the registry, so the host sees
// the reason but the model cannot tell why no tools are available.
#[test]
fn configured_transport_refusal_reaches_first_provider_request() {
    assert_configured_transport_refusal(false);
}

// Catches: suppressing the operator diagnostic also drops the classified note
// needed by the model, even though the refusal itself has not changed.
#[test]
fn quiet_suppresses_host_diagnostic_but_keeps_model_transport_refusal() {
    assert_configured_transport_refusal(true);
}

fn assert_configured_transport_refusal(quiet: bool) {
    let mut args = vec!["--output", "jsonl"];
    if quiet {
        args.push("--quiet");
    }
    let (finished, requests) = run_with_remote_server(&args);
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert_eq!(
        finished.stderr.contains("is unavailable under --offline"),
        !quiet,
        "{finished:?}"
    );
    let started = finished
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["type"] == "session_started")
        .expect("host session-start event");
    assert!(!started.to_string().contains("mcp.example.test"));
    assert!(
        started["mcp_errors"].as_array().is_none_or(Vec::is_empty),
        "the transport refusal should happen before registry discovery"
    );
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(!request.to_string().contains("mcp.example.test"));
    let messages = request["messages"].as_array().expect("provider messages");
    assert!(
        messages.iter().any(|message| {
            let text = message["content"].to_string();
            text.contains("MCP server 'remote'")
                && text.contains("offline")
                && text.contains("No tool call was attempted")
        }),
        "first provider request omitted the pre-registry transport refusal"
    );
}

// Catches: explicit selection losing its hard failure when admission refuses.
#[test]
fn refused_explicit_selection_stays_a_usage_error() {
    let (finished, _) = run_with_remote_server(&["--mcp", "remote"]);
    assert_ne!(finished.code, Some(0), "{finished:?}");
    assert!(
        finished.stderr.contains("is unavailable under --offline"),
        "{finished:?}"
    );
}

// Story 1029-896e. Catches: --no-tools still starting a configured MCP child
// and printing its startup failure despite offering no tools to the model.
#[test]
fn no_tools_does_not_start_a_configured_mcp_child_or_warn() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("NO-TOOLS-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--bare",
        "--no-hooks",
        "--no-tools",
    ]);
    let marker = ego.workspace_path().join("mcp-started");
    let server = ego.file(
        "failing-server.py",
        "import pathlib, sys\npathlib.Path(sys.argv[1]).write_text('started')\nsys.exit(1)\n",
    );
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.failing]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{:?}, {:?}]\n",
            server.display().to_string(),
            marker.display().to_string(),
        ),
    );
    let finished = ego.arg("hello").run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        finished.stdout.contains("NO-TOOLS-COMPLETE"),
        "{finished:?}"
    );
    assert!(
        !marker.exists() && !finished.stderr.contains("MCP"),
        "--no-tools started MCP: child_started={}; {finished:?}",
        marker.exists(),
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "{generations:?}");
    assert!(
        generations[0]["tools"].as_array().is_none_or(Vec::is_empty),
        "--no-tools offered tools: {generations:?}"
    );
}

// Catches: a server that cannot start stopping the session, or going unreported.
#[test]
fn configured_server_that_fails_to_start_is_reported_once_and_the_session_continues() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("BROKEN-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let missing = ego.workspace_path().join("missing-server");
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.missing]\ntransport='stdio'\ncommand={:?}\n",
            missing
        ),
    );
    let finished = ego.arg("hello").run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(finished.stdout.contains("BROKEN-COMPLETE"), "{finished:?}");
    assert!(!finished.stderr.contains("configured but not attached"));
    assert_eq!(
        finished.stderr.matches("MCP server 'missing'").count(),
        1,
        "{finished:?}"
    );
    assert!(
        !finished.stderr.contains(&missing.display().to_string()),
        "unsafe raw configuration diagnostic: {finished:?}"
    );
}

// Story 957-0130. Catches: a server without `server/discover` reported as an invalid
// configuration, hiding the protocol version ego needs.
#[test]
fn legacy_stdio_server_is_reported_as_a_protocol_version_mismatch() {
    const LEGACY: &str = r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"],
                      "error": {"code": -32601, "message": "Method not found"}}), flush=True)
"#;
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("LEGACY-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let server = ego.file("legacy-server.py", LEGACY);
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.legacy]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{:?}]\n",
            server.display().to_string()
        ),
    );
    let finished = ego.arg("hello").run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        finished.stderr.contains("protocol-version mismatch")
            && finished.stderr.contains("server/discover"),
        "{finished:?}"
    );
    assert!(
        !finished.stderr.contains("configuration is invalid"),
        "{finished:?}"
    );
}

/// List one stdio server `failing` that runs `command args` and return its diagnostic.
fn mcp_ls_diagnostic(command: &str, script: Option<&str>) -> Value {
    let ego = Ego::nowhere();
    let args = script.map_or_else(String::new, |script| {
        format!(
            "{:?}",
            ego.file("failing-server.py", script).display().to_string()
        )
    });
    ego.home_file(
        "config.toml",
        &format!("[mcp.servers.failing]\ntransport='stdio'\ncommand={command:?}\nargs=[{args}]\n"),
    );
    let listed = ego.subcommand("mcp").args(["ls", "--json"]).run();
    assert_eq!(listed.code, Some(0), "{listed:?}");
    let report: Value = serde_json::from_str(&listed.stdout).expect("JSON listing");
    let row = &report["servers"][0];
    assert_eq!(row["name"], "failing", "{report}");
    assert_eq!(row["admitted"], false, "{report}");
    row["diagnostic"].clone()
}

/// A stdio server that answers every request with `reply` in place of `result`.
fn answering(reply: &str) -> String {
    format!(
        "import json, sys\nfor line in sys.stdin:\n    request = json.loads(line)\n    if 'id' in request:\n        print(json.dumps({{'jsonrpc': '2.0', 'id': request['id'], {reply}}}), flush=True)\n"
    )
}

// Story 1102-a6de. Catches: admission mapping every server-side protocol failure to
// invalid_value, so a server that answers `server/discover` with a transient -32000
// tells the user to fix a configuration that is correct.
#[test]
fn mcp_ls_reports_a_failing_discover_as_a_protocol_failure() {
    let script = answering("'error': {'code': -32000, 'message': 'backend unavailable'}");
    let diagnostic = mcp_ls_diagnostic("/usr/bin/python3", Some(&script));
    assert_eq!(diagnostic["class"], "protocol", "{diagnostic}");
    assert!(
        diagnostic["message"]
            .as_str()
            .is_some_and(|message| message.contains("communication with the server failed")),
        "{diagnostic}"
    );
}

// Story 1102-a6de. Catches: a stdio server that exits before it answers reported as
// a protocol failure or an invalid configuration, not as a server ego cannot reach.
#[test]
fn mcp_ls_reports_a_server_that_exits_at_once_as_a_transport_failure() {
    let diagnostic = mcp_ls_diagnostic("/usr/bin/true", None);
    assert_eq!(diagnostic["class"], "transport", "{diagnostic}");
    assert!(
        diagnostic["message"]
            .as_str()
            .is_some_and(|message| message.contains("could not connect to the server")),
        "{diagnostic}"
    );
}

// Story 1102-a6de. Catches: a `server/discover` answer that is not a discover result
// reported as a protocol failure or an invalid configuration, hiding that the
// server's answer was malformed.
#[test]
fn mcp_ls_reports_a_malformed_discover_answer_as_an_invalid_response() {
    let script = answering("'result': {'unexpected': True}");
    let diagnostic = mcp_ls_diagnostic("/usr/bin/python3", Some(&script));
    assert_eq!(diagnostic["class"], "invalid_response", "{diagnostic}");
    assert!(
        diagnostic["message"]
            .as_str()
            .is_some_and(|message| message.contains("server returned an invalid response")),
        "{diagnostic}"
    );
}

// Story 957-0130. Catches: `ego mcp ls` hiding a server the invocation selected with
// `--mcp @FILE`.
#[test]
fn mcp_ls_lists_an_invocation_selected_server() {
    let ego = Ego::nowhere();
    let selector = ego.file(
        "mcp.json",
        r#"{"name":"fixture","transport":"stdio","command":"/usr/bin/python3","args":[],"inherit_env":[]}"#,
    );
    let listed = ego
        .subcommand("mcp")
        .args(["ls", "--mcp"])
        .arg(&format!("@{}", selector.display()))
        .run();
    assert_eq!(listed.code, Some(0), "{listed:?}");
    assert!(listed.stdout.starts_with("fixture\t"), "{listed:?}");
}

// Story 785-8798. Catches: an OAuth credential crossing the network in
// plaintext without a notice, or the notice repeating per request. The
// IPv4-mapped literal is a host that the loopback rule refuses.
#[test]
fn plaintext_oauth_server_off_loopback_is_warned_once() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("LAN-COMPLETE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--model", "ollama/qwen3"]);
    ego.home_file(
        "config.toml",
        "[mcp.servers.lan]\ntransport='http'\nurl='http://[::ffff:127.0.0.1]:9/mcp'\nauth='oauth'\nallow_insecure_loopback=true\n",
    );
    let finished = ego.arg("hello").run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert_eq!(
        finished
            .stderr
            .matches("MCP server 'lan' sends its OAuth credential over plaintext HTTP")
            .count(),
        1,
        "{finished:?}"
    );
}

// Story 1026-6fff. Catches: one silent stdio server blocking the entire
// listing, hiding healthy siblings, or surviving the listing deadline.
#[test]
fn mcp_ls_returns_within_a_deadline_when_a_stdio_server_never_answers() {
    use std::io::Read;
    use std::process::Command;
    use std::time::{Duration, Instant};

    let ego = Ego::nowhere();
    let pid_file = ego.file("silent.pid", "");
    let request_file = ego.file("silent.requests", "");
    let silent = ego.file(
        "silent.sh",
        "echo $$ > \"$1\"\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$2\"; done\n",
    );
    let healthy = ego.file("healthy.py", SERVER);
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.a_silent]\ntransport='stdio'\ncommand='/bin/sh'\nargs=[{silent:?}, {pid_file:?}, {request_file:?}]\n\n[mcp.servers.z_healthy]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{healthy:?}]\n",
        ),
    );
    let start = Instant::now();
    let mut running = ego.subcommand("mcp").args(["ls", "--json"]).spawn();
    let status = loop {
        if let Some(status) = running.child.try_wait().expect("query listing process") {
            break Some(status);
        }
        if start.elapsed() >= Duration::from_secs(10) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let elapsed = start.elapsed();
    if status.is_none() {
        running.child.kill().expect("stop hung listing");
        running.child.wait().expect("reap hung listing");
    }
    let pid = std::fs::read_to_string(&pid_file).expect("silent server started");
    let alive = Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .output()
        .expect("inspect our silent child")
        .status
        .success();
    if alive {
        Command::new("/bin/kill")
            .args(["-KILL", pid.trim()])
            .output()
            .expect("stop our silent child");
    }
    assert!(
        status.is_some(),
        "mcp ls still waiting after {elapsed:?}; silent server received: {}",
        std::fs::read_to_string(&request_file).unwrap(),
    );
    assert!(!alive, "silent child {pid} survived the listing");
    let mut stdout = String::new();
    running
        .child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let finished = running.finish();
    assert!(status.unwrap().success(), "{finished:?}");
    let listed: Value = serde_json::from_str(&stdout).expect("JSON listing");
    let rows = listed["servers"].as_array().unwrap();
    let silent = rows.iter().find(|row| row["name"] == "a_silent").unwrap();
    assert_eq!(silent["admitted"], false, "{listed}");
    assert_eq!(silent["diagnostic"]["class"], "timeout", "{listed}");
    let diagnostic = silent["diagnostic"]["message"].as_str().unwrap();
    assert!(
        diagnostic.contains("deadline")
            || diagnostic.contains("timed out")
            || diagnostic.contains("timeout"),
        "silent server needs a timeout cause: {listed}",
    );
    let healthy = rows.iter().find(|row| row["name"] == "z_healthy").unwrap();
    assert_eq!(healthy["admitted"], true, "{listed}");
}

// Story 1026-6fff. Catches: interactive startup inheriting the 30-minute
// tool deadline instead of reaching the prompt after a silent server times out,
// or dropping a healthy server that takes six seconds to start.
#[test]
fn interactive_startup_continues_when_a_stdio_server_never_answers() {
    use std::time::{Duration, Instant};

    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("SESSION-READY"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let script = ego.file("silent.sh", "while IFS= read -r line; do :; done\n");
    let healthy = ego.file(
        "slow-healthy.py",
        &format!("import time\ntime.sleep(6)\n{SERVER}"),
    );
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.silent]\ntransport='stdio'\ncommand='/bin/sh'\nargs=[{script:?}]\n\n[mcp.servers.slow_healthy]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{healthy:?}]\n",
        ),
    );
    let start = Instant::now();
    let finished = ego.run_pty("hello");
    assert!(start.elapsed() < Duration::from_secs(45), "{finished:?}");
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(finished.stdout.contains("SESSION-READY"), "{finished:?}");
    assert!(
        stub.generations()
            .first()
            .is_some_and(|generation| offers_tool(generation, "search_tools")),
        "the healthy six-second server was not attached: {finished:?}",
    );
    assert!(
        finished.stdout.contains("server is not responding"),
        "{finished:?}",
    );
}
