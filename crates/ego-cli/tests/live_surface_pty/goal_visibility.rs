use super::*;

fn goal_visibility_transcript() -> String {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(support::tool_call(
                "set-goal",
                "goal",
                r#"{"objective":"Build artifact 527"}"#,
            )),
            Reply::Sse(support::tool_call(
                "update-goal",
                "goal",
                r#"{"objective":"Verify artifact 527"}"#,
            )),
            Reply::Sse(text("REGRESSION-MARKER")),
            Reply::Sse(text("REGRESSION-MARKER")),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    let checker = serde_json::json!([
        "/bin/sh",
        "-c",
        "cat >/dev/null; printf '%s\\n' '{\"verdict\":\"continue\",\"reason\":\"artifact missing 527\"}'"
    ])
    .to_string();
    let ego = places
        .ego(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--yolo",
            "--trust-workspace",
            "--allow",
            "exec(/bin/sh *)",
            "--goal-turns",
            "1",
            "--goal-check-command",
            &checker,
        ])
        .env("TERM", "xterm-256color");
    let (session, mut input, reader) = PtySession::start(&ego, 32, 180, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"do the work\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "goal continuation budget exhausted",
    );
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(stub.generations().len(), 4);
    normalized(&output[start..])
}

// Catches: model-authored goals start or change silently.
#[test]
fn goal_visibility_receipts_name_objective_and_owner() {
    let output = goal_visibility_transcript();
    for objective in ["Build artifact 527", "Verify artifact 527"] {
        let receipt = format!("Goal (automated): {objective}");
        assert_eq!(output.matches(&receipt).count(), 1, "{output}");
    }
}

// Catches: automatic work runs as an invisible system message.
#[test]
fn goal_visibility_continuation_names_turn_and_unmet_condition() {
    let output = goal_visibility_transcript();
    assert!(
        output.contains("Goal turn 1 of 1: artifact missing 527"),
        "{output}"
    );
    assert!(output.contains("goal 1/1"), "{output}");
}

// Catches: exhaustion tells the operator only a number.
#[test]
fn goal_visibility_exhaustion_names_objective_condition_and_actions() {
    let output = goal_visibility_transcript();
    for expected in [
        "Goal stopped: Verify artifact 527",
        "turns 1/1",
        "artifact missing 527",
        "/goal show",
        "/goal achieve",
        "/goal block",
        "--goal-turns",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
}

// Catches: a checker that needs operator action leaves the goal active or spends automatic turns.
#[test]
fn goal_visibility_needs_operator_blocks_without_spending_allowance() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("REGRESSION-MARKER"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    let checker = serde_json::json!([
        "/bin/sh", "-c",
        "cat >/dev/null; printf '%s\\n' '{\"verdict\":\"needs_operator\",\"reason\":\"Operator must approve account login\"}'"
    ]).to_string();
    let ego = places
        .ego(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--trust-workspace",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--goal",
            "Verify operator account",
            "--goal-turns",
            "20",
            "--allow",
            "exec(/bin/sh *)",
            "--goal-check-command",
            &checker,
        ])
        .env("TERM", "xterm-256color");
    let (session, mut input, reader) = PtySession::start(&ego, 32, 180, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"verify the account\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Goal stopped:");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let transcript = normalized(&output[start..]);
    let log = std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .unwrap();
    let events: Vec<serde_json::Value> = log
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["payload"]["event"]["kind"]
                .clone()
        })
        .collect();
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "GoalTransitioned"
                && event["transition"]["kind"] == "blocked"
                && event["transition"]["reason"] == "Operator must approve account login"),
        "{log}"
    );
    assert!(
        !events
            .iter()
            .any(|event| event["kind"] == "GoalContinuation"),
        "{log}"
    );
    assert_eq!(stub.generations().len(), 1);
    for expected in [
        "needs operator",
        "Operator must approve account login",
        "turns 0/20",
        "/goal set TEXT",
        "require an active goal",
        "/goal show",
        "/goal achieve",
    ] {
        assert!(
            transcript.contains(expected),
            "missing {expected}: {transcript}"
        );
    }
}
