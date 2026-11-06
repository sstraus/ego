//! ACP v1 permission adapter: the connected host is the sole human seat.
//!
//! The connected client is the sole human seat in an ACP-owned turn.
//! The adapter must not open stdin when that client cannot answer. Questions
//! carry the canonical operation; every answer other than an explicit grant
//! settles as a denial.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::{Client, ConnectionTo};
use agent_client_protocol_schema::v1;
use ego_core::config::ManagedRuleEffect;
use ego_core::config::rule_management::RememberPlan;
use ego_core::event::id::ItemId;
use ego_core::permit::{Escalation, HumanApprover, Op, PermissionDecision, PermissionRequest};

/// Atomic user-rule mutation owned by the binary, never by the ACP host.
pub trait PermissionRuleWriter: std::fmt::Debug + Send + Sync {
    fn remember<'a>(
        &'a self,
        effect: ManagedRuleEffect,
        plan: RememberPlan,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// One connection's ACP permission boundary. Durable request ordering and the
/// evaluation that follows the answer remain core-owned.
#[derive(Clone)]
pub struct AcpPermissionAdapter {
    session: v1::SessionId,
    connection: Option<ConnectionTo<Client>>,
    rule_writer: Option<Arc<dyn PermissionRuleWriter>>,
    session_approval: Arc<AtomicBool>,
}

impl std::fmt::Debug for AcpPermissionAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpPermissionAdapter")
            .field("session", &self.session)
            .field("connected", &self.connection.is_some())
            .field("remembering", &self.rule_writer.is_some())
            .finish()
    }
}

impl AcpPermissionAdapter {
    /// Wire IDs for the one-turn and user-rule-backed answers.
    pub const ALLOW_ONCE: &'static str = "allow-once";
    pub const REJECT_ONCE: &'static str = "reject-once";
    pub const ALLOW_ALWAYS: &'static str = "allow-always";
    pub const ALLOW_SESSION: &'static str = "allow-session";
    pub const REJECT_ALWAYS: &'static str = "reject-always";

    pub fn new(session: v1::SessionId) -> Self {
        Self {
            session,
            connection: None,
            rule_writer: None,
            session_approval: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn live(session: v1::SessionId, connection: ConnectionTo<Client>) -> Self {
        Self {
            session,
            connection: Some(connection),
            rule_writer: None,
            session_approval: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_rule_writer(mut self, rule_writer: Option<Arc<dyn PermissionRuleWriter>>) -> Self {
        self.rule_writer = rule_writer;
        self
    }

    pub fn with_session_approval(mut self, approval: Arc<AtomicBool>) -> Self {
        self.session_approval = approval;
        self
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
        Self::wire_request(session, item, title, false, false)
    }

    fn wire_request(
        session: &v1::SessionId,
        item: ItemId,
        title: String,
        rememberable: bool,
        explicit_answer: bool,
    ) -> v1::RequestPermissionRequest {
        let tool_call = v1::ToolCallUpdate::new(
            v1::ToolCallId::new(item.to_string()),
            v1::ToolCallUpdateFields::new().title(title),
        );
        let mut options = vec![
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
        ];
        if !explicit_answer {
            options.push(v1::PermissionOption::new(
                v1::PermissionOptionId::new(Self::ALLOW_SESSION),
                "Allow all for this session",
                v1::PermissionOptionKind::AllowAlways,
            ));
        }
        if rememberable && !explicit_answer {
            options.push(v1::PermissionOption::new(
                v1::PermissionOptionId::new(Self::ALLOW_ALWAYS),
                "Allow always (remember displayed rules)",
                v1::PermissionOptionKind::AllowAlways,
            ));
            options.push(v1::PermissionOption::new(
                v1::PermissionOptionId::new(Self::REJECT_ALWAYS),
                "Reject always (remember displayed rules)",
                v1::PermissionOptionKind::RejectAlways,
            ));
        }
        v1::RequestPermissionRequest::new(session.clone(), tool_call, options)
    }

    fn live_request(
        &self,
        item: ItemId,
        request: &PermissionRequest,
    ) -> v1::RequestPermissionRequest {
        let plan = self
            .rule_writer
            .as_ref()
            .and_then(|_| request.presentation())
            .and_then(|presentation| presentation.remember_plan().ok());
        let mut title = request.to_string();
        if let Some(plan) = plan {
            for (_, rule) in plan.candidate_rows() {
                title.push_str(&format!("\nRemembered rule: {rule}"));
            }
            for (_, dimension) in plan.omission_rows() {
                title.push_str(&format!(
                    "\nRemembering omits: {}",
                    format!("{dimension:?}").to_lowercase()
                ));
            }
            if plan.discloses_values() {
                title.push_str(&format!("\nWarning: {}", RememberPlan::DISCLOSURE_NOTICE));
            }
        }
        Self::wire_request(
            &self.session,
            item,
            title,
            plan.is_some(),
            request.requires_explicit_answer(),
        )
    }

    /// What the host answered, in core's vocabulary.
    ///
    /// Only a one-turn grant can be decided without the rule writer. The
    /// `*_always` answers are handled by `decide_live` after persistence.
    /// `Cancelled` and an unknown option ID deny.
    ///
    /// There is no `Amend`. ACP v1 has no shape for returning replacement
    /// tool parameters, so the ACP seat is a truthful two-answer downgrade of
    /// the three-answer vocabulary rather than a pretend amendment.
    pub fn decision(response: &v1::RequestPermissionResponse) -> PermissionDecision {
        match &response.outcome {
            v1::RequestPermissionOutcome::Selected(selected)
                if selected.option_id.0.as_ref() == Self::ALLOW_ONCE =>
            {
                PermissionDecision::AllowOnce
            }
            _ => PermissionDecision::Deny,
        }
    }

    /// Waits as long as the host does, deliberately.
    ///
    /// ACP v1 defines no timeout, deadline or expiry on a permission
    /// request — the pinned schema has no such field anywhere. It defines
    /// the opposite: `RequestPermissionOutcome::Cancelled` exists precisely
    /// for this, and a client that cancels the turn MUST answer every
    /// pending request with it. The bound belongs to the host.
    ///
    /// A wall-clock timeout here would be worse than none. It would refuse
    /// an operation on the operator's behalf while the host is still
    /// legitimately waiting for a click, and the host would never learn it:
    /// from its side the prompt is still open.
    async fn decide_live(&self, item: ItemId, request: PermissionRequest) -> PermissionDecision {
        let Some(connection) = self.connection.as_ref() else {
            // No seat is not consent. Never open terminal input behind an ACP connection.
            return PermissionDecision::Deny;
        };
        // A previous tool-session grant cannot approve newly reconstructed
        // execution authority. Use this same permission seat for a fresh answer.
        if self.session_approval.load(Ordering::Acquire) && !request.requires_explicit_answer() {
            return PermissionDecision::Allow;
        }
        let wire = self.live_request(item, &request);
        match connection.send_request(wire).block_task().await {
            Ok(response) => self.apply_answer(&request, &response).await,
            // A transport failure settles the waiting boundary without
            // granting authority; the run records the denial, not a hang.
            Err(error) => {
                tracing::warn!(%error, "ACP permission request failed");
                PermissionDecision::Deny
            }
        }
    }

    async fn apply_answer(
        &self,
        request: &PermissionRequest,
        response: &v1::RequestPermissionResponse,
    ) -> PermissionDecision {
        if request.requires_explicit_answer() {
            return Self::decision(response);
        }
        let v1::RequestPermissionOutcome::Selected(selected) = &response.outcome else {
            return PermissionDecision::Deny;
        };
        match selected.option_id.0.as_ref() {
            Self::ALLOW_SESSION => {
                self.session_approval.store(true, Ordering::Release);
                PermissionDecision::Allow
            }
            Self::ALLOW_ALWAYS => {
                self.remember_answer(request, ManagedRuleEffect::Allow)
                    .await
            }
            Self::REJECT_ALWAYS => self.remember_answer(request, ManagedRuleEffect::Deny).await,
            _ => Self::decision(response),
        }
    }

    async fn remember_answer(
        &self,
        request: &PermissionRequest,
        effect: ManagedRuleEffect,
    ) -> PermissionDecision {
        let Some(writer) = &self.rule_writer else {
            return PermissionDecision::Deny;
        };
        let Some(plan) = request
            .presentation()
            .and_then(|presentation| presentation.remember_plan().ok())
        else {
            return PermissionDecision::Deny;
        };
        match writer.remember(effect, plan.clone()).await {
            Ok(()) if effect == ManagedRuleEffect::Allow => PermissionDecision::Allow,
            Ok(()) => PermissionDecision::Deny,
            Err(error) => {
                tracing::warn!(%error, "ACP remembered permission could not be stored");
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
