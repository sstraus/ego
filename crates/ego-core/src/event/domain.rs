//! `DomainEvent`: the exhaustive internal reducer input.
//!
//! See `ARCHITECTURE.md` §1 for the variant set and the envelope fields.

use serde::{Deserialize, Serialize};

use super::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use super::time::Timestamp;
use crate::cost::CostDelta;
use crate::generator::attempt::{AttemptRef, Outcome};
use crate::generator::types::{ErrorKind, ReasoningState};
use crate::state::Usage;

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

/// Who stopped a turn that did not finish on its own.
///
/// Carried by [`ExitReason::Cancelled`] rather than left to the run level,
/// because a timeout that interrupts a turn has to be durable: without this,
/// the log and `PublicEvent::TurnCompleted` would say `Cancelled` while the
/// CLI printed `timeout`, and replay, JSONL and ACP would give three accounts
/// of one fact. A limit applied *between* turns is not here — that is the run
/// level's, where it is true. See
/// `docs/decisions/2026-08-21-two-levels-of-ending.md`.
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
    /// The turn was stopped by a budget limit.
    Budget,
    /// The run was asked for something it cannot do, and no retry will change
    /// that: `CLI.md` §4.3's exit 2. Distinct from [`Provider`] because
    /// nothing was sent, and from [`Internal`] because nothing of ours
    /// failed — the invocation and the resolved model disagree, and the
    /// caller is the only party who can settle it. See
    /// `docs/decisions/2026-08-21-a-request-the-model-will-not-take.md`.
    ///
    /// [`Provider`]: ExitReason::Provider
    /// [`Internal`]: ExitReason::Internal
    Usage,
    /// A call needed a grant that nobody in this run could give. `CLI.md`
    /// §4.4: a headless run never hangs on a prompt, so an unresolvable
    /// permission ends it rather than being answered on the model's behalf.
    PermissionRequired,
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

/// The exhaustive set of internal reducer inputs, per ARCH §1:
///
/// ```text
/// UserPrompt | AssistantDelta | AssistantMessage | ReasoningDelta |
/// ToolCallStarted | ToolCallDelta | ToolCallCompleted | PermissionRequested |
/// PermissionDecided | UsageUpdated | ContextWindowUpdated | TurnCompleted |
/// Compacted | Forked | HookFired | SubagentSpawned | SubagentReturned | Error
/// ```
///
/// plus the attempt lifecycle and accounting variants added in
/// `docs/decisions/2026-08-21-attempt-accounting-ledger.md`:
///
/// ```text
/// GenerationStarted | AttemptSettled | UsageCorrected
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
/// corrected in `docs/decisions/2026-08-21-attempt-accounting-ledger.md`.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum EventKind {
    /// The user submitted a prompt.
    UserPrompt { text: String },
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
    /// A tool call finished, with its outcome and final output.
    ToolCallCompleted { ok: bool, output: String },
    /// A sensitive action needs a permission decision.
    PermissionRequested { description: String },
    /// A permission decision was made.
    PermissionDecided { allowed: bool },
    /// One generation was admitted and is on its way to the provider.
    ///
    /// Opens the attempt's lifecycle in the log, which is what makes a
    /// resumed session able to say that a request was in flight — and, once
    /// `AttemptSettled` arrives, whether it may safely be sent again.
    GenerationStarted { attempt: AttemptRef },
    /// One attempt reached a terminal state.
    ///
    /// Carries no money. `UsageUpdated` is the single accounting fact
    /// (`docs/decisions/2026-08-21-attempt-accounting-ledger.md`); two events
    /// carrying the same figure are two events that can disagree once they
    /// are bytes on disk. The `UsageReport` inside `outcome` is the
    /// provider's own report, kept as *evidence* about the bookings rather
    /// than as a booking: the reducer checks the two against each other.
    AttemptSettled {
        attempt: AttemptRef,
        outcome: Outcome,
    },
    /// One generation's usage was reported (added to the running totals),
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
    },
    /// A turn finished.
    TurnCompleted { ok: bool, exit_reason: ExitReason },
    /// The session log was compacted; `summary` replaces the compacted span.
    Compacted { summary: String },
    /// This session was forked from `source` at `at_seq`.
    Forked { source: SessionId, at_seq: Seq },
    /// A hook fired; `blocked` is whether it blocked the action it observed.
    HookFired { name: String, blocked: bool },
    /// A subagent was spawned.
    SubagentSpawned { subagent_id: String },
    /// A subagent returned its final result.
    SubagentReturned { subagent_id: String, result: String },
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
    /// A short, stable name for the variant — used as a tracing span field
    /// so `apply` spans are queryable by kind without formatting the (often
    /// large) payload.
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::UserPrompt { .. } => "user_prompt",
            EventKind::AssistantDelta { .. } => "assistant_delta",
            EventKind::AssistantMessage { .. } => "assistant_message",
            EventKind::ReasoningDelta { .. } => "reasoning_delta",
            EventKind::ReasoningState { .. } => "reasoning_state",
            EventKind::ToolCallStarted { .. } => "tool_call_started",
            EventKind::ToolCallDelta { .. } => "tool_call_delta",
            EventKind::ToolCallCompleted { .. } => "tool_call_completed",
            EventKind::PermissionRequested { .. } => "permission_requested",
            EventKind::PermissionDecided { .. } => "permission_decided",
            EventKind::GenerationStarted { .. } => "generation_started",
            EventKind::AttemptSettled { .. } => "attempt_settled",
            EventKind::UsageUpdated { .. } => "usage_updated",
            EventKind::UsageCorrected { .. } => "usage_corrected",
            EventKind::ContextWindowUpdated { .. } => "context_window_updated",
            EventKind::TurnCompleted { .. } => "turn_completed",
            EventKind::Compacted { .. } => "compacted",
            EventKind::Forked { .. } => "forked",
            EventKind::HookFired { .. } => "hook_fired",
            EventKind::SubagentSpawned { .. } => "subagent_spawned",
            EventKind::SubagentReturned { .. } => "subagent_returned",
            EventKind::Error { .. } => "error",
        }
    }
}

/// One event in the internal log: an envelope plus its payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEvent {
    pub env: Envelope,
    pub kind: EventKind,
}
