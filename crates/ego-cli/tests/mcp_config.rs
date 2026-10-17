//! contract for regression case semantic MCP CLI values.

use std::path::Path;

use ego_cli::cli::{Invocation, McpAction, parse};

fn parse_ok(args: &[&str]) -> Invocation {
    parse(args.iter().map(|s| (*s).to_owned()).collect()).expect("valid MCP argv")
}

#[test]
fn add_payloads_assert_stdio_and_http_every_field() {
    let Invocation::Mcp(args) = parse_ok(&[
        "ego",
        "mcp",
        "add",
        "local",
        "--stdio",
        "--cwd",
        "/tmp",
        "--inherit-env",
        "TOKEN",
        "--inherit-env",
        "HOME",
        "--",
        "srv",
        "--flag",
    ]) else {
        panic!("expected MCP");
    };
    let McpAction::Add { name, server } = args.action else {
        panic!("expected add");
    };
    assert_eq!(name.to_string(), "local");
    let ego_core::config::McpServerConfig::Stdio(stdio) = server else {
        panic!("expected stdio");
    };
    assert_eq!(stdio.command, "srv");
    assert_eq!(stdio.args, vec!["--flag"]);
    assert_eq!(stdio.cwd.as_deref(), Some(Path::new("/tmp")));
    assert_eq!(
        stdio
            .inherit_env
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["TOKEN", "HOME"]
    );

    let Invocation::Mcp(args) = parse_ok(&[
        "ego",
        "mcp",
        "add",
        "remote",
        "--http",
        "http://localhost:8787",
        "--auth",
        "none",
        "--allow-insecure-loopback",
    ]) else {
        panic!("expected MCP");
    };
    let McpAction::Add { name, server } = args.action else {
        panic!("expected add");
    };
    assert_eq!(name.to_string(), "remote");
    let ego_core::config::McpServerConfig::Http(http) = server else {
        panic!("expected HTTP");
    };
    assert_eq!(http.url.to_string(), "http://localhost:8787");
    assert_eq!(http.auth, ego_core::config::McpHttpAuth::None);
    assert!(http.allow_insecure_loopback);

    let Invocation::Mcp(args) = parse_ok(&[
        "ego",
        "mcp",
        "add",
        "secure",
        "--http",
        "https://mcp.example.test/v1",
        "--auth",
        "oauth",
    ]) else {
        panic!("expected MCP");
    };
    let McpAction::Add { server, .. } = args.action else {
        panic!("expected add");
    };
    let ego_core::config::McpServerConfig::Http(secure) = server else {
        panic!("expected HTTP");
    };
    assert_eq!(secure.url.to_string(), "https://mcp.example.test/v1");
    assert_eq!(secure.auth, ego_core::config::McpHttpAuth::Oauth);
    assert!(!secure.allow_insecure_loopback);
}

#[test]
fn stdio_child_arguments_after_literal_separator_are_preserved_exactly() {
    let Invocation::Mcp(args) = parse_ok(&[
        "ego",
        "mcp",
        "add",
        "local",
        "--stdio",
        "--",
        "srv",
        "--unknown",
        "--",
        "-x",
    ]) else {
        panic!("expected MCP");
    };
    let McpAction::Add { server, .. } = args.action else {
        panic!("expected add");
    };
    let ego_core::config::McpServerConfig::Stdio(stdio) = server else {
        panic!("expected stdio");
    };
    assert_eq!(stdio.command, "srv");
    assert_eq!(stdio.args, vec!["--unknown", "--", "-x"]);
}

#[test]
fn every_management_action_has_exact_typed_payload() {
    let Invocation::Mcp(args) = parse_ok(&["ego", "mcp", "rm", "remote"]) else {
        panic!("MCP");
    };
    assert!(matches!(args.action, McpAction::Remove { name } if name.to_string() == "remote"));
    let Invocation::Mcp(args) = parse_ok(&["ego", "mcp", "ls"]) else {
        panic!("MCP");
    };
    assert!(matches!(args.action, McpAction::List { json: false }));
    let Invocation::Mcp(args) = parse_ok(&["ego", "mcp", "ls", "--json"]) else {
        panic!("MCP");
    };
    assert!(matches!(args.action, McpAction::List { json: true }));
}

#[test]
fn invalid_at_names_missing_separator_mixed_options_applicability_loopback_aliases_and_extras_reject()
 {
    let invalid: &[&[&str]] = &[
        &["ego", "run", "--mcp", "@"],
        &["ego", "mcp", "add", "Bad", "--http", "https://x.test"],
        &["ego", "mcp", "add", "x", "--stdio", "srv"],
        &[
            "ego",
            "mcp",
            "add",
            "x",
            "--stdio",
            "--http",
            "https://x.test",
        ],
        &[
            "ego",
            "mcp",
            "add",
            "x",
            "--http",
            "https://x.test",
            "--cwd",
            "/tmp",
        ],
        &[
            "ego",
            "mcp",
            "add",
            "x",
            "--http",
            "http://example.test",
            "--allow-insecure-loopback",
        ],
        &[
            "ego",
            "mcp",
            "add",
            "x",
            "--http",
            "https://x.test",
            "--allow-insecure-loopback",
        ],
        &["ego", "mcp", "list"],
        &["ego", "mcp", "add", "x", "--command", "srv"],
        &[
            "ego",
            "mcp",
            "add",
            "x",
            "--stdio",
            "--inherit-env",
            "TOKEN",
            "--",
        ],
    ];
    for argv in invalid {
        assert!(
            parse(argv.iter().map(|s| (*s).to_owned()).collect()).is_err(),
            "accepted invalid argv: {argv:?}"
        );
    }
}

#[test]
fn add_legacy_sse_keeps_explicit_profile_in_the_typed_payload() {
    let Invocation::Mcp(args) = parse_ok(&[
        "ego",
        "mcp",
        "add",
        "events",
        "--http",
        "https://example.test/sse",
        "--profile",
        "legacy-sse",
    ]) else {
        panic!("MCP");
    };
    let McpAction::Add {
        server: ego_core::config::McpServerConfig::Http(http),
        ..
    } = args.action
    else {
        panic!("HTTP");
    };
    assert_eq!(http.profile, ego_core::config::McpHttpProfile::LegacySse);
    assert!(
        parse(
            [
                "ego",
                "mcp",
                "add",
                "events",
                "--http",
                "https://example.test/sse",
                "--profile",
                "auto"
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        )
        .is_err()
    );
}
