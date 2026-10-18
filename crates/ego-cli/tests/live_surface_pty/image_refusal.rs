use super::*;

// Catches: busy paste is refused, or an attachment is separated from its
// queued draft and applied to the active or a later prompt instead.
#[test]
fn busy_image_paste_keeps_attachments_with_the_submitted_draft() {
    let (reply, gate) = gated_paced_line("BUSY-IMAGE-ACTIVE\n");
    let stub = Stub::start_concurrent(Plan {
        completions: vec![reply, Reply::Sse(text("QUEUED-IMAGE-ANSWER"))],
        // Ollama takes capabilities from daemon discovery, not models.toml.
        // The existing status/body override also serves successful tag responses.
        tags_refusal: Some((
            200,
            serde_json::json!({
                "models": [{ "name": "qwen3", "capabilities": ["completion", "tools", "vision"] }]
            })
            .to_string(),
        )),
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--trust-workspace",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# busy image draft fixture\n");
    let path = ego.file("queued-image.png", "");
    std::fs::write(&path, support::PNG_1X1).unwrap();
    let (session, mut input, reader) = PtySession::start(&ego, 32, 160, Some((32, 160)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"initial work\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "BUSY-IMAGE-ACTIVE");
    input.write_all(b"describe queued image").unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    let paste_start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", path.display()).as_bytes())
        .unwrap();
    input.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let rendered = normalized(&output[paste_start..]);
        if rendered.contains("[image: queued-image.png]")
            || rendered.contains("image paste is unavailable while source work is pending")
        {
            break;
        }
        if let Ok(chunk) = reader.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }
    let staged_while_busy = normalized(&output[paste_start..]);
    let mut screen = vt100::Parser::new(32, 160, 0);
    screen.process(&output);
    let draft_after_paste = screen.screen().contents();
    let active_requests = stub.generations();
    // Force a real submission-boundary acquisition failure after staging.
    // The owner must retain the exact draft without queueing any partial turn.
    std::fs::remove_file(&path).unwrap();
    let refusal_start = output.len();
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, refusal_start, "could not open");
    drain_until_quiet(&reader, &mut output);
    let mut refused_screen = vt100::Parser::new(32, 160, 0);
    refused_screen.process(&output);
    let retained = refused_screen.screen().contents();
    assert!(retained.contains("describe queued image"), "{retained:?}");
    assert!(
        retained.contains("[image: queued-image.png]"),
        "{retained:?}"
    );
    assert_eq!(stub.generations().len(), 1, "refusal must not submit");
    let journal = std::fs::read_to_string(
        ego.home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(
        !journal
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|event| event["payload"]["event"]["kind"]["kind"] == "PromptQueued"),
        "refusal must not durably queue a partial prompt"
    );
    // Repair only the external file, then resubmit without retyping or repasting.
    std::fs::write(&path, support::PNG_1X1).unwrap();
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, refusal_start, "queue=1/0");
    gate.release();
    until_normalized(&reader, &mut output, paste_start, "QUEUED-IMAGE-ANSWER");
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    let status = session.quit(input);
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        staged_while_busy.contains("[image: queued-image.png]"),
        "image must enter the draft before active work finishes: {staged_while_busy:?}"
    );
    assert_eq!(
        draft_after_paste.matches("describe queued image").count(),
        1,
        "image paste must not leave a second visible draft: {draft_after_paste:?}"
    );
    assert_eq!(active_requests.len(), 1, "paste must not submit a turn");
    assert!(
        !active_requests[0].to_string().contains("data:image/png"),
        "paste must not change the active request"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 2, "queued draft must be delivered once");
    // Host context is also sent with the user role; select the actual image
    // envelope rather than relying on which role appears last on this host.
    let image_prompts = generations[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_array())
        .filter(|content| content.iter().any(|block| block["type"] == "image_url"))
        .collect::<Vec<_>>();
    assert_eq!(image_prompts.len(), 1, "one queued image envelope");
    let content = image_prompts[0];
    let text = content
        .iter()
        .filter(|block| block["type"] == "text")
        .map(|block| block["text"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        text,
        ["describe queued image"],
        "exact draft must survive refusal"
    );
    let images = content
        .iter()
        .filter(|block| block["type"] == "image_url")
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 1, "the retained image must be sent once");
    assert!(
        images[0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
}

// Catches: a busy image capability refusal is repainted on each activity tick, glued to the
// composer, or leaves cooked-mode literal-next/CR echo in the draft.
#[test]
fn busy_image_capability_refusal_is_one_transcript_line_and_enter_stays_a_key() {
    let (reply, gate) = gated_paced_line("PASTE-REFUSAL-WORKING\n");
    let stub = Stub::start_concurrent(Plan {
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
    let path = ego.file("refused-image.png", "");
    std::fs::write(&path, support::PNG_1X1).unwrap();
    let (session, mut input, reader) = PtySession::start(&ego, 32, 160, Some((32, 160)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"work\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "PASTE-REFUSAL-WORKING");
    let start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", path.display()).as_bytes())
        .unwrap();
    input.flush().unwrap();
    let refusal = "does not support image input";
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
        text.lines().any(|line| line.contains(refusal)),
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
