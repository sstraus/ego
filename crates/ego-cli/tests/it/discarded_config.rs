//! Public diagnostics for configuration discarded by the untrusted posture.

use crate::support;

use support::{AcpHost, Ego, Plan, Reply, Stub, text};

fn ignored_config(ego: &Ego) {
    ego.file(
        ".ego/config.toml",
        "model = \"ollama/qwen3\"\nsandbox = \"ro\"\n",
    );
    ego.file(".ego/config.local.toml", "sandbox = \"workspace\"\n");
}

#[test]
fn headless_json_keeps_diagnostics_on_stderr_and_names_discarded_settings() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("ok"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    ignored_config(&ego);
    let finished = ego
        .args([
            "-m",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--output",
            "json",
            "hello",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert!(
        !finished.stdout.trim().is_empty(),
        "successful run emitted no JSON"
    );
    assert!(finished.stderr.contains("project"), "{}", finished.stderr);
    assert!(finished.stderr.contains("local"), "{}", finished.stderr);
    assert!(finished.stderr.contains("model"), "{}", finished.stderr);
    assert!(finished.stderr.contains("sandbox"), "{}", finished.stderr);
    for line in finished
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|error| panic!("stdout was not JSON ({error}): {line}"));
    }
}

#[test]
fn acp_startup_reports_discarded_workspace_settings_without_polluting_json_rpc_stdout() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    ignored_config(&ego);
    let workspace = ego.workspace_path().to_owned();
    let running = Ego::at(ego.home_path(), &workspace, Some(&stub))
        .subcommand("acp")
        .spawn_conversing();
    let mut host = AcpHost::adopt(running);
    let initialized = host.initialize();
    assert!(initialized.get("result").is_some(), "{initialized}");
    let stderr = host.finish();
    assert!(stderr.contains("project"), "{stderr}");
    assert!(stderr.contains("local"), "{stderr}");
    assert!(stderr.contains("sandbox"), "{stderr}");
}

#[test]
fn empty_project_and_local_layers_are_silent() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    ego.file(".ego/config.toml", "");
    ego.file(".ego/config.local.toml", "");
    let workspace = ego.workspace_path().to_owned();
    let running = Ego::at(ego.home_path(), &workspace, Some(&stub))
        .subcommand("acp")
        .spawn_conversing();
    let mut host = AcpHost::adopt(running);
    let initialized = host.initialize();
    assert!(initialized.get("result").is_some(), "{initialized}");
    let stderr = host.finish();
    assert!(!stderr.contains("configuration set"), "{stderr}");
    assert!(!stderr.contains("bare posture"), "{stderr}");
}

#[test]
fn untrusted_acp_startup_does_not_parse_ignored_malformed_layers() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    ego.file(".ego/config.toml", "sandbox = [\n");
    ego.file(".ego/config.local.toml", "this is not valid TOML = [\n");
    let workspace = ego.workspace_path().to_owned();
    let running = Ego::at(ego.home_path(), &workspace, Some(&stub))
        .subcommand("acp")
        .spawn_conversing();
    let mut host = AcpHost::adopt(running);
    let initialized = host.initialize();
    assert!(
        initialized.get("result").is_some(),
        "ignored malformed project/local config blocked ACP startup: {initialized}"
    );
    let stderr = host.finish();
    assert!(
        !stderr.contains("invalid"),
        "ignored config was parsed: {stderr}"
    );
    assert!(
        !stderr.contains("TOML"),
        "ignored config was parsed: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn untrusted_acp_startup_does_not_follow_ignored_nonregular_or_symlink_layers() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let config_dir = ego.workspace_path().join(".ego/config.toml");
    std::fs::create_dir_all(&config_dir).expect("a non-regular config path");
    let symlink_target = ego.home_file("ignored-config.toml", "sandbox = [\n");
    let local = ego.workspace_path().join(".ego/config.local.toml");
    std::os::unix::fs::symlink(&symlink_target, &local).expect("a symlinked config path");
    let workspace = ego.workspace_path().to_owned();
    let running = Ego::at(ego.home_path(), &workspace, Some(&stub))
        .subcommand("acp")
        .spawn_conversing();
    let mut host = AcpHost::adopt(running);
    let initialized = host.initialize();
    assert!(
        initialized.get("result").is_some(),
        "ignored non-regular/symlink config blocked ACP startup: {initialized}"
    );
    let stderr = host.finish();
    assert!(
        !stderr.contains("invalid"),
        "ignored config was parsed: {stderr}"
    );
    assert!(
        !stderr.contains("TOML"),
        "ignored config was parsed: {stderr}"
    );
}
