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

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use futures::StreamExt;

use crate::budget::{BudgetGate, Metering, PricingQuote, ResolvedRates, SettlementSource};
use crate::ceiling::tokens_reached;
use crate::config::SystemPrompt;
use crate::cost::{Money, RateSource};
use crate::event::domain::{
    CancelCause, EventKind, ExitReason, GenerationRole, OccupancyProvenance,
};
use crate::event::id::{EventId, GenerationId, ItemId, Producer, SessionId, TurnId};
use crate::event::time::Clock;
use crate::generator::attempt::{AttemptStream, GenerationAttempt, UsageReport, settled_cost};
use crate::generator::capabilities::ModelCapabilities;
use crate::generator::history::conversation_from;
use crate::generator::port::{AiEvent, Generator};
use crate::generator::types::{
    Conversation, ErrorKind, GenRequest, Message, Role, StopReason, ThinkingLevel,
};
use crate::permit::ExecObservation;
use crate::permit::judge::{JudgeConfig, JudgeDecoder, JudgeInput, JudgeOutcome};
use crate::permit::{Authority, Classification, Pending, PolicyDenied, Resolution, ToolResult};
use crate::public::PublicEvent;
use crate::run::{Denial, DeniedBy};
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
    /// Whether the model may ask for several tools at once.
    ///
    /// The loop runs them one after another either way, and that is now a
    /// scheduling choice rather than a fidelity one: the calls of a single
    /// generation are recorded as such and rebuild into one assistant turn
    /// however they were executed. See
    /// `docs/decisions/2026-08-22-the-generation-a-call-came-out-of.md`.
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
    /// What was refused and by whom, when `exit_reason` is
    /// [`ExitReason::PermissionRequired`]. `CLI.md` §4.1 requires the denied
    /// action in the result: a script handed exit 3 and no subject has been
    /// told nothing it can act on. §4.4's four remedies need the authority
    /// too, which is why this is one [`Denial`] and not a string.
    pub denied: Option<Denial>,
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

/// The separately resolved model and host facts used by auto-mode judging.
pub struct JudgeModel<'a> {
    pub generator: &'a dyn Generator,
    pub metering: Metering,
    pub rates: Option<ResolvedRates>,
    pub caps: ModelCapabilities,
    pub config: JudgeConfig,
    pub confinement: crate::permit::RunConfinement,
    pub exec_caps: crate::permit::ExecCaps,
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
    judge: Option<JudgeModel<'a>>,
    execution_observation: Option<ExecObservation>,
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
            judge: None,
            execution_observation: None,
            tools,
            cancel: session.cancel,
        }
    }

    /// Open an auto-capable loop with its independently resolved judge.
    pub fn with_judge(
        session: Session<'a>,
        model: Model<'a>,
        tools: Tools<'a>,
        judge: JudgeModel<'a>,
    ) -> Self {
        let mut loop_ = Self::new(session, model, tools);
        loop_.judge = Some(judge);
        loop_
    }

    /// Attach one run observation, consumed before the first public turn event.
    pub fn with_execution_observation(mut self, observation: ExecObservation) -> Self {
        self.execution_observation = Some(observation);
        self
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
        if let Some(observation) = self.execution_observation.take()
            && let Err(cause) = self.journal.append(
                turn,
                None,
                None,
                EventKind::ExecutionObserved { observation },
            )
        {
            return Err(self.abandon(turn, cause));
        }
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
    /// [`ExitReason::PermissionRequired`], what was refused and by whom.
    async fn drive(
        &mut self,
        turn: TurnId,
        prompt: &str,
    ) -> Result<(ExitReason, Option<Denial>), StoreError> {
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

            let (reason, pending, error_kind) = match self.generate(turn).await? {
                Generated::Ended(reason) => return Ok((reason, None)),
                Generated::Stopped {
                    reason,
                    pending,
                    error_kind,
                } => (reason, pending, error_kind),
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
                        StopReason::Deferred => ExitReason::Provider,
                        // The one place a provider's classification changes
                        // where the run lands. An `Invalid` is a request that
                        // will be wrong again — schema, context length, an
                        // unsupported parameter — so it is exit 2 and not
                        // exit 4: `CLI.md:215` tells a script to retry on 4,
                        // and a request that cannot succeed is precisely what
                        // it must not retry.
                        //
                        // Everything else is exit 4. `Unclassified` lands
                        // there by failing closed — nothing was read that
                        // says otherwise, and one wasted attempt is cheaper
                        // than a wrong exit code.
                        //
                        // `Refused` is knowingly imperfect: a `401` or an
                        // exhausted credit balance will not succeed on retry
                        // either, but `ExitReason` has no value for "the
                        // credential, not the request", and inventing one is
                        // a change to `CLI.md`'s published exit codes rather
                        // than to this mapping. Recorded here so the next
                        // reader knows it was seen and not missed.
                        StopReason::Error => match error_kind {
                            Some(ErrorKind::Invalid) => ExitReason::Usage,
                            _ => ExitReason::Provider,
                        },
                    },
                    None,
                ));
            }

            for call in pending {
                // `CLI.md` §4.4: a run with nobody to ask does not answer a
                // permission question on the model's behalf. The denial is
                // still logged against the call — the model's audience
                // argument holds for the *record*, it just no longer decides
                // whether the run continues.
                //
                // Both endings below are exit 3 and they are not the same
                // fact. "Denied by r1:…" tells an operator which line of their
                // config to go and look at; "no rule covers write /ws/x" tells
                // them there is no line to look at and they have to write one.
                // Collapsing them would leave the commonest case — a run that
                // needed an answer — reading like a refusal somebody
                // configured on purpose. `DeniedBy` is what carries that
                // difference past the prose and into §4.1's object.
                let denial = match self.run_tool(turn, &call).await? {
                    ToolOutcome::Answered { ok, result } => {
                        self.journal.append(
                            turn,
                            Some(call.item),
                            Some(call.started),
                            EventKind::ToolCallCompleted { ok, result },
                        )?;
                        continue;
                    }
                    ToolOutcome::Refused { denial } => Denial {
                        action: denial.to_string(),
                        // Never assembled here: `PolicyDenied::authority` is
                        // the single definition of who said so, and a second
                        // caller deciding it from the arms would be a second
                        // chance to attribute a denial to the wrong party.
                        by: DeniedBy::Decided(denial.authority()),
                    },
                    ToolOutcome::Unanswerable { pending } => Denial {
                        action: pending.request().to_string(),
                        by: DeniedBy::Nobody,
                    },
                    ToolOutcome::Ended(reason) => return Ok((reason, None)),
                };
                self.journal.append(
                    turn,
                    Some(call.item),
                    Some(call.started),
                    EventKind::ToolCallCompleted {
                        ok: false,
                        result: ToolResult::builtin(denial.action.clone()),
                    },
                )?;
                return Ok((ExitReason::PermissionRequired, Some(denial)));
            }
        }
    }

    /// One generation: reserve, stream it into the log, settle.
    async fn generate(&mut self, turn: TurnId) -> Result<Generated, StoreError> {
        // A log this build wrote cannot fail here: the guard in `drain` refuses
        // a reused call id before it is ever appended. A log written before
        // that guard existed can, and there is nothing to do about it from
        // here — the log is never rewritten, so the run ends visibly at the
        // provider's exit code rather than resuming a session with a hole in
        // it (ADR §6).
        let (history, repairs) = match conversation_from(self.journal.state()) {
            Ok(rebuilt) => rebuilt,
            // `Provider` is the exit even though the history on disk is what
            // broke and no provider was asked: §4.3's exit 4 is "the run could
            // not get a generation", and this run cannot.
            Err(malformed) => {
                tracing::warn!(%malformed, "the stored history cannot be rebuilt into a request");
                return self.refuse(
                    turn,
                    format!(
                        "this session's log cannot be sent to a provider and \
                         cannot be repaired: {malformed}"
                    ),
                    ExitReason::Provider,
                );
            }
        };
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
            return self.refuse(
                turn,
                gaps.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
                ExitReason::Usage,
            );
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
                    return self.refuse(turn, denied.to_string(), ExitReason::Budget);
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
                return self.refuse(turn, denied.to_string(), ExitReason::Internal);
            }
        };

        let reference = attempt.attempt();
        // Kept, not discarded: this is the id every item from this generation
        // names as its causal parent, and it is the only record of which
        // generation produced which call. Without it a rebuilt history cannot
        // tell two calls issued at once from two issued in turn.
        let generation = self.journal.append(
            turn,
            None,
            None,
            EventKind::GenerationStarted {
                attempt: reference,
                role: GenerationRole::Main,
            },
        )?;

        let stream = self.generator.generate(request, attempt);
        let drained = self.drain(turn, generation, stream).await;

        // Settle before reporting anything, including a store failure: the
        // stream owns the reservation, and returning early would drop it.
        let settlement = match drained.stream.settle(self.gate) {
            Ok(settlement) => settlement,
            Err(denied) => {
                tracing::error!(%denied, "the gate refused to settle an attempt it minted");
                // Before the refusal is written: a store that has already
                // failed cannot record why the gate refused, and the store
                // failure is the one that has to reach the caller.
                drained.failure.map_or(Ok(()), Err)?;
                return self.refuse(turn, denied.to_string(), ExitReason::Internal);
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
        // After the settlement and after cancellation: the attempt is paid for
        // either way, and someone who asked to stop gets that answer rather
        // than the provider's. The error is already in the log; this is only
        // the exit. `Provider` and not `Internal` — `CLI.md` §4.3 makes them
        // different codes, and a script that retries 4 is right to: the next
        // response may well be well-formed.
        if drained.corrupt.is_some() {
            return Ok(Generated::Ended(ExitReason::Provider));
        }
        Ok(Generated::Stopped {
            reason: settlement.outcome.stop(),
            pending: drained.pending,
            error_kind: drained.error_kind,
        })
    }

    /// Write down why this loop stopped a generation, and end the turn on it.
    ///
    /// Every caller is a refusal *ego* made — a history that cannot be rebuilt,
    /// a request the model does not accept, a gate that would not authorise, a
    /// reservation that does not cover the request it was minted for, a
    /// settlement the gate rejected. None of them is something a provider said,
    /// and most happen before a socket exists, which is why the kind is
    /// `Unclassified` here rather than chosen five times: `ErrorKind`
    /// classifies a provider's own answer and there is none to classify.
    ///
    /// The `ExitReason` stays the caller's, because these refusals are four
    /// different exit codes (`CLI.md` §4.3). What they share is the order — the
    /// reason is in the log *before* the turn ends — so a session says why it
    /// stopped and not only that it did.
    fn refuse(
        &mut self,
        turn: TurnId,
        reason: String,
        exit: ExitReason,
    ) -> Result<Generated, StoreError> {
        self.journal.append(
            turn,
            None,
            None,
            EventKind::Error {
                message: reason,
                kind: ErrorKind::Unclassified,
            },
        )?;
        Ok(Generated::Ended(exit))
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
    async fn drain(
        &mut self,
        turn: TurnId,
        generation: EventId,
        mut stream: AttemptStream,
    ) -> Drained {
        let reference = stream.attempt();
        let mut pending = Vec::new();
        let mut message: Option<ItemId> = None;
        let mut reasoning: Option<ItemId> = None;
        let mut assembled = String::new();
        let mut failure = None;
        let mut cancelled = false;
        let mut corrupt: Option<String> = None;
        // Every provider id this session has already used, seeded from the
        // reduction rather than from this response. A decoder can only compare
        // ids inside one response — `settle_identity` does not even do that
        // much, comparing only within a single tool-call index — and reuse of
        // an id from an *earlier* generation is just as unattributable. The
        // set is built once per generation and grown as calls are accepted.
        let mut seen: BTreeSet<String> = self
            .journal
            .state()
            .items
            .values()
            .filter_map(|item| item.call_id.clone())
            .collect();
        let mut error_kind = None;
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
                // A reused id ends the generation before anything runs (ADR
                // §6). Not appended, because a `ToolCallStarted` the loop has
                // already decided not to dispatch is a call the log says was
                // made and no result will ever answer.
                AiEvent::ToolCall { call_id, .. } if seen.contains(&call_id) => {
                    corrupt = Some(call_id.clone());
                    self.journal
                        .append(
                            turn,
                            None,
                            None,
                            EventKind::Error {
                                message: format!(
                                    "the provider reused tool call id {call_id}, so a result \
                                     could not be attributed to the call that earned it; \
                                     nothing from this generation was run"
                                ),
                                // The provider misbehaved but classified
                                // nothing: this is our reading of a response
                                // that arrived intact, not an error the
                                // provider reported. `Unclassified` fails
                                // closed, which is right — nobody knows
                                // whether the next generation reuses the id
                                // again.
                                kind: ErrorKind::Unclassified,
                            },
                        )
                        .map(|_| ())
                }
                // The close of the block the deltas opened, so it is journalled
                // against that item and then *takes* it: the next
                // `ReasoningDelta` opens a new one rather than appending to a
                // block the provider has already sealed.
                //
                // `take` and not a peek, because the boundary is the whole
                // point. Anthropic documents a `400` when blocks are not
                // replayed exactly, and two blocks merged into one is not an
                // exact replay — it is a block no provider minted.
                //
                // State with no preceding delta still opens an item. A block
                // whose `thinking` field is empty is the case Anthropic's
                // `400` names explicitly, so an item is exactly what must
                // exist for it.
                AiEvent::ReasoningState(state) => {
                    // `unwrap_or_default` mints a fresh id here, not a nil
                    // one: `ItemId`'s `Default` is written as `new()`.
                    let item = reasoning.take().unwrap_or_default();
                    self.journal
                        .append(turn, Some(item), None, EventKind::ReasoningState { state })
                        .map(|_| ())
                }
                AiEvent::ToolCall {
                    call_id,
                    name,
                    args,
                } => {
                    let item = ItemId::new();
                    seen.insert(call_id.clone());
                    self.journal
                        .append(
                            turn,
                            Some(item),
                            // The generation that issued it, named as the
                            // causal parent the envelope already has room
                            // for. This is what lets a rebuilt history put
                            // the calls of one generation in one turn; a
                            // second attribution field beside it would be a
                            // second fact that could disagree with this one.
                            Some(generation),
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
                AiEvent::Error { message, kind } => {
                    error_kind = Some(kind);
                    self.journal
                        .append(turn, None, None, EventKind::Error { message, kind })
                        .map(|_| ())
                }
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
            && let Err(cause) = self.journal.append(
                turn,
                Some(item),
                None,
                EventKind::AssistantMessage { text: assembled },
            )
        {
            failure = Some(cause);
        }

        Drained {
            stream,
            // Not one of them, and not just the duplicate. Once two calls
            // claim one id, the *first* is no more attributable than the
            // second: a result naming that id answers either. Running the one
            // that happened to arrive first would be picking by stream order,
            // which the provider did not promise means anything.
            pending: if corrupt.is_some() {
                Vec::new()
            } else {
                pending
            },
            failure,
            cancelled,
            corrupt,
            error_kind,
        }
    }

    /// Run the one paid, tool-less opinion allowed for this pending call.
    async fn judge_pending(
        &mut self,
        turn: TurnId,
        pending: &Pending,
    ) -> Result<JudgeRun, StoreError> {
        let Some(judge) = self.judge.as_ref() else {
            return Ok(JudgeRun::Unavailable);
        };
        let generator = judge.generator;
        let metering = judge.metering.clone();
        let rates = judge.rates.clone();
        let caps = judge.caps.clone();
        let config = judge.config.clone();
        let input = JudgeInput::new(
            pending.call(),
            &judge.confinement,
            judge.exec_caps,
            pending.request().why(),
        );
        let request = input.request(&config);

        if !caps.request_gaps(&request).is_empty() {
            return Ok(JudgeRun::Unavailable);
        }
        let quote = rates.as_ref().map(|rates| &rates.quote);
        let reservation = match self.gate.reserve(&metering, quote, &request, &caps) {
            Ok(reservation) => reservation,
            Err(denied) => {
                tracing::warn!(%denied, "the budget gate refused the judge generation");
                return Ok(JudgeRun::Ended(ExitReason::Budget));
            }
        };
        let attempt = match GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &metering,
            quote,
            &request,
            &caps,
        ) {
            Ok(attempt) => attempt,
            Err(denied) => {
                tracing::error!(%denied, "the judge reservation did not authorise its request");
                return Ok(JudgeRun::Ended(ExitReason::Internal));
            }
        };
        let reference = attempt.attempt();
        self.journal.append(
            turn,
            None,
            None,
            EventKind::GenerationStarted {
                attempt: reference,
                role: GenerationRole::Judge,
            },
        )?;

        let mut stream = generator.generate(request, attempt);
        let mut decoder = JudgeDecoder::new();
        let mut usage: Option<Usage> = None;
        let mut provider_cost: Option<Money> = None;
        while let Some(event) = stream.next().await {
            decoder.observe(&event);
            match &event {
                AiEvent::Usage(delta) => {
                    let mut total = usage.unwrap_or(Usage::IDENTITY);
                    total.add(delta);
                    usage = Some(total);
                }
                AiEvent::ProviderCost { cost } => provider_cost = Some(*cost),
                _ => {}
            }
        }

        let mut failure = None;
        if usage.is_some() || provider_cost.is_some() {
            let observed = usage.unwrap_or(Usage::IDENTITY);
            let cost_delta = settled_cost(
                &metering,
                quote,
                usage.map_or(UsageReport::Missing, UsageReport::Reported),
                provider_cost,
                self.gate.currency(),
            );
            if let Err(cause) = self.journal.append(
                turn,
                None,
                None,
                EventKind::UsageUpdated {
                    usage: observed,
                    cost_delta,
                    attempt: Some(reference),
                },
            ) {
                failure = Some(cause);
            }
        }

        let settlement = match stream.settle(self.gate) {
            Ok(settlement) => settlement,
            Err(denied) => {
                failure.map_or(Ok(()), Err)?;
                tracing::error!(%denied, "the gate refused to settle its judge attempt");
                return Ok(JudgeRun::Ended(ExitReason::Internal));
            }
        };
        failure.map_or(Ok(()), Err)?;
        self.journal.append(
            turn,
            None,
            None,
            EventKind::AttemptSettled {
                attempt: settlement.attempt,
                outcome: settlement.outcome,
            },
        )?;

        let outcome = decoder.finish();
        let by = Authority::Judge {
            model: config.identity,
            attempt: reference,
        };
        Ok(JudgeRun::Decided { outcome, by })
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
    async fn run_tool(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
    ) -> Result<ToolOutcome, StoreError> {
        let answered = |ok, text: String| ToolOutcome::Answered {
            ok,
            result: ToolResult::builtin(text),
        };
        let (tool, mut prepared) = match self.tools.prepare(&call.name, &call.args) {
            Ok(prepared) => prepared,
            // Not a permission event. `prepare` refuses calls that never
            // reached the engine — an unknown tool, unparseable arguments —
            // and appending a decision here would record a fact the policy
            // never established.
            Err(refusal) => return Ok(answered(false, refusal)),
        };
        let authorized = loop {
            match self.tools.authorize(prepared) {
                Classification::Settled(Ok(authorized)) => {
                    self.decided(turn, call, true, authorized.by().clone())?;
                    break authorized;
                }
                Classification::Settled(Err(denial)) => {
                    self.decided(turn, call, false, denial.authority())?;
                    return Ok(ToolOutcome::Refused { denial });
                }
                Classification::Ask(pending) => match self.tools.settle(pending).await {
                    Resolution::Settled(Ok(authorized)) => {
                        self.decided(turn, call, true, authorized.by().clone())?;
                        break authorized;
                    }
                    Resolution::Settled(Err(denial)) => {
                        self.decided(turn, call, false, denial.authority())?;
                        return Ok(ToolOutcome::Refused { denial });
                    }
                    Resolution::Ask(pending) => {
                        let pending = match self.judge_pending(turn, &pending).await? {
                            JudgeRun::Unavailable => pending,
                            JudgeRun::Ended(reason) => return Ok(ToolOutcome::Ended(reason)),
                            JudgeRun::Decided { outcome, by } => {
                                self.judge_decided(turn, call, &outcome, by.clone())?;
                                match self.tools.settle_judge(pending, &outcome, by) {
                                    Resolution::Settled(Ok(authorized)) => break authorized,
                                    Resolution::Settled(Err(denial)) => {
                                        return Ok(ToolOutcome::Refused { denial });
                                    }
                                    Resolution::Ask(pending) => pending,
                                    Resolution::HumanSettled { .. }
                                    | Resolution::Reclassify { .. } => {
                                        unreachable!("a judge cannot produce a human resolution")
                                    }
                                }
                            }
                        };
                        self.requested(turn, call, pending.request())?;
                        match self.tools.decide(pending).await {
                            Resolution::Ask(pending) => {
                                return Ok(ToolOutcome::Unanswerable { pending });
                            }
                            Resolution::HumanSettled { result, .. } => match result {
                                Ok(authorized) => {
                                    self.decided(turn, call, true, authorized.by().clone())?;
                                    break authorized;
                                }
                                Err(denial) => {
                                    self.decided(turn, call, false, denial.authority())?;
                                    return Ok(ToolOutcome::Refused { denial });
                                }
                            },
                            Resolution::Reclassify { request, params } => {
                                self.permission(
                                    turn,
                                    call,
                                    EventKind::PermissionAmended {
                                        tool: request.tool().to_string(),
                                    },
                                )?;
                                prepared = match self.tools.prepare_amendment(tool, params) {
                                    Ok(prepared) => prepared,
                                    Err(refusal) => return Ok(answered(false, refusal)),
                                };
                            }
                            Resolution::Settled(result) => match result {
                                Ok(authorized) => {
                                    self.decided(turn, call, true, authorized.by().clone())?;
                                    break authorized;
                                }
                                Err(denial) => {
                                    self.decided(turn, call, false, denial.authority())?;
                                    return Ok(ToolOutcome::Refused { denial });
                                }
                            },
                        }
                    }
                    Resolution::HumanSettled { result, .. } => match result {
                        Ok(authorized) => {
                            self.decided(turn, call, true, authorized.by().clone())?;
                            break authorized;
                        }
                        Err(denial) => {
                            self.decided(turn, call, false, denial.authority())?;
                            return Ok(ToolOutcome::Refused { denial });
                        }
                    },
                    Resolution::Reclassify { request, params } => {
                        self.permission(
                            turn,
                            call,
                            EventKind::PermissionAmended {
                                tool: request.tool().to_string(),
                            },
                        )?;
                        prepared = match self.tools.prepare_amendment(tool, params) {
                            Ok(prepared) => prepared,
                            Err(refusal) => return Ok(answered(false, refusal)),
                        };
                    }
                },
            }
        };
        Ok(match self.tools.execute(tool, authorized).await {
            Ok(result) => ToolOutcome::Answered { ok: true, result },
            Err(failure) => ToolOutcome::Answered {
                ok: false,
                result: failure.into_failed_result(),
            },
        })
    }

    /// Record the canonical question that was actually presented.
    fn requested(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
        request: &crate::permit::PermissionRequest,
    ) -> Result<(), StoreError> {
        self.permission(
            turn,
            call,
            EventKind::PermissionRequested {
                tool: request.tool().to_string(),
                escalation: request.why().clone(),
            },
        )
    }

    /// §7: *every decision is appended, asked or not.*
    ///
    /// So this fires for a call the rules waved through as well as for one
    /// they refused. `PermissionRequested` is appended when mode settlement
    /// raises the canonical question, before either a human answer or the
    /// headless unanswerable result.
    fn decided(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
        allowed: bool,
        by: Authority,
    ) -> Result<(), StoreError> {
        self.permission(
            turn,
            call,
            EventKind::PermissionDecided {
                allowed: Some(allowed),
                by: Some(by),
                reason: None,
            },
        )
    }

    fn judge_decided(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
        outcome: &JudgeOutcome,
        by: Authority,
    ) -> Result<(), StoreError> {
        let allowed = match outcome {
            JudgeOutcome::Allow { .. } => Some(true),
            JudgeOutcome::Deny { .. } => Some(false),
            JudgeOutcome::Escalate { .. } => None,
        };
        self.permission(
            turn,
            call,
            EventKind::PermissionDecided {
                allowed,
                by: Some(by),
                reason: Some(outcome.reason().to_owned()),
            },
        )
    }

    /// One permission event, on an item of its own and caused by the call's
    /// `ToolCallStarted`.
    ///
    /// **Its own item, not the call's.** An item has one kind, and the tool
    /// call already owns `call.item` as `ItemKind::Tool`; projecting a
    /// permission onto it would put two kinds on one id and leave a consumer
    /// unable to say which lifecycle it was watching. The parent link is what
    /// ties the two together, and it is the tie that holds regardless of what
    /// any projection chooses to do with the permission item.
    fn permission(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
        kind: EventKind,
    ) -> Result<(), StoreError> {
        self.journal
            .append(turn, Some(ItemId::new()), Some(call.started), kind)?;
        Ok(())
    }
}

/// What running one tool call produced.
enum ToolOutcome {
    /// It ran, or it failed in a way the model should read and adapt to.
    Answered { ok: bool, result: ToolResult },
    /// Policy refused it. A settled answer, and still not one for the model:
    /// telling it "denied" invites it to try a variant, which is a prompt loop
    /// run against a boundary.
    Refused { denial: PolicyDenied },
    /// Policy could not settle it and there is nobody here to ask.
    Unanswerable { pending: Pending },
    /// Judge admission failed for a cause that ends the whole turn.
    Ended(ExitReason),
}

enum JudgeRun {
    Unavailable,
    Ended(ExitReason),
    Decided {
        outcome: JudgeOutcome,
        by: Authority,
    },
}

/// What one generation did.
enum Generated {
    /// The turn is over, for a reason that is not the model's stop reason.
    Ended(ExitReason),
    /// The generation finished. `pending` is what it asked to call.
    Stopped {
        reason: StopReason,
        pending: Vec<PendingCall>,
        /// How the provider classified its failure, when `reason` is
        /// [`StopReason::Error`]. Meaningless otherwise, and not read.
        error_kind: Option<ErrorKind>,
    },
}

/// A drained stream, handed back so it can be settled.
struct Drained {
    stream: AttemptStream,
    pending: Vec<PendingCall>,
    failure: Option<StoreError>,
    cancelled: bool,
    /// A provider `call_id` this session had already used, if one arrived.
    ///
    /// See the tool-use-history ADR §6. Held rather than raised so the stream
    /// still drains and the attempt still settles — it is already paid for —
    /// and separate from `failure`, which is *our* store breaking, because
    /// `CLI.md` §4.3 gives those two different exits.
    corrupt: Option<String>,
    /// The classification of the last error this generation reported, if it
    /// reported one.
    ///
    /// The *last* and not the first: the terminal frame follows the error
    /// that caused it — a wire format emits `Error` then `Stop(Error)` — so
    /// the most recent classification is the one describing how the
    /// generation ended. An earlier, recovered-from error does not decide
    /// the exit of a stream that went on to end some other way, and is never
    /// consulted, because this is only read when the stop reason is `Error`.
    error_kind: Option<ErrorKind>,
}
