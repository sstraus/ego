//! The parties that may answer a question the rules did not settle.
//!
//! The engine orders mode, judge and human decisions. Approvers answer
//! whether a call may proceed; the engine interprets that answer. The human
//! port is separate so the turn can durably record a question before waiting.
//!
//! The mode and a distinct optional human seat exist in this build. The mode
//! implementation is deliberately *not* public: an [`Approver`] is a seam for
//! policy parties, never a lever for choosing whose authority applies. The
//! human has its own [`HumanApprover`] vocabulary because only a person may
//! amend raw tool parameters.

use futures::future::{self, BoxFuture};
use serde_json::Value;
use std::fmt;

use super::authority::Authority;
use super::grant::{Escalation, Pending, PermissionRequest};
use super::path::CanonPath;
use super::prepared_call::Op;
use crate::config::Mode;
use crate::event::id::ItemId;

/// A party's verdict on a pending call. The policy engine owns permit construction and audit
/// attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Approval controls whether to proceed, not grant width. Scope remains derived from the
    /// prepared call. The answering party supplies its identity for the permission audit; the
    /// engine must not infer it behind a dynamic approver.
    Allow { by: Authority },
    /// Refused. The call does not run and no later party is consulted.
    /// `by` names the party, for the same reason `Allow`'s does.
    ///
    /// The human uses [`PermissionDecision::Deny`], not this policy-layer
    /// verdict. `Deny` remains because it defines the port: [`Approver`] models the
    /// parties who may *answer* a question, and a party that cannot say no is
    /// an escalator, not an approver. Trimming the vocabulary to today's one
    /// implementation would make this trait describe [`ModeApprover`] instead
    /// of the role.
    ///
    /// Nor is it an unreachable variant in the sense `AGENTS.md:22-37`
    /// forbids: a second real implementation produces it in this crate's
    /// tests and `PolicyEngine::settle`'s handling of it — a
    /// `PolicyDenied::Refused` that names no rule — is asserted. Dead code is
    /// a variant nothing produces and nothing pins. So do not "simplify" this
    /// away, and do not read its absence from the production path as a bug.
    Deny { by: Authority },
    /// Not this party's to answer.
    Escalate { why: Escalation },
}

/// A party that may answer a [`Pending`].
///
/// Async because later policy parties such as the judge are model calls. The
/// terminal human has a separate port below; an ACP human may also implement
/// that port asynchronously without changing the engine contract.
///
/// Takes the `Pending` by reference. An approver reads the question and
/// answers it; it never consumes it, because deciding what becomes of the
/// question is the engine's job — `PolicyEngine::settle` is the only thing
/// that turns a verdict into a permit or a refusal.
pub trait Approver: fmt::Debug + Send + Sync {
    fn approve<'a>(&'a self, pending: &'a Pending) -> BoxFuture<'a, Verdict>;
}

/// What the final, human party answered.
///
/// Distinct from [`Verdict`]: policy parties may escalate but never rewrite a
/// call; a human may amend but is the end of the chain and therefore cannot
/// escalate. Raw parameters deliberately go back through `Tool::prepare` and
/// the whole classifier before any permit can be minted.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    /// Approve this exact action for reuse within the live session.
    Allow,
    /// Approve only the displayed invocation.
    AllowOnce,
    Deny,
    /// A refusal that carries the human's words. Unlike [`Self::Deny`], which
    /// ends the run, the call is answered with the instruction and the model
    /// continues.
    DenyWithInstruction {
        instruction: String,
    },
    Amend {
        params: Value,
    },
}

/// The final party in the permission order, called only after the turn has
/// durably appended the canonical request.
///
/// This port cannot replace [`ModeApprover`]. `PolicyEngine::with_human`
/// always constructs the mode from resolved config and installs this party
/// after it.
///
/// Carries the tool call's [`ItemId`] for the same reason
/// [`crate::question::Elicitor`] does: a seat that renders the question
/// somewhere other than a terminal has to say *which* call it is asking
/// about. The ACP projection turns that item into the tool call id it already
/// streamed, so the host attaches the prompt to the call the operator can
/// see. A terminal seat ignores it, which is why this is one argument rather
/// than a second port.
pub trait HumanApprover: fmt::Debug + Send + Sync {
    fn decide<'a>(
        &'a self,
        item: ItemId,
        request: &'a PermissionRequest,
    ) -> BoxFuture<'a, PermissionDecision>;
}

/// Mode approver, crate-private and built only from resolved configuration by
/// `PolicyEngine::new`. External callers cannot inject a permissive implementation that
/// contradicts the configured mode.
#[derive(Debug)]
pub(super) struct ModeApprover {
    mode: Mode,
    workspace: CanonPath,
}

impl ModeApprover {
    pub(super) fn new(mode: Mode, workspace: CanonPath) -> Self {
        Self { mode, workspace }
    }

    /// The whole of step 5, and it is sync because a mode is a value the run
    /// already holds. The trait is async for the parties that are not.
    fn verdict(&self, pending: &Pending) -> Verdict {
        for op in pending.call().ops() {
            let approved = match self.mode {
                // Bypass: "do not ask about it". Step 0 is not reached from
                // here — the boundary was settled before the question was
                // ever asked — so this widens nothing.
                Mode::Yolo => true,
                // Auto-approves workspace writes. `classify`'s step 0 already
                // established that every path in this call is in the
                // workspace.
                Mode::Edits => {
                    (!op.mutates()
                        || op.subject_path(&self.workspace).is_some_and(|path| {
                            path.as_path().starts_with(self.workspace.as_path())
                        }))
                        && !matches!(
                            op,
                            Op::Spawn { .. }
                                | Op::Opaque { .. }
                                | Op::Connect { .. }
                                | Op::SecretEgress { .. }
                                | Op::ProtocolConnect { .. }
                        )
                }
                // Approvers decide whether to proceed; the policy engine interprets the
                // answer.
                //
                // `Plan` never arrives here with a mutating op: the floor at
                // step 2 already refused it. `Auto` deliberately declines at
                // this seat so TurnLoop can insert the paid judge before the
                // human; `Default` proceeds directly to the human.
                Mode::Plan | Mode::Default | Mode::Auto => !op.mutates(),
            };
            if !approved {
                let why = match op.subject_path(&self.workspace) {
                    Some(path) => Escalation::ModeAsks {
                        mode: self.mode,
                        op: op.name(),
                        path: path.clone(),
                    },
                    None => Escalation::ModeAsksSubject {
                        mode: self.mode,
                        op: op.name(),
                        subject: op.subject(&self.workspace),
                    },
                };
                return Verdict::Escalate { why };
            }
        }
        Verdict::Allow {
            by: Authority::Mode { mode: self.mode },
        }
    }
}

impl Approver for ModeApprover {
    fn approve<'a>(&'a self, pending: &'a Pending) -> BoxFuture<'a, Verdict> {
        Box::pin(future::ready(self.verdict(pending)))
    }
}
