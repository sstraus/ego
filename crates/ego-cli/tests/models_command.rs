//! RED contract for the real ego models process surface.

mod support;
use support::{Ego, Plan, Stub};

const MODEL_HEADER: &str =
    "SLUG  CONTEXT  OUTPUT  REASONING  METERING  SOURCE  FRESHNESS  AVAILABILITY";
const SOURCE_HEADER: &str = "PROVIDER  SOURCE  FRESHNESS  AVAILABILITY";

#[test]
fn text_always_renders_both_tables_deterministically_without_trailing_space() {
    let first = Ego::nowhere().subcommand("models").run();
    let second = Ego::nowhere().subcommand("models").run();
    assert_eq!(first.code, Some(0));
    assert_eq!(first.stdout, second.stdout);
    assert!(first.stdout.contains(MODEL_HEADER));
    assert!(first.stdout.contains("SOURCES"));
    assert!(first.stdout.contains(SOURCE_HEADER));
    assert!(first.stdout.lines().all(|line| !line.ends_with(char::is_whitespace)));
}

#[test]
fn json_is_exactly_the_snapshot_envelope_with_one_newline_and_clean_stdout() {
    let stub = Stub::start(Plan {
        installed: vec!["qwen3".to_string()],
        ..Plan::default()
    });
    let result = Ego::against(&stub)
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_eq!(result.code, Some(0));
    assert!(result.stderr.is_empty());
    assert_eq!(result.stdout.bytes().filter(|byte| *byte == b'\n').count(), 1);
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let object = value.as_object().unwrap();
    assert_eq!(object.len(), 3);
    assert!(object.contains_key("v"));
    assert!(object.contains_key("models"));
    assert!(object.contains_key("sources"));
    assert_eq!(value["v"], 1);
    assert!(!value["models"].as_array().unwrap().is_empty());
    assert!(value["models"].is_array());
    assert!(value["models"].as_array().unwrap().iter().all(|model| {
        model["slug"].as_str().unwrap().starts_with("ollama/")
    }));
    assert!(value["sources"].as_array().unwrap().iter().all(|source| {
        source["provider"] == "ollama"
    }));
    assert_eq!(
        stub.requests().iter().map(|request| request.path.as_str()).collect::<Vec<_>>(),
        ["/api/tags"]
    );
}

#[test]
fn provider_filter_is_applied_before_transport_and_refresh_is_filter_scoped() {
    let stub = Stub::start(Plan::default());
    let result = Ego::against(&stub)
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_eq!(result.code, Some(0));
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert!(value["models"].as_array().unwrap().iter().all(|row| {
        row["slug"].as_str().unwrap().starts_with("ollama/")
    }));
    assert_eq!(
        stub.requests().iter().map(|request| request.path.as_str()).collect::<Vec<_>>(),
        ["/api/tags"]
    );
    assert!(value["sources"].as_array().unwrap().iter().all(|source| {
        source["provider"] == "ollama"
    }));
}

#[test]
fn unknown_provider_exits_two_before_any_transport() {
    for provider in ["not-admitted", "xai", "gemini"] {
        let stub = Stub::start(Plan {
            installed: vec!["qwen3".to_string()],
            ..Plan::default()
        });
        let result = Ego::against(&stub)
            .subcommand("models")
            .args(["--provider", provider])
            .run();
        assert_eq!(result.code, Some(2), "{provider}");
        assert!(result.stderr.contains(provider), "{provider}");
        assert!(result.stdout.is_empty(), "{provider}");
        assert!(stub.requests().is_empty(), "{provider}");
    }
}
