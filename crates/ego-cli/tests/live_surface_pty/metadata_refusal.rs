use super::*;

// Catches: invalid metadata arguments disappear into a transient prompt notice
// instead of leaving one visible refusal with the valid subcommands.
#[test]
fn unknown_metadata_subcommands_print_usage_without_changing_state() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# metadata refusal fixture\n");
    let journal = ego
        .home_path()
        .join("sessions")
        .join(support::FIXTURE_SESSION_ID)
        .join("events.jsonl");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);

    for (command, receipt) in [
        ("/goal set REGRESSION-MARKER-OBJECTIVE", "REGRESSION-MARKER-OBJECTIVE"),
        ("/name set REGRESSION-MARKER-TITLE", "REGRESSION-MARKER-TITLE"),
    ] {
        let start = output.len();
        input.write_all(format!("{command}\r").as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, receipt);
        until_prompt_from(&reader, &mut output, start);
        drain_until_quiet(&reader, &mut output);
    }
    // Echoed input and an intermediate repaint can precede durable metadata
    // application. Establish the real baseline before testing refusal effects.
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let before = loop {
        let journal = std::fs::read_to_string(&journal).expect("metadata journal");
        if journal.contains("\"kind\":\"GoalSet\"")
            && journal.contains("\"kind\":\"SessionTitleChanged\"")
        {
            break journal;
        }
        assert!(
            Instant::now() < deadline,
            "metadata was not durably applied: {journal}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut expected_usages = Vec::new();
    for (command, usage) in [
        (
            "/goal clean",
            "usage: /goal [show|set TEXT|achieve|block REASON]",
        ),
        ("/name clean", "usage: /name [show|set TEXT|clear]"),
    ] {
        let start = output.len();
        input.write_all(format!("{command}\r").as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, usage);
        drain_until_quiet(&reader, &mut output);
        // A local status command replaces prompt notices. A refusal must stay
        // readable in the transcript instead of vanishing on the next action.
        input.write_all(b"/name show\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "REGRESSION-MARKER-TITLE");
        drain_until_quiet(&reader, &mut output);
        expected_usages.push(usage);
    }
    let after = std::fs::read_to_string(&journal).expect("metadata journal after refusal");
    let rows = screen_rows(&output, 40, 120);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        before, after,
        "refused metadata commands changed durable state"
    );
    assert!(
        stub.generations().is_empty(),
        "metadata typo reached the model"
    );
    for usage in expected_usages {
        assert_eq!(
            rows.iter().filter(|row| row.trim() == usage).count(),
            1,
            "metadata refusal must be printed once on its own row: {rows:?}"
        );
    }
}
