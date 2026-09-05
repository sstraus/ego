//! An ACP image block, followed all the way to the provider request.
//!
//! `ego-acp/tests/prompt.rs` proves the wire half — that an image block becomes
//! a decoded, measured `Image` on the `RunPrompt` — and `agent_file.rs`
//! proves the publication half for `--attach`. Neither drives the join, and the
//! join is where this used to be impossible: the ACP adapter has no blob store,
//! so the picture has to survive the `RunExecutor` port as content and be
//! published by the composition root that opened the session.
//!
//! So this drives the real `ego acp` binary over stdio against the provider
//! stub. Passing it is the claim that a picture sent by a host reaches the model
//! having gone through the same capability check, the same blob commit and the
//! same durable prompt record a `--attach` would.

use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout};

mod support;

use support::{Ego, Plan, Running, Stub, text};

/// The 1×1 PNG the attachment tests use, so both paths are measured on one
/// fixture rather than on two pictures that could differ in what they prove.
const PNG_1X1: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 0,
    5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// The `image_input` fact that admits this PNG, and the one that admits none.
const READS_PICTURES: &str = r#"image_input = { kind = "supported", accepted_media_types = ["png"], max_images = 4, max_bytes_per_image = 65536, max_total_bytes = 65536, max_width = 16384, max_height = 16384, max_pixels = 100000000 }"#;
const READS_NO_PICTURES: &str = r#"image_input = "unsupported""#;

fn facts(image_input: &str) -> String {
    format!(
        r#"
[model."openai/fixture-vision".capabilities]
schema_version = 4
max_context_tokens = 128000
max_output_tokens = 4096
max_tools = 16
supports_parallel_tool_calls = false
reasoning_carryover = "unsupported"
reasoning_efforts = "any"
cancel = "cooperative"
cache = "none"
transport = "sse"
{image_input}
"#
    )
}

/// Base64, written out rather than pulled in as a dependency of `ego-cli`.
///
/// It is not what is under test: what is under test is whether the bytes on the
/// far side are the bytes sent, so an encoder that got this wrong would fail
/// the assertion loudly rather than let a defect through.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let packed =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[((packed >> (18 - index * 6)) & 0x3f) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One `ego acp` process, spoken to a frame at a time.
struct Host {
    running: Running,
    inbox: BufReader<ChildStdout>,
    outbox: ChildStdin,
    next_id: u64,
}

impl Host {
    fn start(stub: &Stub, image_input: &str) -> (Self, Ego) {
        let ego = Ego::against(stub);
        ego.home_file("models.toml", &facts(image_input));
        ego.home_file("config.toml", "model = \"openai/fixture-vision\"\n");
        let workspace = ego.workspace_path().to_owned();
        let mut running = Ego::at(ego.home_path(), &workspace, Some(stub))
            .subcommand("acp")
            .env("OPENAI_API_KEY", "test-key")
            .spawn_conversing();
        let inbox = BufReader::new(running.child.stdout.take().expect("stdout was piped"));
        let outbox = running.child.stdin.take().expect("stdin was piped");
        (
            Self {
                running,
                inbox,
                outbox,
                next_id: 1,
            },
            ego,
        )
    }

    /// Write one request and read until *its* answer, skipping the session
    /// updates that arrive first.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        });
        writeln!(self.outbox, "{frame}").expect("the agent is reading");
        self.outbox.flush().expect("the agent is reading");
        loop {
            let mut line = String::new();
            let read = self
                .inbox
                .read_line(&mut line)
                .expect("the agent is writing");
            assert!(read > 0, "the agent closed without answering {method}");
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return message;
            }
        }
    }

    /// `initialize` then `session/new`, returning the session id minted.
    fn open(&mut self, cwd: &str) -> String {
        let initialized = self.request(
            "initialize",
            serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        assert!(
            initialized["result"]["agentCapabilities"]["promptCapabilities"]["image"] == true,
            "the binary does not advertise image prompts: {initialized}"
        );
        let opened = self.request(
            "session/new",
            serde_json::json!({"cwd": cwd, "mcpServers": []}),
        );
        opened["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new answered without a session id: {opened}"))
            .to_owned()
    }

    fn finish(self) -> String {
        drop(self.outbox);
        self.running.finish().stderr
    }
}

fn one_picture() -> serde_json::Value {
    serde_json::json!({"type": "image", "data": base64(PNG_1X1), "mimeType": "image/png"})
}

#[test]
fn an_acp_image_block_reaches_the_provider_through_the_blob_path() {
    let stub = Stub::start(Plan {
        completions: vec![support::Reply::Sse(text("I see one pixel."))],
        ..Plan::default()
    });
    let (mut host, ego) = Host::start(&stub, READS_PICTURES);
    let cwd = ego
        .workspace_path()
        .to_str()
        .expect("a utf-8 workspace")
        .to_owned();
    let session = host.open(&cwd);

    let answered = host.request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": "what is this"}, one_picture()],
        }),
    );
    let stderr = host.finish();
    assert_eq!(
        answered["result"]["stopReason"], "end_turn",
        "the turn did not end normally: {answered} / stderr: {stderr}"
    );

    let body = stub
        .generations()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("the turn never reached the provider. stderr: {stderr}"));
    let encoded = body.to_string();
    assert!(
        encoded.contains("data:image/png;base64,"),
        "the image did not reach the provider inlined: {body}"
    );
    assert!(
        encoded.contains(&base64(PNG_1X1)),
        "the bytes the provider was sent are not the bytes the host sent: {body}"
    );
}

/// A model that reads no pictures refuses the prompt, and the refusal happens
/// before the provider is contacted.
///
/// The ACP capability says this build can *receive* an image; only the resolved
/// model can say whether one can be sent on. This is the second gate, and it
/// has to hold on this path exactly as it does for `--attach` — otherwise the
/// wire capability would have quietly widened what a model accepts.
#[test]
fn an_image_the_resolved_model_cannot_read_is_refused_before_the_provider() {
    let stub = Stub::start(Plan::default());
    let (mut host, ego) = Host::start(&stub, READS_NO_PICTURES);
    let cwd = ego
        .workspace_path()
        .to_str()
        .expect("a utf-8 workspace")
        .to_owned();
    let session = host.open(&cwd);

    let answered = host.request(
        "session/prompt",
        serde_json::json!({"sessionId": session, "prompt": [one_picture()]}),
    );
    let stderr = host.finish();

    assert!(
        answered.get("error").is_some() || answered["result"]["stopReason"] != "end_turn",
        "a model that reads no pictures answered as if it had seen one: {answered}"
    );
    assert!(
        stub.generations().is_empty(),
        "a model that reads no pictures was sent one. stderr: {stderr}"
    );
}
