use super::*;

// Catches: /goal accepts and persists an objective but never sends its first work turn.
#[test]
fn typed_goal_starts_work_with_or_without_set_keyword() {
    for command in [
        "/goal build the fixture artifact",
        "/goal set build the fixture artifact",
    ] {
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(text("GOAL-WORK-DONE"))],
            ..Plan::default()
        });
        let checker = serde_json::json!([
            "/bin/sh",
            "-c",
            "cat >/dev/null; printf '%s\\n' '{\"verdict\":\"achieved\",\"reason\":\"fixture artifact built\"}'"
        ])
        .to_string();
        let ego = Ego::against(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--trust-workspace",
            "--allow",
            "exec(/bin/sh *)",
            "--goal-check-command",
            &checker,
            "--session-id",
            support::FIXTURE_SESSION_ID,
        ]);
        let journal = ego
            .home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl");
        let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
        let mut output = Vec::new();
        until_prompt_from(&reader, &mut output, 0);
        let start = output.len();
        input.write_all(format!("{command}\r").as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "GOAL-WORK-DONE");
        until_prompt_from(&reader, &mut output, start);
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        let events = std::fs::read_to_string(journal).unwrap();
        assert!(
            events.contains("\"kind\":\"GoalSet\""),
            "{command}: {events}"
        );
        assert!(
            events.contains("\"kind\":\"UserPrompt\""),
            "{command}: {events}"
        );
        let requests = stub.generations();
        assert_eq!(requests.len(), 1, "{command}: {requests:?}");
        assert_eq!(
            count_exact_strings(&requests[0]["messages"], "build the fixture artifact"),
            1,
            "{command}: {requests:?}"
        );
    }
}
