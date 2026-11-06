//! Recovery from provider names outside the request's declared tool aliases.
use crate::support;

use ego_core::event::domain::{DomainEvent, EventKind};
use support::{Places, Plan, Reply, Stub, text, tool_call};

// Catches: one undeclared tool aborts a recoverable goal-mode run.
#[cfg(unix)]
#[test]
fn undeclared_tool_feedback_allows_correction_in_goal_mode() {
    correction("ls", "{}", "undeclared tool name 'ls'", true);
    correction("", "{}", "no function name", true);
}

// Catches: malformed arguments abort rather than receiving tool error feedback.
#[cfg(unix)]
#[test]
fn malformed_arguments_allow_correction_in_goal_mode() {
    correction("read", "{", "EOF", false);
}

#[cfg(unix)]
fn correction(name: &str, args: &str, notice: &str, lists_tools: bool) {
    let places = Places::new();
    let path = places.file("note.txt", "verified note");
    let evaluator = places.file("check.sh", "cat >/dev/null\nprintf '%s\\n' '{\"verdict\":\"achieved\",\"reason\":\"checked completion\"}'\n");
    let command = serde_json::json!(["/bin/sh", evaluator]).to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("bad", name, args)),
            Reply::Sse(tool_call(
                "corrected",
                "read",
                &serde_json::json!({"files":[{"path": path}]}).to_string(),
            )),
            Reply::Sse(text("The note was verified.")),
        ],
        ..Plan::default()
    });
    let finished = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
            "--goal",
            "Verify the note",
            "--allow",
            "exec(/bin/sh *)",
            "--goal-check-command",
            &command,
            "--output",
            "json",
            "Read the note",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let report = finished.report();
    assert_eq!(report["exit_reason"], "done");
    assert_eq!(report["goal"]["status"], "achieved");
    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    let feedback = generations[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "bad")
        .expect("failed call is answered to the model");
    let feedback = feedback["content"].as_str().unwrap();
    assert!(feedback.contains(notice), "{feedback}");
    if lists_tools {
        assert!(
            feedback.contains("Declared tools:") && feedback.contains("read"),
            "{feedback}"
        );
    }
    assert!(
        generations[2]["messages"]
            .to_string()
            .contains("verified note")
    );
    let journal = places
        .home_path()
        .join("sessions")
        .join(report["session"].as_str().unwrap())
        .join("events.jsonl");
    let events: Vec<DomainEvent> = std::fs::read_to_string(journal)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            serde_json::from_value(record["payload"]["event"].clone()).ok()
        })
        .collect();
    for (call, expected_ok) in [("bad", false), ("corrected", true)] {
        let item = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ToolCallStarted {
                    call_id: Some(id), ..
                } if id == call => event.env.item,
                _ => None,
            })
            .expect("call is durably started");
        assert!(events.iter().any(|event| event.env.item == Some(item)
            && matches!(event.kind, EventKind::ToolCallCompleted { ok, .. } if ok == expected_ok)));
    }
}
