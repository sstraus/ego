//! Story 883-99be: the owner's `tools` key selects the model tools of every run.

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

// Catches: argv-only search selection is lost on resume, model change, or explicit reselection.
#[test]
fn explicit_search_recipient_survives_resume_and_model_change_with_external_credentials() {
    for selector in ["web_search", "web_search_external"] {
        let mut fixture = plan();
        fixture.installed.push("other".to_owned());
        let stub = Stub::start(fixture);
        let places = Places::new();
        places.ego(&stub).home_file(
            "config.toml",
            "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
        );
        let session = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718293";
        let first = places
            .ego(&stub)
            .platform_sandbox()
            .env("EXA_API_KEY", "owned-unused-external-search-key")
            .args([
                "--offline",
                "--session-id",
                session,
                "--tools",
                selector,
                "Reply READY.",
            ])
            .run();
        assert_eq!(first.code, Some(0), "{selector}: {}", first.stderr);
        let resumed = places
            .ego(&stub)
            .platform_sandbox()
            .env("EXA_API_KEY", "owned-unused-external-search-key")
            .subcommand("resume")
            .args([
                session,
                "--offline",
                "--model",
                "ollama/other",
                "Reply again.",
            ])
            .run();
        assert_eq!(resumed.code, Some(0), "{selector}: {}", resumed.stderr);
        let requests = stub.generations();
        assert_eq!(requests.len(), 2, "{selector}: {requests:?}");
        assert_eq!(requests[1]["model"], "other");
        for request in &requests {
            let names: Vec<_> = request["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tool| tool["function"]["name"].as_str())
                .collect();
            let expected = if selector == "web_search_external" {
                vec!["web_search_external"]
            } else {
                // This local model has no native search. Never offer Exa in its place.
                vec![]
            };
            assert_eq!(names, expected, "{selector}: {request}");
        }
        let overridden = places
            .ego(&stub)
            .platform_sandbox()
            .env("EXA_API_KEY", "owned-unused-external-search-key")
            .subcommand("resume")
            .args([
                session,
                "--offline",
                "--tools",
                "read",
                "Reply with new tools.",
            ])
            .run();
        assert_eq!(overridden.code, Some(0), "{}", overridden.stderr);
        let requests = stub.generations();
        assert_eq!(requests.len(), 3);
        let names: Vec<_> = requests[2]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert_eq!(names, vec!["read"], "{}", requests[2]);
    }
}

// Catches: native search requires an external key, or a missing external backend falls back.
#[test]
fn search_selectors_without_external_credentials_do_not_substitute_recipients() {
    for selector in ["web_search", "web_search_external"] {
        let stub = Stub::start(plan());
        let places = Places::new();
        let run = places
            .ego(&stub)
            .platform_sandbox()
            .args([
                "--offline",
                "--model",
                "ollama/qwen3",
                "--tools",
                selector,
                "Reply READY.",
            ])
            .run();
        if selector == "web_search" {
            assert_eq!(run.code, Some(0), "{}", run.stderr);
            let requests = stub.generations();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].get("tools").is_none(), "{}", requests[0]);
        } else {
            assert_ne!(run.code, Some(0));
            assert!(
                run.stderr.contains("--tools web_search_external"),
                "{}",
                run.stderr
            );
            assert!(run.stderr.contains("EXA_API_KEY"), "{}", run.stderr);
            assert!(stub.generations().is_empty());
        }
    }
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

// Catches: restoring a durable external selector silently substitutes native/default tools.
#[test]
fn restored_external_search_without_its_backend_fails_before_generation() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let session = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718294";
    let first = places
        .ego(&stub)
        .platform_sandbox()
        .env("EXA_API_KEY", "unused-restored-search-fixture-key")
        .args([
            "--offline",
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--session-id",
            session,
            "--tools",
            "web_search_external",
            "Reply READY.",
        ])
        .run();
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert_eq!(stub.generations().len(), 1);
    let resumed = places
        .ego(&stub)
        .platform_sandbox()
        .subcommand("resume")
        .args([session, "--offline", "Reply again."])
        .run();
    assert_ne!(resumed.code, Some(0));
    assert!(
        resumed.stderr.contains("--tools web_search_external"),
        "{}",
        resumed.stderr
    );
    assert!(resumed.stderr.contains("EXA_API_KEY"), "{}", resumed.stderr);
    assert_eq!(stub.generations().len(), 1);
}
