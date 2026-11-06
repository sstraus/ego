//! The owner's `tools` key selects the model tools of every run.

use crate::support;

use support::{MODEL, Places, Plan, Reply, Stub};

fn plan() -> Plan {
    Plan {
        installed: vec![MODEL.to_owned()],
        context_length: Some(32_768),
        completions: vec![Reply::Sse(support::text("done"))],
        ..Plan::default()
    }
}

fn tool_names(stub: &Stub) -> Vec<String> {
    stub.generations()[0]["tools"]
        .as_array()
        .expect("the request carries a tool list")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

// Catches: the `tools` key is accepted and stored but the session still registers every tool.
#[test]
fn the_tools_key_limits_the_tools_a_run_offers() {
    let stub = Stub::start(plan());
    let places = Places::new();
    places.ego(&stub).home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\ntools = [\"read\"]\n",
    );
    let run = places
        .ego(&stub)
        .args(["--sandbox", "off"])
        .arg("Reply READY.")
        .run();
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(tool_names(&stub), vec!["read".to_owned()]);
}

// Catches: the key wins over an explicit `--tools`, so a script cannot widen one run.
#[test]
fn the_tools_flag_overrides_the_tools_key() {
    let stub = Stub::start(plan());
    let places = Places::new();
    places.ego(&stub).home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\ntools = [\"read\"]\n",
    );
    let run = places
        .ego(&stub)
        .args(["--sandbox", "off", "--tools", "read,glob"])
        .arg("Reply READY.")
        .run();
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let mut names = tool_names(&stub);
    names.sort();
    assert_eq!(names, vec!["glob".to_owned(), "read".to_owned()]);
}
