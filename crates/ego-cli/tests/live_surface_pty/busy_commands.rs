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
    let (reply, gate) = gated_paced_line("TASKS-539-RUNNING\n");
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
    until(&reader, &mut output, b"TASKS-539-RUNNING");
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
            Reply::Sse(support::costing("PRESSURE-580", 244796, 4)),
            Reply::GatedPaced {
                head: String::new(),
                tail: text(checkpoint),
                gate: Arc::clone(&gate),
            },
            Reply::Sse(text("RECOVERED-580")),
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
    until_normalized(&reader, &mut output, start, "PRESSURE-580");
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while stub.generations().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "automatic checkpoint did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // Catches: an actual in-flight compaction still paints the generic model row.
    until_normalized_without_new_generation(&reader, &mut output, start, "Compacting ", &stub, 2);
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
    until_normalized(&reader, &mut output, start, "RECOVERED-580");
    until_mcp_composer_is_idle(&reader, &mut output);
    let mut terminal = vt100::Parser::new(24, 100, 0);
    terminal.process(&output);
    assert!(
        !terminal.screen().contents().contains("Compacting"),
        "compaction activity leaked into the idle successor: {}",
        terminal.screen().contents()
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(
        !busy_output.contains("action is no longer available"),
        "{busy_output}"
    );
    assert_eq!(stub.generations().len(), 3, "duplicate compaction request");
}

// Catches: safe native catalogues are refused, silently deferred, or sent as
// another model request instead of being serviced by the busy host owner.
#[test]
fn busy_native_catalogues_render_before_the_provider_is_released() {
    let (reply, gate) = gated_paced_line("NATIVE-CATALOGUES-HELD\n");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file(".ego/skills/busy-inventory/SKILL.md",
        "---\nname: busy-inventory\ndescription: Native catalogue fixture\n---\nInspect this local fixture.\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, None);
    // A failed terminal assertion must release the owned provider before the
    // PTY/session destructors wait for shutdown, retaining the actual failure.
    struct ReleaseOnDrop(Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"hold catalogues\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"NATIVE-CATALOGUES-HELD");
    for (command, expected) in [
        ("/hooks\r", "no hooks are configured and enabled"),
        ("/hooks details\r", "\"hooks\": []"),
        ("/skills\r", "/busy-inventory"),
        ("/skills details\r", "description=Native catalogue fixture"),
        ("/skills diagnose\r", "skills: no admission diagnostics"),
        ("/tools details\r", "Tool"),
        ("/mcp\r", "mcp: no configured servers"),
        (
            "/config details\r",
            "Configuration (startup snapshot; saved edits apply to future sessions)",
        ),
        (
            "/config set mode 'plan'\r",
            "Saved to your user configuration for future sessions",
        ),
        ("/fast off\r", "ollama/qwen3: standard (this session only)"),
        ("/mode plan\r", "staged for the next generation"),
        (
            "/tasks cancel not-a-session-id\r",
            "invalid background exec ID",
        ),
        ("/doctor\r", "doctor:"),
    ] {
        let start = output.len();
        input.write_all(command.as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized_without_new_generation(&reader, &mut output, start, expected, &stub, 1);
        if command == "/tools details\r" {
            // A summary/count is not the promised detailed catalogue.
            for tool in ["exec", "read"] {
                until_normalized_without_new_generation(
                    &reader,
                    &mut output,
                    start,
                    tool,
                    &stub,
                    1,
                );
            }
        }
    }
    let saved: toml::Value =
        toml::from_str(&std::fs::read_to_string(ego.home_path().join("config.toml")).unwrap())
            .unwrap();
    assert_eq!(
        saved["mode"].as_str(),
        Some("plan"),
        "native save must actually persist the future default"
    );
    let start = output.len();
    gate.release();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(stub.generations().len(), 1);
}
