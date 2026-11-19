//! Child identity and closed outcome without private task or result content.

use serde::{Deserialize, Serialize};

use super::PublicEvent;
use crate::event::domain::{DomainEvent, EventKind, SubagentTerminal};
use crate::event::id::{SubagentInvocationId, TurnId};
use crate::run::{RunId, RunSettlement};

/// Native execution identity; foreground tasks do not own durable runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChildIdentity {
    Durable {
        invocation: SubagentInvocationId,
        run: RunId,
    },
    Foreground {
        turn: TurnId,
    },
}

/// A closed child outcome without private task or diagnostic text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildOutcome {
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

/// Admission and settlement of an exact child run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ChildPhase {
    Started,
    Settled { outcome: ChildOutcome },
}

/// Project the additional admitted-child event without replacing its parent item.
pub fn project_child_lifecycle(event: &DomainEvent) -> Option<PublicEvent> {
    let (invocation, child, phase) = match &event.kind {
        EventKind::SubagentSpawned { invocation, child } => {
            (*invocation, *child, ChildPhase::Started)
        }
        EventKind::SubagentSettled {
            invocation,
            child: Some(child),
            terminal,
        } => {
            let outcome = match terminal {
                SubagentTerminal::Returned { .. } => ChildOutcome::Completed,
                SubagentTerminal::Failed {
                    settlement: RunSettlement::Cancelled { .. },
                } => ChildOutcome::Interrupted,
                SubagentTerminal::Failed {
                    settlement: RunSettlement::CancelledBeforeStart,
                } => ChildOutcome::Cancelled,
                _ => ChildOutcome::Failed,
            };
            (*invocation, *child, ChildPhase::Settled { outcome })
        }
        _ => return None,
    };
    Some(PublicEvent::ChildLifecycle {
        turn: event.env.turn,
        item: event.env.item?,
        child: ChildIdentity::Durable {
            invocation,
            run: child,
        },
        phase,
    })
}
