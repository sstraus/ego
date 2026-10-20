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
    // The marker is plain ASCII in the composer. Re-normalizing the growing
    // 2000-row backlog after each chunk measures the observer, not draft echo.
    until_ascii_echo(&reader, &mut output, draft_start, b"DRAFT574RESPONSIVE");
    let elapsed = typed.elapsed();
    assert!(
        screen_rows(&output, 40, 120)
            .join("\n")
            .contains("DRAFT574RESPONSIVE"),
        "the observed bytes must be visible in the composer"
    );
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

/// Scan each new chunk once, retaining only the overlap needed for a split echo.
fn until_ascii_echo(
    reader: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &[u8],
) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let mut scan_from = start;
    loop {
        if output[scan_from..]
            .windows(needle.len())
            .any(|bytes| bytes == needle)
        {
            return;
        }
        scan_from = output.len().saturating_sub(needle.len() - 1).max(start);
        let remaining = deadline.saturating_duration_since(Instant::now());
        let chunk = reader.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "ASCII echo wait failed: {error}; output: {:?}",
                normalized(&output[start..])
            )
        });
        output.extend(chunk);
    }
}

// Catches: the linear observer accepts an old echo or misses one split across reads.
#[test]
fn ascii_echo_wait_ignores_old_output_and_accepts_a_split_marker() {
    let (sender, receiver) = mpsc::channel();
    let mut output = b"DRAFT574RESPONSIVE old echo\n".to_vec();
    let start = output.len();
    sender.send(b"DRAFT574RESP".to_vec()).unwrap();
    sender.send(b"ONSIVE".to_vec()).unwrap();
    until_ascii_echo(&receiver, &mut output, start, b"DRAFT574RESPONSIVE");
    assert_eq!(&output[start..], b"DRAFT574RESPONSIVE");
}
