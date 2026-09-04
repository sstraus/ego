//! Connection-neutral policy for durable session catalogue and activation.

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::ceiling::Ceilings;
use crate::event::domain::{DomainEvent, Envelope, EventKind};
use crate::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use crate::event::time::{Clock, Timestamp};
use crate::goal::{GoalOwner, GoalRecord, GoalRefusal, GoalState, GoalTransition};
use crate::public::{PublicEvent, project_public};
use crate::session::{
    ForkError, PreparedFork, ReplayContext, ReplayError, ReplaySource, ReplaySourceError,
    SessionBlobSource, TargetPublication, TargetSeed,
};
use crate::state::SessionState;
use crate::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};

/// Publication at a lifecycle boundary. Visibility, not a later cache write,
/// decides whether retrying could create a second transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecyclePublication {
    NotPublished { diagnostic: String },
    Published,
    PublishedDurabilityUncertain { diagnostic: String },
}

impl LifecyclePublication {
    pub const fn is_visible(&self) -> bool {
        !matches!(self, Self::NotPublished { .. })
    }

    pub const fn retry_safe(&self) -> bool {
        matches!(self, Self::NotPublished { .. })
    }
}

impl From<TargetPublication> for LifecyclePublication {
    fn from(value: TargetPublication) -> Self {
        match value {
            TargetPublication::NotPublished { diagnostic } => Self::NotPublished { diagnostic },
            TargetPublication::PublishedDurably { .. } => Self::Published,
            TargetPublication::PublishedDurabilityUncertain { diagnostic } => {
                Self::PublishedDurabilityUncertain { diagnostic }
            }
        }
    }
}

/// Atomic effects the existing append/read store does not express.
pub trait SessionLifecycleStore:
    SessionStore + ReplaySource + SessionBlobSource + Send + Sync
{
    fn publish_empty_session(
        &self,
        id: SessionId,
        root: &Path,
        created: Timestamp,
    ) -> Result<LifecyclePublication, StoreError>;

    fn compare_and_append_lifecycle(
        &self,
        id: SessionId,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError>;

    fn lifecycle_meta(&self, id: SessionId) -> Result<Option<SessionMeta>, StoreError>;

    fn acquire_inactive_lease(
        &self,
        id: SessionId,
    ) -> Result<Box<dyn InactiveSessionLease>, LifecycleAppendError>;

    fn publish_fork(&self, id: SessionId, seed: &TargetSeed) -> TargetPublication;
}

/// Exclusive proof that no session writer can attach until this value drops.
pub trait InactiveSessionLease: Send {}

#[derive(Debug)]
pub enum LifecycleAppendError {
    Conflict,
    LeaseHeld,
    Store(StoreError),
}

impl fmt::Display for LifecycleAppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => formatter.write_str("session changed concurrently"),
            Self::LeaseHeld => formatter.write_str("session has a live lease"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LifecycleAppendError {}

impl From<StoreError> for LifecycleAppendError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

/// Durable activity as observed by the one rooted run controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActivity {
    Inactive,
    Active,
    LeaseHeld,
    Uncertain,
}

pub type CloseFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ClosedSession, LifecycleError>> + Send + 'a>>;
pub type ActivityFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SessionActivity, LifecycleError>> + Send + 'a>>;

/// Narrow live-effect seam. Implementations delegate to `RunController` and
/// never keep a second durable run state.
pub trait SessionRunLifecycle: Send + Sync {
    fn activity(&self, session: SessionId) -> ActivityFuture<'_>;
    fn cancel_and_settle(&self, session: SessionId) -> CloseFuture<'_>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub id: SessionId,
    pub publication: LifecyclePublication,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedSession {
    pub id: SessionId,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayFrame {
    pub event: PublicEvent,
    pub state: SessionState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttachment {
    pub id: SessionId,
    pub root: PathBuf,
    pub replay: Vec<ReplayFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPage {
    pub items: Vec<SessionMeta>,
    pub next_cursor: Option<SessionCursor>,
}

/// Opaque, restart-safe keyset cursor bound to the exact root filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCursor(String);

#[derive(Serialize, Deserialize)]
struct CursorWire {
    v: u8,
    root: Option<PathBuf>,
    created: Timestamp,
    id: SessionId,
}

impl SessionCursor {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, LifecycleError> {
        let cursor = Self(value.into());
        cursor.decode()?;
        Ok(cursor)
    }

    fn new(root: Option<&Path>, meta: &SessionMeta) -> Self {
        let bytes = serde_json::to_vec(&CursorWire {
            v: 1,
            root: root.map(Path::to_path_buf),
            created: meta.created,
            id: meta.id,
        })
        .expect("the closed cursor shape serializes");
        Self(hex_encode(&bytes))
    }

    fn decode(&self) -> Result<CursorWire, LifecycleError> {
        let bytes = hex_decode(&self.0).ok_or(LifecycleError::InvalidCursor)?;
        let wire: CursorWire =
            serde_json::from_slice(&bytes).map_err(|_| LifecycleError::InvalidCursor)?;
        if wire.v != 1 {
            return Err(LifecycleError::InvalidCursor);
        }
        Ok(wire)
    }
}

impl fmt::Display for SessionCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

#[derive(Debug)]
pub enum LifecycleError {
    UnknownSession,
    Deleted,
    InvalidCursor,
    InvalidLimit,
    RootMismatch { stored: PathBuf, requested: PathBuf },
    Active,
    LeaseHeld,
    EffectUncertain,
    Conflict,
    Goal(GoalRefusal),
    InvalidText,
    Fork(ForkError),
    Ancestry(ReplayError),
    Replay(ReplaySourceError),
    Store(StoreError),
    Run(String),
    NotPublished(String),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSession => formatter.write_str("session not found"),
            Self::Deleted => formatter.write_str("session was deleted"),
            Self::InvalidCursor => formatter.write_str("invalid session cursor"),
            Self::InvalidLimit => formatter.write_str("session page limit must be non-zero"),
            Self::RootMismatch { stored, requested } => write!(
                formatter,
                "session belongs to {}, not {}",
                stored.display(),
                requested.display()
            ),
            Self::Active => formatter.write_str("session has active work"),
            Self::LeaseHeld => formatter.write_str("session has a live lease"),
            Self::EffectUncertain => formatter.write_str("session has an uncertain effect"),
            Self::Conflict => formatter.write_str("session changed concurrently"),
            Self::Goal(error) => error.fmt(formatter),
            Self::InvalidText => formatter.write_str("invalid operator text"),
            Self::Fork(error) => error.fmt(formatter),
            Self::Ancestry(error) => error.fmt(formatter),
            Self::Replay(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Run(diagnostic) => formatter.write_str(diagnostic),
            Self::NotPublished(diagnostic) => formatter.write_str(diagnostic),
        }
    }
}

impl std::error::Error for LifecycleError {}
impl From<StoreError> for LifecycleError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<ReplaySourceError> for LifecycleError {
    fn from(value: ReplaySourceError) -> Self {
        Self::Replay(value)
    }
}
impl From<ForkError> for LifecycleError {
    fn from(value: ForkError) -> Self {
        Self::Fork(value)
    }
}
impl From<ReplayError> for LifecycleError {
    fn from(value: ReplayError) -> Self {
        Self::Ancestry(value)
    }
}
impl From<LifecycleAppendError> for LifecycleError {
    fn from(value: LifecycleAppendError) -> Self {
        match value {
            LifecycleAppendError::Conflict => Self::Conflict,
            LifecycleAppendError::LeaseHeld => Self::LeaseHeld,
            LifecycleAppendError::Store(error) => Self::Store(error),
        }
    }
}

impl From<GoalRefusal> for LifecycleError {
    fn from(value: GoalRefusal) -> Self {
        Self::Goal(value)
    }
}

/// One shared lifecycle service. Durable facts are always reconstructed from
/// the store; the run collaborator owns only live effect coordination.
pub struct SessionLifecycle<S> {
    store: Arc<S>,
    runs: Arc<dyn SessionRunLifecycle>,
    clock: Arc<dyn Clock>,
}

impl<S> SessionLifecycle<S>
where
    S: SessionLifecycleStore,
{
    pub fn new(store: Arc<S>, runs: Arc<dyn SessionRunLifecycle>, clock: Arc<dyn Clock>) -> Self {
        Self { store, runs, clock }
    }

    pub fn create(&self, root: PathBuf) -> Result<CreatedSession, LifecycleError> {
        self.create_with_id(SessionId::new(), root)
    }

    pub fn create_with_id(
        &self,
        id: SessionId,
        root: PathBuf,
    ) -> Result<CreatedSession, LifecycleError> {
        let publication = self
            .store
            .publish_empty_session(id, &root, self.clock.now())?;
        if let LifecyclePublication::NotPublished { diagnostic } = &publication {
            return Err(LifecycleError::NotPublished(diagnostic.clone()));
        }
        Ok(CreatedSession { id, publication })
    }

    pub fn list(
        &self,
        cursor: Option<&SessionCursor>,
        limit: usize,
        root: Option<&Path>,
    ) -> Result<SessionPage, LifecycleError> {
        let limit = NonZeroUsize::new(limit)
            .ok_or(LifecycleError::InvalidLimit)?
            .get();
        let after = cursor.map(SessionCursor::decode).transpose()?;
        if after
            .as_ref()
            .is_some_and(|value| value.root.as_deref() != root)
        {
            return Err(LifecycleError::InvalidCursor);
        }
        let mut items = self.store.list()?;
        items.retain(|meta| root.is_none_or(|root| meta.workspace_root == root));
        let mut visible = Vec::with_capacity(items.len());
        for meta in items {
            if !self.state(meta.id)?.deleted {
                visible.push(meta);
            }
        }
        let mut items = visible;
        items.sort_by(|left, right| {
            right
                .created
                .cmp(&left.created)
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some(after) = after {
            items.retain(|meta| {
                meta.created < after.created
                    || (meta.created == after.created && meta.id > after.id)
            });
        }
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = has_more.then(|| {
            SessionCursor::new(
                root,
                items.last().expect("a non-zero full page has a last row"),
            )
        });
        Ok(SessionPage { items, next_cursor })
    }

    pub fn meta(&self, id: SessionId) -> Result<SessionMeta, LifecycleError> {
        let meta = self.raw_meta(id)?;
        if self.state(id)?.deleted {
            return Err(LifecycleError::Deleted);
        }
        Ok(meta)
    }

    pub async fn load(
        &self,
        id: SessionId,
        root: &Path,
    ) -> Result<SessionAttachment, LifecycleError> {
        self.activate(id, root, true).await
    }

    pub async fn resume(
        &self,
        id: SessionId,
        root: &Path,
    ) -> Result<SessionAttachment, LifecycleError> {
        self.activate(id, root, false).await
    }

    pub async fn attach(
        &self,
        id: SessionId,
        root: &Path,
    ) -> Result<SessionAttachment, LifecycleError> {
        self.resume(id, root).await
    }

    async fn activate(
        &self,
        id: SessionId,
        root: &Path,
        replay: bool,
    ) -> Result<SessionAttachment, LifecycleError> {
        let meta = self.raw_meta(id)?;
        if meta.workspace_root != root {
            return Err(LifecycleError::RootMismatch {
                stored: meta.workspace_root,
                requested: root.to_path_buf(),
            });
        }
        let (state, frames) = self.replay(id)?;
        if state.deleted {
            return Err(LifecycleError::Deleted);
        }
        if self.runs.activity(id).await? == SessionActivity::Uncertain {
            return Err(LifecycleError::EffectUncertain);
        }
        Ok(SessionAttachment {
            id,
            root: root.to_path_buf(),
            replay: if replay { frames } else { Vec::new() },
        })
    }

    pub async fn fork(&self, source: SessionId) -> Result<CreatedSession, LifecycleError> {
        let _source_lease = self.require_inactive(source).await?;
        let meta = self.meta(source)?;
        let (state, _) = self.replay(source)?;
        let prepared = PreparedFork::new(source, &state, self.clock.now())?;
        ReplayContext::new(&*self.store, &*self.store).replay_lineage(prepared.seed().lineage())?;
        let seed = prepared
            .seed()
            .clone()
            .with_title(meta.title)
            .with_goal(state.goal.as_ref());
        let target = SessionId::new();
        let publication = self.store.publish_fork(target, &seed);
        if !publication.switch_target() {
            return Err(LifecycleError::NotPublished(
                publication
                    .diagnostic()
                    .unwrap_or("fork was not published")
                    .to_owned(),
            ));
        }
        Ok(CreatedSession {
            id: target,
            publication: publication.into(),
        })
    }

    pub fn set_title(
        &self,
        id: SessionId,
        title: Option<String>,
    ) -> Result<SessionMeta, LifecycleError> {
        let title = title.map(validate_operator_text).transpose()?;
        let current = self.meta(id)?;
        if current.title == title {
            return Ok(current);
        }
        let state = self.state(id)?;
        let event = lifecycle_event(
            state.last_seq,
            self.clock.now(),
            EventKind::SessionTitleChanged { title },
        );
        require_publication(
            self.store
                .compare_and_append_lifecycle(id, state.last_seq, event)?,
        )?;
        self.meta(id)
    }

    pub fn goal(&self, id: SessionId) -> Result<Option<GoalRecord>, LifecycleError> {
        self.meta(id)?;
        Ok(self.state(id)?.goal)
    }

    pub fn set_goal(
        &self,
        session: SessionId,
        objective: String,
        owner: GoalOwner,
        ceilings: Ceilings,
    ) -> Result<GoalRecord, LifecycleError> {
        let objective = validate_operator_text(objective)?;
        self.meta(session)?;
        let state = self.state(session)?;
        if state
            .goal
            .as_ref()
            .is_some_and(|goal| goal.state == GoalState::Active)
        {
            return Err(GoalRefusal::ActiveGoalExists.into());
        }
        let event = lifecycle_event(
            state.last_seq,
            self.clock.now(),
            EventKind::GoalSet {
                id: crate::event::id::GoalId::new(),
                objective,
                owner,
                ceilings,
                derived_from: None,
            },
        );
        require_publication(self.store.compare_and_append_lifecycle(
            session,
            state.last_seq,
            event,
        )?)?;
        self.goal(session)?
            .ok_or_else(|| GoalRefusal::NoActiveGoal.into())
    }

    pub fn transition_goal(
        &self,
        session: SessionId,
        transition: GoalTransition,
    ) -> Result<GoalRecord, LifecycleError> {
        let transition = match transition {
            GoalTransition::BlockerObserved { reason } => GoalTransition::BlockerObserved {
                reason: validate_operator_text(reason)?,
            },
            achieved @ GoalTransition::Achieved { .. } => achieved,
        };
        self.meta(session)?;
        let state = self.state(session)?;
        let goal = state.goal.as_ref().ok_or(GoalRefusal::NoActiveGoal)?;
        goal.admits(&transition)?;
        let event = lifecycle_event(
            state.last_seq,
            self.clock.now(),
            EventKind::GoalTransitioned {
                id: goal.id,
                transition,
            },
        );
        require_publication(self.store.compare_and_append_lifecycle(
            session,
            state.last_seq,
            event,
        )?)?;
        self.goal(session)?
            .ok_or_else(|| GoalRefusal::NoActiveGoal.into())
    }

    pub async fn delete(&self, id: SessionId) -> Result<LifecyclePublication, LifecycleError> {
        let state = self.state(id)?;
        if state.deleted {
            return Ok(LifecyclePublication::Published);
        }
        let _session_lease = self.require_inactive(id).await?;
        let event = lifecycle_event(state.last_seq, self.clock.now(), EventKind::SessionDeleted);
        let publication = self
            .store
            .compare_and_append_lifecycle(id, state.last_seq, event)?;
        require_publication(publication.clone())?;
        Ok(publication)
    }

    async fn require_inactive(
        &self,
        id: SessionId,
    ) -> Result<Box<dyn InactiveSessionLease>, LifecycleError> {
        let lease = self.store.acquire_inactive_lease(id)?;
        match self.runs.activity(id).await? {
            SessionActivity::Inactive => {}
            SessionActivity::LeaseHeld => return Err(LifecycleError::LeaseHeld),
            SessionActivity::Active => return Err(LifecycleError::Active),
            SessionActivity::Uncertain => return Err(LifecycleError::EffectUncertain),
        }
        Ok(lease)
    }

    pub async fn close(&self, id: SessionId) -> Result<ClosedSession, LifecycleError> {
        self.meta(id)?;
        if self.runs.activity(id).await? == SessionActivity::Uncertain {
            return Err(LifecycleError::EffectUncertain);
        }
        self.runs.cancel_and_settle(id).await
    }

    fn raw_meta(&self, id: SessionId) -> Result<SessionMeta, LifecycleError> {
        self.store
            .lifecycle_meta(id)?
            .ok_or(LifecycleError::UnknownSession)
    }

    fn state(&self, id: SessionId) -> Result<SessionState, LifecycleError> {
        Ok(self.replay(id)?.0)
    }

    fn replay(&self, id: SessionId) -> Result<(SessionState, Vec<ReplayFrame>), LifecycleError> {
        let entries = self
            .store
            .entries(&id)?
            .ok_or(LifecycleError::UnknownSession)?;
        let mut state = SessionState::default();
        let mut frames = Vec::new();
        let mut semantic = Seq::ZERO;
        for entry in entries {
            if let SessionEntry::Event(event) = entry {
                let expected = semantic.next();
                if event.env.seq != expected {
                    return Err(LifecycleError::Replay(ReplaySourceError::CorruptSession {
                        session: id,
                        reason: "semantic event sequence is not contiguous".to_owned(),
                    }));
                }
                state.apply(&event);
                if let Some(public) = project_public(&event) {
                    frames.push(ReplayFrame {
                        event: public,
                        state: state.clone(),
                    });
                }
            }
        }
        if !state.unresolved_parents.is_empty() {
            return Err(LifecycleError::Replay(ReplaySourceError::CorruptSession {
                session: id,
                reason: "event ancestry contains an unresolved parent".to_owned(),
            }));
        }
        Ok((state, frames))
    }
}

fn lifecycle_event(previous: Seq, at: Timestamp, kind: EventKind) -> DomainEvent {
    let mut seq = previous;
    seq.next();
    DomainEvent {
        env: Envelope {
            seq,
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: at,
            producer: Producer::Main,
        },
        kind,
    }
}

const OPERATOR_TEXT_MAX_BYTES: usize = 1024;

fn validate_operator_text(text: String) -> Result<String, LifecycleError> {
    let text = text.trim().to_owned();
    if text.is_empty() || text.len() > OPERATOR_TEXT_MAX_BYTES || text.chars().any(char::is_control)
    {
        return Err(LifecycleError::InvalidText);
    }
    Ok(text)
}

fn require_publication(publication: LifecyclePublication) -> Result<(), LifecycleError> {
    match publication {
        LifecyclePublication::NotPublished { diagnostic } => {
            Err(LifecycleError::NotPublished(diagnostic))
        }
        LifecyclePublication::Published
        | LifecyclePublication::PublishedDurabilityUncertain { .. } => Ok(()),
    }
}
