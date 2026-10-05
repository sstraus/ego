//! Cache reuse requires the same actual wire bytes at the inherited boundary.

mod support;

use std::collections::BTreeMap;

use ego_core::event::domain::EventKind;
use ego_core::store::session_store::SessionEntry;
use serde_json::{Value, value::RawValue};
use support::{Finished, Places, Plan, Stub};

const MODEL: &str = "anthropic/claude-opus-5-5";

fn turn(places: &Places, stub: &Stub, id: &str, prompt: &str, resume: bool) -> Finished {
    let ego = places.ego(stub).env("ANTHROPIC_API_KEY", "sk-test").args([
        "--thinking",
        "low",
        "--model",
        MODEL,
        "--sandbox",
        "off",
        "--output",
        "json",
    ]);
    let finished = if resume {
        ego.subcommand("resume").args([id, prompt]).run()
    } else {
        ego.args(["--session-id", id, prompt]).run()
    };
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    finished
}

fn fork(places: &Places, stub: &Stub, at: Option<&str>) -> String {
    let ego = places
        .ego(stub)
        .subcommand("fork")
        .arg(support::FIXTURE_SESSION_ID);
    let finished = match at {
        Some(message) => ego.args(["--at", message]).run(),
        None => ego.run(),
    };
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    finished.stdout.trim().to_owned()
}

fn requests(stub: &Stub) -> Vec<String> {
    stub.requests()
        .into_iter()
        .filter(|request| request.path.ends_with("/messages"))
        .map(|request| request.body)
        .collect()
}

/// Keep raw JSON slices: reparsing and serializing would hide byte differences.
fn stable_prefix(body: &str, history_messages: usize) -> Vec<&str> {
    let fields: BTreeMap<&str, &RawValue> = serde_json::from_str(body).unwrap();
    let system: Value = serde_json::from_str(fields["system"].get()).unwrap();
    assert_eq!(
        system[0]["cache_control"],
        serde_json::json!({"type":"ephemeral"})
    );
    let messages: Vec<&RawValue> = serde_json::from_str(fields["messages"].get()).unwrap();
    assert!(
        messages.len() > history_messages,
        "no volatile/request tail"
    );
    for (index, message) in messages.iter().enumerate() {
        let value: Value = serde_json::from_str(message.get()).unwrap();
        let blocks = value["content"].as_array().unwrap();
        let marked: Vec<_> = blocks
            .iter()
            .filter(|block| block.get("cache_control").is_some())
            .collect();
        if index + 1 == history_messages {
            assert_eq!(value["role"], "assistant");
            assert_eq!(
                marked.len(),
                1,
                "last stable message must have one breakpoint"
            );
            assert_eq!(
                blocks.last().unwrap()["cache_control"],
                serde_json::json!({"type":"ephemeral"})
            );
        } else {
            assert!(
                marked.is_empty(),
                "breakpoint on another history block or volatile tail"
            );
        }
    }
    let mut prefix = vec![fields["tools"].get(), fields["system"].get()];
    prefix.extend(
        messages[..history_messages]
            .iter()
            .map(|message| message.get()),
    );
    prefix
}

// Catches: a tip fork re-renders changed instructions, reorders tools or moves
// the stable breakpoint instead of inheriting the source request prefix.
#[test]
fn tip_fork_keeps_source_continuation_bytes_through_the_stable_breakpoint() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    places.file(
        "AGENTS.md",
        "Answer briefly. Original durable instructions.\n",
    );
    turn(
        &places,
        &stub,
        support::FIXTURE_SESSION_ID,
        "first topic",
        false,
    );
    turn(
        &places,
        &stub,
        support::FIXTURE_SESSION_ID,
        "second topic",
        true,
    );
    let child = fork(&places, &stub, None);
    turn(
        &places,
        &stub,
        support::FIXTURE_SESSION_ID,
        "source continuation",
        true,
    );
    places.file(
        "AGENTS.md",
        "Changed instructions must not rewrite the inherited epoch.\n",
    );
    turn(&places, &stub, &child, "a different topic", true);
    let bodies = requests(&stub);
    assert_eq!(bodies.len(), 4);
    assert_ne!(bodies[2], bodies[3], "the current prompts must differ");
    assert_eq!(stable_prefix(&bodies[2], 4), stable_prefix(&bodies[3], 4));
}

// Catches: an earlier fork includes later source messages or uses the current
// epoch instead of the selected turn's durable prefix and breakpoint.
#[test]
fn earlier_turn_fork_keeps_source_boundary_bytes_through_the_stable_breakpoint() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    places.file(
        "AGENTS.md",
        "Answer briefly. Original durable instructions.\n",
    );
    turn(
        &places,
        &stub,
        support::FIXTURE_SESSION_ID,
        "first topic",
        false,
    );
    let journal = std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .unwrap();
    let message = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|record| record.get("payload").cloned())
        .map(|payload| serde_json::from_value::<SessionEntry>(payload).unwrap())
        .find_map(|entry| match entry {
            SessionEntry::Event(event)
                if matches!(event.kind, EventKind::AssistantMessage { .. }) =>
            {
                event.env.item
            }
            _ => None,
        })
        .expect("the real first turn persisted an assistant message")
        .to_string();
    turn(
        &places,
        &stub,
        support::FIXTURE_SESSION_ID,
        "later source topic",
        true,
    );
    let child = fork(&places, &stub, Some(&message));
    places.file(
        "AGENTS.md",
        "Changed instructions must not rewrite the inherited epoch.\n",
    );
    turn(
        &places,
        &stub,
        &child,
        "an earlier branch on a different topic",
        true,
    );
    let bodies = requests(&stub);
    assert_eq!(bodies.len(), 3);
    assert_ne!(bodies[1], bodies[2], "the current prompts must differ");
    assert_eq!(stable_prefix(&bodies[1], 2), stable_prefix(&bodies[2], 2));
}
