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
use crate::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use crate::event::time::{Clock, Timestamp};
use crate::evidence::{EvidenceLogSource, ManifestEvidence, ManifestEvidenceError};
use crate::goal::{GoalOwner, GoalRecord, GoalRefusal, GoalState, GoalTransition};
use crate::public::{PublicEvent, project_public};
use crate::run::RequestId;
use crate::session::{
    ForkError, LineageEdges, PreparedFork, ReplayContext, ReplayError, ReplaySource,
    ReplaySourceError, SessionBlobSource, SessionLineage, TargetPublication, TargetSeed,
    walk_lineage,
};
use crate::state::{HoldReceipts, PauseState, SessionState};
use crate::store::session_store::{
    SessionEntry, SessionMeta, SessionProfiles, SessionStore, StoreError,
};
use crate::turn::{Journal, SharedJournal, SharedJournalError};

/// Publication at a lifecycle boundary. Visibility, not a later cache write,
/// decides whether retrying could create a second transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecyclePublication {
    NotPublished { diagnostic: String },
    Published,
    PublishedWithProjectionWarning { diagnostic: String },
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
            TargetPublication::PublishedDurably { diagnostic: None } => Self::Published,
            TargetPublication::PublishedDurably {
                diagnostic: Some(diagnostic),
            } => Self::PublishedWithProjectionWarning { diagnostic },
            TargetPublication::PublishedDurabilityUncertain { diagnostic } => {
                Self::PublishedDurabilityUncertain { diagnostic }
            }
        }
    }
}

/// Atomic effects the existing append/read store does not express.
pub trait SessionLifecycleStore:
    SessionStore + ReplaySource + SessionBlobSource + EvidenceLogSource + Send + Sync
{
    fn publish_empty_session(
        &self,
        id: SessionId,
        root: &Path,
        created: Timestamp,
    ) -> Result<LifecyclePublication, StoreError> {
        self.publish_session(id, root, created, SessionProfiles::default())
    }

    fn publish_session(
        &self,
        id: SessionId,
        root: &Path,
        created: Timestamp,
        profiles: SessionProfiles,
    ) -> Result<LifecyclePublication, StoreError>;

    fn lifecycle_meta(&self, id: SessionId) -> Result<Option<SessionMeta>, StoreError>;

    fn acquire_inactive_lease(
        &self,
        id: SessionId,
    ) -> Result<Box<dyn InactiveSessionLease + '_>, LifecycleAppendError>;

    fn publish_fork(&self, id: SessionId, seed: &TargetSeed) -> TargetPublication;

    /// Hide a session from every lifecycle operation, durably and without its
    /// writer lease. For a session a request created and then refused while
    /// another writer holds it, so neither a delete nor an appended event can
    /// reach it. Idempotent. The log is left as it is: a later delete may still
    /// remove it.
    fn quarantine(&self, id: SessionId) -> Result<(), StoreError>;

    fn is_quarantined(&self, id: SessionId) -> Result<bool, StoreError>;
}

/// Session-bound authority to append a lifecycle event while its writer lease is held.
///
/// A bare lifecycle store deliberately cannot implement this port. The session id
/// comes from the capability so a caller cannot use one session's lease to mutate
/// another session.
pub trait LeasedLifecycleAppend: Send + Sync {
    fn session_id(&self) -> SessionId;

    fn compare_and_append_lifecycle(
        &self,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError>;
}

/// The session's single journal shared by ordinary and lifecycle appends.
pub struct LeasedSharedJournal<'s, S>
where
    S: SessionStore + LeasedLifecycleAppend,
{
    append: &'s S,
    journal: SharedJournal<'s>,
}

impl<'s, S> LeasedSharedJournal<'s, S>
where
    S: SessionStore + LeasedLifecycleAppend,
{
    pub fn new(
        append: &'s S,
        clock: &'s dyn Clock,
        sink: &'s dyn crate::sink::EventSink,
        producer: Producer,
        state: SessionState,
    ) -> Self {
        let journal = Journal::new(append.session_id(), append, clock, sink, producer, state);
        Self {
            append,
            journal: SharedJournal::new(journal),
        }
    }

    pub fn shared(&self) -> SharedJournal<'s> {
        self.journal.clone()
    }

    pub fn try_into_state(self) -> Result<SessionState, SharedJournalError> {
        self.journal.try_into_state()
    }
}

impl<S> LeasedLifecycleAppend for LeasedSharedJournal<'_, S>
where
    S: SessionStore + LeasedLifecycleAppend,
{
    fn session_id(&self) -> SessionId {
        self.append.session_id()
    }

    fn compare_and_append_lifecycle(
        &self,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        self.journal
            .compare_and_append_lifecycle(self.append, expected, event)
    }
}

/// Exclusive, session-bound authority to append while no writer can attach.
pub trait InactiveSessionLease: LeasedLifecycleAppend {}

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

/// What a session's hold is, as a host can observe it.
///
/// Three values and not a boolean, because the middle one is the whole
/// reason `docs/decisions/2026-09-03-acp-host-control-is-negotiated-per-
/// session.md` distinguishes a request from a boundary: a host told `Paused`
/// may assume no further effect will happen, and a host told `Pending` may
/// not. Collapsing them would let a client act on a hold that has not landed.
///
/// Beside [`SessionActivity`] and not folded into it. Activity is about the
/// run — is a turn in flight — and this is about the session: a session can
/// be inactive because it finished and inactive because it is being held,
/// and only one of the two will start work when asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldState {
    /// Nothing is holding this session.
    Running,
    /// A pause is recorded and the turn has not reached a boundary yet.
    Pending,
    /// The hold is durable: the session stopped and nothing more runs under
    /// it until it is released.
    Paused,
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
pub type HoldSignalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, LifecycleError>> + Send + 'a>>;

/// Narrow live-effect seam. Implementations delegate to `RunController` and
/// never keep a second durable run state.
pub trait SessionRunLifecycle: Send + Sync {
    fn activity(&self, session: SessionId) -> ActivityFuture<'_>;
    fn cancel_and_settle(&self, session: SessionId) -> CloseFuture<'_>;

    /// Deliver a pause to the turn this session is running, if it is running
    /// one. Answers whether there was a turn to deliver it to.
    ///
    /// Beside `cancel_and_settle` and deliberately not awaiting the outcome:
    /// closing a session waits for the effect it interrupted to settle,
    /// because the caller is about to stop caring about the session. A pause
    /// waits for nothing — the boundary lands in the log, and the caller
    /// reads it there.
    fn hold(&self, session: SessionId, request: RequestId) -> HoldSignalFuture<'_>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub id: SessionId,
    pub publication: LifecyclePublication,
}

/// A lifecycle value together with the publication outcome that made it current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleMutation<T> {
    pub value: T,
    pub publication: Option<LifecyclePublication>,
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
    pub inherited: bool,
}

/// Transient catalogue facts reconstructed from durable lineage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLineageInfo {
    pub lineage: SessionLineage,
    pub root: SessionId,
    pub at_message: Option<ItemId>,
    pub source_deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttachment {
    pub id: SessionId,
    pub root: PathBuf,
    pub meta: SessionMeta,
    pub context_window: Option<u64>,
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
    RootMismatch {
        stored: PathBuf,
        requested: PathBuf,
    },
    Active,
    LeaseHeld,
    EffectUncertain,
    Held,
    Conflict,
    Goal(GoalRefusal),
    /// A resume was asked for a session that is not durably paused.
    NotPaused {
        state: HoldState,
    },
    InvalidText,
    Fork(ForkError),
    Ancestry(ReplayError),
    Replay(ReplaySourceError),
    Evidence(ManifestEvidenceError),
    Store(StoreError),
    Run(String),
    NotPublished(String),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSession => formatter.write_str("session not found"),
            Self::Deleted => formatter.write_str("session was deleted"),
            Self::NotPaused { state } => write!(
                formatter,
                "the session is not paused; it is {}",
                match state {
                    HoldState::Running => "running",
                    HoldState::Pending => "still reaching its boundary",
                    HoldState::Paused => "paused",
                }
            ),
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
            Self::EffectUncertain => formatter
                .write_str("session has an uncertain effect; deletion requires explicit force"),
            Self::Held => formatter.write_str("session is held; release the hold before deleting"),
            Self::Conflict => formatter.write_str("session changed concurrently"),
            Self::Goal(error) => error.fmt(formatter),
            Self::InvalidText => formatter.write_str("invalid operator text"),
            Self::Fork(error) => error.fmt(formatter),
            Self::Ancestry(error) => error.fmt(formatter),
            Self::Replay(error) => error.fmt(formatter),
            Self::Evidence(error) => error.fmt(formatter),
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
impl From<ManifestEvidenceError> for LifecycleError {
    fn from(value: ManifestEvidenceError) -> Self {
        Self::Evidence(value)
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

/// Durable session metadata operations, independent of live run coordination.
pub struct SessionMetadata<S> {
    store: Arc<S>,
    clock: Arc<dyn Clock>,
}

impl<S> SessionMetadata<S>
where
    S: SessionLifecycleStore,
{
    pub fn new(store: Arc<S>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    pub fn meta(&self, id: SessionId) -> Result<SessionMeta, LifecycleError> {
        let meta = self.raw_meta(id)?;
        if self.state(id)?.deleted {
            return Err(LifecycleError::Deleted);
        }
        Ok(meta)
    }

    pub fn set_title(
        &self,
        append: &dyn LeasedLifecycleAppend,
        expected: Seq,
        title: Option<String>,
    ) -> Result<LifecycleMutation<SessionMeta>, LifecycleError> {
        let id = append.session_id();
        let title = title.map(validate_operator_text).transpose()?;
        let current = self.meta(id)?;
        if current.title == title {
            return Ok(LifecycleMutation {
                value: current,
                publication: None,
            });
        }
        let event = lifecycle_event(
            expected,
            self.clock.now(),
            EventKind::SessionTitleChanged { title },
        );
        let state = self.state(id)?;
        let mut prepared_state = state.clone();
        prepared_state.apply(&event);
        let mut prepared = current;
        prepared.updated = event.env.ts;
        prepared.title = prepared_state.title;
        let publication = append.compare_and_append_lifecycle(expected, event)?;
        require_publication(publication.clone())?;
        Ok(LifecycleMutation {
            value: prepared,
            publication: Some(publication),
        })
    }

    /// The hold this session is under right now.
    pub fn hold(&self, id: SessionId) -> Result<HoldState, LifecycleError> {
        self.meta(id)?;
        Ok(hold_state(self.state(id)?.pause))
    }

    /// What this session has already answered each hold request with.
    ///
    /// Read at two seams: before a live run is signalled, so a re-delivered
    /// request never reaches a turn twice, and again under the lease, which is
    /// the only place the answer is authoritative.
    pub fn receipts(&self, id: SessionId) -> Result<HoldReceipts, LifecycleError> {
        self.meta(id)?;
        Ok(self.state(id)?.holds)
    }

    /// Record a hold on a session with no turn to stop.
    ///
    /// The idle half of `_ego/pause`. When a turn is running the boundary is
    /// written by that turn, which is the one writer holding the lease; when
    /// none is, there is nothing to wait for and the boundary is now.
    ///
    /// A request arriving while a hold is already in force finds it and
    /// answers with it, so a second host asking does not stack a second hold.
    ///
    /// This is idempotence by *state*, which is all this seam can offer: it
    /// sees where the session is, not what any particular request was told.
    /// Replaying a request the session has moved past is
    /// [`SessionLifecycle::pause`]'s job, because that answer has to be given
    /// before the run is signalled and this function runs after.
    pub fn pause(
        &self,
        append: &dyn LeasedLifecycleAppend,
        expected: Seq,
        request: RequestId,
    ) -> Result<HoldState, LifecycleError> {
        let id = append.session_id();
        self.meta(id)?;
        let current = self.state(id)?.pause;
        if let PauseState::Paused { .. } = current {
            // Told about a boundary it did not establish, and owed a receipt
            // for having been told: `paused` promises a durable record, and a
            // host cannot see that the boundary carries another request's id.
            // Not for `Requested`, which answers `pending` — a hold still
            // walking to its boundary has nothing durable to replay.
            let event = lifecycle_event(
                expected,
                self.clock.now(),
                EventKind::SessionPauseObserved { request },
            );
            require_publication(append.compare_and_append_lifecycle(expected, event)?)?;
            return Ok(HoldState::Paused);
        }
        if current != PauseState::Running {
            return Ok(hold_state(current));
        }
        // Two events and not one, for the reason the turn loop writes two: a
        // hold that recorded only its boundary would say a session stopped
        // without saying who asked, and the request id is what a reconnecting
        // host matches its own call against.
        let mut expected = expected;
        for kind in [
            EventKind::SessionPauseRequested { request },
            EventKind::SessionPaused { request },
        ] {
            let event = lifecycle_event(expected, self.clock.now(), kind);
            let appended = event.env.seq;
            require_publication(append.compare_and_append_lifecycle(expected, event)?)?;
            expected = appended;
        }
        Ok(HoldState::Paused)
    }

    /// Release a hold that is durably in place.
    ///
    /// Refuses only a session no hold was ever placed on. Replaying a release
    /// this session already performed belongs to [`SessionLifecycle::release`],
    /// which answers before taking the lease this function needs.
    ///
    /// A hold whose boundary never arrived is released like any other. The
    /// pair is two appends, not one record, so a process that died between
    /// them leaves `Requested` standing alone — and the writer that would have
    /// finished it is gone, so the boundary will never come. Refusing would
    /// wedge that session for good: too paused to run, not paused enough to
    /// resume. It is what the reducer's `SessionResumed` arm already assumes.
    ///
    /// A turn still walking to its boundary is a different thing and is
    /// refused earlier, by the inactive lease [`SessionLifecycle::release`]
    /// takes before calling this — releasing the hold underneath a live turn
    /// would let it carry on as though nobody had asked, the one outcome
    /// `_ego/pause` exists to prevent.
    pub fn release(
        &self,
        append: &dyn LeasedLifecycleAppend,
        expected: Seq,
        request: RequestId,
    ) -> Result<HoldState, LifecycleError> {
        let id = append.session_id();
        self.meta(id)?;
        let current = self.state(id)?.pause;
        if current == PauseState::Running {
            return Err(LifecycleError::NotPaused {
                state: HoldState::Running,
            });
        }
        let event = lifecycle_event(
            expected,
            self.clock.now(),
            EventKind::SessionResumed { request },
        );
        require_publication(append.compare_and_append_lifecycle(expected, event)?)?;
        Ok(HoldState::Running)
    }

    pub fn goal(&self, id: SessionId) -> Result<Option<GoalRecord>, LifecycleError> {
        self.meta(id)?;
        Ok(self.state(id)?.goal)
    }

    pub fn set_goal(
        &self,
        append: &dyn LeasedLifecycleAppend,
        expected: Seq,
        objective: String,
        owner: GoalOwner,
        ceilings: Ceilings,
    ) -> Result<LifecycleMutation<GoalRecord>, LifecycleError> {
        let session = append.session_id();
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
            expected,
            self.clock.now(),
            EventKind::GoalSet {
                id: crate::event::id::GoalId::new(),
                objective,
                owner,
                ceilings,
                derived_from: None,
            },
        );
        let mut prepared_state = state.clone();
        prepared_state.apply(&event);
        let prepared = prepared_state
            .goal
            .ok_or_else(|| LifecycleError::from(GoalRefusal::NoActiveGoal))?;
        let publication = append.compare_and_append_lifecycle(expected, event)?;
        require_publication(publication.clone())?;
        Ok(LifecycleMutation {
            value: prepared,
            publication: Some(publication),
        })
    }

    pub fn transition_goal(
        &self,
        append: &dyn LeasedLifecycleAppend,
        expected: Seq,
        transition: GoalTransition,
    ) -> Result<LifecycleMutation<GoalRecord>, LifecycleError> {
        let session = append.session_id();
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
            expected,
            self.clock.now(),
            EventKind::GoalTransitioned {
                id: goal.id,
                transition,
            },
        );
        let mut prepared_state = state.clone();
        prepared_state.apply(&event);
        let prepared = prepared_state
            .goal
            .ok_or_else(|| LifecycleError::from(GoalRefusal::NoActiveGoal))?;
        let publication = append.compare_and_append_lifecycle(expected, event)?;
        require_publication(publication.clone())?;
        Ok(LifecycleMutation {
            value: prepared,
            publication: Some(publication),
        })
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
        self.replay_prefix(id, None)
    }

    fn replay_prefix(
        &self,
        id: SessionId,
        cutoff: Option<Seq>,
    ) -> Result<(SessionState, Vec<ReplayFrame>), LifecycleError> {
        if self.store.is_quarantined(id)? {
            return Err(LifecycleError::UnknownSession);
        }
        let entries = self
            .store
            .entries(&id)?
            .ok_or(LifecycleError::UnknownSession)?;
        let mut state = SessionState::default();
        let mut frames = Vec::new();
        let mut semantic = Seq::ZERO;
        for entry in entries {
            if let SessionEntry::Event(event) = entry {
                if cutoff.is_some_and(|cutoff| event.env.seq > cutoff) {
                    break;
                }
                let expected = semantic.next();
                if event.env.seq != expected {
                    return Err(LifecycleError::Replay(ReplaySourceError::CorruptSession {
                        session: id,
                        reason: "semantic event sequence is not contiguous".to_owned(),
                    }));
                }
                semantic = event.env.seq;
                state.apply(&event);
                if let Some(public) = project_public(&event) {
                    frames.push(ReplayFrame {
                        event: public,
                        state: state.clone(),
                        inherited: false,
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

/// One shared lifecycle service. Durable facts are always reconstructed from
/// the store; the run collaborator owns only live effect coordination.
pub struct SessionLifecycle<S> {
    metadata: SessionMetadata<S>,
    runs: Arc<dyn SessionRunLifecycle>,
}

impl<S> SessionLifecycle<S>
where
    S: SessionLifecycleStore,
{
    pub fn new(store: Arc<S>, runs: Arc<dyn SessionRunLifecycle>, clock: Arc<dyn Clock>) -> Self {
        Self {
            metadata: SessionMetadata::new(store, clock),
            runs,
        }
    }

    pub fn meta(&self, id: SessionId) -> Result<SessionMeta, LifecycleError> {
        self.metadata.meta(id)
    }

    /// Creation authority remains readable for deleted ancestors of successors.
    pub fn profiles(&self, id: SessionId) -> Result<SessionProfiles, LifecycleError> {
        Ok(self.metadata.raw_meta(id)?.profiles)
    }

    /// Read the current durable goal for an attached consumer.
    pub fn goal(&self, id: SessionId) -> Result<Option<GoalRecord>, LifecycleError> {
        self.metadata.goal(id)
    }

    /// Derive tree metadata without retaining a second lineage authority.
    pub fn lineage_info(
        &self,
        id: SessionId,
    ) -> Result<Option<SessionLineageInfo>, LifecycleError> {
        let state = self.metadata.state(id)?;
        let Some(lineage) = state.lineage else {
            return Ok(None);
        };
        let steps = walk_lineage(
            &*self.metadata.store,
            lineage,
            &mut std::collections::HashSet::from([id]),
            LineageEdges::ForksAndCompactions,
        )?;
        let source = steps.first().expect("a lineage has a source").source;
        let parent = self.metadata.state(source)?;
        let cutoff = match lineage {
            SessionLineage::Fork { at_seq, .. } => Some(at_seq),
            SessionLineage::Compaction { .. } => None,
        };
        let at_message = if let Some(cutoff) = cutoff {
            // A turn has several messages but one settled cutoff. Use its last
            // visible message as the canonical marker, also for tip forks.
            self.metadata
                .store
                .entries(&source)?
                .ok_or(LifecycleError::UnknownSession)?
                .into_iter()
                .filter_map(|entry| match entry {
                    SessionEntry::Event(event)
                        if event.env.seq <= cutoff
                            && matches!(
                                event.kind,
                                EventKind::UserPrompt { .. }
                                    | EventKind::AssistantDelta { .. }
                                    | EventKind::AssistantMessage { .. }
                            ) =>
                    {
                        event.env.item
                    }
                    _ => None,
                })
                .next_back()
        } else {
            None
        };
        Ok(Some(SessionLineageInfo {
            lineage,
            root: steps.last().expect("a lineage has a root").source,
            at_message,
            source_deleted: parent.deleted,
        }))
    }

    /// Hide a session the caller refused to open (see
    /// [`SessionLifecycleStore::quarantine`]).
    pub fn quarantine(&self, id: SessionId) -> Result<(), LifecycleError> {
        Ok(self.metadata.store.quarantine(id)?)
    }

    pub fn create(&self, root: PathBuf) -> Result<CreatedSession, LifecycleError> {
        self.create_with_id(SessionId::new(), root)
    }

    pub fn create_with_id(
        &self,
        id: SessionId,
        root: PathBuf,
    ) -> Result<CreatedSession, LifecycleError> {
        self.create_with_profiles(id, root, SessionProfiles::default())
    }

    /// Publish the session and its creation authority in one durable record.
    pub fn create_with_profiles(
        &self,
        id: SessionId,
        root: PathBuf,
        profiles: SessionProfiles,
    ) -> Result<CreatedSession, LifecycleError> {
        let publication =
            self.metadata
                .store
                .publish_session(id, &root, self.metadata.clock.now(), profiles)?;
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
        let mut items = self.metadata.store.list()?;
        items.retain(|meta| root.is_none_or(|root| meta.workspace_root == root));
        let mut visible = Vec::with_capacity(items.len());
        for meta in items {
            if self.metadata.store.is_quarantined(meta.id)? {
                continue;
            }
            if !self.metadata.state(meta.id)?.deleted {
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
        let meta = self.metadata.raw_meta(id)?;
        if meta.workspace_root != root {
            return Err(LifecycleError::RootMismatch {
                stored: meta.workspace_root,
                requested: root.to_path_buf(),
            });
        }
        let (state, mut frames) = self.metadata.replay(id)?;
        if state.deleted {
            return Err(LifecycleError::Deleted);
        }
        if self.runs.activity(id).await? == SessionActivity::Uncertain {
            return Err(LifecycleError::EffectUncertain);
        }
        // Another connection may have quarantined the session while activity
        // was awaited; the check inside `replay` ran before that.
        if self.metadata.store.is_quarantined(id)? {
            return Err(LifecycleError::UnknownSession);
        }
        if replay && let Some(lineage) = state.lineage {
            let steps = walk_lineage(
                &*self.metadata.store,
                lineage,
                &mut std::collections::HashSet::from([id]),
                LineageEdges::Forks,
            )?;
            let mut inherited = Vec::new();
            for step in steps.into_iter().rev() {
                let SessionLineage::Fork { at_seq, .. } = step.edge else {
                    continue;
                };
                let (_, ancestor_frames) =
                    self.metadata.replay_prefix(step.source, Some(at_seq))?;
                inherited.extend(ancestor_frames.into_iter().map(|mut frame| {
                    frame.inherited = true;
                    frame
                }));
            }
            inherited.append(&mut frames);
            frames = inherited;
        }
        let context_window = ManifestEvidence::rebuild(&*self.metadata.store, &id)?
            .generations()
            .last()
            .and_then(|generation| generation.capabilities().max_context_tokens);
        Ok(SessionAttachment {
            id,
            root: root.to_path_buf(),
            meta,
            context_window,
            replay: if replay { frames } else { Vec::new() },
        })
    }

    pub async fn fork(&self, source: SessionId) -> Result<CreatedSession, LifecycleError> {
        self.fork_as(source, SessionId::new()).await
    }

    /// Fork under a target id the caller already holds, so it can keep the
    /// child out of sight from the moment the fork is published.
    pub async fn fork_as(
        &self,
        source: SessionId,
        target: SessionId,
    ) -> Result<CreatedSession, LifecycleError> {
        let _source_lease = self.require_inactive(source).await?;
        self.metadata.meta(source)?;
        let (state, _) = self.metadata.replay(source)?;
        let prepared = PreparedFork::new(source, &state, self.metadata.clock.now())?;
        let publication = self.publish_prepared_fork(target, &prepared)?;
        Ok(CreatedSession {
            id: target,
            publication: publication.into(),
        })
    }

    /// Fork at a visible user or assistant message without taking the source writer lease.
    pub async fn fork_at(
        &self,
        source: SessionId,
        message: ItemId,
    ) -> Result<CreatedSession, LifecycleError> {
        self.fork_at_as(source, message, SessionId::new()).await
    }

    /// Fork at a message under an id the adapter can hide during admission.
    pub async fn fork_at_as(
        &self,
        source: SessionId,
        message: ItemId,
        target: SessionId,
    ) -> Result<CreatedSession, LifecycleError> {
        let prepared = self.prepare_fork_at(source, message)?;
        let publication = self.publish_prepared_fork(target, &prepared)?;
        Ok(CreatedSession {
            id: target,
            publication: publication.into(),
        })
    }

    /// Resolve an item through visible fork ancestry, stopping at a compaction checkpoint.
    pub fn prepare_fork_at(
        &self,
        mut source: SessionId,
        message: ItemId,
    ) -> Result<PreparedFork, LifecycleError> {
        let mut cutoff = None;
        let mut visited = std::collections::HashSet::new();
        loop {
            if !visited.insert(source) {
                return Err(ReplayError::Cycle { session: source }.into());
            }
            if self.metadata.store.is_quarantined(source)? {
                return Err(LifecycleError::UnknownSession);
            }
            let entries = self
                .metadata
                .store
                .entries(&source)?
                .ok_or(LifecycleError::UnknownSession)?;
            let mut events = Vec::new();
            let mut seq = Seq::ZERO;
            for entry in entries {
                if let SessionEntry::Event(event) = entry {
                    if cutoff.is_some_and(|cutoff| event.env.seq > cutoff) {
                        break;
                    }
                    if event.env.seq != seq.next() {
                        return Err(LifecycleError::Replay(ReplaySourceError::CorruptSession {
                            session: source,
                            reason: "semantic event sequence is not contiguous".into(),
                        }));
                    }
                    events.push(event);
                }
            }
            match PreparedFork::after_message(source, &events, message, self.metadata.clock.now()) {
                Ok(prepared) => return Ok(prepared),
                Err(ForkError::UnknownItem) => {
                    let state = SessionState::replay(events);
                    match state.lineage {
                        Some(crate::session::SessionLineage::Fork {
                            source: ancestor,
                            at_seq,
                        }) => {
                            source = ancestor;
                            cutoff = Some(at_seq);
                        }
                        _ => return Err(ForkError::UnknownItem.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Publish a settled fork prepared under the caller's authority or from an immutable prefix.
    pub fn publish_prepared_fork(
        &self,
        target: SessionId,
        prepared: &PreparedFork,
    ) -> Result<TargetPublication, LifecycleError> {
        // Keep verified bytes alive through publication. Source journals are append-only;
        // their references retain the immutable blobs on subsequent writer recovery.
        let _pinned = ReplayContext::new(&*self.metadata.store, &*self.metadata.store)
            .replay_lineage(prepared.seed().lineage())?;
        let publication = self.metadata.store.publish_fork(target, prepared.seed());
        if !publication.switch_target() {
            return Err(LifecycleError::NotPublished(
                publication
                    .diagnostic()
                    .unwrap_or("fork was not published")
                    .to_owned(),
            ));
        }
        Ok(publication)
    }

    pub async fn delete(&self, id: SessionId) -> Result<LifecyclePublication, LifecycleError> {
        self.delete_with_force(id, false).await
    }

    /// Force acknowledges uncertain effects, never a live writer or a hold.
    /// History remains intact for descendants and audit.
    pub async fn delete_with_force(
        &self,
        id: SessionId,
        force: bool,
    ) -> Result<LifecyclePublication, LifecycleError> {
        let append = self.metadata.store.acquire_inactive_lease(id)?;
        let state = self.metadata.state(id)?;
        if state.deleted {
            return Ok(LifecyclePublication::Published);
        }
        if self.metadata.hold(id)? != HoldState::Running {
            return Err(LifecycleError::Held);
        }
        match self.runs.activity(id).await? {
            SessionActivity::Active => return Err(LifecycleError::Active),
            SessionActivity::LeaseHeld => return Err(LifecycleError::LeaseHeld),
            SessionActivity::Uncertain if !force => return Err(LifecycleError::EffectUncertain),
            _ => {}
        }
        let entries = self
            .metadata
            .store
            .entries(&id)?
            .ok_or(LifecycleError::UnknownSession)?;
        let status = entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::RunTransaction(transaction) => Some(transaction.status()),
            _ => None,
        });
        if !force
            && matches!(
                status,
                Some(
                    crate::run::RunStatus::Uncertain
                        | crate::run::RunStatus::Running
                        | crate::run::RunStatus::Cancelling
                )
            )
        {
            return Err(LifecycleError::EffectUncertain);
        }
        let expected = state.last_seq;
        let event = lifecycle_event(
            expected,
            self.metadata.clock.now(),
            EventKind::SessionDeleted,
        );
        let publication = append.compare_and_append_lifecycle(expected, event)?;
        require_publication(publication.clone())?;
        Ok(publication)
    }

    /// Ask this session to stop at its next boundary and hold there.
    ///
    /// Two paths and one meaning. A session with a turn running is told
    /// through the run seam and answers `Pending`: the turn owns the writer
    /// lease, so it is the only party that may record the boundary, and it
    /// records it when it gets there. A session with nothing running has
    /// already reached one, so the boundary is written here under an inactive
    /// lease and the answer is `Paused`.
    ///
    /// The order matters. The run seam is asked first, so a turn that starts
    /// between the two steps sees the hold rather than slipping past a lease
    /// check that found nothing to stop.
    pub async fn pause(
        &self,
        id: SessionId,
        request: RequestId,
    ) -> Result<HoldState, LifecycleError> {
        self.metadata.meta(id)?;
        // Before the run is signalled, not after: a request this session has
        // already paused under has been acted on once, and delivering it to a
        // turn again would stop a turn nobody asked to stop.
        if self.metadata.receipts(id)?.paused(request) {
            return Ok(HoldState::Paused);
        }
        if self.runs.hold(id, request).await? {
            return Ok(HoldState::Pending);
        }
        let append = self.require_inactive(id).await?;
        let expected = self.metadata.state(id)?.last_seq;
        self.metadata.pause(append.as_ref(), expected, request)
    }

    /// Release a hold that is durably in place.
    ///
    /// `release` and not `resume`, which this type already uses for
    /// `session/resume` — reattaching a client to a session. The two are
    /// unrelated: one gives a connection a view, the other lets a stopped
    /// session run again. The wire name stays `_ego/resume` because the ADR
    /// ratified it; the collision is only in Rust.
    ///
    /// Only ever the idle path: [`SessionMetadata::release`] refuses anything
    /// but a session that is durably paused, and a durably paused session by
    /// definition has no turn left to hold the lease.
    pub async fn release(
        &self,
        id: SessionId,
        request: RequestId,
    ) -> Result<HoldState, LifecycleError> {
        self.metadata.meta(id)?;
        // Before the lease, so a release whose reply was lost is answered
        // again even though the session has since been given more work — the
        // lease would have refused it as active and reported the opposite of
        // what happened.
        if self.metadata.receipts(id)?.resumed(request) {
            return Ok(HoldState::Running);
        }
        let append = self.require_inactive(id).await?;
        let expected = self.metadata.state(id)?.last_seq;
        self.metadata.release(append.as_ref(), expected, request)
    }

    /// The hold this session is under right now.
    pub fn hold(&self, id: SessionId) -> Result<HoldState, LifecycleError> {
        self.metadata.hold(id)
    }

    async fn require_inactive(
        &self,
        id: SessionId,
    ) -> Result<Box<dyn InactiveSessionLease + '_>, LifecycleError> {
        let lease = self.metadata.store.acquire_inactive_lease(id)?;
        match self.runs.activity(id).await? {
            SessionActivity::Inactive => {}
            SessionActivity::LeaseHeld => return Err(LifecycleError::LeaseHeld),
            SessionActivity::Active => return Err(LifecycleError::Active),
            SessionActivity::Uncertain => return Err(LifecycleError::EffectUncertain),
        }
        Ok(lease)
    }

    pub async fn close(&self, id: SessionId) -> Result<ClosedSession, LifecycleError> {
        self.metadata.meta(id)?;
        match self.runs.cancel_and_settle(id).await {
            Err(LifecycleError::EffectUncertain)
                if self.runs.activity(id).await? == SessionActivity::Uncertain =>
            {
                // Abandon the session without claiming its effect was resolved.
                // Keep the run log intact and durably prevent later activation.
                self.quarantine(id)?;
                Ok(ClosedSession {
                    id,
                    cancelled: false,
                })
            }
            result => result,
        }
    }
}

/// The reduction's hold, in the vocabulary a host is answered in.
fn hold_state(pause: PauseState) -> HoldState {
    match pause {
        PauseState::Running => HoldState::Running,
        PauseState::Requested { .. } => HoldState::Pending,
        PauseState::Paused { .. } => HoldState::Paused,
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
        | LifecyclePublication::PublishedWithProjectionWarning { .. }
        | LifecyclePublication::PublishedDurabilityUncertain { .. } => Ok(()),
    }
}
