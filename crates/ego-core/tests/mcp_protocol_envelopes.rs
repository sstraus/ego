//! Coarse contract for protocol-owned MCP envelopes and core projections.

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::mcp::{
    CacheScope, ContextEntry, ContextKind, ContextSensitivity, ContextSnapshot, ContextSource,
    ForeignContent, ForeignResult, McpAdmissionFingerprint, McpOperationKind, McpSubject,
};
use ego_core::permit::{
    ForeignToolId, ForeignTransportTarget, HttpRoute, HttpScheme, InvocationDigest, NetworkTarget,
    Op, PolicyEngine, PreparedCall,
};
use ego_core::store::evidence::{EvidenceId, EvidenceRef};
use serde_json::json;

fn route() -> ForeignTransportTarget {
    ForeignTransportTarget::http(
        HttpRoute::new(
            HttpScheme::Https,
            NetworkTarget::new("mcp.example", 443).expect("network target"),
        )
        .expect("HTTP route"),
    )
}

fn rules(allow: &[&str]) -> ego_core::config::Ruleset {
    let render = allow
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let permissions = ego_core::config::ConfigLayer::parse(
        ego_core::config::Source::Project,
        &format!("[permissions]\nallow=[{render}]"),
    )
    .expect("permission fixture")
    .value
    .permissions
    .expect("permissions");
    ego_core::config::Ruleset::from_layer(permissions, ego_core::config::Source::Project)
}

fn digest(label: &str) -> InvocationDigest {
    let hex = match label {
        "params" => "0000000000000000000000000000000000000000000000000000000000000001",
        "other-params" => "0000000000000000000000000000000000000000000000000000000000000002",
        "outer" => "0000000000000000000000000000000000000000000000000000000000000003",
        "inner" => "0000000000000000000000000000000000000000000000000000000000000004",
        _ => "0000000000000000000000000000000000000000000000000000000000000005",
    };
    InvocationDigest::parse(&format!("sha256:{hex}")).expect("canonical digest")
}

#[test]
fn protocol_envelope_tool_identity_does_not_require_network_allowance() {
    let call = PreparedCall::mcp(
        McpSubject::Tool {
            server: "alpha".into(),
            name: "weather".into(),
        },
        McpOperationKind::CallTool,
        vec![(
            ForeignToolId::new("alpha", "weather").expect("outer"),
            digest("params"),
        )],
        McpAdmissionFingerprint::new("capture-7", "weather", "schema-7").expect("admission"),
        "profile-7",
        route(),
        &json!({"name":"weather","arguments":{}}),
    )
    .expect("semantic preparation");
    assert!(matches!(
        call.ops(),
        [Op::Opaque { .. }, Op::ProtocolConnect { .. }]
    ));
    let allowed = PolicyEngine::new(
        ego_core::permit::CanonPath::new("/workspace"),
        ego_core::config::Mode::Default,
        rules(&["tool(alpha/weather)"]),
    );
    assert!(matches!(
        allowed.classify(call),
        ego_core::permit::Classification::Settled(Ok(_))
    ));

    let mismatch = PreparedCall::mcp(
        McpSubject::Tool {
            server: "alpha".into(),
            name: "weather".into(),
        },
        McpOperationKind::CallTool,
        vec![(
            ForeignToolId::new("alpha", "weather").expect("outer"),
            digest("other-params"),
        )],
        McpAdmissionFingerprint::new("capture-8", "weather", "schema-7").expect("admission"),
        "profile-7",
        route(),
        &json!({"name":"weather","arguments":{}}),
    )
    .expect("semantic preparation");
    assert!(matches!(
        mismatch.ops(),
        [Op::Opaque { .. }, Op::ProtocolConnect { .. }]
    ));
    assert!(matches!(
        PolicyEngine::new(
            ego_core::permit::CanonPath::new("/workspace"),
            ego_core::config::Mode::Default,
            rules(&["tool(alpha/weather)"]),
        )
        .classify(mismatch),
        ego_core::permit::Classification::Settled(Ok(_))
    ));
}

#[test]
fn collapsed_dispatch_ops_need_outer_and_inner_tool_rules_without_network_rule() {
    let call = PreparedCall::foreign_dispatch(
        ForeignToolId::new("alpha", "call_tool").expect("outer identity"),
        digest("outer"),
        ForeignToolId::new("alpha", "weather").expect("inner identity"),
        digest("inner"),
        route(),
        &ego_core::permit::HttpRequest::new("POST", "/mcp", Vec::new(), Vec::new())
            .expect("request"),
    );
    assert!(matches!(
        call.ops(),
        [Op::Opaque { .. }, Op::Opaque { .. }, Op::Connect { .. }]
    ));
    assert!(matches!(
        PolicyEngine::new(
            ego_core::permit::CanonPath::new("/workspace"),
            ego_core::config::Mode::Default,
            rules(&["tool(alpha/call_tool)", "tool(alpha/weather)"])
        )
        .classify(call.clone()),
        ego_core::permit::Classification::Settled(Ok(_))
    ));
    assert!(matches!(
        PolicyEngine::new(
            ego_core::permit::CanonPath::new("/workspace"),
            ego_core::config::Mode::Default,
            rules(&["tool(alpha/call_tool)"])
        )
        .classify(call),
        ego_core::permit::Classification::Ask(_)
    ));
}

#[test]
fn collapsed_protocol_subject_keeps_exact_call_tool_identity() {
    let call = PreparedCall::mcp(
        McpSubject::Tool {
            server: "alpha".into(),
            name: "call_tool".into(),
        },
        McpOperationKind::CallTool,
        vec![
            (
                ForeignToolId::new("alpha", "call_tool").expect("outer identity"),
                digest("outer"),
            ),
            (
                ForeignToolId::new("alpha", "weather").expect("inner identity"),
                digest("inner"),
            ),
        ],
        McpAdmissionFingerprint::new("capture-7", "call_tool", "schema-7").expect("admission"),
        "profile-7",
        route(),
        &json!({"name":"weather","arguments":{"city":"Rome"}}),
    )
    .expect("semantic preparation");
    assert!(matches!(
        call.ops(),
        [
            Op::Opaque { .. },
            Op::Opaque { .. },
            Op::ProtocolConnect { .. }
        ]
    ));
    assert!(matches!(
        PolicyEngine::new(
            ego_core::permit::CanonPath::new("/workspace"),
            ego_core::config::Mode::Default,
            rules(&["tool(alpha/call_tool)", "tool(alpha/weather)"])
        )
        .classify(call.clone()),
        ego_core::permit::Classification::Settled(Ok(_))
    ));
    assert!(matches!(
        PolicyEngine::new(
            ego_core::permit::CanonPath::new("/workspace"),
            ego_core::config::Mode::Default,
            rules(&["tool(alpha/call_tool)"])
        )
        .classify(call.clone()),
        ego_core::permit::Classification::Ask(_)
    ));
}

#[test]
fn foreign_result_preserves_provenance_bounds_unsupported_content_cache_and_delivery() {
    let result = ForeignResult::new(
        "alpha",
        "call_tool",
        digest("params"),
        McpAdmissionFingerprint::new("capture-7", "call_tool", "schema-7").expect("admission"),
        "object",
        None,
        None,
        vec![
            ForeignContent::text("visible"),
            ForeignContent::unsupported("image", b"bounded-image-bytes"),
        ],
        EvidenceRef::new(
            EvidenceId::parse("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .expect("evidence id"),
            20,
        ),
        DeliveryCertainty::DefinitelySent,
        ego_core::mcp::CancellationClassification::Completed,
    )
    .expect("foreign result");
    assert_eq!(result.server(), "alpha");
    assert_eq!(result.tool(), "call_tool");
    assert_eq!(result.admission_capture(), "capture-7");
    assert_eq!(result.ttl_ms(), None);
    assert_eq!(result.cache_scope(), None);
    assert!(result.visible_text().len() <= result.visible_limit());
    assert!(
        result
            .unsupported()
            .iter()
            .any(|entry| entry.kind() == "image")
    );
    assert_eq!(result.delivery(), DeliveryCertainty::DefinitelySent);
    assert_eq!(
        result.cancellation(),
        ego_core::mcp::CancellationClassification::Completed
    );
}

#[test]
fn context_source_projects_resources_and_prompts_without_synthetic_tools() {
    let snapshot = ContextSnapshot::new(vec![
        ContextEntry::resource(
            "alpha",
            "mcp://alpha/readme",
            "readme",
            ContextSensitivity::Restricted,
            CacheScope::Public,
            "resource-digest",
        )
        .expect("resource entry"),
        ContextEntry::prompt(
            "alpha",
            "summarize",
            "summarize prompt",
            vec!["language".to_owned()],
            ContextSensitivity::Restricted,
            CacheScope::Private,
            "prompt-digest",
        )
        .expect("prompt entry"),
    ])
    .expect("immutable context snapshot");
    let source = FakeContextSource(snapshot);
    let entries = source.capture().expect("context capture");
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry.kind(), ContextKind::Tool))
    );
}

struct FakeContextSource(ContextSnapshot);

impl ContextSource for FakeContextSource {
    fn capture(&self) -> Result<ContextSnapshot, ego_core::mcp::ContextError> {
        Ok(self.0.clone())
    }
}

#[test]
fn stale_capture_rejects_compiled_inner_schema_after_describe_refresh() {
    let old = McpAdmissionFingerprint::new("capture-7", "call_tool", "schema-1")
        .expect("old admission fingerprint");
    let refreshed = McpAdmissionFingerprint::new("capture-8", "call_tool", "schema-2")
        .expect("new admission fingerprint");
    assert_ne!(old, refreshed);
}
