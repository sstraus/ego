//! Configuration setup never uses a model or accepts a diagnostic prompt.
use crate::support;

use support::{Plan, Stub};

// Catches: headless setup starts provider inference or writes config without a human terminal.
#[test]
fn setup_without_terminal_refuses_without_provider_calls_or_config_writes() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let original = "model='ollama/qwen3'\nmode='plan'\n";
    std::fs::write(home.path().join("config.toml"), original).unwrap();
    let provider = Stub::start(Plan::default());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ego"))
        .env_clear()
        .envs(support::coverage_environment())
        .env("EGO_HOME", home.path())
        .env("HOME", home.path())
        .env("OLLAMA_HOST", &provider.address)
        .current_dir(workspace.path())
        .arg("setup")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("setup needs a terminal"));
    assert!(provider.generations().is_empty());
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        original
    );
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
}

// Catches: a setup prompt re-enables the retired model-selected probe flow.
#[test]
fn setup_refuses_a_diagnostic_prompt() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ego"))
        .args(["setup", "inspect work"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
}
