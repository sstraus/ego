use super::*;

// Catches: draining a long output backlog monopolizes the terminal owner and
// prevents a concurrently typed draft from being echoed within one input poll.
#[test]
fn typing_during_long_output_echoes_within_500ms() {
    let answer: String = (0..2000)
        .map(|row| format!("REGRESSION-MARKER-ROW-{row:04}\n"))
        .collect();
    let (reply, gate) = gated_paced_line(&answer);
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# concurrent output latency fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, start);
    let answer_start = output.len();
    input.write_all(b"print a long answer\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, answer_start, "REGRESSION-MARKER-ROW-0000");
    let draft_start = output.len();
    let typed = Instant::now();
    input.write_all(b"DRAFT574RESPONSIVE").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, draft_start, "DRAFT574RESPONSIVE");
    let elapsed = typed.elapsed();
    gate.release();
    until_normalized(&reader, &mut output, answer_start, "REGRESSION-MARKER-ROW-1999");
    until_mcp_composer_is_idle(&reader, &mut output);
    let cleared = output.len();
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, cleared);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(stub.generations().len(), 1, "draft submitted to provider");
    eprintln!("draft echo during 2000-row output: {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(500),
        "draft echo took {elapsed:?}: output pump delayed keyboard input"
    );
}

// Catches: one external message per 100 ms input poll turns local help into seconds of output.
#[test]
fn help_finishes_within_500ms_without_per_row_poll_delays() {
    let stub = Stub::start(Plan::default());
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# help latency fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, start);
    let start = output.len();
    let submitted = Instant::now();
    input.write_all(b"/help\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "twice in a row, exit");
    let elapsed = submitted.elapsed();
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(stub.generations().is_empty(), "help called the provider");
    eprintln!("complete /help block: {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(500),
        "help took {elapsed:?}: external output waited between rows"
    );
}
