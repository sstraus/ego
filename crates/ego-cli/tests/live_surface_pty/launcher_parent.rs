use super::*;

// Catches: ego run as a launcher's child (inherited terminal, parent in the same
// foreground process group, parent waiting) mishandles the terminal's SIGINT or
// loses its exit status, so the launcher sees a wrong code or never regains
// control. The parent traps SIGINT with a handler (reset on exec, as a launcher
// that waits does) and prints the child's status.
#[test]
fn a_waiting_launcher_parent_sees_the_interrupted_session_exit_status() {
    let (first, gate) = gated_paced_line("LAUNCHER-STREAMING");
    let stub = Stub::start(Plan {
        completions: vec![first],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# launcher parent fixture\n");

    let child = ego.pty_command();
    let mut launcher = portable_pty::CommandBuilder::new("/bin/sh");
    launcher.env_clear();
    for (key, value) in child.iter_extra_env_as_str() {
        launcher.env(key, value);
    }
    launcher.cwd(child.get_cwd().expect("fixture workspace"));
    launcher.args([
        "-c",
        r#"trap : INT; "$@"; echo "LAUNCHER-EXIT=$?""#,
        "launcher",
    ]);
    launcher.args(child.get_argv());

    let (session, mut input, reader) = PtySession::start_command(launcher, 24, 100, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"stream output\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "LAUNCHER-STREAMING");

    let start = output.len();
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "cancelling active work");
    gate.release();
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    let status = session.quit(input);

    assert_eq!(status.exit_code(), 0, "the launcher itself survives SIGINT");
    until_normalized(&reader, &mut output, start, "LAUNCHER-EXIT=130");
}
