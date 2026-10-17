use super::*;

// Catches: a busy /mcp notice commits the live Working row and empty composer
// into scrollback, or uses a different prefix from an idle notice.
#[test]
fn busy_seat_notice_does_not_leave_stale_working_row() {
    let (reply, gate) = gated_paced_line("BUSY-NOTICE-RUNNING\n");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# busy notice fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, Some((24, 80)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"start held turn\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"BUSY-NOTICE-RUNNING");
    for command in ["mcp", "mcp", "tools"] {
        let start = output.len();
        input.write_all(format!("/{command}\r").as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, &format!("{command}:"));
        drain_until_quiet(&reader, &mut output);
        let mut screen = vt100::Parser::new(24, 80, 100);
        screen.process(&output);
        let visible = screen.screen().contents();
        screen.screen_mut().set_scrollback(usize::MAX);
        let depth = screen.screen().scrollback();
        let mut history = String::new();
        for offset in (1..=depth).rev() {
            screen.screen_mut().set_scrollback(offset);
            history.push_str(
                screen
                    .screen()
                    .contents()
                    .lines()
                    .next()
                    .unwrap_or_default(),
            );
            history.push('\n');
        }
        history.push_str(&visible);
        assert_eq!(history.matches("Working").count(), 1, "{history:?}");
        assert_eq!(
            history.lines().filter(|row| row.trim() == ">").count(),
            1,
            "{history:?}"
        );
        let notice = visible
            .lines()
            .position(|row| row.starts_with(&format!("εgo: {command}:")));
        let working = visible.lines().position(|row| row.contains("Working"));
        assert!(
            notice.is_some() && notice < working,
            "notice must precede the live footprint: {visible:?}"
        );
    }
    assert_eq!(
        stub.generations().len(),
        1,
        "local notices must not send a prompt"
    );
    gate.release();
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    session.quit(input);
}
