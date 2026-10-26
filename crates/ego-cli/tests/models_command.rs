//! contract for the real ego models process surface.

mod support;
use std::path::{Path, PathBuf};
use support::{Ego, Plan, Stub};

use ego_ai::catalogue::DiscoveryCacheV2;

const MODEL_HEADER: &str =
    "SLUG  CONTEXT  OUTPUT  REASONING  METERING  FAST  SOURCE  FRESHNESS  AVAILABILITY";
const SOURCE_HEADER: &str = "PROVIDER  SOURCE  FRESHNESS  AVAILABILITY";
const CACHE_RELATIVE_PATH: &str = "model-catalogue/snapshot-v1.json";

fn cache_path(home: &Path) -> PathBuf {
    home.join(CACHE_RELATIVE_PATH)
}

fn seed_current_cache(home: &Path, workspace: &Path) -> Vec<u8> {
    let stub = Stub::start(Plan::default());
    let result = Ego::at(home, workspace, Some(&stub))
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
    std::fs::read(cache_path(home)).expect("refresh writes the discovery cache")
}

fn stale_capabilities(bytes: &[u8]) -> Vec<u8> {
    stale_capabilities_for(bytes, "ollama")
}

fn stale_capabilities_for(bytes: &[u8], provider: &str) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("cache is JSON");
    value["providers"][provider]["models"][0]["entry"]["capabilities"]["schema_version"] =
        serde_json::json!(4);
    serde_json::to_vec(&value).expect("stale cache remains JSON")
}

fn stale_kimi_cache(bytes: &[u8]) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("cache is JSON");
    let mut slice = value["providers"]["ollama"].clone();
    for model in slice["models"].as_array_mut().expect("cached models") {
        model["entry"]["slug"] = serde_json::json!("kimi-coding/qwen3");
        model["entry"]["provenance"]["provider"] = serde_json::json!("kimi-coding");
    }
    value["providers"]["kimi-coding"] = slice;
    value["providers"].as_object_mut().unwrap().remove("ollama");
    let stale = serde_json::to_vec(&value).expect("provider cache remains JSON");
    stale_capabilities_for(&stale, "kimi-coding")
}

fn newer_cache(bytes: &[u8]) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("cache is JSON");
    value["v"] = serde_json::json!(3);
    serde_json::to_vec(&value).expect("newer cache remains JSON")
}

fn write_cache(home: &Path, bytes: &[u8]) {
    std::fs::write(cache_path(home), bytes).expect("cache is writable");
}

// Catches: an upgrade refuses interactive startup on its own obsolete derived cache.
#[test]
fn obsolete_schema_five_cache_does_not_block_interactive_startup() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("model-catalogue")).unwrap();
    let stale = include_bytes!("fixtures/discovery-cache-capabilities-v5.json");
    write_cache(home.path(), stale);
    std::fs::write(
        home.path().join("models.toml"),
        "[default_model]\nollama = \"ollama/qwen3\"\n",
    )
    .unwrap();
    let stub = Stub::start(Plan::default());
    let result = Ego::at(home.path(), workspace.path(), Some(&stub))
        .conversing()
        .args(["--sandbox", "off", "--thinking", "off"])
        .run_pty("hello");
    assert_eq!(
        result.code,
        Some(0),
        "stdout: {} stderr: {}",
        result.stdout,
        result.stderr
    );
    assert_eq!(stub.generations().len(), 1, "startup must reach generation");
    assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), stale);
}

// Catches: obsolete display facts prevent an explicitly selected headless run.
#[test]
fn obsolete_schema_five_cache_does_not_block_headless_generation() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("model-catalogue")).unwrap();
    let stale = include_bytes!("fixtures/discovery-cache-capabilities-v5.json");
    write_cache(home.path(), stale);
    let stub = Stub::start(Plan::default());
    let result = Ego::at(home.path(), workspace.path(), Some(&stub))
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--thinking",
            "off",
            "hello",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(stub.generations().len(), 1);
    assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), stale);
}

// Catches: obsolete-cache recovery silently discards corrupt or future cache bytes.
#[test]
fn startup_rejects_corrupt_and_newer_cache_without_replacing_bytes() {
    let old = include_bytes!("fixtures/discovery-cache-capabilities-v5.json");
    let mut future_capabilities: serde_json::Value = serde_json::from_slice(old).unwrap();
    future_capabilities["providers"]["ollama"]["models"][0]["entry"]["capabilities"]["schema_version"] =
        serde_json::json!(7);
    for (bytes, diagnostic) in [
        (b"not-json".to_vec(), "corrupt"),
        (newer_cache(old), "unsupported"),
        (
            serde_json::to_vec(&future_capabilities).unwrap(),
            "unsupported",
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("model-catalogue")).unwrap();
        write_cache(home.path(), &bytes);
        let result = Ego::at(home.path(), workspace.path(), None)
            .args(["--sandbox", "off", "hello"])
            .run();
        assert_ne!(result.code, Some(0));
        assert!(result.stderr.contains(diagnostic), "{}", result.stderr);
        assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), bytes);
    }
}

// Catches: an I/O error is mistaken for a missing, rebuildable cache.
#[test]
fn startup_reports_unreadable_cache_without_removing_it() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(cache_path(home.path())).unwrap();
    let result = Ego::at(home.path(), workspace.path(), None)
        .args(["--sandbox", "off", "hello"])
        .run();
    assert_ne!(result.code, Some(0));
    assert!(
        result.stderr.contains("cache could not be accessed"),
        "{}",
        result.stderr
    );
    assert!(cache_path(home.path()).is_dir());
}

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
    assert_eq!(value["v"], 2);
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
fn embedding_only_ollama_model_is_absent_from_chat_catalogue_and_refused_for_run() {
    // Catches: the CLI catalogue hides the embedding row but explicit --model
    // still sends a chat request to it.
    let tags = r#"{"models":[{"name":"qwen3","capabilities":["completion"]},{"name":"embeddinggemma:300m-qat-q8_0","capabilities":["embedding"]}]}"#;
    let stub = Stub::start(Plan {
        tags_refusal: Some((200, tags.to_string())),
        ..Plan::default()
    });
    let listing = Ego::against(&stub)
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_eq!(listing.code, Some(0), "{}", listing.stderr);
    let snapshot: serde_json::Value =
        serde_json::from_str(&listing.stdout).expect("catalogue JSON");
    let slugs: Vec<_> = snapshot["models"]
        .as_array()
        .expect("model rows")
        .iter()
        .filter_map(|row| row["slug"].as_str())
        .collect();
    assert_eq!(slugs, ["ollama/qwen3"]);

    let stub = Stub::start(Plan {
        tags_refusal: Some((200, tags.to_string())),
        ..Plan::default()
    });
    let run = Ego::against(&stub)
        .args(["--model", "ollama/embeddinggemma:300m-qat-q8_0", "Ask"])
        .run();
    assert_ne!(run.code, Some(0));
    assert!(run.stderr.contains("embedding-only"), "{}", run.stderr);
    assert_eq!(
        stub.requests()
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        ["/api/tags"],
        "explicit chat selection must stop before model load or generation"
    );
}

#[test]
fn user_row_for_ollama_does_not_replace_the_daemon_facts_the_run_resolves() {
    // Catches: `ego models` overlays a models.toml row on every discovered
    // provider, but only the OpenRouter resolver reads it, so the listing
    // states an Ollama context and reasoning set that `ego run` never uses.
    let home = tempfile::tempdir().expect("a temp home");
    let workspace = tempfile::tempdir().expect("a temp workspace");
    std::fs::write(
        home.path().join("models.toml"),
        r#"
[model."ollama/qwen3".capabilities]
schema_version = 6
max_context_tokens = 12345
max_output_tokens = 4096
supports_parallel_tool_calls = true
reasoning_carryover = "unsupported"
reasoning_efforts = "any"
cancel = "cooperative"
cache = "none"
transport = "sse"
"#,
    )
    .expect("the user row is writable");
    let tags = r#"{"models":[{"name":"qwen3","capabilities":["completion","thinking"]}]}"#;
    let stub = Stub::start(Plan {
        tags_refusal: Some((200, tags.to_string())),
        ..Plan::default()
    });
    let listing = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_eq!(listing.code, Some(0), "{}", listing.stderr);
    let snapshot: serde_json::Value =
        serde_json::from_str(&listing.stdout).expect("catalogue JSON");
    let row = snapshot["models"]
        .as_array()
        .expect("model rows")
        .iter()
        .find(|row| row["slug"] == "ollama/qwen3")
        .expect("the daemon's model is listed");
    assert_eq!(row["provenance"]["source"], "discovered", "{row}");
    assert_ne!(row["capabilities"]["max_context_tokens"], 12345, "{row}");
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
schema_version = 6
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
fn refresh_rebuilds_obsolete_cache_atomically_and_publishes_current_bytes() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let current = seed_current_cache(home.path(), workspace.path());
    let stale = stale_capabilities(&current);
    write_cache(home.path(), &stale);

    let stub = Stub::start(Plan::default());
    let result = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
    let rebuilt = std::fs::read(cache_path(home.path())).unwrap();
    assert_ne!(
        rebuilt, stale,
        "successful rebuild must replace stale bytes"
    );
    let cache = DiscoveryCacheV2::decode(&rebuilt).expect("rebuild publishes current schema");
    assert!(cache.providers.contains_key("ollama"));
}

#[test]
fn obsolete_cache_preserves_bytes_when_provider_transport_fails() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let current = seed_current_cache(home.path(), workspace.path());
    let stale = stale_capabilities(&current);
    write_cache(home.path(), &stale);

    let stub = Stub::start(Plan {
        tags_refusal: Some((500, "discovery refused".to_string())),
        ..Plan::default()
    });
    let result = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_ne!(result.code, Some(0));
    assert!(
        !result.stderr.is_empty(),
        "transport failure must be actionable"
    );
    assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), stale);
}

#[test]
fn obsolete_cache_preserves_bytes_when_model_report_fails() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let current = seed_current_cache(home.path(), workspace.path());
    let stale = stale_capabilities(&current);
    write_cache(home.path(), &stale);

    let stub = Stub::start(Plan {
        installed: vec!["qwen3-cloud".to_string()],
        ..Plan::default()
    });
    let result = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--provider", "ollama", "--refresh", "--json"])
        .run();
    assert_ne!(result.code, Some(0));
    assert!(
        result.stderr.contains("rebuild") || result.stderr.contains("discovery"),
        "model failure must identify the failed recovery: {}",
        result.stderr
    );
    assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), stale);
}

#[test]
fn obsolete_cache_preserves_bytes_when_required_credential_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let current = seed_current_cache(home.path(), workspace.path());
    let stale = stale_kimi_cache(&current);
    write_cache(home.path(), &stale);

    let stub = Stub::start(Plan::default());
    let result = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--provider", "kimi-coding", "--refresh", "--json"])
        .run();
    assert_ne!(result.code, Some(0));
    assert!(
        result.stderr.contains("credential") || result.stderr.contains("auth"),
        "missing required credential must be actionable: {}",
        result.stderr
    );
    assert!(
        stub.requests().is_empty(),
        "credential refusal precedes transport"
    );
    assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), stale);
}

#[test]
fn unfiltered_refresh_skips_optional_missing_credentials_and_reports_unavailable() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let current = seed_current_cache(home.path(), workspace.path());
    let stale = stale_kimi_cache(&current);
    write_cache(home.path(), &stale);

    let stub = Stub::start(Plan::default());
    let result = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--refresh", "--json"])
        .run();
    assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
    let snapshot: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let kimi = snapshot["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["provider"] == "kimi-coding")
        .expect("the optional provider remains visible");
    assert_eq!(kimi["availability"]["status"], "unavailable");
    assert_eq!(kimi["availability"]["cause"], "missing_credential");
}

// Catches: ordinary catalogue reads still require manual refresh after an upgrade.
#[test]
fn ordinary_read_rebuilds_obsolete_cache_without_explicit_refresh() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let current = seed_current_cache(home.path(), workspace.path());
    let stale = stale_capabilities(&current);
    write_cache(home.path(), &stale);

    let stub = Stub::start(Plan::default());
    let result = Ego::at(home.path(), workspace.path(), Some(&stub))
        .subcommand("models")
        .args(["--provider", "ollama", "--json"])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let rebuilt = std::fs::read(cache_path(home.path())).unwrap();
    assert_ne!(rebuilt, stale);
    let cache = DiscoveryCacheV2::decode(&rebuilt).expect("rebuild publishes current schema");
    assert!(cache.providers.contains_key("ollama"));
}

#[test]
fn malformed_and_newer_cache_fail_distinctly_without_refreshing() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let current = seed_current_cache(home.path(), workspace.path());

    write_cache(home.path(), b"not-json");
    let malformed = Ego::at(home.path(), workspace.path(), None)
        .subcommand("models")
        .args(["--provider", "ollama", "--json"])
        .run();
    assert_ne!(malformed.code, Some(0));
    assert!(malformed.stderr.contains("corrupt"), "{}", malformed.stderr);
    assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), b"not-json");

    let newer = newer_cache(&current);
    write_cache(home.path(), &newer);
    let unsupported = Ego::at(home.path(), workspace.path(), None)
        .subcommand("models")
        .args(["--provider", "ollama", "--json"])
        .run();
    assert_ne!(unsupported.code, Some(0));
    assert!(
        unsupported.stderr.contains("unsupported"),
        "{}",
        unsupported.stderr
    );
    assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), newer);
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
    assert_eq!(snapshot["v"], 2);
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
                    "message": "not signed in to the subscription; sign in with 'ego auth login kimi-coding'"
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
schema_version = 6
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

/// A successful subscription login must make its current default selectable
/// without asking the operator to reconstruct model facts in `models.toml`.
#[test]
fn stored_openai_codex_credential_lists_the_builtin_default_on_a_fresh_home() {
    let ego = Ego::nowhere();
    ego.home_file(
        "auth.json",
        r#"{"openai-codex":{"type":"o_auth","access":"head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig"}}"#,
    );

    let listed = ego
        .subcommand("models")
        .args(["--provider", "openai-codex", "--refresh", "--json"])
        .run();
    assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);

    let snapshot: serde_json::Value =
        serde_json::from_str(&listed.stdout).expect("the model report is JSON");
    let model = snapshot["models"]
        .as_array()
        .expect("the report carries models")
        .iter()
        .find(|model| model["slug"] == "openai-codex/gpt-6.1-sol")
        .expect("a fresh Codex subscription home lists its builtin default");
    assert_eq!(model["capabilities"]["max_context_tokens"], 272_000);
    assert_eq!(model["capabilities"]["max_output_tokens"], 128_000);
    assert_eq!(model["metering"], "subscription");
    assert_eq!(model["provenance"]["authority"], "builtin");
    assert_eq!(model["availability"]["status"], "available");
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
            "--sandbox",
            "off",
            "--model",
            "openai/gpt-6-sol",
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

// Catches: OpenRouter discovery is wired to neither the CLI nor the shared picker.
#[test]
fn openrouter_recorded_inventory_reaches_json_and_picker_without_invented_limits() {
    let stub = Stub::start(Plan {
        models_response: Some(include_str!("fixtures/openrouter-models.json").to_owned()),
        ..Plan::default()
    });
    let result = Ego::against(&stub)
        .env("OPENROUTER_API_KEY", "fixture-key")
        .env(
            "OPENROUTER_BASE_URL",
            &format!("http://{}/api/v1/", stub.address),
        )
        .subcommand("models")
        .args(["--provider", "openrouter", "--refresh", "--json"])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let snapshot: ego_ai::catalogue::ModelCatalogueSnapshot =
        serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(
        snapshot.models.len(),
        459,
        "snapshot={}, requests={:?}",
        result.stdout,
        stub.requests()
    );
    let picker = ego_cli::model_picker::ModelPicker::new(std::sync::Arc::new(snapshot));
    let muse = picker
        .entries()
        .iter()
        .find(|entry| entry.slug.to_string() == "openrouter/meta/muse-wonder")
        .unwrap();
    assert_eq!(muse.capabilities.max_context_tokens, Some(1048576));
    assert_eq!(muse.capabilities.max_output_tokens, Some(943718));
    assert_eq!(muse.capabilities.max_tools, None);
    assert!(!result.stdout.contains("pricing"));
    assert!(!result.stdout.contains("rates"));
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/openrouter-models.json")).unwrap();
    for entry in picker.entries() {
        let row = fixture["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"].as_str() == Some(entry.slug.model.as_str()))
            .unwrap();
        assert_eq!(
            entry.capabilities.max_context_tokens,
            row["context_length"].as_u64()
        );
        assert_eq!(
            entry.capabilities.max_output_tokens,
            row["top_provider"]["max_completion_tokens"].as_u64()
        );
        assert_eq!(entry.capabilities.max_tools, None);
    }
    assert_eq!(
        stub.requests()
            .iter()
            .map(|r| r.path.as_str())
            .collect::<Vec<_>>(),
        ["/api/v1/models"]
    );
}

// Catches: an OpenRouter row whose response states no limit is given an invented one.
#[test]
fn openrouter_row_without_stated_limits_lists_no_limit() {
    let stub = Stub::start(Plan {
        models_response: Some(r#"{"data":[{"id":"vendor/bare"}]}"#.to_owned()),
        ..Plan::default()
    });
    let result = Ego::against(&stub)
        .subcommand("models")
        .args(["--provider", "openrouter", "--refresh", "--json"])
        .env("OPENROUTER_API_KEY", "test-key")
        .env(
            "OPENROUTER_BASE_URL",
            &format!("http://{}/api/v1/", stub.address),
        )
        .run();
    assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let models = value["models"].as_array().unwrap();
    assert_eq!(models.len(), 1, "{models:?}");
    let capabilities = &models[0]["capabilities"];
    assert!(
        capabilities["max_context_tokens"].is_null(),
        "{capabilities}"
    );
    assert!(
        capabilities["max_output_tokens"].is_null(),
        "{capabilities}"
    );
    assert!(capabilities["max_tools"].is_null(), "{capabilities}");
}

const OPENROUTER_USER_ROWS: &str = r#"
[model."openrouter/vendor/bare".capabilities]
schema_version = 6
max_context_tokens = 50000
max_output_tokens = 4000
max_tools = 16
supports_parallel_tool_calls = true
reasoning_carryover = "unsupported"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "none"
transport = "sse"

[model."openrouter/vendor/absent".capabilities]
schema_version = 6
max_context_tokens = 50000
max_output_tokens = 4000
max_tools = 16
supports_parallel_tool_calls = true
reasoning_carryover = "unsupported"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "none"
transport = "sse"
"#;

// Catches: user models.toml rows for a dynamically listed provider are ignored, so a listed
// OpenRouter model keeps `unknown` reasoning and a row absent from the listing never appears.
#[test]
fn openrouter_user_rows_replace_discovered_facts_and_add_unlisted_models() {
    let stub = Stub::start(Plan {
        models_response: Some(r#"{"data":[{"id":"vendor/bare"}]}"#.to_owned()),
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .env("OPENROUTER_API_KEY", "test-key")
        .env(
            "OPENROUTER_BASE_URL",
            &format!("http://{}/api/v1/", stub.address),
        );
    ego.home_file("models.toml", OPENROUTER_USER_ROWS);
    let result = ego
        .subcommand("models")
        .args(["--provider", "openrouter", "--refresh", "--json"])
        .run();
    assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let models = value["models"].as_array().unwrap();
    let row = |slug: &str| {
        models
            .iter()
            .find(|model| model["slug"] == slug)
            .unwrap_or_else(|| panic!("{slug} is not listed: {models:?}"))
    };
    let bare = row("openrouter/vendor/bare");
    assert_eq!(bare["capabilities"]["reasoning_efforts"], "any", "{bare}");
    assert_eq!(bare["provenance"]["authority"], "user", "{bare}");
    assert_eq!(models.len(), 2, "{models:?}");
    assert_eq!(
        row("openrouter/vendor/absent")["capabilities"]["max_context_tokens"],
        50000
    );
}

// Catches: a run on a user-described OpenRouter model still stops at "nobody stated whether the
// model accepts `medium` reasoning" because resolution reads only the discovered facts.
#[test]
fn openrouter_run_uses_the_user_row_to_accept_a_thinking_level() {
    let stub = Stub::start(Plan {
        models_response: Some(r#"{"data":[{"id":"vendor/bare"}]}"#.to_owned()),
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .env("OPENROUTER_API_KEY", "test-key")
        .env(
            "OPENROUTER_BASE_URL",
            &format!("http://{}/api/v1/", stub.address),
        );
    ego.home_file("models.toml", OPENROUTER_USER_ROWS);
    let result = ego
        .args([
            "--sandbox",
            "off",
            "--model",
            "openrouter/vendor/bare",
            "--thinking",
            "medium",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "Ask",
        ])
        .run();
    assert!(
        !result.stderr.contains("nobody stated whether"),
        "the user row was not consulted: {}",
        result.stderr
    );
}

// Catches: a vendor-stated alias omitted by snapshot selection, API limits
// copied from GPT-6 Astra, or unverified reasoning/subscription support advertised.
#[test]
fn chat_latest_lists_vendor_limits_without_invented_controls_or_subscription() {
    let listed = Ego::nowhere()
        .subcommand("models")
        .args(["--provider", "openai", "--json"])
        .run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert!(listed.stderr.is_empty(), "{}", listed.stderr);
    let snapshot: serde_json::Value = serde_json::from_str(&listed.stdout).unwrap();
    let model = snapshot["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "openai/chat-latest")
        .expect("the current ChatGPT Instant alias is listed");
    let caps = &model["capabilities"];
    assert_eq!(caps["max_context_tokens"], 400_000);
    assert_eq!(caps["max_output_tokens"], 128_000);
    assert_eq!(
        caps["reasoning_efforts"],
        serde_json::json!({"only": ["medium"]})
    );
    assert_eq!(caps["supports_parallel_tool_calls"], false);
    assert_eq!(caps["image_input"]["kind"], "supported");
    assert_eq!(model["provenance"]["authority"], "builtin");

    let codex = Ego::nowhere()
        .subcommand("models")
        .args(["--provider", "openai-codex", "--json"])
        .run();
    assert_eq!(codex.code, Some(0), "{}", codex.stderr);
    let snapshot: serde_json::Value = serde_json::from_str(&codex.stdout).unwrap();
    assert!(
        !snapshot["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == "openai-codex/chat-latest")
    );
}
