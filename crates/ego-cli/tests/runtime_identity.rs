//! The model is told what it is and where it runs.
//!
//! Asked "which model are you?" over ACP in TUICommander, ego answered that it
//! was ChatGPT and could not see its model name; asked "which repo?", it ran
//! `pwd`. Every fact it lacked was already resolved by the harness: the agent
//! and its version, the model reference and effort, the workspace root, the
//! permission mode and sandbox, the host that connected, and the tools admitted.
//!
//! Each test drives the real binary against the provider stub and reads the
//! system text of the first request the provider received. The expected values
//! come from the command line, the config and the host frame the test itself
//! wrote, never from ego's own rendering.

mod support;

use support::{Ego, Plan, Stub};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MODEL: &str = "anthropic/claude-opus-5-5";

/// Every system-role text the provider was sent, joined.
///
/// Messages API only: `system` is a string or a list of text blocks.
fn system_text(body: &serde_json::Value) -> String {
    match &body["system"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn first_system_text(stub: &Stub, stderr: &str) -> String {
    let body = stub
        .generations()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no request reached the provider; stderr: {stderr}"));
    system_text(&body)
}

fn canonical_workspace(ego: &Ego) -> String {
    std::fs::canonicalize(ego.workspace_path())
        .expect("the workspace exists")
        .display()
        .to_string()
}

fn assert_contains(system: &str, needle: &str, what: &str) {
    assert!(
        system.contains(needle),
        "the system text does not carry {what} ({needle:?}):\n{system}"
    );
}

fn assert_builtin_tools(system: &str) {
    for tool in ["read", "glob", "grep", "write", "edit", "exec"] {
        assert_contains(system, &format!("`{tool}`"), "the admitted tool inventory");
    }
}

#[test]
fn a_headless_run_tells_the_model_its_identity_model_effort_workspace_and_posture() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let workspace = canonical_workspace(&ego);
    let finished = ego
        .without_fixture_thinking()
        .env("ANTHROPIC_API_KEY", "sk-test")
        .args([
            "--thinking",
            "medium",
            "--model",
            MODEL,
            "--mode",
            "plan",
            "--sandbox",
            "off",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "che modello sei e in che repo sei?",
        ])
        .run();

    let system = first_system_text(&stub, &finished.stderr);
    assert_contains(
        &system,
        &format!("ego {VERSION}"),
        "the agent and its version",
    );
    assert_contains(&system, MODEL, "the resolved model reference");
    assert_contains(&system, "effort: medium", "the reasoning effort");
    assert_contains(&system, &workspace, "the workspace root");
    assert_contains(&system, "permission mode: plan", "the permission mode");
    assert_contains(&system, "sandbox: off", "the sandbox");
    assert_contains(&system, "Host: none", "the absence of an ACP host");
    assert_contains(&system, "MCP servers: none", "the MCP inventory");
    assert_builtin_tools(&system);
}

#[test]
fn a_conversational_session_tells_the_model_the_same_facts() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let workspace = canonical_workspace(&ego);
    let finished = ego
        .conversing()
        .without_fixture_thinking()
        .env("ANTHROPIC_API_KEY", "sk-test")
        .args([
            "--thinking",
            "high",
            "--model",
            MODEL,
            "--sandbox",
            "off",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "che modello sei?",
        ])
        .run();

    let system = first_system_text(&stub, &finished.stderr);
    assert_contains(
        &system,
        &format!("ego {VERSION}"),
        "the agent and its version",
    );
    assert_contains(&system, MODEL, "the resolved model reference");
    assert_contains(&system, "effort: high", "the reasoning effort");
    assert_contains(&system, &workspace, "the workspace root");
    assert_contains(&system, "permission mode: default", "the permission mode");
    assert_builtin_tools(&system);
}

/// The identity is its own block: a `--system` prompt replaces the workspace
/// instructions, not the facts about the runtime.
#[test]
fn a_verbatim_system_prompt_does_not_erase_the_runtime_identity() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let finished = ego
        .without_fixture_thinking()
        .env("ANTHROPIC_API_KEY", "sk-test")
        .args([
            "--thinking",
            "low",
            "--model",
            MODEL,
            "--sandbox",
            "off",
            "--system",
            "Answer tersely.",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "hi",
        ])
        .run();

    let system = first_system_text(&stub, &finished.stderr);
    assert_contains(&system, "Answer tersely.", "the operator's system prompt");
    assert_contains(
        &system,
        &format!("ego {VERSION}"),
        "the agent and its version",
    );
    assert_contains(&system, MODEL, "the resolved model reference");
}

#[test]
fn an_acp_run_names_the_host_that_initialized_the_connection() {
    let stub = Stub::start(Plan {
        completions: vec![support::Reply::Sse(support::text("ok"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    ego.home_file(
        "config.toml",
        &format!("model = \"{MODEL}\"\nsandbox = \"off\"\nthinking = \"low\"\n"),
    );
    let workspace = canonical_workspace(&ego);
    let running = Ego::at(ego.home_path(), ego.workspace_path(), Some(&stub))
        .subcommand("acp")
        .env("ANTHROPIC_API_KEY", "test-key")
        .spawn_conversing();
    let mut host = support::AcpHost::adopt(running);

    let initialized = host.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {"name": "tuicommander", "version": "1.7.7"},
        }),
    );
    assert!(initialized.get("result").is_some(), "{initialized}");
    let cwd = ego.workspace_path().to_str().expect("utf-8").to_owned();
    let session = host.new_session(&cwd);
    let answered = host.request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": "che modello usi?"}],
        }),
    );
    let stderr = host.finish();
    assert_eq!(
        answered["result"]["stopReason"], "end_turn",
        "{answered} / stderr: {stderr}"
    );

    let system = first_system_text(&stub, &stderr);
    assert_contains(
        &system,
        &format!("ego {VERSION}"),
        "the agent and its version",
    );
    assert_contains(&system, MODEL, "the resolved model reference");
    assert_contains(&system, "effort: low", "the reasoning effort");
    assert_contains(&system, &workspace, "the workspace root");
    assert_contains(
        &system,
        "Host: tuicommander 1.7.7 over ACP",
        "the ACP client",
    );
    assert_builtin_tools(&system);
}
