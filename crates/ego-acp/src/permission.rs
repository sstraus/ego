//! ACP v1 permission adapter: the connected host is the sole human seat.
//!
//! `docs/decisions/2026-09-03-acp-host-control-is-negotiated-per-session.md`
//! makes the connected client the only person in an ACP-owned turn, and
//! forbids the adapter from opening stdin when that client cannot answer.
//! Everything below exists to keep both halves of that true: the question
//! carries the canonical operation, and every answer that is not an explicit
//! grant settles as a denial.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use agent_client_protocol::{Client, ConnectionTo};
use agent_client_protocol_schema::v1;
use ego_core::event::id::ItemId;
use ego_core::permit::{Escalation, HumanApprover, Op, PermissionDecision, PermissionRequest};

/// One connection's ACP permission boundary. Durable request ordering and the
/// evaluation that follows the answer remain core-owned.
#[derive(Clone)]
pub struct AcpPermissionAdapter {
    session: v1::SessionId,
    connection: Option<ConnectionTo<Client>>,
}

impl std::fmt::Debug for AcpPermissionAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpPermissionAdapter")
            .field("session", &self.session)
            .field("connected", &self.connection.is_some())
            .finish()
    }
}

impl AcpPermissionAdapter {
    /// The only option that grants, and the only one that refuses.
    ///
    /// ACP also defines `allow_always` and `reject_always`, and ego offers
    /// neither. A remembered decision needs a store with a scope, a key and a
    /// revocation rule; none exists. Advertising the option anyway would let
    /// one answer widen every later turn, which is the exact thing
    /// `2026-08-21-a-permit-nobody-can-widen.md` forbids.
    pub const ALLOW_ONCE: &'static str = "allow-once";
    pub const REJECT_ONCE: &'static str = "reject-once";

    pub fn new(session: v1::SessionId) -> Self {
        Self {
            session,
            connection: None,
        }
    }

    pub fn live(session: v1::SessionId, connection: ConnectionTo<Client>) -> Self {
        Self {
            session,
            connection: Some(connection),
        }
    }

    pub fn approver(&self) -> Arc<dyn HumanApprover> {
        Arc::new(self.clone())
    }

    /// The question, addressed to the host.
    ///
    /// `item` is the durable tool-call item, and `project.rs` already used it
    /// as the streamed `ToolCallId`. Reusing it here is what lets the host
    /// render the prompt on the call the operator is watching rather than as
    /// a free-floating dialog.
    pub fn permission_request(
        session: &v1::SessionId,
        item: ItemId,
        tool: &str,
        ops: &[Op],
        why: &Escalation,
    ) -> v1::RequestPermissionRequest {
        let title = PermissionRequest::new(tool, ops.to_vec(), why.clone()).to_string();
        let tool_call = v1::ToolCallUpdate::new(
            v1::ToolCallId::new(item.to_string()),
            v1::ToolCallUpdateFields::new().title(title),
        );
        v1::RequestPermissionRequest::new(
            session.clone(),
            tool_call,
            vec![
                v1::PermissionOption::new(
                    v1::PermissionOptionId::new(Self::ALLOW_ONCE),
                    "Allow once",
                    v1::PermissionOptionKind::AllowOnce,
                ),
                v1::PermissionOption::new(
                    v1::PermissionOptionId::new(Self::REJECT_ONCE),
                    "Reject once",
                    v1::PermissionOptionKind::RejectOnce,
                ),
            ],
        )
    }

    /// What the host answered, in core's vocabulary.
    ///
    /// Only the one option ego offered grants. `Cancelled` is the outcome a
    /// client must send for every pending request when it cancels the turn,
    /// and an option id ego never put on the wire is a client asserting an
    /// authority it was not given — both deny, because the base decision says
    /// an unreadable answer mints no permit.
    ///
    /// There is no `Amend`. ACP v1 has no shape for returning replacement
    /// tool parameters, so the ACP seat is a truthful two-answer downgrade of
    /// the three-answer vocabulary rather than a pretend third option.
    pub fn decision(response: &v1::RequestPermissionResponse) -> PermissionDecision {
        match &response.outcome {
            v1::RequestPermissionOutcome::Selected(selected)
                if selected.option_id.0.as_ref() == Self::ALLOW_ONCE =>
            {
                PermissionDecision::Allow
            }
            _ => PermissionDecision::Deny,
        }
    }

    async fn decide_live(&self, item: ItemId, request: PermissionRequest) -> PermissionDecision {
        let Some(connection) = self.connection.as_ref() else {
            // No seat is not consent, and the fallback a terminal would offer
            // is the second invisible human the ADR rejects.
            return PermissionDecision::Deny;
        };
        let wire = Self::permission_request(
            &self.session,
            item,
            request.tool(),
            request.ops(),
            request.why(),
        );
        match connection.send_request(wire).block_task().await {
            Ok(response) => Self::decision(&response),
            // A transport failure settles the waiting boundary without
            // granting authority; the run records the denial, not a hang.
            Err(error) => {
                tracing::warn!(%error, "ACP permission request failed");
                PermissionDecision::Deny
            }
        }
    }
}

impl HumanApprover for AcpPermissionAdapter {
    fn decide<'a>(
        &'a self,
        item: ItemId,
        request: &'a PermissionRequest,
    ) -> Pin<Box<dyn Future<Output = PermissionDecision> + Send + 'a>> {
        Box::pin(self.decide_live(item, request.clone()))
    }
}
