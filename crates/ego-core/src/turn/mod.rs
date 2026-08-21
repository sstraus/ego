//! The turn loop: the one component that runs a turn and writes the log.
//!
//! See `docs/decisions/2026-08-21-the-turn-loop.md`. Named `turn` rather than
//! the plan's `loop`, which is a keyword and would spell every reference
//! `r#loop`.
//!
//! What a turn is, here: one user prompt, and then as many generations as it
//! takes to answer it — a generation that stops to call tools is followed by
//! their results and another generation, until one stops without calling
//! anything. `ARCHITECTURE.md` §4: this is thin async orchestration, and every
//! decision inside it is a pure function it calls.

mod journal;
mod tools;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;

use crate::budget::{BudgetGate, PricingQuote};
use crate::event::domain::{EventKind, ExitReason};
use crate::event::id::{EventId, GenerationId, ItemId, Producer, SessionId, TurnId};
use crate::event::time::Clock;
use crate::generator::attempt::{AttemptStream, GenerationAttempt};
use crate::generator::capabilities::ModelCapabilities;
use crate::generator::history::conversation_from;
use crate::generator::port::{AiEvent, Generator};
use crate::generator::types::{GenRequest, StopReason, ThinkingLevel};
use crate::public::PublicEvent;
use crate::state::SessionState;
use crate::store::session_store::{SessionStore, StoreError};

pub use journal::Journal;
pub use tools::Tools;

/// A cooperative cancellation signal
/// (`docs/decisions/2026-08-21-the-turn-loop.md` §4).
///
/// A concrete type and not a trait: there is exactly one way to implement
/// "someone asked to stop", and a trait would buy an indirection for it. Not
/// `tokio_util::sync::CancellationToken` either — `ego-core` has no tokio
/// dependency, and this needs an atomic and a `clone`, not a runtime.
///
/// Checked before each generation and after each stream event. The limit is
/// worth stating rather than discovering: a provider that has gone silent
/// delays cancellation until its transport gives up. Racing the await instead
/// would mean dropping a stream that owns an unsettled reservation, which is
/// precisely the leak [`AttemptStream`] exists to prevent.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the turn to stop at its next check.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The generation settings a turn does not derive from its history.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub model: String,
    pub thinking: ThinkingLevel,
    pub max_output_tokens: Option<u64>,
    /// Whether the model may ask for several tools at once. The loop runs
    /// them one after another either way — see `POTENTIALS.md` P22 for why
    /// that is currently the *more* faithful choice.
    pub parallel_tool_calls: bool,
}

/// What ended a turn, and everything it published.
#[derive(Debug)]
pub struct TurnOutcome {
    pub turn: TurnId,
    pub ok: bool,
    pub exit_reason: ExitReason,
    /// The privacy-filtered projection, in order. The internal log stays
    /// inside this crate; this is what a host is given.
    pub published: Vec<PublicEvent>,
}

/// A turn that could not be written down.
///
/// The only failure the loop reports to its caller. Everything else — a
/// refused budget, a provider error, a denied tool — is a fact *about* the
/// turn, so it goes in the log and comes back as an [`ExitReason`]. A store
/// failure is different in kind: the log is the product, and if it cannot be
/// written there is nothing to report through.
#[derive(Debug)]
pub struct TurnAbandoned {
    pub turn: TurnId,
    pub cause: StoreError,
    /// The state as it stood when the append failed — which still matches the
    /// durable log exactly, because [`Journal`] applies only after a
    /// successful append.
    pub state: SessionState,
}

/// One tool call the model asked for and the loop has not yet answered.
struct PendingCall {
    /// The item the call was logged against. `ToolCallCompleted` must name
    /// the same one, or the result attaches to nothing.
    item: ItemId,
    /// The `ToolCallStarted` event, named as the result's causal parent.
    started: EventId,
    name: String,
    args: String,
}

/// Where a session's events go, and who can stop it.
pub struct Session<'a> {
    pub id: SessionId,
    /// The session so far: `SessionState::default()` for a new one, a
    /// replayed state for a resumed one. Its `last_seq` is where the loop's
    /// numbering continues from.
    pub state: SessionState,
    pub store: &'a dyn SessionStore,
    pub clock: &'a dyn Clock,
    /// Held by the caller, not minted here: whoever wants to stop a turn is
    /// by definition not the turn.
    pub cancel: CancelToken,
}

/// What generates, and what a generation is allowed to cost.
pub struct Model<'a> {
    pub generator: &'a dyn Generator,
    pub gate: &'a mut BudgetGate,
    pub quote: PricingQuote,
    pub caps: ModelCapabilities,
    pub config: TurnConfig,
}

/// Runs turns against one session.
///
/// Holds the session's whole world: what generates, what it costs, what it may
/// call, and where the log goes. One `run` call is one turn; the state carries
/// over between them.
pub struct TurnLoop<'a> {
    journal: Journal<'a>,
    generator: &'a dyn Generator,
    gate: &'a mut BudgetGate,
    quote: PricingQuote,
    caps: ModelCapabilities,
    config: TurnConfig,
    tools: Tools<'a>,
    cancel: CancelToken,
}

impl<'a> TurnLoop<'a> {
    /// Open a loop over a session.
    ///
    /// Three arguments rather than ten loose ones: a turn is a session, a
    /// model and a set of tools, and at ten positional parameters — four of
    /// which were `&dyn` — a call site said nothing about which was which.
    pub fn new(session: Session<'a>, model: Model<'a>, tools: Tools<'a>) -> Self {
        Self {
            journal: Journal::new(
                session.id,
                session.store,
                session.clock,
                Producer::Main,
                session.state,
            ),
            generator: model.generator,
            gate: model.gate,
            quote: model.quote,
            caps: model.caps,
            config: model.config,
            tools,
            cancel: session.cancel,
        }
    }

    /// The session so far.
    pub fn state(&self) -> &SessionState {
        self.journal.state()
    }

    /// Take the session state back, to resume with a different loop.
    pub fn into_state(self) -> SessionState {
        self.journal.into_state()
    }

    /// Run one turn: the prompt, every generation it takes to answer it, and
    /// the tool calls in between.
    pub async fn run(&mut self, prompt: &str) -> Result<TurnOutcome, TurnAbandoned> {
        let turn = TurnId::new();
        let published_before = self.journal.published().len();

        let (ok, exit_reason) = match self.drive(turn, prompt).await {
            Ok(reason) => (matches!(reason, ExitReason::Completed), reason),
            Err(cause) => return Err(self.abandon(turn, cause)),
        };

        if let Err(cause) = self.journal.append(
            turn,
            None,
            None,
            EventKind::TurnCompleted { ok, exit_reason },
        ) {
            return Err(self.abandon(turn, cause));
        }

        Ok(TurnOutcome {
            turn,
            ok,
            exit_reason,
            published: self.journal.published()[published_before..].to_vec(),
        })
    }

    /// Give up on a turn whose log could not be written.
    ///
    /// Takes the state by clone rather than by move: `run` borrows `self`, and
    /// a caller holding a `TurnLoop` is entitled to look at what survived.
    fn abandon(&self, turn: TurnId, cause: StoreError) -> TurnAbandoned {
        tracing::error!(?turn, %cause, "turn abandoned: its log could not be written");
        TurnAbandoned {
            turn,
            cause,
            state: self.journal.state().clone(),
        }
    }

    /// Everything between the prompt and `TurnCompleted`.
    async fn drive(&mut self, turn: TurnId, prompt: &str) -> Result<ExitReason, StoreError> {
        self.journal.append(
            turn,
            Some(ItemId::new()),
            None,
            EventKind::UserPrompt {
                text: prompt.to_string(),
            },
        )?;

        loop {
            if self.cancel.is_cancelled() {
                return Ok(ExitReason::Cancelled);
            }

            let (reason, pending) = match self.generate(turn).await? {
                Generated::Ended(reason) => return Ok(reason),
                Generated::Stopped { reason, pending } => (reason, pending),
            };

            // A generation that says it stopped for tools and then names none
            // has nothing to answer. Generating again on that evidence loops
            // forever against a provider that keeps saying it.
            if reason != StopReason::ToolUse || pending.is_empty() {
                return Ok(match reason {
                    StopReason::Stop | StopReason::ToolUse => ExitReason::Completed,
                    StopReason::Length => ExitReason::Budget,
                    StopReason::Aborted => ExitReason::Cancelled,
                    StopReason::Error | StopReason::Deferred => ExitReason::Error,
                });
            }

            for call in pending {
                let (ok, output) = self.run_tool(&call).await;
                self.journal.append(
                    turn,
                    Some(call.item),
                    Some(call.started),
                    EventKind::ToolCallCompleted { ok, output },
                )?;
            }
        }
    }

    /// One generation: reserve, stream it into the log, settle.
    async fn generate(&mut self, turn: TurnId) -> Result<Generated, StoreError> {
        let (conversation, repairs) = conversation_from(self.journal.state());
        if !repairs.is_empty() {
            // Not a durable event: whether a repair becomes one is
            // `POTENTIALS.md` P2, and it is waiting on a decision rather than
            // on an implementation.
            tracing::warn!(
                ?repairs,
                "the resumed history was repaired before it could be sent"
            );
        }

        let request = GenRequest {
            model: self.config.model.clone(),
            messages: conversation,
            tools: self.tools.descriptors(),
            thinking: self.config.thinking,
            max_output_tokens: self.config.max_output_tokens,
            parallel_tool_calls: self.config.parallel_tool_calls,
            // Nothing carries reasoning across generations yet: `ego-core`
            // never fabricates one, and no wire format produces one to carry.
            reasoning_state: None,
        };

        let reservation = match self.gate.reserve(&self.quote, &request, &self.caps) {
            Ok(reservation) => reservation,
            Err(denied) => {
                tracing::warn!(%denied, "the budget gate refused a generation");
                self.journal.append(
                    turn,
                    None,
                    None,
                    EventKind::Error {
                        message: denied.to_string(),
                    },
                )?;
                return Ok(Generated::Ended(ExitReason::Budget));
            }
        };

        let attempt = match GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &self.quote,
            &request,
            &self.caps,
        ) {
            Ok(attempt) => attempt,
            Err(denied) => {
                // The reservation went into the error and is not released.
                // Loud, and the turn stops: an authorisation that does not
                // authorise its own request is a defect in this loop, not a
                // condition to work around.
                tracing::error!(%denied, "a reservation did not authorise the request it was minted for");
                self.journal.append(
                    turn,
                    None,
                    None,
                    EventKind::Error {
                        message: denied.to_string(),
                    },
                )?;
                return Ok(Generated::Ended(ExitReason::Error));
            }
        };

        let reference = attempt.attempt();
        self.journal.append(
            turn,
            None,
            None,
            EventKind::GenerationStarted { attempt: reference },
        )?;

        let stream = self.generator.generate(request, attempt);
        let drained = self.drain(turn, stream).await;

        // Settle before reporting anything, including a store failure: the
        // stream owns the reservation, and returning early would drop it.
        let settlement = match drained.stream.settle(self.gate) {
            Ok(settlement) => settlement,
            Err(denied) => {
                tracing::error!(%denied, "the gate refused to settle an attempt it minted");
                drained.failure.map_or(Ok(()), Err)?;
                self.journal.append(
                    turn,
                    None,
                    None,
                    EventKind::Error {
                        message: denied.to_string(),
                    },
                )?;
                return Ok(Generated::Ended(ExitReason::Error));
            }
        };
        drained.failure.map_or(Ok(()), Err)?;

        self.journal.append(
            turn,
            None,
            None,
            EventKind::AttemptSettled {
                attempt: settlement.attempt,
                outcome: settlement.outcome,
            },
        )?;

        if drained.cancelled {
            return Ok(Generated::Ended(ExitReason::Cancelled));
        }
        Ok(Generated::Stopped {
            reason: settlement.outcome.stop(),
            pending: drained.pending,
        })
    }

    /// Turn one generation's stream into log entries.
    ///
    /// Returns the stream so its caller can settle it. A store failure is
    /// carried back rather than propagated with `?` for the same reason:
    /// leaving here early would drop an unsettled attempt.
    async fn drain(&mut self, turn: TurnId, mut stream: AttemptStream) -> Drained {
        let reference = stream.attempt();
        let mut pending = Vec::new();
        let mut message: Option<ItemId> = None;
        let mut reasoning: Option<ItemId> = None;
        let mut assembled = String::new();
        let mut failure = None;
        let mut cancelled = false;

        while let Some(event) = stream.next().await {
            let appended = match event {
                AiEvent::TextDelta { text } => {
                    let item = *message.get_or_insert_with(ItemId::new);
                    assembled.push_str(&text);
                    self.journal
                        .append(turn, Some(item), None, EventKind::AssistantDelta { text })
                        .map(|_| ())
                }
                AiEvent::ReasoningDelta { text } => {
                    let item = *reasoning.get_or_insert_with(ItemId::new);
                    self.journal
                        .append(turn, Some(item), None, EventKind::ReasoningDelta { text })
                        .map(|_| ())
                }
                AiEvent::ToolCall {
                    call_id,
                    name,
                    args,
                } => {
                    let item = ItemId::new();
                    self.journal
                        .append(
                            turn,
                            Some(item),
                            None,
                            EventKind::ToolCallStarted {
                                name: name.clone(),
                                args: args.clone(),
                                // Carried, never dropped: nothing local can
                                // stand in for a provider-issued id, and
                                // without it the next request cannot say
                                // which call a result answers.
                                call_id: Some(call_id),
                            },
                        )
                        .map(|started| {
                            pending.push(PendingCall {
                                item,
                                started,
                                name,
                                args,
                            });
                        })
                }
                AiEvent::Usage(usage) => self
                    .journal
                    .append(
                        turn,
                        None,
                        None,
                        EventKind::UsageUpdated {
                            usage,
                            // Priced with the same quote the gate reserved
                            // against. A cost computed under another revision
                            // cannot be reconciled with the authorisation
                            // that admitted the request.
                            cost_delta: self.quote.price(&usage),
                            attempt: Some(reference),
                        },
                    )
                    .map(|_| ()),
                AiEvent::Error { message } => self
                    .journal
                    .append(turn, None, None, EventKind::Error { message })
                    .map(|_| ()),
                // Folded by `AttemptStream` into the outcome, which is what
                // `AttemptSettled` carries. A second spelling in the log
                // would be a second place for it to disagree.
                AiEvent::DeliveryObserved(_) | AiEvent::Stop(_) => Ok(()),
            };

            if let Err(cause) = appended {
                failure = Some(cause);
                break;
            }
            if self.cancel.is_cancelled() {
                cancelled = true;
                break;
            }
        }

        // The assembled message closes the item the deltas opened: the
        // reducer replaces rather than appends, so this is the same text, and
        // it is what tells a host the item finished.
        if failure.is_none()
            && let Some(item) = message
        {
            if let Err(cause) = self.journal.append(
                turn,
                Some(item),
                None,
                EventKind::AssistantMessage { text: assembled },
            ) {
                failure = Some(cause);
            }
        }

        Drained {
            stream,
            pending,
            failure,
            cancelled,
        }
    }

    /// Look up, authorise and run one call the model asked for.
    ///
    /// Every failure comes back as `(false, prose)` and is logged as a failed
    /// tool result rather than raised. The model is the audience: it asked for
    /// something that did not happen, and the only useful thing to do with
    /// that is tell it why.
    async fn run_tool(&self, call: &PendingCall) -> (bool, String) {
        let (tool, prepared) = match self.tools.prepare(&call.name, &call.args) {
            Ok(prepared) => prepared,
            Err(refusal) => return (false, refusal),
        };
        let permit = match self.tools.authorize(&prepared) {
            Ok(permit) => permit,
            Err(denial) => return (false, denial),
        };
        match self.tools.execute(tool, prepared, &permit).await {
            Ok(output) => (true, output),
            Err(failure) => (false, failure),
        }
    }
}

/// What one generation did.
enum Generated {
    /// The turn is over, for a reason that is not the model's stop reason.
    Ended(ExitReason),
    /// The generation finished. `pending` is what it asked to call.
    Stopped {
        reason: StopReason,
        pending: Vec<PendingCall>,
    },
}

/// A drained stream, handed back so it can be settled.
struct Drained {
    stream: AttemptStream,
    pending: Vec<PendingCall>,
    failure: Option<StoreError>,
    cancelled: bool,
}
