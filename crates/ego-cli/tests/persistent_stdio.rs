//! coverage for the live `ego mcp-server` stdio process.
//!
//! The child receives three complete current MCP requests while its stdin
//! remains open. Responses must be observable before that stdin is closed;
//! waiting for EOF is not a valid persistent-peer implementation.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// How long a response may take while the child's stdin is still open.
///
/// The contract these tests hold is that a response arrives *without* stdin
/// being closed, not that it arrives quickly: stdin is dropped only after the
/// window, so the window's length is what makes the observation possible and
/// not what it asserts. Sized for a machine running the whole workspace's test
/// binaries at once, since two sibling PTY suites failed on exactly that gap
/// while passing alone.
const RESPONSE_WINDOW: Duration = Duration::from_secs(30);

const EXPECTED_TOOLS: [&str; 5] = ["run", "continue", "cancel", "status", "compact"];
const EXPECTED_RESOURCES: [&str; 3] = [
    "ego://capabilities",
    "ego://runs/{run_id}/summary",
    "ego://runs/{run_id}/artifacts/{artifact_id}",
];

fn request(id: usize, method: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": {}
        })
    )
}

fn initialize_request() -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 99, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "claude", "version": "1.0"}}
        })
    )
}

fn initialized_notification() -> String {
    format!(
        "{}\n",
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
}

fn stdout_lines(stdout: std::process::ChildStdout) -> Receiver<Result<String, std::io::Error>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

fn reap(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("the MCP server can be polled") {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("the MCP server can be killed");
            return child.wait().expect("the killed MCP server can be reaped");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn parse_lines(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("complete JSON-RPC response"))
        .collect()
}

#[test]
fn mcp_server_processes_three_requests_before_piped_stdin_eof() {
    let fixture = TempDir::new().expect("temporary MCP fixture");
    let home = fixture.path().join("ego-home");
    let workspace = fixture.path().join("workspace");
    let sessions = workspace.join("sessions");
    std::fs::create_dir_all(&home).expect("MCP home");
    std::fs::create_dir_all(&sessions).expect("MCP sessions");
    std::fs::write(
        home.join("config.toml"),
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nbudget = \"4k\"\n",
    )
    .expect("isolated MCP config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ego"))
        .env_clear()
        .env("HOME", &home)
        .env("EGO_HOME", &home)
        .env("EGO_MODEL", "ollama/qwen3")
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .env("PATH", "/usr/bin:/bin")
        .current_dir(&workspace)
        .args([
            "mcp-server",
            "-C",
            workspace.to_str().expect("workspace path"),
            "--session-dir",
            sessions.to_str().expect("session path"),
            "--max-concurrent-runs",
            "1",
            "--retained-requests",
            "4",
            "--max-resource-bytes",
            "65536",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the ego binary was built");
    let mut stdin = child.stdin.take().expect("piped MCP stdin");
    let responses = stdout_lines(child.stdout.take().expect("piped MCP stdout"));

    let mut input = initialize_request();
    input.push_str(&initialized_notification());
    input.push_str(&request(0, "resources/list"));
    input.push_str(&request(1, "tools/list"));
    input.push_str(&request(2, "resources/list"));
    stdin
        .write_all(input.as_bytes())
        .expect("the MCP server accepts all requests");
    stdin.flush().expect("the MCP requests are flushed");

    let mut lines = Vec::new();
    let mut timed_out = false;
    let mut stdout_error = None;
    for _ in 0..4 {
        match responses.recv_timeout(RESPONSE_WINDOW) {
            Ok(Ok(line)) => lines.push(line),
            Ok(Err(error)) => {
                timed_out = true;
                stdout_error = Some(error.to_string());
                break;
            }
            Err(error) => {
                timed_out = true;
                stdout_error = Some(error.to_string());
                break;
            }
        }
    }
    // This is intentionally after the response window. Closing stdin before
    // the assertion would turn an EOF-triggered flush into false liveness.
    drop(stdin);
    let status = reap(&mut child);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped MCP stderr")
        .read_to_string(&mut stderr)
        .expect("MCP stderr is readable");

    assert!(
        !timed_out,
        "all three MCP responses must arrive before piped stdin EOF; got {}; child status: {status:?}; stdout error: {stdout_error:?}; stderr: {stderr:?}",
        lines.len(),
    );
    let responses = parse_lines(&lines);
    assert_eq!(
        responses
            .iter()
            .map(|response| response["id"].as_u64().expect("response id"))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([0, 1, 2, 99])
    );
    // Concurrent JSON-RPC handlers may complete in any order; the ID set is
    // the contract, and each payload is selected by its own ID below.
    assert_eq!(responses.len(), 4, "one response per request");
    let response_for = |id| {
        responses
            .iter()
            .find(|response| response["id"].as_u64() == Some(id))
            .unwrap_or_else(|| panic!("response for request id {id}"))
    };

    let resources = response_for(0)["result"]["resources"]
        .as_array()
        .expect("resources/list result");
    assert_eq!(
        resources
            .iter()
            .map(|resource| resource["uri"].as_str().expect("resource URI"))
            .collect::<Vec<_>>(),
        EXPECTED_RESOURCES.to_vec()
    );

    let tools = response_for(1)["result"]["tools"]
        .as_array()
        .expect("tools/list result");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect::<Vec<_>>(),
        EXPECTED_TOOLS.to_vec()
    );

    let resources_again = response_for(2)["result"]["resources"]
        .as_array()
        .expect("second resources/list result");
    assert_eq!(
        resources_again
            .iter()
            .map(|resource| resource["uri"].as_str().expect("resource URI"))
            .collect::<Vec<_>>(),
        EXPECTED_RESOURCES.to_vec()
    );
}

#[test]
fn mcp_server_negotiates_claude_protocol_revision() {
    let fixture = TempDir::new().expect("temporary MCP fixture");
    let home = fixture.path().join("ego-home");
    let workspace = fixture.path().join("workspace");
    let sessions = workspace.join("sessions");
    std::fs::create_dir_all(&home).expect("MCP home");
    std::fs::create_dir_all(&sessions).expect("MCP sessions");
    std::fs::write(
        home.join("config.toml"),
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nbudget = \"4k\"\n",
    )
    .expect("isolated MCP config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ego"))
        .env_clear()
        .env("HOME", &home)
        .env("EGO_HOME", &home)
        .env("EGO_MODEL", "ollama/qwen3")
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .env("PATH", "/usr/bin:/bin")
        .current_dir(&workspace)
        .args([
            "mcp-server",
            "-C",
            workspace.to_str().expect("workspace path"),
            "--session-dir",
            sessions.to_str().expect("session path"),
            "--max-concurrent-runs",
            "1",
            "--retained-requests",
            "4",
            "--max-resource-bytes",
            "65536",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the ego binary was built");
    let mut stdin = child.stdin.take().expect("piped MCP stdin");
    let responses = stdout_lines(child.stdout.take().expect("piped MCP stdout"));
    let initialize = serde_json::json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "claude", "version": "1.0"}}
    });
    writeln!(stdin, "{initialize}").expect("initialize request is written");
    stdin.flush().expect("initialize request is flushed");
    let response = responses
        .recv_timeout(RESPONSE_WINDOW)
        .expect("initialize response before stdin EOF")
        .expect("stdout response");
    drop(stdin);
    let _ = reap(&mut child);
    let response: Value = serde_json::from_str(&response).expect("complete JSON-RPC response");
    assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
}
