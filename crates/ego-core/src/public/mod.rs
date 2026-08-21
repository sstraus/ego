//! `PublicEvent`: the privacy-filtered, compatibility-governed projection of
//! the internal event log.
//!
//! See `ARCHITECTURE.md` §1 (four serialisations, one semantic model) and
//! `CLI.md` §4.2. A new internal event never becomes public automatically.

use serde::{Deserialize, Serialize};

use crate::cost::CostDelta;
use crate::event::domain::{DomainEvent, EventKind, ExitReason, OccupancyProvenance};
use crate::event::id::{ItemId, SessionId, TurnId};
use crate::state::Usage;

/// The current `PublicEvent` schema version, carried in `SessionStarted`'s
/// `v` field. `CLI.md` §4.2: "the schema is versioned in the header and
/// published".
pub const PUBLIC_EVENT_VERSION: u32 = 4;

/// How a run came to load one instruction file.
///
/// Two values and not a boolean, because the difference decides what the file
/// is allowed to be: an ambient `AGENTS.md` is discovered under the trust gate
/// and refused if it is a symlink, while an explicit one was named by the
/// person running the command. A consumer reading a descriptor without this
/// cannot tell the workspace's claim from the user's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionSource {
    /// Discovered as [`INSTRUCTIONS_FILE`](crate::config::INSTRUCTIONS_FILE)
    /// somewhere between the workspace root and the working directory.
    Ambient,
    /// Named outright, with `--instructions FILE`.
    Explicit,
}

/// One instruction file a run loaded, described but **never quoted**.
///
/// `docs/08-codex-critique.md` I2 asks for each path, digest and scope to be
/// recorded in the public start event, and asks for the *text* to stay out of
/// it. Those are one requirement, not two: the stream this rides is what a CI
/// job archives, and an `AGENTS.md` is one symlink away from being something
/// else — which is why `config::Refusal::Symlink` exists at all. A descriptor
/// proves which file contributed; it does not republish it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionDescriptor {
    /// Where the file was found.
    pub path: String,
    /// Lowercase hex SHA-256 of the bytes as read — the digest
    /// `config::load` already computed, carried rather than recomputed.
    ///
    /// It describes the file, not the prompt: the assembler frames what it
    /// sends, so a digest over the framed text would change with the framing
    /// and prove nothing about the repository. See
    /// `docs/decisions/2026-08-21-a-binary-that-produces-the-exit-code.md` §4.
    pub sha256: String,
    /// How many bytes that digest covers.
    pub bytes: u64,
    /// The workspace-relative directory this file governs, `"."` at the root.
    /// A file named outside the workspace has no relative scope and reports
    /// the directory it lives in.
    pub scope: String,
    pub source: InstructionSource,
}

/// The item kinds carried by `item.*` events, per `CLI.md` §4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A user or assistant message.
    Message,
    /// A block of the model's reasoning/thinking.
    Reasoning,
    /// A tool call.
    Tool,
    /// A permission request/decision.
    Permission,
    /// A subagent invocation.
    Subagent,
}

/// The privacy-filtered, versioned JSONL wire event — `CLI.md` §4.2's
/// three-level envelope: `session.started` / `turn.started|completed` /
/// `item.started|delta|completed`.
///
/// `SessionStarted` and `TurnStarted` have no `DomainEvent` counterpart:
/// nothing in `EventKind` (ARCH §1's exhaustive internal set) marks the
/// start of a session or a turn, because that's not a fact the *reducer*
/// establishes — it's a fact the harness that drives a session already
/// knows the instant it opens one, before any domain event exists. So the
/// harness constructs these two variants directly; `project_public` below
/// only ever returns the other variants, which do each originate from a
/// `DomainEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PublicEvent {
    /// The header event: schema version and session-wide facts known once,
    /// at session start.
    SessionStarted {
        /// Schema version. Always `PUBLIC_EVENT_VERSION`.
        v: u32,
        session: SessionId,
        /// Feature detection by name, never version parsing (`CLI.md`
        /// §4.2).
        capabilities: Vec<String>,
        /// MCP server load errors. Present only when non-empty — a CI gate
        /// is a presence check, not a length check.
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        mcp_errors: Vec<String>,
        /// Hook load errors. Present only when non-empty, same rule.
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        hook_errors: Vec<String>,
        /// What instructions this session was started with, described and
        /// never quoted. Present only when non-empty, same rule again: a run
        /// in a bare posture loaded nothing, and an absent key says so
        /// without a consumer having to compare a length.
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        instructions: Vec<InstructionDescriptor>,
    },
    /// A turn began.
    TurnStarted { turn: TurnId },
    /// An item (message, reasoning block, tool call, permission request or
    /// subagent invocation) began.
    ItemStarted {
        turn: TurnId,
        item: ItemId,
        kind: ItemKind,
        /// The tool name, for `kind: tool`; absent for every other kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The tool call's arguments, the permission's description, or the
        /// subagent id — this item kind's one piece of starting detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// A streamed increment of an item's content.
    ItemDelta {
        turn: TurnId,
        item: ItemId,
        kind: ItemKind,
        text: String,
    },
    /// An item finished.
    ItemCompleted {
        turn: TurnId,
        item: ItemId,
        kind: ItemKind,
        /// One outcome field, never a second status field beside it
        /// (`ARCHITECTURE.md` §1: "single `ok` field for outcomes").
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// A turn finished. Every `TurnCompleted` carries its own outcome; the
    /// *last* one in the stream is, by construction, the terminal event —
    /// no separate "final result" mechanism is needed (`CLI.md` §4.2: "no
    /// `--output-last-message` temp-file dance").
    ///
    /// Deliberately no usage field: turn completion is a turn *outcome*,
    /// while usage is observed at a *generation* boundary — a fallback
    /// chain can switch providers mid-turn and report several separately
    /// attributable usage events inside one turn. Usage rides its own
    /// `UsageUpdated` variant instead, so this event has exactly one
    /// concern (`ok` + `exit_reason`) and never a running total that could
    /// drift from `SessionState.usage`, the actual cumulative source of
    /// truth.
    TurnCompleted {
        turn: TurnId,
        ok: bool,
        exit_reason: ExitReason,
    },
    /// Incremental usage was reported.
    ///
    /// `usage_delta`, not `usage`: this is the increment observed at *this*
    /// generation boundary, never a cumulative total. A field named
    /// `usage` on a stable, compatibility-governed DTO reads as a running
    /// total to anyone who hasn't read this comment, and that misreading
    /// is expensive once it reaches budget or billing logic — so the name
    /// carries the semantics instead of relying on documentation alone.
    ///
    /// `cost_delta` is the price of *this* generation, and it rides the same
    /// event as the tokens rather than a separate one: the cost is the
    /// tokens priced, and separating them would let a consumer see a price
    /// it cannot attribute to any usage. It is an explicit
    /// `known`/`unknown` value, never an absent field, so a consumer cannot
    /// read "we could not price this" as "this was free".
    UsageUpdated {
        turn: TurnId,
        #[serde(skip_serializing_if = "Option::is_none")]
        item: Option<ItemId>,
        usage_delta: Usage,
        cost_delta: CostDelta,
    },
    /// The context window's occupancy changed.
    ///
    /// Public because it is the only honest source for "how full is the
    /// context" — it cannot be derived from the cumulative token totals,
    /// which recount prior history as input on every call and therefore
    /// diverge from occupancy from the second generation onward. Carries its
    /// `provenance` so a consumer can tell a provider's authoritative answer
    /// from a local estimate instead of treating every figure as equally
    /// solid.
    ContextWindowUpdated {
        turn: TurnId,
        used_tokens: u64,
        provenance: OccupancyProvenance,
    },
    /// An error occurred, possibly outside any single item.
    Error {
        turn: TurnId,
        #[serde(skip_serializing_if = "Option::is_none")]
        item: Option<ItemId>,
        message: String,
    },
}

/// Project one internal `DomainEvent` onto the public wire contract.
///
/// Returns `None` for:
/// - internal-only event kinds that never leak into the public stream
///   (`Compacted`, `Forked`, `HookFired` — none of them is one of the five
///   public item kinds, and none belongs on the three-level envelope);
/// - an item-shaped event (message/tool/permission/subagent content) whose
///   envelope carries no item id. `state.rs`'s reducer already tolerates
///   this degenerate case without panicking (`SessionState::item_mut`
///   returns `None`); `project_public` is equally total, dropping the event
///   rather than fabricating an item id or panicking.
pub fn project_public(ev: &DomainEvent) -> Option<PublicEvent> {
    let turn = ev.env.turn;
    let item = ev.env.item;

    match &ev.kind {
        EventKind::UserPrompt { text } => Some(PublicEvent::ItemCompleted {
            turn,
            item: item?,
            kind: ItemKind::Message,
            ok: true,
            text: Some(text.clone()),
        }),
        EventKind::AssistantDelta { text } => Some(PublicEvent::ItemDelta {
            turn,
            item: item?,
            kind: ItemKind::Message,
            text: text.clone(),
        }),
        EventKind::AssistantMessage { text } => Some(PublicEvent::ItemCompleted {
            turn,
            item: item?,
            kind: ItemKind::Message,
            ok: true,
            text: Some(text.clone()),
        }),
        EventKind::ReasoningDelta { text } => Some(PublicEvent::ItemDelta {
            turn,
            item: item?,
            kind: ItemKind::Reasoning,
            text: text.clone(),
        }),
        // `call_id` is deliberately not projected. It is the *provider's*
        // correlation token, private to conversation replay; the public
        // surface identifies a tool call by the harness's own `ItemId`, and
        // adding a second identity to a versioned schema would be a
        // compatibility-governed change with no consumer asking for it.
        EventKind::ToolCallStarted { name, args, .. } => Some(PublicEvent::ItemStarted {
            turn,
            item: item?,
            kind: ItemKind::Tool,
            name: Some(name.clone()),
            text: Some(args.clone()),
        }),
        EventKind::ToolCallDelta { text } => Some(PublicEvent::ItemDelta {
            turn,
            item: item?,
            kind: ItemKind::Tool,
            text: text.clone(),
        }),
        EventKind::ToolCallCompleted { ok, output } => Some(PublicEvent::ItemCompleted {
            turn,
            item: item?,
            kind: ItemKind::Tool,
            ok: *ok,
            text: Some(output.clone()),
        }),
        EventKind::PermissionRequested { description } => Some(PublicEvent::ItemStarted {
            turn,
            item: item?,
            kind: ItemKind::Permission,
            name: None,
            text: Some(description.clone()),
        }),
        EventKind::PermissionDecided { allowed } => Some(PublicEvent::ItemCompleted {
            turn,
            item: item?,
            kind: ItemKind::Permission,
            ok: *allowed,
            text: None,
        }),
        // `attempt` is deliberately not projected: the public schema is
        // compatibility-governed and v1 has no field for it, so surfacing an
        // internal identity here would be a schema change for no consumer.
        EventKind::UsageUpdated {
            usage, cost_delta, ..
        } => Some(PublicEvent::UsageUpdated {
            turn,
            item,
            usage_delta: *usage,
            cost_delta: cost_delta.clone(),
        }),
        EventKind::ContextWindowUpdated {
            used_tokens,
            provenance,
        } => Some(PublicEvent::ContextWindowUpdated {
            turn,
            used_tokens: *used_tokens,
            provenance: *provenance,
        }),
        EventKind::TurnCompleted { ok, exit_reason } => Some(PublicEvent::TurnCompleted {
            turn,
            ok: *ok,
            exit_reason: *exit_reason,
        }),
        EventKind::Compacted { .. } => None,
        EventKind::Forked { .. } => None,
        EventKind::HookFired { .. } => None,
        EventKind::SubagentSpawned { subagent_id } => Some(PublicEvent::ItemStarted {
            turn,
            item: item?,
            kind: ItemKind::Subagent,
            name: None,
            text: Some(subagent_id.clone()),
        }),
        EventKind::SubagentReturned { result, .. } => Some(PublicEvent::ItemCompleted {
            turn,
            item: item?,
            kind: ItemKind::Subagent,
            ok: true,
            text: Some(result.clone()),
        }),
        EventKind::Error { message } => Some(PublicEvent::Error {
            turn,
            item,
            message: message.clone(),
        }),

        // The attempt lifecycle is internal, and stays internal. A client
        // consumes the *effect* — usage, cost, occupancy — not the harness's
        // bookkeeping about which journey to the wire produced it, and
        // `AttemptRef` is an identity core mints for its own accounting.
        // A correction is not a public event either: it does not add a
        // figure, it revises one, and what a client should see afterwards is
        // the corrected cumulative total, which the next `UsageUpdated` or
        // `ContextWindowUpdated` already carries.
        EventKind::GenerationStarted { .. }
        | EventKind::AttemptSettled { .. }
        | EventKind::UsageCorrected { .. } => None,
    }
}
