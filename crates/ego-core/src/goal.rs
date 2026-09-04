//! Persistent session-goal state and transitions.

use serde::{Deserialize, Serialize};

use crate::ceiling::Ceilings;
use crate::event::id::{GoalId, Seq, TurnId};
use crate::store::evidence::EvidenceRef;

/// Authority responsible for a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalOwner {
    Operator,
    Automated,
}

/// Current state of a persistent goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Active,
    Achieved,
    Blocked,
}

/// An observation that may change a goal's reduced state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalTransition {
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
    AlreadyTerminal(GoalState),
    ActiveGoalExists,
    EvidenceRequired,
    AttestationWouldWiden,
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
    pub blocker_reason: Option<String>,
    pub blocker_observations: u64,
    pub achieved_by: Option<GoalOwner>,
    pub achievement_evidence: Option<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) blocker_turns: Vec<TurnId>,
}

impl GoalRecord {
    pub fn admits(&self, transition: &GoalTransition) -> Result<(), GoalRefusal> {
        if self.state != GoalState::Active {
            return Err(GoalRefusal::AlreadyTerminal(self.state));
        }
        if let GoalTransition::Achieved {
            attested_by: GoalOwner::Automated,
            evidence,
        } = transition
        {
            if self.owner != GoalOwner::Automated {
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

        let reason = normalize_reason(reason);
        if self.blocker_reason.as_deref() != Some(reason.as_str()) {
            self.blocker_reason = Some(reason);
            self.blocker_turns.clear();
        }
        if self.blocker_turns.contains(&turn) {
            return;
        }

        self.blocker_turns.push(turn);
        self.blocker_observations = self.blocker_turns.len() as u64;
        if self.blocker_observations >= 3 {
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
