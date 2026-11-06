//! Adversarial cases for `tool(<server>/*)` (regression).

use std::path::Path;

use ego_core::config::{ConfigLayer, Mode, Ruleset, Source, Trust, resolve};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Classification, ForeignToolId, ForeignTransportTarget, HttpHeader, HttpRequest,
    HttpRoute, HttpScheme, InvocationDigest, NetworkTarget, Op, PolicyEngine, PreparedCall,
};

const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";

fn digest() -> InvocationDigest {
    InvocationDigest::parse(DIGEST).expect("digest")
}

fn tool(server: &str, name: &str) -> ForeignToolId {
    ForeignToolId::new(server, name).expect("identity")
}

fn route() -> HttpRoute {
    HttpRoute::new(
        HttpScheme::Https,
        NetworkTarget::parse("api.example.test:443").expect("target"),
    )
    .expect("route")
}

fn opaque(server: &str, name: &str) -> Op {
    Op::Opaque {
        tool: tool(server, name),
        params_digest: digest(),
        transport: ForeignTransportTarget::http(route()),
    }
}

fn request() -> HttpRequest {
    HttpRequest::new(
        "GET",
        "/",
        vec![HttpHeader::new("x-trace", b"one".to_vec()).expect("header")],
        b"body".to_vec(),
    )
    .expect("request")
}

fn http_call(server: &str, name: &str) -> PreparedCall {
    PreparedCall::foreign_http(
        tool(server, name),
        digest(),
        ForeignTransportTarget::http(route()),
        &request(),
    )
}

/// An engine whose rules came from the user layer, where a wildcard is legal.
fn user_engine(mode: Mode, allow: &[&str], deny: &[&str]) -> PolicyEngine {
    let render = |values: &[&str]| {
        values
            .iter()
            .map(|value| format!("{value:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let permissions = ConfigLayer::parse(
        Source::User,
        &format!(
            "[permissions]\nallow=[{}]\ndeny=[{}]",
            render(allow),
            render(deny)
        ),
    )
    .expect("user permission fixture")
    .value
    .permissions
    .expect("permissions");
    PolicyEngine::new(
        CanonPath::new("/workspace"),
        mode,
        Ruleset::from_layer(permissions, Source::User),
    )
}

fn settled_ok(class: Classification) -> bool {
    matches!(class, Classification::Settled(Ok(_)))
}

fn settled_err(class: Classification) -> bool {
    matches!(class, Classification::Settled(Err(_)))
}

const NON_OPERATOR: [Source; 5] = [
    Source::Project,
    Source::Local,
    Source::Env,
    Source::AgentFile,
    Source::Flags,
];

/// Catches: the load-time gate checking only `[permissions].allow` of the base
/// layer, so a repository smuggles `tool(srv/*)` through `deny`, a profile's
/// allow/deny, or padding around the `*`; or the gate keyed on `Project` alone
/// so Local/Env/AgentFile/Flags slip through.
#[test]
fn a_wildcard_is_rejected_from_every_non_operator_source_in_every_position() {
    let bodies = [
        "[permissions]\nallow=[\"tool(srv/*)\"]",
        "[permissions]\ndeny=[\"tool(srv/*)\"]",
        "[permissions]\nallow=[\"exec(ls)\", \"tool(srv/a)\", \"tool(srv/*)\"]",
        "[permissions]\nallow=[\"tool( srv / * )\"]",
        "[profile.p.permissions]\nallow=[\"tool(srv/*)\"]",
        "[profile.p.permissions]\ndeny=[\"tool(srv/*)\"]",
    ];
    for source in NON_OPERATOR {
        for body in bodies {
            let err = ConfigLayer::parse(source, body)
                .err()
                .unwrap_or_else(|| panic!("{source:?} accepted a wildcard in: {body}"));
            assert!(
                err.to_string().contains("system and user"),
                "{source:?}/{body}: rejected for the wrong reason: {err}"
            );
        }
    }
}

/// Catches: the gate rejecting every `tool(...)` rule from a repository (an
/// exact tool name must stay legal there) or rejecting any rule containing `*`
/// (partial globs are plain exact names that match nothing wider).
#[test]
fn exact_and_partial_names_stay_legal_in_a_repository_layer_and_match_nothing_wider() {
    for source in NON_OPERATOR {
        for raw in ["tool(srv/session)", "tool(srv/se*)", "tool(srv/**)"] {
            ConfigLayer::parse(source, &format!("[permissions]\nallow=[{raw:?}]"))
                .unwrap_or_else(|err| panic!("{source:?} rejected {raw}: {err}"));
        }
    }
    let root = Path::new("/work");
    for raw in ["tool(srv/se*)", "tool(srv/**)", "tool(*/session)"] {
        let rule = Rule::load(raw).expect("rule");
        assert!(!rule.is_tool_wildcard(), "{raw} counted as the wildcard");
        assert!(
            !rule.matches(&opaque("srv", "session"), root),
            "{raw} acted as a glob over tool names"
        );
        assert!(
            !rule.matches(&opaque("evil", "session"), root),
            "{raw} acted as a glob over server names"
        );
    }
}

/// Catches: `tool(*/*)` or a server glob widening the wildcard past one
/// server, and the wildcard approving a non-foreign operation or a server that
/// merely shares a prefix.
#[test]
fn the_wildcard_has_no_second_wildcard_and_covers_only_foreign_tool_calls() {
    let root = Path::new("/work");
    let any = Rule::load("tool(*/*)").expect("rule");
    for (server, name) in [("evil", "x"), ("tuicommander", "session")] {
        assert!(
            !any.matches(&opaque(server, name), root),
            "tool(*/*) approved {server}/{name}"
        );
    }
    let rule = Rule::load("tool(tuic/*)").expect("rule");
    assert!(rule.matches(&opaque("tuic", "*"), root));
    assert!(!rule.matches(&opaque("tuic2", "a"), root));
    assert!(!rule.matches(&opaque("tui", "a"), root));
    assert!(!rule.matches(&opaque("TUIC", "a"), root));
    assert!(
        !rule.matches(
            &Op::Read {
                path: CanonPath::new("/work/tuic/a")
            },
            root
        ),
        "a tool rule approved a builtin read"
    );
}

/// Catches: criterion 3 — a wildcard allow overriding a specific deny, and a
/// wildcard deny failing to beat a specific allow, in the real engine rather
/// than in `Rule::matches`.
#[test]
fn a_deny_beats_a_wildcard_allow_and_a_wildcard_deny_beats_an_exact_allow() {
    let allowed = user_engine(Mode::Default, &["tool(srv/*)"], &[]);
    assert!(settled_ok(allowed.classify(http_call("srv", "agent"))));
    assert!(
        matches!(
            allowed.classify(http_call("other", "agent")),
            Classification::Ask(_)
        ),
        "the wildcard settled another server's call"
    );

    let exact_deny = user_engine(Mode::Default, &["tool(srv/*)"], &["tool(srv/agent)"]);
    assert!(settled_err(exact_deny.classify(http_call("srv", "agent"))));
    assert!(settled_ok(exact_deny.classify(http_call("srv", "session"))));

    let wildcard_deny = user_engine(Mode::Default, &["tool(srv/agent)"], &["tool(srv/*)"]);
    assert!(settled_err(
        wildcard_deny.classify(http_call("srv", "agent"))
    ));

    let yolo = user_engine(Mode::Yolo, &["tool(srv/*)"], &["tool(srv/agent)"]);
    assert!(settled_err(yolo.classify(http_call("srv", "agent"))));
}

/// Catches: a wildcard on the inner server settling a collapsed dispatch whose
/// outer router tool was never allowed.
#[test]
fn a_wildcard_for_the_inner_server_does_not_settle_the_outer_dispatch_tool() {
    let engine = user_engine(Mode::Default, &["tool(srv/*)"], &[]);
    let outer = PreparedCall::foreign_dispatch(
        tool("router", "call_tool"),
        digest(),
        tool("srv", "search"),
        digest(),
        ForeignTransportTarget::http(route()),
        &request(),
    );
    assert!(matches!(engine.classify(outer), Classification::Ask(_)));
}

/// Catches: a project replacing the user server while inheriting its wildcard grant.
#[test]
fn a_project_redefinition_does_not_inherit_the_user_server_wildcard() {
    let user = ConfigLayer::parse(
        Source::User,
        "[permissions]\nallow=['tool(srv/*)']\n[mcp.servers.srv]\ntransport='stdio'\ncommand='operator-server'",
    ).expect("user");
    let project = ConfigLayer::parse(
        Source::Project,
        "[mcp.servers.srv]\ntransport='stdio'\ncommand='./project-server'",
    )
    .expect("project definitions remain legal");
    let effective = resolve(&[user, project], Trust::Trusted, None).expect("resolve");
    let engine = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        effective.permissions,
    );
    assert!(matches!(
        engine.classify(http_call("srv", "new_tool")),
        Classification::Ask(_)
    ));
}

/// Catches: missing, removed or non-operator servers keeping a wildcard grant;
/// profile or trust handling disabling a legitimate operator server grant.
#[test]
fn wildcard_grants_follow_the_effective_server_authority() {
    let definition = "[mcp.servers.srv]\ntransport='stdio'\ncommand='server'";
    let profile_definition = "[profile.p.mcp.servers.srv]\ntransport='stdio'\ncommand='server'";
    let cases = [
        (Source::User, definition, None, Trust::Trusted, true),
        (
            Source::User,
            profile_definition,
            Some("p"),
            Trust::Trusted,
            true,
        ),
        (
            Source::System,
            profile_definition,
            Some("p"),
            Trust::Trusted,
            true,
        ),
        (Source::System, definition, None, Trust::Trusted, false),
        (Source::User, "", None, Trust::Trusted, false),
        (
            Source::User,
            "[mcp.servers.srv]\nremoved=true",
            None,
            Trust::Trusted,
            false,
        ),
        (Source::Project, definition, None, Trust::Trusted, false),
        (Source::Local, definition, None, Trust::Trusted, false),
        (
            Source::Project,
            profile_definition,
            Some("p"),
            Trust::Trusted,
            false,
        ),
        (
            Source::Local,
            profile_definition,
            Some("p"),
            Trust::Trusted,
            false,
        ),
        (Source::Project, definition, None, Trust::Untrusted, true),
        (
            Source::User,
            profile_definition,
            None,
            Trust::Trusted,
            false,
        ),
    ];
    for (source, body, profile, trust, allowed) in cases {
        let user = ConfigLayer::parse(Source::User,
            "[permissions]\nallow=['tool(srv/*)']\n[mcp.servers.srv]\ntransport='stdio'\ncommand='operator-server'")
            .expect("user");
        let layers = if source == Source::User {
            let permissions = "[permissions]\nallow=['tool(srv/*)']\n";
            vec![
                ConfigLayer::parse(source, &format!("{permissions}{body}"))
                    .expect("user definition"),
            ]
        } else if source == Source::System {
            vec![
                ConfigLayer::parse(Source::User, "[permissions]\nallow=['tool(srv/*)']")
                    .expect("permissions"),
                ConfigLayer::parse(source, body).expect("system"),
            ]
        } else {
            vec![user, ConfigLayer::parse(source, body).expect("replacement")]
        };
        let effective = resolve(&layers, trust, profile).expect("resolve");
        let engine = PolicyEngine::new(
            CanonPath::new("/workspace"),
            Mode::Default,
            effective.permissions,
        );
        let classification = engine.classify(http_call("srv", "new_tool"));
        if allowed {
            assert!(
                settled_ok(classification),
                "{source:?}, {profile:?}, {trust:?}: {body}"
            );
        } else {
            assert!(
                matches!(classification, Classification::Ask(_)),
                "{source:?}, {profile:?}, {trust:?}: {body}"
            );
        }
    }
}

/// Catches: filtering inactive wildcard grants also drops exact grants or denies.
#[test]
fn project_redefinition_preserves_exact_grants_and_wildcard_denies() {
    let user = ConfigLayer::parse(Source::User,
        "[permissions]\nallow=['tool(srv/*)', 'tool(srv/session)']\ndeny=['tool(srv/agent)', 'tool(blocked/*)']\n[mcp.servers.srv]\ntransport='stdio'\ncommand='operator-server'").expect("user");
    let project = ConfigLayer::parse(
        Source::Project,
        "[mcp.servers.srv]\ntransport='stdio'\ncommand='./project-server'",
    )
    .expect("project");
    let effective = resolve(&[user, project], Trust::Trusted, None).expect("resolve");
    let engine = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        effective.permissions,
    );
    assert!(settled_ok(engine.classify(http_call("srv", "session"))));
    assert!(settled_err(engine.classify(http_call("srv", "agent"))));
    assert!(settled_err(
        engine.classify(http_call("blocked", "new_tool"))
    ));
    assert!(matches!(
        engine.classify(http_call("srv", "new_tool")),
        Classification::Ask(_)
    ));
}

/// Catches: a selected user profile's wildcard grant failing to reach the engine.
#[test]
fn a_user_profile_grants_its_server_wildcard_with_deny_priority() {
    let user = ConfigLayer::parse(Source::User,
        "[profile.p.permissions]\nallow=['tool(srv/*)']\ndeny=['tool(srv/agent)']\n[profile.p.mcp.servers.srv]\ntransport='stdio'\ncommand='operator-server'").expect("user profile");
    let effective = resolve(&[user], Trust::Trusted, Some("p")).expect("resolve");
    let engine = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        effective.permissions,
    );
    assert!(settled_ok(engine.classify(http_call("srv", "new_tool"))));
    assert!(settled_err(engine.classify(http_call("srv", "agent"))));
    assert!(matches!(
        engine.classify(http_call("other", "new_tool")),
        Classification::Ask(_)
    ));
}
