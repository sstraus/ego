use super::*;

// Catches: draining a long output backlog monopolizes the terminal owner and
// prevents a concurrently typed draft from being echoed within one input poll.
#[test]
fn typing_during_long_output_echoes_within_500ms() {
    let answer: String = (0..2000)
        .map(|row| format!("OUTPUT-DIP-ROW-{row:04}\n"))
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
    until_normalized(&reader, &mut output, answer_start, "OUTPUT-DIP-ROW-0000");
    let draft_start = output.len();
    let typed = Instant::now();
    input.write_all(b"DRAFTFIXRESPONSIVE").unwrap();
    input.flush().unwrap();
    // PTY reads can coalesce a complete draft frame with a later output erase.
    // Observe the completed frame, not the screen at an arbitrary read boundary.
    let echoed = until_composer_frame(&reader, &mut output, draft_start, |rows| {
        rows.iter().any(|row| row.contains("> DRAFTFIXRESPONSIVE"))
    });
    let elapsed = typed.elapsed();
    assert!(echoed.join("\n").contains("DRAFTFIXRESPONSIVE"));
    gate.release();
    until_normalized(&reader, &mut output, answer_start, "OUTPUT-DIP-ROW-1999");
    until_mcp_composer_is_idle(&reader, &mut output);
    let cleared = output.len();
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_composer_frame(&reader, &mut output, cleared, |rows| {
        rows.iter()
            .rfind(|row| !row.trim().is_empty())
            .is_some_and(|row| row.trim() == ">")
            && !rows.iter().any(|row| row.contains("Esc to interrupt"))
    });
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

/// Observe completed painter frames, independent of PTY chunk boundaries.
/// Feed each byte to the emulator once; never reparse the growing output backlog.
fn until_composer_frame(
    reader: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    ready: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let mut parser = vt100::Parser::new(40, 120, 0);
    parser.process(&output[..start]);
    let mut scan_from = start;
    loop {
        let end = output.len();
        let mut parsed = scan_from;
        for index in scan_from..end {
            if output[..=index].ends_with(SHOW_CURSOR) {
                parser.process(&output[parsed..=index]);
                parsed = index + 1;
                let rows: Vec<_> = parser
                    .screen()
                    .contents()
                    .lines()
                    .map(str::to_owned)
                    .collect();
                if ready(&rows) {
                    return rows;
                }
            }
        }
        parser.process(&output[parsed..end]);
        scan_from = end;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let chunk = reader.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "composer frame wait failed: {error}; screen: {:?}; output: {:?}",
                parser.screen().contents(),
                String::from_utf8_lossy(&output[start..])
            )
        });
        output.extend(chunk);
    }
}

// Catches: an old echo satisfies the observer, a split frame is missed, or one
// PTY read coalesces a visible draft frame with its subsequent erase.
#[test]
fn draft_echo_observer_accepts_completed_frame_before_coalesced_erase() {
    // Recorded from rb repetition 36 in help-red-100.log; no synthetic terminal bytes.
    let captured = include_bytes!("../fixtures/help-latency-erased-draft.txt");
    for split in 0..=captured.len() {
        let (sender, receiver) = mpsc::channel();
        sender.send(captured[..split].to_vec()).unwrap();
        sender.send(captured[split..].to_vec()).unwrap();
        drop(sender);
        let mut output = captured.to_vec();
        let start = output.len();
        let rows = until_composer_frame(&receiver, &mut output, start, |rows| {
            rows.iter().any(|row| row.contains("> DRAFTFIXRESPONSIVE"))
        });
        assert!(
            rows.join("\n").contains("> DRAFTFIXRESPONSIVE"),
            "split {split}"
        );
        assert!(output.len() > start, "an old echo satisfied split {split}");
    }
}
