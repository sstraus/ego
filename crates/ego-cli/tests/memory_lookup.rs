//! Typed memory publication through the real CLI and guarded child path.
#![cfg(unix)]
mod support;
use std::os::unix::fs::PermissionsExt;
use support::{Ego, Places, Plan, Reply, Stub, text, tool_call};

fn setup(script: Option<&str>) -> Places {
    let places = Places::new();
    std::fs::write(places.home_path().join("config.toml"),
        "[profile.coordinator]\nmodel='ollama/qwen3'\nthinking='off'\nsandbox='off'\n[profile.coordinator.permissions]\nallow=['exec(mdkb *)']\n").unwrap();
    let bin = places.workspace_path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    if let Some(script) = script {
        let path = bin.join("mdkb");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    places
}

fn run(places: &Places, stub: &Stub) -> support::Finished {
    Ego::at(places.home_path(), places.workspace_path(), Some(stub))
        .args(["--profile", "coordinator", "Lookup memory"])
        .env(
            "PATH",
            places.workspace_path().join("bin").to_str().unwrap(),
        )
        .run()
}

fn stub(entry_type: &str) -> Stub {
    Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "memory-call",
                "memory_lookup",
                &serde_json::json!({
                    "query": "workspace trust", "entry_type": entry_type,
                })
                .to_string(),
            )),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    })
}

fn tool_answer(stub: &Stub) -> String {
    let requests = stub.generations();
    requests.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

// Catches full memory bodies, untyped records and an unbounded result reaching context.
#[test]
fn recorded_mdkb_results_publish_only_five_typed_summaries_within_one_kib() {
    let places = setup(Some(
        "printf '%s\\n' \"$@\" > argv.txt\nexec /bin/cat response.json",
    ));
    // Repeat synthetic schema records to simulate an over-returning child.
    let recorded: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../ego-tools/tests/fixtures/mdkb_memory_decisions.json"
    ))
    .unwrap();
    let response: Vec<_> = recorded.iter().cycle().take(9).collect();
    std::fs::write(
        places.workspace_path().join("response.json"),
        serde_json::to_vec(&response).unwrap(),
    )
    .unwrap();
    let stub = stub("decision");
    let finished = run(&places, &stub);
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let answer = tool_answer(&stub);
    assert!(answer.len() <= 1024, "{} bytes", answer.len());
    let entries: Vec<serde_json::Value> = serde_json::from_str(&answer).unwrap();
    assert!(!entries.is_empty() && entries.len() <= 5);
    for entry in entries {
        assert_eq!(entry.as_object().unwrap().len(), 3);
        assert_eq!(entry["type"], "decision");
        assert!(entry["id"].is_string() && entry["title"].is_string());
        assert!(
            recorded
                .iter()
                .any(|source| source["id"] == entry["id"] && source["title"] == entry["title"])
        );
    }
    let argv = std::fs::read_to_string(places.workspace_path().join("argv.txt")).unwrap();
    assert_eq!(
        argv,
        "search\nworkspace trust\n--scope\nmemory\n--entry-type\ndecision\n--limit\n5\n--format\njson\n"
    );
}

// Catches a child ignoring entry_type and disclosing decisions in a problem lookup.
#[test]
fn requested_memory_type_is_honoured_even_if_the_child_returns_other_types() {
    let places = setup(Some("exec /bin/cat response.json"));
    std::fs::write(
        places.workspace_path().join("response.json"),
        include_str!("../../ego-tools/tests/fixtures/mdkb_memory_decisions.json"),
    )
    .unwrap();
    let stub = stub("problem");
    let finished = run(&places, &stub);
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert_eq!(tool_answer(&stub), "[]");
}

// Catches missing/failing/dead mdkb returning success or holding the turn indefinitely.
#[test]
fn missing_failing_malformed_and_dead_mdkb_return_named_tool_errors() {
    for (script, expected) in [
        (None, "mdkb_unavailable_or_failed"),
        (Some("exit 7"), "mdkb_unavailable_or_failed"),
        (Some("printf 'not json'"), "mdkb_invalid_response"),
        (Some("exec /bin/sleep 30"), "mdkb_timeout"),
    ] {
        let places = setup(script);
        let stub = stub("handoff");
        let started = std::time::Instant::now();
        let finished = run(&places, &stub);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        assert!(
            tool_answer(&stub).contains(expected),
            "{}",
            tool_answer(&stub)
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "dead child held the turn"
        );
    }
}

// Catches the coordinator-only tool escaping into standalone sessions.
#[test]
fn standalone_tool_offer_does_not_expose_memory_lookup() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("done"))],
        ..Plan::default()
    });
    let finished = Ego::against(&stub)
        .args(["--model", "ollama/qwen3", "Hello"])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let request = stub.generations().pop().unwrap();
    assert!(!request["tools"].to_string().contains("memory_lookup"));
}
