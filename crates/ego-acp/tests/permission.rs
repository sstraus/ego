//! contract for the ACP human permission seat (regression case).
//!
//! The connected host is the only human seat for an ACP-owned turn. These
//! pin the two halves that decide whether that is true: the question ego
//! puts on the wire, and what it does with every answer a host can give.
//! Nothing here may resolve to approval by default — the base decision
//! (`2026-08-24-interactive-trust-and-human-approval`) says an absent or
//! unreadable answer mints no permit.

use std::sync::Arc;

use agent_client_protocol_schema::v1;
use ego_acp::permission::AcpPermissionAdapter;
use ego_core::config::Mode;
use ego_core::event::id::ItemId;
use ego_core::permit::{
    CanonPath, Escalation, HumanApprover, Op, OpName, PermissionDecision, PermissionRequest,
};
use futures::executor::block_on;

fn session() -> v1::SessionId {
    v1::SessionId::new("session-under-test")
}

fn why() -> Escalation {
    Escalation::ModeAsks {
        mode: Mode::Default,
        op: OpName::Spawn,
        path: CanonPath::new("/workspace"),
    }
}

fn ops() -> Vec<Op> {
    vec![Op::Read {
        path: CanonPath::new("/workspace/secrets.txt"),
    }]
}

fn answered(option: &str) -> v1::RequestPermissionResponse {
    v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Selected(
        v1::SelectedPermissionOutcome::new(v1::PermissionOptionId::new(option)),
    ))
}

/// The tool call id ego streams is the durable item id (`project.rs:438`).
/// The permission prompt has to name the same one or a host renders it
/// attached to nothing, which is why the port carries the item.
#[test]
fn the_permission_prompt_names_the_tool_call_the_host_is_already_showing() {
    let item = ItemId::new();
    let request =
        AcpPermissionAdapter::permission_request(&session(), item, "exec", &ops(), &why());
    assert_eq!(request.tool_call.tool_call_id.0.as_ref(), item.to_string());
    assert_eq!(request.session_id, session());
}

/// ego persists no permission decision, so it must not offer to remember
/// one. Advertising `allow_always` would promise a store that does not
/// exist and let one answer widen every later turn.
#[test]
fn only_the_once_options_are_offered_because_ego_remembers_no_decision() {
    let request =
        AcpPermissionAdapter::permission_request(&session(), ItemId::new(), "exec", &ops(), &why());
    let kinds: Vec<_> = request.options.iter().map(|option| option.kind).collect();
    assert_eq!(
        kinds,
        vec![
            v1::PermissionOptionKind::AllowOnce,
            v1::PermissionOptionKind::RejectOnce
        ]
    );
}

#[test]
fn the_allow_option_is_the_only_answer_that_grants() {
    assert_eq!(
        AcpPermissionAdapter::decision(&answered(AcpPermissionAdapter::ALLOW_ONCE)),
        PermissionDecision::Allow
    );
    assert_eq!(
        AcpPermissionAdapter::decision(&answered(AcpPermissionAdapter::REJECT_ONCE)),
        PermissionDecision::Deny
    );
}

/// A host that cancels the turn answers every pending permission request
/// with `Cancelled`. That is not consent.
#[test]
fn a_cancelled_turn_denies_rather_than_leaving_the_seat_open() {
    let response = v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Cancelled);
    assert_eq!(
        AcpPermissionAdapter::decision(&response),
        PermissionDecision::Deny
    );
}

/// An option ego never offered cannot be the option ego honours. Mapping an
/// unknown id onto anything but denial would let a host mint authority by
/// inventing a string.
#[test]
fn an_option_ego_never_offered_is_a_denial() {
    assert_eq!(
        AcpPermissionAdapter::decision(&answered("allow-always")),
        PermissionDecision::Deny
    );
    assert_eq!(
        AcpPermissionAdapter::decision(&answered("")),
        PermissionDecision::Deny
    );
}

/// No connection is no human. The adapter must never fall through to stdin
/// or to approval; the ADR calls terminal input behind an ACP connection a
/// second, invisible human seat.
#[test]
fn a_seat_with_no_live_connection_denies_and_never_reaches_for_a_terminal() {
    let adapter = AcpPermissionAdapter::new(session());
    let request = PermissionRequest::new("exec", ops(), why());
    let decision = block_on(adapter.approver().decide(ItemId::new(), &request));
    assert_eq!(decision, PermissionDecision::Deny);
}

/// The adapter is the host implementation of core's one human port; serve
/// must hand this to the run rather than settling permission in the
/// adapter. Compile-checked so the wiring cannot silently regress.
#[allow(dead_code)]
fn acp_exposes_the_core_human_seat(adapter: &AcpPermissionAdapter) -> Arc<dyn HumanApprover> {
    adapter.approver()
}
