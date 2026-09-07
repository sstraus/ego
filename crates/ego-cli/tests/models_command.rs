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
    // By column *names*, not by the literal's spacing: the table pads every
    // column to its widest cell, so the gaps in the header move whenever the
    // rows change. The columns are the contract; their width is not.
    let model_header = first
        .stdout
        .lines()
        .next()
        .expect("the model table has a header");
    assert_eq!(
        model_header.split_whitespace().collect::<Vec<_>>(),
        MODEL_HEADER.split_whitespace().collect::<Vec<_>>(),
    );
    assert!(first.stdout.contains("SOURCES"));
    let source_header = first
        .stdout
        .lines()
        .skip_while(|line| *line != "SOURCES")
        .nth(1)
        .expect("the source table has a header");
    assert_eq!(
        source_header.split_whitespace().collect::<Vec<_>>(),
        SOURCE_HEADER.split_whitespace().collect::<Vec<_>>(),
    );
    assert!(
        first
            .stdout
            .lines()
            .all(|line| !line.ends_with(char::is_whitespace))
    );
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
    assert_eq!(
        result.stdout.bytes().filter(|byte| *byte == b'\n').count(),
        1
    );
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let object = value.as_object().unwrap();
    assert_eq!(object.len(), 3);
    assert!(object.contains_key("v"));
    assert!(object.contains_key("models"));
    assert!(object.contains_key("sources"));
    assert_eq!(value["v"], 1);
    assert!(!value["models"].as_array().unwrap().is_empty());
    assert!(value["models"].is_array());
    assert!(
        value["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|model| { model["slug"].as_str().unwrap().starts_with("ollama/") })
    );
    assert!(
        value["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| { source["provider"] == "ollama" })
    );
    assert_eq!(
        stub.requests()
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
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
    assert!(
        value["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| { row["slug"].as_str().unwrap().starts_with("ollama/") })
    );
    assert_eq!(
        stub.requests()
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        ["/api/tags"]
    );
    assert!(
        value["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| { source["provider"] == "ollama" })
    );
}

#[test]
fn provider_filter_excludes_unadmitted_supplied_sources() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    ego.home_file(
        "models.toml",
        r#"
[model."gemini/gemini-2.5-pro".capabilities]
schema_version = 2
max_context_tokens = 1000000
max_output_tokens = 65536
max_tools = 128
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
cancel = "cooperative"
cache = "long"
transport = "sse"
"#,
    );
    let result = ego
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_eq!(result.code, Some(0));
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert!(
        value["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| { source["provider"] == "ollama" })
    );
    assert!(
        !value["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| { source["provider"] == "gemini" })
    );
}

#[test]
fn empty_ollama_refresh_is_available_and_cache_first_reuses_empty_slice() {
    let stub = Stub::start(Plan {
        installed: Vec::new(),
        ..Plan::default()
    });
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    let first = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_eq!(first.code, Some(0));
    assert!(first.stderr.is_empty());
    let first_json: serde_json::Value = serde_json::from_str(&first.stdout).unwrap();
    assert!(first_json["models"].as_array().unwrap().is_empty());
    assert_eq!(first_json["sources"].as_array().unwrap().len(), 1);
    assert_eq!(first_json["sources"][0]["provider"], "ollama");
    assert_eq!(
        first_json["sources"][0]["availability"]["status"],
        "available"
    );

    let second = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--provider", "ollama", "--json"])
        .run();
    assert_eq!(second.code, Some(0));
    assert!(second.stderr.is_empty());
    let second_json: serde_json::Value = serde_json::from_str(&second.stdout).unwrap();
    assert!(second_json["models"].as_array().unwrap().is_empty());
    assert_eq!(
        stub.requests()
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        ["/api/tags"]
    );
}

#[test]
fn kimi_refresh_without_credential_returns_an_unavailable_snapshot_without_transport() {
    let stub = Stub::start(Plan::default());
    let result = Ego::against(&stub)
        .subcommand("models")
        .args(["--provider", "kimi-coding", "--refresh", "--json"])
        .run();

    assert_eq!(result.code, Some(0));
    assert!(result.stderr.is_empty());
    let snapshot: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(snapshot["v"], 1);
    assert_eq!(snapshot["models"].as_array().unwrap().len(), 0);
    assert_eq!(
        snapshot["sources"],
        serde_json::json!([
            {
                "provider": "kimi-coding",
                "provenance": {
                    "source": "discovered",
                    "provider": "kimi-coding"
                },
                "freshness": "current",
                "availability": {
                    "status": "unavailable",
                    "cause": "missing_credential",
                    "message": "no stored subscription credential: run `ego auth login kimi-coding`"
                }
            }
        ])
    );
    assert!(
        stub.requests().is_empty(),
        "missing credentials must fail before transport"
    );
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

/// A row nobody configured is listed, and says where it came from.
///
/// Story `regression_case`'s fifth criterion, both halves in one run because the
/// interesting claim is the *difference* between them: the same command, the
/// same provider, one file added, and only the overridden slug changes source.
///
/// `--provider anthropic` rather than the whole table on purpose. Anthropic's
/// resolver is `Supplied`, so it enumerates from the catalogue and opens no
/// socket, which makes this a test about layering rather than about whichever
/// endpoints happen to answer on the machine running it.
///
/// The home is held here rather than by the harness because the two runs have
/// to share it: `Ego` owns its temp directories and consumes itself on `run`,
/// so a second invocation against the *same* `$EGO_HOME` needs the directory
/// to outlive both.
#[test]
fn builtin_rows_are_listed_as_builtin_until_a_file_overrides_one() {
    let home = tempfile::tempdir().expect("a temp home");
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let sources = || -> Vec<(String, String)> {
        let result = Ego::at(home.path(), workspace.path(), None)
            .subcommand("models")
            .args(["--provider", "anthropic", "--json"])
            .run();
        assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
        let value: serde_json::Value =
            serde_json::from_str(&result.stdout).expect("the snapshot is JSON");
        value["models"]
            .as_array()
            .expect("models is an array")
            .iter()
            .map(|row| {
                (
                    row["slug"].as_str().expect("a slug").to_owned(),
                    row["provenance"]["authority"]
                        .as_str()
                        .expect("a supplied authority")
                        .to_owned(),
                )
            })
            .collect()
    };

    let before = sources();
    assert!(
        !before.is_empty(),
        "no anthropic row is listed with no models.toml anywhere",
    );
    assert!(
        before.iter().all(|(_, source)| source == "builtin"),
        "a row came from somewhere other than the compiled-in catalogue: {before:?}",
    );
    let overridden = before.first().expect("at least one builtin row").0.clone();

    std::fs::write(
        home.path().join("models.toml"),
        format!(
            r#"
[model."{overridden}".capabilities]
schema_version = 4
max_context_tokens = 12345
max_output_tokens = 128000
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
cancel = "cooperative"
cache = "long"
transport = "sse"
"#
        ),
    )
    .expect("the override is writable");

    let after = sources();
    assert_eq!(
        after.len(),
        before.len(),
        "overriding one row changed how many there are: {after:?}",
    );
    for (slug, source) in &after {
        let expected = if *slug == overridden {
            "user"
        } else {
            "builtin"
        };
        assert_eq!(
            source, expected,
            "`{slug}` reads as `{source}` after overriding `{overridden}`: {after:?}",
        );
    }
}
