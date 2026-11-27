use super::*;

// Catches: a wildcard exec allow rule lets the model run `ego roots add` without asking, so an
// agent widens its own perimeter silently; or the owner's answer no is not honoured.
#[test]
fn a_wildcard_allow_rule_does_not_silence_an_ego_admin_command() {
    let call = serde_json::json!({
        "argv": [env!("CARGO_BIN_EXE_ego"), "roots", "add", "/tmp"],
        "timeout_ms": 5000,
    })
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("admin-0", "exec", &call)),
            Reply::Sse(text(TOOL_TURN_DONE)),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--mode",
            "default",
            "--sandbox",
            "off",
        ])
        .env("TERM", "xterm-256color");
    ego.home_file("config.toml", "[permissions]\nallow = [\"exec(*)\"]\n");
    let mut session = modal_session_on(ego, stub);
    session.press(b"use the tools\r");
    session.wait("[d] details");
    let screen = session.screen().join("\n");
    assert!(
        screen.contains("changes ego's own permissions or configuration"),
        "the question does not say what the command changes:\n{screen}"
    );
    session.press(b"n");
    session.wait("What should it do instead?");
    session.press(b"leave the roots alone\r");
    session.wait(TOOL_TURN_DONE);
    let (generations, config) = session.finish();
    let result = generations[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the refused call is answered");
    assert!(!result.contains("Saved root"), "the command ran: {result}");
    assert!(!config.contains("roots"), "a root was saved: {config}");
}
