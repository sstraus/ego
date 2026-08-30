//! Durable session lineage, ancestor conversation replay, and target publication.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::digest::sha256_hex;
use crate::event::domain::{DomainEvent, Envelope, EventKind};
use crate::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use crate::event::time::Timestamp;
use crate::generator::history::conversation_from_with_blob_reader;
use crate::generator::types::Conversation;
use crate::state::SessionState;
use crate::store::blob::{BlobError, BlobRef};
use crate::store::session_store::{SessionEntry, SessionStore};

/// The durable ancestry of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionLineage {
    Fork {
        source: SessionId,
        at_seq: Seq,
    },
    Compaction {
        source: SessionId,
        source_seq: Seq,
        prompt_revision: u32,
    },
}

/// Failures produced by the durable replay adapter, before core policy is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaySourceError {
    CorruptSession { session: SessionId, reason: String },
    UnknownEntry { session: SessionId, position: Seq },
    Unavailable { session: SessionId, reason: String },
}

impl fmt::Display for ReplaySourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CorruptSession { session, reason } => {
                write!(formatter, "session {session} is corrupt: {reason}")
            }
            Self::UnknownEntry { session, position } => {
                write!(
                    formatter,
                    "session {session} has an unknown entry at {position:?}"
                )
            }
            Self::Unavailable { session, reason } => {
                write!(formatter, "session {session} could not be read: {reason}")
            }
        }
    }
}

impl std::error::Error for ReplaySourceError {}

/// Consumer-owned access to authoritative session lineage and entries.
pub trait ReplaySource: Send + Sync {
    fn lineage(&self, session: &SessionId) -> Result<Option<SessionLineage>, ReplaySourceError>;

    fn entries(&self, session: &SessionId) -> Result<Option<Vec<SessionEntry>>, ReplaySourceError>;
}

/// Consumer-owned access to a blob qualified by the session that owns it.
pub trait SessionBlobSource: Send + Sync {
    fn read(&self, session: &SessionId, reference: &BlobRef) -> Result<Vec<u8>, BlobError>;
}

/// A refusal to construct trusted ancestor conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    MissingSession {
        session: SessionId,
    },
    CorruptSession {
        session: SessionId,
        reason: String,
    },
    UnknownEntry {
        session: SessionId,
        position: Seq,
    },
    MissingBlob {
        session: SessionId,
        reference: BlobRef,
    },
    CorruptBlob {
        session: SessionId,
        reference: BlobRef,
        reason: String,
    },
    Cycle {
        session: SessionId,
    },
    InvalidConversation {
        reason: String,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSession { session } => write!(formatter, "session {session} is missing"),
            Self::CorruptSession { session, reason } => {
                write!(formatter, "session {session} is corrupt: {reason}")
            }
            Self::UnknownEntry { session, position } => {
                write!(
                    formatter,
                    "session {session} has an unknown entry at {position:?}"
                )
            }
            Self::MissingBlob { session, reference } => {
                write!(formatter, "session {session} is missing blob {reference}")
            }
            Self::CorruptBlob {
                session,
                reference,
                reason,
            } => write!(
                formatter,
                "session {session} has corrupt blob {reference}: {reason}"
            ),
            Self::Cycle { session } => {
                write!(formatter, "session lineage cycles through {session}")
            }
            Self::InvalidConversation { reason } => {
                write!(formatter, "ancestor conversation is invalid: {reason}")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

pub(crate) fn map_source(error: ReplaySourceError) -> ReplayError {
    match error {
        ReplaySourceError::CorruptSession { session, reason }
        | ReplaySourceError::Unavailable { session, reason } => {
            ReplayError::CorruptSession { session, reason }
        }
        ReplaySourceError::UnknownEntry { session, position } => {
            ReplayError::UnknownEntry { session, position }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineageEdges {
    Forks,
    ForksAndCompactions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineageStep {
    pub(crate) source: SessionId,
    pub(crate) edge: SessionLineage,
    pub(crate) lineage: Option<SessionLineage>,
}

pub(crate) fn walk_lineage<S: ReplaySource + ?Sized>(
    source: &S,
    mut edge: SessionLineage,
    visited: &mut HashSet<SessionId>,
    edges: LineageEdges,
) -> Result<Vec<LineageStep>, ReplayError> {
    let mut steps = Vec::new();
    loop {
        if edges == LineageEdges::Forks && !matches!(edge, SessionLineage::Fork { .. }) {
            break;
        }
        let ancestor = match edge {
            SessionLineage::Fork { source, .. } | SessionLineage::Compaction { source, .. } => {
                source
            }
        };
        if !visited.insert(ancestor) {
            return Err(ReplayError::Cycle { session: ancestor });
        }
        let lineage = source.lineage(&ancestor).map_err(map_source)?;
        steps.push(LineageStep {
            source: ancestor,
            edge,
            lineage,
        });
        let Some(next) = lineage else {
            break;
        };
        edge = next;
    }
    Ok(steps)
}

pub(crate) fn read_verified_blob<B: SessionBlobSource + ?Sized>(
    blobs: &B,
    session: SessionId,
    reference: &BlobRef,
) -> Result<Vec<u8>, ReplayError> {
    let bytes = blobs
        .read(&session, reference)
        .map_err(|error| match error {
            BlobError::NotFound { .. } => ReplayError::MissingBlob {
                session,
                reference: reference.clone(),
            },
            other => ReplayError::CorruptBlob {
                session,
                reference: reference.clone(),
                reason: other.to_string(),
            },
        })?;
    if bytes.len() as u64 != reference.bytes || sha256_hex(&bytes) != reference.id.as_str() {
        return Err(ReplayError::CorruptBlob {
            session,
            reference: reference.clone(),
            reason: "content identity or length does not match its reference".to_string(),
        });
    }
    Ok(bytes)
}

/// The ancestor-only result of a lineage replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedContext {
    conversation: Conversation,
    session_pins: Vec<SessionId>,
    blob_pins: Vec<BlobRef>,
}

impl ReplayedContext {
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub fn session_pins(&self) -> &[SessionId] {
        &self.session_pins
    }

    pub fn blob_pins(&self) -> &[BlobRef] {
        &self.blob_pins
    }
}

/// Resolve durable lineage into provider conversation without mutating target state.
pub struct ReplayContext<'a> {
    source: &'a dyn ReplaySource,
    blobs: &'a dyn SessionBlobSource,
}

impl<'a> ReplayContext<'a> {
    pub fn new(source: &'a dyn ReplaySource, blobs: &'a dyn SessionBlobSource) -> Self {
        Self { source, blobs }
    }

    pub fn replay(&self, target: &SessionId) -> Result<ReplayedContext, ReplayError> {
        let Some(lineage) = self.source.lineage(target).map_err(map_source)? else {
            return Ok(ReplayedContext {
                conversation: Conversation::new(Vec::new())
                    .expect("an empty conversation is valid"),
                session_pins: Vec::new(),
                blob_pins: Vec::new(),
            });
        };

        let mut visited = HashSet::from([*target]);
        self.replay_from(lineage, &mut visited)
    }

    /// Resolve a lineage fact before its target is published.
    ///
    /// Fork creation uses this to prove the entire referenced prefix before
    /// the atomic publication boundary. The lineage value is the same opaque
    /// fact carried by [`TargetSeed`]; no target-local state is involved.
    pub fn replay_lineage(&self, lineage: SessionLineage) -> Result<ReplayedContext, ReplayError> {
        self.replay_from(lineage, &mut HashSet::new())
    }

    fn replay_from(
        &self,
        lineage: SessionLineage,
        visited: &mut HashSet<SessionId>,
    ) -> Result<ReplayedContext, ReplayError> {
        let steps = walk_lineage(self.source, lineage, visited, LineageEdges::Forks)?;
        let session_pins = steps.iter().map(|step| step.source).collect();
        let frames: Vec<_> = steps
            .into_iter()
            .filter_map(|step| match step.edge {
                SessionLineage::Fork { at_seq, .. } => Some((step.source, at_seq)),
                SessionLineage::Compaction { .. } => None,
            })
            .collect();

        let mut replay_seq = Seq::ZERO;
        let mut events = Vec::new();
        let mut blob_pins = Vec::new();
        let mut verified_blobs: HashMap<BlobRef, Vec<u8>> = HashMap::new();
        for (session, cutoff) in frames.into_iter().rev() {
            let entries = self
                .source
                .entries(&session)
                .map_err(map_source)?
                .ok_or(ReplayError::MissingSession { session })?;
            for entry in entries {
                let SessionEntry::Event(mut event) = entry else {
                    continue;
                };
                if event.env.seq > cutoff {
                    continue;
                }

                for reference in event.kind.blob_references() {
                    let bytes = read_verified_blob(self.blobs, session, &reference)?;
                    if !blob_pins.contains(&reference) {
                        blob_pins.push(reference.clone());
                    }
                    verified_blobs.insert(reference, bytes);
                }

                match &mut event.kind {
                    EventKind::PromptQueued { images, .. }
                    | EventKind::UserPrompt { images, .. } => {
                        for image in images.iter() {
                            let reference = image.reference();
                            let bytes =
                                verified_blobs.get(reference).cloned().ok_or_else(|| {
                                    ReplayError::MissingBlob {
                                        session,
                                        reference: reference.clone(),
                                    }
                                })?;
                            image
                                .hydrate(bytes)
                                .map_err(|error| ReplayError::CorruptBlob {
                                    session,
                                    reference: reference.clone(),
                                    reason: error.to_string(),
                                })?;
                        }
                    }
                    _ => {}
                }

                event.env.seq = replay_seq.next();
                events.push(event);
            }
        }

        let state = SessionState::replay(events);
        let (conversation, repairs) = conversation_from_with_blob_reader(&state, &|reference| {
            verified_blobs
                .get(reference)
                .cloned()
                .ok_or_else(|| BlobError::NotFound {
                    reference: reference.id.clone(),
                })
        })
        .map_err(|error| ReplayError::InvalidConversation {
            reason: error.to_string(),
        })?;
        if !repairs.is_empty() {
            return Err(ReplayError::InvalidConversation {
                reason: format!("ancestor conversation required repair: {repairs:?}"),
            });
        }
        Ok(ReplayedContext {
            conversation,
            session_pins,
            blob_pins,
        })
    }
}

/// One canonical sequence-1 event and its tagged lineage for a new target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSeed {
    event: DomainEvent,
    lineage: SessionLineage,
}

impl TargetSeed {
    pub fn fork(source: SessionId, at_seq: Seq, at: Timestamp) -> Self {
        let mut seq = Seq::ZERO;
        seq.next();
        Self {
            event: DomainEvent {
                env: Envelope {
                    seq,
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: None,
                    parent: None,
                    ts: at,
                    producer: Producer::Main,
                },
                kind: EventKind::Forked { source, at_seq },
            },
            lineage: SessionLineage::Fork { source, at_seq },
        }
    }

    /// Build the canonical sequence-one checkpoint seed consumed by a target
    /// store. Ordinary callers obtain this from `PreparedCompaction`; the
    /// constructor also lets outer adapters prove their publication contract
    /// without fabricating serialized records.
    pub fn compaction(
        source: SessionId,
        source_seq: Seq,
        prompt_revision: u32,
        checkpoint: String,
        at: Timestamp,
    ) -> Self {
        let mut seq = Seq::ZERO;
        seq.next();
        Self {
            event: DomainEvent {
                env: Envelope {
                    seq,
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: Some(ItemId::new()),
                    parent: None,
                    ts: at,
                    producer: Producer::Main,
                },
                kind: EventKind::CompactionSeeded {
                    source,
                    source_seq,
                    prompt_revision,
                    checkpoint,
                },
            },
            lineage: SessionLineage::Compaction {
                source,
                source_seq,
                prompt_revision,
            },
        }
    }

    pub fn event(&self) -> &DomainEvent {
        &self.event
    }

    pub fn lineage(&self) -> SessionLineage {
        self.lineage
    }
}

/// Publication outcome at the atomic target-log boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetPublication {
    NotPublished { diagnostic: String },
    PublishedDurably { diagnostic: Option<String> },
    PublishedDurabilityUncertain { diagnostic: String },
}

impl TargetPublication {
    pub fn switch_target(&self) -> bool {
        !matches!(self, Self::NotPublished { .. })
    }

    pub fn retry_safe(&self) -> bool {
        matches!(self, Self::NotPublished { .. })
    }

    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::NotPublished { diagnostic }
            | Self::PublishedDurabilityUncertain { diagnostic } => Some(diagnostic),
            Self::PublishedDurably { diagnostic } => diagnostic.as_deref(),
        }
    }
}

/// Consumer-owned target publication boundary, implemented by a lease-held store.
pub trait TargetStore: SessionStore {
    fn publish_target(&self, seed: &TargetSeed) -> TargetPublication;
}

/// An idle source bound to one canonical fork seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFork {
    seed: TargetSeed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkError {
    AcceptedPrompt,
    PendingQuestion,
    LiveAttempt,
}

impl fmt::Display for ForkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AcceptedPrompt => "the source session has accepted prompt work",
            Self::PendingQuestion => "the source session has a pending question",
            Self::LiveAttempt => "the source session has a live generation attempt",
        })
    }
}

impl std::error::Error for ForkError {}

impl PreparedFork {
    pub fn new(source: SessionId, state: &SessionState, at: Timestamp) -> Result<Self, ForkError> {
        if !state.unacknowledged_prompts().is_empty() {
            return Err(ForkError::AcceptedPrompt);
        }
        if state.pending_question().is_some() {
            return Err(ForkError::PendingQuestion);
        }
        if !state.live_attempts.is_empty() {
            return Err(ForkError::LiveAttempt);
        }
        Ok(Self {
            seed: TargetSeed::fork(source, state.last_seq, at),
        })
    }

    pub fn seed(&self) -> &TargetSeed {
        &self.seed
    }

    pub fn publish(&self, store: &dyn TargetStore) -> TargetPublication {
        store.publish_target(&self.seed)
    }
}
