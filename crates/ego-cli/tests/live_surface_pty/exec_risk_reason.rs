use super::*;

// Catches: Risk: unknown with no reason on a command the shell read-only proof
// refuses for a known reason (git can run repository-configured commands, a
// search reads files the command does not name).
#[test]
fn default_mode_exec_prompt_says_why_git_status_and_rg_are_not_read_only() {
    let mut session = modal_session_with(vec![
        Reply::Sse(tool_call(
            "call-0",
            "exec",
            r#"{"argv":["git","status","--short","--branch"],"timeout_ms":5000}"#,
        )),
        Reply::Sse(tool_call(
            "call-1",
            "exec",
            r#"{"argv":["rg","-n","fixture","AGENTS.md"],"timeout_ms":5000}"#,
        )),
        Reply::Sse(text(TOOL_TURN_DONE)),
    ]);
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(session.ego.workspace_path())
        .status()
        .expect("git init");
    assert!(init.success(), "{init:?}");

    session.press(b"use the tools\r");
    session.wait("[d] details");
    let git = risk_row(&mut session, "Allow run git status --short --branch?");
    assert!(
        git.starts_with("Risk: unknown - ")
            && git.contains("not read-only: git may run repository-configured commands"),
        "{git}"
    );
    session.press(b"y");
    session.wait("Allow run rg");
    let rg = risk_row(&mut session, "Allow run rg -n fixture AGENTS.md?");
    assert!(
        rg.starts_with("Risk: unknown - ")
            && rg.contains("not read-only: the command does not name every file it searches"),
        "{rg}"
    );
    // Declined: the prompt is the subject, and rg need not be installed.
    session.press(b"n");
    session.wait("What should it do instead?");
    session.press(b"skip the search\r");
    session.wait(TOOL_TURN_DONE);

    let (generations, _) = session.finish();
    let status = generations.last().expect("a generation after the tools")["messages"]
        .as_array()
        .expect("provider messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the git status result");
    assert!(
        status.contains("## "),
        "git status ran in a real worktree: {status}"
    );
}

/// The complete wrapped risk reason below the headline, before the decision keys.
fn risk_row(session: &mut ModalSession, headline: &str) -> String {
    let screen = session.screen();
    let at = screen
        .iter()
        .rposition(|line| line.trim_end() == headline)
        .unwrap_or_else(|| panic!("no {headline:?} row: {screen:#?}"));
    let reason = screen[at + 1..]
        .iter()
        .take_while(|line| !line.trim().is_empty())
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        screen[at + 1..]
            .iter()
            .any(|line| line.contains("[d] details")),
        "decision keys disappeared: {screen:#?}"
    );
    reason
}
