//! RED contract for regression case MCP protocol-facing CLI composition.
//!
//! These assertions are intentionally isolated from regression case: the current
//! branch does not yet expose the Login/Reload actions or `--mcp` attachment.

use ego_cli::cli::{Invocation, McpAction, McpSelector, parse};

#[test]
fn run_keeps_ordered_named_and_file_selectors() {
    let Invocation::Run(args) = parse(
        [
            "ego",
            "run",
            "--mcp",
            "remote",
            "--mcp",
            "@scope.json",
            "prompt",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
    .expect("valid run composition") else {
        panic!("expected run")
    };
    assert_eq!(
        args.mcp,
        vec![
            McpSelector::Named("remote".parse().expect("name")),
            McpSelector::File("scope.json".into())
        ]
    );
}

#[test]
fn final_management_grammar_composes_login_and_reload_as_typed_actions() {
    let login = parse(
        ["ego", "mcp", "login", "remote"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("login is a current MCP management action");
    assert!(
        matches!(login, Invocation::Mcp(args) if matches!(args.action, McpAction::Login { ref name } if name.to_string() == "remote"))
    );

    let reload = parse(
        ["ego", "mcp", "reload"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("reload is a current MCP management action");
    assert!(
        matches!(reload, Invocation::Mcp(args) if matches!(args.action, McpAction::Reload { name: None }))
    );

    let reload_one = parse(
        ["ego", "mcp", "reload", "remote"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("named reload is a current MCP management action");
    assert!(
        matches!(reload_one, Invocation::Mcp(args) if matches!(args.action, McpAction::Reload { name: Some(ref name) } if name.to_string() == "remote"))
    );
}

#[test]
fn run_composes_current_mcp_selector() {
    let invocation = parse(
        ["ego", "run", "--mcp", "remote", "prompt"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("--mcp is a current run composition input");
    let Invocation::Run(args) = invocation else {
        panic!("expected run invocation")
    };
    assert_eq!(args.mcp.len(), 1);
    assert!(matches!(&args.mcp[0], McpSelector::Named(name) if name.to_string() == "remote"));
}
