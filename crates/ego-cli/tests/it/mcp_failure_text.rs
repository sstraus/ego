#![cfg(unix)]

//! What the model reads when an MCP call fails in transit. The fixture server
//! is real (a child process over stdio or a loopback HTTP listener), so every
//! text below passes through the production transport and projection code.

use crate::support;

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use support::{Ego, Plan, Reply, Stub, text, tool_call};

const STDIO_SERVER: &str = r#"
import json
import sys

mode = sys.argv[1]
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "server/discover":
        result = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {"listChanged": False}},
            "ttlMs": 0,
            "cacheScope": "private",
        }
    elif method == "tools/list":
        result = {
            "resultType": "complete",
            "tools": [{"name": "ping", "description": "Ping", "inputSchema": {"type": "object", "additionalProperties": False}}],
            "nextCursor": None,
            "ttlMs": 0,
            "cacheScope": "private",
        }
    elif method == "tools/call":
        if mode == "exit":
            sys.stderr.write("fixture crashed\n")
            sys.exit(3)
        result = {"resultType": "complete", "content": [{"type": "text", "text": "pong"}], "isError": False}
    else:
        raise AssertionError(method)
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

const HTTP_SERVER: &str = r#"
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

mode = sys.argv[1]

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if "id" not in request:
            self.send_response(202)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        method = request["method"]
        if method == "tools/call":
            if mode == "drop":
                self.connection.close()
                return
            status, _, envelope = mode.partition(":")
            if status in ("503", "429", "401", "403"):
                body = {
                    "401": b'{"error":"authentication required","echo":"fixture-secret-marker"}',
                    "403": b'{"error":"permission denied","echo":"fixture-secret-marker"}',
                }.get(status, b'{"error":"upstream said no"}')
                content_type = "application/json"
                if envelope in ("jsonrpc", "sse"):
                    body = json.dumps({
                        "jsonrpc": "2.0", "id": request["id"],
                        "error": {"code": -32000, "message": "fixture-secret-marker"},
                    }).encode()
                    if envelope == "sse":
                        content_type = "text/event-stream"
                        body = b"event: message\ndata: " + body + b"\n\n"
                self.send_response(int(status))
                if status == "429":
                    self.send_header("retry-after", "7")
                self.send_header("content-type", content_type)
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            result = {"resultType": "complete", "content": [{"type": "text", "text": "pong"}], "isError": False}
        elif method == "server/discover":
            if mode == "discover-503":
                body = b'{"error":"fixture discovery unavailable"}'
                self.send_response(503)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            result = {"resultType": "complete", "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {"listChanged": False}}, "ttlMs": 0, "cacheScope": "private"}
        elif method == "tools/list":
            result = {"resultType": "complete", "tools": [{"name": "ping", "description": "Ping", "inputSchema": {"type": "object", "additionalProperties": False}}], "nextCursor": None, "ttlMs": 0, "cacheScope": "private"}
        else:
            raise AssertionError(method)
        body = json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = HTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
"#;

struct HttpFixture {
    child: Child,
    port: u16,
}

impl HttpFixture {
    fn start(script: &std::path::Path, mode: &str) -> Self {
        let mut child = Command::new("/usr/bin/python3")
            .arg(script)
            .arg(mode)
            .stdout(Stdio::piped())
            .spawn()
            .expect("start fixture HTTP server");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("fixture stdout"))
            .read_line(&mut line)
            .expect("fixture port");
        Self {
            child,
            port: line.trim().parse().expect("fixture port number"),
        }
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run one `ping` call against the fixture and return the text the model got.
fn model_text_for(transport: &str, mode: &str) -> String {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("ping-call", "ping", "{}")),
            Reply::Sse(text("PING-DONE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
        "--output",
        "jsonl",
    ]);
    let (selector, _http) = if transport == "stdio" {
        let server = ego.file("stdio-server.py", STDIO_SERVER);
        let selector = serde_json::json!({
            "name": "fixture",
            "transport": "stdio",
            "command": "/usr/bin/python3",
            "args": [server, mode],
            "inherit_env": []
        });
        (selector, None)
    } else {
        let server = ego.file("http-server.py", HTTP_SERVER);
        let fixture = HttpFixture::start(&server, mode);
        let selector = serde_json::json!({
            "name": "fixture",
            "transport": "http",
            "url": format!("http://127.0.0.1:{}/mcp", fixture.port),
            "auth": "none",
            "allow_insecure_loopback": true
        });
        (selector, Some(fixture))
    };
    let selector = ego.file("mcp.json", &selector.to_string());
    let finished = ego
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .arg("Ping the fixture")
        .run();
    let generations = stub.generations();
    let messages = generations.last().expect("final generation")["messages"]
        .as_array()
        .expect("messages");
    let response = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "ping-call")
        .unwrap_or_else(|| panic!("no tool result: {finished:?}; {generations:?}"));
    let content: &Value = &response["content"];
    content.as_str().expect("text content").to_owned()
}

// Catches: a discovery failure reaches the host but disappears from the first
// provider request, leaving the model unaware that the tool was not admitted.
#[test]
fn failed_discovery_reaches_the_host_and_first_provider_request() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("DISCOVERY-DONE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--output",
        "jsonl",
    ]);
    let server = ego.file("discovery-server.py", HTTP_SERVER);
    let fixture = HttpFixture::start(&server, "discover-503");
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.discovery_fixture]\ntransport='http'\nurl='http://127.0.0.1:{}/mcp'\nauth='none'\nallow_insecure_loopback=true\n",
            fixture.port
        ),
    );
    // Do not mention the server or cause in the prompt: any model-visible
    // diagnostic must come from the host, not from the fixture prompt.
    let finished = ego.arg("Which integrations are available?").run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    let started = finished
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["type"] == "session_started")
        .expect("host receives a session-start event");
    let errors = started["mcp_errors"].as_array().expect("host MCP errors");
    assert!(
        errors.iter().filter_map(Value::as_str).any(|error| {
            error.contains("discovery_fixture") && error.contains("could not connect")
        }),
        "missing host diagnostic: {errors:?}"
    );

    let requests = stub.generations();
    assert_eq!(requests.len(), 1);
    let first = &requests[0];
    assert!(
        !first["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "ping"))
    );
    let messages = first["messages"].as_array().expect("provider messages");
    assert!(
        !first.to_string().contains("fixture discovery unavailable"),
        "raw discovery response reached the provider"
    );
    assert!(
        messages.iter().any(|message| {
            let text = message["content"].to_string();
            text.contains("discovery_fixture")
                && text.contains("discover")
                && text.contains("could not connect to the server")
                && text.contains("No tool call was attempted")
        }),
        "first provider request omitted the discovery failure or the no-call outcome"
    );
}

// Catches: missing OAuth credentials still cause discovery traffic, or the
// first provider request omits the operator action needed to connect.
#[test]
fn missing_startup_credential_reaches_the_model_without_contacting_server() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("CREDENTIAL-CHECK-DONE"))],
        ..Plan::default()
    });
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("MCP listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let ego = Ego::against(&stub).args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--output",
        "jsonl",
        "--timeout",
        "8s",
    ]);
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.credential_fixture]\ntransport='http'\nurl='http://{}/private-fixture-route'\nauth='oauth'\nallow_insecure_loopback=true\n",
            listener.local_addr().unwrap()
        ),
    );
    let finished = ego.arg("Which integrations are available?").run();
    assert_eq!(finished.code, Some(0), "{finished:?}");
    let connection = listener
        .accept()
        .expect_err("no MCP connection without a credential");
    assert_eq!(connection.kind(), std::io::ErrorKind::WouldBlock);

    let requests = stub.generations();
    assert_eq!(requests.len(), 1);
    let request = requests[0].to_string();
    assert!(request.contains("MCP server 'credential_fixture'"));
    assert!(request.contains("credential is unavailable"));
    assert!(request.contains("/mcp login credential_fixture"));
    assert!(request.contains("No tool call was attempted"));
    assert!(!request.contains("private-fixture-route"));
    assert!(!request.contains(&listener.local_addr().unwrap().to_string()));
}

const UNKNOWN_EFFECT: &str = "The server may have run the tool before the failure, so its effect is unknown. \
Do not repeat a call that changes state until another source confirms the result. \
If the cause is transient, a call that only reads can be repeated once.";

fn failed(cause: &str) -> String {
    format!("MCP tool `ping` on server `fixture` failed: {cause}. {UNKNOWN_EFFECT}")
}

/// Catches: a server that exits mid-call reaches the model as the bare words
/// "Transport closed", with no operation, no cause and no word on whether the
/// tool ran.
#[test]
fn server_exit_mid_call_says_the_effect_is_unknown() {
    assert_eq!(
        model_text_for("stdio", "exit"),
        failed("the server closed the connection before it answered")
    );
}

/// Catches: an HTTP 5xx answer is reported as a JSON deserialisation error
/// and the status and the server's own words are lost.
#[test]
fn http_503_keeps_the_status_and_the_server_text() {
    assert_eq!(
        model_text_for("http", "503"),
        failed(r#"the server answered HTTP 503: {"error":"upstream said no"}"#)
    );
}

/// Catches: the `Retry-After` of a 429 is dropped, so the model retries
/// blindly or gives up without the wait the server asked for.
#[test]
fn http_429_keeps_the_retry_after_the_server_sent() {
    assert_eq!(
        model_text_for("http", "429"),
        failed(r#"the server answered HTTP 429 (Retry-After: 7): {"error":"upstream said no"}"#)
    );
}

// Catches: a 401 or 403 exposes the response body, omits operator recovery,
// or advises an unchanged retry. Each envelope exercises a distinct parser.
#[test]
fn http_401_plain_gives_safe_recovery() {
    assert_http_auth_refusal("401", "plain", "check the credentials");
}

#[test]
fn http_401_jsonrpc_gives_safe_recovery() {
    assert_http_auth_refusal("401", "jsonrpc", "check the credentials");
}

#[test]
fn http_401_sse_gives_safe_recovery() {
    assert_http_auth_refusal("401", "sse", "check the credentials");
}

#[test]
fn http_403_plain_gives_safe_recovery() {
    assert_http_auth_refusal("403", "plain", "check access");
}

#[test]
fn http_403_jsonrpc_gives_safe_recovery() {
    assert_http_auth_refusal("403", "jsonrpc", "check access");
}

#[test]
fn http_403_sse_gives_safe_recovery() {
    assert_http_auth_refusal("403", "sse", "check access");
}

fn assert_http_auth_refusal(status: &str, envelope: &str, remedy: &str) {
    let mode = format!("{status}:{envelope}");
    let outcome = model_text_for("http", &mode);
    assert!(
        !outcome.contains("fixture-secret-marker"),
        "{mode}: remote body reached the model"
    );
    assert!(
        outcome.contains(&format!("HTTP {status}")),
        "{mode}: {outcome}"
    );
    assert!(outcome.contains(remedy), "{mode}: {outcome}");
    assert!(
        outcome.contains("Retrying unchanged will not help"),
        "{mode}: {outcome}"
    );
    assert!(
        outcome.contains("its effect is unknown"),
        "{mode}: {outcome}"
    );
    assert!(
        outcome.contains("Do not repeat a call that changes state"),
        "{mode}: {outcome}"
    );
    assert!(
        !outcome.contains("the tool did not run"),
        "{mode}: {outcome}"
    );
}

/// Catches: a connection lost mid-call reaches the model as Rust `Debug` text
/// with transport type names.
#[test]
fn connection_lost_mid_call_is_plain_text() {
    assert_eq!(
        model_text_for("http", "drop"),
        failed(
            "the HTTP exchange failed: error sending request: client error (SendRequest): \
             connection closed before message completed"
        )
    );
}
