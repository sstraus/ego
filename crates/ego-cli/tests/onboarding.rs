//! process-boundary contract for regression case first-run model onboarding.
//!
//! These cases deliberately use only the public `ego` executable and the
//! existing loopback harness. Provider key values are synthetic and every
//! assertion also checks that they do not cross the diagnostic boundary.

mod support;

use std::fs;

use support::{Ego, Plan, Stub};

const DEFAULT_MODEL_MAP: &str = r#"
[default_model]
openai = "openai/gpt-6-sol"
anthropic = "anthropic/claude-opus-5-5"
"#;

const OPENAI_KEY: &str = "fixture-openai-key";
const ANTHROPIC_KEY: &str = "fixture-anthropic-key";

fn combined(result: &support::Finished) -> String {
    format!("{}\n{}", result.stdout, result.stderr)
}

fn assert_no_secret(result: &support::Finished, secret: &str) {
    assert!(
        !combined(result).contains(secret),
        "credential value crossed the CLI output boundary"
    );
}

fn pty_output(result: &support::Finished) -> String {
    format!("{}\n{}", result.stdout, result.stderr)
}

fn assert_headless_refusal_without_config(result: &support::Finished, providers: &[&str]) {
    assert_ne!(result.code, Some(0), "headless startup unexpectedly ran");
    let text = combined(result).to_ascii_lowercase();
    assert!(
        text.contains("model"),
        "refusal must name the missing model selection: {text}"
    );
    assert!(
        text.contains("--model"),
        "refusal must name the exact CLI remedy: {text}"
    );
    assert!(
        text.contains("config set model"),
        "refusal must name the exact persistent remedy: {text}"
    );
    for provider in providers {
        assert!(
            text.contains(provider),
            "refusal must list detected provider `{provider}`: {text}"
        );
    }
}

#[test]
fn headless_missing_model_with_zero_usable_providers_is_actionable_and_read_only() {
    let ego = Ego::nowhere();
    let config = ego.home_path().join("config.toml");
    let result = ego
        .args(["--sandbox", "off", "--output", "json", "hello"])
        .run();

    assert_headless_refusal_without_config(&result, &[]);
    assert!(
        !config.exists(),
        "headless refusal persisted a model selection"
    );
}

#[test]
fn headless_missing_model_with_one_usable_environment_provider_lists_only_the_remedy() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .env("OPENAI_API_KEY", OPENAI_KEY);
    ego.home_file("models.toml", DEFAULT_MODEL_MAP);
    let config = ego.home_path().join("config.toml");
    let result = ego
        .args(["--sandbox", "off", "--output", "json", "hello"])
        .run();

    assert_headless_refusal_without_config(&result, &["openai"]);
    assert!(
        !config.exists(),
        "headless refusal persisted a model selection"
    );
    assert_no_secret(&result, OPENAI_KEY);
    assert!(
        !combined(&result).to_ascii_lowercase().contains("anthropic"),
        "one-provider refusal invented an unavailable provider"
    );
}

#[test]
fn headless_missing_model_with_two_usable_environment_providers_lists_both_and_does_not_choose() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .env("OPENAI_API_KEY", OPENAI_KEY)
        .env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    ego.home_file("models.toml", DEFAULT_MODEL_MAP);
    let config = ego.home_path().join("config.toml");
    let result = ego
        .args(["--sandbox", "off", "--output", "json", "hello"])
        .run();

    assert_headless_refusal_without_config(&result, &["openai", "anthropic"]);
    assert!(
        !config.exists(),
        "headless refusal persisted a model selection"
    );
    assert_no_secret(&result, OPENAI_KEY);
    assert_no_secret(&result, ANTHROPIC_KEY);
    assert!(
        stub.generations().is_empty(),
        "headless missing-model refusal started a generation"
    );
}

#[cfg(unix)]
fn credential_command_fixture(ego: &Ego) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let marker = ego.home_path().join("credential-command-invoked");
    let script = ego.home_file(
        "credential-command.sh",
        &format!(
            "#!/bin/sh\ntouch '{}'\nprintf 'not-a-credential'\n",
            marker.display()
        ),
    );
    let mut permissions = fs::metadata(&script)
        .expect("credential fixture")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("credential fixture executable");
    (script, marker)
}

#[cfg(unix)]
#[test]
fn explicit_and_persisted_model_selection_skip_credential_commands_during_observation() {
    let stub = Stub::start(Plan::default());

    let explicit = Ego::against(&stub).env("OLLAMA_HOST", &stub.address);
    let (script, marker) = credential_command_fixture(&explicit);
    explicit.home_file(
        "config.toml",
        &format!(
            "[auth.anthropic]\ncredential_command = '{}'\n",
            script.display()
        ),
    );
    let explicit_result = explicit
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--output",
            "json",
            "hello",
        ])
        .run();
    assert_no_secret(&explicit_result, "not-a-credential");
    assert!(
        !marker.exists(),
        "explicit model startup probed an unrelated credential command"
    );

    let persisted = Ego::against(&stub).env("OLLAMA_HOST", &stub.address);
    let (script, marker) = credential_command_fixture(&persisted);
    persisted.home_file(
        "config.toml",
        &format!(
            "model = \"ollama/qwen3\"\n[auth.anthropic]\ncredential_command = '{}'\n",
            script.display()
        ),
    );
    let persisted_result = persisted
        .args(["--sandbox", "off", "--output", "json", "hello"])
        .run();
    assert_no_secret(&persisted_result, "not-a-credential");
    assert!(
        !marker.exists(),
        "persisted model startup probed an unrelated credential command"
    );
}

#[test]
fn interactive_single_provider_persists_the_explicit_default_before_generation() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    ego.home_file("models.toml", DEFAULT_MODEL_MAP);
    let config = ego.home_path().join("config.toml");
    let result = ego
        .conversing()
        .without_fixture_thinking()
        .args(["--sandbox", "off"])
        .run_pty("hello");

    assert_eq!(
        result.code,
        Some(0),
        "interactive run failed: {}",
        result.stdout
    );
    let config = fs::read_to_string(config).expect("auto-selection persisted atomically");
    assert!(config.contains("model = \"anthropic/claude-opus-5-5\""));
    assert!(
        !config.contains("thinking"),
        "automatic model selection must not persist a derived reasoning default"
    );
    assert_eq!(
        stub.generations().len(),
        1,
        "selection was not persisted before run"
    );
    assert_no_secret(&result, ANTHROPIC_KEY);
}

#[cfg(unix)]
#[test]
fn interactive_default_persistence_failure_blocks_generation() {
    use std::os::unix::fs::PermissionsExt;

    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    ego.home_file("models.toml", DEFAULT_MODEL_MAP);
    let config = ego.home_file("config.toml", "");
    let mut permissions = fs::metadata(ego.home_path()).expect("home").permissions();
    permissions.set_mode(0o500);
    fs::set_permissions(ego.home_path(), permissions).expect("read-only home");
    let result = ego
        .conversing()
        .args(["--sandbox", "off"])
        .run_pty_to_exit();

    assert_ne!(
        result.code,
        Some(0),
        "persistence failure started a session"
    );
    let output = pty_output(&result).to_ascii_lowercase();
    assert!(
        output.contains("config") || output.contains("persist"),
        "persistence refusal was not reported: {}",
        pty_output(&result)
    );
    assert_eq!(
        fs::read_to_string(config).expect("config remains readable"),
        ""
    );
    assert!(stub.generations().is_empty());
    assert_no_secret(&result, ANTHROPIC_KEY);
}

#[test]
fn startup_banner_names_ego_version_model_and_help_unless_quiet() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--thinking",
        "off",
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# trust ordering fixture\n");
    let normal = ego.run_pty("hello");
    let normal_text = normal.stdout.to_ascii_lowercase();
    assert!(normal_text.contains("ego"));
    assert!(normal_text.contains("model"));
    assert!(normal_text.contains("qwen3"));
    assert!(normal_text.contains("/help"));
    let banner = normal_text
        .find(concat!("ego ", env!("CARGO_PKG_VERSION")))
        .expect("banner identity");
    let trust = normal_text
        .find("trust project files")
        .expect("first-run trust question");
    assert!(
        banner < trust,
        "the program must identify itself before requesting trust: {normal_text}"
    );

    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--thinking",
        "off",
        "--sandbox",
        "off",
        "--quiet",
    ]);
    ego.file("AGENTS.md", "# quiet trust ordering fixture\n");
    let quiet = ego.run_pty("hello");
    let quiet_text = quiet.stdout.to_ascii_lowercase();
    assert!(
        !quiet_text.contains("/help"),
        "quiet startup printed the banner"
    );
}

#[test]
fn startup_without_default_reaches_the_shared_picker_for_zero_multiple_and_unmapped_rows() {
    let multiple_stub = Stub::start(Plan::default());
    let multiple = Ego::against(&multiple_stub)
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .env("OPENAI_API_KEY", OPENAI_KEY)
        .env("ANTHROPIC_API_KEY", ANTHROPIC_KEY);
    multiple.home_file("models.toml", DEFAULT_MODEL_MAP);

    // The built-in OpenAI and Anthropic defaults are merged below user facts;
    // omitting a user key no longer removes either default. A live local
    // Ollama row with no user models.toml is therefore the true sole-usable,
    // no-default case that must open the picker.
    let unmapped_stub = Stub::start(Plan::default());
    let unmapped = Ego::against(&unmapped_stub).env("OLLAMA_HOST", &unmapped_stub.address);

    for (label, ego) in [
        ("zero", Ego::nowhere()),
        ("multiple", multiple),
        ("unmapped", unmapped),
    ] {
        let result = ego
            .conversing()
            .args(["--sandbox", "off"])
            .run_pty_until("Select model");
        let output = pty_output(&result);
        assert!(
            output.contains("Select model") || output.contains("select model"),
            "{label} startup did not reach the shared model picker: {}",
            output
        );
    }
}
