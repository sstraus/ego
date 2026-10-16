use super::*;

// Catches: Ctrl+V refusal is repainted on each activity tick, glued to the
// composer, or leaves cooked-mode literal-next/CR echo in the draft.
#[test]
fn image_paste_ctrl_v_refusal_is_one_transcript_line_and_enter_stays_a_key() {
    let (reply, gate) = gated_paced_line("PASTE-REFUSAL-WORKING\n");
    let stub = Stub::start(Plan {
        completions: vec![reply, Reply::Sse(text("AFTER-PASTE-ANSWER"))],
        ..Plan::default()
    });
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
    ego.file("AGENTS.md", "# paste refusal fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 32, 160, Some((32, 160)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"work\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "PASTE-REFUSAL-WORKING");
    let start = output.len();
    input.write_all(b"\x16").unwrap();
    input.flush().unwrap();
    let refusal = "image paste is unavailable while source work is pending";
    until_normalized(&reader, &mut output, start, refusal);
    // Two real activity ticks must not republish the outcome.
    let deadline = Instant::now() + Duration::from_millis(2200);
    while Instant::now() < deadline {
        if let Ok(chunk) = reader.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }
    let refused = normalized(&output[start..]);
    input.write_all(b"draft-after-paste").unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    gate.release();
    let resumed = output.len();
    until_prompt_from(&reader, &mut output, resumed);
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, resumed, "AFTER-PASTE-ANSWER");
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    let status = session.quit(input);
    assert!(status.success(), "{status:?}");
    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        2,
        "Enter must submit exactly one next turn"
    );
    assert!(generations[1].to_string().contains("draft-after-paste"));
    assert_eq!(refused.matches(refusal).count(), 1, "{refused:?}");
    assert!(!refused.contains(" | >"), "{refused:?}");
    assert!(
        !normalized(&output[start..]).contains("^M"),
        "literal CR: {:?}",
        normalized(&output[start..])
    );
    let mut screen = vt100::Parser::new(32, 160, 0);
    screen.process(&output);
    let text = screen.screen().contents();
    assert!(
        text.lines().any(|line| line.trim() == refusal),
        "refusal needs its own row: {text:?}"
    );
}

// Catches: a capability refusal is stored in the prompt and repeated on redraw.
#[test]
fn image_paste_unknown_capability_refusal_is_not_composer_state() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--sandbox",
            "off",
            "--trust-workspace",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts("image_input = \"unknown\""),
    );
    ego.file("AGENTS.md", "# refusal fixture\n");
    let path = ego.file("test.png", "");
    std::fs::write(&path, support::PNG_1X1).unwrap();
    let (session, mut input, reader) = PtySession::start(&ego, 32, 200, Some((32, 200)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", path.display()).as_bytes())
        .unwrap();
    input.flush().unwrap();
    let refusal = "image-input capability is unknown; no attachment was fetched";
    until_normalized(&reader, &mut output, start, refusal);
    input.write_all(b"\x1b[D\x1b[C").unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    let refused = normalized(&output[start..]);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(refused.matches(refusal).count(), 1, "{refused:?}");
    assert!(!refused.contains(" | >"), "{refused:?}");
    assert!(stub.generations().is_empty());
}
