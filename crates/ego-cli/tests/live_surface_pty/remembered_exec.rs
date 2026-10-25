use super::*;

// Catches: a displayed program rule is saved but a later shell compound
// still asks, or the preview hides the authority until after saving it.
#[test]
fn remembered_git_status_runs_a_later_compound_without_another_prompt() {
    let first = r#"{"argv":["git","status","--short"],"timeout_ms":5000}"#;
    let second = r#"{"argv":["/bin/sh","-c","git status --porcelain; git status --short"],"timeout_ms":5000}"#;
    let mut session = modal_session_with(vec![
        Reply::Sse(tool_call("regression_case", "exec", first)),
        Reply::Sse(text("REGRESSION-MARKER-DONE")),
        Reply::Sse(tool_call("regression_case", "exec", second)),
        Reply::Sse(text("REGRESSION-MARKER-DONE")),
    ]);
    let init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(session.ego.workspace_path())
        .output()
        .expect("git fixture");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    session.press(b"first status\r");
    session.wait("[d] details");
    let preview = session.screen().join("\n");
    assert!(
        preview.contains("[a] yes, do not ask again for exec(git status *)"),
        "{preview}"
    );
    session.press(b"a");
    session.wait("REGRESSION-MARKER-DONE");
    session.press(b"status again\r");
    session.wait("REGRESSION-MARKER-DONE");
    let screen = session.screen().join("\n");
    assert!(
        !screen.contains("Allow run"),
        "a second permission was shown: {screen}"
    );
    let (generations, config) = session.finish();
    assert_eq!(generations.len(), 4);
    assert!(config.contains("exec(git status *)"), "{config}");
    assert!(!config.contains("exec(/bin/sh"), "{config}");
    let result = generations[3].to_string();
    assert!(!result.contains("permission refused"), "{result}");
}
