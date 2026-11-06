use crate::support;
use support::{Ego, FIXTURE_SESSION_ID, Plan, Stub};

// Catches: the model has no ego session identity and substitutes a host PTY id.
#[test]
fn provider_runtime_context_exposes_the_durable_ego_session_id() {
    let stub = Stub::start(Plan::default());
    let result = Ego::against(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--session-id",
            FIXTURE_SESSION_ID,
            "Which session?",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let generation = stub.generations().remove(0);
    let messages = generation["messages"].to_string();
    assert!(
        messages.contains(&format!("Ego session ID: {FIXTURE_SESSION_ID}")),
        "{messages}"
    );
}
