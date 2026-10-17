use super::*;

// Catches: a Plan-mode tool denial returns to idle without a complete cause and next action.
#[test]
fn plan_denial_explains_the_stopped_turn_without_running_the_effect() {
    for cols in [80, 120] {
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(tool_call(
                "denied-call",
                "exec",
                r#"{"argv":["/bin/sh","-c","printf forbidden > denied-marker"],"timeout_ms":5000}"#,
            ))],
            ..Plan::default()
        });
        let ego = Ego::against(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--mode",
            "plan",
            "--sandbox",
            "off",
        ]);
        let mut session = modal_session_on(ego, stub);
        session.resize(MODAL_ROWS, cols);
        session.press(b"run the requested tool\r");
        session.wait("✗");
        session.press(b"/version\r");
        session.wait("ACP protocol");
        let screen = session.screen();
        let shown = screen.join("\n");
        let effect_ran = session.ego.workspace_path().join("denied-marker").exists();
        let (status, generations, _) = session.quit();
        assert_eq!(status.exit_code(), 3, "permission termination changed");
        assert!(!effect_ran, "Plan mode ran the prohibited shell effect");
        assert_eq!(generations.len(), 1, "denial must not continue generation");
        assert_eq!(
            shown.matches("stopped: permission refused").count(),
            1,
            "{cols} columns: no unique denial receipt before idle:\n{shown}"
        );
        let prose = screen
            .iter()
            .map(|row| row.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            prose.contains("exec may not") && prose.contains("plan mode forbids"),
            "the receipt must identify the denied tool and mode: {shown}"
        );
        assert!(
            prose.contains("/mode") && prose.contains("resubmit"),
            "the receipt has no actionable next step: {shown}"
        );
        assert!(
            !shown.contains("Esc to interrupt"),
            "stale Working row: {shown}"
        );
    }
}

// Catches: the denial receipt is emitted for an allowed Plan-mode read turn.
#[test]
fn allowed_plan_read_does_not_report_a_permission_denial() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("read-call", "read", r#"{"path":"AGENTS.md"}"#)),
            Reply::Sse(text("PLAN-READ-ANSWER")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--mode",
        "plan",
        "--sandbox",
        "off",
    ]);
    let mut session = modal_session_on(ego, stub);
    session.press(b"read the fixture\r");
    session.wait("PLAN-READ-ANSWER");
    session.press(b"/version\r");
    session.wait("ACP protocol");
    let shown = session.screen().join("\n");
    let (generations, _) = session.finish();
    assert_eq!(
        generations.len(),
        2,
        "the allowed read must continue the turn"
    );
    let result = generations[1]["messages"]
        .as_array()
        .expect("provider messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the read result");
    assert!(result.contains("permission modal fixture"), "{result}");
    assert!(
        !shown.contains("stopped: permission refused"),
        "an allowed Plan read was reported as denied: {shown}"
    );
}
