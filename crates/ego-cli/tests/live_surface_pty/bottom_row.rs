use super::*;

// Catches: the idle composer or working status occupies TUIC's Compose overlay row.
#[test]
fn live_surface_keeps_bottom_row_empty_idle_and_working() {
    let mut failures = Vec::new();
    for (rows, cols) in [(24, 80), (40, 120)] {
        let (reply, gate) = gated_paced_line(&format!(
            "{}BOTTOM-HELD\n",
            "A real streamed transcript row.\n".repeat(usize::from(rows) * 2)
        ));
        let stub = Stub::start(Plan {
            completions: vec![reply],
            ..Plan::default()
        });
        let ego = Ego::against(&stub)
            .conversing()
            .env("TERM", "xterm-256color")
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--mode",
                "yolo",
            ]);
        ego.file("AGENTS.md", "# bottom row fixture\n");
        let (session, mut input, reader) = PtySession::start(&ego, rows, cols, Some((rows, cols)));
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        input.write_all(b"yes\r").unwrap();
        input.flush().unwrap();
        until_post_trust_prompt(&reader, &mut output, 0);
        // Fill the real terminal before checking idle, rather than assume startup
        // has already placed the composer at the bottom.
        let mark = output.len();
        input.write_all(b"/help\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, mark, "Retry a waiting provider now");
        until_local_action_completed(&reader, &mut output, mark, "help", PROGRESS_DEADLINE);
        drain_until_quiet(&reader, &mut output);
        snapshot(&output, rows, cols, "idle", &mut failures);
        input.write_all(b"exercise the bottom row\r").unwrap();
        input.flush().unwrap();
        until(&reader, &mut output, b"BOTTOM-HELD");
        drain_until_quiet(&reader, &mut output);
        snapshot(&output, rows, cols, "working", &mut failures);
        let mark = output.len();
        gate.release();
        until_idle_status_ready(&reader, &mut output, mark);
        drain_until_quiet(&reader, &mut output);
        snapshot(&output, rows, cols, "completed", &mut failures);
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn snapshot(output: &[u8], rows: u16, cols: u16, phase: &str, failures: &mut Vec<String>) {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(output);
    let screen = parser.screen();
    let bottom = screen.rows(0, cols).last().unwrap_or_default();
    let cursor = screen.cursor_position();
    let visible = screen.contents();
    assert!(
        visible.contains("> ") || visible.lines().any(|row| row.trim() == ">"),
        "{cols}x{rows} {phase}: missing real composer: {visible:?}"
    );
    if !bottom.trim().is_empty() || cursor.0 >= rows - 1 {
        failures.push(format!(
            "{cols}x{rows} {phase}: last row={bottom:?}, cursor={cursor:?}"
        ));
    }
    println!(
        "BOTTOM-MARK {cols}x{rows} {phase}: cursor={cursor:?}; last row={bottom:?}; grid={visible:?}"
    );
    if let Some(dir) = std::env::var_os("EGO_BOTTOM_ROW_CAPTURE_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("ego-{cols}x{rows}-{phase}.raw")), output).unwrap();
        std::fs::write(
            dir.join(format!("ego-{cols}x{rows}-{phase}.txt")),
            format!("cursor (zero-based row,col): {cursor:?}\n{visible}\n"),
        )
        .unwrap();
    }
}
