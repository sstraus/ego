//! Who decided, recorded so an audit can answer "what did this run do
//! without asking me".
//!
//! Attribution includes rules, mode, boundary and external judge or human
//! parties that can produce decisions in this build.

use serde::{Deserialize, Serialize};

use super::rule::RuleId;
use crate::config::ModelRef;
use crate::config::{Mode, OriginSet, Source, SourcedRule};
use crate::generator::attempt::AttemptRef;

/// One rule that contributed to a decision, and where it came from.
///
/// Share the `{ id, from, also }` shape with `PolicyDenied::Denied` so rule
/// identity and provenance use one vocabulary.
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

/// Authority that admitted or refused a call. Deny stops at its first matching rule; allow can
/// require different rules for different operations. Preserve every contributing rule's
/// attribution. System origin already lives on the rule and needs no duplicate variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Authority {
    /// Every rule that covered an op of this call. Non-empty.
    Rules { rules: Vec<RuleAuthority> },
    /// Mode authority, at its read-only floor or final approval decision.
    Mode { mode: Mode },
    /// A party at the [`Approver`](super::Approver) port decided, and it was
    /// not the mode.
    ///
    /// A non-mode approver must name the party that decided. Attributing its
    /// verdict to mode would misstate which authority can change the outcome.
    ///
    /// It intentionally names the role rather than a person. This build has no
    /// identity source for the operator at the terminal; inventing one would
    /// turn a constant into audit data. Interactive allow and deny decisions
    /// are the production writers of this arm.
    Party,
    /// Workspace boundary, not a revocable rule. Neither mode nor an allow can widen it, so it
    /// has no rule ID.
    Boundary,
    /// The run's session-bound evidence store intrinsically admitted one
    /// exact evidence reference.
    SessionEvidence,
    /// An exact ID owned by the current session admits inspection or cancellation.
    SessionProcess,
    /// One paid judge attempt decided the exact pending call.
    Judge {
        model: ModelRef,
        attempt: AttemptRef,
    },
    /// A previously generated opinion was reapplied after fresh policy checks.
    CachedJudge { model: ModelRef, analysis: String },
}
