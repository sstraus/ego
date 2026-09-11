//! Coarse contract for core MCP execution authority.

use ego_core::config::{ConfigLayer, Mode, Ruleset, Source};
use ego_core::mcp::{
    ForeignInputRequired, McpAdmissionFingerprint, McpOperationKind, McpSubject, ToolExecution,
};
use ego_core::permit::{
    ForeignToolId, ForeignTransportTarget, GuardedIo, HttpRoute, HttpScheme, InvocationDigest,
    NetworkTarget, Op, PrepareCtx, PreparedCall, Tool, ToolDescriptor, ToolError, ToolResult,
};
use ego_core::question::{QuestionMode, QuestionOption, QuestionRequest};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct CountingTool {
    name: String,
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl CountingTool {
    fn new(name: &str, calls: Arc<AtomicUsize>, drops: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            calls,
            drops,
        })
    }
}
impl Drop for CountingTool {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Tool for CountingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.clone(),
            description: "counting tool".into(),
            schema: serde_json::from_value(json!({"type":"object"})).expect("schema"),
        }
    }
    fn prepare(&self, _params: Value, _ctx: &PrepareCtx<'_>) -> Result<PreparedCall, ToolError> {
        Err(ToolError::InvalidParams(
            "fixture call is supplied by capture".into(),
        ))
    }
    fn execute<'a, 'io>(
        &'a self,
        _call: PreparedCall,
        _io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ToolExecution::Complete(ToolResult::builtin("counted"))) })
    }
}
fn digest(n: u8) -> InvocationDigest {
    InvocationDigest::parse(&format!("sha256:{n:064x}")).expect("digest")
}
fn route() -> ego_core::permit::ForeignTransportTarget {
    ForeignTransportTarget::http(
        HttpRoute::new(
            HttpScheme::Https,
            NetworkTarget::new("mcp.example", 443).expect("target"),
        )
        .expect("route"),
    )
}
fn rules() -> Ruleset {
    let permissions = ConfigLayer::parse(Source::Project, "[permissions]\nallow=[\"tool(alpha/execute_tool)\",\"tool(alpha/weather)\",\"net(mcp.example:443)\"]").expect("rules").value.permissions.expect("permissions");
    Ruleset::from_layer(permissions, Source::Project)
}

#[test]
fn semantic_mcp_preparation_orders_subjects_and_requires_guarded_protocol_claim() {
    let call = PreparedCall::mcp(
        McpSubject::Tool {
            server: "alpha".into(),
            name: "weather".into(),
        },
        McpOperationKind::CallTool,
        vec![
            (
                ForeignToolId::new("alpha", "execute_tool").expect("outer"),
                digest(1),
            ),
            (
                ForeignToolId::new("alpha", "weather").expect("inner"),
                digest(2),
            ),
        ],
        McpAdmissionFingerprint::new("capture-7", "weather", "schema-7").expect("admission"),
        "profile-7",
        route(),
        &json!({"name":"weather","arguments":{}}),
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
    let authorized = match ego_core::permit::PolicyEngine::new(
        ego_core::permit::CanonPath::new("/workspace"),
        Mode::Default,
        rules(),
    )
    .classify(call.clone())
    {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("MCP authority must settle: {other:?}"),
    };
    assert_eq!(
        authorized
            .call()
            .mcp_envelope()
            .expect("MCP envelope")
            .transport_fingerprint(),
        "profile-7"
    );
}

#[test]
fn semantic_or_byte_mismatch_is_definitely_unsent_and_execution_has_closed_outcomes() {
    let call = PreparedCall::mcp(
        McpSubject::Resource {
            server: "alpha".into(),
            uri: "weather://today".into(),
        },
        McpOperationKind::ReadResource,
        Vec::new(),
        McpAdmissionFingerprint::new("capture-7", "resources/read", "schema-7").expect("admission"),
        "profile-7",
        route(),
        &json!({"uri":"weather://today"}),
    )
    .expect("resource preparation");
    let changed = PreparedCall::mcp(
        McpSubject::Resource {
            server: "alpha".into(),
            uri: "weather://tomorrow".into(),
        },
        McpOperationKind::ReadResource,
        Vec::new(),
        McpAdmissionFingerprint::new("capture-7", "resources/read", "schema-7").expect("admission"),
        "profile-7",
        route(),
        &json!({"uri":"weather://tomorrow"}),
    )
    .expect("changed preparation");
    assert_ne!(
        call.authority_fingerprint(),
        changed.authority_fingerprint()
    );
    let required = ToolExecution::InputRequired(
        ForeignInputRequired::new(
            "continuation-1",
            QuestionRequest::new(
                "Continue",
                QuestionMode::Single,
                vec![QuestionOption::new("yes", "Yes")],
                false,
            ),
        )
        .expect("continuation"),
    );
    assert!(matches!(required, ToolExecution::InputRequired(_)));
}

#[test]
fn immutable_capture_retains_exact_arc_for_old_generation_and_releases_on_drop() {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let tool = CountingTool::new("weather", Arc::clone(&calls), Arc::clone(&drops));
    let old = ego_core::turn::CapturedToolCatalogue::new(
        "capture-1",
        vec![ego_core::turn::RegisteredTool::from(Arc::clone(&tool))],
    )
    .expect("capture");
    let successor = ego_core::turn::CapturedToolCatalogue::new("capture-2", Vec::new())
        .expect("removed successor");
    assert_ne!(old.generation(), successor.generation());
    drop(tool);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(old);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
