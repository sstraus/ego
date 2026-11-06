use super::*;

// Catches: buffered prose and the command are not painted until a tool exits.
#[test]
fn prose_and_command_are_visible_while_exec_is_held() {
    let call = tool_call(
        "held-command",
        "exec",
        r#"{"argv":["sh","-c","while [ ! -f release-898 ]; do sleep 0.05; done"],"timeout_ms":30000}"#,
    );
    let prose = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"PROSE-BEFORE-898\"},\"finish_reason\":null}]}\n\n";
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(format!("{prose}{call}")),
            Reply::Sse(text("TOOL-RELEASE-ANSWER")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--mode",
            "yolo",
            "--sandbox",
            "off",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, Some((40, 160)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"run held command\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "release-898");
    drain_until_quiet(&reader, &mut output);
    let live = screen_rows(&output, 40, 160).join("\n");
    assert!(
        live.contains("PROSE-BEFORE-898"),
        "prose absent while held: {live}"
    );
    assert!(
        live.contains("release-898"),
        "command absent while held: {live}"
    );
    assert!(
        !live.contains("TOOL-RELEASE-ANSWER"),
        "tool already completed: {live}"
    );
    ego.file("release-898", "release");
    until_normalized(&reader, &mut output, 0, "TOOL-RELEASE-ANSWER");
    until_prompt_from(&reader, &mut output, 0);
    drain_until_quiet(&reader, &mut output);
    let settled = screen_rows(&output, 40, 160).join("\n");
    assert!(
        settled.contains("exit 0"),
        "completion outcome absent: {settled}"
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}

// Catches: cancellation leaves a second, stale running row beside the receipt.
#[test]
fn cancelled_held_command_has_no_stale_running_row() {
    let call = tool_call(
        "cancel-898",
        "exec",
        r#"{"argv":["sh","-c","while [ ! -f release-cancel-898 ]; do sleep 0.05; done"],"timeout_ms":30000}"#,
    );
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(call)],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--mode",
            "yolo",
            "--sandbox",
            "off",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, Some((40, 160)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"run held command\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "release-cancel-898");
    let cancel_start = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, cancel_start, "Interrupted by user");
    until_idle_status_ready(&reader, &mut output, cancel_start);
    drain_until_quiet(&reader, &mut output);
    let settled = screen_rows(&output, 40, 160).join("\n");
    assert_eq!(
        settled.matches("release-cancel-898").count(),
        1,
        "stale or missing command row: {settled}"
    );
    assert_eq!(
        settled.matches("Interrupted by user").count(),
        1,
        "interruption receipt: {settled}"
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    session.quit(input);
}
