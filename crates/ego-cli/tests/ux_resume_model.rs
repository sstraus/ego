#![cfg(unix)]
mod support;
use support::{FIXTURE_SESSION_ID, Places, Plan, Stub};

// Catches: an explicit model override replaces the saved model without a notice.
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
        .args([
            FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
        ])
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

// Catches: the model-change notice prints when the resumed model is unchanged.
#[test]
fn resume_same_model_does_not_announce_a_change() {
    let stub = Stub::start(Plan::default());
    let places = Places::new();
    let original = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--session-id",
            FIXTURE_SESSION_ID,
            "first",
        ])
        .run();
    assert_eq!(original.code, Some(0), "{}", original.stderr);
    let resumed = places
        .ego(&stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "--sandbox", "off"])
        .run_pty("next");
    assert_eq!(resumed.code, Some(0), "{}", resumed.stdout);
    assert!(
        !resumed.stdout.contains("Resume model changed:"),
        "{}",
        resumed.stdout
    );
}

// Catches: --quiet leaks a model-change notice even when the model changes.
#[test]
fn resume_quiet_does_not_announce_a_model_change() {
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
        .args([
            FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--quiet",
        ])
        .run_pty("next");
    assert_eq!(resumed.code, Some(0), "{}", resumed.stdout);
    assert!(
        !resumed.stdout.contains("Resume model changed:"),
        "{}",
        resumed.stdout
    );
}
