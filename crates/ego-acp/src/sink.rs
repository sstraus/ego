//! The live end of the projection: a [`PublicEvent`] becomes a
//! `session/update` notification the moment it is durable.
//!
//! [`project`](crate::project) says what an event *is* in ACP terms. This says
//! where it goes, and it is the only thing in this crate that touches a
//! connection.
//!
//! ## Why this can be a synchronous, infallible sink
//!
//! [`EventSink::publish`] is `&self`, sync and infallible, and that looked like
//! it would need a channel and a draining task to reach an async transport. It
//! does not. `ConnectionTo::send_notification` is itself synchronous — it
//! `unbounded_send`s onto the connection's outgoing queue and returns — so the
//! turn loop hands an event over without awaiting anything, without a queue of
//! our own, and in the order the loop published it. There is no second buffer
//! to reorder or drop.
//!
//! ## A failed send is said out loud, not swallowed
//!
//! `sink.rs` is explicit: absorbing a transport error is not the same as
//! discarding the event that hit it. `send_notification` fails for exactly one
//! reason — the connection's outgoing queue is closed, meaning the host has
//! gone — so there is nobody left to answer with a JSON-RPC error, and telling
//! a client that stopped listening that it stopped listening is not a report.
//!
//! It goes to the log instead, once: a `tracing` warning on the first failure
//! and silence after, because every later send fails identically and the last
//! one names nothing the first did not. An agent's stderr is not its protocol
//! stream, so this is a channel that exists precisely for the case where the
//! protocol stream is the thing that broke.

use std::sync::Mutex;

use agent_client_protocol::{Client, ConnectionTo};
use agent_client_protocol_schema::v1;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::public::PublicEvent;
use ego_core::sink::EventSink;
use ego_core::state::SessionState;

use crate::project::project_acp_v1;

/// Publishes a run's public events to an ACP client as `session/update`.
///
/// v1 and only v1: [`SERVED`](crate::negotiate::SERVED) is the one protocol
/// version this build configures, so a v2 twin of this type would be a sink
/// nothing can select. When the draft stabilises, that constant and this type
/// change together.
pub struct AcpSink {
    connection: ConnectionTo<Client>,
    session: v1::SessionId,
    /// The resolved model's limits, for the one field of `UsageUpdate` that
    /// lives in neither the event nor the reduction.
    caps: ModelCapabilities,
    /// Whether the failure below has already been reported.
    reported: Mutex<bool>,
}

impl AcpSink {
    pub fn new(
        connection: ConnectionTo<Client>,
        session: v1::SessionId,
        caps: ModelCapabilities,
    ) -> Self {
        AcpSink {
            connection,
            session,
            caps,
            reported: Mutex::new(false),
        }
    }
}

impl EventSink for AcpSink {
    fn publish(&self, event: &PublicEvent, state: &SessionState) {
        // `project_acp_v1` and not `project_v1`: the complete entry point, which
        // is the whole reason `publish` carries the reduction. See
        // `docs/decisions/2026-08-21-what-a-sink-is-handed.md`.
        let Some(update) = project_acp_v1(event, state, &self.caps) else {
            return;
        };
        let notification = v1::SessionNotification::new(self.session.clone(), update);
        if let Err(err) = self.connection.send_notification(notification) {
            let mut reported = self
                .reported
                .lock()
                .expect("a poisoned lock means a publisher panicked mid-event");
            if !*reported {
                *reported = true;
                tracing::warn!(
                    session = %self.session,
                    error = %err,
                    "the client stopped listening; the rest of this turn's \
                     updates cannot be delivered"
                );
            }
        }
    }
}
