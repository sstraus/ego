//! The loop guard's terminal result must survive the real headless process.

use crate::support;

use support::{Places, Plan, Reply, Stub, text, tool_call};

// Catches: a detected tool loop is incorrectly reported as process success.
#[test]
fn repeated_tool_loop_exits_nonzero_with_visible_cause() {
    let places = Places::new();
    let path = places.file("note.txt", "pending");
    let args = serde_json::json!({"path":path}).to_string();
    let mut replies: Vec<_> = (0..8)
        .map(|index| Reply::Sse(tool_call(&format!("loop-{index}"), "read", &args)))
        .collect();
    replies.push(Reply::Sse(text("done")));
    let stub = Stub::start(Plan {
        completions: replies,
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
            "--output",
            "json",
            "read the note",
        ])
        .run();
    assert_eq!(finished.code, Some(7), "{}", finished.stderr);
    assert_eq!(finished.report()["exit_reason"], "guard");
    assert!(
        finished.stderr.contains("repeated tool call limit"),
        "{}",
        finished.stderr
    );
    let journal = places
        .home_path()
        .join("sessions")
        .join(finished.report()["session"].as_str().expect("session id"))
        .join("events.jsonl");
    let records = std::fs::read_to_string(journal).expect("durable journal");
    assert!(
        records.lines().any(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["payload"]["event"]["kind"]["exit_reason"] == "Guard"
        }),
        "no durable Guard turn outcome"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 8);
    assert!(
        generations[3]["messages"]
            .to_string()
            .contains("Tool loop warning")
    );
}

// Catches: repeated undeclared tool names, answered as failed feedback, escape
// the existing loop guard and run forever instead of ending the turn.
#[cfg(unix)]
#[test]
fn repeated_undeclared_tool_calls_end_at_the_existing_guard_limit() {
    let places = Places::new();
    let mut replies: Vec<_> = (0..8)
        .map(|index| Reply::Sse(tool_call(&format!("bad-{index}"), "ls", "{}")))
        .collect();
    replies.push(Reply::Sse(text("done")));
    let stub = Stub::start(Plan {
        completions: replies,
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
            "--output",
            "json",
            "list the files",
        ])
        .run();
    assert_eq!(finished.code, Some(7), "{}", finished.stderr);
    assert_eq!(finished.report()["exit_reason"], "guard");
    assert!(
        finished.stderr.contains("repeated tool call limit"),
        "{}",
        finished.stderr
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 8);
    assert!(
        generations[3]["messages"]
            .to_string()
            .contains("Tool loop warning")
    );
}
