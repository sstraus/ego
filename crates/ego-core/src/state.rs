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
use crate::generator::types::ReasoningState;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed at the full rate — not served from cache.
    ///
    /// **The partition direction is this type's, not a wire's**, and the
    /// wires disagree: OpenAI's `prompt_tokens` *includes* its cache figures
    /// while Anthropic's `input_tokens` *excludes* them, so the same
    /// arithmetic applied to both is a silent factor-of-two in one direction
    /// or the other. Here the four fields are disjoint and this one is what
    /// is left after the two cache categories are taken out; a mapping that
    /// receives an already-exclusive figure passes it through, and one that
    /// receives an inclusive figure subtracts. Asserted per profile by check
    /// 4 of `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §9, which is where it can actually fail.
    pub uncached_input_tokens: u64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written into the provider's prompt cache at its standard
    /// retention.
    pub cache_write_tokens: u64,
    /// Input tokens written at an extended retention the request asked for.
    ///
    /// Its own category because every provider that offers one charges more
    /// for it: Anthropic prices its 5-minute and 1-hour writes at 1.25× and
    /// 2× the base input rate. Named standard/extended and not "5m"/"1h",
    /// which are one vendor's numbers and would put a vendor's TTL in a
    /// provider-neutral type; standard against extended is the distinction
    /// every provider offering a choice actually makes. The whole
    /// openai-completions family publishes one cache-write figure, which is
    /// the standard one, so nothing there moves.
    ///
    /// A plain `u64` where [`Self::reasoning_tokens`] below is an `Option`,
    /// and the asymmetry is the point: only a provider that *offers* an
    /// extended retention can write at one, and the one row that does —
    /// `anthropic-messages`, which reports the split under
    /// `cache_creation.ephemeral_1h` — was added no earlier than this field
    /// was. So a record without this field still came from a run that provably
    /// wrote nothing at extended retention: there was no row it could have
    /// written to. Zero is that run's measurement. The reasoning split is the
    /// opposite case — a figure some rows publish and this one used to
    /// discard.
    #[serde(default)]
    pub cache_write_extended_tokens: u64,
    /// Generated tokens, including reasoning tokens billed as output.
    pub output_tokens: u64,
    /// How many of [`Self::output_tokens`] the provider attributed to
    /// reasoning — **when it said**.
    ///
    /// `None` is not zero, and that is the whole reason this is an `Option`.
    /// OpenRouter and xAI publish `completion_tokens_details.reasoning_tokens`
    /// and Gemini publishes `total_thought_tokens`; **Ollama publishes
    /// nothing**, so on that row the split is not merely undeclared but
    /// unrecoverable after the fact. A `0` there would state a measurement
    /// nobody made. The cost of getting this wrong is recorded live in
    /// `crates/ego-ai/tests/ollama_reasoning.rs`: a turn whose visible answer
    /// was the single character `9` reported 118 output tokens, 117 of them
    /// reasoning, and the daemon reported none of that.
    ///
    /// This is the same deliberate reading `map_usage` already makes when it
    /// treats an absent `prompt_tokens_details` as "all uncached" and says so
    /// — an assumption written down is falsifiable by the next reader; one
    /// left as a default is invisible to them.
    ///
    /// **Not a fifth category — a breakdown of the fourth.** All six wires
    /// bill reasoning as output, so these tokens are already inside
    /// `output_tokens`, and [`Self::total_tokens`] deliberately does not add
    /// them. Gemini states thoughts as a separate summand; its mapping folds
    /// them into `output_tokens` *and* reports them here.
    ///
    /// Skipped when unknown rather than written as `null`, on the rule
    /// `EventKind::ToolCallStarted::call_id` already set and its codec test
    /// pins: "nobody told us" gets one spelling on disk, and it is the same
    /// spelling a log written before this field existed already has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// A usage of nothing: every category counted, all of them zero.
    ///
    /// This is the identity of [`add`](Usage::add), and the only correct seed
    /// for a fold. Every accumulation in the process starts here — the two
    /// `SessionState` folds, `attempt.rs`'s `UsageReport::Missing`, and
    /// `turn`'s held total.
    ///
    /// **`reasoning_tokens` is `Some(0)` and not `None`**, because the sum
    /// treats unknown as *absorbing*: seeding a fold with `None` would make
    /// every total in the process unknown no matter what the providers went
    /// on to report. A run that has measured nothing yet has measured zero.
    ///
    /// **A const rather than a `Default` impl**, and that is the load-bearing
    /// half. `Default` is reachable without being named —
    /// `unwrap_or_default`, `or_default`, `..Default::default()`, a `derive`
    /// on any struct that later gains a `Usage` field — and each of those
    /// paths would mint a *measured zero* for a reasoning split nobody
    /// reported. That is precisely the falsehood
    /// [`reasoning_tokens`](Usage::reasoning_tokens) exists to prevent, and
    /// it would arrive silently. `cost.rs`'s module doc ("Unknown is a value,
    /// not a missing value") rejected the same hazard one field over, for the
    /// same reason: `Option` invites `unwrap_or_default()`, which prices an
    /// unpriced generation at zero. Naming the identity makes the seed a
    /// decision at every site instead of a default nobody chose.
    ///
    /// A record *decoded* without the field is the opposite case and does get
    /// `None`: nothing predating the field ever stated one. The two differ on
    /// purpose, and the difference is what the field is for.
    pub const IDENTITY: Self = Self {
        uncached_input_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_extended_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: Some(0),
    };

    /// Every token in every category.
    ///
    /// Saturating for the same reason `add` is. At a generation boundary
    /// this sum is also the industry-standard *proxy* for context
    /// occupancy — but only there, and only until the history changes, so
    /// occupancy has its own event and its own field rather than being read
    /// off the running totals. See `EventKind::ContextWindowUpdated`.
    ///
    /// [`Self::reasoning_tokens`] is absent from this sum on purpose: it is a
    /// breakdown of `output_tokens` rather than a category beside it, and
    /// adding it would count those tokens twice.
    pub fn total_tokens(&self) -> u64 {
        self.uncached_input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.cache_write_extended_tokens)
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
        self.cache_write_extended_tokens = self
            .cache_write_extended_tokens
            .saturating_add(delta.cache_write_extended_tokens);
        self.output_tokens = self.output_tokens.saturating_add(delta.output_tokens);
        // Unknown is absorbing. A total that summed one generation's 117
        // stated reasoning tokens with another generation that stated none is
        // not 117 — it is at least 117 and unknown, and reporting the lower
        // bound as the total is the same falsehood as defaulting the field to
        // zero. Nothing is lost by this: each generation's own figure stays in
        // the log, where it was written.
        self.reasoning_tokens = match (self.reasoning_tokens, delta.reasoning_tokens) {
            (Some(total), Some(delta)) => Some(total.saturating_add(delta)),
            _ => None,
        };
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

/// What an item is, and therefore who authored it.
///
/// Not [`crate::public::ItemKind`], which answers a different question for a
/// different audience: that one collapses user and assistant into a single
/// `Message` (`CLI.md` §4.2) and adds kinds that never become items here.
/// Rebuilding a conversation needs exactly the distinction the public one
/// throws away.
///
/// Decided by the first event that touches the item and never revised: an
/// item's author is settled when it is created. Without it a rebuilt history
/// cannot tell a user's prompt from the assistant's reply — all three of
/// `UserPrompt`, `AssistantMessage` and `ReasoningDelta` write the same
/// text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserPrompt,
    Assistant,
    Reasoning,
    ToolCall,
}

/// Per-item state: accumulated text for messages, reasoning and tool calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    pub turn: TurnId,
    /// The `seq` of the event that created this item.
    ///
    /// [`SessionState::items`] is a `HashMap`, so the order items happened in
    /// has to be carried rather than iterated. Sorting by `ItemId` would
    /// *usually* agree — they are UUIDv7 — which is what makes it the wrong
    /// thing to rely on.
    pub seq: Seq,
    pub kind: ItemKind,
    /// The provider's id for a tool call, when the record carried one.
    pub call_id: Option<String>,
    pub tool_name: Option<String>,
    /// A tool call's arguments.
    ///
    /// Its own field, and not shared with [`Self::text`]: `ToolCallCompleted`
    /// writes the output, and while the two lived in one field a *completed*
    /// call could no longer say what it had been called with — which is
    /// exactly what a rebuilt history needs from it.
    pub args: String,
    /// Message and reasoning text, and a tool call's output.
    pub text: String,
    /// A reasoning item's opaque continuation state, once its block closed.
    ///
    /// Its own field and not [`Self::text`]: the two are different values on
    /// every wire that mints state, and a block can carry one without the
    /// other in both directions — `redacted_thinking` has state and no
    /// readable text, and three of the six wires publish readable reasoning
    /// and mint nothing replayable. `None` means the block minted none, which
    /// is why a reasoning item with text alone is not replayed.
    #[serde(default)]
    pub reasoning_state: Option<ReasoningState>,
    pub completed: bool,
    pub ok: Option<bool>,
    /// The generation this item came out of, when the log says which.
    ///
    /// Derived, not recorded: no event carries this field. It is resolved
    /// from the envelope's causal parent through
    /// [`SessionState::generations`], which is why there is exactly one fact
    /// about attribution in the log and not two that can disagree.
    ///
    /// The full [`AttemptRef`] and not the generation alone: retries
    /// deliberately share a generation, so grouping by generation would merge
    /// a retried call with the attempt it replaced.
    ///
    /// `None` on every record written before the parent edge existed, and on
    /// any item whose producer names no parent. It means "this log does not
    /// say", which is not the same as "on its own" — readers must not infer
    /// grouping from its absence.
    #[serde(default)]
    pub generation: Option<AttemptRef>,
}

impl ItemState {
    fn new(turn: TurnId, seq: Seq, kind: ItemKind) -> Self {
        Self {
            turn,
            seq,
            kind,
            call_id: None,
            tool_name: None,
            args: String::new(),
            text: String::new(),
            reasoning_state: None,
            completed: false,
            ok: None,
            generation: None,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Which attempt each accepted `GenerationStarted` opened, keyed by the
    /// event's own id — the table [`ItemState::generation`] is resolved
    /// through.
    ///
    /// Only accepted starts are here. A start the reducer refused opened
    /// nothing, so an item naming it as parent belongs to no generation this
    /// state knows, and saying otherwise would attribute items to a record
    /// that was declined.
    ///
    /// Grows by one entry per generation and is not pruned, for the same
    /// reason [`Self::settled_attempts`] is not: a parent may be named long
    /// after the generation that wrote it settled.
    #[serde(default)]
    pub generations: HashMap<EventId, AttemptRef>,
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

/// A session that has observed nothing.
///
/// Written out rather than derived for one field: `usage` seeds the fold and
/// must be [`Usage::IDENTITY`], which is a *measured* zero in every category
/// including the reasoning split. `Usage` deliberately has no `Default` impl
/// so that this choice cannot be made silently — see the const's doc — and
/// the price of that is this block. It is the intended price: a field added
/// here later will not compile until somebody says what an empty session's
/// value for it is.
impl Default for SessionState {
    fn default() -> Self {
        Self {
            usage: Usage::IDENTITY,
            last_seq: Seq::default(),
            turns: HashMap::default(),
            items: HashMap::default(),
            known_events: HashSet::default(),
            unresolved_parents: Vec::default(),
            generations: HashMap::default(),
            cost: CumulativeCost::default(),
            charges: Vec::default(),
            settled_attempts: HashMap::default(),
            live_attempts: HashMap::default(),
            attribution_began: false,
            refusals: Vec::default(),
            context: ContextWindow::default(),
            permissions: Vec::default(),
            hooks: Vec::default(),
            subagents: Vec::default(),
            errors: Vec::default(),
            compactions: Vec::default(),
            forks: Vec::default(),
        }
    }
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
                if let Some(item) = self.item_mut(ev, ItemKind::UserPrompt) {
                    item.text = text.clone();
                }
            }
            EventKind::AssistantDelta { text } => {
                if let Some(item) = self.item_mut(ev, ItemKind::Assistant) {
                    item.text.push_str(text);
                }
            }
            EventKind::AssistantMessage { text } => {
                if let Some(item) = self.item_mut(ev, ItemKind::Assistant) {
                    item.text = text.clone();
                }
            }
            EventKind::ReasoningDelta { text } => {
                if let Some(item) = self.item_mut(ev, ItemKind::Reasoning) {
                    item.text.push_str(text);
                }
            }
            // Replace and not append: this is one block's whole state, and a
            // block that somehow reported twice reported a correction, not a
            // continuation. Concatenating two opaque blobs would produce a
            // third that no provider minted.
            EventKind::ReasoningState { state } => {
                if let Some(item) = self.item_mut(ev, ItemKind::Reasoning) {
                    item.reasoning_state = Some(state.clone());
                }
            }
            EventKind::ToolCallStarted {
                name,
                args,
                call_id,
            } => {
                if let Some(item) = self.item_mut(ev, ItemKind::ToolCall) {
                    item.tool_name = Some(name.clone());
                    item.args = args.clone();
                    item.call_id = call_id.clone();
                }
            }
            EventKind::ToolCallDelta { text } => {
                if let Some(item) = self.item_mut(ev, ItemKind::ToolCall) {
                    item.text.push_str(text);
                }
            }
            EventKind::ToolCallCompleted { ok, output } => {
                if let Some(item) = self.item_mut(ev, ItemKind::ToolCall) {
                    item.text = output.clone();
                    item.completed = true;
                    item.ok = Some(*ok);
                }
            }
            EventKind::PermissionRequested { escalation, .. } => {
                self.permissions.push(PermissionEvent::Requested {
                    item: ev.env.item,
                    description: escalation.to_string(),
                });
            }
            // `by` is not reduced. State answers "what is this session's
            // shape now"; who authorised a call is a fact about one decision,
            // it is already in the log verbatim, and nothing reads
            // `permissions` to ask. Keeping it here would put an `Authority`
            // inside the snapshot for no consumer.
            EventKind::PermissionDecided { allowed, .. } => {
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
            // `kind` is deliberately not reduced: `errors` is a list of
            // what was said, and nothing in the reducer branches on why.
            // Adding a field no reader consults would be a second place for
            // the classification to drift from the log's own copy.
            EventKind::Error { message, .. } => self.errors.push(message.clone()),
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
        // Recorded only once the start stands, and kept after it settles: an
        // item may name this event as its parent at any point in the log.
        self.generations.insert(ev.env.id, *attempt);
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
            total
                .get_or_insert(Usage::IDENTITY)
                .add(&self.charges[at].usage);
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
        let mut usage = Usage::IDENTITY;
        let mut cost = CumulativeCost::default();
        for charge in &self.charges {
            usage.add(&charge.usage);
            cost.add_delta(&charge.cost);
        }
        self.usage = usage;
        self.cost = cost;
    }

    /// The item this event names, creating it as `kind` if it is new.
    ///
    /// `None` when the envelope carries no item id — a degenerate but valid
    /// input that `apply` must not panic on.
    ///
    /// `kind` applies only on creation. A later event of a different kind
    /// against the same id does not re-author the item: `apply` is total and
    /// has to survive a hostile or torn log, and silently changing an item's
    /// author mid-stream is worse than keeping the first claim.
    ///
    /// [`ItemState::generation`] is resolved here and follows the same rule
    /// for the same reason: the first event that attributes the item is the
    /// one believed, so a later event naming a different generation as its
    /// parent cannot move an item into another turn.
    fn item_mut(&mut self, ev: &DomainEvent, kind: ItemKind) -> Option<&mut ItemState> {
        let item_id = ev.env.item?;
        let generation = ev
            .env
            .parent
            .and_then(|parent| self.generations.get(&parent).copied());
        let item = self
            .items
            .entry(item_id)
            .or_insert_with(|| ItemState::new(ev.env.turn, ev.env.seq, kind));
        if item.generation.is_none() {
            item.generation = generation;
        }
        Some(item)
    }
}
