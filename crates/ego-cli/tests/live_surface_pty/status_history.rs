use super::*;

// Catches: every Enter commits the native status row with the prompt.
#[test]
fn status_not_committed_after_repeated_prompts() {
    submission_contract(false, false);
}

// Catches: removing the status also drops a multiline prompt's continuation.
#[test]
fn status_not_committed_with_multiline_prompt() {
    submission_contract(true, false);
}

// Catches: a resized composer retains its old status row on submission.
#[test]
fn status_not_committed_after_resize() {
    submission_contract(false, true);
}

fn submission_contract(multiline: bool, resize: bool) {
    let reply = (0..20)
        .map(|index| format!("history reply row {index}\n"))
        .collect::<String>();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text(&format!("{reply}HISTORY-FIRST-DONE"))),
            Reply::Sse(text(&format!("{reply}HISTORY-SECOND-DONE"))),
        ],
        ..Plan::default()
    });
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
    ego.file("AGENTS.md", "# status history fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, Some((24, 100)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let mut parser = vt100::Parser::new(24, 100, 10_000);
    parser.process(&output);
    let processed = output.len();
    let width = if resize { 60 } else { 100 };
    if resize {
        session
            .pair
            .master
            .resize(PtySize {
                rows: 24,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        parser.screen_mut().set_size(24, width);
        until_from(&reader, &mut output, processed, "qwen3 · ".as_bytes());
        drain_until_quiet(&reader, &mut output);
    }
    for (prompt, done) in [
        (
            if multiline {
                "history-first\nhistory-continuation"
            } else {
                "history-first"
            },
            "HISTORY-FIRST-DONE",
        ),
        ("history-second", "HISTORY-SECOND-DONE"),
    ] {
        let start = output.len();
        input.write_all(b"\x1b[200~").unwrap();
        input.write_all(prompt.as_bytes()).unwrap();
        input.write_all(b"\x1b[201~\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, done);
        // A busy composer also paints a prompt; capture only the completed idle frame.
        until_idle_status_ready(&reader, &mut output, start);
        drain_until_quiet(&reader, &mut output);
    }
    parser.process(&output[processed..]);
    let live = parser.screen().contents();
    parser.screen_mut().set_scrollback(usize::MAX);
    let depth = parser.screen().scrollback();
    let mut history = String::new();
    for offset in (1..=depth).rev() {
        parser.screen_mut().set_scrollback(offset);
        history.push_str(&parser.screen().rows(0, width).next().unwrap());
        history.push('\n');
    }
    parser.screen_mut().set_scrollback(0);
    history.push_str(&live);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());

    assert!(depth > 0, "fixture must exercise real scrollback");
    for prompt in ["> history-first", "> history-second"] {
        assert_eq!(history.matches(prompt).count(), 1, "{history}");
    }
    if multiline {
        assert!(history.contains("… history-continuation"), "{history}");
    }
    assert_eq!(live.matches("qwen3 · ").count(), 1, "{live}");
    assert_eq!(
        history.matches("qwen3 · ").count(),
        1,
        "submitted composer status leaked into terminal history: {history}"
    );
}
