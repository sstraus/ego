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

mod catalogue;
mod control;
mod journal;
mod mcp_registry;
mod tools;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use futures::StreamExt;
use futures::future::{Either, select};

use crate::budget::{
    BudgetSynchronizationError, Metering, PricingQuote, SettlementSource, SharedBudgetError,
    SharedBudgetGate,
};
use crate::ceiling::tokens_reached;
use crate::compact::{
    COMPACTION_PROMPT, Checkpoint, CompactionBusy, CompactionError, PreparedCompaction,
    checkpoint_frame,
};
use crate::config::{ModelRef, SystemPrompt};
use crate::cost::{Money, RateSource};
use crate::event::domain::{
    CancelCause, EventKind, ExitReason, GenerationRole, OccupancyProvenance,
};
use crate::event::hook::{HookLifecycle, HookOccurrenceKey, HookSubject};
use crate::event::id::{
    EventId, GenerationId, ItemId, Producer, QueuedPromptId, SessionId, TurnId,
};
use crate::event::time::Clock;
use crate::evidence::{
    EvidenceAdmissionService, EvidenceStore, EvidenceWrite, GenerationAdmissionEvidence,
    GenerationTarget, PricingEvidence, RunInvocationId,
};
use crate::generation_model_source::{
    GenerationModelErrorClass, GenerationModelRequest, GenerationModelSource, GenerationProfile,
};
use crate::generator::attempt::DeliveryCertainty;
use crate::generator::attempt::{AttemptStream, GenerationAttempt, UsageReport, settled_cost};
use crate::generator::history::conversation_from_with_blobs;
use crate::generator::port::AiEvent;
use crate::generator::types::{
    Conversation, ErrorKind, GenRequest, Message, Role, StopReason, ToolDescriptor,
};
use crate::hook::{HookCausality, HookEvent, HookOccurrence, HookRunError, SessionHookRuntime};
use crate::permit::ExecObservation;
use crate::permit::judge::{JudgeConfig, JudgeDecoder, JudgeInput, JudgeOutcome};
use crate::permit::{Authority, Classification, Pending, PolicyDenied, Resolution, ToolResult};
use crate::public::PublicEvent;
use crate::question::{Elicitor, QuestionAnswer, QuestionOutcome, QuestionTarget};
use crate::run::{Denial, DeniedBy};
use crate::sink::EventSink;
use crate::state::{SessionState, Usage};
use crate::store::evidence::EvidenceRef;
use crate::store::session_store::{SessionStore, StoreError};

use catalogue::ToolCaptureAuthority;
pub use catalogue::{AdmittedToolSource, CapturedToolCatalogue, ToolCatalogueError};
use control::{BoundaryIntent, ControlIntent};
pub use control::{
    ControlError, InteractiveControl, InteractiveIngress, QueueStatus, interactive_control,
};
pub use journal::{Journal, SharedJournal, SharedJournalError};
pub use mcp_registry::{
    McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError, McpRegistryJournalErrorKind,
    McpRegistryJournalOperation,
};
use tools::{GenerationToolCatalogue, PreparedToolCall};
pub use tools::{ToolBatchError, ToolEffectPorts, Tools};

impl From<SharedJournalError> for CompactionError {
    fn from(error: SharedJournalError) -> Self {
        Self::Store(error.into())
    }
}

/// The final model-visible registry. Only `Effect` enters preparation,
/// permission policy, and guarded I/O; human input crosses its own typed port.
#[derive(Clone)]
pub enum RegisteredTool {
    Effect(Arc<dyn crate::permit::Tool>),
    HumanInput(Arc<dyn crate::question::HumanInputTool>),
    Skill(Arc<crate::skill::SkillCatalogue>),
}

impl RegisteredTool {
    pub fn descriptor(&self) -> crate::generator::types::ToolDescriptor {
        match self {
            Self::Effect(tool) => {
                let descriptor = tool.descriptor();
                crate::generator::types::ToolDescriptor {
                    name: descriptor.name,
                    description: descriptor.description,
                    parameters: descriptor.schema.to_value(),
                }
            }
            Self::HumanInput(tool) => tool.descriptor(),
            Self::Skill(_) => crate::generator::types::ToolDescriptor {
                name: "skill".to_owned(),
                description:
                    "Invoke one admitted skill or read one resource from a live invocation"
                        .to_owned(),
                parameters: serde_json::json!({
                    "oneOf": [
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["kind", "name", "arguments"],
                            "properties": {
                                "kind": { "const": "invoke" },
                                "name": { "type": "string" },
                                "arguments": { "type": "string" }
                            }
                        },
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["kind", "invocation", "path"],
                            "properties": {
                                "kind": { "const": "read_resource" },
                                "invocation": { "type": "string" },
                                "path": { "type": "string" }
                            }
                        }
                    ]
                }),
            },
        }
    }

    pub fn skill(catalogue: Arc<crate::skill::SkillCatalogue>) -> Self {
        Self::Skill(catalogue)
    }

    pub(crate) fn matches_name(&self, name: &str) -> bool {
        match self {
            Self::Skill(catalogue) => name == "skill" || catalogue.authorize_model(name).is_ok(),
            _ => self.descriptor().name == name,
        }
    }

    pub fn as_effect(&self) -> Option<&Arc<dyn crate::permit::Tool>> {
        match self {
            Self::Effect(tool) => Some(tool),
            Self::HumanInput(_) | Self::Skill(_) => None,
        }
    }

    pub fn as_human_input(&self) -> Option<&Arc<dyn crate::question::HumanInputTool>> {
        match self {
            Self::HumanInput(tool) => Some(tool),
            Self::Effect(_) | Self::Skill(_) => None,
        }
    }
}

impl From<Arc<dyn crate::permit::Tool>> for RegisteredTool {
    fn from(tool: Arc<dyn crate::permit::Tool>) -> Self {
        Self::Effect(tool)
    }
}

impl<T> From<Arc<T>> for RegisteredTool
where
    T: crate::permit::Tool + 'static,
{
    fn from(tool: Arc<T>) -> Self {
        Self::Effect(tool)
    }
}

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
    /// A ceiling on this session's cumulative billed tokens.
    pub token_ceiling: Option<u64>,
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
    selected: Option<Result<RegisteredTool, String>>,
    args: String,
    _catalogue_generation: Arc<str>,
    capture_authority: ToolCaptureAuthority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptDisposition {
    Return,
    AwaitEffect,
}

enum Controlled<T> {
    Completed(T),
    Interrupted,
    Failed(StoreError),
}

enum ControlRace<T> {
    Work(T),
    Intent(Option<ControlIntent>),
}

async fn receive_intent(control: &mut Option<&mut InteractiveControl>) -> Option<ControlIntent> {
    match control.as_deref_mut() {
        Some(control) => control.next().await,
        None => std::future::pending().await,
    }
}

fn apply_intent(
    journal: &SharedJournal<'_>,
    cancel: &CancelToken,
    control: &mut InteractiveControl,
    turn: TurnId,
    intent: ControlIntent,
) -> Result<bool, StoreError> {
    match intent {
        ControlIntent::Submit { text, reply } => {
            let id = QueuedPromptId::new();
            match journal.append(
                turn,
                None,
                None,
                EventKind::PromptQueued {
                    id,
                    text,
                    images: Vec::new(),
                },
            ) {
                Ok(_) => {
                    control.refresh(
                        journal
                            .inspect(SessionState::queue_status)
                            .map_err(StoreError::from)?,
                    );
                    let _ = reply.send(Ok(control.status()));
                    Ok(false)
                }
                Err(cause) => {
                    let _ = reply.send(Err(ControlError::Store(cause.to_string())));
                    Err(cause.into())
                }
            }
        }
        ControlIntent::Interrupt { reply } => {
            let through = journal
                .inspect(|state| {
                    state
                        .unacknowledged_prompts()
                        .last()
                        .map(|prompt| prompt.id())
                })
                .map_err(StoreError::from)?;
            if let Some(through) = through
                && journal
                    .inspect(|state| state.can_expedite_prompt(through))
                    .map_err(StoreError::from)?
                && let Err(cause) = journal.append(
                    turn,
                    None,
                    None,
                    EventKind::PromptDeliveryExpedited { through },
                )
            {
                let _ = reply.send(Err(ControlError::Store(cause.to_string())));
                return Err(cause.into());
            }
            control.refresh(
                journal
                    .inspect(SessionState::queue_status)
                    .map_err(StoreError::from)?,
            );
            cancel.cancel(CancelCause::User);
            let _ = reply.send(Ok(control.status()));
            Ok(true)
        }
    }
}

async fn await_controlled<F, T>(
    journal: &SharedJournal<'_>,
    cancel: &CancelToken,
    control: &mut Option<&mut InteractiveControl>,
    turn: TurnId,
    work: F,
    disposition: InterruptDisposition,
) -> Controlled<T>
where
    F: Future<Output = T>,
{
    let mut work = Box::pin(work);
    loop {
        if control.is_none() {
            return Controlled::Completed(work.await);
        }
        let raced = {
            let intent = Box::pin(receive_intent(control));
            match select(intent, work.as_mut()).await {
                Either::Left((intent, _)) => ControlRace::Intent(intent),
                Either::Right((result, _)) => ControlRace::Work(result),
            }
        };
        match raced {
            ControlRace::Work(result) => return Controlled::Completed(result),
            ControlRace::Intent(None) => {
                *control = None;
            }
            ControlRace::Intent(Some(intent)) => {
                let applied = {
                    let coordinator = control
                        .as_deref_mut()
                        .expect("an intent came from an attached coordinator");
                    apply_intent(journal, cancel, coordinator, turn, intent)
                };
                match applied {
                    Ok(true) if disposition == InterruptDisposition::Return => {
                        return Controlled::Interrupted;
                    }
                    Ok(_) => {}
                    Err(cause) if disposition == InterruptDisposition::AwaitEffect => {
                        let _ = work.await;
                        return Controlled::Failed(cause);
                    }
                    Err(cause) => return Controlled::Failed(cause),
                }
            }
        }
    }
}

/// Where a session's events go, and who can stop it.
pub struct Session<'a> {
    pub id: SessionId,
    /// The session so far: `SessionState::default()` for a new one, a
    /// replayed state for a resumed one. Its `last_seq` is where the loop's
    /// numbering continues from.
    pub state: SessionState,
    pub store: &'a dyn SessionStore,
    pub evidence: &'a dyn EvidenceStore,
    pub clock: &'a dyn Clock,
    /// Where the public projection goes as it happens. A session-level
    /// concern beside the store and the clock, because it outlives any one
    /// turn: a host subscribes once and watches the whole session.
    pub sink: &'a dyn EventSink,
    /// Held by the caller, not minted here: whoever wants to stop a turn is
    /// by definition not the turn.
    pub cancel: CancelToken,
}

/// The run-owned inputs needed at every fresh generation boundary.
pub struct GenerationRuntime {
    pub source: Arc<dyn GenerationModelSource>,
    pub gate: SharedBudgetGate,
    pub config: TurnConfig,
    pub admission: Arc<EvidenceAdmissionService>,
    pub invocation: RunInvocationId,
}

/// Stable host facts used only after policy reaches the optional judge seat.
#[derive(Clone)]
pub struct JudgeContext {
    pub confinement: crate::permit::RunConfinement,
    pub exec_caps: crate::permit::ExecCaps,
}

/// One tool-less generation owner sharing the enclosing session's journal,
/// evidence admission, invocation, and budget authority.
#[derive(Clone)]
pub struct ToollessGenerationOwner<'session, 'effects> {
    session_id: SessionId,
    journal: SharedJournal<'session>,
    run_evidence: &'session dyn EvidenceStore,
    response_evidence: &'effects dyn crate::store::evidence::EvidenceStore,
    admission: Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    gate: SharedBudgetGate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToollessGenerated {
    raw: Vec<u8>,
    evidence: EvidenceRef,
    delivery: DeliveryCertainty,
}

impl ToollessGenerated {
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn evidence(&self) -> &EvidenceRef {
        &self.evidence
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToollessGenerationFailure {
    message: String,
    delivery: DeliveryCertainty,
}

impl ToollessGenerationFailure {
    fn new(message: impl Into<String>, delivery: DeliveryCertainty) -> Self {
        Self {
            message: message.into(),
            delivery,
        }
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
}

impl std::fmt::Display for ToollessGenerationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToollessGenerationFailure {}

impl<'session, 'effects> ToollessGenerationOwner<'session, 'effects> {
    pub fn new(
        session_id: SessionId,
        journal: SharedJournal<'session>,
        run_evidence: &'session dyn EvidenceStore,
        response_evidence: &'effects dyn crate::store::evidence::EvidenceStore,
        admission: Arc<EvidenceAdmissionService>,
        invocation: RunInvocationId,
        gate: SharedBudgetGate,
    ) -> Self {
        Self {
            session_id,
            journal,
            run_evidence,
            response_evidence,
            admission,
            invocation,
            gate,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn generate(
        &self,
        source: Arc<dyn GenerationModelSource>,
        role: GenerationRole,
        turn: TurnId,
        prompt: String,
        input: String,
        model: Option<ModelRef>,
    ) -> Result<ToollessGenerated, ToollessGenerationFailure> {
        let profile = source
            .resolve(GenerationModelRequest::new(role).with_skill_scope(model, None))
            .await
            .map_err(|error| {
                ToollessGenerationFailure::new(
                    error.to_string(),
                    DeliveryCertainty::DefinitelyUnsent,
                )
            })?;
        let messages = Conversation::new(vec![
            Message::text(Role::System, prompt),
            Message::text(Role::User, input),
        ])
        .map_err(|error| {
            ToollessGenerationFailure::new(error.to_string(), DeliveryCertainty::DefinitelyUnsent)
        })?;
        let request = GenRequest {
            model: profile.request_model.clone(),
            messages,
            tools: Vec::new(),
            thinking: profile.thinking,
            max_output_tokens: profile.output_ceiling(),
            parallel_tool_calls: false,
        };
        let gaps = profile.capabilities.request_gaps(&request);
        if !gaps.is_empty() {
            return Err(ToollessGenerationFailure::new(
                gaps.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
                DeliveryCertainty::DefinitelyUnsent,
            ));
        }
        profile
            .generator
            .ensure_expressible(&request)
            .map_err(|error| {
                ToollessGenerationFailure::new(
                    error.to_string(),
                    DeliveryCertainty::DefinitelyUnsent,
                )
            })?;
        let budget = self.gate.snapshot().map_err(|error| {
            ToollessGenerationFailure::new(error.to_string(), DeliveryCertainty::DefinitelyUnsent)
        })?;
        self.journal
            .publish(&PublicEvent::metering_selected(
                turn,
                &profile.metering,
                budget.ledger(),
                profile
                    .rates
                    .as_ref()
                    .map_or(&RateSource::None, |rates| &rates.source),
            ))
            .map_err(|error| {
                ToollessGenerationFailure::new(
                    error.to_string(),
                    DeliveryCertainty::DefinitelyUnsent,
                )
            })?;
        let quote = profile.rates.as_ref().map(|rates| &rates.quote);
        let reservation = self
            .gate
            .reserve(&profile.metering, quote, &request, &profile.capabilities)
            .map_err(|error| {
                ToollessGenerationFailure::new(
                    error.to_string(),
                    DeliveryCertainty::DefinitelyUnsent,
                )
            })?;
        let attempt = GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &profile.metering,
            quote,
            &request,
            &profile.capabilities,
        )
        .map_err(|error| {
            ToollessGenerationFailure::new(error.to_string(), DeliveryCertainty::DefinitelyUnsent)
        })?;
        let reference = attempt.attempt();
        admit_generation_evidence(
            self.session_id,
            self.invocation,
            self.admission.as_ref(),
            self.run_evidence,
            &profile,
            reference,
            role,
        )
        .map_err(|error| {
            ToollessGenerationFailure::new(error.to_string(), DeliveryCertainty::DefinitelyUnsent)
        })?;
        self.journal
            .append(
                turn,
                None,
                None,
                EventKind::GenerationStarted {
                    attempt: reference,
                    role,
                },
            )
            .map_err(|error| {
                ToollessGenerationFailure::new(
                    error.to_string(),
                    DeliveryCertainty::DefinitelyUnsent,
                )
            })?;

        let mut stream = profile.generator.generate(request, attempt);
        let mut raw = Vec::new();
        let mut usage: Option<Usage> = None;
        let mut provider_cost: Option<Money> = None;
        let mut provider_error = None;
        let mut emitted_tool = false;
        while let Some(event) = stream.next().await {
            match event {
                AiEvent::TextDelta { text } => raw.extend_from_slice(text.as_bytes()),
                AiEvent::Usage(delta) => {
                    let mut total = usage.unwrap_or(Usage::IDENTITY);
                    total.add(&delta);
                    usage = Some(total);
                }
                AiEvent::ProviderCost { cost } => provider_cost = Some(cost),
                AiEvent::ToolCall { .. } => emitted_tool = true,
                AiEvent::Error { message, .. } => {
                    provider_error.get_or_insert(message);
                }
                AiEvent::ReasoningDelta { .. }
                | AiEvent::ReasoningState(_)
                | AiEvent::DeliveryObserved(_)
                | AiEvent::Stop(_) => {}
            }
        }
        let settlement = self.gate.settle(stream).map_err(|error| {
            ToollessGenerationFailure::new(error.to_string(), DeliveryCertainty::PossiblySent)
        })?;
        let delivery = settlement.outcome.delivery();
        let observed = usage.unwrap_or(Usage::IDENTITY);
        let cost_delta = settled_cost(
            &profile.metering,
            quote,
            usage.map_or(UsageReport::Missing, UsageReport::Reported),
            provider_cost,
            self.gate
                .snapshot()
                .map_err(|error| ToollessGenerationFailure::new(error.to_string(), delivery))?
                .currency(),
        );
        self.journal
            .append(
                turn,
                None,
                None,
                EventKind::UsageUpdated {
                    usage: observed,
                    cost_delta,
                    attempt: Some(reference),
                },
            )
            .map_err(|error| ToollessGenerationFailure::new(error.to_string(), delivery))?;
        self.journal
            .append(
                turn,
                None,
                None,
                EventKind::AttemptSettled {
                    attempt: settlement.attempt,
                    outcome: settlement.outcome,
                },
            )
            .map_err(|error| ToollessGenerationFailure::new(error.to_string(), delivery))?;
        if let Some(error) = provider_error {
            return Err(ToollessGenerationFailure::new(error, delivery));
        }
        if emitted_tool || settlement.outcome.stop() != StopReason::Stop {
            return Err(ToollessGenerationFailure::new(
                "tool-less generation did not settle with a normal stop",
                delivery,
            ));
        }
        let mut writer = self
            .response_evidence
            .begin()
            .map_err(|error| ToollessGenerationFailure::new(error.to_string(), delivery))?;
        writer
            .append(&raw)
            .map_err(|error| ToollessGenerationFailure::new(error.to_string(), delivery))?;
        let evidence = writer
            .commit()
            .map_err(|error| ToollessGenerationFailure::new(error.to_string(), delivery))?;
        Ok(ToollessGenerated {
            raw,
            evidence,
            delivery,
        })
    }
}

/// Runs turns against one session.
///
/// Holds the session's whole world: what generates, what it costs, what it may
/// call, and where the log goes. One `run` call is one turn; the state carries
/// over between them.
pub struct TurnLoop<'session, 'turn> {
    session_id: SessionId,
    journal: SharedJournal<'session>,
    source: Arc<dyn GenerationModelSource>,
    evidence: &'session dyn EvidenceStore,
    admission: Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    gate: SharedBudgetGate,
    settlement_currency: crate::cost::CurrencyCode,
    config: TurnConfig,
    judge: Option<JudgeContext>,
    execution_observation: Option<ExecObservation>,
    tools: Tools<'turn>,
    ancestor: Option<Conversation>,
    cancel: CancelToken,
    control: Option<&'turn mut InteractiveControl>,
    elicitor: Option<Arc<dyn Elicitor>>,
    foreign_question_port: Option<Arc<dyn crate::mcp::ForeignQuestionPort + 'turn>>,
    hook_runtime: Option<SessionHookRuntime<'session>>,
    active_skill_scope: Option<crate::skill::ActiveSkillScope>,
    active_skill_invocations:
        BTreeMap<crate::event::skill::SkillInvocationId, crate::skill::SkillInvocation>,
}

fn admit_generation_evidence(
    session: SessionId,
    invocation: RunInvocationId,
    admission: &EvidenceAdmissionService,
    store: &dyn EvidenceStore,
    profile: &GenerationProfile,
    attempt: crate::generator::attempt::AttemptRef,
    role: GenerationRole,
) -> Result<(), StoreError> {
    let provider = profile
        .identity
        .0
        .split_once('/')
        .map_or(profile.identity.0.as_str(), |(provider, _)| provider)
        .to_owned();
    let pricing = match profile.rates.as_ref() {
        Some(rates) => PricingEvidence::Priced {
            source: "resolved_rates".to_owned(),
            revision: rates.quote.revision.clone(),
            metering: profile.metering.identity(),
        },
        None => PricingEvidence::Unpriced {
            source: "generation_profile".to_owned(),
            reason: profile.metering.identity().to_string(),
        },
    };
    let evidence = GenerationAdmissionEvidence::new(
        session,
        invocation,
        attempt,
        role,
        GenerationTarget::new(
            provider,
            profile.identity.clone(),
            profile.request_model.clone(),
        ),
        profile.capabilities.clone(),
        pricing,
    );
    match admission.admit_generation(store, evidence) {
        Ok(EvidenceWrite::PublishedWithProjectionWarning { diagnostic }) => {
            tracing::warn!(%diagnostic, "generation evidence manifest projection failed");
            Ok(())
        }
        Ok(EvidenceWrite::Published | EvidenceWrite::AlreadyRecorded) => Ok(()),
        Err(error) => Err(StoreError::Io(std::io::Error::other(error.to_string()))),
    }
}

impl<'session, 'turn> TurnLoop<'session, 'turn> {
    /// Open a loop over a session.
    ///
    /// Three arguments rather than ten loose ones: a turn is a session, a
    /// model and a set of tools, and at ten positional parameters — four of
    /// which were `&dyn` — a call site said nothing about which was which.
    pub fn new(
        session: Session<'session>,
        runtime: GenerationRuntime,
        tools: Tools<'turn>,
    ) -> Result<Self, BudgetSynchronizationError> {
        let journal = SharedJournal::new(Journal::new(
            session.id,
            session.store,
            session.clock,
            session.sink,
            Producer::Main,
            session.state,
        ));
        Self::from_shared_journal(
            session.id,
            journal,
            session.evidence,
            session.cancel,
            runtime,
            tools,
        )
    }

    /// Open one turn-local coordinator over the session's existing journal.
    pub fn from_shared_journal(
        session_id: SessionId,
        journal: SharedJournal<'session>,
        evidence: &'session dyn EvidenceStore,
        cancel: CancelToken,
        runtime: GenerationRuntime,
        tools: Tools<'turn>,
    ) -> Result<Self, BudgetSynchronizationError> {
        let settlement_currency = runtime.gate.snapshot()?.currency();
        Ok(Self {
            session_id,
            journal,
            source: runtime.source,
            evidence,
            admission: runtime.admission,
            invocation: runtime.invocation,
            gate: runtime.gate,
            settlement_currency,
            config: runtime.config,
            judge: None,
            execution_observation: None,
            tools,
            ancestor: None,
            cancel,
            control: None,
            elicitor: None,
            foreign_question_port: None,
            hook_runtime: None,
            active_skill_scope: None,
            active_skill_invocations: BTreeMap::new(),
        })
    }

    /// Attach the stable host facts for the optional judge seat.
    pub fn with_judge(mut self, judge: JudgeContext) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Attach one run observation, consumed before the first public turn event.
    pub fn with_execution_observation(mut self, observation: ExecObservation) -> Self {
        self.execution_observation = Some(observation);
        self
    }

    /// Attach the session's sole bounded interactive-intent receiver.
    pub fn with_interactive_control(mut self, control: &'turn mut InteractiveControl) -> Self {
        control.refresh(self.state().queue_status());
        control.open();
        self.control = Some(control);
        self
    }

    /// Attach the session's sole human-input adapter.
    pub fn with_elicitor(mut self, elicitor: Arc<dyn Elicitor>) -> Self {
        self.elicitor = Some(elicitor);
        self
    }

    /// Attach the adapter that owns foreign continuation recovery.
    pub fn with_foreign_question_port(
        mut self,
        port: Arc<dyn crate::mcp::ForeignQuestionPort + 'turn>,
    ) -> Self {
        self.foreign_question_port = Some(port);
        self
    }

    pub fn journal(&self) -> SharedJournal<'session> {
        self.journal.clone()
    }

    pub fn toolless_generation_owner(&self) -> ToollessGenerationOwner<'session, 'turn> {
        ToollessGenerationOwner {
            session_id: self.session_id,
            journal: self.journal.clone(),
            run_evidence: self.evidence,
            response_evidence: self.tools.evidence_store(),
            admission: Arc::clone(&self.admission),
            invocation: self.invocation,
            gate: self.gate.clone(),
        }
    }

    /// Attach the session's borrow-scoped hook runtime.
    pub fn with_hook_runtime(
        mut self,
        runtime: SessionHookRuntime<'session>,
    ) -> Result<Self, HookRunError> {
        if !self.journal.same_owner(&runtime.journal()) {
            return Err(HookRunError::new(
                "the hook runtime does not own this turn loop's journal",
            ));
        }
        self.hook_runtime = Some(runtime);
        Ok(self)
    }

    /// Attach ancestor-only provider history resolved by [`crate::session::ReplayContext`].
    pub fn with_ancestor_conversation(mut self, ancestor: Conversation) -> Self {
        self.ancestor = Some(ancestor);
        self
    }

    fn admit_generation(
        &self,
        profile: &GenerationProfile,
        attempt: crate::generator::attempt::AttemptRef,
        role: GenerationRole,
    ) -> Result<(), StoreError> {
        admit_generation_evidence(
            self.session_id,
            self.invocation,
            self.admission.as_ref(),
            self.evidence,
            profile,
            attempt,
            role,
        )
    }

    /// The session so far.
    pub fn state(&self) -> SessionState {
        self.journal
            .inspect(Clone::clone)
            .expect("turn loop journal inspection")
    }

    /// Take the session state back, to resume with a different loop.
    pub fn into_state(self) -> Result<SessionState, SharedJournalError> {
        let Self {
            journal,
            hook_runtime,
            foreign_question_port,
            ..
        } = self;
        drop(foreign_question_port);
        drop(hook_runtime);
        journal.try_into_state()
    }

    /// Produce and validate one clean-session checkpoint without publishing a target.
    pub async fn prepare_compaction(
        &mut self,
        target_system: Option<SystemPrompt>,
        target_tools: Vec<ToolDescriptor>,
    ) -> Result<PreparedCompaction, CompactionError> {
        if !self.state().unacknowledged_prompts().is_empty() {
            return Err(CompactionError::NotIdle(CompactionBusy::AcceptedPrompt));
        }
        if self.state().pending_question().is_some() {
            return Err(CompactionError::NotIdle(CompactionBusy::PendingQuestion));
        }
        if !self.state().live_attempts.is_empty() {
            return Err(CompactionError::NotIdle(CompactionBusy::LiveAttempt));
        }
        if let Some(cause) = self.cancel.cause() {
            return Err(CompactionError::Cancelled(cause));
        }

        let source_seq = self.state().last_seq;
        let turn = TurnId::new();
        self.journal
            .publish(&PublicEvent::TurnStarted { turn })
            .map_err(|error| CompactionError::Store(error.into()))?;
        let result = self
            .prepare_compaction_inner(turn, source_seq, target_system, target_tools)
            .await;
        let (ok, exit_reason) = match &result {
            Ok(_) => (true, ExitReason::Completed),
            Err(CompactionError::Cancelled(cause)) => (false, ExitReason::Cancelled(*cause)),
            Err(CompactionError::Budget(_)) => (false, ExitReason::Budget),
            Err(CompactionError::Resolution { error, .. }) => (
                false,
                match error.class {
                    GenerationModelErrorClass::Usage => ExitReason::Usage,
                    GenerationModelErrorClass::Provider => ExitReason::Provider,
                    GenerationModelErrorClass::Internal => ExitReason::Internal,
                },
            ),
            Err(CompactionError::Capability { .. } | CompactionError::Expressibility { .. }) => {
                (false, ExitReason::Usage)
            }
            Err(
                CompactionError::History(_)
                | CompactionError::UnexpectedStop(_)
                | CompactionError::EmptyCheckpoint
                | CompactionError::InvalidCheckpoint(_),
            ) => (false, ExitReason::Provider),
            Err(CompactionError::Store(_) | CompactionError::Internal(_)) => {
                (false, ExitReason::Internal)
            }
            Err(CompactionError::NotIdle(_)) => {
                unreachable!("idle was checked before the operation")
            }
        };
        self.journal
            .append(
                turn,
                None,
                None,
                EventKind::TurnCompleted { ok, exit_reason },
            )
            .map_err(|error| CompactionError::Store(error.into()))?;
        self.deactivate_skill_hooks(turn)
            .map_err(CompactionError::Store)?;
        result
    }

    async fn prepare_compaction_inner(
        &mut self,
        turn: TurnId,
        source_seq: crate::event::id::Seq,
        target_system: Option<SystemPrompt>,
        target_tools: Vec<ToolDescriptor>,
    ) -> Result<PreparedCompaction, CompactionError> {
        let (history, repairs) =
            conversation_from_with_blobs(&self.state(), self.tools.blob_store())
                .map_err(CompactionError::History)?;
        let history = self
            .with_ancestor(history)
            .map_err(CompactionError::History)?;
        if !repairs.is_empty() {
            tracing::warn!(?repairs, "source history was repaired for compaction");
        }
        let mut source_messages: Vec<Message> = self.with_system(history).into();
        source_messages.extend(
            self.durable_skill_messages()
                .map_err(CompactionError::Store)?,
        );
        source_messages.push(Message::text(Role::System, COMPACTION_PROMPT));
        let source_messages =
            Conversation::new(source_messages).map_err(CompactionError::History)?;

        let source = Arc::clone(&self.source);
        let profile = source
            .resolve(GenerationModelRequest::new(GenerationRole::Compaction))
            .await
            .map_err(|error| CompactionError::Resolution {
                role: GenerationRole::Compaction,
                error,
            })?;
        let request = GenRequest {
            model: profile.request_model.clone(),
            messages: source_messages,
            tools: Vec::new(),
            thinking: profile.thinking,
            max_output_tokens: profile.output_ceiling(),
            parallel_tool_calls: false,
        };
        let gaps = profile.capabilities.request_gaps(&request);
        if !gaps.is_empty() {
            return Err(CompactionError::Capability {
                role: GenerationRole::Compaction,
                gaps,
            });
        }
        profile
            .generator
            .ensure_expressible(&request)
            .map_err(|error| CompactionError::Expressibility {
                role: GenerationRole::Compaction,
                error,
            })?;

        let budget = self
            .gate
            .snapshot()
            .map_err(|error| CompactionError::Internal(error.to_string()))?;
        self.journal
            .publish(&PublicEvent::metering_selected(
                turn,
                &profile.metering,
                budget.ledger(),
                profile
                    .rates
                    .as_ref()
                    .map_or(&RateSource::None, |rates| &rates.source),
            ))
            .map_err(|error| CompactionError::Store(error.into()))?;

        let quote = profile.rates.as_ref().map(|rates| rates.quote.clone());
        let reservation = self
            .gate
            .reserve(
                &profile.metering,
                quote.as_ref(),
                &request,
                &profile.capabilities,
            )
            .map_err(|error| match error {
                SharedBudgetError::Denied(error) => CompactionError::Budget(error),
                error => CompactionError::Internal(error.to_string()),
            })?;
        let attempt = GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &profile.metering,
            quote.as_ref(),
            &request,
            &profile.capabilities,
        )
        .map_err(CompactionError::Budget)?;
        let reference = attempt.attempt();
        self.admit_generation(&profile, reference, GenerationRole::Compaction)
            .map_err(CompactionError::Store)?;
        let generation = self.journal.append(
            turn,
            None,
            None,
            EventKind::GenerationStarted {
                attempt: reference,
                role: GenerationRole::Compaction,
            },
        )?;

        let stream = profile.generator.generate(request, attempt);
        let drained = self
            .drain(
                turn,
                stream,
                DrainGeneration {
                    event: generation,
                    metering: &profile.metering,
                    quote: quote.as_ref(),
                    visibility: GenerationVisibility::CompactionPrivate,
                    catalogue: None,
                },
            )
            .await;

        let settlement = self
            .gate
            .settle(drained.stream)
            .map_err(|error| CompactionError::Internal(error.to_string()))?;
        if let Some(error) = drained.failure {
            return Err(CompactionError::Store(error));
        }
        if !drained.accounting_booked {
            self.journal.append(
                turn,
                None,
                None,
                EventKind::UsageUpdated {
                    usage: Usage::IDENTITY,
                    cost_delta: settled_cost(
                        &profile.metering,
                        quote.as_ref(),
                        UsageReport::Missing,
                        None,
                        self.gate
                            .snapshot()
                            .map_err(|error| CompactionError::Internal(error.to_string()))?
                            .currency(),
                    ),
                    attempt: Some(reference),
                },
            )?;
        }
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
            return Err(CompactionError::Cancelled(
                self.cancel.cause().unwrap_or(CancelCause::User),
            ));
        }
        if drained.private_tool_use
            || drained.error_kind.is_some()
            || settlement.outcome.stop() != StopReason::Stop
        {
            return Err(CompactionError::UnexpectedStop(
                if drained.private_tool_use {
                    StopReason::ToolUse
                } else if drained.error_kind.is_some() {
                    StopReason::Error
                } else {
                    settlement.outcome.stop()
                },
            ));
        }
        let checkpoint_text = drained.private_text;
        if checkpoint_text.trim().is_empty() {
            return Err(CompactionError::EmptyCheckpoint);
        }
        let checkpoint =
            Checkpoint::parse(checkpoint_text).map_err(CompactionError::InvalidCheckpoint)?;

        let main = Arc::clone(&self.source)
            .resolve(GenerationModelRequest::new(GenerationRole::Main))
            .await
            .map_err(|error| CompactionError::Resolution {
                role: GenerationRole::Main,
                error,
            })?;
        let mut target_messages = Vec::new();
        if let Some(system) = target_system {
            target_messages.push(Message::text(Role::System, system.text));
        }
        target_messages.push(Message::text(
            Role::User,
            checkpoint_frame(checkpoint.text()),
        ));
        let baseline = GenRequest {
            model: main.request_model.clone(),
            messages: Conversation::new(target_messages).map_err(CompactionError::History)?,
            tools: target_tools,
            thinking: main.thinking,
            max_output_tokens: main.output_ceiling(),
            parallel_tool_calls: main.capabilities.supports_parallel_tool_calls,
        };
        let gaps = main.capabilities.request_gaps(&baseline);
        if !gaps.is_empty() {
            return Err(CompactionError::Capability {
                role: GenerationRole::Main,
                gaps,
            });
        }
        main.generator
            .ensure_expressible(&baseline)
            .map_err(|error| CompactionError::Expressibility {
                role: GenerationRole::Main,
                error,
            })?;

        Ok(PreparedCompaction {
            source: self.session_id,
            source_seq,
            checkpoint,
            baseline,
        })
    }

    /// Run one turn: the prompt, every generation it takes to answer it, and
    /// the tool calls in between.
    #[expect(
        clippy::result_large_err,
        reason = "TurnAbandoned carries the exact durable session state by contract"
    )]
    pub async fn run(&mut self, prompt: &str) -> Result<TurnOutcome, TurnAbandoned> {
        self.run_with_images(prompt, Vec::new()).await
    }

    /// Run one turn whose first user message carries durable image refs.
    #[expect(
        clippy::result_large_err,
        reason = "TurnAbandoned carries the exact durable session state by contract"
    )]
    pub async fn run_with_images(
        &mut self,
        prompt: &str,
        images: Vec<crate::generator::types::ImageRef>,
    ) -> Result<TurnOutcome, TurnAbandoned> {
        self.run_prompts(vec![(prompt.to_string(), images, None)])
            .await
    }

    /// Settle one durable pending question and continue its original turn.
    /// No new turn or user prompt is minted.
    #[expect(
        clippy::result_large_err,
        reason = "TurnAbandoned carries the exact durable session state by contract"
    )]
    pub async fn resume_question(
        &mut self,
        question: ItemId,
        answer: QuestionAnswer,
    ) -> Result<TurnOutcome, TurnAbandoned> {
        let foreign = self
            .state()
            .pending_question()
            .filter(|pending| pending.item_id() == question)
            .and_then(|pending| match pending.target() {
                QuestionTarget::ForeignContinuation {
                    tool_call,
                    continuation_id,
                } => Some((
                    pending.turn(),
                    pending.request_event(),
                    pending.request().clone(),
                    *tool_call,
                    continuation_id.clone(),
                )),
                QuestionTarget::ModelTool { .. } => None,
            });
        if let Some((turn, parent, request, tool_call, continuation_id)) = foreign {
            if let Err(error) = answer.validate_for(&request) {
                return Err(self.abandon(
                    turn,
                    StoreError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error)),
                ));
            }
            let Some(port) = self.foreign_question_port.as_ref().map(Arc::clone) else {
                self.close_control();
                self.refresh_control_status();
                return Ok(TurnOutcome {
                    turn,
                    ok: false,
                    exit_reason: ExitReason::PermissionRequired,
                    denied: None,
                });
            };
            let progress = match self
                .settle_foreign_question(
                    ForeignQuestionSettlement {
                        turn,
                        question,
                        request_event: parent,
                        tool_call,
                        continuation_id,
                    },
                    answer,
                    port,
                )
                .await
            {
                Ok(progress) => progress,
                Err(cause) => return Err(self.abandon(turn, cause)),
            };
            match progress {
                ForeignQuestionProgress::Completed => return self.resume_turn(turn).await,
                ForeignQuestionProgress::Successor { required, parent } => {
                    let outcome = match self
                        .run_foreign_question(turn, tool_call, parent, required)
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(cause) => return Err(self.abandon(turn, cause)),
                    };
                    return match outcome {
                        ToolOutcome::Recorded => self.resume_turn(turn).await,
                        ToolOutcome::QuestionRequired => {
                            self.close_control();
                            self.refresh_control_status();
                            Ok(TurnOutcome {
                                turn,
                                ok: false,
                                exit_reason: ExitReason::PermissionRequired,
                                denied: None,
                            })
                        }
                        ToolOutcome::Ended(exit_reason) => Ok(TurnOutcome {
                            turn,
                            ok: false,
                            exit_reason,
                            denied: None,
                        }),
                        ToolOutcome::Answered { .. }
                        | ToolOutcome::Executed { .. }
                        | ToolOutcome::Refused { .. }
                        | ToolOutcome::Unanswerable { .. } => unreachable!(
                            "foreign question continuation returns only recorded, pending, or ended"
                        ),
                    };
                }
            }
        }
        let prepared = self.state().resume_question(question, answer.clone());
        let continuation = match prepared {
            Ok(continuation) => {
                let (parent, turn) = {
                    let state = self.state();
                    let pending = state
                        .pending_question()
                        .expect("resume validation found the pending question");
                    (pending.request_event(), pending.turn())
                };
                let settlement = match self.journal.append(
                    turn,
                    Some(question),
                    Some(parent),
                    EventKind::QuestionSettled {
                        outcome: QuestionOutcome::Answered(answer),
                    },
                ) {
                    Ok(event) => event,
                    Err(cause) => return Err(self.abandon(turn, cause)),
                };
                if let Err(cause) = self.journal.append(
                    turn,
                    Some(continuation.tool_call),
                    Some(settlement),
                    EventKind::ToolCallCompleted {
                        ok: continuation.ok(),
                        result: continuation.tool_result.clone(),
                    },
                ) {
                    return Err(self.abandon(turn, cause));
                }
                continuation
            }
            Err(crate::question::QuestionError::AlreadySettled) => {
                let continuation = match self.state().complete_settled_question(question) {
                    Ok(continuation) => continuation,
                    Err(error) => {
                        let turn = self
                            .state()
                            .settled_question(question)
                            .map_or_else(TurnId::new, |settled| settled.turn());
                        return Err(self.abandon(
                            turn,
                            StoreError::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                error,
                            )),
                        ));
                    }
                };
                let parent = self
                    .state()
                    .settled_question(question)
                    .expect("the settlement used for recovery exists")
                    .settlement_event();
                if let Err(cause) = self.journal.append(
                    continuation.turn,
                    Some(continuation.tool_call),
                    Some(parent),
                    EventKind::ToolCallCompleted {
                        ok: continuation.ok(),
                        result: continuation.tool_result.clone(),
                    },
                ) {
                    return Err(self.abandon(continuation.turn, cause));
                }
                continuation
            }
            Err(error) => {
                return Err(self.abandon(
                    self.state()
                        .pending_question()
                        .map_or_else(TurnId::new, |pending| pending.turn()),
                    StoreError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error)),
                ));
            }
        };
        self.resume_turn(continuation.turn).await
    }

    /// Continue an already-settled pure question result on its original turn.
    #[expect(
        clippy::result_large_err,
        reason = "TurnAbandoned carries the exact durable session state by contract"
    )]
    pub async fn resume_turn(&mut self, turn: TurnId) -> Result<TurnOutcome, TurnAbandoned> {
        if let Err(cause) = self.admit_skill_catalogue(turn) {
            return Err(self.abandon(turn, cause));
        }
        if let Err(cause) = self.restore_skill_scope(turn) {
            return Err(self.abandon(turn, cause));
        }
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
        let driven = match self.drive(turn, &[]).await {
            Ok(driven) => driven,
            Err(cause) => return Err(self.abandon(turn, cause)),
        };
        let (exit_reason, denied) = match driven {
            DriveOutcome::Ended(reason, denied) => (reason, denied),
            DriveOutcome::QuestionRequired => {
                self.close_control();
                return Ok(TurnOutcome {
                    turn,
                    ok: false,
                    exit_reason: ExitReason::PermissionRequired,
                    denied: None,
                });
            }
        };
        let ok = exit_reason == ExitReason::Completed;
        if let Err(cause) = self.journal.append(
            turn,
            None,
            None,
            EventKind::TurnCompleted { ok, exit_reason },
        ) {
            return Err(self.abandon(turn, cause));
        }
        if let Err(cause) = self.deactivate_skill_hooks(turn) {
            return Err(self.abandon(turn, cause));
        }
        if let Err(cause) = self.close_and_drain_controls(turn).await {
            return Err(self.abandon(turn, cause));
        }
        self.refresh_control_status();
        Ok(TurnOutcome {
            turn,
            ok,
            exit_reason,
            denied,
        })
    }

    fn restore_skill_scope(&mut self, turn: TurnId) -> Result<(), StoreError> {
        let invocations = self
            .state()
            .skill_events()
            .iter()
            .filter_map(|event| match event {
                crate::state::SkillEvent::Instruction {
                    turn: admitted_turn,
                    invocation,
                    ..
                } if *admitted_turn == turn => Some(*invocation),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.deactivate_skill_hooks(turn)?;
        for invocation_id in invocations {
            let invocation = crate::skill::SkillInvocation::resume_durable(
                invocation_id,
                self.tools.blob_store(),
                &self.journal,
            )
            .map_err(skill_store_error)?;
            let baseline = self.tools.effect_names();
            let scope = self
                .active_skill_scope
                .get_or_insert_with(|| crate::skill::ActiveSkillScope::new(baseline));
            invocation
                .apply_captured_scope(scope)
                .map_err(skill_store_error)?;
            self.active_skill_invocations
                .insert(invocation_id, invocation);
        }
        self.refresh_skill_hook_catalogue(turn)?;
        Ok(())
    }

    fn refresh_skill_hook_catalogue(&self, turn: TurnId) -> Result<(), StoreError> {
        let mut sources = Vec::new();
        for invocation in self
            .state()
            .skill_events()
            .iter()
            .filter_map(|event| match event {
                crate::state::SkillEvent::Instruction {
                    turn: admitted_turn,
                    invocation,
                    ..
                } if *admitted_turn == turn => Some(invocation),
                _ => None,
            })
        {
            let Some(active) = self.active_skill_invocations.get(invocation) else {
                continue;
            };
            sources.push(active.hook_source().map_err(skill_store_error)?);
        }
        match self.hook_runtime.as_ref() {
            Some(runtime) => runtime
                .replace_skill_sources(turn, &sources)
                .map_err(hook_store_error),
            None if sources.iter().any(|source| {
                matches!(source, crate::hook::HookSource::Skill { entries, .. } if !entries.is_empty())
            }) => Err(StoreError::Io(std::io::Error::other(
                "skill hooks require the session hook runtime",
            ))),
            None => Ok(()),
        }
    }

    fn deactivate_skill_hooks(&mut self, turn: TurnId) -> Result<(), StoreError> {
        let had_active = !self.active_skill_invocations.is_empty();
        self.active_skill_invocations.clear();
        self.active_skill_scope = None;
        if had_active {
            self.hook_runtime
                .as_ref()
                .ok_or_else(|| {
                    StoreError::Io(std::io::Error::other(
                        "skill hooks require the session hook runtime",
                    ))
                })?
                .replace_skill_sources(turn, &[])
                .map_err(hook_store_error)?;
        }
        Ok(())
    }

    /// Consume every accepted prompt that lacks a correlated `UserPrompt` as
    /// one fresh user turn. Empty recovery is not a turn and spends nothing.
    #[expect(
        clippy::result_large_err,
        reason = "TurnAbandoned carries the exact durable session state by contract"
    )]
    pub async fn run_queued_turn(&mut self) -> Result<Option<TurnOutcome>, TurnAbandoned> {
        let prompts: Vec<_> = self
            .state()
            .unacknowledged_prompts()
            .into_iter()
            .map(|prompt| {
                (
                    prompt.text().to_string(),
                    prompt.images().to_vec(),
                    Some(prompt.id()),
                )
            })
            .collect();
        if prompts.is_empty() {
            self.close_control();
            return Ok(None);
        }
        self.run_prompts(prompts).await.map(Some)
    }

    #[expect(
        clippy::result_large_err,
        reason = "TurnAbandoned carries the exact durable session state by contract"
    )]
    async fn run_prompts(
        &mut self,
        prompts: Vec<(
            String,
            Vec<crate::generator::types::ImageRef>,
            Option<crate::event::id::QueuedPromptId>,
        )>,
    ) -> Result<TurnOutcome, TurnAbandoned> {
        self.active_skill_scope = None;
        self.active_skill_invocations.clear();
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
        if let Err(cause) = self.journal.publish(&PublicEvent::TurnStarted { turn }) {
            return Err(self.abandon(turn, cause));
        }

        if let Err(cause) = self.admit_skill_catalogue(turn) {
            return Err(self.abandon(turn, cause));
        }

        let driven = match self.drive(turn, &prompts).await {
            Ok(ended) => ended,
            Err(cause) => return Err(self.abandon(turn, cause)),
        };
        let (exit_reason, denied) = match driven {
            DriveOutcome::Ended(exit_reason, denied) => (exit_reason, denied),
            DriveOutcome::QuestionRequired => {
                self.close_control();
                self.refresh_control_status();
                return Ok(TurnOutcome {
                    turn,
                    ok: false,
                    exit_reason: ExitReason::PermissionRequired,
                    denied: None,
                });
            }
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
        self.active_skill_scope = None;
        self.active_skill_invocations.clear();
        if let Err(cause) = self.close_and_drain_controls(turn).await {
            return Err(self.abandon(turn, cause));
        }
        self.refresh_control_status();

        Ok(TurnOutcome {
            turn,
            ok,
            exit_reason,
            denied,
        })
    }

    fn admit_skill_catalogue(&mut self, turn: TurnId) -> Result<(), StoreError> {
        let Some(catalogue) = self.tools.skill_catalogue() else {
            return Ok(());
        };
        if self.state().skill_catalogue_generation() == Some(catalogue.generation()) {
            return Ok(());
        }
        self.journal
            .append(
                turn,
                None,
                None,
                EventKind::SkillCatalogueAdmitted {
                    generation: catalogue.generation(),
                    descriptors: catalogue.evidence().clone(),
                },
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    /// Give up on a turn whose log could not be written.
    ///
    /// Takes the state by clone rather than by move: `run` borrows `self`, and
    /// a caller holding a `TurnLoop` is entitled to look at what survived.
    fn abandon(&mut self, turn: TurnId, cause: impl Into<StoreError>) -> TurnAbandoned {
        let cause = cause.into();
        self.close_control();
        tracing::error!(?turn, %cause, "turn abandoned: its log could not be written");
        TurnAbandoned {
            turn,
            cause,
            state: self.state(),
        }
    }

    /// Everything between the prompt and `TurnCompleted`.
    ///
    /// Returns the reason and, when that reason is
    /// [`ExitReason::PermissionRequired`], what was refused and by whom.
    async fn drive(
        &mut self,
        turn: TurnId,
        prompts: &[(
            String,
            Vec<crate::generator::types::ImageRef>,
            Option<crate::event::id::QueuedPromptId>,
        )],
    ) -> Result<DriveOutcome, StoreError> {
        for (text, images, queued_prompt) in prompts {
            let item = ItemId::new();
            let admitted = self.journal.append(
                turn,
                Some(item),
                None,
                EventKind::UserPrompt {
                    text: text.clone(),
                    images: images.clone(),
                    queued_prompt: *queued_prompt,
                },
            )?;
            if !self
                .dispatch_hook(
                    HookCausality::item(item, turn, admitted),
                    HookEvent::turn(HookLifecycle::UserPromptSubmit, HookSubject::Item { item }),
                )
                .await
            {
                return Ok(DriveOutcome::Ended(ExitReason::Completed, None));
            }
            if queued_prompt.is_some() {
                self.refresh_control_status();
            }
        }

        loop {
            if let Some(cause) = self.cancel.cause() {
                return Ok(DriveOutcome::Ended(ExitReason::Cancelled(cause), None));
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
            if tokens_reached(self.config.token_ceiling, &self.state().usage) {
                return Ok(DriveOutcome::Ended(ExitReason::Budget, None));
            }

            let (generation, terminal, reason, pending, error_kind) =
                match self.generate(turn).await? {
                    Generated::Ended(reason) => return Ok(DriveOutcome::Ended(reason, None)),
                    Generated::Cancelled { cause, pending } => {
                        self.close_unstarted_siblings(turn, &pending)?;
                        return Ok(DriveOutcome::Ended(ExitReason::Cancelled(cause), None));
                    }
                    Generated::Stopped {
                        generation,
                        terminal,
                        reason,
                        pending,
                        error_kind,
                    } => (generation, terminal, reason, pending, error_kind),
                };

            // A generation that says it stopped for tools and then names none
            // has nothing to answer. Generating again on that evidence loops
            // forever against a provider that keeps saying it.
            if reason != StopReason::ToolUse || pending.is_empty() {
                let exit = match reason {
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
                        Some(ErrorKind::ContextOverflow) => ExitReason::ContextOverflow,
                        Some(ErrorKind::Invalid) => ExitReason::Usage,
                        _ => ExitReason::Provider,
                    },
                };
                if exit == ExitReason::Completed && self.deliver_pending(turn)? {
                    continue;
                }
                if reason == StopReason::Stop {
                    if !self
                        .dispatch_hook(
                            HookCausality::generation(generation, turn, terminal),
                            HookEvent::turn(HookLifecycle::Stop, HookSubject::Turn { turn }),
                        )
                        .await
                    {
                        continue;
                    }
                } else if exit != ExitReason::Completed {
                    let _ = self
                        .dispatch_hook(
                            HookCausality::generation(generation, turn, terminal),
                            HookEvent::turn(HookLifecycle::StopFailure, HookSubject::Turn { turn }),
                        )
                        .await;
                }
                return Ok(DriveOutcome::Ended(exit, None));
            }

            if pending.len() != 1
                && pending.iter().any(|call| {
                    matches!(call.selected.as_ref(), Some(Ok(RegisteredTool::Skill(_))))
                })
            {
                for call in &pending {
                    self.journal.append(
                        turn,
                        Some(call.item),
                        Some(call.started),
                        EventKind::ToolCallCompleted {
                            ok: false,
                            result: ToolResult::builtin(
                                "a Skill call must be the only call in its provider response",
                            ),
                        },
                    )?;
                }
                if !self
                    .dispatch_hook(
                        HookCausality::generation(generation, turn, terminal),
                        HookEvent::turn(HookLifecycle::PostToolBatch, HookSubject::Turn { turn }),
                    )
                    .await
                {
                    return Ok(DriveOutcome::Ended(ExitReason::Completed, None));
                }
                self.deliver_pending(turn)?;
                continue;
            }

            let mut pending = pending;
            for index in 0..pending.len() {
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
                let denial = match self.run_tool(turn, &mut pending[index]).await? {
                    ToolOutcome::Answered { ok, result } => {
                        let call = &pending[index];
                        self.journal.append(
                            turn,
                            Some(call.item),
                            Some(call.started),
                            EventKind::ToolCallCompleted { ok, result },
                        )?;
                        if let Some(cause) = self.cancel.cause() {
                            self.close_unstarted_siblings(turn, &pending[index + 1..])?;
                            return Ok(DriveOutcome::Ended(ExitReason::Cancelled(cause), None));
                        }
                        continue;
                    }
                    ToolOutcome::Executed {
                        ok,
                        result,
                        selector,
                    } => {
                        let call = &pending[index];
                        self.journal.append(
                            turn,
                            Some(call.item),
                            Some(call.started),
                            EventKind::ToolCallCompleted { ok, result },
                        )?;
                        let lifecycle = if ok {
                            HookLifecycle::PostToolUse
                        } else {
                            HookLifecycle::PostToolUseFailure
                        };
                        let _ = self
                            .dispatch_hook(
                                HookCausality::item(call.item, turn, call.started),
                                HookEvent::tool(
                                    lifecycle,
                                    HookSubject::Item { item: call.item },
                                    selector,
                                ),
                            )
                            .await;
                        if let Some(cause) = self.cancel.cause() {
                            self.close_unstarted_siblings(turn, &pending[index + 1..])?;
                            return Ok(DriveOutcome::Ended(ExitReason::Cancelled(cause), None));
                        }
                        continue;
                    }
                    ToolOutcome::Recorded => {
                        if let Some(cause) = self.cancel.cause() {
                            self.close_unstarted_siblings(turn, &pending[index + 1..])?;
                            return Ok(DriveOutcome::Ended(ExitReason::Cancelled(cause), None));
                        }
                        continue;
                    }
                    ToolOutcome::QuestionRequired => {
                        self.close_unstarted_siblings(turn, &pending[index + 1..])?;
                        return Ok(DriveOutcome::QuestionRequired);
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
                    ToolOutcome::Ended(reason) => {
                        self.close_unstarted_siblings(turn, &pending[index + 1..])?;
                        return Ok(DriveOutcome::Ended(reason, None));
                    }
                };
                let call = &pending[index];
                self.journal.append(
                    turn,
                    Some(call.item),
                    Some(call.started),
                    EventKind::ToolCallCompleted {
                        ok: false,
                        result: ToolResult::builtin(denial.action.clone()),
                    },
                )?;
                return Ok(DriveOutcome::Ended(
                    ExitReason::PermissionRequired,
                    Some(denial),
                ));
            }
            if !self
                .dispatch_hook(
                    HookCausality::generation(generation, turn, terminal),
                    HookEvent::turn(HookLifecycle::PostToolBatch, HookSubject::Turn { turn }),
                )
                .await
            {
                return Ok(DriveOutcome::Ended(ExitReason::Completed, None));
            }
            self.deliver_pending(turn)?;
        }
    }

    async fn dispatch_hook(&self, causality: HookCausality, event: HookEvent) -> bool {
        let Some(runtime) = self.hook_runtime.as_ref() else {
            return true;
        };
        let lifecycle = event.lifecycle();
        let occurrence = HookOccurrence::new(
            HookOccurrenceKey::new(lifecycle, causality.owner().clone()),
            event,
            causality,
        )
        .expect("the lifecycle key is built from the same typed hook event");
        match runtime.dispatch(Producer::Main, occurrence).await {
            Ok(result) => !result.is_blocked(),
            Err(error) if lifecycle.phase().is_observer() => {
                tracing::warn!(%error, ?lifecycle, "hook observer dispatch failed");
                true
            }
            Err(error) => {
                tracing::warn!(%error, ?lifecycle, "hook gate dispatch failed closed");
                false
            }
        }
    }

    fn refresh_control_status(&mut self) {
        let status = self.state().queue_status();
        if let Some(control) = self.control.as_mut() {
            control.refresh(status);
        }
    }

    fn close_control(&mut self) {
        let status = self.state().queue_status();
        if let Some(control) = self.control.as_deref_mut() {
            control.refresh(status);
            control.close();
        }
    }

    /// Close busy admission at the idle transition and durably accept every
    /// intent ordered before that barrier.
    async fn close_and_drain_controls(&mut self, turn: TurnId) -> Result<(), StoreError> {
        loop {
            let boundary = match self.control.as_deref_mut() {
                Some(control) => control.next_or_close().await,
                None => return Ok(()),
            };
            match boundary {
                BoundaryIntent::Intent(intent) => {
                    let control = self
                        .control
                        .as_deref_mut()
                        .expect("the boundary intent came from this coordinator");
                    let _ = apply_intent(&self.journal, &self.cancel, control, turn, intent)?;
                }
                BoundaryIntent::Closed => {
                    self.control = None;
                    return Ok(());
                }
                BoundaryIntent::Quiescent => return Ok(()),
            }
        }
    }

    /// Correlate input accepted by this turn only at a generation-safe
    /// boundary. Interrupted turns leave every queued id for a fresh turn.
    fn deliver_pending(&mut self, turn: TurnId) -> Result<bool, StoreError> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        let pending: Vec<_> = self
            .state()
            .unacknowledged_prompts()
            .into_iter()
            .filter(|prompt| prompt.turn() == turn)
            .map(|prompt| {
                (
                    prompt.id(),
                    prompt.text().to_string(),
                    prompt.images().to_vec(),
                )
            })
            .collect();
        for (id, text, images) in &pending {
            self.journal.append(
                turn,
                Some(ItemId::new()),
                None,
                EventKind::UserPrompt {
                    text: text.clone(),
                    images: images.clone(),
                    queued_prompt: Some(*id),
                },
            )?;
            self.refresh_control_status();
        }
        Ok(!pending.is_empty())
    }

    fn close_unstarted_siblings(
        &mut self,
        turn: TurnId,
        siblings: &[PendingCall],
    ) -> Result<(), StoreError> {
        for call in siblings {
            self.journal.append(
                turn,
                Some(call.item),
                Some(call.started),
                EventKind::ToolCallCompleted {
                    ok: false,
                    result: ToolResult::builtin("interrupted: not run"),
                },
            )?;
        }
        Ok(())
    }

    /// One generation: reserve, stream it into the log, settle.
    async fn generate(&mut self, turn: TurnId) -> Result<Generated, StoreError> {
        // A log this build wrote cannot fail here: the guard in `drain` refuses
        // a reused call id before it is ever appended. A log written before
        // that guard existed can, and there is nothing to do about it from
        // here — the log is never rewritten, so the run ends visibly at the
        // provider's exit code rather than resuming a session with a hole in
        // it (ADR §6).
        let rebuilt = conversation_from_with_blobs(&self.state(), self.tools.blob_store())
            .and_then(|(history, repairs)| {
                self.with_ancestor(history)
                    .map(|history| (history, repairs))
            });
        let (history, repairs) = match rebuilt {
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

        let source = Arc::clone(&self.source);
        let model_request = self.main_model_request();
        let profile = match await_controlled(
            &self.journal,
            &self.cancel,
            &mut self.control,
            turn,
            source.resolve(model_request),
            InterruptDisposition::Return,
        )
        .await
        {
            Controlled::Completed(Ok(profile)) => profile,
            Controlled::Completed(Err(error)) => {
                let exit = match error.class {
                    GenerationModelErrorClass::Usage => ExitReason::Usage,
                    GenerationModelErrorClass::Provider => ExitReason::Provider,
                    GenerationModelErrorClass::Internal => ExitReason::Internal,
                };
                return self.refuse(turn, error.message, exit);
            }
            Controlled::Interrupted => {
                return Ok(Generated::Cancelled {
                    cause: self.cancel.cause().unwrap_or(CancelCause::User),
                    pending: Vec::new(),
                });
            }
            Controlled::Failed(cause) => return Err(cause),
        };

        let ceiling = self
            .active_skill_scope
            .as_ref()
            .map(crate::skill::ActiveSkillScope::effect_tools);
        let catalogue = match self.tools.capture_with_effect_ceiling(ceiling.as_deref()) {
            Ok(catalogue) => catalogue,
            Err(error) => {
                tracing::warn!(%error, "the admitted tool catalogue refused a generation");
                return self.refuse(turn, error.to_string(), ExitReason::Internal);
            }
        };

        let request = GenRequest {
            model: profile.request_model.clone(),
            messages: conversation,
            tools: catalogue.descriptors(),
            thinking: profile.thinking,
            max_output_tokens: profile.output_ceiling(),
            parallel_tool_calls: profile.capabilities.supports_parallel_tool_calls,
        };

        // Before the gate, deliberately: a reservation taken for a request
        // the model will reject has authorised spending on something that was
        // never going to be sent
        // (`docs/decisions/2026-08-21-a-request-the-model-will-not-take.md`).
        let gaps = profile.capabilities.request_gaps(&request);
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
        if let Err(refused) = profile.generator.ensure_expressible(&request) {
            tracing::warn!(%refused, "the selected generator cannot encode this request");
            return self.refuse(turn, refused.to_string(), ExitReason::Usage);
        }

        // Per generation, and before the gate: a fallback chain can switch
        // rows mid-turn, and on a row that is not metered in currency the
        // gate is about to mint a zero reservation — which means a monetary
        // `--budget` admits everything. The operator is told that here rather
        // than discovering it from a limit that never fired.
        let budget = self
            .gate
            .snapshot()
            .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?;
        self.journal.publish(&PublicEvent::metering_selected(
            turn,
            &profile.metering,
            budget.ledger(),
            profile
                .rates
                .as_ref()
                .map_or(&RateSource::None, |rates| &rates.source),
        ))?;

        let quote = profile.rates.as_ref().map(|rates| rates.quote.clone());
        let reservation = match self.gate.reserve(
            &profile.metering,
            quote.as_ref(),
            &request,
            &profile.capabilities,
        ) {
            Ok(reservation) => reservation,
            Err(denied) => {
                tracing::warn!(%denied, "the budget gate refused a generation");
                let reason = if matches!(&denied, SharedBudgetError::Denied(_)) {
                    ExitReason::Budget
                } else {
                    ExitReason::Internal
                };
                return self.refuse(turn, denied.to_string(), reason);
            }
        };

        let attempt = match GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &profile.metering,
            quote.as_ref(),
            &request,
            &profile.capabilities,
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
        self.admit_generation(&profile, reference, GenerationRole::Main)?;
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

        let stream = profile.generator.generate(request, attempt);
        let drained = self
            .drain(
                turn,
                stream,
                DrainGeneration {
                    event: generation,
                    metering: &profile.metering,
                    quote: quote.as_ref(),
                    visibility: GenerationVisibility::Conversation,
                    catalogue: Some(&catalogue),
                },
            )
            .await;

        // Settle before reporting anything, including a store failure: the
        // stream owns the reservation, and returning early would drop it.
        let settlement = match self.gate.settle(drained.stream) {
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

        let terminal = self.journal.append(
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
            return Ok(Generated::Cancelled {
                cause: self.cancel.cause().unwrap_or(CancelCause::User),
                pending: drained.pending,
            });
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
            generation: reference.generation(),
            terminal,
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

    fn main_model_request(&self) -> GenerationModelRequest {
        let request = GenerationModelRequest::new(GenerationRole::Main);
        let Some(scope) = self.active_skill_scope.as_ref() else {
            return request;
        };
        request.with_skill_scope(
            scope.model().map(|model| ModelRef(model.to_owned())),
            scope.thinking(),
        )
    }

    /// The rebuilt history with the configured system prompt and the exact
    /// durable skill instructions admitted for this turn in front of it.
    ///
    /// Rebuilt through [`Conversation::new`] rather than pushed into: a
    /// `Conversation` is a *checked* history, and there is deliberately no way
    /// to add a message to one without checking it again. The check cannot
    /// fail here — a block of text makes no tool call and answers none, so a
    /// history that validated a moment ago still does — which is what the
    /// `expect` says.
    fn with_system(&self, history: Conversation) -> Conversation {
        if self.config.system.is_none() && self.active_skill_invocations.is_empty() {
            return history;
        }
        let mut messages: Vec<Message> = history.into();
        let mut system_messages = Vec::new();
        if let Some(system) = self.config.system.as_ref() {
            system_messages.push(Message::text(Role::System, &system.text));
        }
        for invocation in self.active_skill_invocations.values() {
            let rendered = format!(
                "Admitted skill instruction (invocation {}; {}):\n{}",
                invocation.id(),
                invocation.provenance_tag(),
                invocation.rendered_text()
            );
            system_messages.push(Message::text(Role::System, rendered));
        }
        messages.splice(0..0, system_messages);
        Conversation::new(messages)
            .expect("a text message adds no call and answers none, so a valid history stays valid")
    }

    fn durable_skill_messages(&self) -> Result<Vec<Message>, StoreError> {
        self.state()
            .skill_events()
            .iter()
            .filter_map(|event| match event {
                crate::state::SkillEvent::Instruction {
                    invocation,
                    source,
                    location,
                    source_digest,
                    rendered,
                    ..
                } if !self.active_skill_invocations.contains_key(invocation) => {
                    Some((invocation, source, location, source_digest, rendered))
                }
                crate::state::SkillEvent::Instruction { .. } => None,
                crate::state::SkillEvent::Resource { .. } => None,
            })
            .map(|(invocation, source, location, source_digest, rendered)| {
                let bytes =
                    self.tools.blob_store().read(rendered).map_err(|error| {
                        StoreError::Io(std::io::Error::other(error.to_string()))
                    })?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "rendered skill blob is not valid UTF-8",
                    ))
                })?;
                Ok(Message::text(
                    Role::System,
                    format!(
                        "Admitted skill instruction (invocation {invocation}; source={source:?} \
                         location={location} digest={source_digest}):\n{text}"
                    ),
                ))
            })
            .collect()
    }

    /// Prefix target-local history with its already-validated ancestor history.
    fn with_ancestor(
        &self,
        history: Conversation,
    ) -> Result<Conversation, crate::generator::types::HistoryError> {
        let Some(ancestor) = self.ancestor.as_ref() else {
            return Ok(history);
        };
        let mut messages: Vec<Message> = ancestor.clone().into();
        messages.extend(Vec::<Message>::from(history));
        Conversation::new(messages)
    }

    /// Turn one generation's stream into log entries.
    ///
    /// Returns the stream so its caller can settle it. A store failure is
    /// carried back rather than propagated with `?` for the same reason:
    /// leaving here early would drop an unsettled attempt.
    async fn drain(
        &mut self,
        turn: TurnId,
        mut stream: AttemptStream,
        generation: DrainGeneration<'_>,
    ) -> Drained {
        let DrainGeneration {
            event: generation,
            metering,
            quote,
            visibility,
            catalogue,
        } = generation;
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
            metering,
            Metering::Currency {
                settlement: SettlementSource::ProviderReported { .. },
                ..
            }
        );
        let mut held_usage: Option<Usage> = None;
        let mut stated_cost: Option<Money> = None;
        let mut accounting_booked = false;
        let mut private_tool_use = false;
        let settlement_currency = self.settlement_currency;

        loop {
            let event = match await_controlled(
                &self.journal,
                &self.cancel,
                &mut self.control,
                turn,
                stream.next(),
                InterruptDisposition::Return,
            )
            .await
            {
                Controlled::Completed(Some(event)) => event,
                Controlled::Completed(None) => break,
                Controlled::Interrupted => {
                    cancelled = true;
                    break;
                }
                Controlled::Failed(cause) => {
                    failure = Some(cause);
                    break;
                }
            };
            let appended = match event {
                AiEvent::TextDelta { text } => {
                    assembled.push_str(&text);
                    if visibility == GenerationVisibility::CompactionPrivate {
                        Ok(())
                    } else {
                        let item = *message.get_or_insert_with(ItemId::new);
                        self.journal
                            .append(turn, Some(item), None, EventKind::AssistantDelta { text })
                            .map(|_| ())
                    }
                }
                AiEvent::ReasoningDelta { text } => {
                    if visibility == GenerationVisibility::CompactionPrivate {
                        Ok(())
                    } else {
                        let item = *reasoning.get_or_insert_with(ItemId::new);
                        self.journal
                            .append(turn, Some(item), None, EventKind::ReasoningDelta { text })
                            .map(|_| ())
                    }
                }
                // A reused id ends the generation before anything runs (ADR
                // §6). Not appended, because a `ToolCallStarted` the loop has
                // already decided not to dispatch is a call the log says was
                // made and no result will ever answer.
                AiEvent::ToolCall { call_id, .. }
                    if visibility == GenerationVisibility::Conversation
                        && seen.contains(&call_id) =>
                {
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
                    if visibility == GenerationVisibility::CompactionPrivate {
                        Ok(())
                    } else {
                        // `unwrap_or_default` mints a fresh id here, not a nil
                        // one: `ItemId`'s `Default` is written as `new()`.
                        let item = reasoning.take().unwrap_or_default();
                        self.journal
                            .append(turn, Some(item), None, EventKind::ReasoningState { state })
                            .map(|_| ())
                    }
                }
                AiEvent::ToolCall {
                    call_id,
                    name,
                    args,
                } => {
                    if visibility == GenerationVisibility::CompactionPrivate {
                        private_tool_use = true;
                        Ok(())
                    } else {
                        let item = ItemId::new();
                        seen.insert(call_id.clone());
                        let catalogue = catalogue.expect(
                            "a conversation generation always retains its captured catalogue",
                        );
                        let selected = catalogue.select(&name);
                        let catalogue_generation = catalogue.generation();
                        let capture_authority = catalogue.authority();
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
                                    selected: Some(selected),
                                    args,
                                    _catalogue_generation: catalogue_generation,
                                    capture_authority,
                                });
                            })
                    }
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
                                metering,
                                quote,
                                UsageReport::Reported(usage),
                                None,
                                settlement_currency,
                            ),
                            attempt: Some(reference),
                        },
                    )
                    .map(|_| {
                        accounting_booked = true;
                    }),
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
                failure = Some(cause.into());
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
                metering,
                quote,
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
                failure = Some(cause.into());
            } else {
                accounting_booked = true;
            }
        }

        // The assembled message closes the item the deltas opened: the
        // reducer replaces rather than appends, so this is the same text, and
        // it is what tells a host the item finished.
        if visibility == GenerationVisibility::Conversation
            && failure.is_none()
            && let Some(item) = message
            && let Err(cause) = self.journal.append(
                turn,
                Some(item),
                None,
                EventKind::AssistantMessage {
                    text: assembled.clone(),
                },
            )
        {
            failure = Some(cause.into());
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
            private_text: assembled,
            private_tool_use,
            accounting_booked,
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
        let confinement = judge.confinement.clone();
        let exec_caps = judge.exec_caps;
        let source = Arc::clone(&self.source);
        let profile = match await_controlled(
            &self.journal,
            &self.cancel,
            &mut self.control,
            turn,
            source.resolve(GenerationModelRequest::new(GenerationRole::Judge)),
            InterruptDisposition::Return,
        )
        .await
        {
            Controlled::Completed(Ok(profile)) => profile,
            Controlled::Completed(Err(error)) => {
                self.journal.append(
                    turn,
                    None,
                    None,
                    EventKind::Error {
                        message: error.message,
                        kind: ErrorKind::Unclassified,
                    },
                )?;
                return Ok(JudgeRun::Unavailable);
            }
            Controlled::Interrupted => {
                return Ok(JudgeRun::Ended(ExitReason::Cancelled(
                    self.cancel.cause().unwrap_or(CancelCause::User),
                )));
            }
            Controlled::Failed(cause) => return Err(cause),
        };
        let config = JudgeConfig {
            identity: profile.identity.clone(),
            request_model: profile.request_model.clone(),
            max_output_tokens: profile.output_ceiling(),
        };
        let input = JudgeInput::new(
            pending.call(),
            &confinement,
            exec_caps,
            pending.request().why(),
        );
        let request = input.request(&config);

        let gaps = profile.capabilities.request_gaps(&request);
        if !gaps.is_empty() {
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
                    kind: ErrorKind::Unclassified,
                },
            )?;
            return Ok(JudgeRun::Unavailable);
        }
        if let Err(refused) = profile.generator.ensure_expressible(&request) {
            self.journal.append(
                turn,
                None,
                None,
                EventKind::Error {
                    message: refused.to_string(),
                    kind: ErrorKind::Unclassified,
                },
            )?;
            return Ok(JudgeRun::Unavailable);
        }
        let budget = self
            .gate
            .snapshot()
            .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?;
        self.journal.publish(&PublicEvent::metering_selected(
            turn,
            &profile.metering,
            budget.ledger(),
            profile
                .rates
                .as_ref()
                .map_or(&RateSource::None, |rates| &rates.source),
        ))?;
        let quote = profile.rates.as_ref().map(|rates| &rates.quote);
        let reservation =
            match self
                .gate
                .reserve(&profile.metering, quote, &request, &profile.capabilities)
            {
                Ok(reservation) => reservation,
                Err(denied) => {
                    tracing::warn!(%denied, "the budget gate refused the judge generation");
                    let reason = if matches!(&denied, SharedBudgetError::Denied(_)) {
                        ExitReason::Budget
                    } else {
                        ExitReason::Internal
                    };
                    return Ok(JudgeRun::Ended(reason));
                }
            };
        let attempt = match GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &profile.metering,
            quote,
            &request,
            &profile.capabilities,
        ) {
            Ok(attempt) => attempt,
            Err(denied) => {
                tracing::error!(%denied, "the judge reservation did not authorise its request");
                return Ok(JudgeRun::Ended(ExitReason::Internal));
            }
        };
        let reference = attempt.attempt();
        self.admit_generation(&profile, reference, GenerationRole::Judge)?;
        self.journal.append(
            turn,
            None,
            None,
            EventKind::GenerationStarted {
                attempt: reference,
                role: GenerationRole::Judge,
            },
        )?;

        let mut stream = profile.generator.generate(request, attempt);
        let mut decoder = JudgeDecoder::new();
        let mut usage: Option<Usage> = None;
        let mut provider_cost: Option<Money> = None;
        let mut interrupted = false;
        let mut failure = None;
        loop {
            let event = match await_controlled(
                &self.journal,
                &self.cancel,
                &mut self.control,
                turn,
                stream.next(),
                InterruptDisposition::Return,
            )
            .await
            {
                Controlled::Completed(Some(event)) => event,
                Controlled::Completed(None) => break,
                Controlled::Interrupted => {
                    interrupted = true;
                    break;
                }
                Controlled::Failed(cause) => {
                    failure = Some(cause);
                    break;
                }
            };
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

        if failure.is_none() && (usage.is_some() || provider_cost.is_some()) {
            let observed = usage.unwrap_or(Usage::IDENTITY);
            let cost_delta = settled_cost(
                &profile.metering,
                quote,
                usage.map_or(UsageReport::Missing, UsageReport::Reported),
                provider_cost,
                self.gate
                    .snapshot()
                    .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?
                    .currency(),
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
                failure = Some(cause.into());
            }
        }

        let settlement = match self.gate.settle(stream) {
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

        if interrupted {
            return Ok(JudgeRun::Ended(ExitReason::Cancelled(
                self.cancel.cause().unwrap_or(CancelCause::User),
            )));
        }

        let outcome = decoder.finish();
        let by = Authority::Judge {
            model: config.identity,
            attempt: reference,
        };
        Ok(JudgeRun::Decided { outcome, by })
    }

    /// Authorise and run one call prepared from its captured catalogue.
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
        call: &mut PendingCall,
    ) -> Result<ToolOutcome, StoreError> {
        let answered = |ok, text: String| ToolOutcome::Answered {
            ok,
            result: ToolResult::builtin(text),
        };
        let selected = match call
            .selected
            .take()
            .expect("a pending call is settled at most once")
        {
            Ok(selected) => selected,
            // Not a permission event. `prepare` refuses calls that never
            // reached the engine — an unknown tool, unparseable arguments —
            // and appending a decision here would record a fact the policy
            // never established.
            Err(refusal) => return Ok(answered(false, refusal)),
        };
        let prepared = match self.tools.prepare_selected(selected, &call.args) {
            Ok(prepared) => prepared,
            Err(refusal) => return Ok(answered(false, refusal)),
        };
        let (tool, mut prepared) = match prepared {
            PreparedToolCall::Effect { tool, call } => (tool, call),
            PreparedToolCall::HumanInput(request) => {
                return self.run_question(turn, call, request).await;
            }
            PreparedToolCall::Skill {
                operation,
                catalogue,
            } => {
                if let crate::skill::SkillOperation::ReadResource { invocation, path } = &operation
                {
                    let admitted = match self.active_skill_invocations.get(invocation).cloned() {
                        Some(admitted) => admitted,
                        None => {
                            return Ok(answered(
                                false,
                                "skill resource invocation is not live in this turn".to_owned(),
                            ));
                        }
                    };
                    let (logical, target, prepared) =
                        match self.tools.prepare_skill_resource(&admitted, path) {
                            Ok(prepared) => prepared,
                            Err(error) => return Ok(answered(false, error)),
                        };
                    let permission_selector = prepared.tool.clone();
                    let authorized = match self.tools.authorize(prepared) {
                        Classification::Settled(Ok(authorized)) => {
                            self.decided(
                                turn,
                                call,
                                &permission_selector,
                                true,
                                authorized.by().clone(),
                            )
                            .await?;
                            authorized
                        }
                        Classification::Settled(Err(denial)) => {
                            self.decided(
                                turn,
                                call,
                                &permission_selector,
                                false,
                                denial.authority(),
                            )
                            .await?;
                            return Ok(ToolOutcome::Refused { denial });
                        }
                        Classification::Ask(pending) => match self.tools.settle(pending).await {
                            Resolution::Settled(Ok(authorized)) => {
                                self.decided(
                                    turn,
                                    call,
                                    &permission_selector,
                                    true,
                                    authorized.by().clone(),
                                )
                                .await?;
                                authorized
                            }
                            Resolution::Settled(Err(denial)) => {
                                self.decided(
                                    turn,
                                    call,
                                    &permission_selector,
                                    false,
                                    denial.authority(),
                                )
                                .await?;
                                return Ok(ToolOutcome::Refused { denial });
                            }
                            Resolution::Ask(pending) => {
                                if !self.requested(turn, call, pending.request()).await? {
                                    return Ok(answered(
                                        false,
                                        "blocked by PermissionRequest hook".to_owned(),
                                    ));
                                }
                                return Ok(ToolOutcome::Unanswerable { pending });
                            }
                            Resolution::HumanSettled { .. } | Resolution::Reclassify { .. } => {
                                unreachable!("mode settlement cannot produce a human resolution")
                            }
                        },
                    };
                    let bytes = match self.tools.read_skill_resource(authorized, &target) {
                        Ok(bytes) => bytes,
                        Err(error) => return Ok(answered(false, error)),
                    };
                    if let Err(error) = admitted.record_resource(
                        logical,
                        &bytes,
                        self.tools.blob_store(),
                        &self.journal,
                        turn,
                    ) {
                        return Ok(answered(false, error.to_string()));
                    }
                    return Ok(answered(true, String::from_utf8_lossy(&bytes).into_owned()));
                }
                let mut next_scope = self.active_skill_scope.clone().unwrap_or_else(|| {
                    crate::skill::ActiveSkillScope::new(self.tools.effect_names())
                });
                if let Err(error) = crate::skill::SkillInvocation::preflight_scope(
                    &catalogue,
                    &operation,
                    &mut next_scope,
                ) {
                    return Ok(answered(false, error.to_string()));
                }
                let skill_selector = operation.name().to_owned();
                let invocation = crate::skill::SkillInvocation::admit_in_turn(
                    &catalogue,
                    operation,
                    self.tools.blob_store(),
                    &self.journal,
                    turn,
                );
                let invocation = match invocation {
                    Ok(invocation) => invocation,
                    Err(error) => return Ok(answered(false, error.to_string())),
                };
                self.active_skill_scope = Some(next_scope);
                self.active_skill_invocations
                    .insert(invocation.id(), invocation.clone());
                if let Err(error) = self.refresh_skill_hook_catalogue(turn) {
                    return Ok(answered(false, error.to_string()));
                }
                let causality = HookCausality::item(call.item, turn, call.started);
                let _ = self
                    .dispatch_hook(
                        causality.clone(),
                        HookEvent::tool(
                            HookLifecycle::InstructionsLoaded,
                            HookSubject::Item { item: call.item },
                            skill_selector.clone(),
                        ),
                    )
                    .await;
                if !self
                    .dispatch_hook(
                        causality,
                        HookEvent::tool(
                            HookLifecycle::UserPromptExpansion,
                            HookSubject::Item { item: call.item },
                            skill_selector,
                        ),
                    )
                    .await
                {
                    return Ok(answered(
                        false,
                        "blocked by UserPromptExpansion hook".to_owned(),
                    ));
                }
                return Ok(match invocation.rendered(self.tools.blob_store()) {
                    Ok(bytes) => answered(true, String::from_utf8_lossy(&bytes).into_owned()),
                    Err(error) => answered(false, error.to_string()),
                });
            }
        };
        if !self
            .dispatch_hook(
                HookCausality::item(call.item, turn, call.started),
                HookEvent::tool(
                    HookLifecycle::PreToolUse,
                    HookSubject::Item { item: call.item },
                    prepared.tool.clone(),
                ),
            )
            .await
        {
            return Ok(answered(false, "blocked by PreToolUse hook".to_owned()));
        }
        let hook_selector = prepared.tool.clone();
        let authorized = loop {
            match self.tools.authorize(prepared) {
                Classification::Settled(Ok(authorized)) => {
                    self.decided(turn, call, &hook_selector, true, authorized.by().clone())
                        .await?;
                    break authorized;
                }
                Classification::Settled(Err(denial)) => {
                    self.decided(turn, call, &hook_selector, false, denial.authority())
                        .await?;
                    return Ok(ToolOutcome::Refused { denial });
                }
                Classification::Ask(pending) => {
                    let resolution = await_controlled(
                        &self.journal,
                        &self.cancel,
                        &mut self.control,
                        turn,
                        self.tools.settle(pending),
                        InterruptDisposition::Return,
                    )
                    .await;
                    let resolution = match resolution {
                        Controlled::Completed(resolution) => resolution,
                        Controlled::Interrupted => {
                            self.decided(turn, call, &hook_selector, false, Authority::Party)
                                .await?;
                            return Ok(answered(false, "interrupted: permission denied".into()));
                        }
                        Controlled::Failed(cause) => return Err(cause),
                    };
                    match resolution {
                        Resolution::Settled(Ok(authorized)) => {
                            self.decided(turn, call, &hook_selector, true, authorized.by().clone())
                                .await?;
                            break authorized;
                        }
                        Resolution::Settled(Err(denial)) => {
                            self.decided(turn, call, &hook_selector, false, denial.authority())
                                .await?;
                            return Ok(ToolOutcome::Refused { denial });
                        }
                        Resolution::Ask(pending) => {
                            if !self.requested(turn, call, pending.request()).await? {
                                return Ok(answered(
                                    false,
                                    "blocked by PermissionRequest hook".to_owned(),
                                ));
                            }
                            let pending = match self.judge_pending(turn, &pending).await? {
                                JudgeRun::Unavailable => pending,
                                JudgeRun::Ended(ExitReason::Cancelled(_)) => {
                                    self.decided(
                                        turn,
                                        call,
                                        &hook_selector,
                                        false,
                                        Authority::Party,
                                    )
                                    .await?;
                                    return Ok(answered(
                                        false,
                                        "interrupted: permission denied".into(),
                                    ));
                                }
                                JudgeRun::Ended(reason) => return Ok(ToolOutcome::Ended(reason)),
                                JudgeRun::Decided { outcome, by } => {
                                    self.judge_decided(
                                        turn,
                                        call,
                                        &hook_selector,
                                        &outcome,
                                        by.clone(),
                                    )
                                    .await?;
                                    match self.tools.settle_judge(pending, &outcome, by) {
                                        Resolution::Settled(Ok(authorized)) => break authorized,
                                        Resolution::Settled(Err(denial)) => {
                                            return Ok(ToolOutcome::Refused { denial });
                                        }
                                        Resolution::Ask(pending) => pending,
                                        Resolution::HumanSettled { .. }
                                        | Resolution::Reclassify { .. } => {
                                            unreachable!(
                                                "a judge cannot produce a human resolution"
                                            )
                                        }
                                    }
                                }
                            };
                            let decision = await_controlled(
                                &self.journal,
                                &self.cancel,
                                &mut self.control,
                                turn,
                                self.tools.decide(pending),
                                InterruptDisposition::Return,
                            )
                            .await;
                            let decision = match decision {
                                Controlled::Completed(decision) => decision,
                                Controlled::Interrupted => {
                                    self.decided(
                                        turn,
                                        call,
                                        &hook_selector,
                                        false,
                                        Authority::Party,
                                    )
                                    .await?;
                                    return Ok(answered(
                                        false,
                                        "interrupted: permission denied".into(),
                                    ));
                                }
                                Controlled::Failed(cause) => return Err(cause),
                            };
                            match decision {
                                Resolution::Ask(pending) => {
                                    return Ok(ToolOutcome::Unanswerable { pending });
                                }
                                Resolution::HumanSettled { result, .. } => match result {
                                    Ok(authorized) => {
                                        self.decided(
                                            turn,
                                            call,
                                            &hook_selector,
                                            true,
                                            authorized.by().clone(),
                                        )
                                        .await?;
                                        break authorized;
                                    }
                                    Err(denial) => {
                                        self.decided(
                                            turn,
                                            call,
                                            &hook_selector,
                                            false,
                                            denial.authority(),
                                        )
                                        .await?;
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
                                    prepared =
                                        match self.tools.prepare_amendment(tool.as_ref(), params) {
                                            Ok(prepared) => prepared,
                                            Err(refusal) => return Ok(answered(false, refusal)),
                                        };
                                }
                                Resolution::Settled(result) => match result {
                                    Ok(authorized) => {
                                        self.decided(
                                            turn,
                                            call,
                                            &hook_selector,
                                            true,
                                            authorized.by().clone(),
                                        )
                                        .await?;
                                        break authorized;
                                    }
                                    Err(denial) => {
                                        self.decided(
                                            turn,
                                            call,
                                            &hook_selector,
                                            false,
                                            denial.authority(),
                                        )
                                        .await?;
                                        return Ok(ToolOutcome::Refused { denial });
                                    }
                                },
                            }
                        }
                        Resolution::HumanSettled { result, .. } => match result {
                            Ok(authorized) => {
                                self.decided(
                                    turn,
                                    call,
                                    &hook_selector,
                                    true,
                                    authorized.by().clone(),
                                )
                                .await?;
                                break authorized;
                            }
                            Err(denial) => {
                                self.decided(turn, call, &hook_selector, false, denial.authority())
                                    .await?;
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
                            prepared = match self.tools.prepare_amendment(tool.as_ref(), params) {
                                Ok(prepared) => prepared,
                                Err(refusal) => return Ok(answered(false, refusal)),
                            };
                        }
                    }
                }
            }
        };
        let resume_context = call.capture_authority.resume_context(
            self.hook_runtime.as_ref(),
            HookCausality::item(call.item, turn, call.started),
        );
        let execution = await_controlled(
            &self.journal,
            &self.cancel,
            &mut self.control,
            turn,
            self.tools
                .execute(tool.as_ref(), authorized, resume_context),
            InterruptDisposition::AwaitEffect,
        )
        .await;
        let execution = match execution {
            Controlled::Completed(execution) => execution,
            Controlled::Interrupted => unreachable!("effect waits are never dropped"),
            Controlled::Failed(cause) => return Err(cause),
        };
        Ok(match execution {
            Ok(crate::mcp::ToolExecution::Complete(result)) => ToolOutcome::Executed {
                ok: true,
                result,
                selector: hook_selector,
            },
            Ok(crate::mcp::ToolExecution::InputRequired(required)) => {
                return self
                    .run_foreign_question(turn, call.item, call.started, required)
                    .await;
            }
            Err(failure) => ToolOutcome::Executed {
                ok: false,
                result: failure.into_failed_result(),
                selector: hook_selector,
            },
        })
    }

    async fn run_question(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
        request: crate::question::QuestionRequest,
    ) -> Result<ToolOutcome, StoreError> {
        let question = ItemId::new();
        let request_event = self.journal.append(
            turn,
            Some(question),
            Some(call.started),
            EventKind::QuestionRequested {
                request: request.clone(),
                target: QuestionTarget::ModelTool {
                    tool_call: call.item,
                },
            },
        )?;

        let outcome = match self.elicitor.as_ref().map(Arc::clone) {
            Some(elicitor) => {
                let elicited = await_controlled(
                    &self.journal,
                    &self.cancel,
                    &mut self.control,
                    turn,
                    elicitor.elicit(question, request.clone()),
                    InterruptDisposition::Return,
                )
                .await;
                match elicited {
                    Controlled::Completed(Ok(outcome)) => outcome,
                    Controlled::Completed(Err(error)) => {
                        tracing::error!(%error, %question, "question elicitation failed");
                        self.journal.append(
                            turn,
                            None,
                            Some(request_event),
                            EventKind::Error {
                                message: error.to_string(),
                                kind: ErrorKind::Unclassified,
                            },
                        )?;
                        return Ok(ToolOutcome::Ended(ExitReason::Internal));
                    }
                    Controlled::Interrupted => {
                        return Ok(ToolOutcome::Ended(ExitReason::Cancelled(
                            self.cancel.cause().unwrap_or(CancelCause::User),
                        )));
                    }
                    Controlled::Failed(cause) => return Err(cause),
                }
            }
            None => QuestionOutcome::Unavailable,
        };

        if outcome == QuestionOutcome::Unavailable {
            return Ok(ToolOutcome::QuestionRequired);
        }
        if let QuestionOutcome::Answered(answer) = &outcome
            && let Err(error) = answer.validate_for(&request)
        {
            tracing::error!(%error, %question, "elicitor returned an invalid answer");
            self.journal.append(
                turn,
                None,
                Some(request_event),
                EventKind::Error {
                    message: error.to_string(),
                    kind: ErrorKind::Unclassified,
                },
            )?;
            return Ok(ToolOutcome::Ended(ExitReason::Internal));
        }

        let settlement = self.journal.append(
            turn,
            Some(question),
            Some(request_event),
            EventKind::QuestionSettled { outcome },
        )?;
        let continuation = self
            .state()
            .complete_settled_question(question)
            .expect("a just-persisted valid settlement has a pure continuation");
        self.journal.append(
            turn,
            Some(continuation.tool_call),
            Some(settlement),
            EventKind::ToolCallCompleted {
                ok: continuation.ok(),
                result: continuation.tool_result,
            },
        )?;
        Ok(ToolOutcome::Recorded)
    }

    async fn run_foreign_question(
        &mut self,
        turn: TurnId,
        tool_call: ItemId,
        parent: EventId,
        required: crate::mcp::ForeignInputRequired,
    ) -> Result<ToolOutcome, StoreError> {
        let mut parent = parent;
        let mut required = required;
        loop {
            let question = ItemId::new();
            let request = required.question_request().clone();
            let continuation_id = required.continuation_id().to_owned();
            let request_event = self.journal.append(
                turn,
                Some(question),
                Some(parent),
                EventKind::QuestionRequested {
                    request: request.clone(),
                    target: QuestionTarget::ForeignContinuation {
                        tool_call,
                        continuation_id: continuation_id.clone(),
                    },
                },
            )?;

            let Some(elicitor) = self.elicitor.as_ref().map(Arc::clone) else {
                return Ok(ToolOutcome::QuestionRequired);
            };
            let Some(port) = self.foreign_question_port.as_ref().map(Arc::clone) else {
                return Ok(ToolOutcome::QuestionRequired);
            };
            let elicited = await_controlled(
                &self.journal,
                &self.cancel,
                &mut self.control,
                turn,
                elicitor.elicit(question, request.clone()),
                InterruptDisposition::Return,
            )
            .await;
            let outcome = match elicited {
                Controlled::Completed(Ok(outcome)) => outcome,
                Controlled::Completed(Err(error)) => {
                    tracing::error!(%error, %question, "foreign question elicitation failed");
                    self.journal.append(
                        turn,
                        None,
                        Some(request_event),
                        EventKind::Error {
                            message: error.to_string(),
                            kind: ErrorKind::Unclassified,
                        },
                    )?;
                    return Ok(ToolOutcome::Ended(ExitReason::Internal));
                }
                Controlled::Interrupted => {
                    return Ok(ToolOutcome::Ended(ExitReason::Cancelled(
                        self.cancel.cause().unwrap_or(CancelCause::User),
                    )));
                }
                Controlled::Failed(cause) => return Err(cause),
            };
            if outcome == QuestionOutcome::Unavailable {
                return Ok(ToolOutcome::QuestionRequired);
            }
            let answer = match outcome {
                QuestionOutcome::Answered(answer) => answer,
                outcome => {
                    let exit_reason = if outcome == QuestionOutcome::Cancelled {
                        ExitReason::Cancelled(CancelCause::User)
                    } else {
                        ExitReason::PermissionRequired
                    };
                    self.journal.append(
                        turn,
                        Some(question),
                        Some(request_event),
                        EventKind::QuestionSettled { outcome },
                    )?;
                    return Ok(ToolOutcome::Ended(exit_reason));
                }
            };
            if let Err(error) = answer.validate_for(&request) {
                tracing::error!(%error, %question, "elicitor returned an invalid foreign answer");
                self.journal.append(
                    turn,
                    None,
                    Some(request_event),
                    EventKind::Error {
                        message: error.to_string(),
                        kind: ErrorKind::Unclassified,
                    },
                )?;
                return Ok(ToolOutcome::Ended(ExitReason::Internal));
            }
            match self
                .settle_foreign_question(
                    ForeignQuestionSettlement {
                        turn,
                        question,
                        request_event,
                        tool_call,
                        continuation_id,
                    },
                    answer,
                    port,
                )
                .await?
            {
                ForeignQuestionProgress::Completed => return Ok(ToolOutcome::Recorded),
                ForeignQuestionProgress::Successor {
                    required: successor,
                    parent: settlement,
                } => {
                    required = successor;
                    parent = settlement;
                }
            }
        }
    }

    async fn settle_foreign_question(
        &mut self,
        settlement: ForeignQuestionSettlement,
        answer: QuestionAnswer,
        port: Arc<dyn crate::mcp::ForeignQuestionPort + 'turn>,
    ) -> Result<ForeignQuestionProgress, StoreError> {
        let ForeignQuestionSettlement {
            turn,
            question,
            request_event,
            tool_call,
            continuation_id,
        } = settlement;
        let canonical_json = answer.model_text();
        let settlement = self.journal.append(
            turn,
            Some(question),
            Some(request_event),
            EventKind::QuestionSettled {
                outcome: QuestionOutcome::Answered(answer),
            },
        )?;
        let causality =
            HookCausality::continuation(continuation_id.clone(), turn, tool_call, settlement)
                .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?;
        let execution = await_controlled(
            &self.journal,
            &self.cancel,
            &mut self.control,
            turn,
            port.resume(&continuation_id, &canonical_json, causality),
            InterruptDisposition::AwaitEffect,
        )
        .await;
        let execution = match execution {
            Controlled::Completed(Ok(execution)) => execution,
            Controlled::Completed(Err(error)) => {
                return Err(StoreError::Io(std::io::Error::other(error)));
            }
            Controlled::Interrupted => unreachable!("foreign effect waits are never dropped"),
            Controlled::Failed(cause) => return Err(cause),
        };
        match execution {
            crate::mcp::ToolExecution::Complete(result) => {
                self.journal.append(
                    turn,
                    Some(tool_call),
                    Some(settlement),
                    EventKind::ToolCallCompleted { ok: true, result },
                )?;
                Ok(ForeignQuestionProgress::Completed)
            }
            crate::mcp::ToolExecution::InputRequired(required) => {
                Ok(ForeignQuestionProgress::Successor {
                    required,
                    parent: settlement,
                })
            }
        }
    }

    /// Record the canonical question that was actually presented.
    async fn requested(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
        request: &crate::permit::PermissionRequest,
    ) -> Result<bool, StoreError> {
        let request_event = self.permission(
            turn,
            call,
            EventKind::PermissionRequested {
                tool: request.tool().to_string(),
                escalation: request.why().clone(),
            },
        )?;
        Ok(self
            .dispatch_hook(
                HookCausality::event(request_event, turn, None),
                HookEvent::tool(
                    HookLifecycle::PermissionRequest,
                    HookSubject::Turn { turn },
                    request.tool(),
                ),
            )
            .await)
    }

    /// §7: *every decision is appended, asked or not.*
    ///
    /// So this fires for a call the rules waved through as well as for one
    /// they refused. `PermissionRequested` is appended when mode settlement
    /// raises the canonical question, before either a human answer or the
    /// headless unanswerable result.
    async fn decided(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
        selector: &str,
        allowed: bool,
        by: Authority,
    ) -> Result<(), StoreError> {
        let decision = self.permission(
            turn,
            call,
            EventKind::PermissionDecided {
                allowed: Some(allowed),
                by: Some(by),
                reason: None,
            },
        )?;
        if !allowed {
            let _ = self
                .dispatch_hook(
                    HookCausality::event(decision, turn, None),
                    HookEvent::tool(
                        HookLifecycle::PermissionDenied,
                        HookSubject::Turn { turn },
                        selector,
                    ),
                )
                .await;
        }
        Ok(())
    }

    async fn judge_decided(
        &mut self,
        turn: TurnId,
        call: &PendingCall,
        selector: &str,
        outcome: &JudgeOutcome,
        by: Authority,
    ) -> Result<(), StoreError> {
        let allowed = match outcome {
            JudgeOutcome::Allow { .. } => Some(true),
            JudgeOutcome::Deny { .. } => Some(false),
            JudgeOutcome::Escalate { .. } => None,
        };
        let decision = self.permission(
            turn,
            call,
            EventKind::PermissionDecided {
                allowed,
                by: Some(by),
                reason: Some(outcome.reason().to_owned()),
            },
        )?;
        if allowed == Some(false) {
            let _ = self
                .dispatch_hook(
                    HookCausality::event(decision, turn, None),
                    HookEvent::tool(
                        HookLifecycle::PermissionDenied,
                        HookSubject::Turn { turn },
                        selector,
                    ),
                )
                .await;
        }
        Ok(())
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
    ) -> Result<EventId, StoreError> {
        self.journal
            .append(turn, Some(ItemId::new()), Some(call.started), kind)
            .map_err(Into::into)
    }
}

/// What running one tool call produced.
enum ToolOutcome {
    /// It did not reach a guarded effect, and the model should read why.
    Answered { ok: bool, result: ToolResult },
    /// A guarded effect settled and owns a post-effect lifecycle point.
    Executed {
        ok: bool,
        result: ToolResult,
        selector: String,
    },
    /// The pure human-input result was already journalled with its settlement
    /// as causal parent.
    Recorded,
    /// The durable request remains pending and the turn is suspended.
    QuestionRequired,
    /// Policy refused it. A settled answer, and still not one for the model:
    /// telling it "denied" invites it to try a variant, which is a prompt loop
    /// run against a boundary.
    Refused { denial: PolicyDenied },
    /// Policy could not settle it and there is nobody here to ask.
    Unanswerable { pending: Pending },
    /// Judge admission failed for a cause that ends the whole turn.
    Ended(ExitReason),
}

enum ForeignQuestionProgress {
    Completed,
    Successor {
        required: crate::mcp::ForeignInputRequired,
        parent: EventId,
    },
}

struct ForeignQuestionSettlement {
    turn: TurnId,
    question: ItemId,
    request_event: EventId,
    tool_call: ItemId,
    continuation_id: String,
}

fn skill_store_error(error: crate::skill::SkillError) -> StoreError {
    StoreError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        error.to_string(),
    ))
}

fn hook_store_error(error: crate::hook::HookRunError) -> StoreError {
    StoreError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        error.to_string(),
    ))
}

enum DriveOutcome {
    Ended(ExitReason, Option<Denial>),
    QuestionRequired,
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
    /// Sampling was interrupted after any calls already observed were
    /// journalled. None has run, so the caller must close all of them.
    Cancelled {
        cause: CancelCause,
        pending: Vec<PendingCall>,
    },
    /// The generation finished. `pending` is what it asked to call.
    Stopped {
        generation: GenerationId,
        terminal: EventId,
        reason: StopReason,
        pending: Vec<PendingCall>,
        /// How the provider classified its failure, when `reason` is
        /// [`StopReason::Error`]. Meaningless otherwise, and not read.
        error_kind: Option<ErrorKind>,
    },
}

/// A drained stream, handed back so it can be settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GenerationVisibility {
    Conversation,
    CompactionPrivate,
}

struct DrainGeneration<'a> {
    event: EventId,
    metering: &'a Metering,
    quote: Option<&'a PricingQuote>,
    visibility: GenerationVisibility,
    catalogue: Option<&'a GenerationToolCatalogue>,
}

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
    /// Text retained for a private generation and never projected as an item.
    private_text: String,
    /// Whether a private, tool-less generation nevertheless emitted a call.
    private_tool_use: bool,
    /// Whether this drain appended at least one charge for the attempt.
    accounting_booked: bool,
}
