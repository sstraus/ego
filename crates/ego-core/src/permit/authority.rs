//! Who decided, recorded so an audit can answer "what did this run do
//! without asking me".
//!
//! `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §7. The arms are
//! the ones with producers in this build, including the judge and human seats
//! at §6 steps 6 and 7.

use serde::{Deserialize, Serialize};

use super::rule::RuleId;
use crate::config::ModelRef;
use crate::config::{Mode, OriginSet, Source, SourcedRule};
use crate::generator::attempt::AttemptRef;

/// One rule that contributed to a decision, and where it came from.
///
/// The same `{ id, from, also }` shape
/// `docs/decisions/2026-08-21-a-deny-that-names-who-can-lift-it.md` gave
/// `PolicyDenied::Denied`, deliberately: "who said so" is one vocabulary, and
/// two that must agree about the same fact is the duplication the reuse rule
/// exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleAuthority {
    /// The rule, by the ID `ego rules revoke` takes.
    pub id: RuleId,
    /// The layer whose copy cannot be got around.
    pub from: Source,
    /// Every other layer that also carries it. Empty for the ordinary
    /// single-layer rule.
    pub also: OriginSet,
}

impl RuleAuthority {
    /// The attribution of a rule the engine matched.
    ///
    /// Takes a [`SourcedRule`] rather than the three fields, so the winner is
    /// still chosen by `SourcedRule::attribution` and not by a second caller
    /// applying `Source::rank` its own way.
    pub fn of(entry: &SourcedRule) -> Self {
        let (from, also) = entry.attribution();
        Self {
            id: entry.rule().id(),
            from,
            also,
        }
    }
}

/// What let a call through, or stopped it.
///
/// **`Rules` is a list, and that is not over-generality.** A denial has one
/// authority because §6 step 1 stops at the first matching deny. An allow has
/// as many as the call has ops, because nothing stops early when the answer
/// is yes: `classify` mints only once *every* op has found a covering rule,
/// and those can be different rules. Recording one rule of three would be a
/// false record, and a false attribution is worse than an absent one — it
/// survives review looking like a fact.
///
/// There is no `SystemCeiling` arm. A system deny is a rule that matched, and
/// `Source::System` is already on that rule's own attribution. One fact,
/// recorded once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Authority {
    /// Every rule that covered an op of this call. Non-empty.
    Rules { rules: Vec<RuleAuthority> },
    /// The mode decided, at §6 step 2 (the floor) or step 5 (the ceiling).
    Mode { mode: Mode },
    /// A party at the [`Approver`](super::Approver) port decided, and it was
    /// not the mode.
    ///
    /// **This arm is what makes the permission ports more than names for
    /// `ModeApprover`.** A non-mode verdict carries who gave it. So
    /// an `Authority` with only `Rules`, `Mode` and `Boundary` would leave
    /// every non-mode approver with nothing true to say: it would have to
    /// claim the mode decided, which is the false attribution
    /// `2026-08-21-a-deny-that-names-who-can-lift-it.md` exists to stop.
    ///
    /// It intentionally names the role rather than a person. This build has no
    /// identity source for the operator at the terminal; inventing one would
    /// turn a constant into audit data. Interactive allow and deny decisions
    /// are the production writers of this arm.
    Party,
    /// The workspace boundary, at §6 step 0. Not a rule: nobody wrote it,
    /// `--allow` cannot widen it and no mode lifts it, so there is no ID to
    /// record and nothing to revoke.
    Boundary,
    /// The run's session-bound evidence store intrinsically admitted one
    /// exact evidence reference.
    SessionEvidence,
    /// One paid judge attempt decided the exact pending call.
    Judge {
        model: ModelRef,
        attempt: AttemptRef,
    },
    /// One calibrated Decisions exchange judged this exact exec call safe.
    DestructiveJudge { model: ModelRef },
    /// A previously generated opinion was reapplied after fresh policy checks.
    CachedJudge { model: ModelRef, analysis: String },
}
