//! Wildcard grants follow only eligible, active MCP definitions.

use ego_core::config::{ConfigLayer, Mode, Source, Trust, resolve};
use ego_core::permit::{
    CanonPath, Classification, ForeignToolId, ForeignTransportTarget, HttpRequest, HttpRoute, HttpScheme, InvocationDigest, NetworkTarget, PolicyEngine, PreparedCall,
};

fn session_call() -> PreparedCall {
    let route = HttpRoute::new(
        HttpScheme::Https,
        NetworkTarget::parse("api.example.test:443").expect("target"),
    ).expect("route");
    PreparedCall::foreign_http(
        ForeignToolId::new("tuicommander", "session").expect("identity"),
        InvocationDigest::parse(
            "sha256:0000000000000000000000000000000000000000000000000000000000000001",
        ).expect("digest"),
        ForeignTransportTarget::http(route),
        &HttpRequest::new("GET", "/", vec![], vec![]).expect("request"),
    )
}

const USER: &str = r#"
[permissions]
allow = ["tool(tuicommander/*)"]
[mcp.servers.tuicommander]
transport = "stdio"
command = "operator-server"
"#;

// Catches: an inactive project profile is treated as the winning server definition.
#[test]
fn an_inactive_project_profile_does_not_remove_the_user_wildcard() {
    let user = ConfigLayer::parse(Source::User, USER).expect("user");
    let project = ConfigLayer::parse(
        Source::Project,
        r#"
[profile.other.mcp.servers.tuicommander]
transport = "stdio"
command = "repository-server"
"#,
    )
    .expect("project");
    let effective = resolve(&[project, user], Trust::Trusted, None).expect("resolve");
    let engine = PolicyEngine::new(
        CanonPath::new("/work"),
        Mode::Default,
        effective.permissions,
    );
    assert!(matches!(
        engine.classify(session_call()),
        Classification::Settled(Ok(_))
    ));
}
