//! The parties that may answer a question the rules did not settle.
//!
//! `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §6 orders three of
//! them: the mode at step 5, the judge at step 6, the human at step 7. One
//! trait serves the policy parties because "may this proceed" is one question
//! however it is answered — and the engine, not the party, decides what the
//! answer means. The human has a distinct port so the turn can durably record
//! the request before awaiting that final seat.
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

/// What a party answered about a [`Pending`].
///
/// §4's `Verdict`, minus its `by: Authority`. That field exists for one
/// reader — the audit event that records *who* let something through — and
/// that event is the next slice, with its own contract decision. A field
/// whose only reader is unbuilt is a field nothing can be wrong about, so it
/// arrives with its reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Approved without asking anybody further.
    ///
    /// **Not "wider authority".** The scope is still derived from the call by
    /// `scope_of`, so an approval decides *whether to ask*, never *what for*.
    ///
    /// `by` names the party. §4 deferred it with "it arrives with its
    /// reader"; the reader is `EventKind::PermissionDecided`, and it has
    /// arrived. The engine does **not** reconstruct this after the fact from
    /// `self.mode` — a party that answers says who it was, because an engine
    /// inferring an authority behind a `dyn Approver` would be recording a
    /// guess in an audit log.
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
    /// Not this party's to answer. The question moves on with the reason
    /// attached, which is what `ARCHITECTURE.md` §3.4 requires to reach both
    /// the next party and the audit event.
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
    Allow,
    Deny,
    Amend { params: Value },
}

/// The final party in the permission order, called only after the turn has
/// durably appended the canonical request.
///
/// This port cannot replace [`ModeApprover`]. `PolicyEngine::with_human`
/// always constructs the mode from resolved config and installs this party
/// after it.
pub trait HumanApprover: fmt::Debug + Send + Sync {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> BoxFuture<'a, PermissionDecision>;
}

/// §6 step 5: the mode ceiling.
///
/// **Crate-private, and that is the condition the whole port rests on.**
/// Until this was a port, the ceiling lived inside `classify` and nothing
/// outside could decline to apply it. A port moves the choice of
/// implementation somewhere, and wherever that somewhere is, *it* becomes the
/// mode: a turn loop that could hand in a `yolo` approver while the operator
/// configured `default` has relocated the policy into the caller.
/// `PolicyEngine` already refuses that exact move for `Scope` — "whoever
/// chose the `Scope` was the real policy, and the caller is the turn loop,
/// which has no business holding that authority" — and a mode chosen by the
/// loop is that sentence with a different noun.
///
/// So this type cannot be named outside `ego-core`, `PolicyEngine::new` is
/// the only thing that builds one, and it builds it from the `Mode` the
/// config layers resolved. There is no expression a caller could write that
/// produces `yolo` behaviour from a `default` configuration.
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
        for op in &pending.call().ops {
            let approved = match self.mode {
                // Bypass: "do not ask about it". Step 0 is not reached from
                // here — the boundary was settled before the question was
                // ever asked — so this widens nothing.
                Mode::Yolo => true,
                // Auto-approves workspace writes. `classify`'s step 0 already
                // established that every path in this call is in the
                // workspace.
                Mode::Edits => !matches!(
                    op,
                    Op::Spawn { .. }
                        | Op::Opaque { .. }
                        | Op::Connect { .. }
                        | Op::SecretEgress { .. }
                        | Op::ProtocolConnect { .. }
                ),
                // `ARCHITECTURE.md:242` — these review unresolved
                // **sensitive** actions, not every action. A read inside the
                // workspace that stopped the run to ask would be the prompt
                // fatigue `ARCHITECTURE.md:238` counts as a security bug.
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
