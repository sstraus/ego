use super::*;

// Catches: inspections sit in the ordinary host queue until the provider finishes.
#[test]
fn read_only_slash_inspections_answer_before_a_held_turn_ends() {
    let (reply, gate) = gated_paced_line("INSPECTION-TURN-HELD\n");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# inspection fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"hold this turn\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "INSPECTION-TURN-HELD");
    drain_until_quiet(&reader, &mut output);

    // The gate is released only after all three answers: completing the turn
    // cannot accidentally satisfy the inspection assertions.
    for (command, answer) in [
        ("/tasks\r", "background tasks: none"),
        ("/config\r", "Configuration (startup snapshot;"),
        ("/usage\r", "usage as of"),
    ] {
        let start = output.len();
        input.write_all(command.as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized_without_new_generation(&reader, &mut output, start, answer, &stub, 1);
        drain_until_quiet(&reader, &mut output);
    }
    gate.release();
    let start = output.len();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        stub.generations().len(),
        1,
        "inspections never start a generation"
    );
}

// Catches: the composer submits an unknown slash or its absolute-path escape as a command.
#[test]
fn unknown_slash_is_local_and_leading_space_sends_a_literal_path() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("LITERAL-PATH-RECEIVED"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# unknown slash fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"/histor\r").unwrap();
    input.flush().unwrap();
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        start,
        "Unknown slash command",
        &stub,
        0,
    );
    until_normalized(&reader, &mut output, start, "/history");
    drain_until_quiet(&reader, &mut output);
    let start = output.len();
    input.write_all(b" /tmp/x\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "LITERAL-PATH-RECEIVED");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let requests = stub.generations();
    assert_eq!(requests.len(), 1);
    assert_eq!(count_exact_strings(&requests[0]["messages"], " /tmp/x"), 1);
}
