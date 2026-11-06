//! The owner switches file checkpoints off with a key or a flag.

use crate::support;

use std::path::Path;
use std::process::Command;

use support::{Places, Plan, Reply, Stub, text, tool_call};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// One edit in a Git workspace; returns the checkpoint refs left behind.
fn edit_in_git_workspace(config: &str, extra: &[&str]) -> (String, String) {
    let places = Places::new();
    git(places.workspace_path(), &["init", "--quiet"]);
    places.ego(&Stub::start(Plan::default())).home_file(
        "config.toml",
        &format!("model = \"ollama/qwen3\"\nsandbox = \"off\"\n{config}"),
    );
    let path = places.file("price.txt", "apple = 1\n");
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "edit-price",
                "edit",
                r#"{"files":[{"path":"price.txt","chunks":[{"old_string":"apple = 1","new_string":"apple = 2"}]}]}"#,
            )),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let mut command = places
        .ego(&stub)
        .args(["--sandbox", "off", "--allow", "write(**)"]);
    for flag in extra {
        command = command.arg(flag);
    }
    let run = command.arg("Change the price").run();
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let refs = git(
        places.workspace_path(),
        &["for-each-ref", "refs/ego/checkpoints"],
    );
    (refs, std::fs::read_to_string(path).unwrap())
}

// Catches: the control, so a missing checkpoint below cannot come from a fixture that never
// reaches a Git worktree.
#[test]
fn an_edit_in_a_git_workspace_saves_a_checkpoint_by_default() {
    let (refs, content) = edit_in_git_workspace("", &[]);
    assert!(refs.contains("refs/ego/checkpoints/"), "{refs}");
    assert_eq!(content, "apple = 2\n");
}

// Catches: `checkpoints = false` is accepted and stored but the edit still writes a ref and
// objects into the user's repository.
#[test]
fn checkpoints_false_leaves_no_ref_and_the_edit_still_applies() {
    let (refs, content) = edit_in_git_workspace("checkpoints = false\n", &[]);
    assert_eq!(refs, "");
    assert_eq!(content, "apple = 2\n");
}

// Catches: the flag is parsed but never reaches the file adapter.
#[test]
fn the_no_checkpoints_flag_leaves_no_ref() {
    let (refs, content) = edit_in_git_workspace("", &["--no-checkpoints"]);
    assert_eq!(refs, "");
    assert_eq!(content, "apple = 2\n");
}
