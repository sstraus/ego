#![cfg(unix)]
mod support;
use support::{FIXTURE_SESSION_ID, Places, Plan, Stub};

// Catches: a configured default silently replaces the resumed session's recorded model.
#[test]
fn resume_announces_a_model_change_from_the_stored_session() {
    let stub = Stub::start(Plan::default());
    let places = Places::new();
    let original = places
        .ego(&stub)
        .env("ANTHROPIC_API_KEY", "sk-test")
        .args([
            "--model",
            "anthropic/claude-opus-5-5",
            "--thinking",
            "medium",
            "--sandbox",
            "off",
            "--session-id",
            FIXTURE_SESSION_ID,
            "first",
        ])
        .run();
    assert_eq!(original.code, Some(0), "{}", original.stderr);
    places
        .ego(&stub)
        .home_file("config.toml", "model='ollama/qwen3'\n");
    let resumed = places
        .ego(&stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "--sandbox", "off"])
        .run_pty("next");
    assert_eq!(resumed.code, Some(0), "{}", resumed.stdout);
    assert!(
        resumed
            .stdout
            .contains("Resume model changed: anthropic/claude-opus-5-5 -> ollama/qwen3"),
        "{}",
        resumed.stdout
    );
}
