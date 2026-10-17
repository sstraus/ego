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

use std::{path::PathBuf, sync::Mutex};

use agent_client_protocol::{Client, ConnectionTo};
use agent_client_protocol_schema::v1;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::public::PublicEvent;
use ego_core::sink::EventSink;
use ego_core::state::SessionState;

use crate::project::{project_acp_updates_v1, provider_retry_cleared_v1, provider_retry_v1};

/// Publishes a run's public events to an ACP client as `session/update`.
///
/// v1 and only v1: [`SERVED`](crate::negotiate::SERVED) is the one protocol
/// version this build configures, so a v2 twin of this type would be a sink
/// nothing can select. When the draft stabilises, that constant and this type
/// change together.
pub struct AcpSink {
    connection: ConnectionTo<Client>,
    session: v1::SessionId,
    /// The limits the caller resolved before the turn began, used for
    /// `UsageUpdate.size` only until a generation reports its own window.
    ///
    /// It is the weaker of the two answers and deliberately the fallback: the
    /// turn walks its fallback candidates and resolves a fresh profile per
    /// candidate, so the model that answers is not always the one resolved
    /// here, and these limits would then divide one model's occupancy by
    /// another's ceiling. See `project::usage_figures`.
    caps: ModelCapabilities,
    root: PathBuf,
    /// Whether the failure below has already been reported.
    reported: Mutex<bool>,
    /// Whether a provider retry status is showing and must be cleared.
    retrying: Mutex<bool>,
    commands_generation: Mutex<Option<ego_core::event::skill::SkillCatalogueGeneration>>,
    context_commands: Mutex<Vec<ego_core::permit::ToolDescriptor>>,
}

impl AcpSink {
    pub fn new(
        connection: ConnectionTo<Client>,
        session: v1::SessionId,
        caps: ModelCapabilities,
        root: PathBuf,
    ) -> Self {
        AcpSink {
            connection,
            session,
            caps,
            root,
            reported: Mutex::new(false),
            retrying: Mutex::new(false),
            commands_generation: Mutex::new(None),
            context_commands: Mutex::new(Vec::new()),
        }
    }
}

impl AcpSink {
    fn available_commands(&self, state: &SessionState) -> Vec<v1::AvailableCommand> {
        let mut commands: Vec<_> = state
            .skill_catalogue()
            .descriptors()
            .iter()
            .filter(|descriptor| descriptor.user_invocable)
            .map(|descriptor| {
                let command = v1::AvailableCommand::new(
                    descriptor.name.clone(),
                    descriptor.description.clone(),
                );
                match &descriptor.argument_hint {
                    Some(hint) => command.input(v1::AvailableCommandInput::Unstructured(
                        v1::UnstructuredCommandInput::new(hint.clone()),
                    )),
                    None => command,
                }
            })
            .collect();
        commands.extend(
            self.context_commands
                .lock()
                .expect("context command lock")
                .iter()
                .map(|descriptor| {
                    let schema = serde_json::to_value(&descriptor.schema).unwrap_or_default();
                    v1::AvailableCommand::new(
                        descriptor.name.clone(),
                        descriptor.description.clone(),
                    )
                    .input(v1::AvailableCommandInput::Unstructured(
                        v1::UnstructuredCommandInput::new(format!("JSON arguments: {schema}")),
                    ))
                }),
        );
        commands
    }
}

impl EventSink for AcpSink {
    fn context_commands(
        &self,
        commands: Vec<ego_core::permit::ToolDescriptor>,
        state: &SessionState,
    ) {
        *self.context_commands.lock().expect("context command lock") = commands;
        let update = v1::SessionUpdate::AvailableCommandsUpdate(v1::AvailableCommandsUpdate::new(
            self.available_commands(state),
        ));
        if let Err(error) = self
            .connection
            .send_notification(v1::SessionNotification::new(self.session.clone(), update))
        {
            tracing::warn!(%error, "ACP context command announcement failed");
        }
    }

    fn publish(&self, event: &PublicEvent, state: &SessionState) {
        // The complete entry point includes reduced usage and tool progress.
        // That is why `publish` carries the reduction. See
        // `ARCHITECTURE.md`.
        let mut updates = Vec::new();
        {
            let mut generation = self
                .commands_generation
                .lock()
                .expect("command catalogue lock");
            if state.skill_catalogue_generation().is_some()
                && *generation != state.skill_catalogue_generation()
            {
                *generation = state.skill_catalogue_generation();
                let commands = self.available_commands(state);
                updates.push(v1::SessionUpdate::AvailableCommandsUpdate(
                    v1::AvailableCommandsUpdate::new(commands),
                ));
            }
        }
        {
            let mut retrying = self
                .retrying
                .lock()
                .expect("a poisoned lock means a publisher panicked mid-event");
            if let Some(status) = provider_retry_v1(event) {
                *retrying = true;
                updates.push(status);
            } else if *retrying && ends_retry_status(event) {
                *retrying = false;
                updates.push(provider_retry_cleared_v1());
            }
        }
        updates.extend(project_acp_updates_v1(event, state, &self.caps, &self.root));
        for update in updates {
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
}

/// Whether `event` ends a showing retry status: the next attempt produced an
/// item, or the turn ended (succeeded, failed after the last retry, or was
/// cancelled during the wait).
fn ends_retry_status(event: &PublicEvent) -> bool {
    matches!(
        event,
        PublicEvent::ItemStarted { .. }
            | PublicEvent::ItemDelta { .. }
            | PublicEvent::ItemCompleted { .. }
            | PublicEvent::TurnCompleted { .. }
    )
}
