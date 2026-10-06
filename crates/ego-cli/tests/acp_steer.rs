//! Durable steering at the ACP host boundary.
mod support;
use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text, tool_call};

fn host(places: &Places, stub: &Stub) -> AcpHost {
    std::fs::write(places.home_path().join("config.toml"),
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"workspace\"\nmode = \"default\"\n").unwrap();
    AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .spawn_conversing(),
    )
}
fn steer(host: &mut AcpHost, session: &str) -> serde_json::Value {
    host.request(
        "_ego/steer",
        serde_json::json!({"v":1,"sessionId":session,
        "prompt":[{"type":"text","text":"Prioritize the steering instruction"}]}),
    )
}
fn start_tool(host: &mut AcpHost, session: &str) -> (u64, serde_json::Value) {
    let id = host.send_request(
        "session/prompt",
        serde_json::json!({"sessionId":session,
        "prompt":[{"type":"text","text":"Run ls"}]}),
    );
    loop {
        let frame = host.read_frame();
        assert_ne!(frame["id"], id, "turn ended before permission: {frame}");
        if frame["method"] == "session/request_permission" {
            return (id, frame);
        }
    }
}
fn finish_turn(host: &mut AcpHost, id: u64) -> Vec<serde_json::Value> {
    let mut frames = Vec::new();
    loop {
        let frame = host.read_frame();
        if frame["id"] == id {
            assert!(frame.get("result").is_some(), "{frame}");
            return frames;
        }
        frames.push(frame);
    }
}
fn tool_stub() -> Stub {
    Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("steering-exec", "exec", r#"{"argv":["ls"]}"#)),
            Reply::Sse(text("finished")),
        ],
        ..Plan::default()
    })
}

// Catches: steering is held until the running tool batch's turn ends.
#[test]
fn steer_during_tool_batch_reaches_next_provider_request() {
    let places = Places::new();
    let stub = tool_stub();
    let mut host = host(&places, &stub);
    let initialized = host.initialize();
    assert_eq!(
        initialized["result"]["agentCapabilities"]["_meta"]["ego"]["steer"]["method"],
        "_ego/steer"
    );
    let session = host.new_session(places.workspace_path().to_str().unwrap());
    let (id, permission) = start_tool(&mut host, &session);
    assert_eq!(steer(&mut host, &session)["result"]["state"], "accepted");
    host.write_frame(serde_json::json!({"jsonrpc":"2.0","id":permission["id"],
        "result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}));
    let frames = finish_turn(&mut host, id);
    assert!(
        frames
            .iter()
            .any(
                |frame| frame["params"]["update"]["sessionUpdate"] == "user_message_chunk"
                    && frame["params"]["update"]["content"]["text"]
                        == "Prioritize the steering instruction"
            )
    );
    let requests = stub.requests();
    let generations: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/chat/completions"))
        .collect();
    assert_eq!(generations.len(), 2);
    let next: serde_json::Value = serde_json::from_str(&generations[1].body).unwrap();
    assert!(
        next["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "user"
                && message["content"] == "Prioritize the steering instruction")
    );
    host.finish();
}

// Catches: idle steering starts an extra turn and the host's fallback duplicates it.
#[test]
fn idle_steer_returns_not_busy_without_starting_turn() {
    let places = Places::new();
    let stub = tool_stub();
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().unwrap());
    assert_eq!(steer(&mut host, &session)["result"]["state"], "not-busy");
    assert!(
        !stub
            .requests()
            .iter()
            .any(|request| request.path.ends_with("/chat/completions"))
    );
    host.finish();
}

// Catches: cancellation discards steering after acknowledging durable acceptance.
#[test]
fn cancel_after_steer_preserves_pending_text_in_durable_journal() {
    let places = Places::new();
    let stub = tool_stub();
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().unwrap());
    let (id, _) = start_tool(&mut host, &session);
    assert_eq!(steer(&mut host, &session)["result"]["state"], "accepted");
    host.write_frame(serde_json::json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}));
    finish_turn(&mut host, id);
    host.finish();
    let journal = std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(&session)
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(
        journal.lines().any(|line| line.contains("PromptQueued")
            && line.contains("Prioritize the steering instruction")),
        "{journal}"
    );
    assert!(
        !journal.lines().any(|line| line.contains("UserPrompt")
            && line.contains("Prioritize the steering instruction")),
        "cancelled input was consumed: {journal}"
    );
}
