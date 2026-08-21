//! The one place a `DomainEvent` is assembled, persisted and reduced.
//!
//! See `docs/decisions/2026-08-21-the-turn-loop.md` §5. Before this existed,
//! eight test files built their own `Envelope`, and they disagreed with each
//! other — `tests/public_golden.rs:167` gives a permission its own `ItemId`
//! while `tests/reducer.rs:143` reuses the tool's. That is what a contract
//! with no implementation looks like.

use crate::event::domain::{DomainEvent, Envelope, EventKind};
use crate::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use crate::event::time::Clock;
use crate::public::{PublicEvent, project_public};
use crate::state::SessionState;
use crate::store::session_store::{SessionStore, StoreError};

/// Assembles envelopes, appends them to the durable log, and folds them into
/// the in-memory state — in that order, and nowhere else.
///
/// The ordering is the point. A store failure must leave [`SessionState`]
/// describing exactly what is on disk, so the fold happens only after the
/// append returns. The reverse order would produce an in-memory session that
/// remembers an event no reader will ever find.
pub struct Journal<'a> {
    session: SessionId,
    store: &'a dyn SessionStore,
    clock: &'a dyn Clock,
    producer: Producer,
    /// Continues from the resumed state's `last_seq`, so reloading a session
    /// and carrying on does not restart its ordering.
    seq: Seq,
    state: SessionState,
    /// Everything this journal has published, in order. The loop's caller
    /// reads it instead of the internal log — `PublicEvent` is the projection
    /// that is allowed to leave the crate.
    published: Vec<PublicEvent>,
}

impl<'a> Journal<'a> {
    /// Open a journal over `state`, continuing its sequence.
    pub fn new(
        session: SessionId,
        store: &'a dyn SessionStore,
        clock: &'a dyn Clock,
        producer: Producer,
        state: SessionState,
    ) -> Self {
        Self {
            session,
            store,
            clock,
            producer,
            seq: state.last_seq,
            state,
            published: Vec::new(),
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
        let id = EventId::new();
        let event = DomainEvent {
            env: Envelope {
                seq: self.seq.next(),
                id,
                turn,
                item,
                parent,
                ts: self.clock.now(),
                producer: self.producer.clone(),
            },
            kind,
        };
        self.store.append(&self.session, &event)?;
        self.state.apply(&event);
        if let Some(public) = project_public(&event) {
            self.published.push(public);
        }
        Ok(id)
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

    /// Everything published so far, in order.
    pub fn published(&self) -> &[PublicEvent] {
        &self.published
    }
}
