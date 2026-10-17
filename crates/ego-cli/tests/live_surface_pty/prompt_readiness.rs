use super::*;

// Catches: the first usable prompt leaves the host's previous busy/progress state latched.
#[test]
fn initial_ready_prompt_resets_stale_host_working_state() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .env("TERM", "xterm-256color")
        .env("TUIC_SOCKET", "/nonexistent/ego-readiness.sock");
    ego.file("AGENTS.md", "# Prompt readiness fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust");
    input.flush().expect("flush");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let ready = String::from_utf8_lossy(&output[start..]).into_owned();
    for command in [b"/help\r".as_slice(), b"/usage\r", b"/version\r"] {
        let start = output.len();
        input.write_all(command).expect("local command");
        input.flush().expect("flush");
        until_prompt_from(&reader, &mut output, start);
        drain_until_quiet(&reader, &mut output);
        let returned = String::from_utf8_lossy(&output[start..]);
        assert!(
            returned.contains("\x1b]7770;state=idle\x07"),
            "no idle after local command: {returned:?}"
        );
        assert!(
            returned.contains("\x1b]9;4;0;0\x07"),
            "no progress reset after local command: {returned:?}"
        );
    }
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("flush");
    session.quit(input);
    assert!(
        ready.contains("\x1b]7770;state=idle\x07"),
        "no idle at readiness: {ready:?}"
    );
    assert!(
        ready.contains("\x1b]9;4;0;0\x07"),
        "no progress reset: {ready:?}"
    );
    assert!(stub.generations().is_empty());
}
