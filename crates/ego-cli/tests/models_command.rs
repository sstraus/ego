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
    // `gemini` was in this list until
    // `2026-09-07-a-published-contract-is-enough-to-offer-a-row.md` put it in
    // the inventory. `xai` stays: the API-key candidate row genuinely is not
    // built, and `xai-oauth` is a different row on a different contract.
    for provider in ["not-admitted", "xai"] {
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

/// The `openai` row reads as available once its key is set, and its
/// availability turns on the key alone.
///
/// This asserted the opposite until
/// `2026-09-07-a-published-contract-is-enough-to-offer-a-row.md`, which amends
/// the live-promotion ADR: the row is offered on its published contract, and
/// no recording exists or is claimed. The two runs below are the whole point —
/// with the key the row is available and with it absent it is
/// `missing_credential`. A gate that still stood would make both readings
/// identical, so a single assertion could not tell the ADR was honoured.
#[test]
fn the_openai_row_reads_as_available_with_its_key_set_and_wants_only_that_key() {
    let availability = |key: Option<&str>| {
        let mut listed = Ego::nowhere().subcommand("models");
        listed = match key {
            Some(key) => listed.env("OPENAI_API_KEY", key),
            None => listed,
        };
        let listed = listed.arg("--json").run();
        assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);
        let report: serde_json::Value =
            serde_json::from_str(&listed.stdout).expect("the models report is JSON");
        report["sources"]
            .as_array()
            .expect("the report carries a source table")
            .iter()
            .find(|row| row["provider"] == "openai")
            .expect("the openai row is listed at all")
            .clone()
    };

    let keyed = availability(Some("sk-test"));
    assert_eq!(
        keyed["availability"]["status"], "available",
        "the openai row is withheld from an operator holding its key: {keyed}"
    );

    let bare = availability(None);
    assert_eq!(
        bare["availability"]["cause"], "missing_credential",
        "a row lacking only a key reports something other than the key: {bare}"
    );
}

/// Selecting the row gets as far as the transport, which is the whole claim.
///
/// The listing above and this run are the same decision on two surfaces, and
/// they used to be the same *refusal*. What proves the gate is gone is not
/// that the run succeeds — `Ego::nowhere` points every provider at a refused
/// port, so it cannot — but **where** it fails: at a socket rather than at a
/// selection. A run stopped by the promotion gate never reached one, and said
/// so by naming `EGO_SMOKE`.
///
/// The exit code is deliberately asserted as "not usage". `2` is the code
/// `CLI.md` §4.3 reserves for an invocation that must change, and a model this
/// build offers is not one.
#[test]
fn selecting_the_openai_row_reaches_the_transport_rather_than_a_gate() {
    let refused = Ego::nowhere()
        .env("OPENAI_API_KEY", "sk-test")
        .args([
            "--model",
            "openai/gpt-5.6-sol",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "Ask",
        ])
        .run();
    assert_ne!(
        refused.code,
        Some(2),
        "a row this build offers was refused as a usage error: {}",
        refused.stderr
    );
    assert!(
        !refused.stderr.contains("EGO_SMOKE"),
        "the run was stopped by the promotion gate: {}",
        refused.stderr
    );
}
