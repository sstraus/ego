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
use std::sync::atomic::{AtomicU8, Ordering};

use futures::StreamExt;

use crate::budget::{BudgetGate, Metering, PricingQuote, ResolvedRates, SettlementSource};
use crate::ceiling::tokens_reached;
use crate::config::SystemPrompt;
use crate::cost::{Money, RateSource};
use crate::event::domain::{CancelCause, EventKind, ExitReason, OccupancyProvenance};
use crate::event::id::{EventId, GenerationId, ItemId, Producer, SessionId, TurnId};
use crate::event::time::Clock;
use crate::generator::attempt::{AttemptStream, GenerationAttempt, UsageReport, settled_cost};
use crate::generator::capabilities::ModelCapabilities;
use crate::generator::history::conversation_from;
use crate::generator::port::{AiEvent, Generator};
use crate::generator::types::{
    Conversation, ErrorKind, GenRequest, Message, Role, StopReason, ThinkingLevel,
};
use crate::permit::PolicyDenied;
use crate::public::PublicEvent;
use crate::sink::EventSink;
use crate::state::{SessionState, Usage};
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
///
/// Carries *who* asked, not just that someone did: a timeout and a `SIGINT`
/// are different process exits (124 and 130), and a token that only said
/// "cancelled" would make them the same value. An `AtomicU8` rather than a
/// second field, so the promise above — an atomic and a `clone`, no runtime —
/// still holds.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicU8>);

const NOT_CANCELLED: u8 = 0;
const BY_USER: u8 = 1;
const BY_TIMEOUT: u8 = 2;
const BY_HOST: u8 = 3;

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the turn to stop at its next check, saying who is asking.
    ///
    /// **The first cause wins.** A `SIGINT` followed a moment later by a
    /// deadline is a run the user stopped, and reporting the timeout because
    /// it stored last would be the wrong account of what happened.
    pub fn cancel(&self, cause: CancelCause) {
        let code = match cause {
            CancelCause::User => BY_USER,
            CancelCause::Timeout => BY_TIMEOUT,
            CancelCause::Host => BY_HOST,
        };
        let _ = self
            .0
            .compare_exchange(NOT_CANCELLED, code, Ordering::SeqCst, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst) != NOT_CANCELLED
    }

    /// Why the turn was asked to stop, if it was.
    pub fn cause(&self) -> Option<CancelCause> {
        match self.0.load(Ordering::SeqCst) {
            BY_USER => Some(CancelCause::User),
            BY_TIMEOUT => Some(CancelCause::Timeout),
            BY_HOST => Some(CancelCause::Host),
            _ => None,
        }
    }
}

/// The generation settings a turn does not derive from its history.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub model: String,
    /// The system prompt, prepended to every generation's conversation.
    ///
    /// Here and not in the log, because it is not something that *happened*:
    /// it is a setting the whole turn is generated under, and writing it into
    /// the history as a message would make a resumed session rebuild it as if
    /// someone had said it. Assembled by the harness —
    /// `config::system_prompt` for instructions, `SystemPrompt::verbatim` for
    /// `--system`.
    ///
    /// A [`SystemPrompt`] and not a `String`, so the text sent and the
    /// descriptors published in `PublicEvent::SessionStarted` are one value
    /// with one origin. A bare string would let a harness send one thing and
    /// publish the digest of another, and nothing could check it.
    pub system: Option<SystemPrompt>,
    pub thinking: ThinkingLevel,
    pub max_output_tokens: Option<u64>,
    /// Whether the model may ask for several tools at once. The loop runs
    /// them one after another either way — see `POTENTIALS.md` P22 for why
    /// that is currently the *more* faithful choice.
    pub parallel_tool_calls: bool,
}

/// What ended a turn.
///
/// No event list. Events reach their consumer through the
/// [`EventSink`](crate::sink::EventSink) as they happen, because `CLI.md`
/// §4.2 says *live* and a vector handed back here could only ever be a
/// batch. A caller that wants the batch anyway supplies a
/// [`CollectingSink`](crate::sink::CollectingSink) and takes it from there.
#[derive(Debug)]
pub struct TurnOutcome {
    pub turn: TurnId,
    pub ok: bool,
    pub exit_reason: ExitReason,
    /// What was refused, when `exit_reason` is
    /// [`ExitReason::PermissionRequired`]. `CLI.md` §4.1 requires the denied
    /// action in the result: a script handed exit 3 and no subject has been
    /// told nothing it can act on.
    pub denied: Option<String>,
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
    /// Where the public projection goes as it happens. A session-level
    /// concern beside the store and the clock, because it outlives any one
    /// turn: a host subscribes once and watches the whole session.
    pub sink: &'a dyn EventSink,
    /// Held by the caller, not minted here: whoever wants to stop a turn is
    /// by definition not the turn.
    pub cancel: CancelToken,
}

/// What generates, and what a generation is allowed to cost.
pub struct Model<'a> {
    pub generator: &'a dyn Generator,
    pub gate: &'a mut BudgetGate,
    pub metering: Metering,
    /// The rates this row was resolved with, and where they came from, when
    /// a ceiling made anybody look them up. `None` is the ordinary case for a
    /// run with no `--budget`: `2026-08-21-a-run-with-no-ceiling.md` §1
    /// consults no rate source at all unless a ceiling was configured.
    pub rates: Option<ResolvedRates>,
    /// A ceiling on this session's cumulative billed tokens, when one was
    /// configured.
    ///
    /// Beside the gate rather than inside it, because it is not money:
    /// `2026-08-21-a-run-with-no-ceiling.md` §9 — it needs no rates, no fetch
    /// and no network, and it binds on every metering class including the two
    /// a monetary ceiling cannot touch. It is what an air-gapped operator, or
    /// one who does not care to trust a third-party price table, reaches for.
    pub token_ceiling: Option<u64>,
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
    metering: Metering,
    rates: Option<ResolvedRates>,
    token_ceiling: Option<u64>,
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
                session.sink,
                Producer::Main,
                session.state,
            ),
            generator: model.generator,
            gate: model.gate,
            metering: model.metering,
            rates: model.rates,
            token_ceiling: model.token_ceiling,
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
        // `turn.started` of `CLI.md` §4.2. Published rather than appended:
        // `EventKind` has no counterpart, because the start of a turn is not
        // something the reducer establishes — it is what the loop knows the
        // instant it mints the id. Before this, nothing in the workspace
        // could emit it at all.
        self.journal.publish(&PublicEvent::TurnStarted { turn });

        let (exit_reason, denied) = match self.drive(turn, prompt).await {
            Ok(ended) => ended,
            Err(cause) => return Err(self.abandon(turn, cause)),
        };
        let ok = matches!(exit_reason, ExitReason::Completed);

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
            denied,
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

    /// The rates themselves, without their provenance — what the arithmetic
    /// takes. Where a rate came from must not change what it computes.
    fn quote(&self) -> Option<&PricingQuote> {
        self.rates.as_ref().map(|rates| &rates.quote)
    }

    /// Where the rates came from, or [`RateSource::None`] when nobody looked
    /// any up. Published, never used to price anything.
    fn rate_source(&self) -> &RateSource {
        self.rates
            .as_ref()
            .map_or(&RateSource::None, |rates| &rates.source)
    }

    /// Everything between the prompt and `TurnCompleted`.
    ///
    /// Returns the reason and, when that reason is
    /// [`ExitReason::PermissionRequired`], the action that was refused.
    async fn drive(
        &mut self,
        turn: TurnId,
        prompt: &str,
    ) -> Result<(ExitReason, Option<String>), StoreError> {
        self.journal.append(
            turn,
            Some(ItemId::new()),
            None,
            EventKind::UserPrompt {
                text: prompt.to_string(),
            },
        )?;

        loop {
            if let Some(cause) = self.cancel.cause() {
                return Ok((ExitReason::Cancelled(cause), None));
            }
            // Beside the cancel check and for the same reason: this is the
            // boundary between generations, the only place a turn can be
            // stopped without abandoning an attempt that is already paid for.
            //
            // Post-generation where money is pre-generation, and the
            // asymmetry is principled (§9). Money must be held before the
            // request because once a provider reports what a generation cost
            // the money is spent; tokens differ in exactly that property —
            // on an unmetered or subscription row no charge is raised at all.
            // `CLI.md:128` spells it the same way: "exit 5 when hit", not
            // "when it would be hit".
            if tokens_reached(self.token_ceiling, &self.journal.state().usage) {
                return Ok((ExitReason::Budget, None));
            }

            let (reason, pending) = match self.generate(turn).await? {
                Generated::Ended(reason) => return Ok((reason, None)),
                Generated::Stopped { reason, pending } => (reason, pending),
            };

            // A generation that says it stopped for tools and then names none
            // has nothing to answer. Generating again on that evidence loops
            // forever against a provider that keeps saying it.
            if reason != StopReason::ToolUse || pending.is_empty() {
                return Ok((
                    match reason {
                        StopReason::Stop | StopReason::ToolUse => ExitReason::Completed,
                        StopReason::Length => ExitReason::Budget,
                        // Not `Cancelled`: our own cancellation was already
                        // answered above and at `drained.cancelled`, so an
                        // `Aborted` reaching here is a stream that ended with no
                        // terminal frame — a provider that went silent.
                        StopReason::Aborted => ExitReason::Provider,
                        // `Deferred` is upstream choosing a delivery mode this
                        // wire format does not consume. Not our defect, so not
                        // `Internal`; the provider decided it, so it reads as an
                        // upstream condition like any other.
                        StopReason::Error | StopReason::Deferred => ExitReason::Provider,
                    },
                    None,
                ));
            }

            for call in pending {
                let (ok, output) = match self.run_tool(&call).await {
                    ToolOutcome::Answered { ok, output } => (ok, output),
                    // `CLI.md` §4.4: a run with nobody to ask does not answer
                    // a permission question on the model's behalf. The denial
                    // is still logged against the call — the model's audience
                    // argument holds for the *record*, it just no longer
                    // decides whether the run continues.
                    ToolOutcome::Refused { denial } => {
                        let action = denial.to_string();
                        self.journal.append(
                            turn,
                            Some(call.item),
                            Some(call.started),
                            EventKind::ToolCallCompleted {
                                ok: false,
                                output: action.clone(),
                            },
                        )?;
                        return Ok((ExitReason::PermissionRequired, Some(action)));
                    }
                };
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
        let (history, repairs) = conversation_from(self.journal.state());
        let conversation = self.with_system(history);
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
        };

        // Before the gate, deliberately: a reservation taken for a request
        // the model will reject has authorised spending on something that was
        // never going to be sent
        // (`docs/decisions/2026-08-21-a-request-the-model-will-not-take.md`).
        let gaps = self.caps.request_gaps(&request);
        if !gaps.is_empty() {
            tracing::warn!(
                ?gaps,
                "the model does not accept the request this run asks for"
            );
            self.journal.append(
                turn,
                None,
                None,
                EventKind::Error {
                    message: gaps
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; "),
                    // The request is unsendable and this run decided so
                    // before a socket existed; no provider classified it.
                    kind: ErrorKind::Unclassified,
                },
            )?;
            return Ok(Generated::Ended(ExitReason::Usage));
        }

        // Per generation, and before the gate: a fallback chain can switch
        // rows mid-turn, and on a row that is not metered in currency the
        // gate is about to mint a zero reservation — which means a monetary
        // `--budget` admits everything. The operator is told that here rather
        // than discovering it from a limit that never fired.
        self.journal.publish(&PublicEvent::metering_selected(
            turn,
            &self.metering,
            self.gate.ledger(),
            self.rate_source(),
        ));

        let quote = self.rates.as_ref().map(|rates| rates.quote.clone());
        let reservation =
            match self
                .gate
                .reserve(&self.metering, quote.as_ref(), &request, &self.caps)
            {
                Ok(reservation) => reservation,
                Err(denied) => {
                    tracing::warn!(%denied, "the budget gate refused a generation");
                    self.journal.append(
                        turn,
                        None,
                        None,
                        EventKind::Error {
                            message: denied.to_string(),
                            // Ours, not the provider's: nothing was sent.
                            kind: ErrorKind::Unclassified,
                        },
                    )?;
                    return Ok(Generated::Ended(ExitReason::Budget));
                }
            };

        let attempt = match GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &self.metering,
            self.quote(),
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
                        // Ours, not the provider's: `ErrorKind` classifies
                        // what a provider said, and nothing was sent.
                        kind: ErrorKind::Unclassified,
                    },
                )?;
                return Ok(Generated::Ended(ExitReason::Internal));
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
                        // Ours, not the provider's: `ErrorKind` classifies
                        // what a provider said, and nothing was sent.
                        kind: ErrorKind::Unclassified,
                    },
                )?;
                return Ok(Generated::Ended(ExitReason::Internal));
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

        // `CLI.md` §4.1's `context`, and the only place it can come from: a
        // generation boundary is where occupancy is observable, and the loop
        // is the only thing that sees one. The figure is the provider's own
        // report for the generation that just finished — the prompt it
        // counted plus what it produced — which is what
        // `OccupancyProvenance::GenerationBoundary` names, proxy and all.
        //
        // An attempt that reported nothing appends nothing. `ContextWindow`
        // holds an `Option` precisely so that "nobody has told us" is not
        // spelled `0`, and a fabricated zero would be indistinguishable from
        // an empty context.
        if let UsageReport::Reported(usage) = settlement.outcome.usage() {
            self.journal.append(
                turn,
                None,
                None,
                EventKind::ContextWindowUpdated {
                    used_tokens: usage.total_tokens(),
                    provenance: OccupancyProvenance::GenerationBoundary,
                },
            )?;
        }

        if drained.cancelled {
            return Ok(Generated::Ended(ExitReason::Cancelled(
                self.cancel.cause().unwrap_or(CancelCause::User),
            )));
        }
        Ok(Generated::Stopped {
            reason: settlement.outcome.stop(),
            pending: drained.pending,
        })
    }

    /// The rebuilt history with this turn's system prompt in front of it.
    ///
    /// Rebuilt through [`Conversation::new`] rather than pushed into: a
    /// `Conversation` is a *checked* history, and there is deliberately no way
    /// to add a message to one without checking it again. The check cannot
    /// fail here — a block of text makes no tool call and answers none, so a
    /// history that validated a moment ago still does — which is what the
    /// `expect` says.
    fn with_system(&self, history: Conversation) -> Conversation {
        let Some(system) = self.config.system.as_ref() else {
            return history;
        };
        let mut messages: Vec<Message> = history.into();
        messages.insert(0, Message::text(Role::System, &system.text));
        Conversation::new(messages)
            .expect("a text message adds no call and answers none, so a valid history stays valid")
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
        // Under a provider-reported settlement the tokens and the money
        // arrive as two frames, and `EventKind::UsageUpdated` carries them
        // together because they are one observation. So both are held and
        // booked once, when the stream is done — which also removes any
        // dependence on the order a decoder chose to emit them in.
        let held = matches!(
            self.metering,
            Metering::Currency {
                settlement: SettlementSource::ProviderReported { .. },
                ..
            }
        );
        let mut held_usage: Option<Usage> = None;
        let mut stated_cost: Option<Money> = None;
        let settlement_currency = self.gate.currency();

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
                // INCOMPLETE: this must journal the state against the open
                // reasoning item and then close it, so the next
                // `ReasoningDelta` opens a new one. Both halves need
                // `EventKind::ReasoningState`, which does not exist yet;
                // closing the item without recording why would lose the state
                // rather than defer it.
                AiEvent::ReasoningState(_) => Ok(()),
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
                AiEvent::Usage(usage) if held => {
                    let mut total = held_usage.unwrap_or(Usage::IDENTITY);
                    total.add(&usage);
                    held_usage = Some(total);
                    Ok(())
                }
                AiEvent::Usage(usage) => self
                    .journal
                    .append(
                        turn,
                        None,
                        None,
                        EventKind::UsageUpdated {
                            usage,
                            // Settled with the same metering the gate
                            // reserved against, through the one function
                            // `AttemptStream::settle` also uses. A cost the
                            // log computed differently from the gate is a
                            // cost the two can disagree about.
                            cost_delta: settled_cost(
                                &self.metering,
                                self.quote(),
                                UsageReport::Reported(usage),
                                None,
                                settlement_currency,
                            ),
                            attempt: Some(reference),
                        },
                    )
                    .map(|_| ()),
                // A total for the request, so the last statement replaces
                // the previous one rather than adding to it — the rule
                // `AttemptStream` folds it under.
                AiEvent::ProviderCost { cost } => {
                    stated_cost = Some(cost);
                    Ok(())
                }
                AiEvent::Error { message, kind } => self
                    .journal
                    .append(turn, None, None, EventKind::Error { message, kind })
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

        // What was held under a provider-reported settlement, booked once.
        //
        // Only when something arrived. A generation that reported neither
        // tokens nor a cost — a request that never left, say — books nothing,
        // exactly as it did before; the gate still charges it the full
        // reserved bound at settlement, which is where that belongs.
        if failure.is_none() && held && (held_usage.is_some() || stated_cost.is_some()) {
            let usage = held_usage.unwrap_or(Usage::IDENTITY);
            let cost_delta = settled_cost(
                &self.metering,
                self.quote(),
                held_usage.map_or(UsageReport::Missing, UsageReport::Reported),
                stated_cost,
                settlement_currency,
            );
            if let Err(cause) = self.journal.append(
                turn,
                None,
                None,
                EventKind::UsageUpdated {
                    usage,
                    cost_delta,
                    attempt: Some(reference),
                },
            ) {
                failure = Some(cause);
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
    /// A *failure* comes back as `Answered { ok: false, .. }` and is logged as
    /// a failed tool result rather than raised. The model is the audience: it
    /// asked for something that did not happen, and the only useful thing to
    /// do is tell it why.
    ///
    /// A *refusal by policy* is not that, and this is where the two parted
    /// company. A denial is a question — "may this happen?" — and answering it
    /// on the model's behalf is the thing `CLI.md` §4.4 forbids in a run with
    /// nobody to ask. It comes back typed, and the caller decides.
    async fn run_tool(&self, call: &PendingCall) -> ToolOutcome {
        let answered = |ok, output| ToolOutcome::Answered { ok, output };
        let (tool, prepared) = match self.tools.prepare(&call.name, &call.args) {
            Ok(prepared) => prepared,
            Err(refusal) => return answered(false, refusal),
        };
        let authorized = match self.tools.authorize(prepared) {
            Ok(authorized) => authorized,
            Err(denial) => return ToolOutcome::Refused { denial },
        };
        match self.tools.execute(tool, authorized).await {
            Ok(output) => answered(true, output),
            Err(failure) => answered(false, failure),
        }
    }
}

/// What running one tool call produced.
enum ToolOutcome {
    /// It ran, or it failed in a way the model should read and adapt to.
    Answered { ok: bool, output: String },
    /// Policy refused it. Not an answer — a question nobody in this run can
    /// be asked, so it is the caller's to decide, not this method's.
    Refused { denial: PolicyDenied },
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
