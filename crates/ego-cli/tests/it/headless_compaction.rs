//! A headless run compacts and continues instead of exiting 4.
//!
//! Every test runs `ego run` as a process against the stub provider. The
//! stub reports a 272000-token window, so the 90% threshold is 244800.
//! Compaction is opt-in (`compaction.headless`).

use crate::support;

use serde_json::Value;
use support::{Ego, Plan, Reply, Stub, text, tool_call};

const CHECKPOINT: &str = "# Objective\nCHECKPOINT-MARKER\n# Binding constraints and preferences\nNone.\n# Completed work and evidence\nRead notes.md.\n# Work in progress\nFinish the task.\n# Decisions and rationale\nNone.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nAnswer.\n# Critical references\nnotes.md";

/// A read tool call whose usage reports `prompt_tokens` of measured occupancy.
fn measured_read(prompt_tokens: u64) -> Reply {
    Reply::Sse(
        tool_call("call_850", "read", "{\"files\":[{\"path\":\"notes.md\"}]}").replace(
            "\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14",
            &format!(
                "\"prompt_tokens\":{prompt_tokens},\"completion_tokens\":4,\"total_tokens\":{}",
                prompt_tokens + 4
            ),
        ),
    )
}

fn run(completions: Vec<Reply>) -> (support::Finished, Stub) {
    run_with(completions, true)
}

fn run_with(completions: Vec<Reply>, headless: bool) -> (support::Finished, Stub) {
    let stub = Stub::start(Plan {
        completions,
        context_length: Some(272_000),
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args(["--sandbox", "off"]);
    ego.file("notes.md", "the contents");
    if headless {
        ego.home_file("config.toml", "[compaction]\nheadless = true\n");
    }
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--retry",
            "0",
            "--output",
            "jsonl",
            "Read notes.md",
        ])
        .run();
    (finished, stub)
}

fn of_type<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

fn request_contains(request: &Value, expected: &str) -> bool {
    request["messages"]
        .as_array()
        .expect("provider request has messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .any(|content| content.contains(expected))
}

fn assert_recovered(finished: &support::Finished, stub: &Stub) {
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let events = finished.events();
    let compacted = of_type(&events, "session_compacted");
    assert_eq!(compacted.len(), 1, "one lineage event: {events:?}");
    let compacted = compacted[0];
    assert_eq!(compacted["source"], support::FIXTURE_SESSION_ID);
    let target = compacted["target"].as_str().expect("target session id");
    assert_ne!(target, support::FIXTURE_SESSION_ID);
    assert!(compacted["source_seq"].as_u64().is_some(), "{compacted}");
    let started = of_type(&events, "session_started");
    assert_eq!(started.len(), 2, "source and successor headers: {events:?}");
    assert_eq!(started[1]["session"], target);
    let answer = of_type(&events, "item_completed")
        .into_iter()
        .rev()
        .find(|event| event["kind"] == "agent_message")
        .expect("a final answer");
    assert_eq!(answer["text"], "RECOVERED-MARK");
    let completed = of_type(&events, "turn_completed");
    assert_eq!(completed.last().expect("a final turn")["ok"], true);
    let requests = stub.generations();
    assert_eq!(requests.len(), 3, "tool turn, checkpoint, continuation");
    // The seed keeps the latest user request by design; the tool result of the
    // source tail must not cross into the successor.
    assert!(
        !request_contains(&requests[2], "the contents"),
        "the successor must not copy the source conversation tail"
    );
    assert!(request_contains(&requests[2], "CHECKPOINT-MARKER"));
}

// Catches: a headless run whose window is exhausted stops with exit 4 instead of compacting.
#[test]
fn an_exhausted_headless_run_compacts_and_continues_in_the_successor() {
    let (finished, stub) = run(vec![
        measured_read(280_000),
        Reply::Sse(text(CHECKPOINT)),
        Reply::Sse(text("RECOVERED-MARK")),
    ]);
    assert_recovered(&finished, &stub);
}

// Catches: a headless tool turn sends another generation past the 90% threshold instead of yielding.
#[test]
fn a_headless_tool_turn_at_the_threshold_compacts_before_the_next_generation() {
    let (finished, stub) = run(vec![
        measured_read(244_800),
        Reply::Sse(text(CHECKPOINT)),
        Reply::Sse(text("RECOVERED-MARK")),
    ]);
    assert_recovered(&finished, &stub);
}

// Catches: a failed headless compaction exits 0, spins on the provider, or loses its typed error.
#[test]
fn a_failed_headless_compaction_keeps_the_typed_overflow_and_exit_four() {
    let (finished, stub) = run(vec![measured_read(280_000), Reply::Status(400)]);
    assert_eq!(finished.code, Some(4), "stderr: {}", finished.stderr);
    let events = finished.events();
    assert!(
        of_type(&events, "session_compacted").is_empty(),
        "{events:?}"
    );
    let settled = of_type(&events, "turn_completed");
    assert!(
        settled
            .iter()
            .any(|turn| turn["exit_reason"] == "ContextOverflow"),
        "the source turn keeps its typed overflow: {settled:?}"
    );
    assert!(
        finished.stderr.contains("compaction failed"),
        "stderr: {}",
        finished.stderr
    );
    assert_eq!(stub.generations().len(), 2, "failed recovery must not spin");
}

// Catches: a headless run compacts without the user's opt-in, spending paid requests unasked.
#[test]
fn without_the_opt_in_an_exhausted_headless_run_ends_with_exit_four_and_no_compaction() {
    let (finished, stub) = run_with(
        vec![measured_read(280_000), Reply::Sse(text(CHECKPOINT))],
        false,
    );
    assert_eq!(finished.code, Some(4), "stderr: {}", finished.stderr);
    let events = finished.events();
    assert!(
        of_type(&events, "session_compacted").is_empty(),
        "{events:?}"
    );
    assert!(
        of_type(&events, "turn_completed")
            .iter()
            .any(|turn| turn["exit_reason"] == "ContextOverflow"),
        "{events:?}"
    );
    assert_eq!(stub.generations().len(), 1, "no summarizer request is sent");
}
