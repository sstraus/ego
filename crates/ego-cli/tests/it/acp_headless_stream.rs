//! Headless streaming uses the real `ego acp` stdio process, not a second RPC.
//! Provider replies come only from the existing local fixture daemon.

use crate::support;

use serde_json::json;
use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text};

fn configure(places: &Places) {
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = 'ollama/qwen3'\nthinking = 'off'\nroots = []\nsandbox = 'workspace'\n",
    )
    .expect("isolated ACP configuration");
}

fn host(places: &Places, stub: &Stub) -> AcpHost {
    configure(places);
    AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .spawn_conversing(),
    )
}

/// Catches malformed content starting a generation or breaking the next turn,
/// and continuation dropping a host attachment or the first turn's history.
#[test]
fn malformed_prompt_is_refused_then_two_stdio_turns_keep_attachment_and_history() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("first reply")),
            Reply::Sse(text("second reply")),
        ],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    assert_eq!(host.initialize()["result"]["protocolVersion"], 1);
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 cwd"));

    let malformed = host.request(
        "session/prompt",
        json!({
            "sessionId": session, "prompt": "not an array of content blocks",
        }),
    );
    assert_eq!(malformed["error"]["code"], -32602, "{malformed}");
    assert!(
        stub.generations().is_empty(),
        "invalid input spent a generation"
    );

    let first = host.request("session/prompt", json!({
        "sessionId": session, "prompt": [
            {"type":"text", "text":"Summarize these notes"},
            {"type":"resource", "resource": {
                "uri":"untitled:notes", "mimeType":"text/plain", "text":"headless attachment marker",
            }},
        ],
    }));
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    let second = host.request(
        "session/prompt",
        json!({
            "sessionId": session, "prompt": [{"type":"text", "text":"Follow up on the notes"}],
        }),
    );
    assert_eq!(second["result"]["stopReason"], "end_turn", "{second}");
    let stderr = host.finish();
    assert!(stderr.is_empty(), "unexpected diagnostics: {stderr}");

    let generations = stub.generations();
    assert_eq!(generations.len(), 2, "malformed input or EOF added a turn");
    let first_messages = generations[0]["messages"].to_string();
    assert!(
        first_messages.contains("headless attachment marker"),
        "{first_messages}"
    );
    let second_messages = generations[1]["messages"].to_string();
    for expected in [
        "headless attachment marker",
        "first reply",
        "Follow up on the notes",
    ] {
        assert!(
            second_messages.contains(expected),
            "missing {expected}: {second_messages}"
        );
    }
}

/// Catches EOF being treated as an empty prompt or waiting for initialization forever.
#[test]
fn stdin_eof_before_initialize_exits_without_generation() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    configure(&places);
    let mut running = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .subcommand("acp")
        .spawn();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while running.is_running() {
        if std::time::Instant::now() >= deadline {
            running
                .child
                .kill()
                .expect("kill timed-out owned ACP process");
            let output = running.finish();
            panic!("ACP did not exit after empty stdin EOF: {}", output.stderr);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = running.finish();
    assert_eq!(output.code, Some(0), "{}", output.stderr);
    assert!(
        output.stderr.is_empty(),
        "unexpected diagnostics: {}",
        output.stderr
    );
    assert!(stub.generations().is_empty(), "EOF submitted a prompt");
}
