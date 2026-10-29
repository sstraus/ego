//! Host attestation is an ego protocol contract, not client-supplied policy.
mod support;
use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text};

fn host(places: &Places, stub: &Stub, profile: bool) -> AcpHost {
    let ego = Ego::at(places.home_path(), places.workspace_path(), Some(stub)).subcommand("acp");
    let ego = if profile {
        ego.args(["--profile", "host"])
    } else {
        ego
    };
    let mut host = AcpHost::adopt(ego.spawn_conversing());
    host.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": 1, "clientCapabilities": {"elicitation": {"form": {}}}
        }),
    );
    host
}

fn setup() -> (Places, Stub) {
    let places = Places::new();
    std::fs::write(places.home_path().join("config.toml"),
        "model='ollama/qwen3'\nthinking='off'\nsandbox='off'\n[profile.host]\nmodel='ollama/qwen3'\nthinking='off'\nsandbox='off'\n").unwrap();
    std::fs::write(
        places.workspace_path().join("AGENTS.md"),
        "HOST-TRUSTED-INSTRUCTIONS-OUR\n",
    )
    .unwrap();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("done"))],
        ..Plan::default()
    });
    (places, stub)
}

fn session(host: &mut AcpHost, places: &Places) -> String {
    let response = host.request(
        "session/new",
        serde_json::json!({
            "cwd": places.workspace_path(), "mcpServers": [], "_meta": {"ego": {"trust": "trusted"}}
        }),
    );
    response["result"]["sessionId"].as_str().unwrap().to_owned()
}

// Catches a host-selected profile still eliciting trust and omitting AGENTS.md.
#[test]
fn selected_profile_attestation_loads_instructions_without_elicitation() {
    let (places, stub) = setup();
    let mut host = host(&places, &stub, true);
    let session = session(&mut host, &places);
    let prompt = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session, "prompt": [{"type": "text", "text": "hello"}]
        }),
    );
    loop {
        let frame = host.read_frame();
        assert_ne!(frame["method"], "session/elicitation", "{frame}");
        if frame["id"].as_u64() == Some(prompt) {
            assert!(frame.get("error").is_none(), "{frame}");
            break;
        }
    }
    assert!(
        stub.generations()
            .last()
            .unwrap()
            .to_string()
            .contains("HOST-TRUSTED-INSTRUCTIONS-OUR")
    );
    host.finish();
}

// Catches an arbitrary unprofiled ACP client granting itself workspace trust.
#[test]
fn unprofiled_attestation_is_ignored_and_the_form_is_used() {
    let (places, stub) = setup();
    let mut host = host(&places, &stub, false);
    let session = session(&mut host, &places);
    let prompt = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session, "prompt": [{"type": "text", "text": "hello"}]
        }),
    );
    let question = loop {
        let frame = host.read_frame();
        if frame.get("method").is_some() && frame.get("id").is_some() {
            break frame;
        }
        assert_ne!(
            frame["id"].as_u64(),
            Some(prompt),
            "prompt completed before asking: {frame}"
        );
    };
    assert!(
        question["params"]["message"]
            .as_str()
            .unwrap()
            .contains("Trust the workspace")
    );
    host.write_frame(serde_json::json!({"jsonrpc":"2.0", "id":question["id"],
        "result":{"action":"accept", "content":{"answer":"untrusted"}}}));
    loop {
        let frame = host.read_frame();
        if frame["id"].as_u64() == Some(prompt) {
            break;
        }
    }
    assert!(
        !stub
            .generations()
            .last()
            .unwrap()
            .to_string()
            .contains("HOST-TRUSTED-INSTRUCTIONS-OUR")
    );
    host.finish();
}
