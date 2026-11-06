use super::*;

// Catches: the 551 collapse hides submitted lines or changes model input.
#[test]
fn long_prompt_echo_shows_every_line_in_the_live_terminal() {
    submitted_prompt_remains_in_terminal_history(160, false);
}

// Catches: editor viewport clipping drops the beginning of a prompt taller than the terminal.
#[test]
fn prompt_taller_than_terminal_keeps_every_line_in_scrollback() {
    submitted_prompt_remains_in_terminal_history(24, false);
}

// Catches: full-echo receipt queries CPR after initialization and loses the
// submission when the terminal withholds its reply, before provider dispatch.
#[test]
fn full_echo_without_cursor_replies_preserves_submission_and_scrollback() {
    for rows in [24, 160] {
        submitted_prompt_remains_in_terminal_history(rows, true);
    }
}

// Catches: zero-width accounting for pasted tabs anchors the next composer
// above the receipt's real final row and overwrites its trailing text.
#[test]
fn pasted_tab_receipt_survives_the_next_composer_without_cursor_replies() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("TAB-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    ego.file("AGENTS.md", "# tab receipt fixture\n");
    let replies = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (session, mut input, reader) = PtySession::start_command_with_cursor_replies(
        ego.pty_command(),
        160,
        40,
        Some((160, 40)),
        Some(Arc::clone(&replies)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    until_mcp_composer_is_idle(&reader, &mut output);
    replies.store(false, std::sync::atomic::Ordering::SeqCst);
    let prompt = format!("TAB-START{}TAB-RECEIPT-END", "\t".repeat(18));
    let start = output.len();
    input.write_all(b"\x1b[200~").unwrap();
    input.write_all(prompt.as_bytes()).unwrap();
    input.write_all(b"\x1b[201~\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "TAB-ANSWER");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(160, 40, 1_000);
    screen.process(&output);
    let visible = screen.screen().contents();
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        count_exact_strings(&stub.generations()[0]["messages"], &prompt),
        1
    );
    assert!(visible.contains("TAB-RECEIPT-END"), "{visible}");
}

fn submitted_prompt_remains_in_terminal_history(rows: u16, withhold_replies: bool) {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("PROMPT-ANSWER")),
            Reply::Sse(text("AFTER-RECEIPT-ANSWER")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    ego.file("AGENTS.md", "# prompt echo fixture\n");
    let replies = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (session, mut input, reader) = PtySession::start_command_with_cursor_replies(
        ego.pty_command(),
        rows,
        120,
        Some((rows, 120)),
        Some(Arc::clone(&replies)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    until_mcp_composer_is_idle(&reader, &mut output);
    if withhold_replies {
        replies.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    let prompt = (1..=130)
        .map(|n| {
            let line = format!("brief line {n:02}");
            if withhold_replies && n == 129 {
                format!("{line} {}", "あ".repeat(121))
            } else if withhold_replies && n == 130 {
                format!("{line}{}Z", "x".repeat(117 - line.len()))
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let start = output.len();
    input.write_all(b"\x1b[200~").unwrap();
    input.write_all(prompt.as_bytes()).unwrap();
    input.write_all(b"\x1b[201~\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "PROMPT-ANSWER");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(rows, 120, 1_000);
    screen.process(&output);
    screen.screen_mut().set_scrollback(usize::MAX);
    let depth = screen.screen().scrollback();
    let mut history = Vec::new();
    for offset in (1..=depth).rev() {
        screen.screen_mut().set_scrollback(offset);
        history.push(screen.screen().rows(0, 120).next().unwrap_or_default());
    }
    screen.screen_mut().set_scrollback(0);
    history.push(screen.screen().contents());
    let receipt = history.join("\n");
    if withhold_replies {
        assert!(
            receipt.contains(prompt.lines().last().unwrap()),
            "{receipt}"
        );
        let followup = output.len();
        input.write_all(b"after the full receipt\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, followup, "AFTER-RECEIPT-ANSWER");
    }
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(receipt.contains("> brief line 01"), "{receipt}");
    for n in 1..=130 {
        assert!(receipt.contains(&format!("brief line {n:02}")), "{receipt}");
    }
    assert!(!receipt.contains("more lines"), "{receipt}");
    assert!(!receipt.contains("Option+O or /expand"), "{receipt}");
    assert_eq!(
        count_exact_strings(&stub.generations()[0]["messages"], &prompt),
        1
    );
}
