use super::*;

fn until_goal_receipt(
    reader: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    prefix: &str,
) {
    until_normalized(reader, output, start, prefix);
    // A wrapped receipt can have Working repaints between its physical rows.
    // Observe retained terminal prose through its final sentence, not quiet time.
    until_screen_prose(
        reader,
        output,
        32,
        180,
        "Inspect the goal with '/goal show'.",
    );
}

// Catches: a quiet interval after the prefix truncates a delayed receipt suffix.
#[test]
fn goal_visibility_waits_for_delayed_receipt_suffix() {
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.args([
        "-c",
        "printf '%s' 'Goal stopped: fixture. Inspect the goal '; sleep 1; printf \"%s\\n\" \"with '/goal show'.\"",
    ]);
    let (session, input, reader) = PtySession::start_command(command, 32, 180, None);
    let mut output = Vec::new();
    until_goal_receipt(&reader, &mut output, 0, "Goal stopped: fixture");
    let captured = normalized(&output);
    // Reap the fixture before asserting, including when the old wait returns early.
    assert!(session.quit(input).success());
    assert!(
        captured.contains("Inspect the goal with '/goal show'."),
        "receipt wait returned before the suffix: {captured}"
    );
}

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
            Reply::Sse(text("FIRST-WORK-ZYY")),
            Reply::Sse(text("LAST-WORK-ZYY")),
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
    let (session, mut input, reader) = PtySession::start(&ego, 32, 180, Some((32, 180)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"do the work\r").unwrap();
    input.flush().unwrap();
    until_goal_receipt(
        &reader,
        &mut output,
        start,
        "goal continuation budget exhausted",
    );
    until_normalized(&reader, &mut output, start, "goal 1/1");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(stub.generations().len(), 4);
    // Inspect durable terminal rows, not raw repaint bytes: the working line
    // can be erased and redrawn between the rows of a wrapped receipt.
    let mut parser = vt100::Parser::new(32, 180, 1_000);
    parser.process(&output);
    let live = parser.screen().contents();
    parser.screen_mut().set_scrollback(usize::MAX);
    let depth = parser.screen().scrollback();
    let mut history = String::new();
    for offset in (1..=depth).rev() {
        parser.screen_mut().set_scrollback(offset);
        history.push_str(&parser.screen().rows(0, 180).next().unwrap());
        history.push('\n');
    }
    history.push_str(&live);
    history.split_whitespace().collect::<Vec<_>>().join(" ")
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
        "Inspect the goal with '/goal show'.",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
}

// Catches: a checker that needs operator action leaves the goal active or spends automatic turns.
#[test]
fn goal_visibility_needs_operator_blocks_without_spending_allowance() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("WAITING-WORK-ZYY"))],
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
    until_goal_receipt(
        &reader,
        &mut output,
        start,
        "Goal stopped: Verify operator account",
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let transcript = normalized(&output[start..])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
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
    assert!(!transcript.contains("turns 0/20"), "{transcript}");
    assert!(!transcript.contains("none recorded"), "{transcript}");
    for expected in [
        "needs operator",
        "Operator must approve account login",
        "Inspect the goal with '/goal show'.",
    ] {
        assert!(
            transcript.contains(expected),
            "missing {expected}: {transcript}"
        );
    }
}
