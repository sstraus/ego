use super::*;

const COLS: u16 = 120;
const ROWS: u16 = 40;

// Catches: a question uses terminal auto-wrap after the pane shrinks, splitting words.
#[test]
fn question_picker_word_wraps_at_live_width_after_resize() {
    let (screen, question) = capture(false, 0);
    let rows = screen_rows_from_grid(&screen);
    let first = rows
        .iter()
        .position(|row| row.starts_with("PICKER-ILO"))
        .unwrap();
    let last = rows.iter().position(|row| row.starts_with("> 1.")).unwrap();
    assert_eq!(
        rows[first..last]
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>(),
        question.split_whitespace().collect::<Vec<_>>(),
        "question words split: {rows:?}"
    );
    assert!(
        (first..last).all(|row| !screen.row_wrapped(row as u16)),
        "question auto-wrapped: {rows:?}"
    );
}

// Catches: Reedline paints the whole picker cyan and never highlights selection.
#[test]
fn question_picker_uses_distinct_role_styles_and_respects_no_color() {
    for no_color in [false, true] {
        let (screen, _) = capture(no_color, 0);
        let rows = screen_rows_from_grid(&screen);
        let row = |prefix: &str| {
            rows.iter()
                .position(|row| row.trim_start().starts_with(prefix))
                .unwrap() as u16
        };
        let selected = screen.cell(row("> 1."), 0).unwrap();
        let label = screen.cell(row("2. Second choice"), 5).unwrap();
        let description = screen.cell(row("First description"), 5).unwrap();
        let hint = screen.cell(row("Up/Down"), 0).unwrap();
        let question = screen.cell(row("PICKER-ILO"), 0).unwrap();
        assert_eq!(
            question.fgcolor(),
            vt100::Color::Default,
            "question must use terminal foreground"
        );
        assert_eq!(label.fgcolor(), vt100::Color::Default);
        assert!(!label.bold() && !label.inverse());
        if no_color {
            assert!(!selected.bold() && !selected.inverse());
            assert!(!description.dim() && !hint.dim());
        } else {
            assert!(
                selected.bold() && selected.inverse(),
                "selected row has no highlight"
            );
            assert!(description.dim(), "description must be dim");
            assert!(hint.dim(), "hint must be dim");
        }
    }
}

// Catches: repeated navigation leaves copies of the overflowing picker on screen.
#[test]
fn question_picker_redraw_keeps_one_copy_after_four_moves() {
    let (screen, _) = capture(false, 4);
    let rows = screen_rows_from_grid(&screen);
    assert_eq!(
        rows.iter()
            .filter(|row| row.starts_with("PICKER-ILO"))
            .count(),
        1,
        "navigation left duplicate picker frames: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("First choice"))
            .count(),
        1
    );
    assert_eq!(rows.iter().filter(|row| row.contains("Up/Down")).count(), 1);
}

fn screen_rows_from_grid(screen: &vt100::Screen) -> Vec<String> {
    (0..ROWS)
        .map(|row| {
            (0..COLS)
                .filter_map(|col| screen.cell(row, col))
                .filter(|cell| !cell.is_wide_continuation())
                .map(|cell| {
                    if cell.contents().is_empty() {
                        " "
                    } else {
                        cell.contents()
                    }
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn capture(no_color: bool, moves: usize) -> (vt100::Screen, String) {
    let rows = if moves > 0 { 24 } else { ROWS };
    let question = format!(
        "PICKER-ILO L’eseguibile è disponibile ma occorre scegliere misure diverse perché {}fine domanda.",
        "la descrizione deve rimanere leggibile senza parole tagliate al bordo della finestra e "
            .repeat(3)
    );
    let call = serde_json::json!({
        "prompt": question, "mode": "single", "free_text": false,
        "options": [
            {"id": "first", "label": "First choice", "description": "First description"},
            {"id": "second", "label": "Second choice", "description": "Second description"},
        ],
    })
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("question", "ask_question", &call)),
            Reply::Sse(text("PICKER-ILO-DONE")),
        ],
        ..Plan::default()
    });
    let mut ego = Ego::against(&stub)
        .conversing()
        .env("TERM", "xterm-256color")
        .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    if no_color {
        ego = ego.env("NO_COLOR", "1");
    }
    ego.file("AGENTS.md", "# picker roles fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child = pair.slave.spawn_command(ego.pty_command()).unwrap();
    let (mut input, responder, writer_join) = writer_owner(pair.master.take_writer().unwrap());
    let size = Arc::new(Mutex::new((rows, 200)));
    let (reader, reader_join) = stream_with_resizable_screen(
        pair.master.try_clone_reader().unwrap(),
        responder,
        Some(Arc::clone(&size)),
    );
    let session = PtySession {
        child,
        pair,
        reader_join,
        writer_join,
    };
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    drain_until_quiet(&reader, &mut output);
    let resize_at = output.len();
    *size.lock().unwrap() = (rows, COLS);
    session
        .pair
        .master
        .resize(PtySize {
            rows,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let start = output.len();
    input.write_all(b"ask\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Esc cancels.");
    drain_until_quiet(&reader, &mut output);
    let mut key_events = Vec::new();
    for index in 0..moves {
        let key = if index % 2 == 0 { "\x1b[B" } else { "\x1b[A" };
        key_events.push(serde_json::json!({"at_byte": output.len(), "key": key}));
        input.write_all(key.as_bytes()).unwrap();
        input.flush().unwrap();
        drain_until_quiet(&reader, &mut output);
    }
    let mut terminal = vt100::Parser::new(rows, 200, 0);
    terminal.process(&output[..resize_at]);
    terminal.screen_mut().set_size(rows, COLS);
    terminal.process(&output[resize_at..]);
    let screen = terminal.screen().clone();
    if let Some(directory) = std::env::var_os("EGO_PICKER_ROLES_CAPTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let stem = format!(
            "{}-{moves}-moves",
            if no_color { "no-color" } else { "styled" }
        );
        std::fs::write(directory.join(format!("{stem}.ansi")), &output).unwrap();
        std::fs::write(directory.join(format!("{stem}.json")), serde_json::to_vec_pretty(&serde_json::json!({
            "rows": rows, "initial_cols": 200, "cols": COLS, "resize_at_byte": resize_at, "key_events": key_events,
        })).unwrap()).unwrap();
    }
    let done = output.len();
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, done, "PICKER-ILO-DONE");
    until_prompt_from(&reader, &mut output, done);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    (screen, question)
}
