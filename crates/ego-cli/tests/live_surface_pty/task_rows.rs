use super::*;

// Catches: each child row repeats the task label instead of sharing one header.
#[test]
fn child_tool_calls_share_one_task_header_and_keep_output_width() {
    let label = "Inspect diff accounting seam";
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("task1", "task", &serde_json::json!({"name": label, "prompt": "Read calc.py then print three progress lines"}).to_string())),
            Reply::Sse(tool_call("read1", "read", r#"{"path":"calc.py"}"#)),
            Reply::Sse(tool_call("exec1", "exec", r#"{"argv":["printf","REGRESSION-MARKER-one\nTASK-535-two\nTASK-535-three\n"]}"#)),
            Reply::Sse(text("child final")),
            Reply::Sse(text("REGRESSION-MARKER-DONE")),
        ],
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
    ego.file("AGENTS.md", "# task rows fixture\n");
    ego.file("calc.py", "print('hello')\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 80, Some((40, 80)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"inspect with a task\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "REGRESSION-MARKER-DONE");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let rows = screen_rows(&output, 40, 80);
    if let Some(directory) = std::env::var_os("EGO_REGRESSION_CAPTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("task.ansi"), &output).unwrap();
        std::fs::write(directory.join("task.json"), b"{\"rows\":40,\"cols\":80}").unwrap();
    }
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains('↳') && row.contains(label))
            .count(),
        1,
        "task label repeated across child rows: {rows:?}"
    );
    let header = rows
        .iter()
        .position(|row| row.trim() == format!("↳ task {label}"))
        .expect("explicit task header");
    let read = rows
        .iter()
        .position(|row| row.trim_start().starts_with("✓ read calc.py"))
        .expect("child read row");
    let exec = rows
        .iter()
        .position(|row| row.trim_start().starts_with("✓ $ printf"))
        .expect("child exec row");
    assert!(header < read && read < exec);
    assert!(rows[read].starts_with("    ") && rows[exec].starts_with("    "));
    for tail in ["REGRESSION-MARKER-one", "REGRESSION-MARKER-two", "REGRESSION-MARKER-three"] {
        assert!(
            rows.iter()
                .any(|row| row.trim() == tail && row.starts_with("      ")),
            "lost output tail: {rows:?}"
        );
    }
    assert!(!rows.join("\n").contains("duration unavailable"));
}

// Catches: real task progress never reaches the seat, includes parent tokens,
// or remains on screen after the child settles.
#[test]
fn running_child_activity_is_cyan_and_retires_without_corrupting_the_composer() {
    let (reply, gate) = gated_paced_line("REGRESSION-MARKER-STREAM");
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "task643",
                "task",
                r#"{"name":"Inspect ledger","prompt":"Read ledger.txt then respond"}"#,
            )),
            Reply::Sse(tool_call("read643", "read", r#"{"path":"ledger.txt"}"#)),
            reply,
            Reply::Sse(text("REGRESSION-MARKER-DONE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ]);
    ego.file("AGENTS.md", "# child activity fixture\n");
    ego.file("ledger.txt", "usage ledger\n");
    // Native evidence uses a copied binary, never an mbx target while it builds.
    let program = std::env::var_os("EGO_REGRESSION_BIN");
    let command = program.as_ref().map_or_else(
        || ego.pty_command(),
        |path| ego.pty_command_with_program(path),
    );
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child = pair.slave.spawn_command(command).unwrap();
    let (mut input, responder, writer_join) = writer_owner(pair.master.take_writer().unwrap());
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().unwrap(),
        responder,
        Some((24, 80)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"inspect with a child\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Responding");
    drain_until_quiet(&reader, &mut output);
    let live = output.clone();
    gate.release();
    until_normalized(&reader, &mut output, start, "REGRESSION-MARKER-DONE");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let settled = output.clone();
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().unwrap();
    writer_join.join().unwrap();
    if let Some(directory) = std::env::var_os("EGO_REGRESSION_CAPTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("live.ansi"), &live).unwrap();
        std::fs::write(directory.join("settled.ansi"), &settled).unwrap();
        std::fs::write(
            directory.join("geometry.json"),
            b"{\"rows\":24,\"cols\":80}",
        )
        .unwrap();
    }
    let mut screen = vt100::Parser::new(24, 80, 0);
    screen.process(&live);
    let visible = screen.screen().contents();
    let rows: Vec<_> = visible.lines().collect();
    let index = rows
        .iter()
        .position(|row| row.contains("○ Inspect ledger"))
        .expect(&visible);
    assert!(
        rows[index].contains("Responding") && rows[index].ends_with("· ↓ 4 tokens"),
        "{visible}"
    );
    assert!(rows[index - 1].contains("Working"), "{visible}");
    assert_eq!(rows[index + 1].trim(), ">");
    assert_eq!(
        screen.screen().cell(index as u16, 0).unwrap().fgcolor(),
        vt100::Color::Rgb(0, 255, 255)
    );
    screen.process(&settled[live.len()..]);
    assert!(
        !screen.screen().contents().contains("○ Inspect ledger"),
        "{}",
        screen.screen().contents()
    );
}
