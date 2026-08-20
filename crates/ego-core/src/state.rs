//! `SessionState` and the pure, sync reducer.
//!
//! See `ARCHITECTURE.md` §1: state is a reduction over an append-only event
//! log, and compaction and forking are events *in* the log rather than
//! operations *on* it.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::event::domain::{DomainEvent, EventKind, ExitReason};
use crate::event::id::{EventId, ItemId, Seq, SessionId, TurnId};

/// Running usage totals, accumulated from `UsageUpdated` events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    /// Add incremental usage to the running totals.
    ///
    /// Saturates rather than panicking or wrapping: `apply` must be total,
    /// and this totals field is later read by budget gates, so a silent
    /// wrap to near-zero (`wrapping_add`) would corrupt spend accounting —
    /// worse than reporting a pinned-at-max total from a degenerate or
    /// hostile log (this is data read back from disk, not just a
    /// theoretical edge).
    fn add(&mut self, input_tokens: u64, output_tokens: u64) {
        self.input_tokens = self.input_tokens.saturating_add(input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(output_tokens);
    }
}

/// Per-turn state: whether, and how, it completed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnState {
    pub completed: bool,
    pub ok: bool,
    pub exit_reason: Option<ExitReason>,
}

/// Per-item state: accumulated text for messages, reasoning and tool calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    pub turn: TurnId,
    pub tool_name: Option<String>,
    pub text: String,
    pub completed: bool,
    pub ok: Option<bool>,
}

impl ItemState {
    fn new(turn: TurnId) -> Self {
        Self {
            turn,
            tool_name: None,
            text: String::new(),
            completed: false,
            ok: None,
        }
    }
}

/// A permission request or decision, in application order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionEvent {
    Requested {
        item: Option<ItemId>,
        description: String,
    },
    Decided {
        item: Option<ItemId>,
        allowed: bool,
    },
}

/// A hook firing, in application order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRecord {
    pub name: String,
    pub blocked: bool,
}

/// A subagent lifecycle event, in application order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentEvent {
    Spawned { subagent_id: String },
    Returned { subagent_id: String, result: String },
}

/// A record of this session having been forked from another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRecord {
    pub source: SessionId,
    pub at_seq: Seq,
}

/// The reduction of a `DomainEvent` log: the harness's entire semantic
/// state. Built exclusively by `apply`, which is pure and sync (ARCH §1, §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    /// The `seq` carried by the most recently applied event. `apply` trusts
    /// the caller's replay order and never reorders, clamps or rejects on
    /// this value — see `Seq`'s and `Timestamp`'s doc comments.
    pub last_seq: Seq,
    /// Per-turn state, keyed by turn id.
    pub turns: HashMap<TurnId, TurnState>,
    /// Per-item state, keyed by item id.
    pub items: HashMap<ItemId, ItemState>,
    /// Every event id observed so far, used to resolve causal parents.
    pub known_events: HashSet<EventId>,
    /// Event ids whose declared causal parent has not (yet) been observed.
    /// `apply` is total: an unresolvable parent is recorded here rather than
    /// rejected or silently dropped.
    pub unresolved_parents: Vec<EventId>,
    /// Running usage totals across the session.
    pub usage: Usage,
    /// Permission requests and decisions, in application order.
    pub permissions: Vec<PermissionEvent>,
    /// Hooks that fired, in application order.
    pub hooks: Vec<HookRecord>,
    /// Subagent lifecycle events, in application order.
    pub subagents: Vec<SubagentEvent>,
    /// Error messages, in application order.
    pub errors: Vec<String>,
    /// Compaction summaries, in application order.
    pub compactions: Vec<String>,
    /// Fork records, in application order.
    pub forks: Vec<ForkRecord>,
}

impl SessionState {
    /// Apply one domain event to the state.
    ///
    /// Pure and sync: no I/O, no wall-clock reads, no randomness. Total: no
    /// input — including an unknown causal parent, an out-of-order `seq`, or
    /// a variant carrying empty/degenerate data — causes a panic.
    pub fn apply(&mut self, ev: &DomainEvent) {
        let _span = tracing::trace_span!(
            "apply",
            seq = ?ev.env.seq,
            event_id = ?ev.env.id,
            turn = ?ev.env.turn,
            item = ?ev.env.item,
            kind = ev.kind.name(),
        )
        .entered();

        self.last_seq = ev.env.seq;

        let parent_resolved = ev
            .env
            .parent
            .is_none_or(|parent| self.known_events.contains(&parent));
        if !parent_resolved {
            self.unresolved_parents.push(ev.env.id);
        }
        self.known_events.insert(ev.env.id);

        match &ev.kind {
            EventKind::UserPrompt { text } => {
                if let Some(item) = self.item_mut(ev) {
                    item.text = text.clone();
                }
            }
            EventKind::AssistantDelta { text } => {
                if let Some(item) = self.item_mut(ev) {
                    item.text.push_str(text);
                }
            }
            EventKind::AssistantMessage { text } => {
                if let Some(item) = self.item_mut(ev) {
                    item.text = text.clone();
                }
            }
            EventKind::ReasoningDelta { text } => {
                if let Some(item) = self.item_mut(ev) {
                    item.text.push_str(text);
                }
            }
            EventKind::ToolCallStarted { name, args } => {
                if let Some(item) = self.item_mut(ev) {
                    item.tool_name = Some(name.clone());
                    item.text = args.clone();
                }
            }
            EventKind::ToolCallDelta { text } => {
                if let Some(item) = self.item_mut(ev) {
                    item.text.push_str(text);
                }
            }
            EventKind::ToolCallCompleted { ok, output } => {
                if let Some(item) = self.item_mut(ev) {
                    item.text = output.clone();
                    item.completed = true;
                    item.ok = Some(*ok);
                }
            }
            EventKind::PermissionRequested { description } => {
                self.permissions.push(PermissionEvent::Requested {
                    item: ev.env.item,
                    description: description.clone(),
                });
            }
            EventKind::PermissionDecided { allowed } => {
                self.permissions.push(PermissionEvent::Decided {
                    item: ev.env.item,
                    allowed: *allowed,
                });
            }
            EventKind::UsageUpdated {
                input_tokens,
                output_tokens,
            } => {
                self.usage.add(*input_tokens, *output_tokens);
            }
            EventKind::TurnCompleted { ok, exit_reason } => {
                let turn = self.turns.entry(ev.env.turn).or_default();
                turn.completed = true;
                turn.ok = *ok;
                turn.exit_reason = Some(*exit_reason);
            }
            EventKind::Compacted { summary } => self.compactions.push(summary.clone()),
            EventKind::Forked { source, at_seq } => self.forks.push(ForkRecord {
                source: *source,
                at_seq: *at_seq,
            }),
            EventKind::HookFired { name, blocked } => self.hooks.push(HookRecord {
                name: name.clone(),
                blocked: *blocked,
            }),
            EventKind::SubagentSpawned { subagent_id } => {
                self.subagents.push(SubagentEvent::Spawned {
                    subagent_id: subagent_id.clone(),
                });
            }
            EventKind::SubagentReturned {
                subagent_id,
                result,
            } => {
                self.subagents.push(SubagentEvent::Returned {
                    subagent_id: subagent_id.clone(),
                    result: result.clone(),
                });
            }
            EventKind::Error { message } => self.errors.push(message.clone()),
        }
    }

    /// The item this event's envelope points at, creating it on first
    /// reference. `None` when the envelope carries no item id — a degenerate
    /// but valid input that `apply` must not panic on.
    fn item_mut(&mut self, ev: &DomainEvent) -> Option<&mut ItemState> {
        let item_id = ev.env.item?;
        Some(
            self.items
                .entry(item_id)
                .or_insert_with(|| ItemState::new(ev.env.turn)),
        )
    }
}
