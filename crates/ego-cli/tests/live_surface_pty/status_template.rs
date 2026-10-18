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
