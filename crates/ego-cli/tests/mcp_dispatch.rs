//! contract for final MCP management dispatch.
//!
//! Management commands run through the real `ego` binary. Protocol discovery
//! and OAuth belong to the later `ego-mcp` spine, not this configuration story.

mod support;

use std::fs;

use support::Places;

#[test]
fn add_and_remove_use_atomic_comment_preserving_publication_and_do_not_claim_admission() {
    let places = Places::new();
    let stub = support::Stub::start(support::Plan::default());
    let config = places.ego(&stub).home_file(
        "config.toml",
        "# preserve this comment\nmodel = 'old'\n\n[mcp.servers.unrelated]\ntransport = 'stdio'\ncommand = 'keep'\n",
    );

    let added = places
        .ego(&stub)
        .subcommand("mcp")
        .args(["add", "managed", "--stdio", "--", "mcp-server", "--safe"])
        .run();
    assert_eq!(added.code, Some(0), "stderr: {}", added.stderr);
    assert!(
        added.stdout.contains("published"),
        "stdout: {}",
        added.stdout
    );
    assert!(
        !added.stdout.contains("admitted: true"),
        "stdout: {}",
        added.stdout
    );
    let after_add = fs::read_to_string(&config).expect("config after add");
    assert!(after_add.starts_with("# preserve this comment\nmodel = 'old'\n"));
    assert!(after_add.contains("[mcp.servers.unrelated]"));
    assert!(after_add.contains("[mcp.servers.managed]"));
    assert!(
        after_add.contains("command = 'mcp-server'")
            || after_add.contains("command = \"mcp-server\"")
    );

    let removed = places
        .ego(&stub)
        .subcommand("mcp")
        .args(["rm", "managed"])
        .run();
    assert_eq!(removed.code, Some(0), "stderr: {}", removed.stderr);
    assert!(
        removed.stdout.contains("published"),
        "stdout: {}",
        removed.stdout
    );
    assert!(
        !removed.stdout.contains("admitted: true"),
        "stdout: {}",
        removed.stdout
    );
    let after_remove = fs::read_to_string(&config).expect("config after remove");
    assert!(after_remove.starts_with("# preserve this comment\nmodel = 'old'\n"));
    assert!(after_remove.contains("[mcp.servers.unrelated]"));
    assert!(!after_remove.contains("[mcp.servers.managed]"));
}

#[test]
fn list_dispatch_distinguishes_configured_from_admitted_state() {
    let places = Places::new();
    let stub = support::Stub::start(support::Plan::default());
    let _config = places.ego(&stub).home_file(
        "config.toml",
        "[mcp.servers.remote]\ntransport = 'http'\nurl = 'https://mcp.example.test/mcp'\nauth = 'oauth'\n",
    );
    let listed = places
        .ego(&stub)
        .subcommand("mcp")
        .args(["ls", "--json"])
        .run();
    assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);
    let report: serde_json::Value =
        serde_json::from_str(listed.stdout.trim()).expect("mcp ls --json emits one machine object");
    let remote = report
        .get("servers")
        .and_then(serde_json::Value::as_array)
        .and_then(|servers| servers.iter().find(|server| server["name"] == "remote"))
        .expect("remote appears in configured listing");
    assert_eq!(remote["configured"], true);
    assert_eq!(remote["admitted"], false);
    assert_eq!(remote["health"], "absent");
    assert_eq!(remote["diagnostic"]["class"], "authority");
    assert!(remote.get("authorization").is_none());
    assert_eq!(remote["provenance"]["source"], "user");
    assert_eq!(remote["provenance"]["slot"], "base");
    assert_eq!(remote["configuration"]["transport"], "http");
    assert_eq!(
        remote["configuration"]["url"],
        "https://mcp.example.test/mcp"
    );
    assert_eq!(remote["configuration"]["auth"], "oauth");
}

#[test]
fn list_rejects_legacy_secret_fields_without_echoing_secret_material() {
    let places = Places::new();
    let stub = support::Stub::start(support::Plan::default());
    let secret = "legacy-secret-must-not-escape";
    places.ego(&stub).home_file(
        "config.toml",
        &format!(
            "[mcp.servers.remote]\ntransport = 'http'\nurl = 'https://mcp.example.test/mcp'\nauth = 'none'\napi_key = '{secret}'\n"
        ),
    );
    let listed = places
        .ego(&stub)
        .subcommand("mcp")
        .args(["ls", "--json"])
        .run();
    assert_ne!(listed.code, Some(0), "legacy MCP field was accepted");
    assert!(!listed.stdout.contains(secret), "secret reached stdout");
    assert!(!listed.stderr.contains(secret), "secret reached stderr");
}
