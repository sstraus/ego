use super::*;

fn start(ego: &Ego) -> (PtySession, PtyWriter, mpsc::Receiver<Vec<u8>>) {
    let command = match std::env::var_os("EGO_QUEUED_CONTROLS_BINARY") {
        Some(binary) => ego.pty_command_with_program(binary),
        None => ego.pty_command(),
    };
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(command).expect("spawn");
    let (input, responder, writer_join) = writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(pair.master.try_clone_reader().expect("reader"), responder);
    (
        PtySession {
            child,
            pair,
            reader_join,
            writer_join,
        },
        input,
        reader,
    )
}

fn journal(ego: &Ego) -> String {
    std::fs::read_to_string(
        ego.home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .expect("durable journal")
}

// Catches: Up refuses to withdraw a queued prompt when the first composer line
// already contains a draft (the empty-buffer gate).
#[test]
fn up_with_nonempty_first_line_withdraws_queued_prompt() {
    let (initial, gate) = gated_paced_line("RECALL-ZAW-RUNNING");
    let stub = Stub::start(Plan {
        completions: vec![initial, Reply::Sse(text("RECALL-ZAW-DONE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# queued controls fixture\n");
    let (session, mut input, reader) = start(&ego);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\rinitial\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"RECALL-ZAW-RUNNING");
    let queued = output.len();
    input.write_all(b"withdraw this queued prompt\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        queued,
        "Messages to be submitted after next tool call",
    );
    input.write_all(b"unsent first-line draft\x1b[A").unwrap();
    input.flush().unwrap();
    collect_retractions(&ego, &reader, &mut output, 1);
    let recalled = journal(&ego);
    // Replace the recalled line and requeue it. The displaced draft is restored
    // by the existing draft stack, never submitted implicitly.
    input.write_all(b"\x15edited withdrawal\r").unwrap();
    input.flush().unwrap();
    let offset = output.len();
    until_normalized(
        &reader,
        &mut output,
        offset,
        "Messages to be submitted after next tool call",
    );
    // Always release and reap the real child, including the pre-fix failure.
    gate.release();
    until_prompt_after_reply(&reader, &mut output, b"RECALL-ZAW-DONE");
    drain_until_quiet(&reader, &mut output);
    let visible = screen_rows(&output, 24, 100).join("\n");
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    if String::from_utf8_lossy(&output).contains("Exit with") {
        input.write_all(b"y\r").unwrap();
        input.flush().unwrap();
    }
    assert!(session.quit(input).success());
    if let Some(path) = std::env::var_os("EGO_QUEUED_CONTROLS_CAPTURE") {
        std::fs::write(path, &output).expect("retain PTY bytes");
    }
    assert_eq!(
        recalled.matches("PromptRetracted").count(),
        1,
        "Up with nonempty first-line draft must durably withdraw the queued prompt; journal: {recalled}"
    );
    assert!(
        visible.contains("unsent first-line draft"),
        "displaced draft must return: {visible}"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 2);
    assert_eq!(
        count_exact_strings(&generations[1]["messages"], "withdraw this queued prompt"),
        0
    );
    assert_eq!(
        count_exact_strings(&generations[1]["messages"], "edited withdrawal"),
        1
    );
}

// Catches: the ingress acknowledgement remains painted after the expedited
// FIFO has been consumed, throughout the next still-running generation.
#[test]
fn esc_interrupt_status_clears_during_the_next_generation() {
    let (initial, first_gate) = gated_paced_line("STATUS-IUW-FIRST");
    let (next, next_gate) = gated_paced_line("STATUS-IUW-NEXT");
    let stub = Stub::start(Plan {
        completions: vec![initial, next],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# interruption status fixture\n");
    let (session, mut input, reader) = start(&ego);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\rinitial\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"STATUS-IUW-FIRST");
    let queued = output.len();
    input.write_all(b"deliver this next\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        queued,
        "Messages to be submitted after next tool call",
    );
    let interrupted = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, interrupted, "interrupt pending");
    first_gate.release();
    until(&reader, &mut output, b"STATUS-IUW-NEXT");
    drain_until_quiet(&reader, &mut output);
    let visible = screen_rows(&output, 24, 100).join("\n");
    let consumed = journal(&ego);
    // Release and reap before asserting, including on the pre-fix failure.
    let finished = output.len();
    next_gate.release();
    until_prompt_from(&reader, &mut output, finished);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(
        consumed.contains("PromptDeliveryExpedited"),
        "Esc expedited the FIFO"
    );
    assert_eq!(
        consumed.matches("queued_prompt").count(),
        1,
        "one consumed queued input"
    );
    assert!(
        visible.contains("Working"),
        "second generation is still live: {visible}"
    );
    assert!(
        !visible.contains("interrupt=yes"),
        "settled interruption stayed visible: {visible}"
    );
    assert!(
        !visible.contains("steer 0 · next 1"),
        "consumed queue stayed visible: {visible}"
    );
}

// Catches: blank-composer rejection eats Ctrl+Enter instead of expediting
// prompts already durably queued.
#[test]
fn ctrl_enter_steers_queued_prompts_without_a_composer_line() {
    steer_queued(None);
}

// Catches: steering the composer delivers it ahead of earlier queued prompts,
// or appends a second copy of any queued input.
#[test]
fn ctrl_enter_steers_queued_prompts_before_the_composer_line() {
    steer_queued(Some("composer after queue"));
}

fn steer_queued(composer: Option<&str>) {
    let (initial, gate) = gated_paced_line("STEER-ZAW-RUNNING");
    let stub = Stub::start(Plan {
        completions: vec![initial, Reply::Sse(text("STEER-ZAW-DONE"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# queued steering fixture\n");
    let (session, mut input, reader) = start(&ego);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\rinitial\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"STEER-ZAW-RUNNING");
    for prompt in ["queued first", "queued second"] {
        let offset = output.len();
        input.write_all(prompt.as_bytes()).unwrap();
        input.write_all(b"\r").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            offset,
            "Messages to be submitted after next tool call",
        );
    }
    if let Some(composer) = composer {
        input.write_all(composer.as_bytes()).unwrap();
    }
    input.write_all(b"\x1b[13;5u").unwrap();
    input.flush().unwrap();
    // A quiet PTY interval is not an acknowledgement of a host command.
    // Collect the real interrupt receipt, bounded so the pre-fix blank-input
    // rejection can still release the provider and reap its child.
    let offset = output.len();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !normalized(&output[offset..]).contains("interrupt pending") && Instant::now() < deadline
    {
        if let Ok(chunk) = reader.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }
    let expedited = journal(&ego);
    gate.release();
    until_prompt_after_reply(&reader, &mut output, b"STEER-ZAW-DONE");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        expedited.matches("PromptDeliveryExpedited").count(),
        1,
        "Ctrl+Enter must expedite the existing queue even without composer text"
    );
    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        2,
        "one interrupted request and one delivery"
    );
    let mut expected = vec!["queued first", "queued second"];
    expected.extend(composer);
    // Consecutive user prompts may share one wire message. The observable
    // contract is each fixture line once in FIFO order, not JSON boundaries.
    let delivered = generations[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .flat_map(|message| {
            message["content"]
                .as_str()
                .expect("text-only user content")
                .lines()
        })
        .filter(|line| expected.contains(line))
        .collect::<Vec<_>>();
    assert_eq!(
        delivered, expected,
        "FIFO delivery including composer, exactly once"
    );
    let final_journal = journal(&ego);
    let events = final_journal
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|record| !record["payload"]["event"].is_null())
        .map(|record| {
            serde_json::from_value::<ego_core::event::domain::DomainEvent>(
                record["payload"]["event"].clone(),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    use ego_core::event::domain::EventKind;
    let queued = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::PromptQueued { id, text, .. } => Some((*id, text.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        queued.iter().map(|(_, text)| *text).collect::<Vec<_>>(),
        expected,
        "durable admissions must match the queued inputs and optional composer"
    );
    let consumed = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::UserPrompt {
                queued_prompt: Some(id),
                text,
                ..
            } => Some((*id, text.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        consumed, queued,
        "each queued ID must be acknowledged once, in admission order"
    );
    let state = replay(&final_journal);
    assert_eq!(
        state.queue_status(),
        ego_core::state::QueueStatus::default()
    );
    assert_eq!(
        final_journal.matches("PromptQueued").count(),
        if composer.is_some() { 3 } else { 2 },
        "empty composer must not introduce a blank queued prompt"
    );
}

// Catches: repeated Up stops at a nonempty recalled draft, loses its images,
// or retracts a prompt while Up should move inside a multiline draft.
#[test]
fn repeated_up_withdraws_images_and_text_without_replaying_them_on_resume() {
    let (initial, gate) = gated_paced_line("IMAGES-ZAW-RUNNING");
    let stub = Stub::start_concurrent(Plan {
        completions: vec![initial, Reply::Sse(text("RESUMED-ZAW-DONE"))],
        tags_refusal: Some((
            200,
            serde_json::json!({
                "models": [{ "name": "qwen3", "capabilities": ["completion", "tools", "vision"] }]
            })
            .to_string(),
        )),
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# queued image recall fixture\n");
    let path = ego.file("recall.png", "");
    std::fs::write(&path, support::PNG_1X1).unwrap();
    let (session, mut input, reader) = start(&ego);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"initial\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"IMAGES-ZAW-RUNNING");
    let offset = output.len();
    input.write_all(b"image queued first").unwrap();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", path.display()).as_bytes())
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, offset, "[image: recall.png]");
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        offset,
        "Messages to be submitted after next tool call",
    );
    let offset = output.len();
    input.write_all(b"text queued second\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        offset,
        "Messages to be submitted after next tool call",
    );
    // On the second logical line, Up remains cursor movement. A second Up
    // now on the first line must withdraw even though the draft is nonempty.
    let offset = output.len();
    input
        .write_all(b"first draft line\nsecond draft line\x1b[A")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, offset, "second draft line");
    drain_until_quiet(&reader, &mut output);
    assert_eq!(
        journal(&ego).matches("PromptRetracted").count(),
        0,
        "Up below the first line must not retract a queued prompt"
    );
    input.write_all(b"\x1b[A").unwrap();
    input.flush().unwrap();
    collect_retractions(&ego, &reader, &mut output, 1);
    input.write_all(b"\x1b[A").unwrap();
    input.flush().unwrap();
    collect_retractions(&ego, &reader, &mut output, 2);
    drain_until_quiet(&reader, &mut output);
    let visible = screen_rows(&output, 24, 100).join("\n");
    assert!(visible.contains("image queued first"), "{visible}");
    assert!(
        visible.contains("recalled 1"),
        "image must return to composer: {visible}"
    );
    let state = replay(&journal(&ego));
    assert_eq!(
        state.queue_status(),
        ego_core::state::QueueStatus::default()
    );
    // Do not requeue either withdrawn prompt. Finish the original turn, then
    // discard only the unsent drafts at the normal explicit exit confirmation.
    let offset = output.len();
    gate.release();
    until_idle_image_composer(&reader, &mut output, offset);
    let offset = output.len();
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, offset);
    let offset = output.len();
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, offset, "Exit with");
    input.write_all(b"y\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        stub.generations().len(),
        1,
        "withdrawn prompts must not run"
    );

    let resumed = Ego::at(ego.home_path(), ego.workspace_path(), Some(&stub))
        .subcommand("resume")
        .args([
            support::FIXTURE_SESSION_ID,
            "--model",
            "ollama/qwen3",
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--trust-workspace",
        ]);
    let (session, mut input, reader) = start(&resumed);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"fresh prompt after resume\r").unwrap();
    input.flush().unwrap();
    until_prompt_after_reply(&reader, &mut output, b"RESUMED-ZAW-DONE");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let requests = stub.generations();
    assert_eq!(requests.len(), 2);
    for withdrawn in ["image queued first", "text queued second"] {
        assert_eq!(
            count_exact_strings(&requests[1]["messages"], withdrawn),
            0,
            "withdrawn input must not return on resume: {withdrawn}"
        );
    }
    assert!(
        !requests[1]["messages"].to_string().contains("image_url"),
        "withdrawn image must not leak into resumed provider request"
    );
    assert_eq!(journal(&ego).matches("PromptRetracted").count(), 2);
}

fn replay(journal: &str) -> ego_core::state::SessionState {
    let mut state = ego_core::state::SessionState::default();
    for line in journal.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        if !value["payload"]["event"].is_null() {
            state.apply(&serde_json::from_value(value["payload"]["event"].clone()).unwrap());
        }
    }
    state
}

fn collect_retractions(
    ego: &Ego,
    reader: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    count: usize,
) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while journal(ego).matches("PromptRetracted").count() < count && Instant::now() < deadline {
        if let Ok(chunk) = reader.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }
}

// Catches: the queued-recall binding steals ordinary history navigation when
// no prompt remains queued.
#[test]
fn up_without_queued_prompts_keeps_history_navigation() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("HISTORY-FIRST")),
            Reply::Sse(text("HISTORY-SECOND")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# ordinary history fixture\n");
    let (session, mut input, reader) = start(&ego);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"ordinary history prompt\r").unwrap();
    input.flush().unwrap();
    until_prompt_after_reply(&reader, &mut output, b"HISTORY-FIRST");
    let offset = output.len();
    input.write_all(b"\x1b[A").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, offset, "ordinary history prompt");
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_prompt_after_reply(&reader, &mut output, b"HISTORY-SECOND");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let requests = stub.generations();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        count_exact_strings(&requests[1]["messages"], "ordinary history prompt"),
        2,
        "history Up must resubmit the prior input once, preserving its earlier history entry"
    );
}

// The recalled image and text stay in place until an idle frame is complete.
// Generic empty/text prompt helpers do not recognize this image-prefixed row.
fn until_idle_image_composer(reader: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, start: usize) {
    const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    loop {
        let ready = output[start..]
            .windows(HIDE_CURSOR.len())
            .enumerate()
            .filter(|(_, bytes)| *bytes == HIDE_CURSOR)
            .any(|(offset, _)| {
                let frame = &output[start + offset..];
                let Some(end) = frame
                    .windows(SHOW_CURSOR.len())
                    .position(|bytes| bytes == SHOW_CURSOR)
                else {
                    return false;
                };
                let mut screen = vt100::Parser::new(24, 100, 0);
                screen.process(&frame[..end + SHOW_CURSOR.len()]);
                let visible = screen.screen().contents();
                !visible.contains("Esc to interrupt")
                    && visible.trim_end().lines().last()
                        == Some("[image: recalled 1] | > image queued first")
            });
        if ready {
            return;
        }
        output.extend(
            reader
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "idle image composer wait: {error}; {:?}",
                        normalized(&output[start..])
                    )
                }),
        );
    }
}
