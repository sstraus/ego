//! The parties that may answer a question the rules did not settle.
//!
//! `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §6 orders three of
//! them: the mode at step 5, the judge at step 6, the human at step 7. One
//! trait serves all three, because "may this proceed" is one question however
//! it is answered — and the engine, not the party, decides what the answer
//! means.
//!
//! Only the mode exists in this build. It is deliberately *not* public: an
//! [`Approver`] is a seam for implementations, never a lever for choosing
//! whose authority applies, and the mode's authority comes from the config
//! layers rather than from whoever holds the engine.

use futures::future::{self, BoxFuture};
use std::fmt;

use super::grant::{Escalation, Pending};
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
    Allow,
    /// Refused. The call does not run and no later party is consulted.
    ///
    /// **No production approver returns this yet, and that is not a gap.**
    /// The human seat at §6 step 7 is what produces it in anger, and that
    /// seat belongs to regression case. `Deny` is here because it defines the
    /// port rather than anticipating a feature: [`Approver`] models the
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
    Deny,
    /// Not this party's to answer. The question moves on with the reason
    /// attached, which is what `ARCHITECTURE.md` §3.4 requires to reach both
    /// the next party and the audit event.
    Escalate { why: Escalation },
}

/// A party that may answer a [`Pending`].
///
/// Async, and that is not anticipation: the human at §6 step 7 is asked over
/// a wire, and a sync signature is one regression case would have to break in
/// order to implement. **That ACP implementation belongs to regression_case, not to
/// this story.**
///
/// Takes the `Pending` by reference. An approver reads the question and
/// answers it; it never consumes it, because deciding what becomes of the
/// question is the engine's job — `PolicyEngine::settle` is the only thing
/// that turns a verdict into a permit or a refusal.
pub trait Approver: fmt::Debug + Send + Sync {
    fn approve<'a>(&'a self, pending: &'a Pending) -> BoxFuture<'a, Verdict>;
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
}

impl ModeApprover {
    pub(super) fn new(mode: Mode) -> Self {
        Self { mode }
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
                Mode::Edits => true,
                // `ARCHITECTURE.md:242` — these review unresolved
                // **sensitive** actions, not every action. A read inside the
                // workspace that stopped the run to ask would be the prompt
                // fatigue `ARCHITECTURE.md:238` counts as a security bug.
                //
                // `Plan` never arrives here with a mutating op: the floor at
                // step 2 already refused it. `Auto` behaves as `Default`
                // because step 6's judge does not exist; when it does, `Auto`
                // consults it rather than declining, and `Default` is
                // unchanged.
                Mode::Plan | Mode::Default | Mode::Auto => !op.mutates(),
            };
            if !approved {
                return Verdict::Escalate {
                    why: Escalation::ModeAsks {
                        mode: self.mode,
                        op: op.name(),
                        path: op.path().clone(),
                    },
                };
            }
        }
        Verdict::Allow
    }
}

impl Approver for ModeApprover {
    fn approve<'a>(&'a self, pending: &'a Pending) -> BoxFuture<'a, Verdict> {
        Box::pin(future::ready(self.verdict(pending)))
    }
}
