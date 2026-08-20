//! `SessionState` and the pure, sync reducer.
//!
//! See `ARCHITECTURE.md` §1: state is a reduction over an append-only event
//! log, and compaction and forking are events *in* the log rather than
//! operations *on* it.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::cost::{CostDelta, CumulativeCost};
use crate::event::domain::{DomainEvent, EventKind, ExitReason, OccupancyProvenance};
use crate::event::id::{AttemptId, EventId, GenerationId, ItemId, Seq, SessionId, TurnId};
use crate::generator::attempt::{AttemptRef, Outcome, UsageReport};

/// Token counts, split by the categories providers price separately.
///
/// Four categories rather than the input/output pair this type used to
/// carry. Cache reads and cache writes are billed at materially different
/// rates from uncached input — typically a fraction and a premium
/// respectively — so an `input_tokens` field that silently mixed all three
/// made the cost of a cached conversation unknowable, and made even the
/// standard context-occupancy proxy impossible to compute. See the mdkb
/// decision `ego-acp-usage-verdict`.
///
/// Used both as one generation's increment (`AiEvent::Usage`,
/// `EventKind::UsageUpdated`) and as the session's running totals
/// (`SessionState::usage`): the same shape, folded by `add`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed at the full rate — not served from cache.
    pub uncached_input_tokens: u64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written into the provider's prompt cache.
    pub cache_write_tokens: u64,
    /// Generated tokens, including reasoning tokens billed as output.
    pub output_tokens: u64,
}

impl Usage {
    /// Every token in every category.
    ///
    /// Saturating for the same reason `add` is. At a generation boundary
    /// this sum is also the industry-standard *proxy* for context
    /// occupancy — but only there, and only until the history changes, so
    /// occupancy has its own event and its own field rather than being read
    /// off the running totals. See `EventKind::ContextWindowUpdated`.
    pub fn total_tokens(&self) -> u64 {
        self.uncached_input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.output_tokens)
    }

    /// Add one generation's usage to the running totals.
    ///
    /// Saturates rather than panicking or wrapping: `apply` must be total,
    /// and this totals field is later read by budget gates, so a silent
    /// wrap to near-zero (`wrapping_add`) would corrupt spend accounting —
    /// worse than reporting a pinned-at-max total from a degenerate or
    /// hostile log (this is data read back from disk, not just a
    /// theoretical edge).
    pub fn add(&mut self, delta: &Usage) {
        self.uncached_input_tokens = self
            .uncached_input_tokens
            .saturating_add(delta.uncached_input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(delta.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(delta.cache_write_tokens);
        self.output_tokens = self.output_tokens.saturating_add(delta.output_tokens);
    }
}

/// What the context window currently holds, as last reported.
///
/// `Option`, not a bare `u64` defaulting to zero: "nobody has told us yet"
/// and "the context is empty" are different facts, and the ACP contract
/// turns on exactly that difference — an agent must not send `UsageUpdate`
/// when occupancy is unknown, and a zero would be indistinguishable from a
/// fresh session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextWindow {
    pub used_tokens: Option<u64>,
    /// Where `used_tokens` came from. Kept beside the number so a consumer
    /// can tell a provider's authoritative answer from a local estimate.
    pub provenance: Option<OccupancyProvenance>,
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

/// One money observation, in the order it was applied.
///
/// One entry per `UsageUpdated`, not one per attempt: providers report usage
/// in increments, and giving each increment its own entry means accumulation
/// is just the fold, with no second addition rule for `CostDelta` to disagree
/// with [`CumulativeCost::add_delta`]. A correction collapses all of one
/// attempt's entries into a single revised one. See
/// `docs/decisions/2026-08-21-attempt-accounting-ledger.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charge {
    /// Which attempt spent it, when the event said so. `None` is a record
    /// written before attempts were attributed, and is not correctable —
    /// there is nothing to key a correction to.
    pub attempt: Option<AttemptRef>,
    /// The tokens observed.
    pub usage: Usage,
    /// What those tokens cost, or why that is not known.
    pub cost: CostDelta,
}

/// An attempt that has started and not yet settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveAttempt {
    /// The generation the attempt was started under. A settlement naming a
    /// different one is refused rather than silently re-attributed.
    pub generation: GenerationId,
}

/// Why the reducer declined to apply an event's effect.
///
/// `apply` is total and returns `()`, so a refusal is *recorded*, never
/// signalled. The envelope bookkeeping (`last_seq`, `known_events`,
/// `unresolved_parents`) still happens — it describes the event's arrival,
/// not its meaning — and the variant's effect does not. This mirrors
/// `unresolved_parents`, which chose recording over rejecting for the same
/// reason: a reducer that panics on a hostile log cannot be replayed, and one
/// that drops events silently cannot be audited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// Named an attempt that was never started, or whose contribution was
    /// never booked.
    UnknownAttempt,
    /// Settled under a different generation than it was started under.
    GenerationMismatch,
    /// Named an attempt that has already settled.
    AlreadySettled,
    /// Started an attempt id that is already in flight. An `AttemptId` is
    /// minted once per journey to the wire, so a second start is a replayed
    /// or forged record.
    AlreadyStarted,
    /// A correction for an attempt that is still live. Corrections replace a
    /// finalised figure; a live one is still accumulating.
    NotSettled,
    /// Usage with no attempt, after attribution had already begun. A producer
    /// does not get to opt out by omitting the field.
    UnattributedUsage,
    /// The settlement's own usage report disagrees with what was booked for
    /// that attempt.
    ///
    /// Recorded, and the settlement still completes. The bookings are what
    /// the harness priced and charged a real budget gate for; the report is
    /// evidence about them. Discarding the booking because a late frame
    /// disagrees would let the provider rewrite what was already paid.
    UsageDisagreesWithSettlement,
}

/// An event whose effect the reducer declined to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub event: EventId,
    pub reason: RefusalReason,
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
    /// Running usage totals across the session. The fold of [`Self::charges`].
    pub usage: Usage,
    /// The running cost of the session, or why it is not known. The fold of
    /// [`Self::charges`].
    pub cost: CumulativeCost,
    /// Every money contribution, in application order.
    ///
    /// A `Vec` and not a map, and the order is load-bearing rather than
    /// incidental: `CumulativeCost::add_delta` is order-dependent twice over
    /// — the first `Known` delta fixes the settlement currency and the first
    /// `Unknown` reason is the one kept — so refolding an unordered
    /// collection can produce a different total from the original fold, and
    /// with a `HashMap` a different one on each replay. See the ADR for the
    /// worked counterexample.
    ///
    /// Grows by one entry per attempt and is not pruned: a correction can
    /// only replace a contribution that is still here. That is proportional
    /// to attempts, not to log length — a log holds many events per attempt —
    /// but it is unbounded over a session's life. The *lifecycle* state
    /// ([`Self::live_attempts`]) is the part bounded by live generations.
    pub charges: Vec<Charge>,
    /// Every attempt that has settled, and the generation it settled under.
    ///
    /// This is what makes "a correction for an unknown attempt is refused"
    /// answerable, and it is also why the memory cannot be bounded: refusing
    /// an unknown attempt means remembering every known one.
    pub settled_attempts: HashMap<AttemptId, GenerationId>,
    /// Attempts started and not yet settled. Cleared at settlement, so this
    /// is bounded by concurrent generations.
    pub live_attempts: HashMap<AttemptId, LiveAttempt>,
    /// Whether any `GenerationStarted` has been observed. Once it has,
    /// unattributed usage is refused.
    pub attribution_began: bool,
    /// Events whose effect was declined, in application order.
    pub refusals: Vec<Refusal>,
    /// The last reported context-window occupancy.
    pub context: ContextWindow,
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
            EventKind::GenerationStarted { attempt } => self.start(ev, attempt),
            EventKind::AttemptSettled { attempt, outcome } => self.settle(ev, attempt, outcome),
            EventKind::UsageUpdated {
                usage,
                cost_delta,
                attempt,
            } => self.book(ev, *attempt, usage, cost_delta),
            EventKind::UsageCorrected {
                attempt,
                usage,
                cost_delta,
            } => self.correct(ev, attempt, usage, cost_delta),
            EventKind::ContextWindowUpdated {
                used_tokens,
                provenance,
            } => {
                // Replace, never add: occupancy is a level, and a compaction
                // makes it go down.
                self.context = ContextWindow {
                    used_tokens: Some(*used_tokens),
                    provenance: Some(*provenance),
                };
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

    /// Decline an event's effect, recording why. See [`RefusalReason`].
    fn refuse(&mut self, ev: &DomainEvent, reason: RefusalReason) {
        tracing::debug!(
            event_id = ?ev.env.id,
            kind = ev.kind.name(),
            ?reason,
            "reducer refused an event's effect"
        );
        self.refusals.push(Refusal {
            event: ev.env.id,
            reason,
        });
    }

    /// Open an attempt's lifecycle.
    ///
    /// Starting an attempt id twice is refused rather than reopening it: an
    /// `AttemptId` is minted once per journey to the wire, so a second start
    /// is a replayed or forged record, and reopening a settled attempt would
    /// let its finalised contribution start accumulating again.
    fn start(&mut self, ev: &DomainEvent, attempt: &AttemptRef) {
        // Set before any refusal: attribution has begun the moment a producer
        // shows it knows how to attribute, whether or not this record stands.
        self.attribution_began = true;
        if self.settled_attempts.contains_key(&attempt.attempt()) {
            return self.refuse(ev, RefusalReason::AlreadySettled);
        }
        if self.live_attempts.contains_key(&attempt.attempt()) {
            return self.refuse(ev, RefusalReason::AlreadyStarted);
        }
        self.live_attempts.insert(
            attempt.attempt(),
            LiveAttempt {
                generation: attempt.generation(),
            },
        );
    }

    /// Book one usage observation against the ledger.
    fn book(
        &mut self,
        ev: &DomainEvent,
        attempt: Option<AttemptRef>,
        usage: &Usage,
        cost: &CostDelta,
    ) {
        let Some(attempt) = attempt else {
            // A legacy record, from before attribution existed. Accepted only
            // while nothing has shown it knows better.
            if self.attribution_began {
                return self.refuse(ev, RefusalReason::UnattributedUsage);
            }
            return self.push_charge(None, *usage, cost.clone());
        };

        let id = attempt.attempt();
        if self.settled_attempts.contains_key(&id) {
            // Past settlement the figures are final. A later revision is a
            // `UsageCorrected`, which replaces; letting a stray `UsageUpdated`
            // add to a settled attempt is exactly the double count this
            // contract exists to prevent.
            return self.refuse(ev, RefusalReason::AlreadySettled);
        }
        let Some(live) = self.live_attempts.get(&id).copied() else {
            return self.refuse(ev, RefusalReason::UnknownAttempt);
        };
        if live.generation != attempt.generation() {
            return self.refuse(ev, RefusalReason::GenerationMismatch);
        }
        self.push_charge(Some(attempt), *usage, cost.clone());
    }

    /// Close an attempt's lifecycle, finalising whatever it booked.
    fn settle(&mut self, ev: &DomainEvent, attempt: &AttemptRef, outcome: &Outcome) {
        let id = attempt.attempt();
        if self.settled_attempts.contains_key(&id) {
            return self.refuse(ev, RefusalReason::AlreadySettled);
        }
        let Some(live) = self.live_attempts.get(&id).copied() else {
            return self.refuse(ev, RefusalReason::UnknownAttempt);
        };
        if live.generation != attempt.generation() {
            // Left live on purpose: a settlement naming the wrong generation
            // says nothing about the attempt that really is in flight, so
            // closing it on that evidence would strand the real one.
            return self.refuse(ev, RefusalReason::GenerationMismatch);
        }
        self.live_attempts.remove(&id);
        self.settled_attempts.insert(id, attempt.generation());

        // Evidence, not a booking: the provider's own report is checked
        // against what was booked, and a disagreement is recorded without
        // rewriting figures a real budget gate was already charged for.
        let booked = self.booked_usage(id);
        let reported = match outcome.usage() {
            UsageReport::Reported(usage) => Some(usage),
            UsageReport::Missing => None,
        };
        if booked != reported {
            self.refuse(ev, RefusalReason::UsageDisagreesWithSettlement);
        }
    }

    /// Replace one settled attempt's contribution with a revised figure.
    ///
    /// Every entry that attempt booked collapses into one: the first keeps
    /// its position in the ledger, because a correction revises where the
    /// spend happened rather than moving it to the end, and order decides
    /// both the settlement currency and which `Unknown` reason is kept.
    fn correct(&mut self, ev: &DomainEvent, attempt: &AttemptRef, usage: &Usage, cost: &CostDelta) {
        let id = attempt.attempt();
        if self.live_attempts.contains_key(&id) {
            // Still accumulating. "Replace" and "accumulate" racing for the
            // same figures is a total nobody can reproduce.
            return self.refuse(ev, RefusalReason::NotSettled);
        }
        let Some(generation) = self.settled_attempts.get(&id).copied() else {
            return self.refuse(ev, RefusalReason::UnknownAttempt);
        };
        if generation != attempt.generation() {
            return self.refuse(ev, RefusalReason::GenerationMismatch);
        }
        let booked: Vec<usize> = self.charge_positions(id).collect();
        let Some(&first) = booked.first() else {
            // Settled without booking anything. There is no contribution to
            // replace, and inventing one would let a correction conjure spend
            // out of nothing.
            return self.refuse(ev, RefusalReason::UnknownAttempt);
        };
        self.charges[first].usage = *usage;
        self.charges[first].cost = cost.clone();
        for &at in booked[1..].iter().rev() {
            self.charges.remove(at);
        }
        self.refold();
    }

    /// Where `id`'s contributions sit in the ledger, in order.
    fn charge_positions(&self, id: AttemptId) -> impl Iterator<Item = usize> + '_ {
        self.charges
            .iter()
            .enumerate()
            .filter(move |(_, c)| c.attempt.is_some_and(|r| r.attempt() == id))
            .map(|(at, _)| at)
    }

    /// Everything booked against `id`, or `None` if it booked nothing.
    fn booked_usage(&self, id: AttemptId) -> Option<Usage> {
        let mut total: Option<Usage> = None;
        for at in self.charge_positions(id) {
            total.get_or_insert_default().add(&self.charges[at].usage);
        }
        total
    }

    /// Append a contribution and fold it into the totals.
    ///
    /// Incremental, not a refold: appending at the end *is* the fold's next
    /// step, so the running totals stay exactly what a full pass would
    /// produce, and the ordinary path stays O(1). Only [`Self::correct`],
    /// which changes an entry that is not at the end, has to pay for a pass.
    fn push_charge(&mut self, attempt: Option<AttemptRef>, usage: Usage, cost: CostDelta) {
        self.usage.add(&usage);
        self.cost.add_delta(&cost);
        self.charges.push(Charge {
            attempt,
            usage,
            cost,
        });
    }

    /// Recompute the session totals from the ledger, in order.
    ///
    /// A full pass, not an adjustment. Subtraction is not available and would
    /// not be enough if it were: an `Unknown` contribution poisons the total
    /// and nobody knows how big it was, so the only way to clear one is to
    /// fold the ledger again without it. That is also the correction that
    /// matters most in practice — a provider that reported no usage, priced
    /// properly an hour later.
    ///
    /// O(n) per correction, and a log crafted to be all corrections replays
    /// in O(n²). See `POTENTIALS.md`.
    fn refold(&mut self) {
        let mut usage = Usage::default();
        let mut cost = CumulativeCost::default();
        for charge in &self.charges {
            usage.add(&charge.usage);
            cost.add_delta(&charge.cost);
        }
        self.usage = usage;
        self.cost = cost;
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
