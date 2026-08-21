//! Where a [`PublicEvent`] goes the moment it becomes durable.
//!
//! See `CLI.md` §4.2 — "one event per line, **live**". Live is the contract,
//! not a quality of the implementation: a batch handed over when the turn
//! ends is a different promise wearing the same flag, and a caller watching
//! `--output jsonl` for a long tool call would see nothing at all until it
//! finished.
//!
//! ## Why the port is infallible
//!
//! [`SessionStore`](crate::store::session_store::SessionStore) is fallible
//! because the log *is* the product: an event that could not be appended did
//! not happen, and the turn has nothing left to report through. A sink is the
//! opposite — a view of a log that is already written. A consumer that walks
//! away (`ego … --output jsonl | head -1`) has not invalidated the turn, and
//! this crate has no pipe, no file and no socket with which to have an
//! opinion about why a write failed.
//!
//! So a sink absorbs its own transport and, if it wants the run to stop,
//! stops it where the transport lives: the CLI owns stdout and owns
//! [`RunTermination`](crate::run::RunTermination). This is the same division
//! [`Clock`](crate::event::time::Clock) already draws — a port that cannot
//! fail in terms this crate can express does not pretend it can.
//!
//! What a sink must not do is drop an event quietly. Absorbing a transport
//! error is not the same as discarding the event that hit it, and an
//! implementation that swallows both is the silent failure this crate spends
//! its error types avoiding.

use std::sync::Mutex;

use crate::public::PublicEvent;
use crate::state::SessionState;

/// A consumer of the public projection, in the order the events happened.
///
/// `&self` rather than `&mut self`: a sink is shared — the session holds one
/// while the loop, and eventually a host, all publish through it — and the
/// alternative is threading a unique borrow through every layer that can
/// produce an event.
///
/// `Send + Sync` follows from that sharing rather than being added to it. A
/// turn is an `async fn`, and an `ego acp` connection drives one on a
/// multi-threaded runtime: a future holding `&dyn EventSink` across an await is
/// `Send` only if the sink is `Sync`. Every implementation here already
/// satisfies it — the ones with interior state use a `Mutex` because a shared
/// `&self` left them no other choice — so this states a property the port had
/// all along and could not be relied on.
pub trait EventSink: Send + Sync {
    /// Take one event, with the reduction it produced.
    ///
    /// Called once per event, after it is durable and after it has been folded
    /// — so `state` always *includes* `event`, never precedes it. See
    /// `docs/decisions/2026-08-21-what-a-sink-is-handed.md`.
    ///
    /// Most sinks ignore `state`: a line of JSONL is the event and nothing
    /// else. It is here for the consumer that cannot work without it — ACP's
    /// `UsageUpdate` reports context *occupancy* and *cumulative* cost, neither
    /// of which any single event carries, and `ego-acp`'s `project_acp_v1`
    /// requires exactly this reduction. Handing it over at the moment of
    /// publication is what makes that requirement true by construction instead
    /// of by a caller remembering a doc comment.
    fn publish(&self, event: &PublicEvent, state: &SessionState);
}

/// A borrowed sink is a sink.
///
/// So that a caller who owns one destination and a caller who is handed
/// `&dyn EventSink` can go through the same generic entry point without one of
/// them wrapping the other in a newtype whose only job is to forward.
impl<T: EventSink + ?Sized> EventSink for &T {
    fn publish(&self, event: &PublicEvent, state: &SessionState) {
        (**self).publish(event, state);
    }
}

/// A sink that keeps everything, for a caller that wants the batch after all.
///
/// The batch is not wrong — an ACP host, and `CLI.md` §4.1's single result
/// object, both want one — it is only wrong as the *sole* path. Making it a
/// sink the caller chooses puts the memory it costs on the caller who asked
/// for it, instead of on every session for its whole life.
#[derive(Debug, Default)]
pub struct CollectingSink {
    events: Mutex<Vec<PublicEvent>>,
}

impl CollectingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything published so far, in order.
    pub fn events(&self) -> Vec<PublicEvent> {
        self.events
            .lock()
            .expect("a poisoned lock means a publisher panicked mid-event")
            .clone()
    }

    /// Everything published so far, leaving the sink empty.
    ///
    /// What a caller that wants one turn at a time uses between turns, so the
    /// vector does not grow for the life of the session.
    pub fn drain(&self) -> Vec<PublicEvent> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .expect("a poisoned lock means a publisher panicked mid-event"),
        )
    }
}

impl EventSink for CollectingSink {
    fn publish(&self, event: &PublicEvent, _state: &SessionState) {
        self.events
            .lock()
            .expect("a poisoned lock means a publisher panicked mid-event")
            .push(event.clone());
    }
}

/// A sink for a caller that publishes nothing anywhere.
///
/// A named type rather than an `Option<&dyn EventSink>` at every call site:
/// "there is no sink" and "the sink is broken" are different facts, and an
/// `Option` spells the first one in a way that invites a `?` to mean the
/// second.
#[derive(Debug, Default)]
pub struct NullSink;

impl EventSink for NullSink {
    fn publish(&self, _event: &PublicEvent, _state: &SessionState) {}
}
