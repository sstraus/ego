use super::*;

// Catches: completed tool rows retain launch width after a PTY grows or shrinks.
#[test]
fn tool_rows_follow_real_pty_growth_and_shrink() {
    let command = format!("printf '%s' {}", "regression_case-".repeat(20));
    let args = serde_json::json!({"argv": ["/bin/sh", "-c", command]}).to_string();
    let mut completions = Vec::new();
    for index in 0..3 {
        completions.push(Reply::Sse(tool_call(
            &format!("width-{index}"),
            "exec",
            &args,
        )));
        completions.push(Reply::Sse(text(&format!("REGRESSION-MARKER-DONE-{index}"))));
    }
    let stub = Stub::start(Plan {
        completions,
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
            "--trust-workspace",
        ]);
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child = pair.slave.spawn_command(ego.pty_command()).unwrap();
    let (mut input, responder, writer_join) = writer_owner(pair.master.take_writer().unwrap());
    let size = Arc::new(Mutex::new((40, 80)));
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
    until_prompt_from(&reader, &mut output, 0);
    let mut captures = Vec::new();
    for (index, width) in [80, 200, 60].into_iter().enumerate() {
        drain_until_quiet(&reader, &mut output);
        *size.lock().unwrap() = (40, width);
        session
            .pair
            .master
            .resize(PtySize {
                rows: 40,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let start = output.len();
        input.write_all(b"run the width fixture\r").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            start,
            &format!("REGRESSION-MARKER-DONE-{index}"),
        );
        until_mcp_composer_is_idle(&reader, &mut output);
        drain_until_quiet(&reader, &mut output);
        captures.push((width, normalized(&output[start..])));
    }
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    for (width, capture) in captures {
        let row = capture
            .lines()
            .map(|line| line.rsplit('\r').next().unwrap_or(line))
            .find(|line| line.starts_with("✓ $ "))
            .unwrap_or_else(|| panic!("missing completed tool row: {capture:?}"));
        let cells = unicode_width::UnicodeWidthStr::width(row.trim_end_matches('\r'));
        assert!(
            cells <= usize::from(width),
            "row overflows live width {width}: {row:?}"
        );
        assert!(
            cells > usize::from(width) - 20,
            "row retains a stale width at {width}: {row:?}"
        );
    }
}
