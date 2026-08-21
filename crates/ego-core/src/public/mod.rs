//! `PublicEvent`: the privacy-filtered, compatibility-governed projection of
//! the internal event log.
//!
//! See `ARCHITECTURE.md` §1 (four serialisations, one semantic model) and
//! `CLI.md` §4.2. A new internal event never becomes public automatically.

use serde::{Deserialize, Serialize};

use crate::budget::{Ledger, Metering};
use crate::cost::{CostDelta, Money, RateSource};
use crate::event::domain::{DomainEvent, EventKind, ExitReason, OccupancyProvenance};
use crate::event::id::{ItemId, SessionId, TurnId};
use crate::state::Usage;

/// The current `PublicEvent` schema version, carried in `SessionStarted`'s
/// `v` field. `CLI.md` §4.2: "the schema is versioned in the header and
/// published".
///
/// **Incremented when the shape moves, not when the branch merges.** This
/// went 5 → 6 for `UsageUpdated`'s `usage_delta`, which embeds [`Usage`] and
/// so gained `cache_write_extended_tokens` and `reasoning_tokens`. A version
/// left behind is a claim a consumer acts on; a version raised early is only
/// an integer pending confirmation, and the two are not equally wrong.
///
/// **The integer is re-derived at merge, never carried from here.** Three
/// branches move this constant — metering, permissions and this one — so the
/// number above is arithmetic on the base this branch forked from, not a
/// measurement of the merged tree. At merge: read the constant on the merge
/// base and add one. A value that is not what you expect means another branch
/// moved it too, and is a reason to stop and look rather than to overwrite.
pub const PUBLIC_EVENT_VERSION: u32 = 6;

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

/// How a generation is metered, as the public stream spells it.
///
/// The three ways ego is paid for, and the reason a monetary budget binds
/// only the first: the other two have no per-token currency charge to
/// reserve against. Deliberately not carrying the rates — a public event
/// says how a run is billed, not what a provider's table contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeteringKind {
    /// Metered in money, per token. A monetary budget binds.
    Currency,
    /// Paid for by a flat fee. No per-token currency charge exists.
    Subscription,
    /// Nobody is charging — a daemon serving its own weights.
    Unmetered,
}

/// What the configured ceiling actually does to a generation.
///
/// Three values and not an `Option<Money>`, because a flag that *appears* to
/// bind and does not is a false statement, and the two ways it fails to bind
/// are different: nobody set one, or one was set against a row no monetary
/// ceiling can reach. See `2026-08-21-a-run-with-no-ceiling.md` §10.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum CeilingEffect {
    /// No ceiling was configured, so nothing binds and no rates were looked
    /// up. The ordinary case: ego imposes no ceiling of its own (§4).
    None,
    /// A ceiling is in force at `limit`.
    Binds {
        limit: Money,
        /// The higher figure that was asked for, when an administrator's
        /// non-relaxable ceiling reduced it. §4 requires the clamp be
        /// **stated, not silent** — otherwise the flag reads as binding at a
        /// figure it does not.
        #[serde(skip_serializing_if = "Option::is_none")]
        clamped_from: Option<Money>,
    },
    /// A monetary ceiling was configured against a row no monetary ceiling
    /// can bind: every reservation on a flat-fee row is zero, so the gate
    /// admits until the provider refuses.
    Inapplicable {
        limit: Money,
        metering: MeteringKind,
    },
}

/// The privacy-filtered, versioned JSONL wire event — `CLI.md` §4.2's
/// three-level envelope: `session.started` / `turn.started|completed` /
/// `item.started|delta|completed`.
///
/// `SessionStarted`, `TurnStarted` and `MeteringSelected` have no
/// `DomainEvent` counterpart: nothing in `EventKind` (ARCH §1's exhaustive
/// internal set) marks the start of a session or a turn, or the choice of a
/// metering, because none of those is a fact the *reducer* establishes —
/// they are facts the harness that drives a session already knows, before
/// any domain event exists. So the harness constructs those three variants
/// directly; `project_public` below only ever returns the others, which do
/// each originate from a `DomainEvent`.
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
    /// How the generation about to run is metered.
    ///
    /// Emitted **per generation**, not once per session, because a fallback
    /// chain can switch rows mid-turn — the same argument
    /// [`TurnCompleted`](Self::TurnCompleted)'s doc already makes for keeping
    /// usage off a turn-level event.
    ///
    /// It exists because a monetary `--budget` bounds nothing on a row that
    /// is not metered in currency: every reservation there is zero, so the
    /// gate admits until the provider refuses. The operator asked for a
    /// ceiling and has to be told it does not bind — a silently vacuous flag
    /// is not an acceptable way to deliver that. The *token* form of
    /// `--budget` is unaffected and constrains such a row exactly as it
    /// constrains any other, which is what this event points an operator
    /// towards.
    ///
    /// Deliberately **not** a `SessionStarted.capabilities` entry: that field
    /// is feature detection by name, answered before any model is resolved,
    /// and how a particular generation is metered is neither a feature of the
    /// harness nor known at session start.
    MeteringSelected {
        turn: TurnId,
        /// Whether a monetary budget can bind this generation at all:
        /// [`MeteringKind::Currency`] binds, the other two do not.
        metering: MeteringKind,
        /// What the configured ceiling does to this generation.
        ceiling: CeilingEffect,
        /// What the ceiling rests on, when it rests on anything.
        rates: RateSource,
    },
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

impl PublicEvent {
    /// Announce how the generation about to run is metered.
    ///
    /// The mapping from [`Metering`] lives here rather than at the one call
    /// site in the turn loop, so a second caller cannot spell it differently:
    /// this event is the only thing that tells an operator their monetary
    /// ceiling does not bind, and a version of it that said otherwise would
    /// be worse than no event at all.
    /// `ceiling` is derived from the gate's own [`Ledger`] rather than from
    /// whatever the caller believes it configured, so the event cannot claim
    /// a ceiling the gate is not holding, nor report an unclamped figure
    /// while the gate holds a clamped one.
    pub fn metering_selected(
        turn: TurnId,
        metering: &Metering,
        ledger: &Ledger,
        rates: &RateSource,
    ) -> Self {
        let kind = match metering {
            Metering::Currency { .. } => MeteringKind::Currency,
            Metering::Subscription { .. } => MeteringKind::Subscription,
            Metering::Unmetered { .. } => MeteringKind::Unmetered,
        };
        let ceiling = match ledger {
            Ledger::Unlimited => CeilingEffect::None,
            // A monetary ceiling cannot reach a row with no per-token
            // currency charge: the metering mints a zero reservation, so the
            // limit admits everything. Said outright rather than left for the
            // operator to infer from a limit that never fires.
            Ledger::Limited { limit, .. } if kind != MeteringKind::Currency => {
                CeilingEffect::Inapplicable {
                    limit: *limit,
                    metering: kind,
                }
            }
            Ledger::Limited {
                limit,
                clamped_from,
                ..
            } => CeilingEffect::Binds {
                limit: *limit,
                clamped_from: *clamped_from,
            },
        };
        PublicEvent::MeteringSelected {
            turn,
            metering: kind,
            ceiling,
            rates: rates.clone(),
        }
    }
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
        // Never public, and this is a privacy decision rather than a
        // tidiness one. The blob is provider-private and opaque — ego does
        // not know what is encoded in it, which is precisely why it cannot
        // publish it. A consumer that needs to know the model reasoned has
        // the reasoning item and its deltas; nothing downstream can act on
        // the continuation token, and `CLI.md` §4.2's rule is that a new
        // internal event never becomes public automatically.
        EventKind::ReasoningState { .. } => None,
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
        // INCOMPLETE: `kind` is dropped here because `PublicEvent::Error`
        // has no field to hold it. Move 10 of the AiEvent ADR adds one; until
        // then a consumer cannot tell a transient failure from a refusal, and
        // the projection says so by omission rather than by guessing.
        EventKind::Error { message, .. } => Some(PublicEvent::Error {
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
