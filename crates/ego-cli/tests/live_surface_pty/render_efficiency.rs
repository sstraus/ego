use super::*;

// Catches: quiet ticks invalidate the anchor and redraw the unchanged composer.
#[test]
fn quiet_working_ticks_do_not_query_the_cursor() {
    let (reply, gate) = gated_paced_line("EFFICIENCY-HELD\n");
    let stub = Stub::start(Plan {
        completions: vec![reply],
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
    ego.file("AGENTS.md", "# rendering efficiency fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"render efficiency\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"EFFICIENCY-HELD");
    drain_until_quiet(&reader, &mut output);
    let mark = output.len();
    until_normalized(&reader, &mut output, mark, "Working 2s");
    drain_until_quiet(&reader, &mut output);
    let ticks = output[mark..].to_vec();
    let mark = output.len();
    gate.release();
    until_idle_status_ready(&reader, &mut output, mark);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(
        !ticks.windows(4).any(|bytes| bytes == b"\x1b[6n"),
        "quiet ticks queried cursor: {ticks:?}"
    );
    assert!(
        !ticks.windows(3).any(|bytes| bytes == b"\x1b[J"),
        "quiet ticks erased the full live region: {ticks:?}"
    );
    assert!(
        !ticks.windows(2).any(|bytes| bytes == b"> "),
        "quiet ticks repainted the composer: {ticks:?}"
    );
}

// Catches: each transcript row emits a terminal-width space-padding eraser.
#[test]
fn external_rows_do_not_emit_space_padding_erasers() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text(&format!(
            "ERASER-ROW\n\n{}\n\n界 e\u{301}\n\n",
            "wrapped ".repeat(24)
        )))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3"])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# external row fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 8, 120, Some((8, 120)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    let mark = output.len();
    input.write_all(b"external rows\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"ERASER-ROW");
    until_idle_status_ready(&reader, &mut output, mark);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let eraser = format!("\r{}\r", " ".repeat(120));
    assert!(
        !output[mark..]
            .windows(eraser.len())
            .any(|bytes| bytes == eraser.as_bytes()),
        "external transcript contains space padding"
    );
}
