//! Running the real binary against a fake provider.
//!
//! Two things this harness refuses to do, and both are the point.
//!
//! **It never reaches the network.** Every test here points `OLLAMA_HOST`,
//! `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` at a loopback socket this process
//! owns — `anthropic` is the hosted row these tests select, and the other two
//! are pointed at loopback so a run that reached them by mistake still cannot
//! leave the machine. A paid call is not
//! an ordinary gate — a suite that bills someone is a suite people stop
//! running — and a stub also lets a test say "the daemon answered 500", which
//! no real provider will do on request.
//!
//! **It never calls a function in `ego-cli`.** These tests run
//! `CARGO_BIN_EXE_ego` as a child process, because the thing under test is the
//! process: an exit status is not observable from inside the program that
//! produces it, and neither is "stdout carried this and stderr carried that".

// This module is compiled independently into each integration-test binary.
// Some helpers are intentionally used only by another test target; report
// those as harness sharing rather than as production dead code under `-D
// warnings`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{NativePtySystem, PtySize, PtySystem};

pub const PNG_1X1: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 0,
    5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

pub const READS_PICTURES: &str = r#"image_input = { kind = "supported", accepted_media_types = ["png"], max_images = 4, max_bytes_per_image = 65536, max_total_bytes = 65536, max_width = 16384, max_height = 16384, max_pixels = 100000000 }"#;
pub const READS_NO_PICTURES: &str = r#"image_input = "unsupported""#;

pub fn vision_model_facts(image_input: &str) -> String {
    format!(
        r#"
[model."anthropic/fixture-vision".capabilities]
schema_version = 5
max_context_tokens = 128000
max_output_tokens = 4096
max_tools = 16
supports_parallel_tool_calls = false
reasoning_carryover = "unsupported"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "none"
transport = "sse"
{image_input}
"#
    )
}

// ACP measures every offered confinement twice before announcing options.
// Under the parallel workspace suite, that work has exceeded 45 seconds.
pub const ACP_MEASURED_OPTIONS_DEADLINE: Duration = Duration::from_secs(180);

pub fn wait_pty_child(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    deadline: Duration,
    transcript: &[u8],
) -> Result<portable_pty::ExitStatus, String> {
    let mut killer = child.clone_killer();
    let (sent, received) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = sent.send(child.wait());
    });
    match received.recv_timeout(deadline) {
        Ok(status) => {
            waiter.join().expect("PTY child waiter thread");
            status.map_err(|error| error.to_string())
        }
        Err(error) => {
            let kill_error = killer.kill().err();
            let reap = received.recv_timeout(Duration::from_secs(2));
            if reap.is_ok() {
                waiter.join().expect("PTY child waiter thread after kill");
            }
            let tail: String = String::from_utf8_lossy(transcript)
                .chars()
                .rev()
                .take(400)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            Err(format!(
                "PTY child did not exit within {deadline:?} after EOF ({error}); \
                 kill error: {kill_error:?}; reap: {reap:?}. Terminal tail:\n{tail}"
            ))
        }
    }
}

#[cfg(not(windows))]
pub fn fixture_path() -> OsString {
    OsString::from("/usr/bin:/bin")
}

#[cfg(windows)]
pub fn fixture_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

#[cfg(not(windows))]
pub fn windows_system_environment() -> Vec<(&'static str, OsString)> {
    Vec::new()
}

#[cfg(windows)]
pub fn windows_system_environment() -> Vec<(&'static str, OsString)> {
    ["SystemRoot", "WINDIR"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
        .collect()
}

#[cfg(not(windows))]
pub fn ready_prompt_count(output: &[u8]) -> usize {
    const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

    // A typed-line repaint also contains "> ". Only a completed, empty
    // editor frame is ready for input; Ctrl-D during submission can be lost
    // while Reedline leaves raw mode and starts its next cursor query.
    output
        .windows(HIDE_CURSOR.len())
        .enumerate()
        .filter(|(_, window)| *window == HIDE_CURSOR)
        .filter(|(start, _)| {
            let frame = &output[start + HIDE_CURSOR.len()..];
            let Some(end) = frame
                .windows(SHOW_CURSOR.len())
                .position(|window| window == SHOW_CURSOR)
            else {
                return false;
            };
            let mut screen = vt100::Parser::new(24, 80, 0);
            screen.process(&frame[..end]);
            screen.screen().contents().trim() == ">"
        })
        .count()
}

#[cfg(windows)]
pub fn ready_prompt_count(output: &[u8]) -> usize {
    const DSR_QUERY: &[u8] = b"\x1b[6n";
    const PLAIN_PROMPT: &[u8] = b"> ";
    const ANSI_PROMPT: &[u8] = b">\x1b[K";
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

    let after_dsr = output
        .windows(DSR_QUERY.len())
        .rposition(|window| window == DSR_QUERY)
        .map_or(0, |position| position + DSR_QUERY.len());
    let ready = &output[after_dsr..];
    (0..ready.len())
        .filter(|&position| {
            let tail = &ready[position..];
            let prompt = tail.starts_with(PLAIN_PROMPT) || tail.starts_with(ANSI_PROMPT);
            prompt
                && tail
                    .windows(SHOW_CURSOR.len())
                    .any(|window| window == SHOW_CURSOR)
        })
        .count()
}

/// What the stub answers a `/v1/chat/completions` request with.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A whole SSE body, written in one go.
    Sse(String),
    /// An SSE body in two parts with a pause between them.
    ///
    /// This is what makes liveness testable from outside: the consumer has a
    /// window in which it has seen `head` and cannot yet have seen the end of
    /// the turn, so "an event reached stdout before the turn that produced it
    /// ended" becomes an observation rather than an assumption.
    Paced {
        head: String,
        tail: String,
        pause: Duration,
    },
    /// An SSE body whose tail is held until the test observes the live
    /// surface event that must precede it.
    GatedPaced {
        head: String,
        tail: String,
        gate: Arc<ReleaseGate>,
    },
    /// An HTTP failure, for the provider-error paths.
    Status(u16),
}

/// A deterministic hand-off between the PTY observer and the fake provider.
#[derive(Debug)]
pub struct ReleaseGate {
    released: Mutex<bool>,
    wake: Condvar,
}

impl ReleaseGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            released: Mutex::new(false),
            wake: Condvar::new(),
        })
    }

    pub fn release(&self) {
        *self.released.lock().expect("release gate lock") = true;
        self.wake.notify_all();
    }

    fn wait(&self) {
        let mut released = self.released.lock().expect("release gate lock");
        while !*released {
            released = self.wake.wait(released).expect("release gate wait");
        }
    }
}

/// What the fake daemon knows.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Hold discovery so a PTY can submit input before the composer opens.
    pub discovery_gate: Option<Arc<ReleaseGate>>,
    /// The names `/api/tags` reports as installed.
    pub installed: Vec<String>,
    /// The context `/api/ps` reports as served. `None` makes the model look
    /// permanently unloaded, which is the `Undiscoverable` path.
    pub context_length: Option<u64>,
    /// Consumed in order, one per generation. The last is repeated once the
    /// list runs out, so a test that does not care how many turns happen does
    /// not have to count them.
    pub completions: Vec<Reply>,
    /// A status and body for `/api/tags` in place of the installed list.
    ///
    /// The control plane otherwise always succeeds, so the only provider
    /// failure observable from outside is a *generation* one — and story
    /// `regression_case` is about the *pre-turn* half, where the catalogue request
    /// carries the operator's credential and the answer's bytes must not be
    /// relayed into what a terminal or an ACP host is shown.
    pub tags_refusal: Option<(u16, String)>,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            discovery_gate: None,
            installed: vec![MODEL.to_string()],
            context_length: Some(32_768),
            completions: vec![Reply::Sse(text("It is done."))],
            tags_refusal: None,
        }
    }
}

/// The model every test asks for unless it is testing what happens when it
/// asks for another.
pub const MODEL: &str = "qwen3";

/// Stable durable id for isolated CLI fixture sessions.
pub const FIXTURE_SESSION_ID: &str = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718293";

/// A complete SSE transcript that answers with `content` and stops.
///
/// Shaped after `crates/ego-ai/tests/goldens/stream_text.sse`, which was itself
/// pinned from `openai/openai-openapi` rather than from memory: usage arrives
/// *after* the chunk carrying `finish_reason`, and `[DONE]` last.
pub fn text(content: &str) -> String {
    costing(content, 10, 4)
}

/// The same, reporting the usage a test needs it to.
///
/// The counts are a *parameter* because two of `CLI.md`'s contracts are about
/// them and neither can be reached with a fixed transcript: `usage` in §4.1's
/// object, and §4.5's "budget beats schema-retry" — which needs a turn
/// expensive enough that the next one crosses a ceiling.
pub fn costing(content: &str, prompt_tokens: u64, completion_tokens: u64) -> String {
    let escaped = serde_json::to_string(content).expect("a string is serialisable");
    let total = prompt_tokens + completion_tokens;
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"\"}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{escaped}}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt_tokens},\"completion_tokens\":{completion_tokens},\"total_tokens\":{total}}}}}\n\n\
         data: [DONE]\n\n"
    )
}

/// The same, split so the first delta is deliverable well before the end.
pub fn paced(content: &str, pause: Duration) -> Reply {
    let escaped = serde_json::to_string(content).expect("a string is serialisable");
    Reply::Paced {
        head: format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{escaped}}},\"finish_reason\":null}}]}}\n\n"
        ),
        tail: "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14}}\n\n\
               data: [DONE]\n\n"
            .to_string(),
        pause,
    }
}

/// A paced assistant delta followed by one complete write call.
pub fn paced_tool_call(id: &str, name: &str, arguments: &str, pause: Duration) -> Reply {
    Reply::Paced {
        head: "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n\
                data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"STREAMED-BEFORE-TOOL\"},\"finish_reason\":null}]}\n\n"
            .to_string(),
        tail: tool_call(id, name, arguments),
        pause,
    }
}

/// A line-framed tool call whose provider tail waits for an explicit test
/// release. The head remains observable, so PTY tests can synchronize on the
/// real UI event instead of racing a wall-clock pause.
pub fn gated_paced_tool_call_line(
    id: &str,
    name: &str,
    arguments: &str,
) -> (Reply, Arc<ReleaseGate>) {
    let Reply::Paced { mut head, tail, .. } = paced_tool_call(id, name, arguments, Duration::ZERO)
    else {
        unreachable!("paced_tool_call always returns a paced reply");
    };
    let encoded =
        serde_json::to_string("STREAMED-BEFORE-TOOL").expect("the marker is serialisable");
    let framed = serde_json::to_string("STREAMED-BEFORE-TOOL\n")
        .expect("the line-framed marker is serialisable");
    head = head.replace(
        &format!("\"content\":{encoded}"),
        &format!("\"content\":{framed}"),
    );
    let gate = ReleaseGate::new();
    (
        Reply::GatedPaced {
            head,
            tail,
            gate: Arc::clone(&gate),
        },
        gate,
    )
}

/// An SSE transcript asking for one tool call and stopping.
pub fn tool_call(id: &str, name: &str, arguments: &str) -> String {
    let arguments = serde_json::to_string(arguments).expect("a string is serialisable");
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14}}}}\n\n\
         data: [DONE]\n\n"
    )
}

/// One request the stub was sent.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub path: String,
    pub body: String,
}

/// A fake Ollama/OpenAI daemon on loopback.
///
/// The listener is dropped when the `Stub` is, which ends the serving thread on
/// its next `accept`. Tests therefore keep it alive for as long as the child
/// runs, which the borrow checker enforces for them.
pub struct Stub {
    pub address: String,
    seen: Arc<Mutex<Vec<Recorded>>>,
    mode: StubMode,
}

#[derive(Debug, Clone, Copy)]
enum StubMode {
    Provider,
    Mcp,
    McpTool,
}

impl Stub {
    pub fn start(plan: Plan) -> Self {
        Self::start_mode(plan, StubMode::Provider)
    }

    /// Start the same provider fixture with a minimal streamable MCP endpoint
    /// on the same listener, so tests can observe discovery without a second
    /// network adapter.
    pub fn start_mcp(plan: Plan) -> Self {
        Self::start_mode(plan, StubMode::Mcp)
    }

    pub fn start_mcp_tool(plan: Plan) -> Self {
        Self::start_mode(plan, StubMode::McpTool)
    }

    fn start_mode(plan: Plan, mode: StubMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
        let address = listener
            .local_addr()
            .expect("a bound listener has an address")
            .to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let thread_seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut generation = 0usize;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                // One request per connection, then closed. Keep-alive would
                // buy nothing here and would make the reply logic guess where
                // one exchange ends.
                if serve(stream, &plan, &thread_seen, &mut generation, mode).is_err() {
                    break;
                }
            }
        });

        Stub {
            address,
            seen,
            mode,
        }
    }

    /// Every request the stub has been sent, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().expect("no writer panics").clone()
    }

    /// The bodies posted to the generation endpoint, parsed.
    pub fn generations(&self) -> Vec<serde_json::Value> {
        self.requests()
            .into_iter()
            .filter(|req| req.path.contains("chat/completions") || req.path.ends_with("/messages"))
            .map(|req| serde_json::from_str(&req.body).expect("the request body is JSON"))
            .collect()
    }
}

fn serve(
    mut stream: TcpStream,
    plan: &Plan,
    seen: &Arc<Mutex<Vec<Recorded>>>,
    generation: &mut usize,
    mode: StubMode,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();

    seen.lock().expect("no writer panics").push(Recorded {
        path: path.clone(),
        body: body.clone(),
    });

    if matches!(mode, StubMode::Mcp | StubMode::McpTool) && path == "/mcp" {
        return mcp_response(&mut stream, &body, matches!(mode, StubMode::McpTool));
    }

    match path.as_str() {
        "/api/tags" if plan.tags_refusal.is_some() => {
            let (status, body) = plan.tags_refusal.as_ref().expect("just matched");
            write!(
                stream,
                "HTTP/1.1 {status} Stub\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )?;
            stream.flush()
        }
        "/api/tags" => {
            if let Some(gate) = &plan.discovery_gate {
                gate.wait();
            }
            let models: Vec<serde_json::Value> = plan
                .installed
                .iter()
                .map(|name| {
                    serde_json::json!({
                        "name": name,
                        "capabilities": ["completion", "tools"],
                    })
                })
                .collect();
            json(&mut stream, &serde_json::json!({ "models": models }))
        }
        "/api/ps" => {
            let models: Vec<serde_json::Value> = plan
                .context_length
                .map(|context| serde_json::json!({ "name": MODEL, "context_length": context }))
                .into_iter()
                .collect();
            json(&mut stream, &serde_json::json!({ "models": models }))
        }
        "/api/chat" => json(&mut stream, &serde_json::json!({ "done_reason": "load" })),
        _ => {
            let reply = plan
                .completions
                .get(*generation)
                .or_else(|| plan.completions.last())
                .cloned()
                .unwrap_or(Reply::Status(500));
            *generation += 1;
            sse(&mut stream, reply, &path)
        }
    }
}

fn mcp_response(stream: &mut TcpStream, body: &str, tool: bool) -> std::io::Result<()> {
    let request: serde_json::Value =
        serde_json::from_str(body).expect("MCP probe receives JSON-RPC request");
    let Some(id) = request.get("id") else {
        write!(
            stream,
            "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        )?;
        return stream.flush();
    };
    let method = request["method"].as_str().unwrap_or_default();
    let result = match method {
        "server/discover" => serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": if tool { serde_json::json!({"tools":{"listChanged":false}}) } else { serde_json::json!({}) },
            "ttlMs": 0,
            "cacheScope": "private"
        }),
        "tools/list" => serde_json::json!({
            "resultType": "complete",
            "tools": if tool { serde_json::json!([{"name":"search_tools","description":"fixture search","inputSchema":{"type":"object","additionalProperties":false}}]) } else { serde_json::json!([]) },
            "nextCursor": null,
            "ttlMs": 0,
            "cacheScope": "private"
        }),
        "tools/call" if tool => serde_json::json!({
            "resultType": "complete",
            "content": [{"type":"text","text":"lookup-complete"}],
            "isError": false
        }),
        _ => serde_json::json!({
            "resultType": "complete",
            "resources": [],
            "prompts": [],
            "nextCursor": null,
            "ttlMs": 0,
            "cacheScope": "private"
        }),
    };
    let response = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let body = response.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn json(stream: &mut TcpStream, value: &serde_json::Value) -> std::io::Result<()> {
    let body = value.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// Re-spell one complete chat-completions transcript as Anthropic Messages.
///
/// The reply builders above speak one dialect, and after story `regression_case` the
/// hosted row these tests select is `anthropic`, which speaks another. The
/// alternative was a second spelling of every fixture at all fifty-odd call
/// sites, where the *semantics* — this answer, these tokens, this tool call —
/// are what each test is actually asserting on and are identical in both.
/// So the transcript is written once and rendered per endpoint.
///
/// It translates a **whole** transcript, never a fragment, which is why
/// [`Reply::Paced`] has no Messages form: `message_start` must carry
/// `input_tokens`, and the chat dialect does not report them until the final
/// chunk. No hosted test paces its stub — every one of them uses
/// `Plan::default()` — so this refuses loudly rather than inventing a figure a
/// budget assertion would then read back as fact.
fn as_messages(chat: &str) -> String {
    let mut text = String::new();
    let mut tools: Vec<serde_json::Value> = Vec::new();
    let mut prompt_tokens = 0u64;
    let mut completion_tokens = 0u64;
    let mut stop_reason = "end_turn";

    for line in chat.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload.trim() == "[DONE]" {
            continue;
        }
        let chunk: serde_json::Value =
            serde_json::from_str(payload).expect("the stub writes its own chunks as JSON");
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            prompt_tokens = usage["prompt_tokens"].as_u64().unwrap_or_default();
            completion_tokens = usage["completion_tokens"].as_u64().unwrap_or_default();
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            if let Some(content) = choice["delta"]["content"].as_str() {
                text.push_str(content);
            }
            for call in choice["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                tools.push(call.clone());
            }
            if choice["finish_reason"].as_str() == Some("tool_calls") {
                stop_reason = "tool_use";
            }
        }
    }

    let mut out = String::new();
    let start = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": "msg_stub",
            "type": "message",
            "role": "assistant",
            "model": MODEL,
            "content": [],
            "stop_reason": serde_json::Value::Null,
            "stop_sequence": serde_json::Value::Null,
            "usage": { "input_tokens": prompt_tokens, "output_tokens": 0 },
        },
    });
    out.push_str(&format!("event: message_start\ndata: {start}\n\n"));

    let mut index = 0;
    if !text.is_empty() {
        let begin = serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": "" },
        });
        let delta = serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "text_delta", "text": text },
        });
        out.push_str(&format!("event: content_block_start\ndata: {begin}\n\n"));
        out.push_str(&format!("event: content_block_delta\ndata: {delta}\n\n"));
        out.push_str(&format!(
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n"
        ));
        index += 1;
    }

    for call in &tools {
        let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
        let begin = serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {
                "type": "tool_use",
                "id": call["id"],
                "name": call["function"]["name"],
                "input": {},
            },
        });
        let delta = serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "input_json_delta", "partial_json": arguments },
        });
        out.push_str(&format!("event: content_block_start\ndata: {begin}\n\n"));
        out.push_str(&format!("event: content_block_delta\ndata: {delta}\n\n"));
        out.push_str(&format!(
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n"
        ));
        index += 1;
    }

    let end = serde_json::json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason, "stop_sequence": serde_json::Value::Null },
        "usage": { "output_tokens": completion_tokens },
    });
    out.push_str(&format!("event: message_delta\ndata: {end}\n\n"));
    out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    out
}

/// An SSE response, delimited by the connection closing.
///
/// No `content-length`, so the client reads until EOF — which is what lets the
/// paced variant stop mid-stream and resume without knowing the total up front.
fn sse(stream: &mut TcpStream, reply: Reply, path: &str) -> std::io::Result<()> {
    let reply = if path.ends_with("/messages") {
        match reply {
            Reply::Sse(body) => Reply::Sse(as_messages(&body)),
            Reply::Status(status) => Reply::Status(status),
            Reply::Paced { .. } | Reply::GatedPaced { .. } => {
                panic!("the Messages rendering has no paced form; see `as_messages`")
            }
        }
    } else {
        reply
    };
    match reply {
        Reply::Status(status) => {
            let body = format!("the stub was told to answer {status}");
            write!(
                stream,
                "HTTP/1.1 {status} Stub\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )?;
            stream.flush()
        }
        Reply::Sse(body) => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            )?;
            stream.flush()
        }
        Reply::Paced { head, tail, pause } => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{head}"
            )?;
            stream.flush()?;
            std::thread::sleep(pause);
            write!(stream, "{tail}")?;
            stream.flush()
        }
        Reply::GatedPaced { head, tail, gate } => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{head}"
            )?;
            stream.flush()?;
            gate.wait();
            write!(stream, "{tail}")?;
            stream.flush()
        }
    }
}

/// A home and a workspace two invocations can share.
///
/// Needed because `--session NAME` is only meaningful across runs: attaching to
/// a named session in a *different* workspace is a different session by design,
/// so a test that gives each run its own directories cannot observe a resume at
/// all.
pub struct Places {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Places {
    pub fn new() -> Self {
        Places {
            home: tempfile::tempdir().expect("a temp dir"),
            workspace: tempfile::tempdir().expect("a temp dir"),
        }
    }

    pub fn ego(&self, stub: &Stub) -> Ego {
        let mut ego = Ego::at(self.home.path(), self.workspace.path(), Some(stub));
        ego.args = vec!["--thinking".to_owned(), "off".to_owned()];
        ego.fixture_default_thinking = true;
        ego
    }

    pub fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.workspace.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a temp dir is writable");
        }
        std::fs::write(&path, contents).expect("a temp dir is writable");
        path
    }

    pub fn home_path(&self) -> &Path {
        self.home.path()
    }

    pub fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }
}

/// An invocation of the real binary, in a workspace of its own.
pub struct Ego {
    home: PathBuf,
    workspace: PathBuf,
    /// Kept alive for as long as the invocation is described, so a builder
    /// that owns its directories does not delete them before it runs.
    keep: Vec<tempfile::TempDir>,
    /// Which of `CLI.md` §2's commands this invocation runs. `run` unless a
    /// test says otherwise, because almost every test here is about a turn.
    subcommand: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    stdin: Option<String>,
    fixture_default_thinking: bool,
}

/// What a finished invocation left behind.
#[derive(Debug)]
pub struct Finished {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Keep test-owned homes and workspaces alive for post-run assertions.
    _keep: Vec<tempfile::TempDir>,
}

impl Finished {
    /// The single object `--output json` writes.
    pub fn report(&self) -> serde_json::Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|err| panic!("stdout was not one JSON object ({err}): {}", self.stdout))
    }

    /// The events `--output jsonl` wrote, one per line.
    pub fn events(&self) -> Vec<serde_json::Value> {
        self.stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|err| panic!("`{line}` is not JSON: {err}"))
            })
            .collect()
    }

    /// The `type` of every event, in order.
    pub fn kinds(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| {
                event
                    .get("type")
                    .and_then(|kind| kind.as_str())
                    .map(str::to_string)
            })
            .collect()
    }

    /// Read durable event payloads from the invocation-owned temporary homes.
    pub fn durable_payloads(&self) -> Vec<serde_json::Value> {
        self._keep
            .iter()
            .flat_map(|home| {
                let sessions = home.path().join("sessions");
                let Ok(entries) = std::fs::read_dir(sessions) else {
                    return Vec::new();
                };
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path().join("events.jsonl"))
                    .filter_map(|path| std::fs::read_to_string(path).ok())
                    .flat_map(|contents| {
                        contents
                            .lines()
                            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                            .filter_map(|record| record.get("payload").cloned())
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

impl Ego {
    /// Build the real binary command for a PTY test, preserving the same
    /// cleared environment and temporary-directory ownership as `run`.
    pub fn pty_command(&self) -> portable_pty::CommandBuilder {
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_ego"));
        command.env_clear();
        command.env("HOME", &self.home);
        command.env("EGO_HOME", &self.home);
        command.env("PATH", fixture_path());
        for (key, value) in windows_system_environment() {
            command.env(key, value);
        }
        // `cargo llvm-cov` points the instrumented child at its own profile
        // directory through this variable; without it the real binary's
        // production paths show 0% coverage.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command.cwd(&self.workspace);
        if !self.subcommand.is_empty() {
            command.arg(&self.subcommand);
        }
        command.args(&self.args);
        command
    }

    /// A run against `stub`, in an empty workspace, with an empty home.
    pub fn against(stub: &Stub) -> Self {
        let home = tempfile::tempdir().expect("a temp dir");
        let workspace = tempfile::tempdir().expect("a temp dir");
        let mut ego = Ego::at(home.path(), workspace.path(), Some(stub));
        // The Ollama discovery fixture intentionally exposes no declared
        // reasoning default. Keep every process-level fixture explicit so a
        // missing provider fact cannot turn an unrelated test into a startup
        // refusal.
        ego.args = vec!["--thinking".to_owned(), "off".to_owned()];
        ego.fixture_default_thinking = true;
        ego.keep = vec![home, workspace];
        ego
    }

    /// A run with no provider at all, for the paths that refuse before one is
    /// reached.
    pub fn nowhere() -> Self {
        let home = tempfile::tempdir().expect("a temp dir");
        let workspace = tempfile::tempdir().expect("a temp dir");
        let mut ego = Ego::at(home.path(), workspace.path(), None);
        ego.keep = vec![home, workspace];
        ego
    }

    /// A run in directories somebody else owns.
    pub fn at(home: &Path, workspace: &Path, stub: Option<&Stub>) -> Self {
        let mut env = HashMap::new();
        if let Some(stub) = stub {
            env.insert("OLLAMA_HOST".to_string(), stub.address.clone());
            env.insert(
                "OPENAI_BASE_URL".to_string(),
                format!("http://{}", stub.address),
            );
            // The hosted row these tests select. `openai` moved to Responses
            // and is unselectable pending live evidence
            // (`2026-09-03-public-provider-wires-require-live-promotion.md`),
            // so `anthropic` carries the offline hosted assertions: it is the
            // selectable row that settles the same way, from a supplied rate
            // table rather than a provider-reported total. `OPENAI_BASE_URL`
            // stays set so the openai row still points at loopback and a test
            // that reaches it by mistake cannot leave this machine.
            env.insert(
                "ANTHROPIC_BASE_URL".to_string(),
                format!("http://{}", stub.address),
            );
        }
        if stub.is_none() {
            // Never the machine's own daemon. A run this harness builds must
            // not be able to reach a provider nobody in the test asked for:
            // port 1 is reserved, so the connection is refused rather than
            // quietly answered by whatever the developer has installed.
            env.insert("OLLAMA_HOST".to_string(), "127.0.0.1:1".to_string());
        }
        Ego {
            home: home.to_path_buf(),
            workspace: workspace.to_path_buf(),
            keep: Vec::new(),
            subcommand: "run".to_string(),
            args: Vec::new(),
            env,
            stdin: None,
            fixture_default_thinking: false,
        }
    }

    /// Run `ego <name>` rather than `ego run`.
    pub fn subcommand(mut self, name: &str) -> Self {
        if !matches!(name, "run" | "resume") && self.fixture_default_thinking {
            self.args.drain(..2);
            self.fixture_default_thinking = false;
        }
        self.subcommand = name.to_string();
        self
    }

    /// Run `ego` with no subcommand at all — the conversational surface.
    ///
    /// Not `subcommand("")`: an empty argument is a *first argument*, and
    /// `ego ""` is a run with an empty prompt rather than a session. The
    /// distinction is the whole of what this method exists to make.
    pub fn conversing(mut self) -> Self {
        self.subcommand.clear();
        self
    }

    /// Exercise model-derived thinking without the Ollama fixture's explicit
    /// `--thinking off` override.
    pub fn without_fixture_thinking(mut self) -> Self {
        if self.fixture_default_thinking {
            self.args.drain(..2);
            self.fixture_default_thinking = false;
        }
        self
    }

    pub fn arg(mut self, arg: &str) -> Self {
        if self.fixture_default_thinking && (arg == "--thinking" || arg.starts_with("--thinking="))
        {
            self.args.drain(..2);
            self.fixture_default_thinking = false;
        }
        self.args.push(arg.to_string());
        self
    }

    pub fn args<const N: usize>(mut self, args: [&str; N]) -> Self {
        if self.fixture_default_thinking
            && args
                .iter()
                .any(|arg| *arg == "--thinking" || arg.starts_with("--thinking="))
        {
            self.args.drain(..2);
            self.fixture_default_thinking = false;
        }
        self.args.extend(args.iter().map(|arg| arg.to_string()));
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_string(), value.to_string());
        self
    }

    pub fn stdin(mut self, text: &str) -> Self {
        self.stdin = Some(text.to_string());
        self
    }

    /// How long the terminal owner waits for the child to say anything more.
    ///
    /// This is a liveness bailout, never a pass condition: no branch below
    /// treats the deadline expiring as the end of the script, so raising it
    /// cannot make a failing run pass. It only decides how long a genuine
    /// hang takes to become a legible failure instead of an idle `cargo
    /// test`. The gap it has to clear is the child's quiet startup — config
    /// load, provider probe, first reedline draw — measured on a machine
    /// running the whole workspace's test binaries at once, not on an
    /// otherwise idle one. Eight seconds was the latter, and
    /// `compaction_skill` (25s alone) failed at 12s under a
    /// `--workspace` run while passing on its own.
    const QUIET_DEADLINE: Duration = Duration::from_secs(60);

    /// Run one interactive script through a real pseudo-terminal.
    ///
    /// Interactive typed commands are classified by the terminal owner after
    /// the editor reads them; a pipe is intentionally treated as one seed
    /// prompt. This keeps process tests on the same public binary seam while
    /// answering the terminal's cursor-position query as the child runs.
    pub fn run_pty(self, input: &str) -> Finished {
        self.run_pty_trusting("yes", input)
    }

    /// Run one interactive script, answering the trust question `trust_answer`.
    ///
    /// Most callers want [`Self::run_pty`] and its `yes`; a caller that is
    /// about the refusal needs to say `no` without reimplementing the whole
    /// terminal owner.
    pub fn run_pty_trusting(self, trust_answer: &str, input: &str) -> Finished {
        self.run_pty_responding("Trust project files", trust_answer, input)
    }

    /// Run one interactive script and answer one startup modal by its stable
    /// prompt text.  This is deliberately narrower than a general terminal
    /// driver: the response is sent once, before the ordinary input script,
    /// so a startup admission can be tested without teaching every caller
    /// about PTY cursor handshakes.
    pub fn run_pty_responding(self, prompt: &str, answer: &str, input: &str) -> Finished {
        self.run_pty_script(prompt, answer, Some(input))
    }

    /// Run a conversation whose first turn is already present in argv, then
    /// close the terminal at the first idle prompt.
    pub fn run_pty_seeded(self) -> Finished {
        self.run_pty_responding_seeded("Trust project files", "yes")
    }

    /// Answer one startup modal for an argv-seeded conversation, then close
    /// the terminal at the first idle prompt.
    pub fn run_pty_responding_seeded(self, prompt: &str, answer: &str) -> Finished {
        self.run_pty_script(prompt, answer, None)
    }

    fn run_pty_script(self, prompt: &str, answer: &str, input: Option<&str>) -> Finished {
        assert!(
            input.is_none_or(|line| !line.contains('\r') && !line.contains('\n')),
            "PTY script must be one unterminated line; the helper submits it"
        );
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty");
        let child = pair.slave.spawn_command(self.pty_command()).expect("spawn");
        let master = pair.master;
        drop(pair.slave);
        let writer = Arc::new(Mutex::new(master.take_writer().expect("writer")));
        let reader = master.try_clone_reader().expect("reader");
        let (chunks, received) = std::sync::mpsc::channel();
        let responder = Arc::clone(&writer);
        let reader = std::thread::spawn(move || {
            const DSR_QUERY: &[u8] = b"\x1b[6n";
            const DSR_RESPONSE: &[u8] = b"\x1b[4;1R";
            let mut reader = reader;
            let mut output = Vec::new();
            let mut chunk = [0; 1024];
            let mut responses = 0;
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        output.extend_from_slice(&chunk[..length]);
                        let queries = output
                            .windows(DSR_QUERY.len())
                            .filter(|window| *window == DSR_QUERY)
                            .count();
                        while responses < queries {
                            let mut writer = responder.lock().expect("pty writer lock");
                            writer.write_all(DSR_RESPONSE).expect("DSR response");
                            writer.flush().expect("DSR response flush");
                            responses += 1;
                        }
                        if chunks.send(chunk[..length].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
            output
        });
        let mut output = Vec::new();
        let mut trust_answered = false;
        let mut submitted = false;
        let mut eof_sent = false;
        let mut prompt_count_before_submit = 0;
        while !eof_sent {
            let chunk = received
                .recv_timeout(Self::QUIET_DEADLINE)
                .unwrap_or_else(|_| {
                    // The tail rather than the whole transcript: what the child
                    // was doing when it went quiet is the diagnosis, and the
                    // escape-heavy redraws before it are noise.
                    let shown = String::from_utf8_lossy(&output);
                    let tail: String = shown
                        .chars()
                        .rev()
                        .take(400)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    panic!(
                        "the child said nothing for {:?}; trust answered: {trust_answered}, \
                     script submitted: {submitted}. Terminal tail:\n{tail}",
                        Self::QUIET_DEADLINE
                    )
                });
            output.extend_from_slice(&chunk);
            if !trust_answered
                && output
                    .windows(prompt.len())
                    .any(|window| window == prompt.as_bytes())
            {
                let mut writer = writer.lock().expect("pty writer lock");
                writer
                    .write_all(format!("{answer}\r").as_bytes())
                    .expect("startup answer");
                writer.flush().expect("startup answer flush");
                trust_answered = true;
            }
            let prompt_count = ready_prompt_count(&output);
            if !submitted && prompt_count > 0 {
                // ConPTY publishes the completed redraw before its input side
                // finishes consuming the DSR response. Keep the scripted
                // line out of that control-sequence read.
                #[cfg(windows)]
                std::thread::sleep(Duration::from_millis(25));
                if input.is_none() {
                    let mut writer = writer.lock().expect("pty writer lock");
                    writer.write_all(b"\x04").expect("interactive EOF");
                    writer.flush().expect("interactive EOF flush");
                    eof_sent = true;
                    continue;
                }
                // Submit at the first completed empty editor frame. A later
                // empty frame still gates EOF after the script is accepted.
                let mut writer = writer.lock().expect("pty writer lock");
                writer
                    .write_all(input.expect("the scripted branch has input").as_bytes())
                    .expect("interactive script");
                writer.write_all(b"\r").expect("interactive submit");
                writer.flush().expect("interactive script flush");
                prompt_count_before_submit = prompt_count;
                submitted = true;
            } else if submitted && prompt_count > prompt_count_before_submit {
                let mut writer = writer.lock().expect("pty writer lock");
                writer.write_all(b"\x04").expect("interactive EOF");
                writer.flush().expect("interactive EOF flush");
                eof_sent = true;
            }
        }
        drop(writer);
        let status = wait_pty_child(child, Self::QUIET_DEADLINE, &output)
            .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
        drop(master);
        let output = reader.join().expect("pty reader thread");
        Finished {
            code: Some(status.exit_code() as i32),
            stdout: String::from_utf8_lossy(&output).into_owned(),
            stderr: String::new(),
            _keep: self.keep,
        }
    }

    /// Observe an early interactive surface through the same PTY owner as
    /// [`Self::run_pty`], without waiting for the idle prompt.  Startup model
    /// selection and refusal happen before reedline draws that prompt, so the
    /// ordinary scripted-input helper cannot prove either path safely.
    pub fn run_pty_until(self, needle: &str) -> Finished {
        self.run_pty_probe("Trust project files", "yes", Some(needle))
    }

    /// Observe an interactive process that must refuse before opening a
    /// prompt.  A bounded kill is returned as a failed `Finished` result on a
    /// genuine hang, keeping a broken startup test from leaking a child.
    pub fn run_pty_to_exit(self) -> Finished {
        self.run_pty_probe("Trust project files", "yes", None)
    }

    /// Observe an interactive process that answers one startup modal and must
    /// then terminate before the ordinary idle prompt.  This is the refusal
    /// counterpart to [`Self::run_pty_responding`].
    pub fn run_pty_responding_to_exit(self, prompt: &str, answer: &str) -> Finished {
        self.run_pty_probe(prompt, answer, None)
    }

    fn run_pty_probe(self, prompt: &str, answer: &str, needle: Option<&str>) -> Finished {
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty");
        let mut child = pair.slave.spawn_command(self.pty_command()).expect("spawn");
        let mut killer = child.clone_killer();
        let (status_sender, status_receiver) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let status = child.wait();
            status_sender.send(status).expect("child status receiver");
        });
        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
        let reader = pair.master.try_clone_reader().expect("reader");
        let (chunks, received) = std::sync::mpsc::channel();
        let responder = Arc::clone(&writer);
        let reader = std::thread::spawn(move || {
            const DSR_QUERY: &[u8] = b"\x1b[6n";
            const DSR_RESPONSE: &[u8] = b"\x1b[4;1R";
            let mut reader = reader;
            let mut output = Vec::new();
            let mut chunk = [0; 1024];
            let mut responses = 0;
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        output.extend_from_slice(&chunk[..length]);
                        let queries = output
                            .windows(DSR_QUERY.len())
                            .filter(|window| *window == DSR_QUERY)
                            .count();
                        while responses < queries {
                            let mut writer = responder.lock().expect("pty writer lock");
                            writer.write_all(DSR_RESPONSE).expect("DSR response");
                            writer.flush().expect("DSR response flush");
                            responses += 1;
                        }
                        if chunks.send(chunk[..length].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
            output
        });

        let mut output = Vec::new();
        let mut trust_answered = false;
        let deadline = Instant::now() + Self::QUIET_DEADLINE;
        let mut timed_out = false;
        let mut status = None;
        let mut kill_error = None;
        loop {
            if let Ok(child_status) = status_receiver.try_recv() {
                status = Some(child_status);
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                kill_error = killer.kill().err();
                timed_out = true;
                break;
            }
            match received.recv_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(chunk) => {
                    output.extend_from_slice(&chunk);
                    if !trust_answered
                        && output
                            .windows(prompt.len())
                            .any(|window| window == prompt.as_bytes())
                    {
                        let mut writer = writer.lock().expect("pty writer lock");
                        writer
                            .write_all(format!("{answer}\r").as_bytes())
                            .expect("startup answer");
                        writer.flush().expect("startup answer flush");
                        trust_answered = true;
                    }
                    if needle.is_some_and(|needle| {
                        output
                            .windows(needle.len())
                            .any(|window| window == needle.as_bytes())
                    }) {
                        kill_error = killer.kill().err();
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        drop(writer);
        drop(pair);
        let status = status
            .unwrap_or_else(|| {
                status_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_else(|receive_error| {
                        panic!(
                            "PTY child did not exit after kill ({kill_error:?}): {receive_error}"
                        )
                    })
            })
            .expect("child ran");
        waiter.join().expect("child waiter thread panicked");
        output = reader.join().expect("pty reader thread");
        Finished {
            code: Some(status.exit_code() as i32),
            stdout: String::from_utf8_lossy(&output).into_owned(),
            stderr: if timed_out {
                format!(
                    "PTY probe timed out after {:?} waiting for {:?}; child exit code {}",
                    Self::QUIET_DEADLINE,
                    needle,
                    status.exit_code()
                )
            } else {
                String::new()
            },
            _keep: self.keep,
        }
    }

    /// Write a file into the workspace and return its path.
    pub fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.workspace.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a temp dir is writable");
        }
        std::fs::write(&path, contents).expect("a temp dir is writable");
        path
    }

    /// Write a file into `$EGO_HOME` and return its path.
    pub fn home_file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.home.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a temp dir is writable");
        }
        std::fs::write(&path, contents).expect("a temp dir is writable");
        path
    }

    pub fn home_path(&self) -> &Path {
        &self.home
    }

    pub fn workspace_path(&self) -> &Path {
        &self.workspace
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ego"));
        // Cleared and rebuilt: a developer with `EGO_MODEL` set would otherwise
        // get different answers from the same test, and a suite whose result
        // depends on the shell that started it proves nothing.
        command.env_clear();
        command.env("HOME", &self.home);
        command.env("EGO_HOME", &self.home);
        command.env("PATH", fixture_path());
        for (key, value) in windows_system_environment() {
            command.env(key, value);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command.current_dir(&self.workspace);
        if !self.subcommand.is_empty() {
            command.arg(&self.subcommand);
        }
        command.args(&self.args);
        command
    }

    /// Run to completion.
    pub fn run(self) -> Finished {
        let mut command = self.command();
        command.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().expect("the binary was built");
        if let Some(text) = &self.stdin {
            child
                .stdin
                .as_mut()
                .expect("stdin was piped")
                .write_all(text.as_bytes())
                .expect("the child is reading");
            drop(child.stdin.take());
        }
        let output = child.wait_with_output().expect("the child ran");
        Finished {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            _keep: self.keep,
        }
    }

    /// Start the binary with stdin held open, for the protocols whose next
    /// frame depends on the answer to the last one.
    ///
    /// [`spawn`](Self::spawn) nulls stdin, which is right for a run that is
    /// only watched. An ACP or MCP conversation cannot be written ahead of
    /// time: every frame after `session/new` names an id the server minted, so
    /// the test has to read before it can write.
    pub fn spawn_conversing(self) -> Running {
        let mut command = self.command();
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let child = command.spawn().expect("the binary was built");
        Running {
            child,
            _keep: self.keep,
        }
    }

    /// Start the binary and hand back the child, for the tests that have to
    /// observe it while it is still running.
    pub fn spawn(self) -> Running {
        let mut command = self.command();
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let child = command.spawn().expect("the binary was built");
        Running {
            child,
            _keep: self.keep,
        }
    }
}

/// A binary that has not finished yet.
pub struct Running {
    pub child: Child,
    _keep: Vec<tempfile::TempDir>,
}

impl Running {
    pub fn stdout(&mut self) -> BufReader<std::process::ChildStdout> {
        BufReader::new(self.child.stdout.take().expect("stdout was piped"))
    }

    /// Whether the process is still running *now*.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn finish(mut self) -> Finished {
        let mut stderr = String::new();
        if let Some(mut handle) = self.child.stderr.take() {
            let _ = handle.read_to_string(&mut stderr);
        }
        let status = self.child.wait().expect("the child ran");
        Finished {
            code: status.code(),
            stdout: String::new(),
            stderr,
            _keep: self._keep,
        }
    }
}

/// A running `ego acp`, spoken to one JSON-RPC frame at a time.
///
/// The framing loop below — write a request, then read past the session
/// updates that arrive first until *this* id comes back — is the only way to
/// observe what a host actually receives, and two suites now need it. It lives
/// here rather than in either of them so the next one does not write a third
/// copy that skips updates slightly differently.
pub struct AcpHost {
    running: Running,
    inbox: BufReader<std::process::ChildStdout>,
    outbox: std::process::ChildStdin,
    next_id: u64,
}

impl AcpHost {
    /// Adopt a process already spawned with `.subcommand("acp")` and
    /// `spawn_conversing()`, taking both of its pipes.
    pub fn adopt(mut running: Running) -> Self {
        let inbox = BufReader::new(running.child.stdout.take().expect("stdout was piped"));
        let outbox = running.child.stdin.take().expect("stdin was piped");
        Self {
            running,
            inbox,
            outbox,
            next_id: 1,
        }
    }

    /// Write one request and read until *its* answer, skipping the session
    /// updates that arrive first.
    pub fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.send_request(method, params);
        loop {
            let message = self.read_frame();
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return message;
            }
        }
    }

    pub fn send_request(&mut self, method: &str, params: serde_json::Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }));
        id
    }

    pub fn write_frame(&mut self, frame: serde_json::Value) {
        writeln!(self.outbox, "{frame}").expect("the agent is reading");
        self.outbox.flush().expect("the agent is reading");
    }

    pub fn read_frame(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let read = self
            .inbox
            .read_line(&mut line)
            .expect("the agent is writing");
        assert!(read > 0, "the agent closed without answering");
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("ACP stdout was not JSON-RPC ({error}): {line}"))
    }

    pub fn initialize(&mut self) -> serde_json::Value {
        self.request(
            "initialize",
            serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
    }

    /// `session/new` for `cwd`, returning the session id minted.
    pub fn new_session(&mut self, cwd: &str) -> String {
        let opened = self.request(
            "session/new",
            serde_json::json!({"cwd": cwd, "mcpServers": []}),
        );
        opened["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new answered without a session id: {opened}"))
            .to_owned()
    }

    /// Close stdin and collect the process's stderr.
    pub fn finish(self) -> String {
        drop(self.outbox);
        self.running.finish().stderr
    }
}
