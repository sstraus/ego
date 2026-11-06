//! The authority to poll or cancel an MCP task.
//!
//! A task id has authority only through a ticket recorded from the prepared
//! `tools/call` that the server answered with a task (coordinator decision,
//! option A, 2026-11-05).

use ego_core::config::{ConfigLayer, Mode, Ruleset, Source};
use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject, McpTaskTicket};
use ego_core::permit::{
    Authorized, CanonPath, Classification, ForeignToolId, ForeignTransportTarget, HttpRoute,
    HttpScheme, InvocationDigest, NetworkTarget, PolicyEngine, PreparedCall,
};
use serde_json::{Value, json};

fn digest(n: u8) -> InvocationDigest {
    InvocationDigest::parse(&format!("sha256:{n:064x}")).expect("digest")
}

fn route() -> ForeignTransportTarget {
    ForeignTransportTarget::http(
        HttpRoute::new(
            HttpScheme::Https,
            NetworkTarget::new("mcp.example", 443).expect("target"),
        )
        .expect("route"),
    )
}

fn original() -> PreparedCall {
    PreparedCall::mcp(
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
        &json!({"name":"weather","arguments":{"city":"Gent"}}),
    )
    .expect("tools/call preparation")
}

fn resource_call() -> PreparedCall {
    PreparedCall::mcp(
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
    .expect("resource preparation")
}

fn engine(allow: &str) -> PolicyEngine {
    let text = format!("[permissions]\nallow=[{allow}]");
    let permissions = ConfigLayer::parse(Source::Project, &text)
        .expect("rules")
        .value
        .permissions
        .expect("permissions");
    PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        Ruleset::from_layer(permissions, Source::Project),
    )
}

const ALLOW_ALL: &str =
    "\"tool(alpha/execute_tool)\",\"tool(alpha/weather)\",\"net(mcp.example:443)\"";

pub(crate) fn authorized_original() -> Authorized {
    match engine(ALLOW_ALL).classify(original()) {
        Classification::Settled(Ok(authorized)) => authorized,
        other => panic!("the original call must be allowed: {other:?}"),
    }
}

/// A ticket recorded the way the tool executor records it: from the split
/// authorization of the call the server answered with a task.
pub(crate) fn ticket(task_id: &str) -> McpTaskTicket {
    let authorized = authorized_original();
    let by = authorized.by().clone();
    let (call, permit) = authorized.into_parts();
    McpTaskTicket::record(&call, &permit, &by, task_id).expect("ticket")
}

// Catches: a poll or cancel is classified again, so an original that a human
// approved prompts at every poll, or it runs under another authority.
#[test]
fn a_poll_and_a_cancel_inherit_the_original_authority_without_classification() {
    let ticket = ticket("task-1");
    let original_by = authorized_original().by().clone();
    for derived in [ticket.poll(), ticket.cancel()] {
        assert_eq!(derived.by(), &original_by);
        assert_eq!(derived.call().ops(), original().ops());
    }
}

// Catches: a poll or cancel sends other parameters than the recorded task id.
#[test]
fn a_poll_and_a_cancel_carry_exactly_the_recorded_task_id() {
    let ticket = ticket("task-1");
    for (derived, operation, method) in [
        (ticket.poll(), McpOperationKind::GetTask, "tasks/get"),
        (
            ticket.cancel(),
            McpOperationKind::CancelTask,
            "tasks/cancel",
        ),
    ] {
        let call = derived.call();
        let envelope = call.mcp_envelope().expect("envelope");
        assert_eq!(envelope.operation(), operation);
        assert_eq!(envelope.operation().method(), method);
        assert_eq!(
            serde_json::from_slice::<Value>(envelope.parameters()).expect("json"),
            json!({"taskId":"task-1"})
        );
        assert_eq!(
            envelope.subject(),
            &McpSubject::Task {
                server: "alpha".into(),
                tool: "weather".into(),
                task_id: "task-1".into(),
            }
        );
        // The original's transport, admission and invocation identities.
        assert_eq!(envelope.transport_fingerprint(), "profile-7");
        assert_eq!(call.ops(), original().ops());
    }
}

// Catches: a claim minted for one task id or one operation is accepted for another.
#[test]
fn another_task_id_or_operation_has_another_authority_fingerprint() {
    let (one, two) = (ticket("task-1"), ticket("task-2"));
    let fingerprints = [
        original().authority_fingerprint(),
        one.poll().call().authority_fingerprint(),
        one.cancel().call().authority_fingerprint(),
        two.poll().call().authority_fingerprint(),
    ];
    for (i, left) in fingerprints.iter().enumerate() {
        for right in &fingerprints[i + 1..] {
            assert_ne!(left, right);
        }
    }
}

// Catches: a resource read or a derived poll becomes task authority, or a
// malformed id (empty, control character, oversize) is recorded.
#[test]
fn a_ticket_needs_a_recorded_tool_call_and_a_well_formed_id() {
    let authorized = authorized_original();
    let by = authorized.by().clone();
    let (call, permit) = authorized.into_parts();
    let resource = resource_call();
    assert!(McpTaskTicket::record(&resource, &permit, &by, "task-1").is_err());
    let long = "x".repeat(257);
    for bad in ["", "   ", "line\nbreak", long.as_str()] {
        assert!(
            McpTaskTicket::record(&call, &permit, &by, bad).is_err(),
            "{bad:?}"
        );
    }
    // A derived poll is not a tools/call, so it cannot mint another ticket.
    let poll = ticket("task-1").poll().into_parts().0;
    assert!(McpTaskTicket::record(&poll, &permit, &by, "task-2").is_err());
}
