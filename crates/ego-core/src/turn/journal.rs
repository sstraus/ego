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
            self.sink.publish(&public, &self.state);
        }
        Ok(id)
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
