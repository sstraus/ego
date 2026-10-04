mod support;

use ego_cli::repl::actions::ActionRegistry;
use portable_pty::{ExitStatus, NativePtySystem, PtySize, PtySystem};
use std::{
    io::{self, Read, Write},
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use support::{
    Ego, Plan, ReleaseGate, Reply, Stub, gated_paced_tool_call_line, paced, text, tool_call,
};

// portable-pty transports bytes but does not emulate a terminal. Keep the
// reported cursor inside both 80x24 and 12x8; painter geometry is proven by
// the vendor unit test, while this test needs only a live DSR response.
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_RESPONSE: &[u8] = b"\x1b[4;1R";

// Catches: cooked type-ahead becomes one Ctrl-J batch and drops the second line.
#[cfg(not(windows))]
#[test]
fn two_lines_submitted_before_composer_readiness_reach_the_session_once() {
    let discovery = ReleaseGate::new();
    let (first, first_reply) = gated_paced_line("EARLY-FIRST-ANSWER");
    let stub = Stub::start(Plan {
        discovery_gate: Some(Arc::clone(&discovery)),
        completions: vec![first, Reply::Sse(text("EARLY-SECOND-ANSWER"))],
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
        ])
        .env("TERM", "xterm-256color");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 90,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let mut writer = pair.master.take_writer().expect("writer");
    // Discovery is a real startup effect. Holding its reply prevents activation,
    // without a timing guess or a production-only test hook.
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !stub
        .requests()
        .iter()
        .any(|request| request.path == "/api/tags")
    {
        assert!(Instant::now() < deadline, "startup did not reach discovery");
        thread::sleep(Duration::from_millis(10));
    }
    writer
        .write_all(b"early alpha\rearly beta\r")
        .expect("type-ahead");
    writer.flush().expect("type-ahead flush");
    let (mut input, dsr_responder, writer_join) = writer_owner(writer);
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((24, 90)),
    );
    let mut output = Vec::new();
    drain(&reader, &mut output);
    assert_eq!(rendered_prompt_count(&output), 0, "composer already opened");
    discovery.release();
    until_normalized(&reader, &mut output, 0, "EARLY-FIRST-ANSWER");
    until_normalized(
        &reader,
        &mut output,
        0,
        "Messages to be submitted after next tool call",
    );
    first_reply.release();
    until_normalized(&reader, &mut output, 0, "EARLY-SECOND-ANSWER");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    let events = std::fs::read_to_string(
        ego.home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .expect("durable session");
    let events: Vec<serde_json::Value> = events
        .lines()
        .map(|line| serde_json::from_str(line).expect("event envelope"))
        .collect();
    let prompts: Vec<&str> = events
        .iter()
        .filter(|event| event["payload"]["event"]["kind"]["kind"] == "UserPrompt")
        .map(|event| {
            event["payload"]["event"]["kind"]["text"]
                .as_str()
                .expect("prompt text")
        })
        .collect();
    assert_eq!(prompts, ["early alpha", "early beta"]);
    let queued: Vec<&str> = events
        .iter()
        .filter(|event| event["payload"]["event"]["kind"]["kind"] == "PromptQueued")
        .map(|event| {
            event["payload"]["event"]["kind"]["text"]
                .as_str()
                .expect("queued text")
        })
        .collect();
    assert_eq!(queued, ["early beta"]);
    let generations = stub.generations();
    assert_eq!(generations.len(), 2, "provider turns: {generations:?}");
    assert_eq!(count_exact_strings(&generations[0], "early alpha"), 1);
    assert_eq!(count_exact_strings(&generations[0], "early beta"), 0);
    assert_eq!(count_exact_strings(&generations[1], "early beta"), 1);
    let mut screen = vt100::Parser::new(24, 90, 100);
    screen.process(&output);
    screen.screen_mut().set_scrollback(100);
    let transcript = screen.screen().contents();
    assert_eq!(
        transcript.matches("early alpha").count(),
        1,
        "duplicate type-ahead: {transcript}"
    );
    assert_eq!(
        transcript.matches("early beta").count(),
        1,
        "duplicate type-ahead: {transcript}"
    );
}

fn double_ctrl_c_exit(status: &ExitStatus) -> bool {
    status.success() || cfg!(windows) && status.exit_code() == 0xC000_013A // STATUS_CONTROL_C_EXIT
}

#[cfg(not(windows))]
#[test]
fn image_paste_path_stages_a_chip_and_sends_image_with_next_turn() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("IMAGE-PASTE-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# image paste fixture\n");
    let image_path = ego.file("sample.png", "");
    std::fs::write(&image_path, support::PNG_1X1).expect("write image fixture");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", image_path.display()).as_bytes())
        .expect("bracketed image path");
    input.flush().expect("paste flush");
    until_normalized(&reader, &mut output, paste_start, "[image: sample.png]");
    let staged = normalized(&output[paste_start..]);
    input
        .write_all(b"describe this image\r")
        .expect("next turn");
    input.flush().expect("next turn flush");
    until_normalized(&reader, &mut output, paste_start, "IMAGE-PASTE-ANSWER");
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        staged.contains("[image: sample.png]"),
        "no removable image chip: {staged:?}"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "unexpected turns: {generations:?}");
    let body = generations[0].to_string();
    assert!(
        body.contains(r#""media_type":"image/png""#),
        "no image sent: {body}"
    );
    assert!(
        !body.contains(&image_path.display().to_string()),
        "image path leaked to model: {body}"
    );
}

/// catches: the `Signalling` wrapper being dropped from `interactive_through`,
/// which the `Announce::signal` unit tests cannot see — a real interactive
/// turn in a TUIC pane must carry the title, intent, busy and idle signals.
#[cfg(not(windows))]
#[test]
fn a_turn_in_a_tuic_pane_tells_tuic_its_title_intent_busy_and_idle() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("SIGNAL-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color")
        .env("TUIC_SOCKET", "/nonexistent/tuic.sock");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# signal fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let turn_start = output.len();
    input.write_all(b"say hello\r").expect("turn");
    input.flush().expect("turn flush");
    until_normalized(&reader, &mut output, turn_start, "SIGNAL-ANSWER");
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    until_from(
        &reader,
        &mut output,
        turn_start,
        b"\x1b]7770;state=idle\x07",
    );
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    let turn = String::from_utf8_lossy(&output[turn_start..]).into_owned();
    for expected in [
        "\x1b]0;ɛ । say hello\x07",
        "\x1b]7770;intent=say hello (εgo)\x07",
        "\x1b]7770;state=busy\x07",
        "\x1b]7770;state=idle\x07",
    ] {
        assert!(turn.contains(expected), "missing {expected:?} in {turn:?}");
    }
}

#[cfg(not(windows))]
#[test]
fn image_paste_chip_can_be_removed_before_submission() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("REMOVED-IMAGE-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# image paste fixture\n");
    let image_path = ego.file("remove.png", "");
    std::fs::write(&image_path, support::PNG_1X1).expect("write image fixture");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", image_path.display()).as_bytes())
        .expect("paste image path");
    input.flush().expect("paste flush");
    until_normalized(&reader, &mut output, paste_start, "[image: remove.png]");
    let removal_start = output.len();
    input.write_all(b"\x1b").expect("remove chip");
    input.flush().expect("remove flush");
    until_prompt_from(&reader, &mut output, removal_start);
    let after_removal = normalized(&output[removal_start..]);
    input
        .write_all(b"describe without attachment\r")
        .expect("submit turn");
    input.flush().expect("submit flush");
    until_normalized(&reader, &mut output, removal_start, "REMOVED-IMAGE-ANSWER");
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        !after_removal.contains("[image: remove.png]"),
        "chip remained: {after_removal:?}"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "unexpected turns: {generations:?}");
    assert!(
        !generations[0]
            .to_string()
            .contains(r#""media_type":"image/png""#),
        "removed image reached model: {}",
        generations[0]
    );
}

#[cfg(unix)]
#[test]
fn image_paste_refuses_unsupported_model_before_reading_image() {
    use std::os::unix::fs::PermissionsExt;

    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_NO_PICTURES),
    );
    ego.file("AGENTS.md", "# image paste fixture\n");
    let image_path = ego.file("unreadable.png", "");
    std::fs::write(&image_path, support::PNG_1X1).expect("write image fixture");
    std::fs::set_permissions(&image_path, std::fs::Permissions::from_mode(0o000))
        .expect("make image unreadable");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", image_path.display()).as_bytes())
        .expect("paste image path");
    input.flush().expect("paste flush");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let rendered = normalized(&output[paste_start..]);
        if rendered.contains("does not support image input")
            || rendered.contains("[image: unreadable.png]")
        {
            break;
        }
        if let Ok(chunk) = reader.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }
    let result = normalized(&output[paste_start..]);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    std::fs::set_permissions(&image_path, std::fs::Permissions::from_mode(0o600))
        .expect("restore image permissions");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        result.contains("does not support image input"),
        "no capability refusal: {result:?}"
    );
    assert!(
        !result.contains("[image: unreadable.png]"),
        "unsupported image staged: {result:?}"
    );
    assert!(
        stub.generations().is_empty(),
        "paste must not submit a turn"
    );
}

#[cfg(not(windows))]
#[test]
fn image_paste_missing_or_non_image_path_stays_as_text() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("PATH-TEXT-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# image paste fixture\n");
    let notes_path = ego.file("notes.txt", "plain notes, not an image");
    let missing_path = notes_path.with_file_name("does-not-exist.png");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", missing_path.display()).as_bytes())
        .expect("paste missing path");
    input.flush().expect("paste flush");
    input
        .write_all(format!(" {}\r", notes_path.display()).as_bytes())
        .expect("paste non-image path and submit");
    input.flush().expect("submit flush");
    until_normalized(&reader, &mut output, paste_start, "PATH-TEXT-ANSWER");
    let staged = normalized(&output[paste_start..]);
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        !staged.contains("[image:"),
        "a missing or non-image path must never stage a chip: {staged:?}"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "unexpected turns: {generations:?}");
    let body = generations[0].to_string();
    assert!(
        !body.contains(r#""media_type":"image/png""#),
        "no image should have been sent: {body}"
    );
    assert!(
        body.contains(&missing_path.display().to_string())
            && body.contains(&notes_path.display().to_string()),
        "both paths must reach the model as literal text: {body}"
    );
}

#[cfg(not(windows))]
#[test]
fn image_paste_disguised_extension_content_stays_as_text() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("DISGUISED-TEXT-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# image paste fixture\n");
    let disguised_path = ego.file("disguised.png", "");
    std::fs::write(&disguised_path, b"not an image, just bytes named .png")
        .expect("write disguised fixture");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", disguised_path.display()).as_bytes())
        .expect("paste disguised path");
    input.flush().expect("paste flush");
    input.write_all(b"\r").expect("submit");
    input.flush().expect("submit flush");
    until_normalized(&reader, &mut output, paste_start, "DISGUISED-TEXT-ANSWER");
    let staged = normalized(&output[paste_start..]);
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        !staged.contains("[image:"),
        "content that fails to decode as an image must never stage a chip, whatever its \
         extension: {staged:?}"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "unexpected turns: {generations:?}");
    let body = generations[0].to_string();
    assert!(
        !body.contains(r#""media_type":"image/png""#),
        "no image should have been sent: {body}"
    );
    assert!(
        body.contains(&disguised_path.display().to_string()),
        "the disguised path must reach the model as literal text: {body}"
    );
}

#[cfg(not(windows))]
#[test]
fn image_paste_oversize_path_is_refused_with_its_size() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# image paste fixture\n");
    let image_path = ego.file("huge.png", "");
    // The fixture model's cap is exactly 65536 bytes; one byte over it must
    // be refused before the file is staged.
    // The bracketed-paste handler sniffs the header before staging a path as
    // an image, so the fixture must carry a real PNG signature; only the
    // total size, not the rest of the content, needs to exceed the cap.
    let mut oversize = support::PNG_1X1.to_vec();
    oversize.resize(65536 + 1, 0_u8);
    std::fs::write(&image_path, &oversize).expect("write oversize fixture");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", image_path.display()).as_bytes())
        .expect("paste oversize path");
    input.flush().expect("paste flush");
    until_normalized(&reader, &mut output, paste_start, "65537");
    let result = normalized(&output[paste_start..]);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        result.contains("65537") && result.contains("65536"),
        "refusal must name the observed size and the limit: {result:?}"
    );
    assert!(
        !result.contains("[image: huge.png]"),
        "oversize image staged: {result:?}"
    );
    assert!(
        stub.generations().is_empty(),
        "refused paste must not submit a turn"
    );
}

/// catches: a per-turn attachment refusal (two images that each fit the
/// per-image cap but together exceed the aggregate cap) propagating out of the
/// turn loop and ending the whole interactive session.
#[cfg(not(windows))]
#[test]
fn image_paste_aggregate_over_cap_refuses_the_turn_and_keeps_the_session() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# image paste fixture\n");
    // 40000 + 40000 > 65536 aggregate, while each is under the per-image cap.
    let mut half = support::PNG_1X1.to_vec();
    half.resize(40_000, 0_u8);
    let first = ego.file("first.png", "");
    let second = ego.file("second.png", "");
    std::fs::write(&first, &half).expect("write first fixture");
    std::fs::write(&second, &half).expect("write second fixture");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    for (path, chip) in [
        (&first, "[image: first.png]"),
        (&second, "[image: second.png]"),
    ] {
        input
            .write_all(format!("\x1b[200~{}\x1b[201~", path.display()).as_bytes())
            .expect("bracketed image path");
        input.flush().expect("paste flush");
        until_normalized(&reader, &mut output, paste_start, chip);
    }
    input.write_all(b"describe these\r").expect("turn");
    input.flush().expect("turn flush");
    until_normalized(&reader, &mut output, paste_start, "aggregate image limit");
    let refused_at = output.len();
    until_prompt_from(&reader, &mut output, refused_at);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(
        status.success(),
        "the refusal ended the session instead of the turn: {status:?}"
    );
    assert!(
        stub.generations().is_empty(),
        "a refused turn must not reach the provider"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "uses the real general pasteboard only when it is empty"]
fn image_paste_clipboard_png_stages_and_reaches_next_turn() {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSData, NSString};

    struct RestorePasteboard(Option<String>);
    impl Drop for RestorePasteboard {
        fn drop(&mut self) {
            let board = NSPasteboard::generalPasteboard();
            board.clearContents();
            if let Some(text) = &self.0 {
                board.setString_forType(
                    &NSString::from_str(text),
                    &NSString::from_str("public.utf8-plain-text"),
                );
            }
        }
    }

    let pasteboard = NSPasteboard::generalPasteboard();
    let original_types = pasteboard
        .types()
        .map(|types| {
            types
                .iter()
                .map(|kind| kind.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        original_types
            .iter()
            .all(|kind| kind.contains("plain-text") || kind == "NSStringPboardType"),
        "live clipboard canary requires empty or plain-text pasteboard; types: {original_types:?}"
    );
    let original_text = pasteboard
        .stringForType(&NSString::from_str("public.utf8-plain-text"))
        .map(|text| text.to_string());
    pasteboard.clearContents();
    let _restore = RestorePasteboard(original_text);
    let png = NSData::with_bytes(support::PNG_1X1);
    assert!(pasteboard.setData_forType(Some(&png), &NSString::from_str("public.png")));

    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("CLIPBOARD-IMAGE-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# clipboard fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input.write_all(b"\x16").expect("Ctrl+V");
    input.flush().expect("paste flush");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let rendered = normalized(&output[paste_start..]);
        if rendered.contains("[image: clipboard]") || rendered.contains("non-text content block") {
            break;
        }
        if let Ok(chunk) = reader.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }
    let staged = normalized(&output[paste_start..]);
    input
        .write_all(b"describe this clipboard image\r")
        .expect("next turn");
    input.flush().expect("next turn flush");
    until_normalized(&reader, &mut output, paste_start, "CLIPBOARD-IMAGE-ANSWER");
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        staged.contains("[image: clipboard]"),
        "no clipboard image chip: {staged:?}"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "unexpected turns: {generations:?}");
    assert!(
        generations[0]
            .to_string()
            .contains(r#""media_type":"image/png""#),
        "clipboard image missing: {}",
        generations[0]
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "uses the real general pasteboard only when it is empty"]
fn image_paste_clipboard_tiff_stages_and_reaches_next_turn() {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSData, NSString};

    struct RestorePasteboard(Option<String>);
    impl Drop for RestorePasteboard {
        fn drop(&mut self) {
            let board = NSPasteboard::generalPasteboard();
            board.clearContents();
            if let Some(text) = &self.0 {
                board.setString_forType(
                    &NSString::from_str(text),
                    &NSString::from_str("public.utf8-plain-text"),
                );
            }
        }
    }

    let pasteboard = NSPasteboard::generalPasteboard();
    let original_types = pasteboard
        .types()
        .map(|types| {
            types
                .iter()
                .map(|kind| kind.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        original_types
            .iter()
            .all(|kind| kind.contains("plain-text") || kind == "NSStringPboardType"),
        "live clipboard canary requires empty or plain-text pasteboard; types: {original_types:?}"
    );
    let original_text = pasteboard
        .stringForType(&NSString::from_str("public.utf8-plain-text"))
        .map(|text| text.to_string());
    pasteboard.clearContents();
    let _restore = RestorePasteboard(original_text);
    let mut tiff_bytes = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut tiff_bytes);
        let mut encoder = tiff::encoder::TiffEncoder::new(&mut cursor).expect("tiff encoder");
        encoder
            .write_image::<tiff::encoder::colortype::RGB8>(1, 1, &[255_u8, 0, 0])
            .expect("write 1x1 tiff");
    }
    let tiff = NSData::with_bytes(&tiff_bytes);
    assert!(pasteboard.setData_forType(Some(&tiff), &NSString::from_str("public.tiff")));

    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("CLIPBOARD-TIFF-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# clipboard tiff fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input.write_all(b"\x16").expect("Ctrl+V");
    input.flush().expect("paste flush");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let rendered = normalized(&output[paste_start..]);
        if rendered.contains("[image: clipboard]") || rendered.contains("non-text content block") {
            break;
        }
        if let Ok(chunk) = reader.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }
    let staged = normalized(&output[paste_start..]);
    input
        .write_all(b"describe this clipboard image\r")
        .expect("next turn");
    input.flush().expect("next turn flush");
    until_normalized(&reader, &mut output, paste_start, "CLIPBOARD-TIFF-ANSWER");
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        staged.contains("[image: clipboard]"),
        "no clipboard image chip: {staged:?}"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "unexpected turns: {generations:?}");
    assert!(
        generations[0]
            .to_string()
            .contains(r#""media_type":"image/png""#),
        "clipboard TIFF must reach the model re-encoded as PNG: {}",
        generations[0]
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "uses the real general pasteboard only when it is empty"]
fn image_paste_clipboard_text_only_leaves_ordinary_paste_unchanged() {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    struct RestorePasteboard(Option<String>);
    impl Drop for RestorePasteboard {
        fn drop(&mut self) {
            let board = NSPasteboard::generalPasteboard();
            board.clearContents();
            if let Some(text) = &self.0 {
                board.setString_forType(
                    &NSString::from_str(text),
                    &NSString::from_str("public.utf8-plain-text"),
                );
            }
        }
    }

    let pasteboard = NSPasteboard::generalPasteboard();
    let original_types = pasteboard
        .types()
        .map(|types| {
            types
                .iter()
                .map(|kind| kind.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        original_types
            .iter()
            .all(|kind| kind.contains("plain-text") || kind == "NSStringPboardType"),
        "live clipboard canary requires empty or plain-text pasteboard; types: {original_types:?}"
    );
    let original_text = pasteboard
        .stringForType(&NSString::from_str("public.utf8-plain-text"))
        .map(|text| text.to_string());
    pasteboard.clearContents();
    let _restore = RestorePasteboard(original_text);
    pasteboard.setString_forType(
        &NSString::from_str("CLIPBOARD-TEXT-PAYLOAD"),
        &NSString::from_str("public.utf8-plain-text"),
    );

    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("CLIPBOARD-TEXT-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("TERM", "xterm-256color");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts(support::READS_PICTURES),
    );
    ego.file("AGENTS.md", "# clipboard text fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    let prompt_start = output.len();
    until_post_trust_prompt(&reader, &mut output, prompt_start);
    let paste_start = output.len();
    input.write_all(b"\x16").expect("Ctrl+V");
    input.flush().expect("paste flush");
    until_normalized(&reader, &mut output, paste_start, "CLIPBOARD-TEXT-PAYLOAD");
    let staged = normalized(&output[paste_start..]);
    input.write_all(b"\r").expect("submit");
    input.flush().expect("submit flush");
    until_normalized(&reader, &mut output, paste_start, "CLIPBOARD-TEXT-ANSWER");
    let quit_start = output.len();
    until_prompt_from(&reader, &mut output, quit_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        !staged.contains("[image:"),
        "text-only pasteboard must never stage an image chip: {staged:?}"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "unexpected turns: {generations:?}");
    let body = generations[0].to_string();
    assert!(
        !body.contains(r#""media_type":"image/png""#),
        "no image should have been sent for a text-only pasteboard: {body}"
    );
    assert!(
        body.contains("CLIPBOARD-TEXT-PAYLOAD"),
        "the pasted text must reach the model as ordinary prompt text: {body}"
    );
}

/// How long every wait below gives the child to make progress.
///
/// One name for what was ten copies of the same eight seconds. Each use is a
/// liveness bailout and none is a pass condition: a wait that expires panics,
/// and the one deadline that does not — the post-EOF exit wait — kills the
/// child and then asserts on a status a killed child cannot have. So the value
/// only decides how long a genuine hang takes to become legible, and raising
/// it cannot turn a failing run green.
///
/// The gap it must clear is the child's, measured while the whole workspace's
/// test binaries run at once rather than on an idle machine. Eight seconds was
/// the idle figure: `live_surface_esc_expedites_sampling_once_and_reports_interrupt_state`
/// passes alone and failed under `cargo test --workspace` having seen only the
/// two idle prompt renders, on a startup gap rather than on anything it
/// asserts.
const PROGRESS_DEADLINE: Duration = Duration::from_secs(60);
/// The full slash catalogue previously exceeded 60 seconds under host load.
/// Its first unresponsive step still fails after at most 180 seconds.
const SLASH_ACTION_DEADLINE: Duration = Duration::from_secs(180);
const HELP_END: &str = "────────────────";

enum WriteRequest {
    Bytes(Vec<u8>),
    Flush,
}

struct PtyWriter {
    sender: mpsc::SyncSender<WriteRequest>,
}

impl Write for PtyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.sender
            .send(WriteRequest::Bytes(bytes.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PTY writer stopped"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sender
            .send(WriteRequest::Flush)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PTY writer stopped"))
    }
}

fn writer_owner(
    mut writer: Box<dyn Write + Send>,
) -> (
    PtyWriter,
    mpsc::SyncSender<WriteRequest>,
    thread::JoinHandle<()>,
) {
    let (sender, requests) = mpsc::sync_channel(32);
    let responder = sender.clone();
    let join = thread::spawn(move || {
        for request in requests {
            let result = match request {
                WriteRequest::Bytes(bytes) => writer.write_all(&bytes),
                WriteRequest::Flush => writer.flush(),
            };
            if result.is_err() {
                break;
            }
        }
    });
    (PtyWriter { sender }, responder, join)
}

fn stream(
    reader: Box<dyn Read + Send>,
    dsr_responder: mpsc::SyncSender<WriteRequest>,
) -> (mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    stream_with_screen(reader, dsr_responder, None)
}

fn stream_with_screen(
    reader: Box<dyn Read + Send>,
    dsr_responder: mpsc::SyncSender<WriteRequest>,
    screen_size: Option<(u16, u16)>,
) -> (mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let join = thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0; 1024];
        let mut pending = Vec::new();
        let mut screen = screen_size.map(|(rows, cols)| vt100::Parser::new(rows, cols, 0));
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(length) => {
                    if let Some(screen) = &mut screen {
                        screen.process(&chunk[..length]);
                    }
                    pending.extend_from_slice(&chunk[..length]);
                    while let Some(index) = pending
                        .windows(DSR_QUERY.len())
                        .position(|window| window == DSR_QUERY)
                    {
                        pending.drain(..index + DSR_QUERY.len());
                        let response = screen.as_ref().map_or_else(
                            || DSR_RESPONSE.to_vec(),
                            |screen| {
                                let (row, col) = screen.screen().cursor_position();
                                format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
                            },
                        );
                        if dsr_responder.send(WriteRequest::Bytes(response)).is_err()
                            || dsr_responder.send(WriteRequest::Flush).is_err()
                        {
                            return;
                        }
                    }
                    if pending.len() > DSR_QUERY.len() - 1 {
                        let keep = DSR_QUERY.len() - 1;
                        pending.drain(..pending.len() - keep);
                    }
                    if sender.send(chunk[..length].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (receiver, join)
}

fn until(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, needle: &[u8]) {
    until_with_context(receiver, output, needle, String::new);
}

fn until_from(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &[u8],
) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !output[start..]
        .windows(needle.len())
        .any(|window| window == needle)
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            drain(receiver, output);
            panic!(
                "raw PTY wait timed out for needle {:?}; output: {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&output[start..]),
            );
        }
        let chunk = receiver.recv_timeout(remaining).unwrap_or_else(|error| {
            drain(receiver, output);
            panic!(
                "raw PTY wait failed for needle {:?}: {error}; output: {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&output[start..]),
            )
        });
        output.extend(chunk);
    }
}

#[cfg(not(windows))]
fn rendered_prompt_count(output: &[u8]) -> usize {
    output.windows(2).filter(|window| *window == b"> ").count()
}

#[cfg(windows)]
fn rendered_prompt_count(output: &[u8]) -> usize {
    const WORKING: &[u8] = b"working \xe2\x80\x94 Esc cancels";
    const CANCELLING: &[u8] = b"cancelling active work";
    let is_busy = |position: usize| {
        let line_start = output[..position]
            .iter()
            .rposition(|byte| matches!(byte, b'\r' | b'\n'))
            .map_or(0, |position| position + 1);
        let prefix = &output[line_start..position];
        prefix
            .windows(WORKING.len())
            .any(|window| window == WORKING)
            || prefix
                .windows(CANCELLING.len())
                .any(|window| window == CANCELLING)
    };
    output
        .iter()
        .enumerate()
        .filter(|(position, byte)| {
            if **byte != b'>' || is_busy(*position) {
                return false;
            }
            let is_status_prompt = output
                .get(position.saturating_sub(2)..*position)
                .is_some_and(|prefix| prefix == b"| ");
            let tail = &output[position + 1..];
            is_status_prompt
                || tail.is_empty()
                || tail.starts_with(b"\x1b[K")
                || tail.starts_with(b"\x1b[1C")
                || tail.starts_with(b"\r")
                || tail.starts_with(b"\n")
        })
        .count()
}

fn until_prompt_from(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, start: usize) {
    until_prompt_from_with_context(receiver, output, start, PROGRESS_DEADLINE, "PTY prompt");
}

fn until_prompt_from_with_context(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    budget: Duration,
    context: &str,
) {
    let deadline = Instant::now() + budget;
    while rendered_prompt_count(&output[start..]) == 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            drain(receiver, output);
            panic!(
                "{context} wait timed out; raw output: {:?}; normalized output: {:?}",
                String::from_utf8_lossy(&output[start..]),
                normalized(&output[start..]),
            );
        }
        let chunk = receiver.recv_timeout(remaining).unwrap_or_else(|error| {
            drain(receiver, output);
            panic!(
                "{context} wait failed: {error}; raw output: {:?}; normalized output: {:?}",
                String::from_utf8_lossy(&output[start..]),
                normalized(&output[start..]),
            )
        });
        output.extend(chunk);
    }
}

fn until_local_action_completed(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    command: &str,
    budget: Duration,
) {
    let marker = format!("/{command}");
    // Reedline toggles bracketed paste around an accepted line; a redraw
    // prompt before that cycle does not mean the command has completed.
    let completed = |bytes: &[u8]| {
        let find_after = |bytes: &[u8], marker: &[u8]| {
            bytes
                .windows(marker.len())
                .position(|window| window == marker)
                .map(|position| position + marker.len())
        };
        let echoed = find_after(bytes, marker.as_bytes())?;
        let stopped = echoed + find_after(&bytes[echoed..], b"\x1b[?2004l")?;
        let resumed = stopped + find_after(&bytes[stopped..], b"\x1b[?2004h")?;
        (rendered_prompt_count(&bytes[resumed..]) > 0).then_some(resumed)
    };
    let deadline = Instant::now() + budget;
    while completed(&output[start..]).is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            drain(receiver, output);
            panic!(
                "/{command} terminal completion timed out; output: {:?}",
                normalized(&output[start..]),
            );
        }
        let chunk = receiver.recv_timeout(remaining).unwrap_or_else(|error| {
            drain(receiver, output);
            panic!(
                "/{command} terminal completion wait failed: {error}; output: {:?}",
                normalized(&output[start..]),
            )
        });
        output.extend(chunk);
    }
}

fn until_with_context(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    needle: &[u8],
    context: impl Fn() -> String,
) {
    while !output.windows(needle.len()).any(|window| window == needle) {
        let chunk = match receiver.recv_timeout(PROGRESS_DEADLINE) {
            Ok(chunk) => chunk,
            Err(error) => {
                drain(receiver, output);
                panic!(
                    "semantic PTY wait failed for needle {:?}: {error}; context: {}; raw output: {:?}; normalized output: {:?}",
                    String::from_utf8_lossy(needle),
                    context(),
                    String::from_utf8_lossy(output),
                    normalized(output),
                );
            }
        };
        output.extend(chunk);
    }
}

fn strip_csi(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 1 < bytes.len()
            && bytes[index] == b'\x1b'
            && matches!(bytes[index + 1], b'7' | b'8')
        {
            index += 2;
            continue;
        }
        if index + 1 < bytes.len() && bytes[index] == b'\x1b' && bytes[index + 1] == b'[' {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
            continue;
        }
        plain.push(bytes[index]);
        index += 1;
    }
    plain
}

fn normalized(bytes: &[u8]) -> String {
    let plain = strip_csi(bytes);
    let text = String::from_utf8_lossy(&plain).replace("\r\n", "\n");
    text.split('\n')
        .map(|line| {
            line.strip_prefix("> ")
                .or_else(|| line.strip_prefix("… "))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn until_normalized(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &str,
) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !normalized(&output[start..]).contains(needle) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            drain(receiver, output);
            panic!(
                "normalized PTY wait timed out for needle {:?}; raw output: {:?}; normalized output: {:?}",
                needle,
                String::from_utf8_lossy(&output[start..]),
                normalized(&output[start..]),
            );
        }
        let chunk = match receiver.recv_timeout(remaining) {
            Ok(chunk) => chunk,
            Err(error) => {
                drain(receiver, output);
                panic!(
                    "normalized PTY wait failed for needle {:?}: {error}; raw output: {:?}; normalized output: {:?}",
                    needle,
                    String::from_utf8_lossy(&output[start..]),
                    normalized(&output[start..]),
                );
            }
        };
        output.extend(chunk);
    }
}

fn until_normalized_or_generation(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &str,
    stub: &Stub,
) {
    while !normalized(&output[start..]).contains(needle) {
        let generations = stub.generations();
        if !generations.is_empty() {
            drain(receiver, output);
            panic!(
                "PTY input fell through to provider while waiting for {needle:?}; generations: {generations:?}; output: {:?}",
                normalized(&output[start..]),
            );
        }
        let chunk = receiver
            .recv_timeout(PROGRESS_DEADLINE)
            .unwrap_or_else(|error| {
                drain(receiver, output);
                panic!(
                    "PTY wait failed for {needle:?}: {error}; generations: {:?}; output: {:?}",
                    stub.generations(),
                    normalized(&output[start..]),
                )
            });
        output.extend(chunk);
    }
}

fn until_normalized_without_new_generation(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &str,
    stub: &Stub,
    generation_count: usize,
) {
    until_normalized_without_new_generation_with_context(
        receiver,
        output,
        start,
        needle,
        stub,
        generation_count,
        (PROGRESS_DEADLINE, "PTY output"),
    );
}

fn until_normalized_without_new_generation_with_context(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &str,
    stub: &Stub,
    generation_count: usize,
    (budget, context): (Duration, &str),
) {
    while !normalized(&output[start..]).contains(needle) {
        let generations = stub.generations();
        if generations.len() != generation_count {
            drain(receiver, output);
            panic!(
                "{context} changed provider generation count while waiting for {needle:?}; expected {generation_count}, generations: {generations:?}; output: {:?}",
                normalized(&output[start..]),
            );
        }
        let chunk = receiver.recv_timeout(budget).unwrap_or_else(|error| {
            drain(receiver, output);
            panic!(
                "{context} wait failed for {needle:?}: {error}; generations: {:?}; output: {:?}",
                stub.generations(),
                normalized(&output[start..]),
            )
        });
        output.extend(chunk);
    }
}

fn until_post_trust_prompt(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, start: usize) {
    until_prompt_from(receiver, output, start);
}

fn submit_local_action(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    input: &mut impl Write,
    stub: &Stub,
    command: &str,
    expected: Option<&str>,
) {
    let start = output.len();
    input
        .write_all(format!("/{command}\r").as_bytes())
        .unwrap_or_else(|error| panic!("write /{command}: {error}"));
    input
        .flush()
        .unwrap_or_else(|error| panic!("flush /{command}: {error}"));
    if let Some(expected) = expected {
        until_normalized_without_new_generation_with_context(
            receiver,
            output,
            start,
            expected,
            stub,
            0,
            (SLASH_ACTION_DEADLINE, &format!("/{command} action output")),
        );
    }
    if expected.is_none() {
        until_local_action_completed(receiver, output, start, command, SLASH_ACTION_DEADLINE);
    }
    assert!(
        stub.generations().is_empty(),
        "/{command} leaked into a provider generation"
    );
    if expected.is_none() {
        // A terminal repaint confirms Reedline accepted the command, but the
        // coordinator may still be consuming an earlier line. /help is a
        // visible driver round trip before the next action is submitted.
        submit_local_action(receiver, output, input, stub, "help", Some(HELP_END));
    }
}

#[test]
fn slash_action_stall_reports_command_and_waiting_step() {
    let stub = Stub::start(Plan::default());
    for (command, expected, step) in [
        ("help", Some("εgo commands"), "action output"),
        ("plan", None, "terminal completion"),
    ] {
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        let mut output = Vec::new();
        let mut input = Vec::new();
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            submit_local_action(&receiver, &mut output, &mut input, &stub, command, expected);
        }))
        .expect_err("a closed PTY output channel must stop the slash-action wait");
        let message = failure
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| failure.downcast_ref::<&str>().copied())
            .expect("panic with a diagnostic message");
        assert!(
            message.contains(&format!("/{command}")),
            "action missing: {message}"
        );
        assert!(message.contains(step), "waiting step missing: {message}");
    }
}

fn until_prompt_after(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, previous: usize) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while rendered_prompt_count(output) <= previous {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            drain(receiver, output);
            panic!(
                "prompt count did not advance beyond {previous}; output: {:?}",
                normalized(output),
            );
        }
        let chunk = receiver.recv_timeout(remaining).unwrap_or_else(|error| {
            drain(receiver, output);
            panic!(
                "prompt count did not advance beyond {previous}: {error}; output: {:?}",
                normalized(output),
            )
        });
        output.extend(chunk);
    }
}

fn drain(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>) {
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        match receiver.try_recv() {
            Ok(chunk) => output.extend(chunk),
            Err(_) => break,
        }
    }
}

/// Waits until an idle composer row (`>` alone) stands below the row showing
/// `marker`. A screen that lacks the streaming prompt is not enough: while the
/// reply prints, the prompt is briefly erased, so only the idle composer proves
/// the turn-end redraw arrived. A screen that never settles is left for the
/// caller's own assertion to report.
fn until_idle_composer_below(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    rows: u16,
    cols: u16,
    marker: &str,
) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    loop {
        let mut screen = vt100::Parser::new(rows, cols, 0);
        screen.process(output);
        let contents = screen.screen().contents();
        let settled = contents
            .lines()
            .skip_while(|line| !line.contains(marker))
            .any(|line| line.trim_end() == ">");
        if settled {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(chunk) => output.extend(chunk),
            Err(_) => return,
        }
    }
}

/// A raw PTY read may have already received a complete redraw while its reader
/// thread still holds the chunk. A key assertion begins only after that queue
/// is quiet, so an old marker cannot satisfy a later-key expectation.
fn drain_until_quiet(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(100)) {
        output.extend(chunk);
        assert!(Instant::now() < deadline, "PTY redraw never became quiet");
    }
}

fn count_exact_strings(value: &serde_json::Value, target: &str) -> usize {
    match value {
        serde_json::Value::String(value) => usize::from(value == target),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| count_exact_strings(value, target))
            .sum(),
        serde_json::Value::Object(values) => values
            .values()
            .map(|value| count_exact_strings(value, target))
            .sum(),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
    }
}

fn count_notes_write_results(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(value) => usize::from(
            value
                .strip_prefix("wrote 10 bytes to ")
                .is_some_and(|path| {
                    Path::new(path)
                        .file_name()
                        .is_some_and(|name| name == "notes.md")
                }),
        ),
        serde_json::Value::Array(values) => values.iter().map(count_notes_write_results).sum(),
        serde_json::Value::Object(values) => values.values().map(count_notes_write_results).sum(),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
    }
}

fn line_framed(reply: Reply, content: &str) -> Reply {
    let Reply::Paced {
        mut head,
        tail,
        pause,
    } = reply
    else {
        return reply;
    };
    let encoded = serde_json::to_string(content).expect("the marker is serialisable");
    let framed = serde_json::to_string(&format!("{content}\n"))
        .expect("the line-framed marker is serialisable");
    head = head.replace(
        &format!("\"content\":{encoded}"),
        &format!("\"content\":{framed}"),
    );
    Reply::Paced { head, tail, pause }
}

fn paced_line(content: &str, pause: Duration) -> Reply {
    line_framed(paced(content, pause), content)
}

fn gated_paced_line(content: &str) -> (Reply, Arc<ReleaseGate>) {
    let Reply::Paced { head, tail, .. } = paced_line(content, Duration::ZERO) else {
        unreachable!("paced_line always returns a paced reply");
    };
    let gate = ReleaseGate::new();
    (
        Reply::GatedPaced {
            head,
            tail,
            gate: Arc::clone(&gate),
        },
        gate,
    )
}

fn find(output: &[u8], needle: &[u8]) -> usize {
    output
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| panic!("missing PTY marker {:?}", String::from_utf8_lossy(needle)))
}

#[test]
fn live_surface_preserves_draft_through_busy_permission_and_resize() {
    #[cfg(not(windows))]
    let older_draft = "fn older_draft() {\n    println!(\"older e\\u{301} 👩‍💻\");\n}\n";
    #[cfg(windows)]
    let older_draft = "fn older_draft() { println!(\"older e\\u{301} 👩💻\"); }";
    #[cfg(not(windows))]
    let draft = "fn newest_draft() {\n    println!(\"newest e\\u{301} 👩‍💻\");\n}\n";
    #[cfg(windows)]
    let draft = "fn newest_draft() { println!(\"newest e\\u{301} 👩💻\"); }";
    let write_arguments = serde_json::json!({
        "path": "notes.md",
        "contents": "permission",
    })
    .to_string();
    let (paced_tool_call, provider_gate) =
        gated_paced_tool_call_line("write-1", "write", &write_arguments);
    let stub = Stub::start(Plan {
        completions: vec![
            paced_tool_call,
            Reply::Sse(text("NEWEST-DRAFT-RECEIVED")),
            Reply::Sse(text("OLDER-DRAFT-RECEIVED")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "default",
        "--sandbox",
        "off",
    ]);
    // These cases all begin by answering the trust question, so the workspace
    // has to hold something worth asking about. An empty checkout is asked
    // nothing at all.
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    #[cfg(unix)]
    {
        let tty = pair.master.tty_name().expect("slave PTY name");
        let option = if cfg!(target_os = "macos") {
            "-f"
        } else {
            "-F"
        };
        let setup = std::process::Command::new("stty")
            .arg(option)
            .arg(tty)
            .arg("ixon")
            .output()
            .expect("enable XOFF on the real slave PTY");
        assert!(
            setup.status.success(),
            "stty could not enable IXON: {}",
            String::from_utf8_lossy(&setup.stderr)
        );
    }
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let idle_start = output.len();
    until_prompt_from(&reader, &mut output, idle_start);

    #[cfg(not(windows))]
    input.write_all(b"\x1b[200~").expect("paste prefix");
    input
        .write_all(older_draft.as_bytes())
        .expect("paste older draft");
    #[cfg(not(windows))]
    input.write_all(b"\x1b[201~").expect("paste suffix");
    input.write_all(b"\x13").expect("stash older draft");
    input.flush().expect("stash older draft flush");
    let older_stash_start = output.len();
    until_prompt_from(&reader, &mut output, older_stash_start);
    #[cfg(not(windows))]
    input.write_all(b"\x1b[200~").expect("paste prefix");
    input
        .write_all(draft.as_bytes())
        .expect("paste newest draft");
    #[cfg(not(windows))]
    input.write_all(b"\x1b[201~").expect("paste suffix");
    input.write_all(b"\x13").expect("stash newest draft");
    input.flush().expect("stash newest draft flush");
    let newest_stash_start = output.len();
    // This next repaint follows XOFF without an XON byte. The real PTY must
    // deliver Ctrl+S to ego rather than freeze terminal output.
    until_prompt_from(&reader, &mut output, newest_stash_start);
    drain_until_quiet(&reader, &mut output);
    let local_action_start = output.len();
    input
        .write_all(b"/version\r")
        .expect("local version action");
    input.flush().expect("local version action flush");
    until_prompt_from(&reader, &mut output, local_action_start);
    drain_until_quiet(&reader, &mut output);
    assert!(
        !output[local_action_start..]
            .windows(b"newest_draft".len())
            .any(|window| window == b"newest_draft"),
        "a local action must leave parked input in the stash"
    );
    let newest_restore_start = output.len();
    input.write_all(b"initial\r").expect("initial prompt");
    input.flush().expect("initial prompt flush");
    until(&reader, &mut output, b"STREAMED-BEFORE-TOOL");
    until_from(&reader, &mut output, newest_restore_start, b"newest_draft");
    let older_restore_start = output.len();
    input.write_all(b"\r").expect("busy steer submit");
    input.flush().expect("busy steer flush");
    until_normalized(
        &reader,
        &mut output,
        0,
        "Messages to be submitted after next tool call (press esc to interrupt and send immediately)",
    );
    until_from(&reader, &mut output, older_restore_start, b"older_draft");
    provider_gate.release();
    until(&reader, &mut output, b"STREAMED-BEFORE-TOOL");
    until_with_context(&reader, &mut output, b"[y] allow [n] deny [e] edit", || {
        let requests = stub.requests();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.path.clone())
            .collect();
        format!("stub request paths: {paths:?}")
    });
    // The banner this wait is about to look for again is on screen already,
    // so a render the reader thread is still holding would satisfy the
    // post-resize wait with a pre-resize frame and prove nothing. The queue is
    // emptied before the marker is taken, for the reason `drain_until_quiet`
    // exists.
    drain_until_quiet(&reader, &mut output);
    let resize_start = output.len();
    pair.master
        .resize(PtySize {
            rows: 8,
            cols: 12,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");
    until_from(
        &reader,
        &mut output,
        resize_start,
        b"[y] allow [n] deny [e] edit",
    );
    input.write_all(b"y").expect("immediate permission answer");
    input.flush().expect("permission answer flush");
    until(&reader, &mut output, b"NEWEST-DRAFT-RECEIVED");
    input.write_all(b"\r").expect("submit restored older draft");
    input.flush().expect("older draft submit flush");
    until(&reader, &mut output, b"OLDER-DRAFT-RECEIVED");
    let unsent_draft_start = output.len();
    input.write_all(b"unsent draft").expect("type unsent draft");
    input.flush().expect("unsent draft flush");
    until_from(&reader, &mut output, unsent_draft_start, b"unsent draft");
    // `rendered_prompt_count` counts every `"> "`, and the frame echoing the
    // draft carries one. A re-render the reader thread was still holding would
    // therefore answer the wait below without the stash having happened, and
    // the EOF two lines down would then arrive at a line that still held text
    // — where Ctrl-D is not EOF at all, and the farewell it is waited on never
    // comes. The queue is emptied before the marker is taken.
    drain_until_quiet(&reader, &mut output);
    let unsent_stash_start = output.len();
    input.write_all(b"\x13").expect("stash unsent draft");
    input.flush().expect("stash unsent draft flush");
    until_post_trust_prompt(&reader, &mut output, unsent_stash_start);
    let manual_restore_start = output.len();
    input
        .write_all(b"\x13")
        .expect("restore from empty composer");
    input.flush().expect("manual restore flush");
    until_from(&reader, &mut output, manual_restore_start, b"unsent draft");
    let manual_restash_start = output.len();
    input.write_all(b"\x13").expect("park restored draft again");
    input.flush().expect("manual restash flush");
    until_post_trust_prompt(&reader, &mut output, manual_restash_start);
    pair.master
        .resize(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("restore terminal width");
    input.write_all(b"\x04").expect("EOF");
    input.flush().expect("EOF flush");
    until_normalized(&reader, &mut output, 0, "1 unsent draft");

    let status = bounded_exit(child);
    assert!(status.success(), "interactive process failed: {status:?}");
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
    drain(&reader, &mut output);

    assert!(!output.windows(8).any(|window| window == b"\x1b[?1049h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1000h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1006h"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?2004h"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?2004l"));

    let rendered = normalized(&output);
    for fact in [
        "Call: \"write\"",
        "✓ write notes.md",
        "Reason:",
        "Operations:",
        "Risk: Bounded",
        "Parameters:",
        "Remember would add:",
        "widening for operation 0",
    ] {
        assert!(
            rendered.contains(fact),
            "permission presentation omitted {fact:?}: {rendered}"
        );
    }

    let streamed = find(&output, b"STREAMED-BEFORE-TOOL");
    let permission = find(&output, b"[y] allow [n] deny [e] edit");
    let draft_received = find(&output, b"NEWEST-DRAFT-RECEIVED");
    let older_draft_received = find(&output, b"OLDER-DRAFT-RECEIVED");
    assert!(streamed < permission);
    assert!(permission < draft_received);
    assert!(draft_received < older_draft_received);

    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    let write_result_counts: Vec<_> = generations.iter().map(count_notes_write_results).collect();
    assert_eq!(
        write_result_counts,
        vec![0, 1, 1],
        "the tool result must reach each subsequent model request exactly once: {generations:#?}"
    );
    let draft_counts: Vec<_> = generations
        .iter()
        .map(|generation| count_exact_strings(generation, draft))
        .collect();
    assert_eq!(draft_counts, vec![0, 1, 1]);
    let older_draft_counts: Vec<_> = generations
        .iter()
        .map(|generation| count_exact_strings(generation, older_draft))
        .collect();
    assert_eq!(older_draft_counts, vec![0, 0, 1]);
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, "unsent draft"))
            .sum::<usize>(),
        0,
        "an unsent draft must never enter model history"
    );
    assert!(
        normalized(&output)
            .to_ascii_lowercase()
            .contains("1 unsent draft"),
        "exit must report the exact unsent-draft count rather than silently discarding it"
    );
}

#[cfg(unix)]
#[test]
fn interactive_exec_resolves_real_home_local_bin_from_effective_path() {
    use std::os::unix::fs::PermissionsExt as _;

    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "local-path-exec",
                "exec",
                r#"{"argv":["wiz-run","stories-cli.js","list"],"cwd":null,"timeout_ms":1000}"#,
            )),
            Reply::Sse(text("LOCAL-PATH-EXEC-DONE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
    ]);
    let executable = ego.home_file(
        ".local/bin/wiz-run",
        "#!/bin/sh\nprintf '%s\\n' WIZ-RUN-FROM-LOCAL-PATH\n",
    );
    let mut permissions = std::fs::metadata(&executable)
        .expect("local executable metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("make local fixture executable");
    let path = format!("{}:/usr/bin:/bin", executable.parent().unwrap().display());

    let finished = ego
        .env("PATH", &path)
        .run_pty("run the local story command");

    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    assert!(finished.stdout.contains("LOCAL-PATH-EXEC-DONE"));
    assert!(
        !finished.stdout.contains("[y] allow"),
        "yolo must not stop for a second confirmation: {}",
        finished.stdout
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 2);
    assert!(
        generations[1]
            .to_string()
            .contains("WIZ-RUN-FROM-LOCAL-PATH"),
        "the executable found on effective PATH did not run: {}",
        generations[1]
    );
}

#[cfg(unix)]
#[test]
fn busy_composer_stays_editable_and_reports_elapsed_tool_work() {
    use std::os::unix::fs::PermissionsExt as _;

    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "slow-exec",
                "exec",
                r#"{"argv":["slow-work"],"cwd":null,"timeout_ms":15000}"#,
            )),
            Reply::Sse(text("SLOW-TOOL-DONE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let executable = ego.home_file(
        ".local/bin/slow-work",
        "#!/bin/sh\nprintf started > \"$EGO_HOME/tool-started\"\ni=0\nwhile [ ! -f \"$EGO_HOME/tool-release\" ]; do\n  /bin/sleep 0.05\n  i=$((i + 1))\n  if [ \"$i\" -ge 300 ]; then exit 124; fi\ndone\nprintf 'TOOL-FINISHED\\n'\n",
    );
    let mut permissions = std::fs::metadata(&executable)
        .expect("slow tool metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("make slow tool executable");
    let path = format!("{}:/usr/bin:/bin", executable.parent().unwrap().display());
    let started = ego.home_path().join("tool-started");
    let release = ego.home_path().join("tool-release");
    let ego = ego.env("PATH", &path).env("TERM", "xterm-256color");

    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"run slow tool\r").expect("first turn");
    input.flush().expect("first turn flush");
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !started.exists() {
        assert!(
            Instant::now() < deadline,
            "tool did not start: {:?}",
            normalized(&output)
        );
        drain(&reader, &mut output);
        thread::sleep(Duration::from_millis(20));
    }

    let draft_start = output.len();
    input
        .write_all(b"draft while tool runs")
        .expect("busy draft");
    input.flush().expect("busy draft flush");
    until_from(&reader, &mut output, draft_start, b"draft while tool runs");
    drain_until_quiet(&reader, &mut output);
    let progress_start = output.len();
    thread::sleep(Duration::from_millis(2200));
    drain(&reader, &mut output);
    let progress = normalized(&output[progress_start..]);

    let finish_start = output.len();
    std::fs::write(&release, b"go").expect("release tool");
    until(&reader, &mut output, b"SLOW-TOOL-DONE");
    until_normalized(&reader, &mut output, finish_start, "qwen3 · ");
    let clear_start = output.len();
    input.write_all(b"\x03").expect("clear draft");
    input.flush().expect("clear draft flush");
    until_prompt_from(&reader, &mut output, clear_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    assert!(status.success(), "interactive process failed: {status:?}");
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
    assert!(
        progress.contains("thinking for "),
        "no elapsed progress while the tool ran: {progress:?}"
    );
    assert!(
        !stub
            .generations()
            .iter()
            .any(|generation| generation.to_string().contains("draft while tool runs")),
        "unsent busy draft reached the provider"
    );
}

#[test]
fn live_surface_esc_expedites_sampling_once_and_reports_interrupt_state() {
    let queued = "steer while sampling";
    let extension = "tail extension while expedited";
    let (initial, initial_gate) = gated_paced_line("STREAMED-BEFORE-ESC");
    let (expedited, expedited_gate) = gated_paced_line("EXPEDITED-RECEIVED");
    let stub = Stub::start(Plan {
        completions: vec![initial, expedited, Reply::Sse(text("EXTENSION-RECEIVED"))],
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
    // These cases all begin by answering the trust question, so the workspace
    // has to hold something worth asking about. An empty checkout is asked
    // nothing at all.
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    input.write_all(b"initial\r").expect("initial prompt");
    input.flush().expect("initial prompt flush");
    until(&reader, &mut output, b"STREAMED-BEFORE-ESC");

    input.write_all(queued.as_bytes()).expect("queue steer");
    input.write_all(b"\r").expect("queue steer submit");
    input.flush().expect("queue steer flush");
    until_normalized(
        &reader,
        &mut output,
        0,
        "Messages to be submitted after next tool call (press esc to interrupt and send immediately)",
    );

    let esc_start = output.len();
    input.write_all(b"\x1b").expect("Esc");
    input.flush().expect("Esc flush");
    until_normalized(&reader, &mut output, esc_start, "interrupt pending");
    initial_gate.release();
    until(&reader, &mut output, b"EXPEDITED-RECEIVED");
    // The queue banner this second steer will raise was already rendered once,
    // by the first steer, and that earlier render sits after `esc_start`.
    // Searching from there would match it and let the gate below open while
    // the extension was still being typed — a pass that depends on how much
    // output the reader thread happened to be holding, which is exactly how
    // this case failed under a loaded suite and passed alone. The marker is
    // taken once the queue is quiet, so nothing before the extension can
    // satisfy the wait for it.
    drain_until_quiet(&reader, &mut output);
    let extension_start = output.len();
    input
        .write_all(extension.as_bytes())
        .expect("extend expedited tail");
    input
        .write_all(b"\r")
        .expect("extend expedited tail submit");
    input.flush().expect("extend expedited tail flush");
    until_normalized(
        &reader,
        &mut output,
        extension_start,
        "Messages to be submitted after next tool call (press esc to interrupt and send immediately)",
    );
    let second_esc_start = output.len();
    input.write_all(b"\x1b").expect("Esc extension");
    input.flush().expect("Esc extension flush");
    until_normalized(&reader, &mut output, second_esc_start, "interrupt pending");
    expedited_gate.release();
    until(&reader, &mut output, b"EXTENSION-RECEIVED");
    until_prompt_from(&reader, &mut output, second_esc_start);
    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    let status = bounded_exit(child);
    assert!(status.success(), "interactive process failed: {status:?}");
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");

    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        3,
        "repeated Esc must not duplicate delivery"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, queued))
            .collect::<Vec<_>>(),
        vec![0, 1, 1],
        "the expedited tail is delivered once and then retained in history"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, extension))
            .collect::<Vec<_>>(),
        vec![0, 0, 1],
        "a tail extension is delivered once after the first marker"
    );
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1049h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1000h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1006h"));
}

#[test]
fn live_surface_meta_p_and_model_slash_share_one_modal_and_paste_stays_prompt() {
    // A real Ollama /api/tags response includes embedding-only rows. The
    // picker must project only chat models from that response.
    let tags = r#"{"models":[{"name":"qwen3","capabilities":["completion","tools"]},{"name":"embeddinggemma:300m-qat-q8_0","capabilities":["embedding"]}]}"#;
    let stub = Stub::start(Plan {
        tags_refusal: Some((200, tags.to_string())),
        completions: vec![Reply::Sse(text("PASTED-SLASH-PROMPT"))],
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
        ])
        .env("TERM", "xterm-256color");
    // These cases all begin by answering the trust question, so the workspace
    // has to hold something worth asking about. An empty checkout is asked
    // nothing at all.
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let startup_start = output.len();
    until_post_trust_prompt(&reader, &mut output, startup_start);
    assert!(
        !String::from_utf8_lossy(&output[startup_start..])
            .contains("the local action handler is not installed"),
        "activation must complete before the first live prompt"
    );
    until(&reader, &mut output, "qwen3 · ".as_bytes());
    drain_until_quiet(&reader, &mut output);

    let typing_start = output.len();
    input.write_all(b"immagini").expect("type a draft");
    input.flush().expect("draft flush");
    until_from(&reader, &mut output, typing_start, b"immagini");
    drain_until_quiet(&reader, &mut output);
    let typed_frame =
        String::from_utf8_lossy(&strip_csi(&output[typing_start..])).replace("\r\n", "\n");
    assert!(
        typed_frame.contains("\n> immagini"),
        "the native status must occupy a row above the composer; raw PTY: {:?}",
        String::from_utf8_lossy(&output[typing_start..])
    );
    let clear_start = output.len();
    input.write_all(b"\x03").expect("clear draft");
    input.flush().expect("clear draft flush");
    until_prompt_from(&reader, &mut output, clear_start);

    let resize_start = output.len();
    pair.master
        .resize(PtySize {
            rows: 8,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("narrow resize");
    until_from(&reader, &mut output, resize_start, "qwen3 · ".as_bytes());
    drain_until_quiet(&reader, &mut output);
    let narrow_start = output.len();
    input.write_all(b"short").expect("type in narrow PTY");
    input.flush().expect("narrow draft flush");
    until_from(&reader, &mut output, narrow_start, b"short");
    drain_until_quiet(&reader, &mut output);
    let narrow_frame =
        String::from_utf8_lossy(&strip_csi(&output[narrow_start..])).replace("\r\n", "\n");
    assert!(
        narrow_frame.contains("\n> short"),
        "resizing must keep status above the composer: {narrow_frame:?}"
    );
    let narrow_clear = output.len();
    input.write_all(b"\x15").expect("clear narrow draft");
    input.flush().expect("clear narrow draft flush");
    until_prompt_from(&reader, &mut output, narrow_clear);
    let restore_start = output.len();
    pair.master
        .resize(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("restore size");
    until_from(&reader, &mut output, restore_start, "qwen3 · ".as_bytes());
    drain_until_quiet(&reader, &mut output);
    let slash_start = output.len();
    input.write_all(b"/model\r").expect("model slash action");
    input.flush().expect("model slash action flush");
    until_normalized_or_generation(&reader, &mut output, slash_start, "Select model", &stub);
    drain_until_quiet(&reader, &mut output);
    let slash_modal = normalized(&output[slash_start..]);
    assert!(slash_modal.contains("Select model"));
    assert!(
        !slash_modal.contains("embeddinggemma"),
        "embedding-only models cannot appear in the chat picker: {slash_modal}"
    );
    let enter_start = output.len();
    input.write_all(b"\r").expect("model picker enter");
    input.flush().expect("model picker enter flush");
    until_normalized(
        &reader,
        &mut output,
        enter_start,
        "Select reasoning for ollama/qwen3",
    );
    drain_until_quiet(&reader, &mut output);
    // This test compares the two model entry points; it does not change the
    // persisted reasoning default, so close the follow-up picker explicitly.
    let slash_accept_start = output.len();
    input
        .write_all(b"\x1b")
        .expect("close model reasoning picker");
    input.flush().expect("close model reasoning picker flush");
    until_prompt_from(&reader, &mut output, slash_accept_start);

    let meta_start = output.len();
    // ConPTY translates the standard ESC-prefixed terminal spelling into a
    // Windows console Alt-P event; writing both bytes together keeps the PTY
    // observation atomic.
    input.write_all(b"\x1bp").expect("Meta-P");
    input.flush().expect("Meta-P flush");
    until_normalized(&reader, &mut output, meta_start, "Select model");
    drain_until_quiet(&reader, &mut output);
    let meta_modal = normalized(&output[meta_start..]);
    // Compare the complete latest menu, not the number of painter redraws.
    assert_eq!(
        meta_modal.rsplit_once("Select model").unwrap().1,
        slash_modal.rsplit_once("Select model").unwrap().1,
        "Meta-P and /model must present the same complete picker"
    );
    let meta_close_start = output.len();
    input.write_all(b"\x1b").expect("close Meta-P modal");
    input.flush().expect("close Meta-P modal flush");
    until_prompt_from(&reader, &mut output, meta_close_start);

    let composed_start = output.len();
    input.write_all("π".as_bytes()).expect("composed Option-P");
    input.flush().expect("composed Option-P flush");
    until_normalized(&reader, &mut output, composed_start, "Select model");
    drain_until_quiet(&reader, &mut output);
    let composed_modal = normalized(&output[composed_start..]);
    assert_eq!(
        composed_modal.rsplit_once("Select model").unwrap().1,
        slash_modal.rsplit_once("Select model").unwrap().1,
        "composed Option-P and /model must present the same complete picker"
    );
    let composed_close_start = output.len();
    input.write_all(b"\x1b").expect("close composed modal");
    input.flush().expect("close composed modal flush");
    until_prompt_from(&reader, &mut output, composed_close_start);

    #[cfg(not(windows))]
    {
        let pasted_start = output.len();
        input
            .write_all(b"\x1b[200~/model\x1b[201~\r")
            .expect("pasted slash");
        input.flush().expect("pasted slash flush");
        until_normalized(&reader, &mut output, pasted_start, "PASTED-SLASH-PROMPT");
        drain_until_quiet(&reader, &mut output);
        assert!(
            rendered_prompt_count(&output[pasted_start..]) > 0,
            "the composer must remain available after the pasted prompt"
        );
        assert!(
            !normalized(&output[pasted_start..]).contains("Select model"),
            "pasted slash text stays a prompt instead of opening the picker"
        );
        assert_eq!(stub.generations().len(), 1);
        assert!(stub.generations()[0].to_string().contains("/model"));
    }

    let draft_start = output.len();
    input
        .write_all(b"draft restored after picker")
        .expect("type modal draft");
    input.flush().expect("type modal draft flush");
    until_from(
        &reader,
        &mut output,
        draft_start,
        b"draft restored after picker",
    );
    let draft_modal_start = output.len();
    input.write_all(b"\x1bp").expect("Meta-P with draft");
    input.flush().expect("Meta-P with draft flush");
    until_normalized(&reader, &mut output, draft_modal_start, "Select model");
    let draft_close_start = output.len();
    input.write_all(b"\x1b").expect("close draft modal");
    input.flush().expect("close draft modal flush");
    until_from(
        &reader,
        &mut output,
        draft_close_start,
        b"draft restored after picker",
    );
    assert!(
        normalized(&output[draft_close_start..]).contains("draft restored after picker"),
        "the modal restores the exact composer text"
    );

    let cleanup_start = output.len();
    input.write_all(b"\x03").expect("clear restored draft");
    input.flush().expect("clear restored draft flush");
    until_prompt_from(&reader, &mut output, cleanup_start);
    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    let status = bounded_exit(child);
    assert!(status.success(), "interactive process failed: {status:?}");
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
}

#[test]
fn model_picker_title_remains_visible_in_a_short_terminal() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 8,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let activation_start = output.len();
    until_post_trust_prompt(&reader, &mut output, activation_start);
    drain_until_quiet(&reader, &mut output);
    let picker_start = output.len();
    input.write_all(b"/model\r").expect("model action");
    input.flush().expect("model action flush");
    until_normalized(&reader, &mut output, picker_start, "Select model");
    drain_until_quiet(&reader, &mut output);

    let mut screen = vt100::Parser::new(8, 40, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    assert!(
        visible.contains("Select model") && visible.contains("  3. "),
        "picker heading and three useful choices must fit the terminal: {visible:?}; raw: {:?}",
        String::from_utf8_lossy(&output[picker_start..])
    );

    let navigation_start = output.len();
    for index in 2..=8 {
        let move_start = output.len();
        input.write_all(b"\x1b[B").expect("navigate down");
        input.flush().expect("navigate down flush");
        until_from(
            &reader,
            &mut output,
            move_start,
            format!("> {index}.").as_bytes(),
        );
        drain_until_quiet(&reader, &mut output);
    }
    screen.process(&output[navigation_start..]);
    let visible = screen.screen().contents();
    assert!(
        visible.contains("Select model") && visible.contains("> 8."),
        "selection and heading must stay visible after scrolling: {visible:?}"
    );

    input.write_all(b"\x1b").expect("close picker");
    input.flush().expect("close picker flush");
    let prompt_start = output.len();
    until_prompt_from(&reader, &mut output, prompt_start);
    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
    assert!(
        normalized(&output).contains("Option+P models"),
        "startup banner must advertise the model picker key: {:?}",
        normalized(&output[activation_start..])
    );
}

#[test]
fn delayed_reply_uses_its_own_row_for_thinking_then_replaces_it_with_text() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Paced {
            head: String::new(),
            tail: text("ANSWER-ON-REPLY-ROW"),
            pause: Duration::from_secs(2),
        }],
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
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((24, 80)),
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"layout check\r").expect("submit turn");
    input.flush().expect("submit flush");
    until_normalized(&reader, &mut output, 0, "Esc cancels");
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(24, 80, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    let lines: Vec<_> = visible.lines().collect();
    let user = lines
        .iter()
        .position(|line| line.contains("> layout check"))
        .unwrap_or_else(|| {
            panic!(
                "user line absent: {visible:?}; raw: {:?}",
                String::from_utf8_lossy(&output)
            )
        });
    assert!(
        lines[user + 1].contains("Esc cancels"),
        "thinking must occupy the reply row: {visible:?}"
    );
    assert!(
        !lines[user + 1].contains('>'),
        "working row must have no input prompt: {visible:?}"
    );
    assert_eq!(
        visible.matches("qwen3 · ").count(),
        1,
        "status must occur once before the user turn: {visible:?}"
    );

    until_normalized(&reader, &mut output, 0, "ANSWER-ON-REPLY-ROW");
    // The turn-end redraw follows the reply text by more than the quiet window
    // on a slow host; wait for the idle composer instead of sampling the
    // streaming prompt.
    until_idle_composer_below(&reader, &mut output, 24, 80, "ANSWER-ON-REPLY-ROW");
    drain_until_quiet(&reader, &mut output);
    let mut final_screen = vt100::Parser::new(24, 80, 0);
    final_screen.process(&output);
    let visible = final_screen.screen().contents();
    let lines: Vec<_> = visible.lines().collect();
    let user = lines
        .iter()
        .position(|line| line.contains("> layout check"))
        .expect("user line");
    assert!(
        lines[user + 1].contains("ANSWER-ON-REPLY-ROW"),
        "reply replaces thinking at the same row: {visible:?}"
    );
    assert!(
        !visible.contains("Esc cancels"),
        "thinking must disappear when reply arrives: {visible:?}"
    );

    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
}

#[test]
fn wrapped_user_prompt_keeps_thinking_and_answer_on_the_next_row() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Paced {
            head: String::new(),
            tail: text("WRAPPED-ANSWER"),
            pause: Duration::from_secs(2),
        }],
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
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((24, 40)),
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input
        .write_all(b"a very long prompt that spans terminal rows and ends in tailmarker\r")
        .expect("wrapped turn");
    input.flush().expect("wrapped turn flush");
    until_normalized(&reader, &mut output, 0, "Esc cancels");
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(24, 40, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    let lines: Vec<_> = visible.lines().collect();
    let tail = lines
        .iter()
        .position(|line| line.contains("tailmarker"))
        .unwrap_or_else(|| panic!("wrapped prompt tail absent: {visible:?}"));
    assert!(
        lines[tail + 1].contains("thinking for") && !lines[tail + 1].contains('>'),
        "thinking must follow the final wrapped prompt row: {visible:?}"
    );

    until_normalized(&reader, &mut output, 0, "WRAPPED-ANSWER");
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(24, 40, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    let lines: Vec<_> = visible.lines().collect();
    let tail = lines
        .iter()
        .position(|line| line.contains("tailmarker"))
        .expect("wrapped prompt tail");
    assert!(
        lines[tail + 1].contains("WRAPPED-ANSWER"),
        "answer must replace thinking after wrapped prompt: {visible:?}"
    );

    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
}

#[test]
fn streamed_reply_keeps_escape_hint_visible_without_a_stale_thinking_row() {
    let (reply, gate) = gated_paced_line("PARTIAL-ANSWER\n");
    let stub = Stub::start(Plan {
        completions: vec![reply],
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
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((24, 80)),
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"stream check\r").expect("submit turn");
    input.flush().expect("submit flush");
    until_normalized(&reader, &mut output, 0, "PARTIAL-ANSWER");
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(24, 80, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    gate.release();
    let lines: Vec<_> = visible.lines().collect();
    let user = lines
        .iter()
        .position(|line| line.contains("> stream check"))
        .expect("user line");
    assert!(
        lines[user + 1].contains("PARTIAL-ANSWER"),
        "partial text must occupy the reply row: {visible:?}"
    );
    assert!(
        visible.contains("Esc cancels"),
        "the cancel hint must remain visible until the turn ends: {visible:?}"
    );
    assert!(
        !visible.contains("thinking"),
        "the thinking row must disappear after text starts: {visible:?}"
    );

    let finish_start = output.len();
    until_prompt_from(&reader, &mut output, finish_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
}

#[test]
fn cancelling_before_reply_clears_the_thinking_row() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Paced {
            head: String::new(),
            tail: text("TOO-LATE"),
            pause: Duration::from_secs(5),
        }],
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
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((24, 80)),
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"cancel check\r").expect("submit turn");
    input.flush().expect("submit flush");
    until_normalized(&reader, &mut output, 0, "Esc cancels");
    let cancel_start = output.len();
    input.write_all(b"\x1b").expect("escape");
    input.flush().expect("escape flush");
    until_normalized(&reader, &mut output, cancel_start, "qwen3 · ");
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(24, 80, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    let lines: Vec<_> = visible.lines().collect();
    let user = lines
        .iter()
        .position(|line| line.contains("> cancel check"))
        .expect("user line");
    assert!(
        !lines[user + 1].contains("thinking"),
        "cancelled work must not leave a stale thinking row: {visible:?}"
    );
    assert!(
        !visible.contains("TOO-LATE"),
        "a cancelled reply must not arrive: {visible:?}"
    );

    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    assert_eq!(
        status.exit_code(),
        130,
        "cancel exit: {status:?}; screen: {visible:?}"
    );
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
}

/// catches: an Esc that stops a turn without a word in the transcript, and a
/// `thinking for` row left behind in the scrollback.
#[test]
fn esc_during_a_turn_leaves_one_interrupted_receipt_and_no_working_row() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Paced {
            head: String::new(),
            tail: text("TOO-LATE"),
            pause: Duration::from_secs(5),
        }],
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
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((40, 100)),
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"cancel check\r").expect("submit turn");
    input.flush().expect("submit flush");
    until_normalized(&reader, &mut output, 0, "Esc cancels");
    let cancel_start = output.len();
    input.write_all(b"\x1b").expect("escape");
    input.flush().expect("escape flush");
    until_normalized(&reader, &mut output, cancel_start, "Interrupted by user");
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(40, 100, 0);
    screen.process(&output);
    let visible = screen.screen().contents();

    assert_eq!(
        visible.matches("Interrupted by user").count(),
        1,
        "one receipt: {visible:?}"
    );
    assert!(
        !visible.contains("thinking for") && !visible.contains("working for"),
        "no activity row stays behind: {visible:?}"
    );

    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
}

/// catches: Esc on a running tool leaving no tool row and no receipt, and its
/// `thinking for` row staying in the scrollback.
#[cfg(unix)]
#[test]
fn esc_on_a_running_tool_leaves_its_row_one_receipt_and_no_working_row() {
    use std::os::unix::fs::PermissionsExt as _;

    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(tool_call(
            "slow-exec",
            "exec",
            r#"{"argv":["slow-work"],"cwd":null,"timeout_ms":15000}"#,
        ))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let executable = ego.home_file(
        ".local/bin/slow-work",
        "#!/bin/sh\nprintf started > \"$EGO_HOME/tool-started\"\n/bin/sleep 30\n",
    );
    let mut permissions = std::fs::metadata(&executable)
        .expect("slow tool metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("make slow tool executable");
    let path = format!("{}:/usr/bin:/bin", executable.parent().unwrap().display());
    let started = ego.home_path().join("tool-started");
    let ego = ego.env("PATH", &path).env("TERM", "xterm-256color");

    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((40, 100)),
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"run slow tool\r").expect("submit turn");
    input.flush().expect("submit flush");
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !started.exists() {
        assert!(
            Instant::now() < deadline,
            "tool did not start: {:?}",
            normalized(&output)
        );
        drain(&reader, &mut output);
        thread::sleep(Duration::from_millis(20));
    }
    let cancel_start = output.len();
    input.write_all(b"\x1b").expect("escape");
    input.flush().expect("escape flush");
    until_normalized(&reader, &mut output, cancel_start, "Interrupted by user");
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(40, 100, 0);
    screen.process(&output);
    let visible = screen.screen().contents();

    assert_eq!(
        visible.matches("Interrupted by user").count(),
        1,
        "one receipt: {visible:?}"
    );
    assert!(
        visible.contains("slow-work"),
        "the cancelled tool keeps its row: {visible:?}"
    );
    assert!(
        !visible.contains("thinking for") && !visible.contains("working for"),
        "no activity row stays behind: {visible:?}"
    );

    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    let log = std::fs::read_to_string(
        ego.home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .expect("session log");
    assert!(
        log.contains(r#""kind":"ToolCallCompleted","ok":false"#)
            && !log.contains(r#""kind":"ToolCallCompleted","ok":true"#),
        "the cancelled call is settled as a failure in the durable record: {log}"
    );
    assert!(
        log.contains(r#""exit_reason":{"Cancelled":"User"}"#),
        "the turn is recorded as cancelled by the user: {log}"
    );
}

/// catches: the elapsed counter counting the time a person spent on the
/// permission modal.
#[cfg(unix)]
#[test]
fn the_working_counter_leaves_out_the_wait_on_a_permission_modal() {
    use std::os::unix::fs::PermissionsExt as _;

    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "slow-exec",
                "exec",
                r#"{"argv":["slow-work"],"cwd":null,"timeout_ms":15000}"#,
            )),
            Reply::Sse(text("SLOW-TOOL-DONE")),
        ],
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
    ego.file("AGENTS.md", "# live surface fixture\n");
    let executable = ego.home_file(
        ".local/bin/slow-work",
        "#!/bin/sh\nprintf started > \"$EGO_HOME/tool-started\"\ni=0\nwhile [ ! -f \"$EGO_HOME/tool-release\" ]; do\n  /bin/sleep 0.05\n  i=$((i + 1))\n  if [ \"$i\" -ge 300 ]; then exit 124; fi\ndone\n",
    );
    let mut permissions = std::fs::metadata(&executable)
        .expect("slow tool metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("make slow tool executable");
    let path = format!("{}:/usr/bin:/bin", executable.parent().unwrap().display());
    let started = ego.home_path().join("tool-started");
    let release = ego.home_path().join("tool-release");
    let ego = ego.env("PATH", &path).env("TERM", "xterm-256color");

    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"run slow tool\r").expect("submit turn");
    input.flush().expect("submit flush");
    until(&reader, &mut output, b"[y] allow [n] deny [e] edit");
    thread::sleep(Duration::from_secs(5));
    let allowed = output.len();
    input.write_all(b"y").expect("allow");
    input.flush().expect("allow flush");
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !started.exists() {
        assert!(
            Instant::now() < deadline,
            "tool did not start: {:?}",
            normalized(&output)
        );
        drain(&reader, &mut output);
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(2200));
    drain(&reader, &mut output);
    let after_allow = normalized(&output[allowed..]);
    std::fs::write(&release, b"go").expect("release tool");
    until(&reader, &mut output, b"SLOW-TOOL-DONE");
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");

    let counted: Vec<u64> = after_allow
        .split("thinking for ")
        .skip(1)
        .filter_map(|rest| rest.split('s').next()?.parse().ok())
        .collect();
    assert!(
        !counted.is_empty() && counted.iter().all(|seconds| *seconds <= 4),
        "the 5 s spent on the modal was counted: {counted:?} in {after_allow:?}"
    );
}

/// catches: Esc on the permission modal leaving no receipt, or two.
#[test]
fn esc_on_the_permission_modal_denies_the_call_and_leaves_one_receipt() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "slow-exec",
                "exec",
                r#"{"argv":["/bin/sh","-c","echo RAN-$((6*7))"],"cwd":null,"timeout_ms":15000}"#,
            )),
            Reply::Sse(text("AFTER-DENIAL")),
        ],
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
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((40, 100)),
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"run a tool\r").expect("submit turn");
    input.flush().expect("submit flush");
    until(&reader, &mut output, b"[y] allow [n] deny [e] edit");
    drain_until_quiet(&reader, &mut output);
    let escaped = output.len();
    input.write_all(b"\x1b").expect("escape");
    input.flush().expect("escape flush");
    until_normalized(&reader, &mut output, escaped, "Interrupted by user");
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(40, 100, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    assert_eq!(
        visible.matches("Interrupted by user").count(),
        1,
        "one receipt: {visible:?}"
    );
    assert!(!visible.contains("RAN-42"), "the call ran: {visible:?}");

    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
}

#[test]
fn current_hosted_models_are_visible_through_the_real_pty_catalogue() {
    let finished = Ego::nowhere()
        .subcommand("models")
        .arg("--json")
        .run_pty_to_exit();
    assert_eq!(finished.code, Some(0), "PTY output: {}", finished.stdout);
    let output = finished.report();
    let slugs: Vec<&str> = output["models"]
        .as_array()
        .expect("catalogue rows")
        .iter()
        .filter_map(|entry| entry["slug"].as_str())
        .collect();
    for current in [
        "openai/gpt-6-astra",
        "openai/gpt-6.1-sol",
        "openai/gpt-6-sol",
        "openai/gpt-6-luna",
        "openai-codex/gpt-6.1-sol",
        "openai-codex/gpt-6-sol",
        "anthropic/claude-fable-5-1",
        "anthropic/claude-opus-5-5",
        "anthropic/claude-sonnet-5-5",
        "anthropic/claude-sonnet-5",
        "anthropic/claude-haiku-4-5",
    ] {
        assert!(slugs.contains(&current), "missing {current} in {slugs:?}");
    }
    for superseded in [
        "openai/gpt-5.6-sol",
        "openai/gpt-5.6-terra",
        "openai/gpt-5.6-luna",
        "openai-codex/gpt-5.6-sol",
        "anthropic/claude-opus-5",
    ] {
        assert!(
            !slugs.contains(&superseded),
            "stale {superseded} in {slugs:?}"
        );
    }
}

// Catches: a later busy steer overtakes or duplicates a follow-up waiting in normal input.
#[test]
fn rapid_follow_ups_after_a_reply_are_durable_once_in_typed_order() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("CRITIC-FIRST-REPLY")),
            Reply::Sse(text("CRITIC-SECOND-REPLY")),
            Reply::Sse(text("CRITIC-THIRD-REPLY")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "critic first",
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust_start = output.len();
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, trust_start);
    until_normalized(&reader, &mut output, 0, "CRITIC-FIRST-REPLY");

    // The user types both lines in a single burst, without waiting for the
    // coordinator to change admission between them.
    let follow_up_start = output.len();
    input
        .write_all(b"critic second\rcritic third\r")
        .expect("rapid follow-ups");
    input.flush().expect("follow-up flush");
    until_normalized(&reader, &mut output, follow_up_start, "CRITIC-THIRD-REPLY");
    until_post_trust_prompt(&reader, &mut output, follow_up_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    assert!(
        !normalized(&output).contains("the interactive turn coordinator is closed"),
        "a follow-up was refused: {:?}",
        normalized(&output)
    );

    let events = std::fs::read_to_string(
        ego.home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .expect("durable session");
    let prompts: Vec<String> = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("event envelope"))
        .filter(|event| event["payload"]["event"]["kind"]["kind"] == "UserPrompt")
        .map(|event| {
            event["payload"]["event"]["kind"]["text"]
                .as_str()
                .expect("prompt text")
                .to_owned()
        })
        .collect();
    assert_eq!(prompts, ["critic first", "critic second", "critic third"]);
    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        3,
        "unexpected provider turns: {generations:?}"
    );
    assert_eq!(count_exact_strings(&generations[2], "critic second"), 1);
    assert_eq!(count_exact_strings(&generations[2], "critic third"), 1);
}

// Catches: closing busy admission before the UI becomes idle rejects the second typed follow-up.
#[test]
fn argv_seed_and_two_typed_follow_ups_are_distinct_provider_turns() {
    let seed = "argv seed";
    let first = "typed follow-up one";
    let second = "typed follow-up two";
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("ARGV-SEED-FIRST\nARGV-SEED-SECOND")),
            Reply::Sse(text("FOLLOW-UP-ONE-REPLY")),
            Reply::Sse(text("FOLLOW-UP-TWO-REPLY")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        seed,
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let post_trust_start = output.len();
    // An argv seed can complete before Reedline finishes its cursor-position
    // handshake. Sending the first typed line merely because provider output
    // arrived races that handshake and the terminal consumes the input as its
    // cursor response. The first post-trust DSR followed by a prompt is the
    // observable boundary at which typed input is safe.
    until_post_trust_prompt(&reader, &mut output, post_trust_start);
    until_normalized(&reader, &mut output, 0, "ARGV-SEED-SECOND");
    drain_until_quiet(&reader, &mut output);
    let visible =
        String::from_utf8_lossy(&strip_csi(&output[post_trust_start..])).replace("\r\n", "\n");
    let first_at = visible.find("ARGV-SEED-FIRST").expect("first streamed row");
    let second_at = visible
        .find("ARGV-SEED-SECOND")
        .expect("final streamed row");
    assert!(
        first_at < second_at,
        "streamed rows changed order: {visible:?}"
    );
    assert!(
        !visible[first_at..second_at].contains("\n> "),
        "the idle composer appeared before the final streamed row: {visible:?}"
    );

    let help_start = output.len();
    input.write_all(b"/help\r").expect("local help command");
    input.flush().expect("local help flush");
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        help_start,
        "/quit  Exit the interactive session",
        &stub,
        1,
    );
    until_post_trust_prompt(&reader, &mut output, help_start);

    let refusal_start = output.len();
    input
        .write_all(b"/mcp add\r")
        .expect("unsupported MCP command");
    input.flush().expect("unsupported MCP command flush");
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        refusal_start,
        "ego mcp add NAME",
        &stub,
        1,
    );
    until_post_trust_prompt(&reader, &mut output, refusal_start);

    let first_follow_up_start = output.len();
    input.write_all(first.as_bytes()).expect("first follow-up");
    input.write_all(b"\r").expect("first follow-up submit");
    input.flush().expect("first follow-up flush");
    until_normalized(&reader, &mut output, refusal_start, "FOLLOW-UP-ONE-REPLY");
    until_post_trust_prompt(&reader, &mut output, first_follow_up_start);

    let second_follow_up_start = output.len();
    input
        .write_all(second.as_bytes())
        .expect("second follow-up");
    input.write_all(b"\r").expect("second follow-up submit");
    input.flush().expect("second follow-up flush");
    until_normalized(&reader, &mut output, refusal_start, "FOLLOW-UP-TWO-REPLY");
    until_post_trust_prompt(&reader, &mut output, second_follow_up_start);

    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let status = loop {
        drain(&reader, &mut output);
        match status_receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(status)) => break status,
            Ok(Err(error)) => panic!("wait failed: {error}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("child waiter disconnected"),
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                drain(&reader, &mut output);
                killer.kill().expect("kill timed out child");
                match status_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("killed child did not exit")
                {
                    Ok(status) => break status,
                    Err(error) => panic!("wait after kill failed: {error}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    assert!(status.success(), "interactive process failed: {status:?}");
    drop(input);
    drop(pair);
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");

    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, seed))
            .collect::<Vec<_>>(),
        vec![1, 1, 1],
        "the argv seed is retained once in context and never replayed"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, first))
            .collect::<Vec<_>>(),
        vec![0, 1, 1],
        "the first typed follow-up is delivered once and retained"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, second))
            .collect::<Vec<_>>(),
        vec![0, 0, 1],
        "the second typed follow-up is delivered once"
    );
}

#[test]
fn busy_ctrl_c_cancels_work_and_idle_double_ctrl_c_exits() {
    let stub = Stub::start(Plan {
        completions: vec![
            paced_line("ACTIVE-WORK", Duration::from_secs(2)),
            Reply::Sse(text("AFTER-CANCEL")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "cancel this turn",
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_normalized(&reader, &mut output, 0, "ACTIVE-WORK");
    let cancel_start = output.len();
    input.write_all(b"\x03").expect("busy Ctrl-C");
    input.flush().expect("busy Ctrl-C flush");
    until_normalized(&reader, &mut output, cancel_start, "cancelling active work");

    input
        .write_all(b"after cancel\r")
        .expect("post-cancel prompt");
    input.flush().expect("post-cancel prompt flush");
    until_normalized(&reader, &mut output, cancel_start, "AFTER-CANCEL");
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"\x03").expect("first idle Ctrl-C");
    input.flush().expect("first idle Ctrl-C flush");
    thread::sleep(Duration::from_millis(50));
    input.write_all(b"\x03").expect("second idle Ctrl-C");
    input.flush().expect("second idle Ctrl-C flush");

    drop(input);
    drop(pair);
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let status = match status_receiver.recv_timeout(PROGRESS_DEADLINE) {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => panic!("wait failed: {error}"),
        Err(error) => {
            killer.kill().expect("kill timed out child");
            panic!("idle double Ctrl-C did not exit: {error}")
        }
    };
    assert!(
        double_ctrl_c_exit(&status),
        "interactive process failed: {status:?}"
    );
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");

    let generations = stub.generations();
    assert_eq!(generations.len(), 2);
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, "after cancel"))
            .collect::<Vec<_>>(),
        vec![0, 1],
    );
}

fn bounded_exit(mut child: Box<dyn portable_pty::Child + Send + Sync>) -> portable_pty::ExitStatus {
    let mut killer = child.clone_killer();
    let (sender, receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = sender.send(child.wait());
    });
    let status = match receiver.recv_timeout(PROGRESS_DEADLINE) {
        Ok(status) => status.expect("child wait"),
        Err(error) => {
            killer.kill().expect("kill stalled child");
            panic!("child did not exit within deadline: {error}");
        }
    };
    waiter.join().expect("child waiter panicked");
    status
}

#[test]
fn startup_picker_escape_exits_without_saving_a_model() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args(["--sandbox", "off"])
        .env("ANTHROPIC_API_KEY", "fixture-anthropic-key");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_normalized(&reader, &mut output, 0, "Select model");
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"\x1b").expect("startup Escape");
    input.flush().expect("startup Escape flush");
    let status = bounded_exit(child);
    assert_eq!(status.exit_code(), 130, "Escape must cancel startup");
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
    assert!(stub.generations().is_empty());
    assert!(!ego.home_path().join("config.toml").exists());
}

#[test]
fn startup_picker_routes_raw_keys_and_persists_the_explicit_choice_at_80x24() {
    startup_picker_routes_raw_keys_and_persists_the_explicit_choice(24, 80);
}

#[test]
fn startup_picker_routes_raw_keys_and_persists_the_explicit_choice_at_120x40() {
    startup_picker_routes_raw_keys_and_persists_the_explicit_choice(40, 120);
}

fn startup_picker_routes_raw_keys_and_persists_the_explicit_choice(rows: u16, cols: u16) {
    let stub = Stub::start(Plan::default());
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "# live surface fixture\n",
    )
    .expect("workspace fixture");
    let first = Ego::at(home.path(), workspace.path(), Some(&stub))
        .conversing()
        .args(["--sandbox", "off"])
        .env("OPENAI_API_KEY", "fixture-openai-key")
        .env("ANTHROPIC_API_KEY", "fixture-anthropic-key");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair
        .slave
        .spawn_command(first.pty_command())
        .expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let picker_start = output.len();
    until_normalized(&reader, &mut output, picker_start, "Select model");
    drain_until_quiet(&reader, &mut output);

    let down_start = output.len();
    input.write_all(b"\x1b[B").expect("startup picker down");
    input.flush().expect("startup picker down flush");
    until_from(
        &reader,
        &mut output,
        down_start,
        b"> 2. anthropic/claude-haiku-4-5",
    );
    drain_until_quiet(&reader, &mut output);

    let up_start = output.len();
    input.write_all(b"\x1b[A").expect("startup picker up");
    input.flush().expect("startup picker up flush");
    until_from(
        &reader,
        &mut output,
        up_start,
        b"> 1. anthropic/claude-fable-5-1",
    );
    drain_until_quiet(&reader, &mut output);

    let chosen_start = output.len();
    input.write_all(b"2").expect("startup picker number two");
    input.flush().expect("startup picker number two flush");
    until_from(
        &reader,
        &mut output,
        chosen_start,
        b"> 2. anthropic/claude-haiku-4-5",
    );
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"\r").expect("startup model accept");
    input.flush().expect("startup model accept flush");
    until_normalized(&reader, &mut output, chosen_start, "Select reasoning for");
    drain_until_quiet(&reader, &mut output);
    let reasoning_start = output.len();
    input.write_all(b"\r").expect("startup reasoning accept");
    input.flush().expect("startup reasoning accept flush");
    until_prompt_from(&reader, &mut output, reasoning_start);
    drain_until_quiet(&reader, &mut output);
    let after_pickers = normalized(&output[reasoning_start..]);
    let banner = after_pickers
        .find("εgo: ego")
        .unwrap_or_else(|| panic!("no banner after the pickers: {after_pickers:?}"));
    assert!(
        after_pickers[..banner].ends_with('\n') || banner == 0,
        "the banner shares a row with the picker: {after_pickers:?}"
    );

    let config = std::fs::read_to_string(home.path().join("config.toml"))
        .expect("startup selection persisted atomically");
    assert!(config.contains("model = \"anthropic/claude-haiku-4-5\""));
    assert!(config.contains("thinking = \"off\""));

    input.write_all(b"\x04").expect("first EOF");
    input.flush().expect("first EOF flush");
    let status = bounded_exit(child);
    assert!(
        status.success(),
        "first interactive process failed: {status:?}"
    );
    drop(input);
    drop(pair);
    reader_join.join().expect("first reader thread panicked");
    writer_join.join().expect("first writer thread panicked");

    let second = Ego::at(home.path(), workspace.path(), Some(&stub))
        .conversing()
        .args(["--sandbox", "off"])
        .env("OPENAI_API_KEY", "fixture-openai-key")
        .env("ANTHROPIC_API_KEY", "fixture-anthropic-key");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("second pty");
    let child = pair
        .slave
        .spawn_command(second.pty_command())
        .expect("second spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("second writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("second reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    let restart_start = 0;
    until_post_trust_prompt(&reader, &mut output, restart_start);
    assert!(
        !normalized(&output[restart_start..]).contains("Select model"),
        "persisted selection must bypass the startup picker: {}",
        normalized(&output[restart_start..]),
    );

    input.write_all(b"\x04").expect("second EOF");
    input.flush().expect("second EOF flush");
    let status = bounded_exit(child);
    assert!(
        status.success(),
        "second interactive process failed: {status:?}"
    );
    drop(input);
    drop(pair);
    reader_join.join().expect("second reader thread panicked");
    writer_join.join().expect("second writer thread panicked");
}

#[test]
fn slash_action_wait_ignores_a_redraw_prompt_before_command_completion() {
    // These frames preserve the observed /clear ordering from the live PTY:
    // the first prompt was painted before the command echo and Enter cycle.
    let redraw = b"\x1b[6n\x1b[4;1H\x1b[J\x1b[96m> \x1b[0m";
    let completion = b"\x1b[4;1H\x1b[J\x1b[96m> \x1b[37m/clear\x1b[0m\x1b[J\r\n\x1b[?2004l\x1b[?2004h\x1b[4;1H\x1b[J\x1b[96m> ";
    let (sender, receiver) = mpsc::channel();
    let (done, result) = mpsc::channel();
    sender.send(redraw.to_vec()).expect("first PTY frame");
    let waiter = thread::spawn(move || {
        let mut output = Vec::new();
        until_local_action_completed(&receiver, &mut output, 0, "clear", Duration::from_secs(2));
        done.send(output).expect("completion result");
    });
    assert!(
        result.recv_timeout(Duration::from_millis(100)).is_err(),
        "a redraw prompt was mistaken for /clear completion"
    );
    sender
        .send(completion.to_vec())
        .expect("completion PTY frame");
    let output = result
        .recv_timeout(Duration::from_secs(2))
        .expect("real command completion");
    assert!(output.ends_with(completion));
    waiter.join().expect("waiter thread");
}

#[test]
fn silent_slash_action_waits_for_a_driver_reply() {
    let (writes, requests) = mpsc::sync_channel(4);
    let (frames, receiver) = mpsc::channel();
    let (done, result) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let stub = Stub::start(Plan::default());
        let mut input = PtyWriter { sender: writes };
        let mut output = Vec::new();
        submit_local_action(&receiver, &mut output, &mut input, &stub, "clear", None);
        done.send(output).expect("completed action");
    });
    let next_write = || loop {
        match requests
            .recv_timeout(Duration::from_secs(2))
            .expect("PTY write")
        {
            WriteRequest::Bytes(bytes) => break bytes,
            WriteRequest::Flush => {}
        }
    };
    assert_eq!(next_write(), b"/clear\r");
    frames
        .send(b"\x1b[96m> /clear\x1b[J\r\n\x1b[?2004l\x1b[?2004h\x1b[96m> ".to_vec())
        .expect("clear terminal frame");
    assert_eq!(next_write(), b"/help\r");
    assert!(
        result.recv_timeout(Duration::from_millis(100)).is_err(),
        "the terminal frame alone must not finish a silent action"
    );
    frames
        .send(
            "\x1b[96m> /help\x1b[J\r\n\x1b[?2004l\x1b[?2004h\x1b[96m> \r\nεgo commands\r\n────────────────\r\n"
                .as_bytes()
                .to_vec(),
        )
        .expect("driver reply");
    let output = result
        .recv_timeout(Duration::from_secs(2))
        .expect("silent action and driver reply");
    assert!(normalized(&output).contains(HELP_END));
    waiter.join().expect("waiter thread");
}

#[test]
fn every_registered_slash_action_is_local_and_has_intentional_terminal_behavior() {
    const COMMANDS: [&str; 25] = [
        "help",
        "model",
        "thinking",
        "mode",
        "plan",
        "yolo",
        "permissions",
        "mcp",
        "sessions",
        "fork",
        "compact",
        "clear",
        "usage",
        "statusline",
        "export",
        "doctor",
        "quit",
        "new",
        "name",
        "goal",
        "tools",
        "expand",
        "skills",
        "hooks",
        "version",
    ];
    assert_eq!(ActionRegistry::final_surface().names(), COMMANDS);

    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# registered action PTY fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    let trust_start = output.len();
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_prompt_from(&reader, &mut output, trust_start);

    submit_local_action(
        &reader,
        &mut output,
        &mut input,
        &stub,
        "help",
        Some(HELP_END),
    );

    for (command, prompt) in [
        ("model", "Select model"),
        ("thinking", "Select reasoning for"),
        ("mode", "Choose execution mode"),
        ("permissions", "Choose permission operation"),
        ("statusline", "Choose status-line setup"),
    ] {
        let start = output.len();
        input
            .write_all(format!("/{command}\r").as_bytes())
            .unwrap_or_else(|error| panic!("write /{command}: {error}"));
        input.flush().expect("modal action flush");
        until_normalized_without_new_generation(&reader, &mut output, start, prompt, &stub, 0);
        let cancel_start = output.len();
        input.write_all(b"\x1b").expect("cancel modal action");
        input.flush().expect("cancel modal action flush");
        until_prompt_from(&reader, &mut output, cancel_start);
        drain_until_quiet(&reader, &mut output);
    }

    submit_local_action(&reader, &mut output, &mut input, &stub, "plan", None);
    submit_local_action(&reader, &mut output, &mut input, &stub, "yolo", None);

    for (command, expected) in [
        ("mcp", Some("mcp:")),
        ("sessions", Some("UPDATED")),
        ("fork", None),
        // This empty fixture cannot prepare compaction; wait for its outcome.
        ("compact", Some("compaction failed")),
        ("clear", None),
        ("usage", Some("usage as of")),
        ("export", None),
        ("doctor", Some("acp.extension:hold")),
        ("new", None),
        ("name", None),
        ("goal", None),
        ("tools", Some("tools:")),
        ("expand", Some("no tool call to expand yet")),
        ("skills", None),
        ("hooks", Some("no hooks are admitted")),
        ("version", Some("ego 0.1.0")),
    ] {
        submit_local_action(&reader, &mut output, &mut input, &stub, command, expected);
    }

    let quit_start = output.len();
    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    let status = bounded_exit(child);
    assert!(status.success(), "interactive process failed: {status:?}");
    drain(&reader, &mut output);
    assert!(
        normalized(&output[quit_start..]).contains("unsent drafts"),
        "/quit did not render its explicit exit accounting"
    );
    assert!(stub.generations().is_empty());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
}

/// Drive one interactive session, optionally through one answered turn, quit it
/// with `/quit` and return everything printed from the quit onwards.
fn output_after_quit(turn: Option<&str>) -> String {
    quit_session(turn).0
}

/// [`output_after_quit`], keeping the quit session's home and workspace.
fn quit_session(turn: Option<&str>) -> (String, Ego) {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("RESUME-HINT-ANSWER"))],
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
    ego.file("AGENTS.md", "# resume hint fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust_start = output.len();
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_prompt_from(&reader, &mut output, trust_start);
    if let Some(prompt) = turn {
        let turn_start = output.len();
        input
            .write_all(format!("{prompt}\r").as_bytes())
            .expect("turn");
        input.flush().expect("turn flush");
        until_normalized(&reader, &mut output, turn_start, "RESUME-HINT-ANSWER");
        until_prompt_from(&reader, &mut output, turn_start);
    }
    let quit_start = output.len();
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    drain(&reader, &mut output);
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    assert!(status.success(), "interactive process failed: {status:?}");
    (normalized(&output[quit_start..]), ego)
}

#[test]
fn a_clean_quit_leaves_a_session_that_opens_without_a_repair() {
    let (_, ego) = quit_session(Some("hello"));
    let workspace = std::fs::canonicalize(ego.workspace_path()).expect("canonical workspace");
    let store = ego_tools::store::JsonlStore::new(
        ego.home_path().join("sessions"),
        workspace,
        Box::new(ego_tools::store::OsIo),
    );
    let id = support::FIXTURE_SESSION_ID
        .parse()
        .expect("the fixture id is a session id");
    let repair = store.recover(&id).expect("recover");
    assert!(
        repair.is_clean(),
        "a clean /quit must not leave anything for the next open to repair: {repair:?}"
    );
}

#[test]
fn quitting_after_a_turn_prints_the_resume_hint_for_that_session() {
    let after_quit = output_after_quit(Some("hello"));
    assert!(
        after_quit.contains(&format!(
            "To resume: ego resume {}",
            support::FIXTURE_SESSION_ID
        )),
        "no resume hint at exit: {after_quit:?}"
    );
}

#[test]
fn quitting_without_any_turn_prints_no_resume_hint() {
    let after_quit = output_after_quit(None);
    assert!(
        after_quit.contains("unsent drafts"),
        "exit accounting missing, the run did not reach its exit: {after_quit:?}"
    );
    assert!(
        !after_quit.contains("ego resume"),
        "hint for a session that cannot be resumed: {after_quit:?}"
    );
}

#[test]
fn help_and_doctor_remain_local_after_80_60_and_40_column_resizes() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    let prompt_count = rendered_prompt_count(&output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_prompt_after(&reader, &mut output, prompt_count);

    for width in [80, 60, 40] {
        let prompt_count = rendered_prompt_count(&output);
        pair.master
            .resize(PtySize {
                rows: 40,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize");
        until_prompt_after(&reader, &mut output, prompt_count);

        let help_start = output.len();
        let prompt_count = rendered_prompt_count(&output);
        input.write_all(b"/help\r").expect("help command");
        input.flush().expect("help command flush");
        until_normalized_without_new_generation(
            &reader,
            &mut output,
            help_start,
            "Surface",
            &stub,
            0,
        );
        until_prompt_after(&reader, &mut output, prompt_count);

        let doctor_start = output.len();
        let prompt_count = rendered_prompt_count(&output);
        input.write_all(b"/doctor\r").expect("doctor command");
        input.flush().expect("doctor command flush");
        until_normalized_without_new_generation(
            &reader,
            &mut output,
            doctor_start,
            "acp.extension:hold",
            &stub,
            0,
        );
        until_prompt_after(&reader, &mut output, prompt_count);
    }

    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    let status = bounded_exit(child);
    assert!(status.success(), "interactive process failed: {status:?}");
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
    assert!(stub.generations().is_empty());
}

#[test]
fn live_surface_native_questions_cover_single_multiple_and_free_text() {
    let single = serde_json::json!({
        "prompt": "Single question",
        "mode": "single",
        "options": [{"id": "yes", "label": "Yes"}, {"id": "no", "label": "No"}],
        "free_text": false,
    })
    .to_string();
    let multiple = serde_json::json!({
        "prompt": "Multiple question",
        "mode": "multiple",
        "options": [{"id": "red", "label": "Red"}, {"id": "blue", "label": "Blue"}],
        "free_text": false,
    })
    .to_string();
    let free_text = serde_json::json!({
        "prompt": "Free question",
        "mode": "free_text",
        "options": [],
        "free_text": true,
    })
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("question-single", "ask_question", &single)),
            Reply::Sse(tool_call("question-multiple", "ask_question", &multiple)),
            Reply::Sse(tool_call("question-free", "ask_question", &free_text)),
            Reply::Sse(text("QUESTIONS-DONE")),
        ],
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
    // These cases all begin by answering the trust question, so the workspace
    // has to hold something worth asking about. An empty checkout is asked
    // nothing at all.
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let idle_start = output.len();
    until_prompt_from(&reader, &mut output, idle_start);

    input
        .write_all(b"ask questions\r")
        .expect("question prompt");
    input.flush().expect("question prompt flush");
    until_normalized(&reader, &mut output, idle_start, "Single question");
    // The selector is the first frame that names the question, so its option
    // rows may already be in `output` when the question text is matched.
    let single_choice_start = idle_start;
    until_from(&reader, &mut output, single_choice_start, b"> 1. Yes");
    input.write_all(b"2").expect("single selection");
    input.flush().expect("single selection flush");
    until_from(&reader, &mut output, single_choice_start, b"> 2. No");
    let single_submit_start = output.len();
    input.write_all(b"\r").expect("single answer");
    input.flush().expect("single answer flush");
    until_from(
        &reader,
        &mut output,
        single_submit_start,
        b"Multiple question",
    );
    let multiple_first_start = single_submit_start;
    until_from(&reader, &mut output, multiple_first_start, b"> [ ] 1. Red");
    input.write_all(b"1").expect("multiple first selection");
    input.flush().expect("multiple first selection flush");
    until_from(&reader, &mut output, multiple_first_start, b"> [x] 1. Red");
    let multiple_second_start = output.len();
    input.write_all(b"2").expect("multiple second selection");
    input.flush().expect("multiple second selection flush");
    until_from(
        &reader,
        &mut output,
        multiple_second_start,
        b"> [x] 2. Blue",
    );
    let multiple_submit_start = output.len();
    input.write_all(b"\r").expect("multiple answer");
    input.flush().expect("multiple answer flush");
    // The trailing space is the whole point. A tool call is announced by
    // printing its arguments, so `{"prompt":"Free question"}` renders the
    // question's own text one frame before the widget that asks it — and there
    // the text is followed by a quote, never by a space. Waiting on the bare
    // text matched that announcement, and under load the two frames were far
    // enough apart that the answer below was typed at the REPL instead, where
    // it became a steer message while the widget waited for an answer nobody
    // could still give. The single and multiple cases above survive the same
    // ambiguity only because each is followed by a wait on an option row,
    // which no announcement can contain; free text has no option row.
    until_normalized(
        &reader,
        &mut output,
        multiple_submit_start,
        "Free question ",
    );
    input.write_all(b"free answer\r").expect("free-text answer");
    input.flush().expect("free-text answer flush");
    until_normalized(
        &reader,
        &mut output,
        multiple_submit_start,
        "QUESTIONS-DONE",
    );
    until_prompt_from(&reader, &mut output, multiple_submit_start);

    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        4,
        "each answer resumes the same live turn"
    );
    assert!(
        count_exact_strings(&generations[1], "no") > 0,
        "the numeric single-choice answer reaches the model"
    );
    assert!(
        count_exact_strings(&generations[2], "[\"red\",\"blue\"]") > 0,
        "both numeric multiple-choice answers reach the model: {}",
        generations[2]
    );
    assert!(
        generations
            .iter()
            .any(|generation| generation.to_string().contains("free answer")),
        "the free-text answer reaches the model using the canonical answer value"
    );

    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    let status = bounded_exit(child);
    assert!(status.success(), "interactive process failed: {status:?}");
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
}

/// The terminal rows, read back through an emulator `cols` wide.
fn screen_rows(output: &[u8], rows: u16, cols: u16) -> Vec<String> {
    let mut screen = vt100::Parser::new(rows, cols, 0);
    screen.process(output);
    screen
        .screen()
        .contents()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// One `ask_question` turn on a 120-column terminal. `drive` answers the
/// question; the run returns what the model was sent and what the terminal
/// showed once the turn was over.
///
/// The oracle is the emulated screen and never the bytes ego wrote: a stray
/// fragment overwritten onto a wrapped row is visible there and nowhere else.
fn question_session(
    call: &str,
    drive: impl FnOnce(&mut PtyWriter, &mpsc::Receiver<Vec<u8>>, &mut Vec<u8>),
) -> (Vec<serde_json::Value>, Vec<String>) {
    const ROWS: u16 = 70;
    const COLS: u16 = 120;
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("question", "ask_question", call)),
            Reply::Sse(text("QUESTION-DONE")),
        ],
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
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# question fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((ROWS, COLS)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);

    let asked = output.len();
    input.write_all(b"ask\r").expect("turn");
    input.flush().expect("turn flush");
    drive(&mut input, &reader, &mut output);
    until_normalized(&reader, &mut output, asked, "QUESTION-DONE");
    until_prompt_from(&reader, &mut output, asked);
    drain_until_quiet(&reader, &mut output);
    let after_turn = screen_rows(&output, ROWS, COLS);

    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    (stub.generations(), after_turn)
}

/// Waits for the selector to be fully drawn and returns the screen.
fn selector_screen(reader: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>) -> Vec<String> {
    until_from(reader, output, 0, b"Up/Down or number selects");
    drain_until_quiet(reader, output);
    screen_rows(output, 70, 120)
}

#[test]
fn a_question_whose_call_json_wraps_shows_its_prompt_and_options_once_and_no_raw_json() {
    // The prompt alone is longer than a row, and so is the call JSON around it.
    let prompt = format!(
        "Which function would you like to add to calc.py? {}",
        "Pick exactly one of the candidates below. ".repeat(3)
    );
    let call = serde_json::json!({
        "prompt": prompt,
        "mode": "single",
        "options": [
            {"id": "subtract", "label": "subtract", "description": "Return the difference of two numbers"},
            {"id": "divide", "label": "divide", "description": "Return the quotient of two numbers"},
            {"id": "power", "label": "power"},
        ],
        "free_text": false,
    })
    .to_string();
    assert!(call.len() > 120, "the call must exceed one row");
    let (generations, after_turn) = question_session(&call, |input, reader, output| {
        let screen = selector_screen(reader, output);
        let joined = screen.join("\n");
        // Catches: a selector origin computed without soft wrap, which left a
        // fragment of the first option ("ubtract") after the wrapped call line.
        assert!(
            !joined.contains("ask_question") && !joined.contains(r#"{""#),
            "the raw call reached the screen: {joined}"
        );
        let with_fragment: Vec<&String> = screen
            .iter()
            .filter(|row| row.contains("ubtract"))
            .collect();
        assert_eq!(with_fragment.len(), 1, "stray option text: {joined}");
        assert_eq!(with_fragment[0].trim_end(), "> 1. subtract", "{joined}");
        assert_eq!(
            screen
                .iter()
                .filter(|row| row.contains("Which function would you like"))
                .count(),
            1,
            "the question is shown once: {joined}"
        );
        // Catches: the description dropped from the rendering.
        let option_row = |label: &str| {
            screen
                .iter()
                .position(|row| row.trim_end().ends_with(label))
                .unwrap_or_else(|| panic!("no {label:?} row: {joined}"))
        };
        assert_eq!(
            screen[option_row("1. subtract") + 1].trim(),
            "Return the difference of two numbers"
        );
        assert_eq!(
            screen[option_row("2. divide") + 1].trim(),
            "Return the quotient of two numbers"
        );
        assert!(
            screen[option_row("3. power") + 1].starts_with("Up/Down"),
            "an option without a description takes no extra row: {joined}"
        );
        // Catches: `Other` offered although the schema forbids free text.
        assert!(!joined.contains("Other"), "{joined}");
        input.write_all(b"\r").expect("answer");
        input.flush().expect("answer flush");
    });
    assert!(
        count_exact_strings(&generations[1], "subtract") > 0,
        "the highlighted option reaches the model: {}",
        generations[1]
    );
    let joined = after_turn.join("\n");
    assert!(
        !joined.contains("ask_question") && !joined.contains(r#"{""#),
        "a row repeated the raw call after the answer: {joined}"
    );
}

/// catches: the editor parsing a burst (`2` then Enter in one write) and running
/// only its first command, so the selector moved to option 2 and the Enter was
/// dropped, leaving the question unanswered.
#[test]
fn a_selector_applies_keys_typed_in_one_burst_in_order() {
    let call = serde_json::json!({
        "prompt": "Burst question",
        "mode": "single",
        "options": [{"id": "yes", "label": "Yes"}, {"id": "no", "label": "No"}],
        "free_text": false,
    })
    .to_string();
    let (generations, _) = question_session(&call, |input, reader, output| {
        selector_screen(reader, output);
        input.write_all(b"2\r").expect("burst answer");
        input.flush().expect("burst answer flush");
    });
    assert!(
        count_exact_strings(&generations[1], "no") > 0,
        "option 2 chosen by the burst reaches the model: {}",
        generations[1]
    );
    assert_eq!(
        count_exact_strings(&generations[1], "yes"),
        0,
        "the burst must not answer with the default option: {}",
        generations[1]
    );
}

#[test]
fn the_other_row_opens_a_text_entry_and_its_text_is_the_answer() {
    let call = serde_json::json!({
        "prompt": "Which function would you like to add?",
        "mode": "single",
        "options": [
            {"id": "subtract", "label": "subtract"},
            {"id": "divide", "label": "divide", "description": "Return the quotient of two numbers"},
        ],
        "free_text": true,
    })
    .to_string();
    let (generations, _) = question_session(&call, |input, reader, output| {
        let screen = selector_screen(reader, output);
        let joined = screen.join("\n");
        let other = screen
            .iter()
            .position(|row| row.trim_end().ends_with("3. Other"))
            .unwrap_or_else(|| panic!("no Other row: {joined}"));
        assert_eq!(screen[other + 1].trim(), "Type your own answer", "{joined}");
        let from = output.len();
        input.write_all(b"3").expect("select Other");
        input.flush().expect("select flush");
        until_from(reader, output, from, b"> 3. Other");
        input.write_all(b"\r").expect("open entry");
        input.flush().expect("open flush");
        until_normalized(reader, output, from, "Other: ");
        // Catches: free_text unreachable from the selector.
        input.write_all(b"modulo\r").expect("typed answer");
        input.flush().expect("typed flush");
    });
    assert!(
        count_exact_strings(&generations[1], "modulo") > 0,
        "the typed text reaches the model as the answer: {}",
        generations[1]
    );
}

/// catches: typed-ahead keys of one burst being dropped when the `Other`
/// entry opens, so the free text never reaches the entry.
#[test]
fn a_burst_that_opens_the_other_row_and_types_its_text_answers_with_it() {
    let call = serde_json::json!({
        "prompt": "Which function would you like to add?",
        "mode": "single",
        "options": [
            {"id": "subtract", "label": "subtract"},
            {"id": "divide", "label": "divide"},
        ],
        "free_text": true,
    })
    .to_string();
    let (generations, _) = question_session(&call, |input, reader, output| {
        selector_screen(reader, output);
        input.write_all(b"3\rhello\r").expect("one burst");
        input.flush().expect("burst flush");
    });
    assert!(
        count_exact_strings(&generations[1], "hello") > 0,
        "the typed-ahead text answers the Other row: {}",
        generations[1]
    );
}

/// catches: keys of a burst that follow the Enter opening the `Other` entry
/// staying in the shared selector queue, so backing out of the entry replays
/// them on the selector (a stale Enter reopens `Other` and swallows the next
/// answer as free text instead of choosing the option typed).
#[test]
fn keys_queued_behind_the_other_row_are_not_replayed_after_backing_out() {
    let call = serde_json::json!({
        "prompt": "Which function would you like to add?",
        "mode": "single",
        "options": [
            {"id": "subtract", "label": "subtract"},
            {"id": "divide", "label": "divide"},
        ],
        "free_text": true,
    })
    .to_string();
    let (generations, _) = question_session(&call, |input, reader, output| {
        selector_screen(reader, output);
        let from = output.len();
        input.write_all(b"3\r\r").expect("burst into Other");
        input.flush().expect("burst flush");
        until_normalized(reader, output, from, "Other: ");
        drain_until_quiet(reader, output);
        // The burst's second Enter is a blank submit when the entry has read
        // it; otherwise this one is. Either way the selector is back, once.
        let screen = normalized(&output[from..]);
        let backed_out = screen
            .rfind("Up/Down or number selects")
            .zip(screen.rfind("Other: "))
            .is_some_and(|(selector, entry)| selector > entry);
        if !backed_out {
            let backed = output.len();
            input.write_all(b"\r").expect("blank line backs out");
            input.flush().expect("back flush");
            until_from(reader, output, backed, b"Up/Down or number selects");
            drain_until_quiet(reader, output);
        }
        input.write_all(b"1\r").expect("answer on the selector");
        input.flush().expect("answer flush");
    });
    assert!(
        count_exact_strings(&generations[1], "subtract") > 0,
        "after backing out, `1` Enter chooses option 1: {}",
        generations[1]
    );
}

/// What one scripted tool turn left on a real terminal screen.
struct ToolTurnScreens {
    /// The screen once the turn has ended and the composer is back.
    after_turn: Vec<String>,
    /// The screen after the expand key, when one was pressed.
    after_expand: Vec<String>,
}

const TOOL_TURN_DONE: &str = "TOOLS-DONE";

/// Run one interactive turn whose model calls the given tools, on a screen
/// `cols` wide, and read the terminal back through an emulator.
///
/// The oracle is the emulated screen and never the bytes ego wrote: a row that
/// wrapped, overwrote the composer or arrived as raw JSON is visible there and
/// nowhere else.
fn tool_turn(
    cols: u16,
    files: &[(&str, &str)],
    calls: &[(&str, &str)],
    expand: bool,
) -> ToolTurnScreens {
    let mut completions: Vec<Reply> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            Reply::Sse(tool_call(&format!("call-{index}"), name, arguments))
        })
        .collect();
    completions.push(Reply::Sse(text(TOOL_TURN_DONE)));
    let stub = Stub::start(Plan {
        completions,
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--mode",
            "yolo",
            "--sandbox",
            "off",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# tool row fixture\n");
    for (name, contents) in files {
        ego.file(name, contents);
    }
    let rows = 70;
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((rows, cols)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);

    let start = output.len();
    input.write_all(b"use the tools\r").expect("turn");
    input.flush().expect("turn flush");
    until_normalized(&reader, &mut output, start, TOOL_TURN_DONE);
    let screen_lines = |output: &[u8]| {
        let mut screen = vt100::Parser::new(rows, cols, 0);
        screen.process(output);
        screen
            .screen()
            .contents()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    // The answer arrives before the composer is idle again: the screen is
    // read only once its last row is the idle composer.
    let idle_by = Instant::now() + PROGRESS_DEADLINE;
    let after_turn = loop {
        drain_until_quiet(&reader, &mut output);
        let lines = screen_lines(&output);
        if lines.last().is_some_and(|line| line.starts_with('>')) {
            break lines;
        }
        assert!(Instant::now() < idle_by, "composer never idle: {lines:?}");
    };

    let mut after_expand = Vec::new();
    if expand {
        let start = output.len();
        input.write_all(b"\x1bo").expect("Meta-o");
        input.flush().expect("expand flush");
        // The last line of the whole result, indented under its row.
        until_normalized(&reader, &mut output, start, "  20");
        drain_until_quiet(&reader, &mut output);
        after_expand = screen_lines(&output);
    }

    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
    ToolTurnScreens {
        after_turn,
        after_expand,
    }
}

/// A row is a tool row when its first cell is an outcome mark.
fn tool_rows(screen: &[String]) -> Vec<&str> {
    screen
        .iter()
        .map(String::as_str)
        .filter(|line| line.starts_with('✓') || line.starts_with('✗'))
        .collect()
}

/// The row ends with a duration such as `12ms`, `0.4s` or `1m05s`.
fn ends_with_duration(row: &str) -> bool {
    let last = row.split_whitespace().last().unwrap_or("");
    last.starts_with(|c: char| c.is_ascii_digit())
        && last.ends_with('s')
        && last
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'm' | 's'))
}

const READ_CALL: (&str, &str) = ("read", r#"{"path":"calc.py"}"#);
const EDIT_CALL: (&str, &str) = (
    "edit",
    r#"{"path":"calc.py","old_string":"a = 1","new_string":"a = 1\nc = 3\nd = 4"}"#,
);
const SEQ_CALL: (&str, &str) = ("exec", r#"{"argv":["seq","1","20"]}"#);

#[test]
fn read_edit_and_exec_each_leave_one_compact_row_and_no_raw_call_json() {
    let screens = tool_turn(
        120,
        &[("calc.py", "a = 1\nb = 2\n")],
        &[READ_CALL, EDIT_CALL, SEQ_CALL],
        false,
    );
    let rows = tool_rows(&screens.after_turn);
    let joined = screens.after_turn.join("\n");
    assert_eq!(rows.len(), 3, "one row per call: {joined}");
    assert!(
        rows[0].starts_with("✓ read calc.py"),
        "read row: {:?}",
        rows[0]
    );
    assert!(
        rows[1].starts_with("✓ edit calc.py +3 -1"),
        "edit row: {:?}",
        rows[1]
    );
    assert!(
        rows[2].starts_with("✓ $ seq 1 20"),
        "exec row: {:?}",
        rows[2]
    );
    for row in &rows {
        assert!(ends_with_duration(row), "no duration on {row:?}");
    }
    assert!(
        !joined.contains(r#"{"path""#) && !joined.contains(r#""argv""#),
        "raw call JSON reached the transcript: {joined}"
    );
}

#[test]
fn exec_row_shows_exit_status_and_a_bounded_tail_and_the_expand_key_shows_all() {
    let screens = tool_turn(120, &[], &[SEQ_CALL], true);
    let trimmed = |screen: &[String]| {
        screen
            .iter()
            .map(|l| l.trim().to_owned())
            .collect::<Vec<_>>()
    };
    let before = trimmed(&screens.after_turn);
    assert!(
        tool_rows(&screens.after_turn)
            .first()
            .is_some_and(|row| row.contains("exit 0")),
        "exit status missing: {before:?}"
    );
    for shown in ["18", "19", "20"] {
        assert!(
            before.iter().any(|l| l == shown),
            "tail {shown} missing: {before:?}"
        );
    }
    assert!(
        !before.iter().any(|l| l == "10"),
        "the tail is unbounded: {before:?}"
    );
    assert!(
        before.iter().any(|l| l.contains("more lines")),
        "no hint that lines were cut: {before:?}"
    );
    let after = trimmed(&screens.after_expand);
    for line in 1..=20 {
        assert!(
            after.iter().any(|l| *l == line.to_string()),
            "line {line} missing after expand: {after:?}"
        );
    }
}

#[test]
fn a_failing_call_shows_the_failure_and_never_a_success_mark() {
    let screens = tool_turn(
        120,
        &[],
        &[
            ("exec", r#"{"argv":["sh","-c","echo boom >&2; exit 3"]}"#),
            ("read", r#"{"path":"missing.py"}"#),
        ],
        false,
    );
    let rows = tool_rows(&screens.after_turn);
    let joined = screens.after_turn.join("\n");
    assert_eq!(rows.len(), 2, "one row per call: {joined}");
    assert!(
        rows[0].starts_with('✗') && rows[0].contains("exit 3"),
        "failed exec row: {:?}",
        rows[0]
    );
    assert!(
        rows[1].starts_with("✗ read missing.py"),
        "failed read row: {:?}",
        rows[1]
    );
    assert!(
        !rows.iter().any(|row| row.contains('✓')),
        "a failed call carries a success mark: {rows:?}"
    );
    assert!(joined.contains("boom"), "stderr tail missing: {joined}");
}

#[test]
fn rows_fit_the_terminal_at_120_80_60_and_40_columns() {
    let long_arg = "a".repeat(130);
    let exec = format!(r#"{{"argv":["echo","{long_arg}"]}}"#);
    let long_missing = format!(r#"{{"path":"no-such-directory/{}.txt"}}"#, "b".repeat(90));
    for cols in [120u16, 80, 60, 40] {
        let screens = tool_turn(
            cols,
            &[("calc.py", "a = 1\n")],
            &[READ_CALL, ("exec", &exec), ("read", &long_missing)],
            false,
        );
        let joined = screens.after_turn.join("\n");
        let rows = tool_rows(&screens.after_turn);
        assert_eq!(rows.len(), 3, "{cols} cols, one row per call: {joined}");
        for row in rows {
            assert!(
                ends_with_duration(row),
                "{cols} cols: row lost its outcome to a soft wrap: {row:?}\n{joined}"
            );
        }
        let done = screens
            .after_turn
            .iter()
            .position(|line| line.contains(TOOL_TURN_DONE))
            .unwrap_or_else(|| panic!("{cols} cols: answer missing: {joined}"));
        assert!(
            screens.after_turn[done + 1..]
                .iter()
                .any(|line| line.starts_with('>')),
            "{cols} cols: the composer is not below the answer: {joined}"
        );
    }
}

const PICKER_ROWS: u16 = 40;
/// Three chat models around the session's `qwen3`, so the current model is
/// neither the first nor the last row.
const PICKER_TAGS: &str = r#"{"models":[{"name":"aaa-first","capabilities":["completion","tools"]},{"name":"qwen3","capabilities":["completion","tools"]},{"name":"zzz-last","capabilities":["completion","tools"]}]}"#;

/// A live terminal whose oracle is the emulated screen, never the bytes ego
/// wrote.
struct Term<'a> {
    input: &'a mut PtyWriter,
    reader: &'a mpsc::Receiver<Vec<u8>>,
    output: &'a mut Vec<u8>,
    cols: u16,
}

impl Term<'_> {
    fn send(&mut self, bytes: &[u8]) {
        self.input.write_all(bytes).expect("terminal input");
        self.input.flush().expect("terminal input flush");
    }

    fn screen(&self) -> Vec<String> {
        screen_rows(self.output, PICKER_ROWS, self.cols)
    }

    /// Reads until the emulated screen satisfies `ready`, lets the redraw
    /// settle, and returns the screen.
    fn until(&mut self, what: &str, ready: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + PROGRESS_DEADLINE;
        while !ready(&self.screen()) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.reader.recv_timeout(remaining) {
                Ok(chunk) => self.output.extend(chunk),
                Err(_) => panic!("never saw {what}; screen:\n{}", self.screen().join("\n")),
            }
        }
        drain_until_quiet(self.reader, self.output);
        self.screen()
    }

    fn close_picker(&mut self) {
        self.send(b"\x1b");
        self.until("the picker to close", |screen| {
            !screen.iter().any(|row| row.starts_with("Select "))
        });
    }
}

/// One session at `cols` columns, past the trust question, with the picker
/// catalogue above. `drive` leaves the composer idle.
fn picker_session(cols: u16, drive: impl FnOnce(&mut Term<'_>, &Stub)) {
    let stub = Stub::start(Plan {
        tags_refusal: Some((200, PICKER_TAGS.to_string())),
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
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# picker fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: PICKER_ROWS,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((PICKER_ROWS, cols)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    drain_until_quiet(&reader, &mut output);
    drive(
        &mut Term {
            input: &mut input,
            reader: &reader,
            output: &mut output,
            cols,
        },
        &stub,
    );
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
}

/// The picker's own rows: everything below its heading.
fn picker_rows(screen: &[String]) -> &[String] {
    let heading = screen
        .iter()
        .rposition(|row| row.starts_with("Select "))
        .expect("a picker heading is on screen");
    &screen[heading..]
}

#[test]
fn the_pickers_open_on_the_current_value_name_it_and_hint_the_session_key_at_step_one() {
    picker_session(120, |term, _stub| {
        term.send(b"/model\r");
        let screen = term.until("the model picker", |screen| {
            screen.iter().any(|row| row.contains("Esc cancels"))
        });
        let rows = picker_rows(&screen);
        let row = |needle: &str| {
            rows.iter()
                .find(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("no {needle:?} row:\n{}", screen.join("\n")))
        };
        // Catches: the cursor fixed on row 1 and no marker on the current model.
        assert!(row("2. ollama/qwen3").starts_with("> "), "{screen:#?}");
        assert!(row("2. ollama/qwen3").contains("✓ current"), "{screen:#?}");
        assert!(row("1. ollama/aaa-first").starts_with("  "), "{screen:#?}");
        assert!(
            !row("1. ollama/aaa-first").contains("current"),
            "{screen:#?}"
        );
        // Catches: the session-only key explained only at the reasoning step.
        assert!(
            rows.join("\n").contains("s = this session only"),
            "{screen:#?}"
        );

        term.send(b"\r");
        let screen = term.until("the reasoning step", |screen| {
            screen
                .iter()
                .any(|row| row.starts_with("Select reasoning for ollama/qwen3"))
        });
        // Catches: no current effort marked after choosing the current model.
        assert_eq!(
            picker_rows(&screen)
                .iter()
                .filter(|row| row.contains("✓ current"))
                .count(),
            1,
            "{screen:#?}"
        );
        term.close_picker();

        term.send(b"/thinking\r");
        let screen = term.until("the effort picker", |screen| {
            screen
                .iter()
                .any(|row| row.starts_with("Select reasoning for ollama/qwen3"))
        });
        // Catches: the effort picker's cursor and marker on different rows.
        let marked = picker_rows(&screen)
            .iter()
            .find(|row| row.contains("✓ current"))
            .unwrap_or_else(|| panic!("no current effort:\n{}", screen.join("\n")));
        assert!(marked.starts_with("> "), "{screen:#?}");
        term.close_picker();
    });
}

#[test]
fn typing_filters_the_model_list_while_digits_select_only_before_the_filter_starts() {
    picker_session(120, |term, _stub| {
        term.send(b"/model\r");
        term.until("the model picker", |screen| {
            screen.iter().any(|row| row.contains("Esc cancels"))
        });
        // Catches: type-to-search that takes digits before any filter exists.
        term.send(b"1");
        term.until("row 1 selected by number", |screen| {
            screen
                .iter()
                .any(|row| row.starts_with("> 1. ollama/aaa-first"))
        });

        for key in [b"z", b"z", b"z"] {
            term.send(key);
        }
        let screen = term.until("the zzz filter", |screen| {
            screen.iter().any(|row| row.contains("filter: zzz"))
        });
        let rows = picker_rows(&screen).join("\n");
        assert!(rows.contains("ollama/zzz-last"), "{screen:#?}");
        assert!(!rows.contains("ollama/aaa-first"), "{screen:#?}");
        assert!(!rows.contains("ollama/qwen3"), "{screen:#?}");

        // Catches: digits still selecting rows once a filter has started, which
        // makes `qwen3` impossible to type.
        for _ in 0..3 {
            term.send(b"\x7f");
        }
        for key in [b"q", b"w", b"e", b"n", b"3"] {
            term.send(key);
        }
        let screen = term.until("the qwen3 filter", |screen| {
            screen.iter().any(|row| row.contains("filter: qwen3"))
        });
        assert!(
            picker_rows(&screen)
                .iter()
                .any(|row| row.starts_with("> 1. ollama/qwen3")),
            "{screen:#?}"
        );
        term.close_picker();
    });
}

#[test]
fn a_filter_with_no_match_shows_an_empty_state_and_enter_does_nothing() {
    picker_session(120, |term, _stub| {
        term.send(b"/model\r");
        term.until("the model picker", |screen| {
            screen.iter().any(|row| row.contains("Esc cancels"))
        });
        // A row selected before the filter emptied the list must not survive it.
        term.send(b"2");
        term.until("row 2 selected", |screen| {
            screen
                .iter()
                .any(|row| row.starts_with("> 2. ollama/qwen3"))
        });
        term.send(b"k");
        term.send(b"k");
        let screen = term.until("the empty state", |screen| {
            screen.iter().any(|row| row.contains("No model matches"))
        });
        assert!(
            !picker_rows(&screen)
                .iter()
                .any(|row| row.contains(". ollama/")),
            "{screen:#?}"
        );
        // Catches: Enter continuing with the stale row.
        term.send(b"\r");
        std::thread::sleep(Duration::from_millis(500));
        drain_until_quiet(term.reader, term.output);
        let screen = term.screen();
        assert!(
            screen.iter().any(|row| row.contains("No model matches")),
            "{screen:#?}"
        );
        assert!(
            !screen.iter().any(|row| row.starts_with("Select reasoning")),
            "{screen:#?}"
        );
        term.close_picker();
    });
}

#[test]
fn choosing_a_model_writes_a_receipt_into_the_transcript() {
    picker_session(120, |term, stub| {
        term.send(b"/model\r");
        term.until("the model picker", |screen| {
            screen.iter().any(|row| row.contains("Esc cancels"))
        });
        for key in [b"a", b"a", b"a"] {
            term.send(key);
        }
        term.until("the aaa filter", |screen| {
            screen.iter().any(|row| row.contains("filter: aaa"))
        });
        term.send(b"\r");
        term.until("the reasoning step", |screen| {
            screen
                .iter()
                .any(|row| row.starts_with("Select reasoning for ollama/aaa-first"))
        });
        term.send(b"1");
        term.until("a highlighted effort", |screen| {
            screen.iter().any(|row| row.starts_with("> 1."))
        });
        term.send(b"s");
        // Catches: a silent model change.
        let screen = term.until("the receipt", |screen| {
            screen
                .iter()
                .any(|row| row.contains("Model: ollama/aaa-first (this session only)"))
        });
        assert!(
            !screen.iter().any(|row| row.starts_with("Select ")),
            "{screen:#?}"
        );
        assert!(stub.generations().is_empty());
    });
}

#[test]
fn the_banner_lists_the_keys_at_120_80_60_and_40_columns() {
    for cols in [120u16, 80, 60, 40] {
        picker_session(cols, |term, _stub| {
            let screen = term.screen();
            let rows = screen.iter().map(|row| row.trim()).collect::<Vec<_>>();
            // Catches: hints missing from the banner, or a key cut by a wrap.
            for item in [
                "Esc interrupts",
                "Enter queues",
                "Ctrl+S draft",
                "Option+P models",
                "/help",
            ] {
                assert!(
                    rows.iter()
                        .any(|row| row.split(" · ").any(|part| part == item)),
                    "{cols} cols: {item:?} missing from the banner:\n{}",
                    screen.join("\n")
                );
            }
        });
    }
}

/// catches: Esc in the permission edit form denying the call without the
/// `Interrupted by user` receipt the plain modal leaves.
#[test]
fn esc_in_the_permission_edit_form_denies_the_call_and_leaves_one_receipt() {
    let write_arguments = serde_json::json!({
        "path": "notes.md",
        "contents": "permission",
    })
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("write-1", "write", &write_arguments)),
            Reply::Sse(text("AFTER-EDIT-ESC")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "default",
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let idle_start = output.len();
    until_prompt_from(&reader, &mut output, idle_start);
    input.write_all(b"initial\r").expect("initial prompt");
    input.flush().expect("initial prompt flush");
    until(&reader, &mut output, b"[y] allow [n] deny [e] edit");
    drain_until_quiet(&reader, &mut output);
    let edit_start = output.len();
    input.write_all(b"e").expect("open the edit form");
    input.flush().expect("edit flush");
    until_normalized(&reader, &mut output, edit_start, "path: ");
    drain_until_quiet(&reader, &mut output);
    let esc_start = output.len();
    input.write_all(b"\x1b").expect("Esc in the form");
    input.flush().expect("Esc flush");
    until_normalized(&reader, &mut output, esc_start, "permission was refused");
    until_prompt_from(&reader, &mut output, esc_start);
    drain_until_quiet(&reader, &mut output);
    // The raw stream repeats the receipt on every repaint; the screen shows
    // what the person sees.
    let mut screen = vt100::Parser::new(24, 80, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    assert_eq!(
        visible.matches("Interrupted by user").count(),
        1,
        "Esc in the edit form must leave exactly one receipt: {visible:?}"
    );
    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit flush");
    let status = bounded_exit(child);
    // A refused permission ends the session with exit 3 by design.
    assert_eq!(status.exit_code(), 3, "unexpected exit: {status:?}");
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
}

/// The parent a job-control test needs. `ego` is spawned as a session leader
/// by the PTY, and a session leader's group is orphaned: the kernel discards
/// `SIGTSTP` for it, so nothing could ever stop. This process is ego's parent
/// in a group of its own, which makes ego a real shell job. It gives ego the
/// terminal and then reports what `waitpid` sees, so the oracle for "stopped"
/// is the kernel's own and no shell's tty bookkeeping sits between the test
/// and the terminal modes it reads.
#[cfg(unix)]
const JOB_PARENT: &str = r#"
import os, signal, sys
signal.signal(signal.SIGTTOU, signal.SIG_IGN)
pid = os.fork()
if pid == 0:
    os.setpgid(0, 0)
    os.tcsetpgrp(0, os.getpgrp())
    signal.signal(signal.SIGTTOU, signal.SIG_DFL)
    os.execv(sys.argv[1], sys.argv[1:])
try:
    os.setpgid(pid, pid)
except OSError:
    pass
os.write(1, ("JOB-PID:%d\n" % pid).encode())
while True:
    _, status = os.waitpid(pid, os.WUNTRACED)
    if os.WIFSTOPPED(status):
        os.write(1, b"JOB-STOPPED\n")
        continue
    code = os.waitstatus_to_exitcode(status)
    os.write(1, ("JOB-EXITED:%d\n" % code).encode())
    sys.exit(0 if code == 0 else 1)
"#;

/// An interactive ego running as a job under [`JOB_PARENT`].
#[cfg(unix)]
struct Job {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pair: portable_pty::PtyPair,
    input: PtyWriter,
    reader: mpsc::Receiver<Vec<u8>>,
    reader_join: thread::JoinHandle<()>,
    writer_join: thread::JoinHandle<()>,
    output: Vec<u8>,
    pid: u32,
    rows: u16,
    cols: u16,
}

#[cfg(unix)]
impl Job {
    /// Start ego under the job parent and answer the trust question.
    fn start(ego: &Ego, rows: u16, cols: u16) -> Self {
        let ego_command = ego.pty_command();
        let mut command = portable_pty::CommandBuilder::new("/usr/bin/python3");
        command.env_clear();
        for (key, value) in ego_command.iter_full_env_as_str() {
            command.env(key, value);
        }
        command.arg("-c");
        command.arg(JOB_PARENT);
        command.args(ego_command.get_argv());
        if let Some(cwd) = ego_command.get_cwd() {
            command.cwd(cwd);
        }
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty");
        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn the job parent");
        let (input, dsr_responder, writer_join) =
            writer_owner(pair.master.take_writer().expect("writer"));
        let (reader, reader_join) = stream_with_screen(
            pair.master.try_clone_reader().expect("reader"),
            dsr_responder,
            Some((rows, cols)),
        );
        let mut job = Self {
            child,
            pair,
            input,
            reader,
            reader_join,
            writer_join,
            output: Vec::new(),
            pid: 0,
            rows,
            cols,
        };
        job.pid = loop {
            let text = String::from_utf8_lossy(&job.output).into_owned();
            if let Some(tail) = text.split("JOB-PID:").nth(1)
                && let Some(digits) = tail.split(['\r', '\n']).next()
                && tail.len() > digits.len()
            {
                break digits.parse().expect("the job parent prints ego's pid");
            }
            let chunk = job
                .reader
                .recv_timeout(PROGRESS_DEADLINE)
                .expect("the job parent never reported ego's pid");
            job.output.extend(chunk);
        };
        until(&job.reader, &mut job.output, b"Trust project files");
        job.send(b"yes\r");
        until_post_trust_prompt(&job.reader, &mut job.output, 0);
        job
    }

    fn send(&mut self, bytes: &[u8]) {
        self.input.write_all(bytes).expect("write to ego");
        self.input.flush().expect("flush to ego");
    }

    /// `Ctrl+Z`, then wait for the kernel to report ego stopped.
    fn suspend(&mut self) -> usize {
        let mark = self.output.len();
        self.send(b"\x1a");
        until_from(&self.reader, &mut self.output, mark, b"JOB-STOPPED");
        mark
    }

    /// `SIGCONT`, as `fg` sends it; the terminal already belongs to ego's group.
    fn resume(&self) {
        let status = std::process::Command::new("kill")
            .args(["-CONT", &self.pid.to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "SIGCONT was not delivered");
    }

    /// The process state letters `ps` reports for ego: `T` is stopped.
    fn state(&self) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &self.pid.to_string()])
            .output()
            .expect("run ps");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Canonical mode is on: the line discipline is the cooked one a shell
    /// expects. Read from the terminal device by `stty`, not from ego.
    fn cooked(&self) -> bool {
        let tty = self.pair.master.tty_name().expect("slave PTY name");
        let option = if cfg!(target_os = "macos") {
            "-f"
        } else {
            "-F"
        };
        let out = std::process::Command::new("stty")
            .arg(option)
            .arg(tty)
            .arg("-a")
            .output()
            .expect("run stty");
        let flags = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            flags
                .split_whitespace()
                .any(|flag| flag.trim_end_matches(';') == "icanon")
                || flags.split_whitespace().any(|flag| flag == "-icanon"),
            "stty did not report the canonical flag: {flags}"
        );
        flags
            .split_whitespace()
            .any(|flag| flag.trim_end_matches(';') == "icanon")
    }

    fn rows(&self) -> Vec<String> {
        screen_rows(&self.output, self.rows, self.cols)
    }

    /// Quit and reap, clearing a draft left in the composer first if asked.
    fn quit(mut self, clear_draft: bool) {
        if clear_draft {
            let mark = self.output.len();
            self.send(b"\x03");
            until_prompt_from(&self.reader, &mut self.output, mark);
            drain_until_quiet(&self.reader, &mut self.output);
        }
        self.send(b"/quit\r");
        until(&self.reader, &mut self.output, b"JOB-EXITED:0");
        let status = bounded_exit(self.child);
        assert!(status.success(), "job parent failed: {status:?}");
        drop(self.input);
        drop(self.pair);
        self.reader_join.join().expect("reader thread panicked");
        self.writer_join.join().expect("writer thread panicked");
    }
}

#[cfg(unix)]
fn job_ego(stub: &Stub, mode: &str) -> Ego {
    let ego = Ego::against(stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--mode",
            mode,
            "--sandbox",
            "off",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# suspend fixture\n");
    ego
}

#[cfg(unix)]
#[test]
fn ctrl_z_stops_ego_with_a_cooked_terminal_and_sigcont_restores_raw_mode_and_the_composer() {
    // Catches: ego stopping with the terminal still raw, which leaves the
    // shell without echo or line editing; and ego resuming without raw mode or
    // without its composer, which leaves a dead prompt.
    let stub = Stub::start(Plan::default());
    let ego = job_ego(&stub, "default");
    let mut job = Job::start(&ego, 24, 80);
    let draft_start = job.output.len();
    job.send(b"keep this draft");
    until_from(
        &job.reader,
        &mut job.output,
        draft_start,
        b"keep this draft",
    );
    drain_until_quiet(&job.reader, &mut job.output);
    assert!(
        !job.cooked(),
        "the composer must read in raw mode, or the cooked check below proves nothing"
    );

    let stop_mark = job.suspend();
    assert!(
        job.state().contains('T'),
        "ego is not stopped: {:?}",
        job.state()
    );
    assert!(
        job.cooked(),
        "ego stopped with the terminal still in raw mode"
    );
    assert!(
        job.output[stop_mark..]
            .windows(8)
            .any(|window| window == b"\x1b[?2004l"),
        "bracketed paste was left on for the shell"
    );

    let resume_mark = job.output.len();
    job.resume();
    until_from(
        &job.reader,
        &mut job.output,
        resume_mark,
        b"keep this draft",
    );
    drain_until_quiet(&job.reader, &mut job.output);
    assert!(!job.cooked(), "raw mode was not restored after SIGCONT");
    assert!(
        job.output[resume_mark..]
            .windows(8)
            .any(|window| window == b"\x1b[?2004h"),
        "bracketed paste was not restored after SIGCONT"
    );
    let rows = job.rows();
    let last = rows
        .iter()
        .rev()
        .find(|row| !row.trim().is_empty())
        .expect("a screen with content");
    assert!(
        last.starts_with('>') && last.contains("keep this draft"),
        "the composer with its draft is not the last row after resume: {rows:?}"
    );
    // The restored composer takes input, not just paint.
    job.send(b"!");
    let typed_by = Instant::now() + PROGRESS_DEADLINE;
    while !job
        .rows()
        .iter()
        .any(|row| row.starts_with('>') && row.contains("keep this draft!"))
    {
        assert!(
            Instant::now() < typed_by,
            "the resumed composer ignored a key: {:?}",
            job.rows()
        );
        let chunk = job
            .reader
            .recv_timeout(PROGRESS_DEADLINE)
            .expect("ego stopped answering after resume");
        job.output.extend(chunk);
    }
    job.quit(true);
}

#[cfg(unix)]
#[test]
fn a_running_exec_is_not_charged_the_time_ego_spent_suspended() {
    use std::os::unix::fs::PermissionsExt as _;

    // Catches the defect pi has: the suspended wall time counts against the
    // tool timeout, so a command that finished is reported as timed out.
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "suspended-exec",
                "exec",
                r#"{"argv":["slow-work"],"cwd":null,"timeout_ms":3000}"#,
            )),
            Reply::Sse(text("SUSPEND-EXEC-DONE")),
        ],
        ..Plan::default()
    });
    let ego = job_ego(&stub, "yolo");
    let executable = ego.home_file(
        ".local/bin/slow-work",
        "#!/bin/sh\nprintf started > \"$EGO_HOME/tool-started\"\n/bin/sleep 2\nprintf 'EXEC-FINISHED-AFTER-STOP\\n'\n",
    );
    let mut permissions = std::fs::metadata(&executable)
        .expect("slow tool metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("make slow tool executable");
    let path = format!("{}:/usr/bin:/bin", executable.parent().unwrap().display());
    let started = ego.home_path().join("tool-started");
    let ego = ego.env("PATH", &path);
    let mut job = Job::start(&ego, 24, 80);
    job.send(b"run the slow tool\r");
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !started.exists() {
        assert!(Instant::now() < deadline, "the tool never started");
        drain(&job.reader, &mut job.output);
        thread::sleep(Duration::from_millis(20));
    }

    job.suspend();
    // Longer than the 3 s timeout, so a clock that keeps running while ego is
    // stopped has expired when ego continues; the tool itself needs only 2 s.
    thread::sleep(Duration::from_millis(4500));
    job.resume();
    until(&job.reader, &mut job.output, b"SUSPEND-EXEC-DONE");

    let generations = stub.generations();
    let result = generations[1].to_string();
    assert!(
        result.contains("EXEC-FINISHED-AFTER-STOP"),
        "the finished command's output did not reach the model: {result}"
    );
    assert!(
        !result.contains("timed out"),
        "suspended time was charged to the exec timeout: {result}"
    );
    drain_until_quiet(&job.reader, &mut job.output);
    job.quit(false);
}

#[cfg(unix)]
#[test]
fn ctrl_z_at_a_permission_modal_leaves_it_unresolved_after_resume() {
    // Catches a suspend that answers the prompt: the key falling through to
    // the modal's decision handling, or the repaint dismissing the modal.
    let write_arguments = r#"{"path":"notes.md","contents":"permission"}"#;
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("suspended-write", "write", write_arguments)),
            Reply::Sse(text("SUSPEND-MODAL-DONE")),
        ],
        ..Plan::default()
    });
    let ego = job_ego(&stub, "default");
    let notes = ego.workspace_path().join("notes.md");
    let mut job = Job::start(&ego, 40, 100);
    job.send(b"write the notes\r");
    until(&job.reader, &mut job.output, b"[y] allow [n] deny [e] edit");
    drain_until_quiet(&job.reader, &mut job.output);
    let requests_before = stub.requests().len();

    job.suspend();
    assert!(job.cooked(), "ego stopped at the modal with a raw terminal");
    let resume_mark = job.output.len();
    job.resume();
    until_from(
        &job.reader,
        &mut job.output,
        resume_mark,
        b"[y] allow [n] deny [e] edit",
    );
    thread::sleep(Duration::from_millis(500));
    drain_until_quiet(&job.reader, &mut job.output);
    assert_eq!(
        stub.requests().len(),
        requests_before,
        "the suspend answered the permission: the run went on to the provider"
    );
    assert!(!notes.exists(), "the write ran without an answer");

    job.send(b"y");
    until(&job.reader, &mut job.output, b"SUSPEND-MODAL-DONE");
    assert!(notes.exists(), "the answer given after resume was lost");
    job.quit(false);
}

#[cfg(windows)]
#[test]
fn ctrl_z_reports_that_windows_cannot_suspend_it() {
    // Catches a silently ignored key: the user presses Ctrl+Z and nothing
    // says why ego did not stop.
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# suspend fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"\x1a").expect("Ctrl+Z");
    input.flush().expect("Ctrl+Z flush");
    until_normalized(
        &reader,
        &mut output,
        0,
        "Ctrl+Z is not supported on Windows",
    );
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
}
