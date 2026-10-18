use super::*;

// Catches: /tasks vanishes while the provider holds the stream, with no answer or receipt.
#[test]
fn busy_tasks_inspection_is_not_silently_deferred() {
    busy_tasks_contract(false);
}

// Catches: Escape drops or duplicates an already-admitted pending command.
#[test]
fn busy_tasks_survives_escape_once() {
    busy_tasks_contract(true);
}

fn busy_tasks_contract(interrupt: bool) {
    let (reply, gate) = gated_paced_line("REGRESSION-MARKER-RUNNING\n");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# Busy command fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust");
    input.flush().expect("flush");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"hold turn\r").expect("turn");
    input.flush().expect("flush");
    until(&reader, &mut output, b"REGRESSION-MARKER-RUNNING");
    let help_start = output.len();
    input.write_all(b"/help\r").expect("help during held turn");
    input.flush().expect("help flush");
    until_normalized_without_new_generation(&reader, &mut output, help_start, HELP_END, &stub, 1);
    let start = output.len();
    input.write_all(b"/tasks\r").expect("tasks");
    input.flush().expect("flush");
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        start,
        "background tasks: none",
        &stub,
        1,
    );
    drain_until_quiet(&reader, &mut output);
    record_slash_capture(&output, 24, 80);
    let during_turn = normalized(&output[start..]);
    if interrupt {
        input.write_all(b"\x1b").expect("interrupt");
        input.flush().expect("flush");
        drain_until_quiet(&reader, &mut output);
    }
    gate.release();
    until_normalized(&reader, &mut output, start, "background tasks: none");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let settled = normalized(&output[start..]);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("flush");
    session.quit(input);
    assert!(
        during_turn.contains("background tasks: none") || during_turn.contains("/tasks pending"),
        "silent deferral: {during_turn}"
    );
    assert_eq!(
        settled.matches("background tasks: none").count(),
        1,
        "duplicate execution: {settled}"
    );
    assert_eq!(stub.generations().len(), 1);
}

// Catches: /compact during automatic compaction reports a stale action instead
// of identifying the already-running checkpoint, or starts a duplicate request.
#[test]
fn busy_compact_names_automatic_compaction_in_a_real_pty() {
    let checkpoint = "# Objective\nContinue the task.\n# Binding constraints and preferences\nNone.\n# Completed work and evidence\nFirst answer.\n# Work in progress\nFinish task.\n# Decisions and rationale\nNone.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nFinish task.\n# Critical references\nNone.";
    let gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(support::costing("REGRESSION-MARKER", 244796, 4)),
            Reply::GatedPaced {
                head: String::new(),
                tail: text(checkpoint),
                gate: Arc::clone(&gate),
            },
            Reply::Sse(text("REGRESSION-MARKER")),
        ],
        context_length: Some(272000),
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# Busy compaction fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"finish this task\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "REGRESSION-MARKER");
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while stub.generations().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "automatic checkpoint did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let start = output.len();
    input.write_all(b"/compact\r").unwrap();
    input.flush().unwrap();
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        start,
        "ego is busy; compaction already running",
        &stub,
        2,
    );
    drain_until_quiet(&reader, &mut output);
    let busy_output = normalized(&output[start..]);
    record_slash_capture(&output, 24, 100);
    gate.release();
    until_normalized(&reader, &mut output, start, "REGRESSION-MARKER");
    until_mcp_composer_is_idle(&reader, &mut output);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(
        !busy_output.contains("action is no longer available"),
        "{busy_output}"
    );
    assert_eq!(stub.generations().len(), 3, "duplicate compaction request");
}
