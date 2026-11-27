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

// Catches: an exact detached-exec cancellation or future-session config save
// waits for the held provider, loses its effect, or executes twice at settlement.
#[test]
fn task_cancel_and_config_set_take_effect_before_the_held_turn_finishes() {
    let arguments = serde_json::json!({
        "argv": ["/bin/sh", "-c", "printf ready > ready; printf TASK-545-OUTPUT; while [ ! -f release ]; do sleep 0.05; done; printf survived > survived"],
        "timeout_ms": 60000
    });
    let (reply, gate) = gated_paced_line("MUTATION-545-HELD\n");
    let mut session = modal_session_with(vec![
        Reply::Sse(tool_call("exec-545", "exec", &arguments.to_string())),
        Reply::Sse(text(TOOL_TURN_DONE)),
        reply,
        Reply::Sse(text("MUTATION-545-SETTLED")),
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

    session.press(b"hold the next turn\r");
    session.wait("MUTATION-545-HELD");
    let mutation_start = session.output.len();
    session.press(format!("/tasks cancel {id}\r").as_bytes());
    session.wait("cancellation requested");
    session.press(b"/config set model \"ollama/deferred-545\"\r");
    session.wait("Saved model =");
    let saved: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(saved["model"].as_str(), Some("ollama/deferred-545"));
    let held = normalized(&session.output[mutation_start..]);
    assert_eq!(held.matches("Saved model =").count(), 1, "{held}");
    assert_eq!(held.matches("cancellation requested").count(), 1, "{held}");
    assert_eq!(
        session.stub.generations().len(),
        3,
        "provider is still held"
    );

    gate.release();
    until_prompt_after_reply(
        &session.reader,
        &mut session.output,
        b"MUTATION-545-SETTLED",
    );
    let receipts = normalized(&session.output[mutation_start..]);
    assert_eq!(receipts.matches("Saved model =").count(), 1, "{receipts}");
    assert!(!receipts.contains("Unchanged: model"), "{receipts}");
    // The immediate cancellation receipt precedes the config mutation. The later
    // settlement announcement is not a second execution of the cancellation.
    let cancellation = receipts.split("Saved model =").next().unwrap();
    assert_eq!(
        cancellation.matches("cancellation requested").count()
            + cancellation.matches("cancelled").count(),
        1,
        "{receipts}"
    );
    assert!(!session.ego.workspace_path().join("survived").exists());
    let (generations, config) = session.finish();
    assert_eq!(
        generations.len(),
        4,
        "one settlement continuation is required"
    );
    let history = generations.last().expect("settlement generation")["messages"].to_string();
    assert!(
        history.contains("cancelled") && history.contains("TASK-545-OUTPUT"),
        "cancellation lost its outcome or output: {history}"
    );
    let config: toml::Value = toml::from_str(&config).expect("saved config");
    assert_eq!(config["model"].as_str(), Some("ollama/deferred-545"));
}

// Catches: a future-session config save is silently deferred or reports success
// without persisting while the current generation is held.
#[test]
fn busy_config_write_persists_and_reports_before_the_turn_finishes() {
    let (reply, gate) = gated_paced_line("CONFIG-RECEIPT-545-HELD\n");
    let mut session = modal_session_with(vec![reply]);
    session.press(b"hold this turn\r");
    session.wait("CONFIG-RECEIPT-545-HELD");
    let start = session.output.len();
    session.press(b"/config set model \"ollama/pending-545\"\r");
    // An immediate inspection proves that the preceding write was consumed.
    session.press(b"/tasks\r");
    session.wait("background tasks: none");
    let held = normalized(&session.output[start..]);
    let saved: toml::Value = toml::from_str(
        &std::fs::read_to_string(session.ego.home_path().join("config.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["model"].as_str(), Some("ollama/pending-545"));
    assert_eq!(held.matches("Saved model =").count(), 1, "{held}");
    assert!(
        held.contains("Saved to your user configuration for future sessions"),
        "the receipt must distinguish the captured active settings: {held}"
    );
    assert!(!held.contains("pending until"), "{held}");
    assert_eq!(
        session.stub.generations().len(),
        1,
        "provider is still held"
    );
    gate.release();
    session.finish();
}
