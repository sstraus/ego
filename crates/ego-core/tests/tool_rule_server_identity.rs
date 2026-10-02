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
