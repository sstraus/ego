//! Persistent session-goal state and transitions.

use serde::{Deserialize, Serialize};

use crate::ceiling::Ceilings;
use crate::event::id::{GoalId, Seq, TurnId};
use crate::store::evidence::EvidenceRef;

/// Automatic work pauses at this allowance until the operator resumes it.
pub const AUTOMATIC_CONTINUATION_LIMIT: u32 = 20;

/// Consecutive distinct-turn observations needed to block an active goal.
pub const BLOCKER_OBSERVATION_LIMIT: u64 = 5;

/// Authority responsible for a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalOwner {
    Operator,
    Automated,
    /// Independent session goal evaluator.
    Check,
}

/// Current state of a persistent goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Active,
    Achieved,
    Blocked,
    Cancelled,
}

/// An observation that may change a goal's reduced state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalTransition {
    /// End continuation without attesting achievement or changing ownership.
    Cancelled {
        cancelled_by: GoalOwner,
    },
    /// Operator grants more automatic work without replacing the goal.
    AllowanceExtended {
        turns: u32,
    },
    /// A checked condition requires operator action, not another work turn.
    Blocked {
        reason: String,
    },
    BlockerObserved {
        reason: String,
    },
    Achieved {
        attested_by: GoalOwner,
        evidence: Option<EvidenceRef>,
    },
}

/// A goal transition that would contradict its durable authority or state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalRefusal {
    NoActiveGoal,
    StaleGoal,
    InvalidExtension,
    AutomaticNotPaused,
    AlreadyTerminal(GoalState),
    ActiveGoalExists,
    AutomaticWorkPaused,
    EvidenceRequired,
    AttestationWouldWiden,
    CancellationWouldWiden,
}

impl std::fmt::Display for GoalRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidExtension => formatter.write_str(
                "goal extension requires a positive turn count and must not overflow the allowance",
            ),
            Self::AutomaticNotPaused => {
                formatter.write_str("only an exhausted active automatic goal can be resumed")
            }
            Self::NoActiveGoal => formatter.write_str("session has no active goal"),
            Self::StaleGoal => formatter.write_str("goal_id does not name the current goal"),
            Self::AlreadyTerminal(state) => write!(formatter, "goal is already {state:?}"),
            Self::ActiveGoalExists => formatter.write_str("session already has an active goal"),
            Self::AutomaticWorkPaused => formatter.write_str(
                "the operator stopped automatic work for this request, so a goal cannot be created now; finish or answer the request without a goal, and retrying unchanged cannot help until a new user request or an explicit operator goal arrives",
            ),
            Self::EvidenceRequired => {
                formatter.write_str("automated goal achievement requires evidence")
            }
            Self::CancellationWouldWiden => formatter
                .write_str("the model may cancel only an automatic (model-owned) goal; operator goals require the operator"),
            Self::AttestationWouldWiden => {
                formatter.write_str("goal achievement would widen automated authority")
            }
        }
    }
}

impl std::error::Error for GoalRefusal {}

/// A model request has one explicit operation and never replaces an active goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GoalRequest {
    Create { objective: String },
    Cancel { goal_id: GoalId },
}

impl GoalRequest {
    pub(crate) fn parse(value: serde_json::Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            operation: Operation,
            objective: Option<String>,
            goal_id: Option<GoalId>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Operation {
            Create,
            Cancel,
        }
        let args: Arguments = serde_json::from_value(value).map_err(|error| error.to_string())?;
        match (args.operation, args.objective, args.goal_id) {
            (Operation::Create, Some(objective), None) if !objective.trim().is_empty() => {
                Ok(Self::Create { objective })
            }
            (Operation::Cancel, None, Some(goal_id)) => Ok(Self::Cancel { goal_id }),
            _ => Err(
                "create requires only a nonempty objective; cancel requires only goal_id".into(),
            ),
        }
    }
}

pub(crate) fn descriptor() -> crate::generator::types::ToolDescriptor {
    crate::generator::types::ToolDescriptor {
        name: "goal".into(),
        description: "Create an automatic goal only for a requested work-and-verification cycle, not for simple answers. Use operation=create with objective; record the outcome and verification, not a worklist. The result includes its goal_id. Use operation=cancel with that exact goal_id to end an automatic (model-owned) goal without claiming achievement. Operator goals cannot be cancelled by the model. Cancellation preserves history and worklist and does not kill tools or children. Never replace an active goal or rewrite its text or ownership. Automatic work pauses after 20 continuation turns until the operator resumes it. One blocked item does not block the goal while independent authorized work remains.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["operation"],
            "additionalProperties": false,
            "properties": {
                "operation": {"type": "string", "enum": ["create", "cancel"]},
                "objective": {"type": ["string", "null"], "minLength": 1},
                "goal_id": {"type": ["string", "null"], "description": "Exact current goal identity from the active-goal block or creation result."}
            }
        }),
    }
}

/// Immutable goal facts plus the current blocker audit projected by replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRecord {
    pub id: GoalId,
    pub objective: String,
    pub owner: GoalOwner,
    pub ceilings: Ceilings,
    pub derived_from: Option<GoalId>,
    pub created_seq: Seq,
    pub state: GoalState,
    #[serde(default)]
    pub continuation_turns: u32,
    #[serde(default)]
    pub continuation_budget: Option<u32>,
    #[serde(default)]
    pub check_stop: Option<String>,
    pub blocker_reason: Option<String>,
    pub blocker_observations: u64,
    pub achieved_by: Option<GoalOwner>,
    pub achievement_evidence: Option<EvidenceRef>,
    /// An operator stop pauses automatic work until a new user request or an
    /// explicit operator goal. Replay derives it, so resume and compaction keep it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub automatic_work_paused: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) blocker_turns: Vec<TurnId>,
}

impl GoalRecord {
    pub(crate) fn admits_cancellation_by(&self, actor: GoalOwner) -> Result<(), GoalRefusal> {
        if actor == GoalOwner::Operator
            || (actor == GoalOwner::Automated && self.owner == GoalOwner::Automated)
        {
            Ok(())
        } else {
            Err(GoalRefusal::CancellationWouldWiden)
        }
    }

    /// The first check may honor a smaller operator limit, never widen an automatic one.
    pub(crate) fn may_configure_allowance(&self, turns: u32) -> bool {
        self.continuation_budget.is_none()
            || (self.owner == GoalOwner::Automated
                && self.continuation_turns == 0
                && self
                    .continuation_budget
                    .is_some_and(|current| turns < current))
    }

    /// Check whether this record may accept a transition without widening its authority.
    pub fn admits(&self, transition: &GoalTransition) -> Result<(), GoalRefusal> {
        if self.state != GoalState::Active {
            return Err(GoalRefusal::AlreadyTerminal(self.state));
        }
        if let GoalTransition::Cancelled { cancelled_by } = transition {
            self.admits_cancellation_by(*cancelled_by)?;
        }
        if let GoalTransition::AllowanceExtended { turns } = transition
            && (*turns == 0
                || self
                    .continuation_budget
                    .unwrap_or(20) // Legacy extension events replay against the historical allowance.
                    .checked_add(*turns)
                    .is_none())
        {
            return Err(GoalRefusal::InvalidExtension);
        }
        if let GoalTransition::Achieved {
            attested_by,
            evidence,
        } = transition
            && *attested_by != GoalOwner::Operator
        {
            if *attested_by == GoalOwner::Automated && self.owner != GoalOwner::Automated {
                return Err(GoalRefusal::AttestationWouldWiden);
            }
            if evidence.is_none() {
                return Err(GoalRefusal::EvidenceRequired);
            }
        }
        Ok(())
    }

    pub(crate) fn observe_blocker(&mut self, turn: TurnId, reason: &str) {
        if self.state != GoalState::Active {
            return;
        }

        let normalized = normalize_reason(reason);
        let same_condition = self
            .blocker_reason
            .as_deref()
            .is_some_and(|current| normalize_reason(current) == normalized);
        if !same_condition {
            self.blocker_reason = Some(reason.to_owned());
            self.blocker_turns.clear();
        }
        if self.blocker_turns.contains(&turn) {
            return;
        }

        self.blocker_turns.push(turn);
        self.blocker_observations = self.blocker_turns.len() as u64;
        if self.blocker_observations >= BLOCKER_OBSERVATION_LIMIT {
            self.state = GoalState::Blocked;
        }
    }

    pub(crate) fn clear_blocker_after_completed_turn(&mut self, turn: TurnId) {
        if self.state == GoalState::Active
            && self
                .blocker_turns
                .last()
                .is_some_and(|observed| *observed != turn)
        {
            self.blocker_reason = None;
            self.blocker_observations = 0;
            self.blocker_turns.clear();
        }
    }
}

fn normalize_reason(reason: &str) -> String {
    let mut normalized = String::new();
    for word in reason.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.extend(word.chars().flat_map(char::to_lowercase));
    }
    normalized
}
