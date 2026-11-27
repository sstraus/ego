//! Cache reuse requires the same actual wire bytes at the inherited boundary.

use crate::support;

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
fn stable_prefix<'a>(body: &'a str, current_prompt: &str) -> Vec<&'a str> {
    let fields: BTreeMap<&str, &RawValue> = serde_json::from_str(body).unwrap();
    let system: Value = serde_json::from_str(fields["system"].get()).unwrap();
    assert_eq!(
        system[0]["cache_control"],
        serde_json::json!({"type":"ephemeral"})
    );
    let messages: Vec<&RawValue> = serde_json::from_str(fields["messages"].get()).unwrap();
    let values: Vec<Value> = messages
        .iter()
        .map(|message| serde_json::from_str(message.get()).unwrap())
        .collect();
    // The current intent is stable for this request, but is not inherited by
    // the sibling. Compare raw inherited bytes before that independently known
    // fixture intent, not a historical hard-coded number of role messages.
    let boundary = values
        .iter()
        .position(|message| {
            message["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|block| block["text"].as_str() == Some(current_prompt))
        })
        .expect("current intent is present as its own content block");
    assert!(boundary > 0, "fixture must contain inherited history");
    let breakpoints: Vec<_> = values
        .iter()
        .enumerate()
        .flat_map(|(index, message)| {
            message["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|block| block.get("cache_control").is_some())
                .map(move |block| (index, block))
        })
        .collect();
    assert_eq!(breakpoints.len(), 1, "exactly one message breakpoint");
    assert_eq!(
        breakpoints[0].0, boundary,
        "breakpoint follows the current intent, before volatile context"
    );
    assert_eq!(
        breakpoints[0].1["cache_control"],
        serde_json::json!({"type":"ephemeral"})
    );
    // Identical context is not appended twice. A changed context may follow
    // the intent, but no tail block may move the cache boundary.
    assert!(values[boundary + 1..].iter().all(|message| {
        message["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block.get("cache_control").is_none())
    }));
    let mut prefix = vec![fields["tools"].get(), fields["system"].get()];
    prefix.extend(messages[..boundary].iter().map(|message| message.get()));
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
    assert_eq!(
        stable_prefix(&bodies[2], "source continuation"),
        stable_prefix(&bodies[3], "a different topic")
    );
}

// Catches: the combined command writes into the source or rebuilds the
// inherited request instead of running the normal resume path in the fork.
#[test]
fn resume_fork_runs_prompt_in_a_new_session_without_touching_source() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let source = support::FIXTURE_SESSION_ID;
    turn(&places, &stub, source, "settled source turn", false);
    let journal = places
        .home_path()
        .join("sessions")
        .join(source)
        .join("events.jsonl");
    let before = std::fs::read(&journal).unwrap();
    let result = places
        .ego(&stub)
        .without_fixture_thinking()
        .env("ANTHROPIC_API_KEY", "sk-test")
        .subcommand("resume")
        .args([
            source,
            "--fork",
            "--sandbox",
            "off",
            "--output",
            "json",
            "fork prompt",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let report = result.report();
    let child = report["session"].as_str().unwrap();
    assert_ne!(child, source);
    assert_eq!(report["result"], "It is done.");
    assert!(report["usage"]["cache_read_tokens"].is_u64());
    assert_eq!(std::fs::read(journal).unwrap(), before);
    turn(&places, &stub, source, "source continuation", true);
    turn(&places, &stub, child, "fork continuation", true);
    let bodies = requests(&stub);
    assert_eq!(bodies.len(), 4);
    assert_eq!(
        stable_prefix(&bodies[1], "fork prompt"),
        stable_prefix(&bodies[2], "source continuation")
    );
    assert!(bodies[3].contains("fork prompt"));
    assert!(!bodies[3].contains("source continuation"));
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
    assert_eq!(
        stable_prefix(&bodies[1], "later source topic"),
        stable_prefix(&bodies[2], "an earlier branch on a different topic")
    );
}

fn skill_file(name: &str) -> String {
    format!("---\nname: {name}\ndescription: {name} fixture description\n---\n{name} body\n")
}

// Catches: a fork or resume re-rendering the skill tool description from the
// live catalogue, which changes the cached tool block of the whole lineage.
#[test]
fn skill_change_keeps_the_fork_and_resume_tool_prefix() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let ego = places.ego(&stub);
    ego.home_file("skills/alpha/SKILL.md", &skill_file("alpha"));
    turn(
        &places,
        &stub,
        support::FIXTURE_SESSION_ID,
        "first topic",
        false,
    );
    let child = fork(&places, &stub, None);
    ego.home_file("skills/beta/SKILL.md", &skill_file("beta"));
    std::fs::remove_dir_all(places.home_path().join("skills").join("alpha")).unwrap();
    turn(
        &places,
        &stub,
        support::FIXTURE_SESSION_ID,
        "source continuation",
        true,
    );
    turn(&places, &stub, &child, "a different topic", true);
    let bodies = requests(&stub);
    assert_eq!(bodies.len(), 3);
    let tools = |body: &str| {
        let fields: BTreeMap<&str, &RawValue> = serde_json::from_str(body).unwrap();
        fields["tools"].get().to_owned()
    };
    assert!(tools(&bodies[0]).contains("alpha fixture description"));
    assert_eq!(tools(&bodies[1]), tools(&bodies[0]), "resume tool block");
    assert_eq!(tools(&bodies[2]), tools(&bodies[0]), "fork tool block");
    assert_eq!(
        stable_prefix(&bodies[1], "source continuation"),
        stable_prefix(&bodies[2], "a different topic")
    );
    // The change still reaches the model, after the inherited history.
    for body in &bodies[1..] {
        let fields: BTreeMap<&str, &RawValue> = serde_json::from_str(body).unwrap();
        let messages = fields["messages"].get();
        assert!(messages.contains("beta fixture description"), "{messages}");
        assert!(messages.contains("No longer available"), "{messages}");
    }
}

// Catches: adding the first skill or removing the last skill changes which
// tools exist, not just their descriptions.
#[test]
fn empty_skill_catalogue_transitions_keep_the_fork_tool_prefix() {
    for initially_empty in [true, false] {
        let places = Places::new();
        let stub = Stub::start(Plan::default());
        let ego = places.ego(&stub);
        let skill = "skills/only/SKILL.md";
        if !initially_empty {
            ego.home_file(skill, &skill_file("only"));
        }
        turn(&places, &stub, support::FIXTURE_SESSION_ID, "source", false);
        let child = fork(&places, &stub, None);
        if initially_empty {
            ego.home_file(skill, &skill_file("only"));
        } else {
            std::fs::remove_file(places.home_path().join(skill)).unwrap();
        }
        turn(&places, &stub, &child, "fork", true);
        let bodies = requests(&stub);
        assert_eq!(bodies.len(), 2);
        let first: BTreeMap<&str, &RawValue> = serde_json::from_str(&bodies[0]).unwrap();
        let next: BTreeMap<&str, &RawValue> = serde_json::from_str(&bodies[1]).unwrap();
        let desc = "only fixture description";
        assert_eq!(first["tools"].get().contains(desc), !initially_empty);
        assert_eq!(
            first["tools"].get(),
            next["tools"].get(),
            "initially_empty={initially_empty}"
        );
        let messages: Vec<Value> = serde_json::from_str(next["messages"].get()).unwrap();
        let expected = if initially_empty {
            desc
        } else {
            "No longer available: [\"only\"]"
        };
        assert!(
            messages
                .iter()
                .filter_map(|message| message["content"].as_array())
                .flatten()
                .filter_map(|block| block["text"].as_str())
                .flat_map(str::lines)
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter_map(|context| context.get("snapshot").and_then(Value::as_array).cloned())
                .flatten()
                .any(|note| note.as_str().is_some_and(|text| text.contains(expected))),
            "missing {expected:?} in {messages:?}"
        );
    }
}
