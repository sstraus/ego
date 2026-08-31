//! The one place a `DomainEvent` is assembled, persisted and reduced.
//!
//! See `docs/decisions/2026-08-21-the-turn-loop.md` §5. Before this existed,
//! eight test files built their own `Envelope`, and they disagreed with each
//! other — `tests/public_golden.rs:167` gives a permission its own `ItemId`
//! while `tests/reducer.rs:143` reuses the tool's. That is what a contract
//! with no implementation looks like.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::event::domain::{DomainEvent, Envelope, EventKind};
use crate::event::hook::{
    HookActivation, HookId, HookInvocationId, HookOccurrenceKey, HookPlan, HookResult, HookSubject,
};
use crate::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use crate::event::time::Clock;
use crate::hook::{HookReachedEvidence, HookStartedEvidence};
use crate::public::{PublicEvent, project_public};
use crate::sink::EventSink;
use crate::state::SessionState;
use crate::store::session_store::{SessionStore, StoreError};

/// Assembles envelopes, appends them to the durable log, folds them into the
/// in-memory state, and publishes the projection — in that order, and nowhere
/// else.
///
/// The ordering is the point, and it now has two halves. A store failure must
/// leave [`SessionState`] describing exactly what is on disk, so the fold
/// happens only after the append returns; the reverse order would produce an
/// in-memory session that remembers an event no reader will ever find. The
/// sink comes last for the same reason one layer out: a host told about an
/// event that was never written would be reporting a turn nobody can resume
/// into.
///
/// That ordering is also why the sink is handed [`SessionState`]: at the
/// moment of publication this journal holds the reduction *including* the
/// event it is publishing, which is precisely what a consumer projecting
/// cumulative figures needs and cannot obtain for itself in the right order.
/// See `docs/decisions/2026-08-21-what-a-sink-is-handed.md`.
pub struct Journal<'a> {
    session: SessionId,
    store: &'a dyn SessionStore,
    clock: &'a dyn Clock,
    /// Where the public projection goes the moment it is durable. Held rather
    /// than accumulated: `CLI.md` §4.2 says *live*, and a `Vec` handed over
    /// at turn end is a different contract wearing the same flag — as well as
    /// a buffer that grows for the life of the session.
    sink: &'a dyn EventSink,
    producer: Producer,
    /// Continues from the resumed state's `last_seq`, so reloading a session
    /// and carrying on does not restart its ordering.
    seq: Seq,
    state: SessionState,
}

/// A cloneable append authority for one borrow-scoped session journal.
///
/// The mutex serialises only synchronous journal operations. Store append,
/// reduction, and publication still happen in [`Journal::append_with_producer`]
/// and in their existing order; this handle adds no second write path.
#[derive(Clone)]
pub struct SharedJournal<'s> {
    inner: Arc<Mutex<Journal<'s>>>,
}

/// Why a shared journal operation could not complete.
#[derive(Debug)]
pub enum SharedJournalError {
    /// Another append-capable handle still owns the journal at finalization.
    InUse,
    /// A prior panic poisoned the journal lock, so no further write is safe.
    Synchronization,
    /// The sole underlying journal append failed.
    Store(StoreError),
}

impl fmt::Display for SharedJournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InUse => f.write_str("the shared journal still has live append handles"),
            Self::Synchronization => f.write_str("the shared journal lock is poisoned"),
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SharedJournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::InUse | Self::Synchronization => None,
        }
    }
}

impl From<StoreError> for SharedJournalError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl<'s> SharedJournal<'s> {
    /// Wrap the session's sole journal in cloneable append authority.
    pub fn new(journal: Journal<'s>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(journal)),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Journal<'s>>, SharedJournalError> {
        self.inner
            .lock()
            .map_err(|_| SharedJournalError::Synchronization)
    }

    /// Append through the existing journal sequence.
    pub fn append(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        kind: EventKind,
    ) -> Result<EventId, SharedJournalError> {
        self.lock()?.append(turn, item, parent, kind).map_err(Into::into)
    }

    /// Durably freeze a lifecycle plan through the existing journal.
    pub fn hook_reached(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        occurrence: HookOccurrenceKey,
        subject: HookSubject,
        plan: HookPlan,
    ) -> Result<HookReachedEvidence, SharedJournalError> {
        self.lock()?
            .hook_reached(turn, item, parent, occurrence, subject, plan)
            .map_err(Into::into)
    }

    /// Durably record one hook delivery boundary through the existing journal.
    #[allow(clippy::too_many_arguments)]
    pub fn hook_started(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        reached: &HookReachedEvidence,
        hook: HookId,
        invocation: HookInvocationId,
        activation: HookActivation,
    ) -> Result<HookStartedEvidence, SharedJournalError> {
        self.lock()?
            .hook_started(turn, item, reached, hook, invocation, activation)
            .map_err(Into::into)
    }

    /// Durably settle one hook start through the existing journal.
    pub fn hook_settled(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        started: HookStartedEvidence,
        result: HookResult,
    ) -> Result<EventId, SharedJournalError> {
        self.lock()?
            .hook_settled(turn, item, started, result)
            .map_err(Into::into)
    }

    /// Inspect current state without allowing a lock guard or reference to escape.
    pub fn inspect<T>(
        &self,
        inspect: impl FnOnce(&SessionState) -> T,
    ) -> Result<T, SharedJournalError> {
        let journal = self.lock()?;
        Ok(inspect(journal.state()))
    }

    /// Recover the exact final state once every other append handle is gone.
    pub fn try_into_state(self) -> Result<SessionState, SharedJournalError> {
        let journal = Arc::try_unwrap(self.inner).map_err(|_| SharedJournalError::InUse)?;
        journal
            .into_inner()
            .map(Journal::into_state)
            .map_err(|_| SharedJournalError::Synchronization)
    }
}

impl<'a> Journal<'a> {
    /// Open a journal over `state`, continuing its sequence.
    pub fn new(
        session: SessionId,
        store: &'a dyn SessionStore,
        clock: &'a dyn Clock,
        sink: &'a dyn EventSink,
        producer: Producer,
        state: SessionState,
    ) -> Self {
        Self {
            session,
            store,
            clock,
            sink,
            producer,
            seq: state.last_seq,
            state,
        }
    }

    /// Append one event to the log and reduce it.
    ///
    /// Returns the new event's own [`EventId`], which is what a later event
    /// names as its causal parent.
    pub fn append(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        kind: EventKind,
    ) -> Result<EventId, StoreError> {
        self.append_with_producer(turn, item, parent, self.producer.clone(), kind)
    }

    fn append_with_producer(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        producer: Producer,
        kind: EventKind,
    ) -> Result<EventId, StoreError> {
        let id = EventId::new();
        let event = DomainEvent {
            env: Envelope {
                seq: self.seq.next(),
                id,
                turn,
                item,
                parent,
                ts: self.clock.now(),
                producer,
            },
            kind,
        };
        self.store.append(&self.session, &event)?;
        self.state.apply(&event);
        if let Some(public) = project_public(&event) {
            self.sink.publish(&public, &self.state);
        }
        Ok(id)
    }

    /// Durably freeze the exact ordered plan at a real owner's lifecycle point.
    pub fn hook_reached(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        occurrence: HookOccurrenceKey,
        subject: HookSubject,
        plan: HookPlan,
    ) -> Result<HookReachedEvidence, StoreError> {
        let producer = self.producer.clone();
        let event = self.append_with_producer(
            turn,
            item,
            parent,
            producer.clone(),
            EventKind::HookLifecycleReached {
                occurrence: occurrence.clone(),
                subject,
                plan: plan.clone(),
            },
        )?;
        Ok(HookReachedEvidence::new(
            event, occurrence, subject, producer, plan,
        ))
    }

    /// Durably record the external-delivery boundary for one planned hook.
    #[allow(clippy::too_many_arguments)]
    pub fn hook_started(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        reached: &HookReachedEvidence,
        hook: HookId,
        invocation: HookInvocationId,
        activation: HookActivation,
    ) -> Result<HookStartedEvidence, StoreError> {
        let event = self.append_with_producer(
            turn,
            item,
            Some(reached.event()),
            reached.producer().clone(),
            EventKind::HookInvocationStarted {
                occurrence: reached.event(),
                hook,
                invocation,
                lifecycle: reached.occurrence().lifecycle(),
                subject: reached.subject(),
                activation,
            },
        )?;
        Ok(HookStartedEvidence::new(
            event,
            reached.event(),
            hook,
            invocation,
            activation,
        ))
    }

    /// Durably settle one start under the hook producer that performed it.
    pub fn hook_settled(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        started: HookStartedEvidence,
        result: HookResult,
    ) -> Result<EventId, StoreError> {
        self.append_with_producer(
            turn,
            item,
            Some(started.event()),
            Producer::Hook(started.hook()),
            EventKind::HookInvocationSettled {
                occurrence: started.occurrence(),
                hook: started.hook(),
                invocation: started.invocation(),
                result,
            },
        )
    }

    /// Publish an event the reducer cannot produce.
    ///
    /// `SessionStarted` and `TurnStarted` have no `EventKind` counterpart —
    /// they are facts the harness knows before any domain event exists (see
    /// `public/mod.rs`) — so they reach the sink through here rather than
    /// through [`append`](Self::append). Nothing is written to the log,
    /// because there is nothing to write: an unwritten event cannot be
    /// published out of order with one that was.
    pub fn publish(&self, event: &PublicEvent) {
        self.sink.publish(event, &self.state);
    }

    /// The reduction of everything appended so far.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Take the state back out, for a caller that wants to continue the
    /// session with another turn.
    pub fn into_state(self) -> SessionState {
        self.state
    }
}
