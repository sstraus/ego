//! Projection of `ego-core` domain events onto ACP `SessionUpdate` values.
//!
//! See `ARCHITECTURE.md` §7 and `plans/m0a-contracts.md` Step 9. Projection
//! functions only — no transport, no server, no state of our own. The types
//! come from the real `agent-client-protocol-schema` crate rather than
//! hand-rolled mirrors, so a schema bump is a compile error instead of a
//! silently stale golden.
//!
//! ## Two functions, not one with a version flag
//!
//! [`project_v1`] and [`project_v2`] are separate mappings over the same
//! `DomainEvent`, matching [`ego_core::public::project_public`]'s shape:
//! pure, total, stateless `fn(&DomainEvent) -> Option<T>`. v1 and v2's
//! `SessionUpdate` enums have genuinely different variant sets — v2 added
//! `UserMessage`/`AgentMessage`/`AgentThought` (replace-semantics upserts)
//! and `ToolCallContentChunk` (an append-only primitive) that v1 simply does
//! not have. A single function branching on a version enum would smear that
//! difference across every match arm; two thin functions keep each version's
//! shape honest and let a v1-only or v2-only schema change show up as a
//! diff in one function, not both.
//!
//! ## `Option`, never `Vec`
//!
//! Every mapping below is 1-to-0-or-1: no domain event needs more than one
//! `SessionUpdate` to represent it. `Option<SessionUpdate>` says that
//! directly; a `Vec` would imply a cardinality this projection never uses.
//!
//! ## The v1/v2 asymmetry principle
//!
//! Several domain events are genuinely unrepresentable *by a stateless
//! projection* in v1 but representable in v2, for one recurring reason:
//! v1's `SessionUpdate` variants are either pure-append (chunks) or
//! pure-replace (`ToolCallUpdate.content`), with no way to say "this is the
//! final, authoritative value, discard what streamed before" for a
//! *message*, and no way to *append* to tool-call content at all. v2 added
//! exactly these two missing primitives (`AgentMessage`/`UserMessage`/
//! `AgentThought` upserts with patch semantics, and `ToolCallContentChunk`).
//!
//! Where that happens the v1 mapping is `None`, documented at the match
//! arm. `None` is *not* a claim that nothing was lost — see the known
//! limitation below. It is the choice not to guess, because the nearest
//! existing v1 variant is wrong in one direction or the other depending on
//! history this function cannot see.
//!
//! ## Known limitation: v1 loses a non-streamed final message
//!
//! `AssistantMessage` is ego's *authoritative* message text — the reducer
//! (`ego_core::state`) assigns it (`item.text = text`) rather than appending
//! it, exactly matching v2's `AgentMessage` patch semantics. v1 has no
//! replace primitive for messages at all: every message-shaped v1 variant is
//! a `*Chunk`, and chunks only ever append.
//!
//! So the correct v1 output for one `AssistantMessage` depends on what the
//! receiving client has *already rendered* for that message id:
//!
//! - it has already rendered the streamed deltas ⇒ the correct output is to
//!   append nothing;
//! - it has rendered nothing (the message never streamed, or the client
//!   attached via `session/resume`, which the schema documents as resuming
//!   "without returning previous messages") ⇒ the correct output is to
//!   append the full text.
//!
//! A stateless `fn(&DomainEvent) -> Option<_>` returns one fixed value, so
//! it cannot satisfy both. Both branches are therefore wrong somewhere, and
//! the choice is which way to be wrong:
//!
//! - **always emit a chunk** — the fully-streamed turn renders as
//!   `"Sure, one sec" + "Sure, one sec"`. Verified against real clients:
//!   v1 receivers concatenate arriving chunks, and no de-duplication is
//!   possible in general (legitimate text repeats — `"ha"` + `"ha"`).
//!   Corrupts the *common* case to rescue the rare one.
//! - **emit nothing** (what this does) — a message that never streamed is
//!   lost from the v1 transcript entirely.
//!
//! This is a real, accepted M0a limitation, not a solved problem, and it is
//! pinned by a test named for it rather than left as an unremarked absence
//! in the golden. It is *not* fixable by adding a field to the domain event
//! (e.g. "was this streamed", or the remainder to append): such a field
//! describes what is earlier in the *log*, whereas correctness depends on
//! what a given *client view* received, and those diverge precisely in the
//! resume case above. The fix belongs to the M0b transport adapter, which
//! is the only component that knows what it handed to a particular client;
//! `ARCHITECTURE.md` §7 already classes ACP adapters as lossy projections,
//! and `plans/m0a-contracts.md` Step 9 explicitly excludes transport.
//!
//! `ToolCallDelta`'s v1 `None` is the same shape of limitation: v1 has no
//! append primitive for tool-call content, and `ToolCallUpdate.content`
//! replaces the whole collection, so emitting one increment needs the
//! accumulated content a stateless projection does not have.
//!
//! ## `SessionUpdate::Other` is never produced
//!
//! v2's `SessionUpdate::Other` (an untagged escape hatch preserving an
//! unrecognised `sessionUpdate` discriminator and its raw fields) exists for
//! *receivers* tolerating updates from a newer protocol revision or a
//! genuinely custom extension they don't understand — forwarding, storing or
//! displaying them generically without choking. We are always a *producer*
//! mapping a fixed, exhaustively-matched domain enum onto a fixed schema
//! version; we never have an "unknown" update to preserve, so there is
//! nothing for `Other` to do on this side. Producing it deliberately (e.g.
//! to smuggle ego-specific data) would just be `SessionUpdate::Other` used
//! as a worse-typed dumping ground for data that belongs in one of the
//! typed variants below, or in `_meta` (every typed variant already carries
//! one) — so it is never constructed here.
//!
//! `v2::StopReason::Other`, by contrast, *is* produced (see
//! [`stop_reason_v2`]): it is a narrowly-scoped, spec-sanctioned extension
//! point on a single field (with the documented `_`-prefix convention for
//! implementation-specific values), not the top-level "unknown update"
//! escape hatch, and it is the only honest way to report `ExitReason::Error`
//! and `ExitReason::Budget` — ACP's `StopReason` has no first-class
//! equivalent for either, and forcing them into `EndTurn` or `MaxTokens`
//! would misreport why the turn actually stopped, exactly the mistake
//! `UsageUpdated` (below) already establishes we must not make.
//!
//! ## The third entry point: `project_acp_v1` / `project_acp_v2`
//!
//! `SessionUpdate::UsageUpdate` cannot be produced by a stateless function,
//! for two independent reasons, and it is the only variant that cannot:
//!
//! - its `used` is tokens *currently in context*, which is a level that a
//!   compaction lowers — not something derivable from any event's payload,
//!   and emphatically not from cumulative token totals, which recount prior
//!   history as input on every call and so diverge from occupancy from the
//!   second generation onward;
//! - its `cost` is the *cumulative session* cost, so even a per-generation
//!   cost delta on the event has to be summed over everything before it;
//! - its `size` is the model's context window, which lives in
//!   `ModelCapabilities`, not in the event at all.
//!
//! So [`project_acp_v1`] and [`project_acp_v2`] take the reduced state
//! *after* the event and the resolved capabilities, and are the complete
//! entry points: each returns whatever its stateless counterpart returns,
//! plus the usage updates only state can produce. [`project_v1`] and
//! [`project_v2`] remain as the pure core, so the great majority of the
//! mapping stays testable without constructing a `SessionState`.
//!
//! The RFD is explicit that an agent must *not* send `UsageUpdate` when the
//! context size is unknown, and that clients must tolerate its absence.
//! Both `used` and `size` are therefore required to be known before anything
//! is emitted; `cost` is genuinely optional and is attached only when the
//! cumulative total is known in a single currency.
//!
//! ## Events with no `SessionUpdate` counterpart in either version
//!
//! - **`PermissionRequested`/`PermissionDecided`** — permissions travel over
//!   `session/request_permission`, a request/response RPC pair the agent
//!   sends to the client, not a `session/update` notification. Handling it
//!   is also the reverse direction (ACP requests → core operations),
//!   explicitly out of scope for this story.
//! - **`Forked`** — `session/fork` is a request/response RPC method (behind
//!   `unstable_session_fork`); no `SessionUpdate` variant announces "this
//!   session was forked" in either protocol version, so there is nothing to
//!   project to regardless of the feature gate.
//! - **`Compacted`**, **`HookFired`**, **`SubagentSpawned`**,
//!   **`SubagentReturned`** — compaction, hooks and subagents are ego
//!   harness internals with no ACP 1.6.0 protocol concept at all.
//! - **`Error`** — a mid-turn error surfaces as the JSON-RPC error response
//!   to the `session/prompt` call, a different message shape (a response's
//!   error field, not a notification). Rendering it as an
//!   `AgentMessageChunk` would misrepresent an RPC-level fact as
//!   conversational content.
//! - **`TurnCompleted`, v1 only** — v1 turn completion rides the
//!   `session/prompt` response's `stop_reason`, never a `session/update`.
//!   (v2 *does* have a mapping — see [`project_v2`]'s `TurnCompleted` arm.)

use agent_client_protocol_schema::{v1, v2};
use ego_core::cost::Money;
use ego_core::event::domain::{DomainEvent, EventKind, ExitReason};
use ego_core::event::id::ItemId;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::state::SessionState;

/// The bare-string wire form ACP ids use (`MessageId`, `ToolCallId`, …).
///
/// `event::id::ItemId` is owned by another story and exposes no accessor for
/// its inner `Uuid`; its `Serialize` impl (a one-field tuple struct with no
/// custom implementation) is what serde calls a newtype, which every
/// serializer — `serde_json` included — renders exactly as the inner value,
/// here a plain JSON string. That is already relied on elsewhere in this
/// workspace (`ego-core/tests/public_golden.rs`'s `id()` helper deserialises
/// a bare UUID literal straight into these newtypes), so encoding through it
/// rather than formatting the `Uuid` by hand keeps this file decoupled from
/// `event::id`'s internal representation.
fn item_id_string(item: ItemId) -> String {
    let value = serde_json::to_value(item).expect("ItemId serialisation is infallible");
    let serde_json::Value::String(s) = value else {
        unreachable!("ItemId always serialises as a bare JSON string (see event::id::ItemId)");
    };
    s
}

/// `ToolCallStarted.args` is documented as "raw (unvalidated) arguments" —
/// usually the JSON object an LLM emitted for a tool call, but never
/// guaranteed well-formed. Preserve structure when it parses; degrade to a
/// JSON string rather than panicking or dropping the value when it does not.
fn tool_raw_input(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// Map ego's turn-outcome vocabulary onto v2's `StopReason`.
///
/// `Completed`/`Cancelled` have exact matches. `Error` and `Budget` do not:
/// ACP's `StopReason` has no "the turn errored" or "a budget limit stopped
/// it" variant, in either protocol version — forcing them into `EndTurn` or
/// `MaxTokens` would misreport why the turn actually stopped (`MaxTokens`
/// specifically means the *model's* token ceiling, not a user-configured
/// cost/token budget). `StopReason::Other` is v2's documented extension
/// point for exactly this case; see the module doc comment for why that is
/// a different, narrower thing than `SessionUpdate::Other`.
fn stop_reason_v2(exit_reason: ExitReason) -> v2::StopReason {
    match exit_reason {
        ExitReason::Completed => v2::StopReason::EndTurn,
        // Every cancellation is `Cancelled` to a host: ACP asks why the turn
        // stopped, and "a human, a deadline or you" is a distinction the
        // protocol has no field for. The cause is not lost — it is in the
        // durable log and in the CLI's exit code; flattening it here is a
        // projection, which is what this module is.
        ExitReason::Cancelled(_) => v2::StopReason::Cancelled,
        ExitReason::Provider => v2::StopReason::Other("_provider".to_string()),
        ExitReason::Internal => v2::StopReason::Other("_internal".to_string()),
        ExitReason::PermissionRequired => v2::StopReason::Other("_permission".to_string()),
        ExitReason::Budget => v2::StopReason::Other("_budget".to_string()),
        ExitReason::Usage => v2::StopReason::Other("_usage".to_string()),
    }
}

/// The `used` and `size` an ACP `UsageUpdate` needs, when both are known.
///
/// `None` whenever either is missing. The ACP RFD says an agent must not
/// send `UsageUpdate` without a context size, and there is no defensible
/// substitute for the other half either: a `used` of zero would claim an
/// empty context, and a `size` guessed from anything but the model's own
/// record would put a real numerator over an invented denominator, which is
/// worse than silence because it looks like information.
fn usage_figures(state_after: &SessionState, caps: &ModelCapabilities) -> Option<(u64, u64)> {
    Some((state_after.context.used_tokens?, caps.max_context_tokens?))
}

/// The cumulative session cost as ACP wants it, when it is known.
///
/// This is the single place where exact money becomes `f64`
/// (`ego_core::cost` keeps everything else exact): ACP's `Cost.amount` is a
/// float, so the conversion has to happen, and confining it to the wire edge
/// means no rounding error can ever be added back into a running total.
fn acp_cost(total: &Money) -> (f64, String) {
    (
        total.amount.to_f64_lossy(),
        total.currency.as_str().to_string(),
    )
}

/// Project one `DomainEvent` onto ACP v1, using the state it produced.
///
/// The complete v1 entry point: everything [`project_v1`] maps, plus the
/// `UsageUpdate` that only reduced state and resolved capabilities can
/// produce. `state_after` must be the reduction *including* `ev` — a caller
/// that passes the state from before the event will report the previous
/// generation's numbers under this one.
///
/// See the module doc comment's "third entry point" section.
pub fn project_acp_v1(
    ev: &DomainEvent,
    state_after: &SessionState,
    caps: &ModelCapabilities,
) -> Option<v1::SessionUpdate> {
    match &ev.kind {
        EventKind::UsageUpdated { .. } | EventKind::ContextWindowUpdated { .. } => {
            let (used, size) = usage_figures(state_after, caps)?;
            let mut update = v1::UsageUpdate::new(used, size);
            if let Some(total) = state_after.cost.known() {
                let (amount, currency) = acp_cost(total);
                update = update.cost(Some(v1::Cost::new(amount, currency)));
            }
            Some(v1::SessionUpdate::UsageUpdate(update))
        }
        _ => project_v1(ev),
    }
}

/// Project one `DomainEvent` onto ACP v2, using the state it produced.
///
/// The v2 twin of [`project_acp_v1`]; `UsageUpdate` is the one variant whose
/// shape is identical in both protocol versions, so the rule is the same and
/// only the types differ.
pub fn project_acp_v2(
    ev: &DomainEvent,
    state_after: &SessionState,
    caps: &ModelCapabilities,
) -> Option<v2::SessionUpdate> {
    match &ev.kind {
        EventKind::UsageUpdated { .. } | EventKind::ContextWindowUpdated { .. } => {
            let (used, size) = usage_figures(state_after, caps)?;
            let mut update = v2::UsageUpdate::new(used, size);
            if let Some(total) = state_after.cost.known() {
                let (amount, currency) = acp_cost(total);
                update = update.cost(Some(v2::Cost::new(amount, currency)));
            }
            Some(v2::SessionUpdate::UsageUpdate(update))
        }
        _ => project_v2(ev),
    }
}

/// Project one internal `DomainEvent` onto ACP protocol v1's `SessionUpdate`.
///
/// See the module doc comment for the events with no v1 counterpart and why.
pub fn project_v1(ev: &DomainEvent) -> Option<v1::SessionUpdate> {
    let item = ev.env.item;

    match &ev.kind {
        // Message content: chunks only in v1. `message_id` is optional on
        // `ContentChunk`, so an item-less event still projects — it just
        // loses chunk-to-chunk continuity, which is strictly better than
        // dropping real content (unlike the tool-call arms below, where a
        // missing item id means there is no id to construct the update
        // with at all).
        EventKind::UserPrompt { text } => Some(v1::SessionUpdate::UserMessageChunk(
            v1::ContentChunk::new(text.clone().into())
                .message_id(item.map(item_id_string).map(v1::MessageId::new)),
        )),
        EventKind::AssistantDelta { text } => Some(v1::SessionUpdate::AgentMessageChunk(
            v1::ContentChunk::new(text.clone().into())
                .message_id(item.map(item_id_string).map(v1::MessageId::new)),
        )),
        // No replace-semantics message primitive in v1. Emitting a chunk
        // here duplicates the text for every message that *did* stream;
        // emitting nothing loses the text for every message that did
        // *not*. Neither is right for both, because the answer depends on
        // what the client already rendered — see the module doc comment's
        // "Known limitation" section, and
        // `acp_golden::v1_loses_a_final_message_that_never_streamed`, which
        // pins this loss deliberately.
        EventKind::AssistantMessage { .. } => None,
        EventKind::ReasoningDelta { text } => Some(v1::SessionUpdate::AgentThoughtChunk(
            v1::ContentChunk::new(text.clone().into())
                .message_id(item.map(item_id_string).map(v1::MessageId::new)),
        )),

        // Tool calls: `tool_call_id` is required, so no item id means no
        // update (mirrors `project_public`'s degenerate-envelope handling).
        EventKind::ToolCallStarted { name, args, .. } => item.map(|item| {
            v1::SessionUpdate::ToolCall(
                v1::ToolCall::new(item_id_string(item), name.clone())
                    .raw_input(tool_raw_input(args)),
            )
        }),
        // `ToolCallUpdateFields.content` replaces the whole collection, it
        // does not append (see module doc comment) — a single increment
        // cannot be represented without the accumulated content so far,
        // which a stateless per-event projection does not have.
        EventKind::ToolCallDelta { .. } => None,
        EventKind::ToolCallCompleted { ok, output } => item.map(|item| {
            v1::SessionUpdate::ToolCallUpdate(v1::ToolCallUpdate::new(
                item_id_string(item),
                v1::ToolCallUpdateFields::new()
                    .status(if *ok {
                        v1::ToolCallStatus::Completed
                    } else {
                        v1::ToolCallStatus::Failed
                    })
                    .content(vec![v1::ToolCallContent::from(output.clone())]),
            ))
        }),

        // See the module doc comment's "no counterpart in either version"
        // list, plus v1's own "no counterpart" note for `TurnCompleted`.
        // `UsageUpdated` and `ContextWindowUpdated` are the two events that
        // need reduced state and resolved capabilities to project — see the
        // module doc comment's "third entry point" section and
        // `project_acp_v1`.
        EventKind::ReasoningState { .. }
        | EventKind::PermissionRequested { .. }
        | EventKind::PermissionDecided { .. }
        | EventKind::UsageUpdated { .. }
        | EventKind::ContextWindowUpdated { .. }
        | EventKind::TurnCompleted { .. }
        | EventKind::Compacted { .. }
        | EventKind::Forked { .. }
        | EventKind::HookFired { .. }
        | EventKind::SubagentSpawned { .. }
        | EventKind::SubagentReturned { .. }
        | EventKind::Error { .. }
        // Internal bookkeeping. ACP describes what the agent did, and which
        // journey to the wire produced a figure is not that; the corrected
        // cumulative total reaches a client through `UsageUpdate` anyway.
        | EventKind::GenerationStarted { .. }
        | EventKind::AttemptSettled { .. }
        | EventKind::UsageCorrected { .. } => None,
    }
}

/// Project one internal `DomainEvent` onto ACP protocol v2's `SessionUpdate`.
///
/// See the module doc comment for the events with no v2 counterpart and why.
pub fn project_v2(ev: &DomainEvent) -> Option<v2::SessionUpdate> {
    let item = ev.env.item;

    match &ev.kind {
        // Unlike v1, every message-shaped `ContentChunk`/`UserMessage`/
        // `AgentMessage`/`AgentThought` in v2 has a *required* `message_id`
        // — so, unlike `project_v1`, an item-less event here has no id to
        // construct the update with and projects to `None`.
        EventKind::UserPrompt { text } => item.map(|item| {
            v2::SessionUpdate::UserMessage(
                v2::UserMessage::new(item_id_string(item)).content(vec![text.clone().into()]),
            )
        }),
        EventKind::AssistantDelta { text } => item.map(|item| {
            v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                text.clone().into(),
                item_id_string(item),
            ))
        }),
        // v2's upserts have explicit patch semantics: "when an
        // `agent_message` update includes `content`, that array replaces
        // any content previously accumulated for the message, including
        // content from earlier chunks." This is exactly the primitive v1
        // lacks (see module doc comment), so — unlike `project_v1` — this
        // is not `None`.
        EventKind::AssistantMessage { text } => item.map(|item| {
            v2::SessionUpdate::AgentMessage(
                v2::AgentMessage::new(item_id_string(item)).content(vec![text.clone().into()]),
            )
        }),
        EventKind::ReasoningDelta { text } => item.map(|item| {
            v2::SessionUpdate::AgentThoughtChunk(v2::ContentChunk::new(
                text.clone().into(),
                item_id_string(item),
            ))
        }),

        EventKind::ToolCallStarted { name, args, .. } => item.map(|item| {
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new(item_id_string(item))
                    .title(name.clone())
                    .status(v2::ToolCallStatus::Pending)
                    .raw_input(tool_raw_input(args)),
            )
        }),
        // v2 has a dedicated append-only primitive v1 lacks (see module doc
        // comment): `ToolCallContentChunk` explicitly appends one
        // `ToolCallContent` item, unlike `ToolCallUpdate.content`'s
        // whole-collection replace. So — unlike `project_v1` — this is not
        // `None`.
        EventKind::ToolCallDelta { text } => item.map(|item| {
            v2::SessionUpdate::ToolCallContentChunk(v2::ToolCallContentChunk::new(
                item_id_string(item),
                text.clone(),
            ))
        }),
        EventKind::ToolCallCompleted { ok, output } => item.map(|item| {
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new(item_id_string(item))
                    .status(if *ok {
                        v2::ToolCallStatus::Completed
                    } else {
                        v2::ToolCallStatus::Failed
                    })
                    .content(vec![output.clone().into()]),
            )
        }),

        // v2 added `StateUpdate` — a `session/update` reporting the agent's
        // foreground-work state independently of the `session/prompt`
        // response, exactly filling the gap v1 has (see v1's own
        // `TurnCompleted` note). `ok` carries no further information beyond
        // what `stop_reason` already conveys (`EndTurn` implies success).
        EventKind::TurnCompleted { exit_reason, .. } => {
            Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new().stop_reason(stop_reason_v2(*exit_reason)),
            )))
        }

        // See the module doc comment's "no counterpart in either version"
        // list, and its "third entry point" section for the two usage
        // events, which need state that is not in the event.
        EventKind::ReasoningState { .. }
        | EventKind::PermissionRequested { .. }
        | EventKind::PermissionDecided { .. }
        | EventKind::UsageUpdated { .. }
        | EventKind::ContextWindowUpdated { .. }
        | EventKind::Compacted { .. }
        | EventKind::Forked { .. }
        | EventKind::HookFired { .. }
        | EventKind::SubagentSpawned { .. }
        | EventKind::SubagentReturned { .. }
        | EventKind::Error { .. }
        // Internal bookkeeping. ACP describes what the agent did, and which
        // journey to the wire produced a figure is not that; the corrected
        // cumulative total reaches a client through `UsageUpdate` anyway.
        | EventKind::GenerationStarted { .. }
        | EventKind::AttemptSettled { .. }
        | EventKind::UsageCorrected { .. } => None,
    }
}
