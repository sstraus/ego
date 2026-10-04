//! A remembered `tool(<server>/<name>)` rule names one server's tool.

use std::path::Path;

use ego_core::permit::rule::Rule;
use ego_core::permit::{
    ForeignToolId, ForeignTransportTarget, HttpRoute, HttpScheme, InvocationDigest, NetworkTarget,
    Op,
};

fn opaque(server: &str, name: &str) -> Op {
    Op::Opaque {
        tool: ForeignToolId::new(server, name).expect("identity"),
        params_digest: InvocationDigest::parse(
            "sha256:0000000000000000000000000000000000000000000000000000000000000001",
        )
        .expect("digest"),
        transport: ForeignTransportTarget::http(
            HttpRoute::new(
                HttpScheme::Https,
                NetworkTarget::parse("api.example.test:443").expect("target"),
            )
            .expect("route"),
        ),
    }
}

/// Catches: a remembered rule for one MCP server's `search_tools` also approves
/// another server's tool of the same name (or a lookalike name or server), so a
/// "always allow" for a trusted server approves an untrusted one.
#[test]
fn a_tool_rule_matches_only_its_own_server_and_exact_tool_name() {
    let rule = Rule::load("tool(tuicommander/search_tools)").expect("rule");
    let root = Path::new("/work");
    assert!(rule.matches(&opaque("tuicommander", "search_tools"), root));
    for (server, name) in [
        ("evil", "search_tools"),
        ("tuicommander2", "search_tools"),
        ("tuic", "search_tools"),
        ("TUICommander", "search_tools"),
        ("tuicommander", "search_tools2"),
        ("tuicommander", "search"),
        ("tuicommander", "Search_Tools"),
    ] {
        assert!(
            !rule.matches(&opaque(server, name), root),
            "tool({server}/{name}) was approved by a rule for tuicommander/search_tools"
        );
    }
}

/// Catches: `tool(tuicommander/*)` approving another server's tool, or a
/// lookalike server, because the wildcard was matched across servers.
#[test]
fn a_tool_wildcard_covers_every_tool_of_its_own_server_only() {
    let rule = Rule::load("tool(tuicommander/*)").expect("rule");
    let root = Path::new("/work");
    assert!(rule.is_tool_wildcard());
    assert!(rule.matches(&opaque("tuicommander", "session"), root));
    assert!(rule.matches(&opaque("tuicommander", "agent"), root));
    for server in ["otherserver", "tuicommander2", "TUICommander"] {
        assert!(
            !rule.matches(&opaque(server, "session"), root),
            "tool({server}/session) was approved by tool(tuicommander/*)"
        );
    }
    assert!(
        !Rule::load("tool(tuicommander/session)")
            .unwrap()
            .is_tool_wildcard()
    );
}
