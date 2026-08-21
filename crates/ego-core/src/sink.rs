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

/// A consumer of the public projection, in the order the events happened.
///
/// `&self` rather than `&mut self`: a sink is shared — the session holds one
/// while the loop, and eventually a host, all publish through it — and the
/// alternative is threading a unique borrow through every layer that can
/// produce an event.
pub trait EventSink {
    /// Take one event. Called once per event, after it is durable.
    fn publish(&self, event: &PublicEvent);
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
    fn publish(&self, event: &PublicEvent) {
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
    fn publish(&self, _event: &PublicEvent) {}
}
