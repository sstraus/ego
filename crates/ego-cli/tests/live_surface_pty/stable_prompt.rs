use super::*;

// Catches: Backspace opens an empty completion menu whose marker hides live status.
#[test]
fn editing_keeps_prompt_and_status_layout_stable() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--trust-workspace",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# stable prompt fixture\n");
    ego.home_file(
        "config.toml",
        "[ui.statusline]\ntemplate = '''STABLE {{ model }} {{ mode }}\n{{ prompt }}'''\n",
    );
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, Some((24, 100)));
    let mut output = Vec::new();
    until_normalized(&reader, &mut output, 0, "STABLE ollama/qwen3");
    drain_until_quiet(&reader, &mut output);
    let mut frames = vec![("empty", screen_rows(&output, 24, 100))];
    for (label, bytes, expected) in [
        ("typed", b"layout-probe".as_slice(), "layout-probe"),
        ("delete-one", b"\x7f", "layout-prob"),
        ("clear", b"\x15", ""),
        ("retype", b"layout-again", "layout-again"),
        ("cursor-left", b"\x1b[D", "layout-again"),
        ("delete-middle", b"\x7f", "layout-agan"),
    ] {
        let start = output.len();
        input.write_all(bytes).unwrap();
        input.flush().unwrap();
        if !expected.is_empty() && label != "cursor-left" {
            until_normalized(&reader, &mut output, start, expected);
        }
        drain_until_quiet(&reader, &mut output);
        frames.push((label, screen_rows(&output, 24, 100)));
    }
    input.write_all(b"\x05\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let baseline = &frames[0].1;
    let prompt_row = baseline
        .iter()
        .position(|row| row.trim_end() == ">")
        .expect("prompt");
    let mut failures = Vec::new();
    for (label, frame) in &frames[1..] {
        if frame.get(..prompt_row) != Some(&baseline[..prompt_row])
            || !frame
                .get(prompt_row)
                .is_some_and(|row| row.starts_with("> "))
        {
            failures.push(format!("{label}: {frame:#?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "baseline: {baseline:#?}\n{}",
        failures.join("\n")
    );
    assert!(stub.generations().is_empty());
}
