//! `DomainEvent`: the exhaustive internal reducer input.
//!
//! See `ARCHITECTURE.md` §1 for the variant set and the envelope fields.

use serde::{Deserialize, Serialize};

use super::hook::{
    HookCatalogueEvidence, HookId, HookInvocationId, HookLifecycle, HookOccurrenceKey, HookPlan,
    HookResult, HookSubject,
};
use super::id::{
    DetachedExecId, EventId, GoalId, ItemId, Producer, QueuedPromptId, Seq, SessionId,
    SubagentInvocationId, TurnId,
};
use super::skill::{
    SkillCatalogueEvidence, SkillCatalogueGeneration, SkillContentDigest, SkillId,
    SkillInvocationId, SkillSourceAuthority, SkillSourceLocation,
};
use super::time::Timestamp;
use crate::cost::CostDelta;
use crate::generator::attempt::{AttemptRef, Outcome};
use crate::generator::capabilities::CapabilityGap;
use crate::generator::types::{ErrorKind, ImageRef, ReasoningState};
use crate::goal::{GoalOwner, GoalTransition};
use crate::permit::residual::{ResidualOutcome, ResidualScope};
use crate::permit::{
    Authority, CanonPath, DetachedExecSnapshot, Escalation, ExecObservation, ToolResult,
    ToolResultProvenance,
};
use crate::question::{QuestionOutcome, QuestionRequest, QuestionTarget};
use crate::run::{CancelReason, DeniedBy, RequestId, RunId, RunSelections, RunSettlement};
use crate::state::Usage;
use crate::store::blob::{BlobId, BlobRef};

/// The full envelope every `DomainEvent` carries, per ARCH §1: a monotonic
/// sequence number, stable session/turn/item identity, a causal parent, an
/// observation timestamp and a producer identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Position in the total order. Authoritative for ordering — see
    /// `Timestamp`'s doc comment for why wall time is not.
    pub seq: Seq,
    /// This event's own identity.
    pub id: EventId,
    /// The turn this event belongs to.
    pub turn: TurnId,
    /// The item (message, reasoning block, tool call, …) this event belongs
    /// to, if any. Some event kinds (e.g. `TurnCompleted`) carry none.
    pub item: Option<ItemId>,
    /// The event this one is causally downstream of, if any.
    pub parent: Option<EventId>,
    /// When this event was observed.
    pub ts: Timestamp,
    /// What produced this event.
    pub producer: Producer,
}

/// Which generation lifecycle opened an attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationRole {
    #[default]
    Main,
    Judge,
    GoalCheck,
    Compaction,
    Subagent,
    Hook,
}

/// Why one ordered fallback candidate was changed or skipped before admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum FallbackDecision {
    /// Opaque provider reasoning was the only incompatibility and was removed.
    ReasoningDropped {
        dropped_blocks: usize,
        dialects: Vec<String>,
    },
    /// Capability facts prove that the candidate cannot accept the request.
    Skipped { gaps: Vec<CapabilityGap> },
    /// The adapter cannot encode the otherwise-capable request.
    ExpressibilitySkipped { message: String },
    /// Current model authority could not be resolved for this candidate.
    ResolutionSkipped { message: String },
}

/// Closed refusal evidence for a child that never reached admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubagentRefusal {
    InvalidTask { diagnostic: String },
    ProfileUnavailable { diagnostic: String },
    AuthorityWouldWiden { diagnostic: String },
    PreparationFailed { diagnostic: String },
    AdmissionFailed { diagnostic: String },
}

/// One final parent-visible child settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubagentTerminal {
    Returned {
        #[serde(with = "m4_blob_ref")]
        final_message: BlobRef,
    },
    Refused {
        failure: SubagentRefusal,
    },
    Failed {
        #[serde(with = "subagent_failed_settlement")]
        settlement: RunSettlement,
    },
    ReturnBlocked {
        #[serde(with = "m4_blob_ref")]
        final_message: BlobRef,
        hook_invocation: HookInvocationId,
    },
}

/// Who stopped a turn that did not finish on its own.
///
/// Carried by [`ExitReason::Cancelled`] rather than left to the run level,
/// because a timeout that interrupts a turn has to be durable: without this,
/// the log and `PublicEvent::TurnCompleted` would say `Cancelled` while the
/// CLI printed `timeout`, and replay, JSONL and ACP would give three accounts
/// of one fact. A limit applied *between* turns is not here — that is the run
/// level's, where it is true. See
/// `ARCHITECTURE.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelCause {
    /// A human asked for it — `SIGINT`, or an interactive interrupt.
    User,
    /// A deadline expired while the turn was running.
    Timeout,
    /// The embedding host withdrew the turn (an ACP client cancelling a
    /// prompt, a supervisor shutting down).
    Host,
}

/// How a turn ended. Deliberately one field (paired with `ok` on
/// `TurnCompleted`), never a separate error field alongside it (ARCH §1
/// review note: "one error field, never two").
///
/// The variants are the *turn's* vocabulary. Why an invocation ended is
/// [`RunTermination`](crate::run::RunTermination), which is a different
/// question with a different lifetime: `max_turns` fires between two
/// completed turns, and is not honestly a cause of the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// The turn ran to completion.
    Completed,
    /// The turn was stopped before it finished, by whom it says.
    Cancelled(CancelCause),
    /// The turn stopped at a safe boundary because a host asked it to, and
    /// can be continued.
    ///
    /// Top-level and deliberately not a [`CancelCause`]. `CancelCause`
    /// answers *who* stopped the turn, and every one of its values maps to
    /// ACP's `StopReason::Cancelled`; a host told its resumable hold was
    /// "cancelled" throws away the turn state it was holding open, which is
    /// the opposite of what it asked for.
    /// `docs/decisions/2026-09-03-acp-host-control-is-negotiated-per-\
    /// session.md` rejects "pause as cancel plus a later prompt" by name.
    Paused,
    /// The turn was stopped by a budget limit.
    Budget,
    /// The run was asked for something it cannot do, and no retry will change
    /// that: `CLI.md` §4.3's exit 2. Distinct from [`Provider`] because
    /// nothing was sent, and from [`Internal`] because nothing of ours
    /// failed — the invocation and the resolved model disagree, and the
    /// caller is the only party who can settle it. See
    /// `ARCHITECTURE.md`.
    ///
    /// [`Provider`]: ExitReason::Provider
    /// [`Internal`]: ExitReason::Internal
    Usage,
    /// A call needed a grant that nobody in this run could give. `CLI.md`
    /// §4.4: a headless run never hangs on a prompt, so an unresolvable
    /// permission ends it rather than being answered on the model's behalf.
    PermissionRequired,
    /// The provider explicitly reported that the model context window was
    /// exhausted. This is settled and never an automatic retry signal.
    ContextOverflow,
    /// The provider or the network failed. Distinct from [`Internal`] because
    /// `CLI.md` §4.3 makes them different exits — a script retries 4 and does
    /// not retry 1.
    ///
    /// [`Internal`]: ExitReason::Internal
    Provider,
    /// **We** failed. Never reported as a provider error: telling a script to
    /// retry our own defect is worse than telling it nothing.
    Internal,
}

/// Where a context-occupancy figure came from, and therefore how much it can
/// be trusted.
///
/// Occupancy is not derivable from cumulative token totals — prior history is
/// counted again as input on every call, so the totals diverge from
/// occupancy from the second generation onward. It has to be *reported*, and
/// the reports differ in quality, so the source travels with the number
/// instead of being lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccupancyProvenance {
    /// Summed from what the provider reported for the generation that just
    /// finished. The industry-standard proxy (it is what the reference
    /// `claude-agent-acp` adapter uses, and its own source calls it a proxy):
    /// accurate at the instant it is observed, and stale the moment anything
    /// changes the history.
    GenerationBoundary,
    /// The provider was asked outright what the context currently holds.
    /// The only figure that is correct straight after a compaction, because
    /// per-message usage cannot describe a history that was rewritten
    /// between generations.
    AuthoritativeQuery,
    /// Counted locally from the prompt the harness assembled. Available when
    /// no provider figure is — after a context edit, say — and no better
    /// than the local tokenizer's agreement with the provider's.
    LocalEstimate,
}

/// Monotonic durations measured by the tool owner, never inferred from wall time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTiming {
    pub execution_ms: Option<u64>,
    pub approval_wait_ms: u64,
}

/// The exhaustive set of internal reducer inputs, per ARCH §1:
///
/// ```text
/// SessionTitleChanged | SessionDeleted | HostContextAppended | SessionPauseRequested |
/// SessionPaused | SessionPauseObserved | SessionResumed | GoalSet |
/// ContextRequested | ContextRequestConsumed | ContextCooldown |
/// GoalAllowanceInherited | GoalTransitioned | GoalCheckConfigured | GoalContinuation | GoalUpdated | GoalCheckStopped | PromptQueued | PromptDeliveryExpedited |
/// PromptRetracted | UserPrompt | AssistantDelta | AssistantMessage |
/// ReasoningDelta | ReasoningState | ProviderContent | ContextCommandStarted | ToolCallStarted | ToolCallDelta |
/// ToolCallCompleted | ToolCallMeasured | DetachedExecStarted |
/// DetachedExecSettled | QuestionRequested | QuestionSettled | QuestionMemberSettled |
/// PermissionRequested | PermissionAmended | PermissionDecided |
/// GenerationStarted | FallbackDecision | ExecutionObserved |
/// ExecutionMeasured | JudgeResponse | SandboxResidualDecided |
/// AttemptSettled | UsageUpdated | UsageCorrected | ContextWindowUpdated |
/// TurnCompleted | CompactionSeeded | Compacted | Forked |
/// HookCatalogueAdmitted | HookLifecycleReached | HookInvocationStarted |
/// HookInvocationSettled | SubagentRequested | SubagentSpawned |
/// SubagentSettled | SkillCatalogueAdmitted | SkillInstructionAdmitted |
/// SkillResourceAdmitted | Error
/// ```
///
/// This is the internal representation: it evolves freely with core, and
/// `PublicEvent` is the privacy-filtered, compatibility-governed projection
/// of it. "Freely" has one hard limit, though — `SessionStore::append`
/// serialises this type verbatim as the `StoreRecord` payload, so its serde
/// shape *is* the durable format and a later build has to read what an
/// earlier one wrote. `tests/codec.rs`'s committed `golden/store_log.jsonl`
/// pins those bytes.
///
/// What is additive, precisely: a new variant, or a new field **that decodes
/// when it is absent** (an `Option`, or `#[serde(default)]` on a type that
/// has one). A required field is not — a record written before it existed
/// has no value for it and fails to decode, which is the same break as a
/// rename. Renaming or re-tagging an existing variant or field is a
/// `CODEC_VERSION` bump plus a migration, not a fixture update. (This note
/// used to say "adding a variant or a field is fine", which was overbroad;
/// corrected in `ARCHITECTURE.md`.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum EventKind {
    /// Operator-facing metadata. The session identity and lineage do not change.
    SessionTitleChanged { title: Option<String> },
    /// Durable soft deletion. Storage remains available to lineage replay.
    SessionDeleted,
    /// A host asked for this session's work to stop at its next safe
    /// boundary, in a way it can later continue.
    ///
    /// The *intent*, not the boundary: the request is recorded before the
    /// turn has stopped, because a pause that were only recorded once it had
    /// taken effect would be lost by a crash in the window between asking and
    /// stopping, and replay would then say the session is running and admit
    /// more effects. `docs/decisions/2026-09-03-acp-host-control-is-\
    /// negotiated-per-session.md` names that window: the response reports
    /// `pending` until [`SessionPaused`](Self::SessionPaused) exists.
    SessionPauseRequested { request: RequestId },
    /// The safe boundary the request asked for was reached: the indivisible
    /// effect that was in flight settled truthfully, and nothing new was
    /// admitted after it.
    ///
    /// `request` is the [`SessionPauseRequested`](Self::SessionPauseRequested)
    /// this boundary answers, so a host retrying the same request over a
    /// reconnected transport is told what its own request achieved rather
    /// than being served a boundary somebody else asked for.
    SessionPaused { request: RequestId },
    /// A second request was told about a hold it did not establish.
    ///
    /// It answers the question [`SessionPaused`](Self::SessionPaused) cannot:
    /// that boundary names the one request that asked for it, so a host told
    /// `paused` because the hold was *already* in force leaves no trace, and
    /// its retry after the release would hold the session again — the defect a
    /// receipt exists to prevent, reached by the one path that wrote nothing.
    /// A host cannot see whose request the boundary carries, so the word it is
    /// given has to be worth the same either way.
    ///
    /// Recorded only where the boundary exists. A request that finds a hold
    /// still walking to its boundary is told `pending`, and `pending` promises
    /// no durable record to replay.
    SessionPauseObserved { request: RequestId },
    /// A paused session was released to continue.
    ///
    /// Its own `request` and not the pause's: resume is idempotent by the id
    /// the host sent for *it*, and pairing them here would make a retried
    /// resume indistinguishable from a retried pause.
    SessionResumed { request: RequestId },
    /// A persistent goal became the session's current goal.
    GoalSet {
        id: GoalId,
        objective: String,
        owner: GoalOwner,
        ceilings: crate::ceiling::Ceilings,
        derived_from: Option<GoalId>,
    },
    /// The goal continuation allowance admitted for this goal identity.
    GoalCheckConfigured {
        id: GoalId,
        continuation_budget: u32,
    },
    /// Existing goal-check accounting carried by an atomic successor seed.
    GoalAllowanceInherited {
        id: GoalId,
        continuation_budget: Option<u32>,
        continuation_turns: u32,
    },
    /// System-owned work admitted after an independent goal check.
    GoalContinuation { id: GoalId, text: String },
    /// Model-authored request, serviced only by the idle session owner.
    ContextRequested {
        request: crate::context::ContextRequest,
    },
    /// Cross-session cooldown carried by the atomic target seed.
    ContextCooldown { turns: u32 },
    /// The idle owner consumed a model request; never retry it implicitly.
    ContextRequestConsumed,
    /// A check could not continue safely or exhausted its allowance.
    GoalCheckStopped { id: GoalId, reason: String },
    /// The model updated its own objective.
    GoalUpdated { id: GoalId, objective: String },
    /// One observation was made against the current goal.
    GoalTransitioned {
        id: GoalId,
        transition: GoalTransition,
    },
    /// Input accepted for later delivery. Its envelope turn determines on
    /// replay whether it was accepted as a busy-turn steer or at an ordinary
    /// queue boundary.
    PromptQueued {
        id: QueuedPromptId,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
    /// A durable request to expedite the FIFO prefix ending at `through`.
    PromptDeliveryExpedited { through: QueuedPromptId },
    /// Withdraw the last unconsumed queued prompt before restoring its composer.
    PromptRetracted { id: QueuedPromptId },
    /// The user submitted a prompt.
    UserPrompt {
        text: String,
        /// Ordered immutable image metadata; bytes live in the session CAS.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
        /// The queue acceptance this conversation item consumes. Absent on
        /// ordinary direct prompts and on records written before queues.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queued_prompt: Option<QueuedPromptId>,
    },
    /// A streamed increment of the assistant's message.
    AssistantDelta { text: String },
    /// The assistant's final, assembled message.
    AssistantMessage { text: String },
    /// A streamed increment of the model's reasoning/thinking.
    ReasoningDelta { text: String },
    /// One reasoning block's opaque continuation state, complete.
    ///
    /// The *close* of the block the preceding `ReasoningDelta`s opened, so it
    /// is a boundary as well as a payload: the next delta after this opens a
    /// new block. Journalled rather than derived, because the state is
    /// positional — Anthropic documents a `400` when blocks are not replayed
    /// exactly, including blocks whose `thinking` field is empty — and a
    /// resumed session that reconstructed the order by guesswork would be
    /// reconstructing the one thing the provider checks.
    ReasoningState { state: ReasoningState },
    /// Private provider output required for exact continuation.
    ProviderContent {
        state: ReasoningState,
        visible: bool,
    },
    /// An explicit user MCP context selection, not a provider-issued tool call.
    ContextCommandStarted { name: String, args: String },
    /// A tool call began, with its name and raw (unvalidated) arguments.
    ToolCallStarted {
        name: String,
        args: String,
        /// The provider's own id for the call, which
        /// [`AiEvent::ToolCall`](crate::generator::port::AiEvent::ToolCall)
        /// carries in. Without it a resumed session cannot build a history
        /// the provider will accept: the id is opaque and provider-issued,
        /// so nothing local can stand in for it.
        ///
        /// `Option` because a *required* field would break every record
        /// written before ids were kept — see this type's own doc comment.
        /// `None` means exactly that, and is not repairable: there is no rule
        /// that reconstructs an opaque id, so the rebuild truncates the
        /// history there rather than inventing one.
        ///
        /// The optionality is `Option`'s doing, not the attribute's: serde
        /// decodes a missing `Option` field as `None` on its own. `default`
        /// is written out anyway so the intent survives a future change of
        /// this field's type, when it would start carrying weight.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    /// A streamed increment of a tool call's output.
    ToolCallDelta { text: String },
    /// A tool call finished, with its outcome and final evidence. This is a
    /// codec-v6 shape: replacing the old string is deliberately not decoded
    /// as a compatibility spelling.
    ToolCallCompleted { ok: bool, result: ToolResult },
    /// Measured before completion so live display and replay read the same facts.
    ToolCallMeasured { timing: ToolTiming },
    /// The launch authority was admitted before the process starts.
    DetachedExecStarted { id: DetachedExecId },
    /// Ego-owned append-only provider context; never an operator prompt.
    HostContextAppended { text: String },
    /// One terminal observation, delivered to the model at a generation boundary.
    DetachedExecSettled { snapshot: DetachedExecSnapshot },
    /// A typed human-input request became durable before any host was asked.
    /// The envelope's item is the sole question identity; `target` names the
    /// closed consumer that can eventually settle the originating tool call.
    QuestionRequested {
        request: QuestionRequest,
        target: QuestionTarget,
    },
    /// The terminal human-input outcome, correlated by the same envelope item
    /// as [`QuestionRequested`](Self::QuestionRequested).
    QuestionSettled { outcome: QuestionOutcome },
    /// A correlated round member was completed; this does not authorize a retry.
    QuestionMemberSettled {
        key: String,
        answer: serde_json::Value,
    },
    /// A sensitive action needs a permission decision.
    ///
    /// Structured rather than a rendered `description`, because the question
    /// an audit asks of this log — *what did this run stop and ask about* —
    /// is a filter over the op and the path, and a sentence is not something
    /// you can filter. `Escalation` renders itself for anywhere a sentence is
    /// what is wanted, so nothing is lost by keeping the fields.
    PermissionRequested {
        /// The tool whose call raised the question. Not the whole
        /// `PreparedCall`: an `Op::Write` carries what will land — the whole
        /// file for a `write`, the replacement text for an `edit` — and a
        /// durable audit record is the last place file contents belong.
        tool: String,
        /// Which step asked, about which operation.
        escalation: Escalation,
    },
    /// A human replaced the proposed tool parameters before any permit was
    /// minted. Raw parameters are intentionally absent: they may contain file
    /// contents. The following prepare result or decision records what the
    /// replacement became.
    PermissionAmended { tool: String },
    /// A permission decision was made, and who made it.
    PermissionDecided {
        /// `None` is the judge's durable escalation, not a denial.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed: Option<bool>,
        /// The rules, the mode, the boundary or the party that decided —
        /// `ARCHITECTURE.md` §7.
        ///
        /// `Option` because a required field would not decode from a record
        /// written before it existed, which this type's own doc comment names
        /// as the same break as a rename. `None` means the decision predates
        /// the attribution, not that nobody decided.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Authority>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// One generation was admitted and is on its way to the provider.
    ///
    /// Opens the attempt's lifecycle in the log, which is what makes a
    /// resumed session able to say that a request was in flight — and, once
    /// `AttemptSettled` arrives, whether it may safely be sent again.
    GenerationStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<crate::session::PromptSnapshot>,
        attempt: AttemptRef,
        #[serde(default)]
        role: GenerationRole,
    },
    /// A fallback was sanitized or rejected before any reservation was minted.
    FallbackDecision {
        candidate: crate::config::ModelRef,
        decision: FallbackDecision,
    },
    /// The process backend evidence for an admitted execution authority.
    ExecutionObserved {
        observation: ExecObservation,
        probe_evidence: Option<crate::store::evidence::EvidenceRef>,
    },
    /// A capability measurement, retained as evidence without admitting authority.
    ExecutionMeasured {
        observation: ExecObservation,
        probe_evidence: Option<crate::store::evidence::EvidenceRef>,
    },
    /// Private bounded judge text, retained even when decoding fails.
    /// This is evidence, never conversation content or permission authority.
    JudgeResponse {
        attempt: AttemptRef,
        text: String,
        truncated: bool,
    },
    /// The loop decided one refused write of a running exec. Written before
    /// a grant is published to the session desk, so the log never holds less
    /// authority than the sandbox enforces. A grant opens `path` for writing,
    /// never for exec, for this session only; `recommended` is the judge's
    /// advice, not an applied scope.
    SandboxResidualDecided {
        path: CanonPath,
        outcome: ResidualOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recommended: Option<ResidualScope>,
        reason: String,
        /// The judge attempt that answered; `None` when no judge ran.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<AttemptRef>,
    },
    /// One attempt reached a terminal state.
    ///
    /// Carries no money. `UsageUpdated` is the single accounting fact
    /// (`ARCHITECTURE.md`); two events
    /// carrying the same figure are two events that can disagree once they
    /// are bytes on disk. The `UsageReport` inside `outcome` is the
    /// provider's own report, kept as *evidence* about the bookings rather
    /// than as a booking: the reducer checks the two against each other.
    AttemptSettled {
        attempt: AttemptRef,
        outcome: Outcome,
    },
    /// One attempt's usage was reported (added to the running totals),
    /// together with what it cost.
    ///
    /// Token counts and cost travel together because they are the same
    /// observation: the cost *is* the token counts priced by the quote that
    /// was resolved before the request. Splitting them into two events would
    /// let a log contain tokens with no price or a price with no tokens, and
    /// nothing could tell which generation a stray price belonged to.
    UsageUpdated {
        usage: Usage,
        cost_delta: CostDelta,
        /// Which attempt spent this, when it is known.
        ///
        /// `Option` because a *required* field would break every record
        /// written before attempts were attributed — see this type's own doc
        /// comment, and `ToolCallStarted::call_id` for why the `default`
        /// attribute is not what makes it optional. `None` means written before
        /// attribution. Once any `GenerationStarted` has been observed the
        /// reducer refuses a `None`, so a producer cannot opt out of
        /// attribution by omitting the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<AttemptRef>,
    },
    /// A provider revised what one attempt actually used, after the fact.
    ///
    /// Replaces that attempt's contribution rather than adding to it, and the
    /// session totals are refolded from the ledger. Addition would
    /// double-count, and replacing the whole session total (what
    /// `CumulativeCost::reconcile` does) would discard every other
    /// generation. A correction is also the only thing that can clear a
    /// poisoned total, which is why it must not be modelled as subtraction:
    /// nobody knows how big an `Unknown` was.
    UsageCorrected {
        attempt: AttemptRef,
        usage: Usage,
        cost_delta: CostDelta,
    },
    /// How much of the context window is currently occupied.
    ///
    /// A separate event from `UsageUpdated`, and reduced with *replace*
    /// rather than *add* semantics, because occupancy is a level and usage
    /// is a flow. They are not two views of one number: cumulative tokens
    /// only ever grow, while occupancy drops the moment a compaction throws
    /// history away. Deriving one from the other is impossible once history
    /// has been recounted as input, which happens on the second generation
    /// of every session.
    ContextWindowUpdated {
        used_tokens: u64,
        provenance: OccupancyProvenance,
        /// The window the generation that reported this occupancy ran under.
        ///
        /// Beside `used_tokens` rather than looked up later, for the reason
        /// this event exists at all: they are one observation. A turn walks
        /// its fallback candidates and resolves a fresh profile per candidate,
        /// so the model that answered is not always the one a caller resolved
        /// before the loop — and a reader that paired this number with a
        /// separately held window would divide the fallback's occupancy by the
        /// primary's ceiling.
        ///
        /// `Option` for two reasons that must not be collapsed: a record
        /// written before this field existed says nothing, and a model whose
        /// facts state no `max_context_tokens` also says nothing. Both mean
        /// "ask elsewhere", and neither means zero.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
    },
    /// A turn finished.
    TurnCompleted { ok: bool, exit_reason: ExitReason },
    /// The first semantic fact in a clean checkpoint-seeded session.
    CompactionSeeded {
        source: SessionId,
        source_seq: Seq,
        prompt_revision: u32,
        checkpoint: String,
    },
    /// The session log was compacted; `summary` replaces the compacted span.
    Compacted { summary: String },
    /// This session was forked from `source` at `at_seq`.
    Forked {
        source: SessionId,
        at_seq: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<crate::session::PromptSnapshot>,
    },
    /// One complete ordered hook catalogue was admitted atomically.
    HookCatalogueAdmitted { descriptors: HookCatalogueEvidence },
    /// One real owner durably froze the ordered hook plan for its occurrence.
    HookLifecycleReached {
        occurrence: HookOccurrenceKey,
        subject: HookSubject,
        plan: HookPlan,
    },
    /// A hook handler crossed its external-delivery boundary.
    HookInvocationStarted {
        occurrence: EventId,
        hook: HookId,
        invocation: HookInvocationId,
        lifecycle: HookLifecycle,
        subject: HookSubject,
        activation: crate::event::hook::HookActivation,
    },
    /// A previously started hook handler reached one final settlement.
    HookInvocationSettled {
        occurrence: EventId,
        hook: HookId,
        invocation: HookInvocationId,
        result: HookResult,
    },
    /// A bounded child request became durable before preparation.
    SubagentRequested {
        invocation: SubagentInvocationId,
        #[serde(with = "m4_blob_ref")]
        task: BlobRef,
        selections: RunSelections,
    },
    /// The rooted run controller admitted the child.
    SubagentSpawned {
        invocation: SubagentInvocationId,
        child: RunId,
    },
    /// One child reached its final parent-return boundary.
    SubagentSettled {
        invocation: SubagentInvocationId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<RunId>,
        terminal: SubagentTerminal,
    },
    /// One complete ordered skill catalogue was admitted atomically.
    SkillCatalogueAdmitted {
        generation: SkillCatalogueGeneration,
        descriptors: SkillCatalogueEvidence,
    },
    /// Exact rendered skill instructions became durable before consumption.
    SkillInstructionAdmitted {
        invocation: SkillInvocationId,
        skill: SkillId,
        catalogue_generation: SkillCatalogueGeneration,
        source: SkillSourceAuthority,
        location: SkillSourceLocation,
        source_digest: SkillContentDigest,
        #[serde(with = "m4_blob_ref")]
        admission: BlobRef,
        #[serde(with = "m4_blob_ref")]
        rendered: BlobRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// One guarded skill resource became durable before disclosure.
    SkillResourceAdmitted {
        invocation: SkillInvocationId,
        skill: SkillId,
        location: SkillSourceLocation,
        #[serde(with = "m4_blob_ref")]
        content: BlobRef,
    },
    /// An error occurred.
    ///
    /// `kind` is `#[serde(default)]`, which is
    /// [`ErrorKind::Unclassified`] — what a record written before this field
    /// existed actually proved, since no provider code was ever read for it.
    /// Failing closed on an absent classification rather than guessing at one
    /// is the same reading `ReasoningEffortSupport::Unknown` takes.
    ///
    /// Serialised as `error_kind` and not `kind`, because this enum is
    /// internally tagged on `kind` and a field of that name would collide
    /// with the discriminant. Renaming the *tag* instead would be a
    /// re-tagging, which this module's own rule puts on the far side of a
    /// `CODEC_VERSION` bump; renaming the field is free because no record
    /// carrying it exists yet.
    Error {
        message: String,
        #[serde(default, rename = "error_kind")]
        kind: ErrorKind,
    },
}

impl EventKind {
    /// Structured blob references carried by this durable event.
    pub fn blob_references(&self) -> Vec<BlobRef> {
        let mut references = Vec::new();
        match self {
            Self::ExecutionObserved { probe_evidence, .. }
            | Self::ExecutionMeasured { probe_evidence, .. } => {
                references.extend(probe_evidence.iter().map(|evidence| evidence.blob_ref()));
            }
            Self::PromptQueued { images, .. } | Self::UserPrompt { images, .. } => {
                references.extend(images.iter().map(|image| image.reference().clone()));
            }
            Self::ToolCallCompleted { result, .. } => {
                references.extend(
                    result
                        .content
                        .iter()
                        .filter_map(|block| block.evidence_reference())
                        .map(|reference| reference.blob_ref()),
                );
                if let ToolResultProvenance::Foreign { result } = result.provenance() {
                    references.push(result.evidence_reference().blob_ref());
                }
            }
            Self::DetachedExecSettled { snapshot } => {
                if let Some(
                    crate::permit::DetachedExecOutcome::Exited { result, .. }
                    | crate::permit::DetachedExecOutcome::Signalled { result, .. },
                ) = &snapshot.outcome
                {
                    references.extend(
                        result
                            .content
                            .iter()
                            .filter_map(|block| block.evidence_reference())
                            .map(|reference| reference.blob_ref()),
                    );
                }
            }
            Self::HookInvocationSettled { result, .. } => {
                if let Some(feedback) = result.feedback() {
                    references.push(feedback.reference().blob_ref());
                }
            }
            Self::GoalTransitioned {
                transition:
                    GoalTransition::Achieved {
                        evidence: Some(evidence),
                        ..
                    },
                ..
            } => references.push(evidence.blob_ref()),
            Self::SubagentRequested { task, .. } => references.push(task.clone()),
            Self::SubagentSettled { terminal, .. } => match terminal {
                SubagentTerminal::Returned { final_message }
                | SubagentTerminal::ReturnBlocked { final_message, .. } => {
                    references.push(final_message.clone());
                }
                SubagentTerminal::Refused { .. } | SubagentTerminal::Failed { .. } => {}
            },
            Self::SkillInstructionAdmitted {
                admission,
                rendered,
                ..
            } => {
                references.push(admission.clone());
                references.push(rendered.clone());
            }
            Self::SkillResourceAdmitted { content, .. } => references.push(content.clone()),
            _ => {}
        }
        references
    }

    /// A short, stable name for the variant — used as a tracing span field
    /// so `apply` spans are queryable by kind without formatting the (often
    /// large) payload.
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::SessionTitleChanged { .. } => "session_title_changed",
            EventKind::SessionDeleted => "session_deleted",
            EventKind::SessionPauseRequested { .. } => "session_pause_requested",
            EventKind::SessionPaused { .. } => "session_paused",
            EventKind::SessionPauseObserved { .. } => "session_pause_observed",
            EventKind::SessionResumed { .. } => "session_resumed",
            EventKind::GoalSet { .. } => "goal_set",
            EventKind::GoalContinuation { .. } => "goal_continuation",
            EventKind::ContextRequested { .. } => "context_requested",
            EventKind::ContextCooldown { .. } => "context_cooldown",
            EventKind::ContextRequestConsumed => "context_request_consumed",
            EventKind::GoalCheckConfigured { .. } => "goal_check_configured",
            EventKind::GoalAllowanceInherited { .. } => "goal_allowance_inherited",
            EventKind::GoalCheckStopped { .. } => "goal_check_stopped",
            EventKind::GoalUpdated { .. } => "goal_updated",
            EventKind::GoalTransitioned { .. } => "goal_transitioned",
            EventKind::PromptQueued { .. } => "prompt_queued",
            EventKind::PromptDeliveryExpedited { .. } => "prompt_delivery_expedited",
            EventKind::PromptRetracted { .. } => "prompt_retracted",
            EventKind::UserPrompt { .. } => "user_prompt",
            EventKind::AssistantDelta { .. } => "assistant_delta",
            EventKind::AssistantMessage { .. } => "assistant_message",
            EventKind::ReasoningDelta { .. } => "reasoning_delta",
            EventKind::ReasoningState { .. } => "reasoning_state",
            EventKind::ContextCommandStarted { .. } => "context_command_started",
            EventKind::ProviderContent { .. } => "provider_content",
            EventKind::ToolCallStarted { .. } => "tool_call_started",
            EventKind::ToolCallDelta { .. } => "tool_call_delta",
            EventKind::ToolCallCompleted { .. } => "tool_call_completed",
            EventKind::ToolCallMeasured { .. } => "tool_call_measured",
            EventKind::DetachedExecStarted { .. } => "detached_exec_started",
            EventKind::HostContextAppended { .. } => "host_context_appended",
            EventKind::DetachedExecSettled { .. } => "detached_exec_settled",
            EventKind::QuestionRequested { .. } => "question_requested",
            EventKind::QuestionSettled { .. } => "question_settled",
            EventKind::QuestionMemberSettled { .. } => "question_member_settled",
            EventKind::PermissionRequested { .. } => "permission_requested",
            EventKind::PermissionAmended { .. } => "permission_amended",
            EventKind::PermissionDecided { .. } => "permission_decided",
            EventKind::GenerationStarted { .. } => "generation_started",
            EventKind::FallbackDecision { .. } => "fallback_decision",
            EventKind::ExecutionObserved { .. } => "execution_observed",
            EventKind::ExecutionMeasured { .. } => "execution_measured",
            EventKind::JudgeResponse { .. } => "judge_response",
            EventKind::SandboxResidualDecided { .. } => "sandbox_residual_decided",
            EventKind::AttemptSettled { .. } => "attempt_settled",
            EventKind::UsageUpdated { .. } => "usage_updated",
            EventKind::UsageCorrected { .. } => "usage_corrected",
            EventKind::ContextWindowUpdated { .. } => "context_window_updated",
            EventKind::TurnCompleted { .. } => "turn_completed",
            EventKind::CompactionSeeded { .. } => "compaction_seeded",
            EventKind::Compacted { .. } => "compacted",
            EventKind::Forked { .. } => "forked",
            EventKind::HookCatalogueAdmitted { .. } => "hook_catalogue_admitted",
            EventKind::HookLifecycleReached { .. } => "hook_lifecycle_reached",
            EventKind::HookInvocationStarted { .. } => "hook_invocation_started",
            EventKind::HookInvocationSettled { .. } => "hook_invocation_settled",
            EventKind::SubagentRequested { .. } => "subagent_requested",
            EventKind::SubagentSpawned { .. } => "subagent_spawned",
            EventKind::SubagentSettled { .. } => "subagent_settled",
            EventKind::SkillCatalogueAdmitted { .. } => "skill_catalogue_admitted",
            EventKind::SkillInstructionAdmitted { .. } => "skill_instruction_admitted",
            EventKind::SkillResourceAdmitted { .. } => "skill_resource_admitted",
            EventKind::Error { .. } => "error",
        }
    }
}

mod m4_blob_ref {
    use super::*;

    #[derive(Serialize)]
    struct Wire<'a> {
        id: &'a str,
        bytes: u64,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OwnedWire {
        id: String,
        bytes: u64,
    }

    pub fn serialize<S>(value: &BlobRef, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Wire {
            id: value.id.as_str(),
            bytes: value.bytes,
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BlobRef, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OwnedWire::deserialize(deserializer)?;
        let id = BlobId::parse(wire.id).map_err(serde::de::Error::custom)?;
        Ok(BlobRef::new(id, wire.bytes))
    }
}

mod subagent_failed_settlement {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum Wire {
        ContextOverflow,
        Cancelled {
            reason: CancelReason,
        },
        PermissionRequired {
            action: String,
            by: DeniedBy,
        },
        CancelledBeforeStart,
        Interrupted {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            diagnostic: Option<String>,
        },
        Uncertain {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            diagnostic: Option<String>,
        },
    }

    pub fn serialize<S>(value: &RunSettlement, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let wire = match value {
            RunSettlement::Completed => {
                return Err(serde::ser::Error::custom(
                    "completed child cannot use the failed terminal",
                ));
            }
            // Pause is a session control and a child run is never its target:
            // a subagent call is one indivisible effect, and the parent's
            // boundary lies after that effect has settled on its own. So this
            // is unreachable for the same reason `Completed` is — a child that
            // reached here failed, and pausing is not a failure.
            RunSettlement::Paused => {
                return Err(serde::ser::Error::custom(
                    "a child run is never paused; the parent's boundary follows it",
                ));
            }
            RunSettlement::ContextOverflow => Wire::ContextOverflow,
            RunSettlement::Cancelled { reason } => Wire::Cancelled { reason: *reason },
            RunSettlement::PermissionRequired { action, by } => Wire::PermissionRequired {
                action: action.clone(),
                by: by.clone(),
            },
            RunSettlement::CancelledBeforeStart => Wire::CancelledBeforeStart,
            RunSettlement::Interrupted { diagnostic } => Wire::Interrupted {
                diagnostic: diagnostic.clone(),
            },
            RunSettlement::Uncertain { diagnostic } => Wire::Uncertain {
                diagnostic: diagnostic.clone(),
            },
        };
        wire.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<RunSettlement, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match Wire::deserialize(deserializer)? {
            Wire::ContextOverflow => RunSettlement::ContextOverflow,
            Wire::Cancelled { reason } => RunSettlement::Cancelled { reason },
            Wire::PermissionRequired { action, by } => {
                RunSettlement::PermissionRequired { action, by }
            }
            Wire::CancelledBeforeStart => RunSettlement::CancelledBeforeStart,
            Wire::Interrupted { diagnostic } => RunSettlement::Interrupted { diagnostic },
            Wire::Uncertain { diagnostic } => RunSettlement::Uncertain { diagnostic },
        })
    }
}

/// One event in the internal log: an envelope plus its payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEvent {
    pub env: Envelope,
    pub kind: EventKind,
}
