//! Local-content transcript projection. The live public/ACP schema stays unchanged.

use serde::Serialize;

use super::{PUBLIC_EVENT_VERSION, PublicEvent, project_public};
use crate::event::domain::{DomainEvent, EventKind};
use crate::event::id::{EventId, GoalId, ItemId, Seq, SessionId, TurnId};
use crate::event::time::Timestamp;
use crate::goal::{GoalOwner, GoalTransition};
use crate::session::SessionLineage;

pub const TRANSCRIPT_VERSION: u32 = 1;

/// Versioned header for exactly one session, never a flattened ancestor history.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "transcript.header")]
pub struct TranscriptHeader {
    pub v: u32,
    pub public_event_version: u32,
    pub session: SessionId,
    pub lineage: Option<SessionLineage>,
    pub compaction_predecessor: Option<SessionId>,
}

impl TranscriptHeader {
    pub fn new(session: SessionId, lineage: Option<SessionLineage>) -> Self {
        Self {
            v: TRANSCRIPT_VERSION,
            public_event_version: PUBLIC_EVENT_VERSION,
            session,
            compaction_predecessor: match lineage {
                Some(SessionLineage::Compaction { source, .. }) => Some(source),
                _ => None,
            },
            lineage,
        }
    }
}

/// Durable identity wraps the existing public content, not provider-private state.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "transcript.event")]
pub struct TranscriptEvent {
    pub session: SessionId,
    pub id: EventId,
    pub seq: Seq,
    pub turn: TurnId,
    pub item: Option<ItemId>,
    pub ts: Timestamp,
    pub event: TranscriptPayload,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum TranscriptPayload {
    Public(Box<PublicEvent>),
    Goal(TranscriptGoal),
}

/// Explicit local goal content; never serialize arbitrary internal event kinds.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptGoal {
    GoalSet {
        id: GoalId,
        objective: String,
        owner: GoalOwner,
        derived_from: Option<GoalId>,
    },
    GoalUpdated {
        id: GoalId,
        objective: String,
    },
    GoalContinuation {
        id: GoalId,
        text: String,
    },
    GoalTransitioned {
        id: GoalId,
        transition: GoalTransition,
    },
}

pub fn project_transcript(session: SessionId, event: &DomainEvent) -> Option<TranscriptEvent> {
    let goal = match &event.kind {
        EventKind::GoalSet {
            id,
            objective,
            owner,
            derived_from,
            ..
        } => Some(TranscriptGoal::GoalSet {
            id: *id,
            objective: objective.clone(),
            owner: *owner,
            derived_from: *derived_from,
        }),
        EventKind::GoalUpdated { id, objective } => Some(TranscriptGoal::GoalUpdated {
            id: *id,
            objective: objective.clone(),
        }),
        EventKind::GoalContinuation { id, text } => Some(TranscriptGoal::GoalContinuation {
            id: *id,
            text: text.clone(),
        }),
        EventKind::GoalTransitioned { id, transition } => Some(TranscriptGoal::GoalTransitioned {
            id: *id,
            transition: transition.clone(),
        }),
        _ => None,
    };
    let payload = match goal {
        Some(goal) => TranscriptPayload::Goal(goal),
        None => TranscriptPayload::Public(Box::new(project_public(event)?)),
    };
    Some(TranscriptEvent {
        session,
        id: event.env.id,
        seq: event.env.seq,
        turn: event.env.turn,
        item: event.env.item,
        ts: event.env.ts,
        event: payload,
    })
}
