//! Settlement seen by a real ACP v1 stdio host of the packaged binary.

mod support;

use ego_cli::auth::{AuthEntry, AuthStore};
use support::{AcpHost, Ego, Places, Plan, ReleaseGate, Reply, Stub, paced, tool_call};

const HOLD_REQUEST: &str = "00000000-0000-0000-0000-000000000154";

fn host(places: &Places, stub: &Stub) -> AcpHost {
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"workspace\"\n",
    )
    .expect("isolated ACP configuration");
    AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .spawn_conversing(),
    )
}

fn prompt(session: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": "Run ls"}],
    })
}

#[test]
fn retired_configured_model_is_a_single_actionable_pre_send_error() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    AuthStore::new(places.home_path())
        .put(
            "openai-codex",
            AuthEntry::OAuth {
                access: "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig".into(),
                refresh: None,
                expires: None,
            },
        )
        .expect("isolated test credential");
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = \"openai-codex/gpt-5.6-sol\"\nsandbox = \"workspace\"\n",
    )
    .expect("isolated ACP configuration");
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let answer = host.request("session/prompt", prompt(&session));
    let message = answer["error"]["message"]
        .as_str()
        .expect("ACP error message");
    assert!(message.contains("configuration error"), "{answer}");
    assert!(message.contains("retired"), "{answer}");
    assert!(message.contains("openai-codex/gpt-6-sol"), "{answer}");
    assert!(!message.contains("uncertain"), "{answer}");
    let detail = answer["error"]["data"].as_str().unwrap_or("");
    assert_eq!(
        format!("{message} {detail}")
            .matches("openai-codex/gpt-5.6-sol")
            .count(),
        1,
        "retirement detail repeated: {answer}"
    );
    assert!(
        stub.requests()
            .iter()
            .all(|request| request.path == "/api/tags"),
        "model refusal reached generation: {:?}",
        stub.requests()
    );
    host.finish();
}

#[test]
fn unknown_configured_model_is_a_configuration_error_without_recovery() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = \"openai/no-such-model\"\nsandbox = \"workspace\"\n",
    )
    .expect("isolated ACP configuration");
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .env("OPENAI_API_KEY", "sk-test")
            .subcommand("acp")
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let answer = host.request("session/prompt", prompt(&session));
    let message = answer["error"]["message"]
        .as_str()
        .expect("ACP error message");
    assert!(message.contains("configuration error"), "{answer}");
    assert!(
        message.contains("no capabilities are configured"),
        "{answer}"
    );
    assert!(!message.contains("uncertain"), "{answer}");
    assert!(answer["error"].get("data").is_none(), "{answer}");
    assert!(
        stub.requests()
            .iter()
            .all(|request| request.path == "/api/tags"),
        "unknown model reached generation: {:?}",
        stub.requests()
    );
    host.finish();
}

#[test]
fn cancelling_an_open_permission_settles_the_packaged_prompt_as_cancelled() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(tool_call(
            "permission-call",
            "exec",
            r#"{"argv":["ls"]}"#,
        ))],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let prompt_id = host.send_request("session/prompt", prompt(&session));
    let permission = loop {
        let frame = host.read_frame();
        if frame["method"] == "session/request_permission" {
            break frame;
        }
    };
    host.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session},
    }));
    host.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "id": permission["id"],
        "result": {"outcome": {"outcome": "cancelled"}},
    }));
    let answer = loop {
        let frame = host.read_frame();
        if frame["id"] == prompt_id {
            break frame;
        }
    };
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    host.finish();
}

#[test]
fn a_reconnecting_stdio_host_discovers_the_durable_hold_and_its_error_code() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let mut first = host(&places, &stub);
    first.initialize();
    let session = first.new_session(cwd);
    let paused = first.request(
        "_ego/pause",
        serde_json::json!({
            "v": 1, "sessionId": session, "requestId": HOLD_REQUEST,
        }),
    );
    assert_eq!(paused["result"]["state"], "paused", "{paused}");
    first.finish();

    let mut second = host(&places, &stub);
    second.initialize();
    let listed = second.request("session/list", serde_json::json!({"cwd": cwd}));
    let row = listed["result"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["sessionId"] == session)
        .expect("held row");
    assert_eq!(row["_meta"]["ego"]["hold"]["state"], "paused", "{listed}");
    let loaded = second.request(
        "session/load",
        serde_json::json!({
            "sessionId": session, "cwd": cwd, "mcpServers": [],
        }),
    );
    assert_eq!(
        loaded["result"]["_meta"]["ego"]["hold"]["state"], "paused",
        "{loaded}"
    );
    let refused = second.request("session/prompt", prompt(&session));
    assert_eq!(refused["error"]["code"], -32011, "{refused}");
    second.finish();
}

#[test]
fn pausing_a_streaming_stdio_turn_settles_with_the_held_code() {
    let places = Places::new();
    let Reply::Paced { head, tail, .. } = paced("before pause", std::time::Duration::ZERO) else {
        unreachable!("paced produces a split reply");
    };
    let gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::GatedPaced {
            head,
            tail,
            gate: gate.clone(),
        }],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let prompt_id = host.send_request("session/prompt", prompt(&session));
    loop {
        let frame = host.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            break;
        }
    }
    let pause_id = host.send_request(
        "_ego/pause",
        serde_json::json!({
            "v": 1, "sessionId": session, "requestId": HOLD_REQUEST,
        }),
    );
    let pending = loop {
        let frame = host.read_frame();
        if frame["id"] == pause_id {
            break frame;
        }
    };
    assert_eq!(pending["result"]["state"], "pending", "{pending}");
    gate.release();
    let answered = loop {
        let frame = host.read_frame();
        if frame["id"] == prompt_id {
            break frame;
        }
    };
    assert_eq!(answered["error"]["code"], -32011, "{answered}");
    let listed = host.request(
        "session/list",
        serde_json::json!({
            "cwd": places.workspace_path().to_str().expect("UTF-8 root"),
        }),
    );
    let row = listed["result"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["sessionId"] == session)
        .expect("paused row");
    assert_eq!(row["_meta"]["ego"]["hold"]["state"], "paused", "{listed}");
    host.finish();
}
