//! Resume must retain the session's admitted selections, not current defaults.
use crate::support;
use support::{Ego, MODEL, Plan, Reply, Stub, text};

// Catches: plain resume replaces explicit launch mode, sandbox, and model
// with wider or different user defaults before re-admission.
#[test]
fn plain_resume_keeps_launch_selections_over_changed_user_defaults() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("first")), Reply::Sse(text("second"))],
        ..Plan::default()
    });
    let first = Ego::at(home.path(), workspace.path(), Some(&stub))
        .args([
            "--model",
            "ollama/qwen3",
            "--thinking",
            "off",
            "--mode",
            "default",
            "--sandbox",
            "workspace",
            "--session-id",
            "c2b477a4-14bc-4a31-9a9e-2147810e0632",
            "--session-dir",
            sessions.path().to_str().unwrap(),
            "--output",
            "json",
            "first",
        ])
        .run();
    assert_eq!(first.code, Some(0), "{}\n{}", first.stdout, first.stderr);
    std::fs::write(
        home.path().join("config.toml"),
        "mode='yolo'\nsandbox='off'\nmodel='missing-provider/changed-default'\n",
    )
    .unwrap();
    let resumed = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("resume")
        .args([
            "c2b477a4-14bc-4a31-9a9e-2147810e0632",
            "--session-dir",
            sessions.path().to_str().unwrap(),
            "--thinking",
            "off",
            "--output",
            "json",
            "second",
        ])
        .run();
    assert_eq!(
        resumed.code,
        Some(0),
        "{}\n{}",
        resumed.stdout,
        resumed.stderr
    );
    let requests = stub.generations();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["model"], MODEL);
    let request = requests[1].to_string();
    assert!(request.contains("permission mode: default"), "{request}");
    assert!(request.contains("sandbox: workspace"), "{request}");
    let overridden = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("resume")
        .args([
            "c2b477a4-14bc-4a31-9a9e-2147810e0632",
            "--session-dir",
            sessions.path().to_str().unwrap(),
            "--model",
            "ollama/qwen3",
            "--mode",
            "plan",
            "--sandbox",
            "ro",
            "--thinking",
            "off",
            "third",
        ])
        .run();
    assert_eq!(
        overridden.code,
        Some(0),
        "{}\n{}",
        overridden.stdout,
        overridden.stderr
    );
    let requests = stub.generations();
    let request = requests[2].to_string();
    assert!(request.contains("permission mode: plan"), "{request}");
    assert!(request.contains("sandbox: ro"), "{request}");
}
