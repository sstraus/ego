use super::*;

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
