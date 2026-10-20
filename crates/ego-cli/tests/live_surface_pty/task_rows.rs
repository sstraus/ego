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
