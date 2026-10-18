use super::*;

// Catches template rows being hard-coded above the buffer, or footer repaint overwriting edits.
#[test]
fn status_template_positions_rows_around_editable_prompt_in_real_pty() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# status template fixture\n");
    ego.home_file("config.toml", "[ui.statusline]\ntemplate = '''TEMPLATE-ABOVE {{ model }}\n{{ prompt }}\nTEMPLATE-BELOW {{ effort }}'''\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, Some((24, 100)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "TEMPLATE-BELOW off");
    input.write_all(b"editable-probe").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "editable-probe");
    drain_until_quiet(&reader, &mut output);
    let mut parser = vt100::Parser::new(24, 100, 0);
    parser.process(&output);
    let screen = parser.screen();
    let rows = screen.rows(0, 100).collect::<Vec<_>>();
    let above = rows
        .iter()
        .position(|row| row.contains("TEMPLATE-ABOVE ollama/qwen3"))
        .unwrap();
    let prompt = rows
        .iter()
        .position(|row| row.starts_with("> editable-probe"))
        .unwrap();
    let below = rows
        .iter()
        .position(|row| row.contains("TEMPLATE-BELOW off"))
        .unwrap();
    assert_eq!((prompt, below), (above + 1, above + 2), "{rows:?}");
    assert_eq!(screen.cursor_position().0 as usize, prompt, "{rows:?}");
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(stub.generations().is_empty());
}

// Catches a footer as tall as the terminal painting over the draft and saved cursor.
#[test]
fn status_template_footer_does_not_overwrite_composer_at_terminal_height() {
    for (height, footer, expected_footer) in [
        (
            4,
            "FOOTER-ONE\nFOOTER-TWO\nFOOTER-THREE\nFOOTER-FOUR",
            vec!["FOOTER-ONE", "FOOTER-TWO", "FOOTER-THREE"],
        ),
        (
            4,
            "FOOTER-ONE\nFOOTER-TWO\nFOOTER-THREE",
            vec!["FOOTER-ONE", "FOOTER-TWO", "FOOTER-THREE"],
        ),
        (
            1,
            "FOOTER-ONE\nFOOTER-TWO\nFOOTER-THREE\nFOOTER-FOUR",
            vec![],
        ),
    ] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub)
            .conversing()
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--mode",
                "yolo",
            ])
            .env("TERM", "xterm-256color");
        ego.file("AGENTS.md", "# status template fixture\n");
        ego.home_file(
            "config.toml",
            &format!("[ui.statusline]\ntemplate = '''{{{{ prompt }}}}\n{footer}'''\n"),
        );
        let (session, mut input, reader) = PtySession::start(&ego, height, 80, Some((height, 80)));
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        let start = output.len();
        input.write_all(b"yes\r").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            start,
            if height > 1 { "FOOTER-ONE" } else { "> " },
        );
        input.write_all(b"editable-probe").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "editable-probe");
        drain_until_quiet(&reader, &mut output);
        let mut parser = vt100::Parser::new(height, 80, 0);
        parser.process(&output);
        let screen = parser.screen();
        let rows = screen.rows(0, 80).collect::<Vec<_>>();
        let cursor = screen.cursor_position();
        input.write_all(b"\x15/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        assert_eq!(rows[0], "> editable-probe", "height={height} {rows:?}");
        assert_eq!(cursor.0, 0, "height={height} {rows:?}");
        assert_eq!(&rows[1..], expected_footer, "height={height}");
        assert!(stub.generations().is_empty());
    }
}

// Catches diagnostics suppressed by the terminal adapter, duplicated on repaint,
// or reissued after a modal retires the notice for the same configuration.
#[test]
fn status_template_diagnostic_is_visible_once_across_live_repaints() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# diagnostic fixture\n");
    ego.home_file(
        "config.toml",
        "[ui.statusline]\ntemplate = '''{{ unknown }}\n{{ prompt }}'''\n",
    );
    let (session, mut input, reader) = PtySession::start(&ego, 24, 120, Some((24, 120)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "statusline:");
    let mut screens = Vec::new();
    for _ in 0..3 {
        let start = output.len();
        // Ctrl+L redraws without dismissing the notice; printable input intentionally does.
        input.write_all(b"\x0c").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "statusline:");
        drain_until_quiet(&reader, &mut output);
        let mut parser = vt100::Parser::new(24, 120, 0);
        parser.process(&output);
        screens.push(parser.screen().contents());
    }
    let start = output.len();
    input.write_all(b"\x15/model\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Select model");
    drain_until_quiet(&reader, &mut output);
    let start = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"after-modal-repaint").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "after-modal-repaint");
    drain_until_quiet(&reader, &mut output);
    let after_modal = normalized(&output[start..]);
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    for screen in screens {
        assert_eq!(screen.matches("statusline:").count(), 1, "{screen}");
        assert!(screen.contains("(line 1)"), "{screen}");
    }
    assert!(!after_modal.contains("statusline:"), "{after_modal}");
    assert!(stub.generations().is_empty());
}
