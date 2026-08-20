//! `DomainEvent`: the exhaustive internal reducer input.
//!
//! See `ARCHITECTURE.md` §1 for the variant set and the envelope fields.

use serde::{Deserialize, Serialize};

use super::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use super::time::Timestamp;
use crate::cost::CostDelta;
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

/// How a turn ended. Deliberately one field (paired with `ok` on
/// `TurnCompleted`), never a separate error field alongside it (ARCH §1
/// review note: "one error field, never two").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// The turn ran to completion.
    Completed,
    /// The turn was cancelled (by the user or a timeout).
    Cancelled,
    /// The turn ended in an error.
    Error,
    /// The turn was stopped by a budget limit.
    Budget,
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
/// This is the internal representation: it evolves freely with core, and
/// `PublicEvent` is the privacy-filtered, compatibility-governed projection
/// of it. "Freely" has one hard limit, though — `SessionStore::append`
/// serialises this type verbatim as the `StoreRecord` payload, so its serde
/// shape *is* the durable format and a later build has to read what an
/// earlier one wrote. `tests/codec.rs`'s committed `golden/store_log.jsonl`
/// pins those bytes: adding a variant or a field is fine, but renaming or
/// re-tagging an existing one is a `CODEC_VERSION` bump plus a migration,
/// not a fixture update.
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
    /// A tool call began, with its name and raw (unvalidated) arguments.
    ToolCallStarted { name: String, args: String },
    /// A streamed increment of a tool call's output.
    ToolCallDelta { text: String },
    /// A tool call finished, with its outcome and final output.
    ToolCallCompleted { ok: bool, output: String },
    /// A sensitive action needs a permission decision.
    PermissionRequested { description: String },
    /// A permission decision was made.
    PermissionDecided { allowed: bool },
    /// One generation's usage was reported (added to the running totals),
    /// together with what it cost.
    ///
    /// Token counts and cost travel together because they are the same
    /// observation: the cost *is* the token counts priced by the quote that
    /// was resolved before the request. Splitting them into two events would
    /// let a log contain tokens with no price or a price with no tokens, and
    /// nothing could tell which generation a stray price belonged to.
    UsageUpdated { usage: Usage, cost_delta: CostDelta },
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
    Error { message: String },
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
            EventKind::ToolCallStarted { .. } => "tool_call_started",
            EventKind::ToolCallDelta { .. } => "tool_call_delta",
            EventKind::ToolCallCompleted { .. } => "tool_call_completed",
            EventKind::PermissionRequested { .. } => "permission_requested",
            EventKind::PermissionDecided { .. } => "permission_decided",
            EventKind::UsageUpdated { .. } => "usage_updated",
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
