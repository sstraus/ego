use super::*;

// Catches: inspections sit in the ordinary host queue until the provider finishes.
#[test]
fn read_only_slash_inspections_answer_before_a_held_turn_ends() {
    let (reply, gate) = gated_paced_line("INSPECTION-TURN-HELD\n");
    let stub = Stub::start(Plan {
        completions: vec![reply],
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
    ego.file("AGENTS.md", "# inspection fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"hold this turn\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "INSPECTION-TURN-HELD");
    drain_until_quiet(&reader, &mut output);

    // The gate is released only after all three answers: completing the turn
    // cannot accidentally satisfy the inspection assertions.
    for (command, answer) in [
        ("/tasks\r", "background tasks: none"),
        ("/config\r", "Configuration (startup snapshot;"),
        ("/usage\r", "usage as of"),
    ] {
        let start = output.len();
        input.write_all(command.as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized_without_new_generation(&reader, &mut output, start, answer, &stub, 1);
        drain_until_quiet(&reader, &mut output);
    }
    gate.release();
    let start = output.len();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        stub.generations().len(),
        1,
        "inspections never start a generation"
    );
}

// Catches: the composer submits an unknown slash or its absolute-path escape as a command.
#[test]
fn unknown_slash_is_local_and_leading_space_sends_a_literal_path() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("LITERAL-PATH-RECEIVED"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# unknown slash fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"/histor-no-such-command\r").unwrap();
    input.flush().unwrap();
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        start,
        "Unknown slash command",
        &stub,
        0,
    );
    until_normalized(&reader, &mut output, start, "/help");
    drain_until_quiet(&reader, &mut output);
    let start = output.len();
    input.write_all(b" /tmp/x\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "LITERAL-PATH-RECEIVED");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let requests = stub.generations();
    assert_eq!(requests.len(), 1);
    assert_eq!(count_exact_strings(&requests[0]["messages"], " /tmp/x"), 1);
}

// TEST-CHANGE: replace private inbox-predicate assertions with real task and
// config effects, observed while the provider gate holds the foreground turn.
// Catches: a task cancellation or config write executes during a held turn,
// or a queued mutation is lost or executed more than once after completion.
#[test]
fn task_cancel_and_config_set_wait_for_the_held_turn_to_finish() {
    let arguments = serde_json::json!({
        "argv": ["/bin/sh", "-c", "printf ready > ready; printf REGRESSION-MARKER-OUTPUT; while [ ! -f release ]; do sleep 0.05; done; printf survived > survived"],
        "timeout_ms": 60000
    });
    let (reply, gate) = gated_paced_line("regression_case-HELD\n");
    let mut session = modal_session_with(vec![
        Reply::Sse(tool_call("regression_case", "exec", &arguments.to_string())),
        Reply::Sse(text(TOOL_TURN_DONE)),
        reply,
    ]);
    session.press(b"start a long exec\r");
    session.wait("[d] details");
    session.press(b"y");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !session.ego.workspace_path().join("ready").exists() {
        assert!(Instant::now() < deadline, "foreground child did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    session.press(b"\x02");
    session.wait(TOOL_TURN_DONE);
    let transcript = normalized(&session.output);
    let prefix = "background exec id: ";
    let offset = transcript.rfind(prefix).expect("background handle") + prefix.len();
    let id = transcript[offset..offset + 36].to_owned();
    assert!(id.parse::<ego_core::event::id::DetachedExecId>().is_ok());
    let config_path = session.ego.home_path().join("config.toml");
    let original = std::fs::read(&config_path).unwrap_or_default();

    session.press(b"hold the next turn\r");
    session.wait("regression_case-HELD");
    session.press(format!("/tasks cancel {id}\r").as_bytes());
    let mutation_start = session.mark;
    session.press(b"/config set model \"ollama/deferred-model\"\r");
    // This inspection is a processing barrier: the terminal has consumed both
    // preceding mutations, but the provider still cannot complete the turn.
    session.press(format!("/tasks {id}\r").as_bytes());
    session.wait("running");
    session.wait("REGRESSION-MARKER-OUTPUT");
    assert_eq!(std::fs::read(&config_path).unwrap_or_default(), original);
    let held = normalized(&session.output[mutation_start..]);
    assert!(!held.contains("Saved model ="), "{held}");
    assert!(!held.contains("cancellation requested"), "{held}");
    assert!(!held.contains("cancelled"), "{held}");

    gate.release();
    session.mark = mutation_start;
    session.wait("Saved model =");
    drain_until_quiet(&session.reader, &mut session.output);
    let receipts = normalized(&session.output[mutation_start..]);
    assert_eq!(receipts.matches("Saved model =").count(), 1, "{receipts}");
    assert!(!receipts.contains("Unchanged: model"), "{receipts}");
    assert_eq!(
        receipts.matches("cancellation requested").count() + receipts.matches("cancelled").count(),
        1,
        "{receipts}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        session.press(format!("/tasks {id}\r").as_bytes());
        drain_until_quiet(&session.reader, &mut session.output);
        if normalized(&session.output[session.mark..]).contains("cancelled") {
            break;
        }
        assert!(Instant::now() < deadline, "cancel did not settle");
    }
    assert!(!session.ego.workspace_path().join("survived").exists());
    let (generations, config) = session.finish();
    assert_eq!(generations.len(), 3, "mutations must not start generations");
    let config: toml::Value = toml::from_str(&config).expect("saved config");
    assert_eq!(config["model"].as_str(), Some("ollama/deferred-model"));
}

// Catches: a saved-default mutation disappears from the busy composer without
// telling the user that it is accepted but still waiting for the active turn.
#[test]
fn busy_config_write_reports_pending_before_the_turn_finishes() {
    let (reply, gate) = gated_paced_line("REGRESSION-MARKER-HELD\n");
    let mut session = modal_session_with(vec![reply]);
    session.press(b"hold this turn\r");
    session.wait("REGRESSION-MARKER-HELD");
    let start = session.output.len();
    session.press(b"/config set model \"ollama/pending-model\"\r");
    // An immediate inspection proves that the preceding write was consumed.
    session.press(b"/tasks\r");
    session.wait("background tasks: none");
    let held = normalized(&session.output[start..]);
    gate.release();
    session.wait("Saved model =");
    session.finish();
    assert!(
        held.contains("config write pending until the active turn finishes"),
        "the user needs an acceptance receipt before completion: {held}"
    );
    assert_eq!(
        held.matches("pending until").count(),
        1,
        "the generic host handoff must not duplicate the config receipt: {held}"
    );
}
