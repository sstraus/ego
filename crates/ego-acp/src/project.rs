//! Projection of `ego-core`'s public event stream onto ACP `SessionUpdate`
//! values.
//!
//! See `ARCHITECTURE.md` §7. Projection functions only — no transport, no
//! server, no state of our own. The types come from the real
//! `agent-client-protocol-schema` crate rather than hand-rolled mirrors, so a
//! schema bump is a compile error instead of a silently stale golden.
//!
//! ## The input is [`PublicEvent`], not `DomainEvent`
//!
//! It used to be the domain event. The seam decided it: `EventSink` is the one
//! live channel a run publishes through (`sink.rs` — "live is the contract"),
//! and it carries the public projection. An ACP connection that wanted domain
//! events would have needed a second port in `ego-core` whose only consumer is
//! this crate, and two ports fed from one journal are two chances to disagree
//! about what was published and in what order.
//!
//! So the chain is **domain → public → ACP**, each link a pure function. What
//! that costs is stated below rather than discovered.
//!
//! ### It cost the item-less v1 chunk
//!
//! `project_public` maps an event with no item id to `None`, so an item-less
//! event no longer reaches this module at all. The old v1 mapping tolerated
//! one — `ContentChunk.message_id` is optional in v1 — and emitted a chunk
//! with no continuity rather than dropping content.
//!
//! That tolerance is gone, and it is a real narrowing, not a tidy-up. It is
//! acceptable because the public stream is the thing a host is being handed:
//! an event the JSONL consumer never sees is not one the ACP consumer should
//! see either, and two surfaces disagreeing about whether an event happened is
//! worse than both omitting it.
//!
//! ## Two functions, not one with a version flag
//!
//! [`project_v1`] and [`project_v2`] are separate mappings over the same
//! `PublicEvent`: pure, total, stateless `fn(&PublicEvent) -> Option<T>`. v1
//! and v2's `SessionUpdate` enums have genuinely different variant sets — v2
//! added `UserMessage`/`AgentMessage`/`AgentThought` (replace-semantics
//! upserts) and `ToolCallContentChunk` (an append-only primitive) that v1
//! simply does not have. A single function branching on a version enum would
//! smear that difference across every match arm; two thin functions keep each
//! version's shape honest and let a v1-only or v2-only schema change show up
//! as a diff in one function, not both.
//!
//! Which of the two a connection calls is decided by
//! [`negotiate`](crate::negotiate::negotiate), and that is the whole reason
//! the negotiated version must be a decision rather than an echo.
//!
//! ## `Option`, never `Vec`
//!
//! Every mapping below is 1-to-0-or-1: no public event needs more than one
//! `SessionUpdate` to represent it. `Option<SessionUpdate>` says that
//! directly; a `Vec` would imply a cardinality this projection never uses.
//!
//! ## The v1/v2 asymmetry principle
//!
//! Several events are genuinely unrepresentable *by a stateless projection* in
//! v1 but representable in v2, for one recurring reason: v1's `SessionUpdate`
//! variants are either pure-append (chunks) or pure-replace
//! (`ToolCallUpdate.content`), with no way to say "this is the final,
//! authoritative value, discard what streamed before" for a *message*, and no
//! way to *append* to tool-call content at all. v2 added exactly these two
//! missing primitives.
//!
//! Where that happens the v1 mapping is `None`, documented at the match arm.
//! `None` is *not* a claim that nothing was lost — see the known limitation
//! below. It is the choice not to guess, because the nearest existing v1
//! variant is wrong in one direction or the other depending on history this
//! function cannot see.
//!
//! ## Known limitation: v1 loses a non-streamed final message
//!
//! `ItemCompleted { kind: AgentMessage }` is ego's *authoritative* message
//! text — the reducer assigns it rather than appending it, exactly matching
//! v2's `AgentMessage` patch semantics. v1 has no replace primitive for
//! messages at all: every message-shaped v1 variant is a `*Chunk`, and chunks
//! only ever append.
//!
//! So the correct v1 output depends on what the receiving client has *already
//! rendered* for that message id:
//!
//! - it has already rendered the streamed deltas ⇒ append nothing;
//! - it has rendered nothing (the message never streamed, or the client
//!   attached via `session/resume`, which the schema documents as resuming
//!   "without returning previous messages") ⇒ append the full text.
//!
//! A stateless function returns one fixed value, so it cannot satisfy both.
//! Both branches are wrong somewhere, and the choice is which way to be wrong:
//!
//! - **always emit a chunk** — the fully-streamed turn renders as
//!   `"Sure, one sec" + "Sure, one sec"`. v1 receivers concatenate arriving
//!   chunks, and no de-duplication is possible in general (legitimate text
//!   repeats — `"ha"` + `"ha"`). Corrupts the *common* case to rescue the rare
//!   one.
//! - **emit nothing** (what this does) — a message that never streamed is lost
//!   from the v1 transcript entirely.
//!
//! This is a real, accepted limitation, pinned by a test named for it rather
//! than left as an unremarked absence in the golden. It is not fixable inside
//! this module: correctness depends on what a given *client view* received,
//! and only the connection knows that. `ARCHITECTURE.md` §7 already classes
//! ACP adapters as lossy projections.
//!
//! `ItemDelta { kind: Tool }`'s v1 `None` is the same shape of limitation: v1
//! has no append primitive for tool-call content, and `ToolCallUpdate.content`
//! replaces the whole collection, so emitting one increment needs the
//! accumulated content a stateless projection does not have.
//!
//! ## `SessionUpdate::Other` is never produced
//!
//! v2's `SessionUpdate::Other` (an untagged escape hatch preserving an
//! unrecognised `sessionUpdate` discriminator and its raw fields) exists for
//! *receivers* tolerating updates from a newer protocol revision or a
//! genuinely custom extension they don't understand. We are always a
//! *producer* mapping a fixed, exhaustively-matched enum onto a fixed schema
//! version; we never have an "unknown" update to preserve, so there is nothing
//! for `Other` to do on this side.
//!
//! `v2::StopReason::Other`, by contrast, *is* produced (see [`stop_reason_v2`]):
//! it is a narrowly-scoped, spec-sanctioned extension point on a single field
//! (with the documented `_`-prefix convention for implementation-specific
//! values), not the top-level "unknown update" escape hatch, and it is the only
//! honest way to report the exit reasons ACP's `StopReason` has no first-class
//! equivalent for — forcing them into `EndTurn` or `MaxTokens` would misreport
//! why the turn actually stopped.
//!
//! ## The third entry point: `project_acp_v1` / `project_acp_v2`
//!
//! `SessionUpdate::UsageUpdate` cannot be produced by a stateless function,
//! and it is the only variant that cannot:
//!
//! - its `used` is tokens *currently in context*, a level a compaction lowers
//!   — not derivable from any event's payload, and emphatically not from
//!   cumulative token totals, which recount prior history as input on every
//!   call and so diverge from occupancy from the second generation onward;
//! - its `cost` is the *cumulative session* cost, so even a per-generation
//!   cost delta has to be summed over everything before it;
//! - its `size` is the context window of the generation that reported the
//!   occupancy, which the reduction learns from the occupancy record itself
//!   and only falls back to `ModelCapabilities` for. The two are not the same
//!   window: a turn resolves a fresh profile per fallback candidate, so the
//!   caller's resolved capabilities describe the model it *intended* to use.
//!
//! So [`project_acp_v1`] and [`project_acp_v2`] take the reduced state *after*
//! the event and the resolved capabilities, and are the complete entry points.
//! [`project_v1`] and [`project_v2`] remain as the pure core, so the great
//! majority of the mapping stays testable without constructing a
//! `SessionState`.
//!
//! The ACP RFD is explicit that an agent must *not* send `UsageUpdate` when the
//! context size is unknown, and that clients must tolerate its absence. Both
//! `used` and `size` are therefore required to be known before anything is
//! emitted; `cost` is genuinely optional and is attached only when the
//! cumulative total is known in a single currency.
//!
//! ## Events with no `SessionUpdate` counterpart in either version
//!
//! - **`SessionStarted`** — the session's existence is established by
//!   `session/new` or `session/load`, a request/response pair, before any
//!   update can be sent. An update announcing it would arrive after the fact.
//! - **`TurnStarted`** — v1 has no turn concept in `session/update` at all,
//!   and v2's `StateUpdate` reports the agent's *working* state, which
//!   `session/prompt` being in flight already conveys.
//! - **`MeteringSelected`** — how a generation is billed is ego harness
//!   knowledge with no ACP concept.
//! - **Permission items** — permissions travel over
//!   `session/request_permission`, a request/response RPC pair the agent sends
//!   the client, not a `session/update` notification.
//! - **Question items** — questions travel over unstable form elicitation, a
//!   request/response RPC pair scoped to the session that asked, not as
//!   transcript chunks or tool-call updates. The durable question item stays
//!   inside ego: the answer arrives as the response to the very request that
//!   asked, so nothing is correlated by an id a host would have to hold.
//! - **Context checkpoints** — ACP has no checkpoint-author role or standard
//!   session-transition update. Omitting one preserves its low-authority
//!   context semantics instead of misreporting it as human or agent speech.
//! - **`Subagent` items** — no ACP 1.5.0 protocol concept.
//! - **`Error`** — a mid-turn error surfaces as the JSON-RPC error response to
//!   the `session/prompt` call, a different message shape (a response's error
//!   field, not a notification). Rendering it as an `AgentMessageChunk` would
//!   misrepresent an RPC-level fact as conversational content.
//! - **`TurnCompleted`, v1 only** — v1 turn completion rides the
//!   `session/prompt` response's `stop_reason`, never a `session/update`.
//!   (v2 *does* have a mapping — see [`project_v2`]'s arm.)

use agent_client_protocol_schema::{v1, v2};
use ego_core::cost::Money;
use ego_core::event::domain::ExitReason;
use ego_core::event::id::ItemId;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::public::{ItemKind, PublicEvent};
use ego_core::run::{DeniedBy, RunTermination};
use ego_core::session_lifecycle::ReplayFrame;
use ego_core::state::SessionState;

/// The bare-string wire form ACP ids use (`MessageId`, `ToolCallId`, …).
///
/// `event::id::ItemId` exposes no accessor for its inner `Uuid`; its
/// `Serialize` impl (a one-field tuple struct with no custom implementation)
/// is what serde calls a newtype, which every serializer — `serde_json`
/// included — renders exactly as the inner value, here a plain JSON string.
/// That is already relied on elsewhere in this workspace
/// (`ego-core/tests/public_golden.rs`'s `id()` helper deserialises a bare UUID
/// literal straight into these newtypes), so encoding through it rather than
/// formatting the `Uuid` by hand keeps this file decoupled from `event::id`'s
/// internal representation.
fn item_id_string(item: ItemId) -> String {
    let value = serde_json::to_value(item).expect("ItemId serialisation is infallible");
    let serde_json::Value::String(s) = value else {
        unreachable!("ItemId always serialises as a bare JSON string (see event::id::ItemId)");
    };
    s
}

/// A tool call's starting arguments are documented as raw (unvalidated) — the
/// JSON object an LLM emitted, never guaranteed well-formed. Preserve
/// structure when it parses; degrade to a JSON string rather than panicking or
/// dropping the value when it does not.
fn tool_raw_input(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// Map ego's turn-outcome vocabulary onto v2's `StopReason`.
///
/// `Completed`/`Cancelled` have exact matches. The rest do not: ACP's
/// `StopReason` has no "the turn errored" or "a budget limit stopped it"
/// variant, in either protocol version — forcing them into `EndTurn` or
/// `MaxTokens` would misreport why the turn actually stopped (`MaxTokens`
/// specifically means the *model's* token ceiling, not a user-configured
/// cost/token budget). `StopReason::Other` is v2's documented extension point
/// for exactly this case; see the module doc comment for why that is a
/// different, narrower thing than `SessionUpdate::Other`.
fn stop_reason_v2(exit_reason: ExitReason) -> v2::StopReason {
    match exit_reason {
        ExitReason::Completed => v2::StopReason::EndTurn,
        // Every cancellation is `Cancelled` to a host: ACP asks why the turn
        // stopped, and "a human, a deadline or you" is a distinction the
        // protocol has no field for. The cause is not lost — it is in the
        // durable log and in the CLI's exit code; flattening it here is a
        // projection, which is what this module is.
        ExitReason::Cancelled(_) => v2::StopReason::Cancelled,
        // Deliberately *not* folded into `Cancelled` with the causes above: a
        // cancelled turn is over and a paused one is waiting, and a host that
        // read them as the same word would either offer no way to resume or
        // offer one that cannot work. See
        // `docs/decisions/2026-09-03-acp-host-control-is-negotiated-per-session.md`.
        ExitReason::Paused => v2::StopReason::Other("_paused".to_string()),
        ExitReason::Provider => v2::StopReason::Other("_provider".to_string()),
        ExitReason::ContextOverflow => v2::StopReason::Other("_context_overflow".to_string()),
        ExitReason::Internal => v2::StopReason::Other("_internal".to_string()),
        ExitReason::PermissionRequired => v2::StopReason::Other("_permission".to_string()),
        ExitReason::Budget => v2::StopReason::Other("_budget".to_string()),
        ExitReason::Usage => v2::StopReason::Other("_usage".to_string()),
    }
}

/// Why the `session/prompt` call stops, in v1's vocabulary — or the sentence to
/// fail it with when v1 has no word for it.
///
/// v1's `StopReason` has five values and no extension point (v2 added
/// `Other`, which is why [`stop_reason_v2`] can be total). Three of ego's
/// endings have no honest v1 spelling, and the two candidates are both worse
/// than an error:
///
/// - `MaxTokens` means the *model's* token ceiling. A budget ceiling is a
///   figure an operator set, and reporting it as `MaxTokens` would tell a host
///   the model ran out of room. [`stop_reason_v2`] rejects the same conflation.
/// - `Refusal` is documented as discarding history: "the user prompt and
///   everything that comes after it won't be included in the next prompt". A
///   budget stop, a permission stop and a failed schema all leave the session
///   intact and resumable, so `Refusal` would instruct the client to throw away
///   a transcript that is still good.
///
/// So they become a JSON-RPC error on the `session/prompt` response, which is
/// where `project.rs`'s module doc already puts a mid-turn error, and the
/// sentence travels with it. `UsageError`, `Provider` and `Failed` are errors
/// on their own terms — the run produced no turn, or broke.
pub fn stop_reason_v1(termination: &RunTermination) -> Result<v1::StopReason, String> {
    match termination {
        RunTermination::Done => Ok(v1::StopReason::EndTurn),
        RunTermination::MaxTurns => Ok(v1::StopReason::MaxTurnRequests),
        // Every cancellation is `Cancelled` to a host, for the reason
        // `stop_reason_v2` gives: ACP asks why the turn stopped, and "a human,
        // a deadline or your own `session/cancel`" is a distinction the
        // protocol has no field for.
        RunTermination::TimedOut | RunTermination::Signalled(_) => Ok(v1::StopReason::Cancelled),
        RunTermination::BudgetExceeded => {
            Err("the turn stopped because a budget ceiling was reached".to_string())
        }
        // An error and not `Cancelled`, for the reason `stop_reason_v2` gives:
        // v1 has no word that distinguishes "over" from "waiting", and the
        // only useful thing to say is the one call that continues it.
        RunTermination::Paused => {
            Err("the turn stopped at a pause boundary; continue it with `_ego/resume`".to_string())
        }
        RunTermination::QuestionRequired { question } => Err(format!(
            "question {} still requires an answer; resume it with `ego resume --answer {}=JSON`",
            question.as_ref().id,
            question.as_ref().id
        )),
        // Split for the reason the line UI splits it (`ego-cli`'s
        // `termination_note`): "nobody could grant it" describes a question
        // the evaluation order exhausted, and said of a refusal somebody made
        // it denies that anyone decided — the one thing that did happen. The
        // authority is not named here and is not named there either, because
        // `action`'s own rendering already carries the rule, the mode or the
        // boundary that produced it.
        RunTermination::PermissionRequired {
            action,
            by: DeniedBy::Decided(_),
        } => Err(format!(
            "the turn stopped because {action} was refused by a permission decision"
        )),
        RunTermination::PermissionRequired {
            action,
            by: DeniedBy::Nobody,
        } => Err(format!(
            "the turn stopped because {action} needed a permission nobody could grant"
        )),
        RunTermination::TaskFailed => Err("the turn ran and the task did not succeed".to_string()),
        RunTermination::UsageError { message }
        | RunTermination::Provider { message }
        | RunTermination::ContextOverflow { message }
        | RunTermination::Failed { message } => Err(message.clone()),
    }
}

/// The `used` and `size` an ACP `UsageUpdate` needs, when both are known.
///
/// `None` whenever either is missing. The ACP RFD says an agent must not send
/// `UsageUpdate` without a context size, and there is no defensible substitute
/// for the other half either: a `used` of zero would claim an empty context,
/// and a `size` guessed from anything but the model's own record would put a
/// real numerator over an invented denominator, which is worse than silence
/// because it looks like information.
///
/// The window the reduction carries wins over `caps` when it has one, because
/// it is the window the generation that reported this occupancy actually ran
/// under. `caps` is what the caller resolved before the turn began, and a turn
/// that fell back to another model no longer runs under it. `caps` is still the
/// answer while no generation has stated a window — a fresh session that has
/// somehow reported occupancy first, and every record written before the
/// occupancy event carried a denominator at all.
fn usage_figures(state_after: &SessionState, caps: &ModelCapabilities) -> Option<(u64, u64)> {
    let size = state_after.context.window.or(caps.max_context_tokens)?;
    Some((state_after.context.used_tokens?, size))
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

/// Whether this event is one only reduced state can answer.
///
/// Both usage-shaped public events land here, and they are the only two: see
/// the module doc comment's "third entry point" section.
fn needs_state(event: &PublicEvent) -> bool {
    matches!(
        event,
        PublicEvent::UsageUpdated { .. } | PublicEvent::ContextWindowUpdated { .. }
    )
}

/// Project one `PublicEvent` onto ACP v1, using the state it produced.
///
/// The complete v1 entry point: everything [`project_v1`] maps, plus the
/// `UsageUpdate` that only reduced state and resolved capabilities can
/// produce. `state_after` must be the reduction *including* this event — a
/// caller that passes the state from before it will report the previous
/// generation's numbers under this one.
pub fn project_acp_v1(
    event: &PublicEvent,
    state_after: &SessionState,
    caps: &ModelCapabilities,
) -> Option<v1::SessionUpdate> {
    if !needs_state(event) {
        return project_v1(event);
    }
    let (used, size) = usage_figures(state_after, caps)?;
    let mut update = v1::UsageUpdate::new(used, size);
    if let Some(total) = state_after.cost.known() {
        let (amount, currency) = acp_cost(total);
        update = update.cost(Some(v1::Cost::new(amount, currency)));
    }
    Some(v1::SessionUpdate::UsageUpdate(update))
}

/// Project ordered durable history for `session/load`.
///
/// Stateless updates retain their event order. Usage-shaped history is reduced
/// to at most one final update because only the final durable state and the
/// latest admitted model's durable context window describe the session being
/// attached now.
pub fn project_replay_v1(
    frames: &[ReplayFrame],
    context_window: Option<u64>,
) -> Vec<v1::SessionUpdate> {
    let mut updates = frames
        .iter()
        .filter_map(|frame| project_v1(&frame.event))
        .collect::<Vec<_>>();
    let Some(state) = frames.last().map(|frame| &frame.state) else {
        return updates;
    };
    if let (Some(used), Some(size)) = (state.context.used_tokens, context_window) {
        let mut usage = v1::UsageUpdate::new(used, size);
        if let Some(total) = state.cost.known() {
            let (amount, currency) = acp_cost(total);
            usage = usage.cost(Some(v1::Cost::new(amount, currency)));
        }
        updates.push(v1::SessionUpdate::UsageUpdate(usage));
    }
    updates
}

/// Project one `PublicEvent` onto ACP v2, using the state it produced.
///
/// The v2 twin of [`project_acp_v1`]; `UsageUpdate` is the one variant whose
/// shape is identical in both protocol versions, so the rule is the same and
/// only the types differ.
pub fn project_acp_v2(
    event: &PublicEvent,
    state_after: &SessionState,
    caps: &ModelCapabilities,
) -> Option<v2::SessionUpdate> {
    if !needs_state(event) {
        return project_v2(event);
    }
    let (used, size) = usage_figures(state_after, caps)?;
    let mut update = v2::UsageUpdate::new(used, size);
    if let Some(total) = state_after.cost.known() {
        let (amount, currency) = acp_cost(total);
        update = update.cost(Some(v2::Cost::new(amount, currency)));
    }
    Some(v2::SessionUpdate::UsageUpdate(update))
}

/// Project one `PublicEvent` onto ACP protocol v1's `SessionUpdate`.
///
/// See the module doc comment for the events with no v1 counterpart and why.
pub fn project_v1(event: &PublicEvent) -> Option<v1::SessionUpdate> {
    match event {
        PublicEvent::ItemStarted {
            item,
            kind,
            name,
            text,
            ..
        } => match kind {
            // A tool call with no name has no `ToolCall` to build: v1's
            // constructor requires one, and inventing a placeholder would put
            // a tool in the client's transcript under a name nothing called.
            ItemKind::Tool => Some(v1::SessionUpdate::ToolCall(
                v1::ToolCall::new(item_id_string(*item), name.clone()?)
                    .raw_input(tool_raw_input(text.as_deref().unwrap_or(""))),
            )),
            // Permissions travel over `session/request_permission`, and
            // subagents have no ACP concept — module doc comment. Messages and
            // reasoning never arrive as `ItemStarted`; that they are listed
            // rather than swept into a wildcard is what makes a new item kind
            // a compile error here instead of a silent `None`.
            ItemKind::UserMessage
            | ItemKind::AgentMessage
            | ItemKind::Reasoning
            | ItemKind::Permission
            | ItemKind::Subagent
            | ItemKind::ContextCheckpoint => None,
            ItemKind::Question => None,
        },

        PublicEvent::ItemDelta {
            item, kind, text, ..
        } => match kind {
            ItemKind::AgentMessage => Some(v1::SessionUpdate::AgentMessageChunk(
                v1::ContentChunk::new(text.clone().into())
                    .message_id(Some(v1::MessageId::new(item_id_string(*item)))),
            )),
            ItemKind::Reasoning => Some(v1::SessionUpdate::AgentThoughtChunk(
                v1::ContentChunk::new(text.clone().into())
                    .message_id(Some(v1::MessageId::new(item_id_string(*item)))),
            )),
            // `ToolCallUpdateFields.content` replaces the whole collection, it
            // does not append (see module doc comment) — a single increment
            // cannot be represented without the accumulated content so far,
            // which a stateless per-event projection does not have.
            ItemKind::Tool => None,
            ItemKind::UserMessage
            | ItemKind::Permission
            | ItemKind::Subagent
            | ItemKind::ContextCheckpoint => None,
            ItemKind::Question => None,
        },

        PublicEvent::ItemCompleted {
            item,
            kind,
            ok,
            text,
            ..
        } => match kind {
            // The caller's own words, echoed back to the client that sent
            // them. v1 has no replace primitive for messages, so this is a
            // chunk — and unlike the agent's, that is not a loss: a user
            // message never streams, so nothing was rendered before it.
            ItemKind::UserMessage => Some(v1::SessionUpdate::UserMessageChunk(
                v1::ContentChunk::new(text.clone()?.into())
                    .message_id(Some(v1::MessageId::new(item_id_string(*item)))),
            )),
            // No replace-semantics message primitive in v1. A chunk here
            // duplicates the text for every message that *did* stream; nothing
            // loses the text for every message that did *not*. Neither is
            // right for both, because the answer depends on what the client
            // already rendered — see the module doc comment's "Known
            // limitation", and the test that pins this loss deliberately.
            ItemKind::AgentMessage => None,
            ItemKind::Tool => Some(v1::SessionUpdate::ToolCallUpdate(v1::ToolCallUpdate::new(
                item_id_string(*item),
                v1::ToolCallUpdateFields::new()
                    .status(tool_status_v1(*ok))
                    .content(vec![v1::ToolCallContent::from(
                        text.clone().unwrap_or_default(),
                    )]),
            ))),
            ItemKind::Reasoning
            | ItemKind::Permission
            | ItemKind::Subagent
            | ItemKind::ContextCheckpoint => None,
            ItemKind::Question => None,
        },

        // See the module doc comment's "no counterpart in either version"
        // list, plus v1's own note for `TurnCompleted`. The two usage-shaped
        // events need reduced state and are handled by `project_acp_v1`.
        PublicEvent::SessionStarted { .. }
        | PublicEvent::TurnStarted { .. }
        | PublicEvent::MeteringSelected { .. }
        | PublicEvent::FallbackDecision { .. }
        | PublicEvent::TurnCompleted { .. }
        | PublicEvent::UsageUpdated { .. }
        | PublicEvent::ContextWindowUpdated { .. }
        | PublicEvent::Error { .. } => None,
    }
}

/// Project one `PublicEvent` onto ACP protocol v2's `SessionUpdate`.
///
/// See the module doc comment for the events with no v2 counterpart and why.
pub fn project_v2(event: &PublicEvent) -> Option<v2::SessionUpdate> {
    match event {
        PublicEvent::ItemStarted {
            item,
            kind,
            name,
            text,
            ..
        } => match kind {
            ItemKind::Tool => Some(v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new(item_id_string(*item))
                    .title(name.clone()?)
                    .status(v2::ToolCallStatus::Pending)
                    .raw_input(tool_raw_input(text.as_deref().unwrap_or(""))),
            )),
            ItemKind::UserMessage
            | ItemKind::AgentMessage
            | ItemKind::Reasoning
            | ItemKind::Permission
            | ItemKind::Subagent
            | ItemKind::ContextCheckpoint => None,
            ItemKind::Question => None,
        },

        PublicEvent::ItemDelta {
            item, kind, text, ..
        } => match kind {
            ItemKind::AgentMessage => Some(v2::SessionUpdate::AgentMessageChunk(
                v2::ContentChunk::new(text.clone().into(), item_id_string(*item)),
            )),
            ItemKind::Reasoning => Some(v2::SessionUpdate::AgentThoughtChunk(
                v2::ContentChunk::new(text.clone().into(), item_id_string(*item)),
            )),
            // v2 has a dedicated append-only primitive v1 lacks:
            // `ToolCallContentChunk` explicitly appends one
            // `ToolCallContent`, unlike `ToolCallUpdate.content`'s
            // whole-collection replace. So — unlike `project_v1` — this is
            // not `None`.
            ItemKind::Tool => Some(v2::SessionUpdate::ToolCallContentChunk(
                v2::ToolCallContentChunk::new(item_id_string(*item), text.clone()),
            )),
            ItemKind::UserMessage
            | ItemKind::Permission
            | ItemKind::Subagent
            | ItemKind::ContextCheckpoint => None,
            ItemKind::Question => None,
        },

        PublicEvent::ItemCompleted {
            item,
            kind,
            ok,
            text,
            ..
        } => match kind {
            // v2 has a first-class user message with replace semantics, so
            // unlike v1 this is an upsert rather than a chunk.
            ItemKind::UserMessage => Some(v2::SessionUpdate::UserMessage(
                v2::UserMessage::new(item_id_string(*item)).content(vec![text.clone()?.into()]),
            )),
            // v2's upserts have explicit patch semantics: "when an
            // `agent_message` update includes `content`, that array replaces
            // any content previously accumulated for the message, including
            // content from earlier chunks." This is exactly the primitive v1
            // lacks, so — unlike `project_v1` — this is not `None`.
            ItemKind::AgentMessage => Some(v2::SessionUpdate::AgentMessage(
                v2::AgentMessage::new(item_id_string(*item)).content(vec![text.clone()?.into()]),
            )),
            ItemKind::Tool => Some(v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new(item_id_string(*item))
                    .status(tool_status_v2(*ok))
                    .content(vec![text.clone().unwrap_or_default().into()]),
            )),
            ItemKind::Reasoning
            | ItemKind::Permission
            | ItemKind::Subagent
            | ItemKind::ContextCheckpoint => None,
            ItemKind::Question => None,
        },

        // v2 added `StateUpdate` — a `session/update` reporting the agent's
        // foreground-work state independently of the `session/prompt`
        // response, exactly filling the gap v1 has. `ok` carries no further
        // information beyond what `stop_reason` already conveys (`EndTurn`
        // implies success).
        PublicEvent::TurnCompleted { exit_reason, .. } => {
            Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new().stop_reason(stop_reason_v2(*exit_reason)),
            )))
        }

        // See the module doc comment's "no counterpart in either version"
        // list. The two usage-shaped events need reduced state and are
        // handled by `project_acp_v2`.
        PublicEvent::SessionStarted { .. }
        | PublicEvent::TurnStarted { .. }
        | PublicEvent::MeteringSelected { .. }
        | PublicEvent::FallbackDecision { .. }
        | PublicEvent::UsageUpdated { .. }
        | PublicEvent::ContextWindowUpdated { .. }
        | PublicEvent::Error { .. } => None,
    }
}

/// One outcome flag, two protocol spellings — written once so the two
/// projections cannot disagree about which boolean means failed.
fn tool_status_v1(ok: bool) -> v1::ToolCallStatus {
    match ok {
        true => v1::ToolCallStatus::Completed,
        false => v1::ToolCallStatus::Failed,
    }
}

fn tool_status_v2(ok: bool) -> v2::ToolCallStatus {
    match ok {
        true => v2::ToolCallStatus::Completed,
        false => v2::ToolCallStatus::Failed,
    }
}
