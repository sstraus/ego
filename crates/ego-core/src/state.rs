//! `SessionState` and the pure, sync reducer.
//!
//! See `ARCHITECTURE.md` §1: state is a reduction over an append-only event
//! log, and compaction and forking are events *in* the log rather than
//! operations *on* it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::cost::{CostDelta, CumulativeCost};
use crate::event::domain::{
    DomainEvent, EventKind, ExitReason, GenerationRole, OccupancyProvenance, SubagentTerminal,
};
use crate::event::hook::{
    HookActivation, HookCatalogueEvidence, HookDecision, HookId, HookInvocationId, HookLifecycle,
    HookOccurrenceKey, HookOccurrenceOwner, HookPlan, HookResult, HookSubject,
};
use crate::event::id::{
    AttemptId, DetachedExecId, EventId, GenerationId, ItemId, Producer, QueuedPromptId, Seq,
    SessionId, SubagentInvocationId, TurnId,
};
use crate::event::skill::{
    SkillCatalogueEvidence, SkillCatalogueGeneration, SkillContentDigest, SkillId,
    SkillInvocationId, SkillSourceAuthority, SkillSourceLocation,
};
use crate::event::time::Timestamp;
use crate::generator::attempt::{AttemptRef, DeliveryCertainty, Outcome, UsageReport};
use crate::generator::types::{ImageRef, ReasoningState};
use crate::goal::{GoalRecord, GoalState, GoalTransition};
use crate::permit::{DetachedExecSnapshot, ToolResult, ToolResultProvenance};
use crate::question::{
    QuestionAnswer, QuestionError, QuestionOutcome, QuestionRequest, QuestionTarget,
};
use crate::run::{RequestId, RunId, RunSelections};
use crate::session::SessionLineage;
use crate::store::blob::BlobRef;

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
/// Used both as one attempt's increment (`AiEvent::Usage`,
/// `EventKind::UsageUpdated`) and as the session's running totals
/// (`SessionState::usage`): the same shape, folded by `add`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Provider-billed searches, separate from all token categories.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub web_search_requests: u64,
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
    /// 4 of `ARCHITECTURE.md`
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

fn is_zero(value: &u64) -> bool {
    *value == 0
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
        web_search_requests: 0,
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

    /// Add one attempt's usage to the running totals.
    ///
    /// Saturates rather than panicking or wrapping: `apply` must be total,
    /// and this totals field is later read by budget gates, so a silent
    /// wrap to near-zero (`wrapping_add`) would corrupt spend accounting —
    /// worse than reporting a pinned-at-max total from a degenerate or
    /// hostile log (this is data read back from disk, not just a
    /// theoretical edge).
    pub fn add(&mut self, delta: &Usage) {
        self.web_search_requests = self
            .web_search_requests
            .saturating_add(delta.web_search_requests);
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

/// Whether a session is running, holding, or held.
///
/// Three states and not a `bool`, because the middle one is the whole point.
/// `ARCHITECTURE.md`
/// requires that `_ego/pause` report `paused` **only** once the durable
/// boundary exists and report `pending` before then: an indivisible provider,
/// permission or tool effect that is already in flight settles truthfully
/// first, and that gap is observable to a host and can outlive a crash. A
/// boolean would have to answer "is it paused?" with "yes" during a window
/// where an effect is still running, which is the one thing the contract
/// forbids saying.
///
/// The request id travels with both states so a host retrying over a
/// reconnected transport learns what *its own* request achieved. It answers
/// only "where is this session now"; what a *particular* request was answered
/// with outlives the episode that answered it and is kept in
/// [`HoldReceipts`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PauseState {
    /// Nothing has asked this session to stop.
    #[default]
    Running,
    /// A pause was asked for and the boundary does not exist yet. Work
    /// already in flight is settling; nothing new is admitted.
    Requested { request: RequestId },
    /// The boundary exists. Only a resume moves the session out of this.
    Paused { request: RequestId },
}

impl PauseState {
    /// Whether this session may admit a new effect.
    ///
    /// `Requested` blocks as firmly as `Paused` does: the request is the
    /// point at which admission closes, and the boundary is only the record
    /// that everything already admitted has finished. A gate that let effects
    /// through until the boundary existed would never reach one.
    pub fn admits_effects(self) -> bool {
        matches!(self, PauseState::Running)
    }

    /// The request that put this session where it is, if anything did.
    pub fn request(self) -> Option<RequestId> {
        match self {
            PauseState::Running => None,
            PauseState::Requested { request } | PauseState::Paused { request } => Some(request),
        }
    }
}

/// Which hold requests this session has already answered, and with what.
///
/// The idempotency key of `_ego/pause` and `_ego/resume`, and the reason
/// [`PauseState`] alone is not enough: state-based idempotence only holds
/// while the state has not moved on. Two retries defeat it. A resume whose
/// answer was lost in transit, re-sent, finds the session already running and
/// would be told "not paused" — that its request failed, when it succeeded. A
/// pause re-delivered late, after the host released it, would find the session
/// running and hold it a second time, on one request the host made once.
///
/// So the answer is remembered per request rather than inferred from where the
/// session ended up. This is not a second store: every entry is folded from
/// `SessionPaused` and `SessionResumed`, which already carry the request that
/// asked. The log stays the only authority and this is its index.
///
/// The key is `(session, operation, request)`. Session, because the request id
/// is session-bound at the wire and coupling two logs through a shared id
/// would let one session refuse work on account of another. Operation,
/// because a host is entitled to spend one id on a pause and the same id on
/// the resume that releases it; those are two requests, not one reused. The
/// ADR's "reuse with different bytes refuses" needs no fingerprint here: the
/// request body is `v`, `sessionId` and `requestId` and nothing else, so the
/// key *is* the body. A future field would change that and would have to bring
/// a fingerprint with it.
///
/// It grows and is not pruned, like `generations` below and for the same
/// reason: an entry that was dropped would be answered as though it had never
/// arrived, which is the behaviour this type exists to prevent. Holds are
/// human-paced acts, a handful per session, not per-token records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldReceipts {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    paused: BTreeSet<RequestId>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    resumed: BTreeSet<RequestId>,
}

impl HoldReceipts {
    /// Whether this request already reached a durable paused boundary.
    ///
    /// Deliberately not true for a *pending* pause: nothing durable records
    /// one, and a host whose pause never landed should be able to ask again
    /// and have the ask reach the turn rather than be replayed away.
    pub fn paused(&self, request: RequestId) -> bool {
        self.paused.contains(&request)
    }

    /// Whether this request already released a hold.
    pub fn resumed(&self, request: RequestId) -> bool {
        self.resumed.contains(&request)
    }

    /// Whether this session has answered no hold request at all — the state a
    /// session that was never paused stays in, and the one worth omitting from
    /// a snapshot.
    pub fn is_empty(&self) -> bool {
        self.paused.is_empty() && self.resumed.is_empty()
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
    /// The window `used_tokens` is a fraction of, as the reporting
    /// generation stated it.
    ///
    /// Reduced with replace semantics like the numerator, and for the same
    /// reason: a session that moves onto a fallback model moves onto that
    /// model's ceiling, and a reader holding the previous one would render a
    /// fraction of the wrong whole. `None` while no generation has stated a
    /// window — either because none has reported yet, or because the model's
    /// facts carry no `max_context_tokens`.
    #[serde(default)]
    pub window: Option<u64>,
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
    ContextCheckpoint,
    GoalContinuation,
    UserPrompt,
    Assistant,
    Reasoning,
    ToolCall,
    Question,
    DetachedExecNotification,
    HostContext,
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
    #[serde(default)]
    pub context_command: bool,
    /// The exact event that admitted this item as a tool call.
    ///
    /// Kept separately from [`Self::seq`] because continuation causality
    /// names an event id, and a tool delta may create the item before the
    /// corresponding `ToolCallStarted` arrives. `None` means no tool start
    /// has been accepted for this item.
    pub tool_call_started: Option<EventId>,
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
    /// Message and reasoning text, plus any still-streaming tool delta. A
    /// completed tool's semantic result lives in [`Self::tool_result`].
    pub text: String,
    /// Ordered durable image metadata for a user prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
    /// The final evidence returned by a completed tool call.
    pub tool_result: Option<ToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timing: Option<crate::event::domain::ToolTiming>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_content: Option<ReasoningState>,
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
            context_command: false,
            tool_call_started: None,
            call_id: None,
            tool_name: None,
            args: String::new(),
            text: String::new(),
            images: Vec::new(),
            tool_result: None,
            tool_timing: None,
            reasoning_state: None,
            provider_content: None,
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
    Amended {
        item: Option<ItemId>,
        tool: String,
    },
    Decided {
        item: Option<ItemId>,
        allowed: bool,
    },
}

/// One hook start accepted into a reached occurrence's immutable plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookOccurrenceStart {
    event: EventId,
    hook: HookId,
    invocation: HookInvocationId,
    activation: HookActivation,
}

impl HookOccurrenceStart {
    pub fn event(&self) -> EventId {
        self.event
    }

    pub fn hook(&self) -> HookId {
        self.hook
    }

    pub fn invocation(&self) -> HookInvocationId {
        self.invocation
    }

    pub fn activation(&self) -> HookActivation {
        self.activation
    }
}

/// Durable state of one lifecycle occurrence reached by its real owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReachedHookOccurrence {
    event: EventId,
    key: HookOccurrenceKey,
    subject: HookSubject,
    plan: HookPlan,
    producer: Producer,
    turn: TurnId,
    item: Option<ItemId>,
    starts: Vec<HookOccurrenceStart>,
}

impl ReachedHookOccurrence {
    pub fn event(&self) -> EventId {
        self.event
    }

    pub fn key(&self) -> &HookOccurrenceKey {
        &self.key
    }

    pub fn subject(&self) -> HookSubject {
        self.subject
    }

    pub fn plan(&self) -> &HookPlan {
        &self.plan
    }

    pub fn producer(&self) -> &Producer {
        &self.producer
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }

    pub fn item(&self) -> Option<ItemId> {
        self.item
    }

    pub fn starts(&self) -> &[HookOccurrenceStart] {
        &self.starts
    }

    pub fn replay_disposition(&self) -> HookReplayDisposition {
        if self.key.lifecycle().phase().is_pre_effect() {
            HookReplayDisposition::FailClosed
        } else {
            HookReplayDisposition::ObserverUncertain
        }
    }
}

/// A hook invocation that crossed its external-delivery boundary but has not
/// yet settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveHookInvocation {
    occurrence: EventId,
    start: EventId,
    hook: HookId,
    lifecycle: HookLifecycle,
    subject: HookSubject,
    activation: HookActivation,
    turn: TurnId,
    item: Option<ItemId>,
}

impl LiveHookInvocation {
    pub fn occurrence(&self) -> EventId {
        self.occurrence
    }

    pub fn start(&self) -> EventId {
        self.start
    }

    pub fn hook(&self) -> HookId {
        self.hook
    }

    pub fn lifecycle(&self) -> HookLifecycle {
        self.lifecycle
    }

    pub fn subject(&self) -> HookSubject {
        self.subject
    }

    pub fn activation(&self) -> HookActivation {
        self.activation
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }

    pub fn item(&self) -> Option<ItemId> {
        self.item
    }

    /// Crossing the start boundary cannot prove whether an external handler
    /// received the invocation.
    pub fn delivery(&self) -> DeliveryCertainty {
        DeliveryCertainty::PossiblySent
    }

    pub fn replay_disposition(&self) -> HookReplayDisposition {
        if self.lifecycle.phase().is_pre_effect() {
            HookReplayDisposition::FailClosed
        } else {
            HookReplayDisposition::ObserverUncertain
        }
    }
}

/// What replay must do with an invocation whose settlement is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookReplayDisposition {
    FailClosed,
    ObserverUncertain,
}

/// One accepted settlement, retained in application order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSettlement {
    event: EventId,
    occurrence: EventId,
    hook: HookId,
    invocation: HookInvocationId,
    result: HookResult,
    turn: TurnId,
    item: Option<ItemId>,
}

impl HookSettlement {
    pub fn event(&self) -> EventId {
        self.event
    }

    pub fn hook(&self) -> HookId {
        self.hook
    }

    pub fn occurrence(&self) -> EventId {
        self.occurrence
    }

    pub fn invocation(&self) -> HookInvocationId {
        self.invocation
    }

    pub fn result(&self) -> &HookResult {
        &self.result
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }

    pub fn item(&self) -> Option<ItemId> {
        self.item
    }
}

/// A subagent lifecycle event, in application order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentEvent {
    Requested {
        invocation: SubagentInvocationId,
        task: BlobRef,
        selections: Box<RunSelections>,
    },
    Spawned {
        invocation: SubagentInvocationId,
        child: RunId,
    },
    Settled {
        invocation: SubagentInvocationId,
        child: Option<RunId>,
        terminal: SubagentTerminal,
    },
}

/// Durable rendered skill evidence retained by the reducer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInstructionEvent {
    pub event: EventId,
    pub turn: TurnId,
    pub item: Option<ItemId>,
    pub parent: Option<EventId>,
    pub invocation: SkillInvocationId,
    pub skill: SkillId,
    pub catalogue_generation: SkillCatalogueGeneration,
    pub source: SkillSourceAuthority,
    pub location: SkillSourceLocation,
    pub source_digest: SkillContentDigest,
    pub admission: BlobRef,
    pub rendered: BlobRef,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// A skill state record. Boxing the instruction payload bounds the enum while
/// Serde preserves the same externally tagged object as the former struct
/// variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillEvent {
    Instruction(Box<SkillInstructionEvent>),
    Resource {
        invocation: SkillInvocationId,
        skill: SkillId,
        location: SkillSourceLocation,
        content: BlobRef,
    },
}

/// A record of this session having been forked from another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRecord {
    pub source: SessionId,
    pub at_seq: Seq,
}

/// The one durable human-input request that currently suspends a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingQuestion {
    item: ItemId,
    turn: TurnId,
    request_event: EventId,
    target: QuestionTarget,
    request: QuestionRequest,
}

/// A durable settlement retained for deterministic recovery until (and after)
/// the correlated pure tool result is observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettledQuestion {
    pending: PendingQuestion,
    settlement_event: EventId,
    outcome: QuestionOutcome,
}

impl SettledQuestion {
    pub fn item_id(&self) -> ItemId {
        self.pending.item
    }

    pub fn tool_call(&self) -> ItemId {
        self.pending.target.tool_call()
    }

    pub fn turn(&self) -> TurnId {
        self.pending.turn
    }

    pub fn outcome(&self) -> &QuestionOutcome {
        &self.outcome
    }

    pub fn settlement_event(&self) -> EventId {
        self.settlement_event
    }
}

impl PendingQuestion {
    pub fn item_id(&self) -> ItemId {
        self.item
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }

    pub fn request_event(&self) -> EventId {
        self.request_event
    }

    pub fn tool_call(&self) -> ItemId {
        self.target.tool_call()
    }

    pub fn target(&self) -> &QuestionTarget {
        &self.target
    }

    pub fn request(&self) -> &QuestionRequest {
        &self.request
    }
}

/// One not-yet-started sibling closed before a question suspends its turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiblingClosure {
    item: ItemId,
    seq: Seq,
}

impl SiblingClosure {
    pub fn item(&self) -> ItemId {
        self.item
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }
}

/// The pure suspension plan consumed by the journal's existing sibling-close
/// path before control returns to a headless caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionSuspension {
    siblings: Vec<ItemId>,
    closures: Vec<SiblingClosure>,
}

impl QuestionSuspension {
    pub fn sibling_items(&self) -> &[ItemId] {
        &self.siblings
    }

    pub fn sibling_closures(&self) -> &[SiblingClosure] {
        &self.closures
    }
}

/// Everything resume needs to close the original pure model-tool call and
/// continue the turn that requested it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionContinuation {
    pub question: ItemId,
    pub turn: TurnId,
    pub original_turn: TurnId,
    pub tool_call: ItemId,
    pub tool_result: ToolResult,
    ok: bool,
}

impl QuestionContinuation {
    pub fn continues_original_turn(&self) -> bool {
        self.turn == self.original_turn
    }

    pub fn ok(&self) -> bool {
        self.ok
    }
}

/// One money observation, in the order it was applied.
///
/// One entry per `UsageUpdated`, not one per attempt: providers report usage
/// in increments, and giving each increment its own entry means accumulation
/// is just the fold, with no second addition rule for `CostDelta` to disagree
/// with [`CumulativeCost::add_delta`]. A correction collapses all of one
/// attempt's entries into a single revised one. See
/// `ARCHITECTURE.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charge {
    /// Which durable generation fact owns this spend, or whether it predates
    /// generation attribution entirely.
    pub attribution: ChargeAttribution,
    /// The tokens observed.
    pub usage: Usage,
    /// What those tokens cost, or why that is not known.
    pub cost: CostDelta,
}

/// Durable ownership of one ledger contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChargeAttribution {
    /// Spend written before attempts carried durable role and start time.
    LegacyUnattributed,
    /// Spend joined to the generation that originally opened its attempt.
    Generation {
        attempt: AttemptRef,
        role: GenerationRole,
        started_at: Timestamp,
    },
}

/// An attempt that has started and not yet settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveAttempt {
    /// The generation the attempt was started under. A settlement naming a
    /// different one is refused rather than silently re-attributed.
    pub generation: GenerationId,
    /// The accounting line this generation contributes to.
    pub role: GenerationRole,
    /// The original start observation time, retained across later bookings
    /// and corrections.
    pub started_at: Timestamp,
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
    /// A hook invocation id is already live.
    HookInvocationAlreadyStarted,
    /// A hook invocation id has already settled and cannot be reopened.
    HookInvocationAlreadySettled,
    /// A settlement named an invocation that never started.
    HookInvocationUnknown,
    /// The subject correlation does not agree with the event envelope.
    HookSubjectMismatch,
    /// A hook-produced effect attempted to recursively start another hook.
    RecursiveHookInvocation,
    /// A settlement named a different hook than its start.
    HookInvocationHookMismatch,
    /// A settlement's turn differs from its start.
    HookInvocationTurnMismatch,
    /// A settlement's item differs from its start.
    HookInvocationItemMismatch,
    /// A settlement is not causally parented by its start event.
    HookInvocationParentMismatch,
    /// A settlement was not produced by the hook named by its start.
    HookProducerMismatch,
    /// Settlement feedback claimed a source other than the invoked hook.
    HookFeedbackSourceMismatch,
    /// An observer attempted to block an effect that had already settled.
    HookBlockAtObserver,
    /// Escalation was requested outside the sole `PreToolUse` owner.
    HookEscalationOutsidePreToolUse,
    /// The occurrence key was already reached and cannot be rebound.
    HookOccurrenceAlreadyReached,
    /// A start or settlement named no accepted reached occurrence.
    HookOccurrenceUnknown,
    /// The occurrence was not causally parented by its real owner event.
    HookOccurrenceParentMismatch,
    /// The occurrence plan contains an unknown hook or changes catalogue order.
    HookOccurrencePlanMismatch,
    /// A start did not name the next hook in the frozen occurrence plan.
    HookOccurrenceOrderMismatch,
    /// A start's producer differs from the producer that reached the occurrence.
    HookOccurrenceProducerMismatch,
    /// A start or settlement named a different reached occurrence.
    HookInvocationOccurrenceMismatch,
    /// A child fact named an invocation with no durable request.
    SubagentInvocationUnknown,
    /// One invocation was requested more than once.
    SubagentAlreadyRequested,
    /// One invocation was spawned more than once.
    SubagentAlreadySpawned,
    /// One invocation was settled more than once.
    SubagentAlreadySettled,
    /// A spawned or terminal child id disagreed with its admitted run.
    SubagentChildMismatch,
    /// Refusal/admission or success/failure facts formed an impossible terminal.
    SubagentTerminalMismatch,
}

/// An event whose effect the reducer declined to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub event: EventId,
    pub reason: RefusalReason,
}

/// One prompt accepted into the durable queue ledger.
///
/// Its class is deliberately not stored. The event's turn supplies the
/// durable context from which [`SessionState::pending_steers`] and
/// [`SessionState::queued_prompts`] project it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedPrompt {
    id: QueuedPromptId,
    text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<ImageRef>,
    turn: TurnId,
    seq: Seq,
}

/// Durable queue facts projected from the session event log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStatus {
    pub pending_steers: usize,
    pub queued: usize,
    pub interrupt_pending: bool,
}

impl QueuedPrompt {
    pub fn id(&self) -> QueuedPromptId {
        self.id
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn images(&self) -> &[ImageRef] {
        &self.images
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }
}

/// The reduction of a `DomainEvent` log: the harness's entire semantic
/// state. Built exclusively by `apply`, which is pure and sync (ARCH §1, §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(default)]
    pub context_request: Option<crate::context::ContextRequest>,
    #[serde(default)]
    pub context_cooldown: u32,
    /// Latest admitted execution authority, reduced from the durable session log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_observation: Option<crate::permit::ExecObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_snapshot: Option<crate::session::PromptSnapshot>,
    /// The `seq` carried by the most recently applied event. `apply` trusts
    /// the caller's replay order and never reorders, clamps or rejects on
    /// this value — see `Seq`'s and `Timestamp`'s doc comments.
    pub last_seq: Seq,
    /// The tagged source boundary that created this fork or compaction target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<SessionLineage>,
    /// Mutable display metadata, reduced from the authoritative event stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Compact resume description derived from durable conversation events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Completed turns whose text could not produce a synopsis.
    #[serde(default)]
    pub summary_failures: u32,
    /// A soft-deleted session remains readable only for internal lineage replay.
    #[serde(default)]
    pub deleted: bool,
    /// Whether this session is running, has been asked to stop at its next
    /// safe boundary, or has reached one.
    #[serde(default)]
    pub pause: PauseState,
    /// What each `_ego/pause` and `_ego/resume` was answered with, kept past
    /// the episode that answered it so a retry is replayed and not re-run.
    #[serde(default, skip_serializing_if = "HoldReceipts::is_empty")]
    pub holds: HoldReceipts,
    /// The session's current goal, if one has been set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalRecord>,
    /// Per-turn state, keyed by turn id.
    pub turns: HashMap<TurnId, TurnState>,
    /// Per-item state, keyed by item id.
    pub items: HashMap<ItemId, ItemState>,
    #[serde(default)]
    pub detached_execs: BTreeMap<DetachedExecId, Option<DetachedExecSnapshot>>,
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
    /// Grows by one entry per admitted attempt and is not pruned, for the same
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
    /// The latest complete admitted hook catalogue.
    hook_catalogue: HookCatalogueEvidence,
    /// Real owner occurrences in durable reach order.
    hook_occurrences: Vec<ReachedHookOccurrence>,
    /// Hook invocations that started but have not settled.
    live_hooks: HashMap<HookInvocationId, LiveHookInvocation>,
    /// Accepted hook settlements, in application order.
    hook_settlements: Vec<HookSettlement>,
    /// Every invocation id that has settled, so it cannot be reopened.
    settled_hooks: HashSet<HookInvocationId>,
    /// Subagent lifecycle events, in application order.
    pub subagents: Vec<SubagentEvent>,
    /// The latest complete admitted skill catalogue.
    skill_catalogue_generation: Option<SkillCatalogueGeneration>,
    skill_catalogue: SkillCatalogueEvidence,
    /// Rendered instruction and guarded resource facts, in application order.
    skill_events: Vec<SkillEvent>,
    /// Error messages, in application order.
    pub errors: Vec<String>,
    /// Compaction summaries, in application order.
    pub compactions: Vec<String>,
    /// Fork records, in application order.
    pub forks: Vec<ForkRecord>,
    /// Every unique queue acceptance, retained in authoritative event order.
    #[serde(default)]
    queued_prompt_ledger: Vec<QueuedPrompt>,
    /// Queue ids consumed by their first correlated `UserPrompt`, in
    /// consumption order. Duplicate correlations do not add another entry.
    #[serde(default)]
    consumed_prompts: Vec<QueuedPromptId>,
    /// Greatest accepted FIFO tail through which delivery was expedited.
    #[serde(default)]
    expedited_prompt_through: Option<QueuedPromptId>,
    /// The sole requested-but-unsettled human-input item.
    #[serde(default)]
    pending_question: Option<PendingQuestion>,
    /// Settlements are retained both for deterministic completion recovery
    /// and to distinguish duplicate resume from an identity never observed.
    #[serde(default)]
    settled_questions: HashMap<ItemId, SettledQuestion>,
    /// Turns whose not-yet-started sibling batch was closed for suspension.
    #[serde(default)]
    question_sibling_closures: HashMap<TurnId, Vec<SiblingClosure>>,
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
            context_request: None,
            context_cooldown: 0,
            execution_observation: None,
            usage: Usage::IDENTITY,
            last_seq: Seq::default(),
            lineage: None,
            prompt_snapshot: None,
            title: None,
            summary: None,
            summary_failures: 0,
            deleted: false,
            pause: PauseState::Running,
            holds: HoldReceipts::default(),
            goal: None,
            turns: HashMap::default(),
            items: HashMap::default(),
            detached_execs: BTreeMap::default(),
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
            hook_catalogue: HookCatalogueEvidence::default(),
            hook_occurrences: Vec::default(),
            live_hooks: HashMap::default(),
            hook_settlements: Vec::default(),
            settled_hooks: HashSet::default(),
            subagents: Vec::default(),
            skill_catalogue_generation: None,
            skill_catalogue: SkillCatalogueEvidence::default(),
            skill_events: Vec::default(),
            errors: Vec::default(),
            compactions: Vec::default(),
            forks: Vec::default(),
            queued_prompt_ledger: Vec::default(),
            consumed_prompts: Vec::default(),
            expedited_prompt_through: None,
            pending_question: None,
            settled_questions: HashMap::default(),
            question_sibling_closures: HashMap::default(),
        }
    }
}

impl SessionState {
    fn refresh_synopsis(&mut self, turn: TurnId) {
        let prompt = self
            .items
            .values()
            .filter(|item| item.turn == turn && item.kind == ItemKind::UserPrompt)
            .min_by_key(|item| item.seq)
            .map(|item| item.text.as_str())
            .filter(|text| !text.trim().is_empty());
        let answer = self
            .items
            .values()
            .filter(|item| item.turn == turn && item.kind == ItemKind::Assistant)
            .max_by_key(|item| item.seq)
            .map(|item| item.text.as_str())
            .filter(|text| !text.trim().is_empty());
        let (Some(prompt), Some(answer)) = (prompt, answer) else {
            self.summary_failures = self.summary_failures.saturating_add(1);
            return;
        };
        if self.title.is_none() {
            let first_prompt = self
                .items
                .values()
                .filter(|item| item.kind == ItemKind::UserPrompt)
                .filter(|item| !item.text.trim().is_empty())
                .min_by_key(|item| item.seq)
                .map_or(prompt, |item| item.text.as_str());
            self.title = Some(synopsis_excerpt(first_prompt, 80));
        }
        self.summary = Some(synopsis_excerpt(&format!("{prompt} — {answer}"), 300));
    }

    pub fn unsettled_detached_execs(&self) -> Vec<DetachedExecId> {
        self.detached_execs
            .iter()
            .filter_map(|(id, snapshot)| snapshot.is_none().then_some(*id))
            .collect()
    }
    /// Rebuild the pure state from durable events in their supplied order.
    pub fn replay(events: impl IntoIterator<Item = DomainEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(&event);
        }
        state
    }

    pub fn turn(&self, turn: TurnId) -> Option<&TurnState> {
        self.turns.get(&turn)
    }

    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.pending_question.as_ref().or_else(|| {
            // A durable answer can precede continuation acceptance/claim. Recovery
            // requires a fresh explicit resume; it never repeats an effect itself.
            self.settled_questions
                .values()
                .filter(|settled| {
                    matches!(settled.outcome, QuestionOutcome::Answered(_))
                        && matches!(
                            settled.pending.request.mode,
                            crate::question::QuestionMode::Round(_)
                        )
                        && matches!(
                            settled.pending.target,
                            QuestionTarget::ForeignContinuation { .. }
                        )
                        && self
                            .items
                            .get(&settled.pending.target.tool_call())
                            .is_some_and(|item| !item.completed)
                })
                .max_by_key(|settled| self.items.get(&settled.pending.item).map(|item| item.seq))
                .map(|settled| &settled.pending)
        })
    }

    pub fn settled_question(&self, question: ItemId) -> Option<&SettledQuestion> {
        self.settled_questions.get(&question)
    }

    /// Whether one durable answer settlement owns this exact foreign
    /// continuation lifecycle parent.
    pub(crate) fn owns_foreign_continuation_settlement(
        &self,
        continuation_id: &str,
        turn: TurnId,
        tool_call: ItemId,
        settlement: EventId,
    ) -> bool {
        self.settled_questions.values().any(|settled| {
            settled.pending.turn == turn
                && settled.pending.target.tool_call() == tool_call
                && settled.settlement_event == settlement
                && matches!(
                    &settled.pending.target,
                    QuestionTarget::ForeignContinuation {
                        continuation_id: candidate,
                        ..
                    } if candidate == continuation_id
                )
        })
    }

    /// Parse a resume value against the pending request without changing
    /// state. The caller persists `QuestionSettled` before applying anything.
    pub fn answer_pending(
        &self,
        question: ItemId,
        json: &str,
    ) -> Result<QuestionAnswer, QuestionError> {
        let pending = self.pending_for(question)?;
        let answer = QuestionAnswer::from_json(&pending.request, json)?;
        if let Some(settled) = self.settled_questions.get(&question)
            && settled.outcome != QuestionOutcome::Answered(answer.clone())
        {
            return Err(QuestionError::AlreadySettled);
        }
        Ok(answer)
    }

    /// Validate an already parsed answer and prepare the pure tool result for
    /// the original turn. Durable events remain the only mutation authority.
    pub fn resume_question(
        &self,
        question: ItemId,
        answer: QuestionAnswer,
    ) -> Result<QuestionContinuation, QuestionError> {
        let pending = self.pending_for(question)?;
        answer.validate_for(&pending.request)?;
        if matches!(&pending.target, QuestionTarget::ForeignContinuation { .. }) {
            return Err(QuestionError::ForeignContinuationRequired);
        }
        Ok(QuestionContinuation {
            question,
            turn: pending.turn,
            original_turn: pending.turn,
            tool_call: pending.target.tool_call(),
            tool_result: ToolResult::builtin(answer.model_text()),
            ok: true,
        })
    }

    fn pending_for(&self, question: ItemId) -> Result<&PendingQuestion, QuestionError> {
        match self.pending_question() {
            Some(pending) if pending.item == question => Ok(pending),
            Some(_) => Err(QuestionError::WrongQuestion),
            None if self.settled_questions.contains_key(&question) => {
                Err(QuestionError::AlreadySettled)
            }
            None => Err(QuestionError::NoPendingQuestion),
        }
    }

    /// Recreate the pure result for a settlement that survived a process exit
    /// before its correlated `ToolCallCompleted` was appended.
    pub fn complete_settled_question(
        &self,
        question: ItemId,
    ) -> Result<QuestionContinuation, QuestionError> {
        let settled = self
            .settled_questions
            .get(&question)
            .ok_or(QuestionError::NoPendingQuestion)?;
        if matches!(
            &settled.pending.target,
            QuestionTarget::ForeignContinuation { .. }
        ) {
            return Err(QuestionError::ForeignContinuationRequired);
        }
        if self
            .items
            .get(&settled.pending.target.tool_call())
            .is_some_and(|item| item.completed)
        {
            return Err(QuestionError::AlreadySettled);
        }
        let (ok, text) = match &settled.outcome {
            QuestionOutcome::Answered(answer) => (true, answer.model_text()),
            QuestionOutcome::Declined => (false, "question declined".to_string()),
            QuestionOutcome::Cancelled => (false, "question cancelled".to_string()),
            QuestionOutcome::Unavailable | QuestionOutcome::MemberCompleted { .. } => {
                return Err(QuestionError::NoPendingQuestion);
            }
        };
        Ok(QuestionContinuation {
            question,
            turn: settled.pending.turn,
            original_turn: settled.pending.turn,
            tool_call: settled.pending.target.tool_call(),
            tool_result: ToolResult::builtin(text),
            ok,
        })
    }

    /// Record the ordered sibling-close plan used before returning a durable
    /// question suspension. The journal turns this plan into ordinary failed
    /// `ToolCallCompleted` events.
    pub fn suspend_question(
        &mut self,
        question: ItemId,
        siblings: impl IntoIterator<Item = ItemId>,
    ) -> Result<QuestionSuspension, QuestionError> {
        let turn = self.pending_for(question)?.turn;
        let siblings: Vec<ItemId> = siblings.into_iter().collect();
        let mut seq = self.last_seq;
        let closures: Vec<SiblingClosure> = siblings
            .iter()
            .copied()
            .map(|item| SiblingClosure {
                item,
                seq: seq.next(),
            })
            .collect();
        self.question_sibling_closures
            .insert(turn, closures.clone());
        Ok(QuestionSuspension { siblings, closures })
    }

    pub fn not_started_siblings_closed(&self, turn: TurnId) -> bool {
        self.question_sibling_closures.contains_key(&turn)
    }

    pub fn has_turn_completed(&self, turn: TurnId) -> bool {
        self.turns.get(&turn).is_some_and(|state| state.completed)
    }

    pub fn refuse_new_prompt(&self, _turn: TurnId) -> Result<(), QuestionError> {
        if self.pending_question().is_some() {
            Err(QuestionError::PendingQuestion)
        } else {
            Ok(())
        }
    }

    /// Incomplete effect calls excluding the pure call intentionally waiting
    /// on the pending question.
    pub fn torn_tool_errors(&self) -> Vec<ItemId> {
        let pending_call = self
            .pending_question
            .as_ref()
            .map(|pending| pending.target.tool_call());
        self.items
            .iter()
            .filter_map(|(id, item)| {
                (item.kind == ItemKind::ToolCall && !item.completed && Some(*id) != pending_call)
                    .then_some(*id)
            })
            .collect()
    }

    /// The terminal-facing queue projection. The reducer remains its sole
    /// owner; interactive control carries intents, not a second copy.
    pub fn queue_status(&self) -> QueueStatus {
        QueueStatus {
            pending_steers: self.pending_steers().len(),
            queued: self.queued_prompts().len(),
            interrupt_pending: self.interrupt_pending(),
        }
    }

    /// Accepted prompts without a correlated consumption, in FIFO order.
    pub fn unacknowledged_prompts(&self) -> Vec<&QueuedPrompt> {
        self.queued_prompt_ledger
            .iter()
            .filter(|prompt| !self.consumed_prompts.contains(&prompt.id))
            .collect()
    }

    /// Unacknowledged input accepted while its regular turn was active.
    pub fn pending_steers(&self) -> Vec<&QueuedPrompt> {
        self.unacknowledged_prompts()
            .into_iter()
            .filter(|prompt| self.is_pending_steer(prompt))
            .collect()
    }

    /// Unacknowledged input accepted outside a steerable regular turn.
    pub fn queued_prompts(&self) -> Vec<&QueuedPrompt> {
        self.unacknowledged_prompts()
            .into_iter()
            .filter(|prompt| !self.is_pending_steer(prompt))
            .collect()
    }

    /// The greatest durable expedited FIFO tail accepted by this session.
    pub fn expedited_through(&self) -> Option<QueuedPromptId> {
        self.expedited_prompt_through
    }

    /// Whether an expedited prefix still contains unconsumed input.
    pub fn interrupt_pending(&self) -> bool {
        let Some(through) = self.expedited_prompt_through else {
            return false;
        };
        let Some(last) = self
            .queued_prompt_ledger
            .iter()
            .position(|prompt| prompt.id == through)
        else {
            return false;
        };
        self.queued_prompt_ledger[..=last]
            .iter()
            .any(|prompt| !self.consumed_prompts.contains(&prompt.id))
    }

    /// Whether `id` has one accepted consumption correlation.
    pub fn consumed_prompt_count(&self, id: QueuedPromptId) -> usize {
        usize::from(self.consumed_prompts.contains(&id))
    }

    /// Queue ids in the order their first consumption was observed.
    pub fn consumed_prompt_ids(&self) -> Vec<QueuedPromptId> {
        self.consumed_prompts.clone()
    }

    fn is_pending_steer(&self, prompt: &QueuedPrompt) -> bool {
        let has_user_prompt = self
            .items
            .values()
            .any(|item| item.turn == prompt.turn && item.kind == ItemKind::UserPrompt);
        let completed = self
            .turns
            .get(&prompt.turn)
            .is_some_and(|turn| turn.completed);
        has_user_prompt && !completed
    }

    fn accept_prompt(
        &mut self,
        ev: &DomainEvent,
        id: QueuedPromptId,
        text: &str,
        images: &[ImageRef],
    ) {
        if self
            .queued_prompt_ledger
            .iter()
            .any(|prompt| prompt.id == id)
        {
            return;
        }
        self.queued_prompt_ledger.push(QueuedPrompt {
            id,
            text: text.to_string(),
            images: images.to_vec(),
            turn: ev.env.turn,
            seq: ev.env.seq,
        });
    }

    pub(crate) fn can_expedite_prompt(&self, through: QueuedPromptId) -> bool {
        let Some(candidate) = self
            .queued_prompt_ledger
            .iter()
            .position(|prompt| prompt.id == through)
        else {
            return false;
        };
        let latest_unacknowledged = self
            .queued_prompt_ledger
            .iter()
            .rposition(|prompt| !self.consumed_prompts.contains(&prompt.id));
        if latest_unacknowledged != Some(candidate) {
            return false;
        }
        let current = self.expedited_prompt_through.and_then(|id| {
            self.queued_prompt_ledger
                .iter()
                .position(|prompt| prompt.id == id)
        });
        current.is_none_or(|current| candidate > current)
    }

    fn expedite_prompt(&mut self, through: QueuedPromptId) {
        if self.can_expedite_prompt(through) {
            self.expedited_prompt_through = Some(through);
        }
    }

    fn consume_prompt(&mut self, id: QueuedPromptId) {
        let accepted = self
            .queued_prompt_ledger
            .iter()
            .any(|prompt| prompt.id == id);
        if accepted && !self.consumed_prompts.contains(&id) {
            self.consumed_prompts.push(id);
        }
    }

    pub fn hook_catalogue(&self) -> &HookCatalogueEvidence {
        &self.hook_catalogue
    }

    pub fn live_hooks(&self) -> &HashMap<HookInvocationId, LiveHookInvocation> {
        &self.live_hooks
    }

    pub fn hook_occurrences(&self) -> &[ReachedHookOccurrence] {
        &self.hook_occurrences
    }

    pub fn hook_occurrence(&self, key: &HookOccurrenceKey) -> Option<&ReachedHookOccurrence> {
        self.hook_occurrences
            .iter()
            .find(|occurrence| occurrence.key() == key)
    }

    pub fn hook_occurrence_settlements(&self, event: EventId) -> Vec<&HookSettlement> {
        self.hook_settlements
            .iter()
            .filter(|settlement| settlement.occurrence == event)
            .collect()
    }

    pub fn hook_occurrence_is_closed(&self, event: EventId) -> bool {
        self.hook_occurrences
            .iter()
            .find(|occurrence| occurrence.event == event)
            .is_some_and(|occurrence| {
                self.hook_settlements
                    .iter()
                    .filter(|settlement| settlement.occurrence == event)
                    .count()
                    == occurrence.plan.len()
            })
    }

    pub fn hook_settlements(&self) -> &[HookSettlement] {
        &self.hook_settlements
    }

    pub fn skill_catalogue_generation(&self) -> Option<SkillCatalogueGeneration> {
        self.skill_catalogue_generation
    }

    pub fn skill_catalogue(&self) -> &SkillCatalogueEvidence {
        &self.skill_catalogue
    }

    pub fn skill_events(&self) -> &[SkillEvent] {
        &self.skill_events
    }

    /// Every structured reference the current snapshot keeps reachable.
    ///
    /// Semantic traversal belongs in core. The filesystem adapter receives
    /// only generic references and never learns about event variants.
    pub fn blob_references(&self) -> Vec<BlobRef> {
        let mut references = Vec::new();
        for prompt in &self.queued_prompt_ledger {
            references.extend(prompt.images.iter().map(|image| image.reference().clone()));
        }
        for item in self.items.values() {
            references.extend(item.images.iter().map(|image| image.reference().clone()));
            if let Some(result) = &item.tool_result {
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
        }
        references.extend(
            self.hook_settlements
                .iter()
                .filter_map(|settlement| settlement.result().feedback())
                .map(|feedback| feedback.reference().blob_ref()),
        );
        for event in &self.subagents {
            match event {
                SubagentEvent::Requested { task, .. } => references.push(task.clone()),
                SubagentEvent::Settled { terminal, .. } => match terminal {
                    SubagentTerminal::Returned { final_message }
                    | SubagentTerminal::ReturnBlocked { final_message, .. } => {
                        references.push(final_message.clone());
                    }
                    SubagentTerminal::Refused { .. } | SubagentTerminal::Failed { .. } => {}
                },
                SubagentEvent::Spawned { .. } => {}
            }
        }
        for event in &self.skill_events {
            match event {
                SkillEvent::Instruction(instruction) => {
                    references.push(instruction.admission.clone());
                    references.push(instruction.rendered.clone());
                }
                SkillEvent::Resource { content, .. } => references.push(content.clone()),
            }
        }
        references
    }

    fn complete_turn(&mut self, turn_id: TurnId, ok: bool, exit_reason: ExitReason) {
        if self.items.values().any(|item| {
            item.turn == turn_id
                && matches!(
                    item.kind,
                    ItemKind::UserPrompt | ItemKind::Assistant | ItemKind::ToolCall
                )
        }) {
            self.context_cooldown = self.context_cooldown.saturating_sub(1);
        }
        if let Some(goal) = &mut self.goal {
            goal.clear_blocker_after_completed_turn(turn_id);
        }
        let turn = self.turns.entry(turn_id).or_default();
        turn.completed = true;
        turn.ok = ok;
        turn.exit_reason = Some(exit_reason);
        if ok {
            self.refresh_synopsis(turn_id);
        }
    }

    fn apply_goal(&mut self, ev: &DomainEvent) {
        match &ev.kind {
            EventKind::GoalSet {
                id,
                objective,
                owner,
                ceilings,
                derived_from,
            } => {
                let may_set = self
                    .goal
                    .as_ref()
                    .is_none_or(|goal| goal.state != GoalState::Active && goal.id != *id);
                if may_set {
                    self.goal = Some(GoalRecord {
                        id: *id,
                        objective: objective.clone(),
                        owner: *owner,
                        ceilings: ceilings.clone(),
                        derived_from: *derived_from,
                        created_seq: ev.env.seq,
                        state: GoalState::Active,
                        continuation_turns: 0,
                        continuation_budget: None,
                        check_stop: None,
                        blocker_reason: None,
                        blocker_observations: 0,
                        achieved_by: None,
                        achievement_evidence: None,
                        blocker_turns: Vec::new(),
                    });
                }
            }
            EventKind::GoalAllowanceInherited {
                id,
                continuation_budget,
                continuation_turns,
            } => {
                if let Some(goal) = self.goal.as_mut().filter(|goal| goal.id == *id) {
                    goal.continuation_budget = *continuation_budget;
                    goal.continuation_turns = *continuation_turns;
                }
            }
            EventKind::GoalCheckConfigured {
                id,
                continuation_budget,
            } => {
                if let Some(goal) = self
                    .goal
                    .as_mut()
                    .filter(|goal| goal.id == *id && goal.continuation_budget.is_none())
                {
                    goal.continuation_budget = Some(*continuation_budget);
                }
            }
            EventKind::GoalContinuation { id, text } => {
                if let Some(goal) = self.goal.as_mut().filter(|goal| goal.id == *id) {
                    goal.continuation_turns = goal.continuation_turns.saturating_add(1);
                    goal.check_stop = None;
                }
                if let Some(item) = self.item_mut(ev, ItemKind::GoalContinuation) {
                    item.text = text.clone();
                }
            }
            EventKind::GoalCheckStopped { id, reason } => {
                if let Some(goal) = self.goal.as_mut().filter(|goal| goal.id == *id) {
                    goal.check_stop = Some(reason.clone());
                }
            }
            EventKind::GoalUpdated { id, objective } => {
                if let Some(goal) = self.goal.as_mut().filter(|goal| {
                    goal.id == *id
                        && goal.state == GoalState::Active
                        && goal.owner == crate::goal::GoalOwner::Automated
                }) {
                    goal.objective = objective.clone();
                    goal.blocker_reason = None;
                    goal.blocker_observations = 0;
                    goal.blocker_turns.clear();
                }
            }
            EventKind::GoalTransitioned { id, transition } => {
                self.transition_goal(ev, id, transition)
            }
            _ => unreachable!("only goal events reach the goal reducer"),
        }
    }

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
            EventKind::SessionTitleChanged { title } => self.title = title.clone(),
            EventKind::SessionDeleted => self.deleted = true,
            // Total like every other arm, and the transitions it will not make
            // are as load-bearing as the ones it will. A second request while
            // one is already outstanding leaves the first standing, because
            // the first is the one the boundary will answer and overwriting it
            // would tell a retrying host its own request had been honoured. A
            // boundary for a request nothing asked for is ignored rather than
            // trusted: it can only come from a log that lost its request, and
            // reducing it would manufacture the very boundary the ADR says is
            // never manufactured.
            EventKind::SessionPauseRequested { request } => {
                if self.pause == PauseState::Running {
                    self.pause = PauseState::Requested { request: *request };
                }
            }
            EventKind::SessionPaused { request } => {
                if self.pause == (PauseState::Requested { request: *request }) {
                    self.pause = PauseState::Paused { request: *request };
                }
                // Recorded even when the boundary was ignored above. The
                // receipt answers "was this request already honoured", and a
                // boundary this reduction declined was still honoured by the
                // writer that appended it.
                self.holds.paused.insert(*request);
            }
            // The hold itself is untouched: this request was told about a
            // boundary it did not establish, and only the receipt is its own.
            EventKind::SessionPauseObserved { request } => {
                self.holds.paused.insert(*request);
            }
            // Unconditional, unlike the two above: a resume releases the
            // session whatever stage the pause had reached, including a
            // request whose boundary never arrived. Refusing to release a
            // pending pause would leave a session nothing could free.
            EventKind::SessionResumed { request } => {
                self.pause = PauseState::Running;
                self.holds.resumed.insert(*request);
            }
            EventKind::GoalSet { .. }
            | EventKind::GoalAllowanceInherited { .. }
            | EventKind::GoalCheckConfigured { .. }
            | EventKind::GoalContinuation { .. }
            | EventKind::GoalCheckStopped { .. }
            | EventKind::GoalUpdated { .. }
            | EventKind::GoalTransitioned { .. } => self.apply_goal(ev),
            EventKind::ContextRequested { request } => {
                self.context_request = Some(request.clone());
            }
            EventKind::ContextRequestConsumed => {
                self.context_request = None;
            }
            EventKind::ContextCooldown { turns } => {
                self.context_cooldown = *turns;
            }
            EventKind::PromptQueued { id, text, images } => {
                self.accept_prompt(ev, *id, text, images)
            }
            EventKind::PromptDeliveryExpedited { through } => self.expedite_prompt(*through),
            EventKind::PromptRetracted { id } => {
                if self
                    .unacknowledged_prompts()
                    .last()
                    .map(|prompt| prompt.id())
                    == Some(*id)
                {
                    self.queued_prompt_ledger.retain(|prompt| prompt.id != *id);
                }
            }
            EventKind::UserPrompt {
                text,
                images,
                queued_prompt,
            } => {
                if let Some(id) = queued_prompt {
                    self.consume_prompt(*id);
                }
                if let Some(item) = self.item_mut(ev, ItemKind::UserPrompt) {
                    item.text = text.clone();
                    item.images = images.clone();
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
            EventKind::ProviderContent { state, visible } => {
                let kind = if *visible {
                    ItemKind::Assistant
                } else {
                    ItemKind::Reasoning
                };
                if let Some(item) = self.item_mut(ev, kind) {
                    item.provider_content = Some(state.clone());
                    item.completed = true;
                }
            }
            EventKind::ReasoningState { state } => {
                if let Some(item) = self.item_mut(ev, ItemKind::Reasoning) {
                    item.reasoning_state = Some(state.clone());
                }
            }
            EventKind::ContextCommandStarted { name, args } => {
                if let Some(item) = self.item_mut(ev, ItemKind::ToolCall) {
                    item.tool_call_started.get_or_insert(ev.env.id);
                    item.context_command = true;
                    item.tool_name = Some(name.clone());
                    item.args = args.clone();
                }
            }
            EventKind::ToolCallStarted {
                name,
                args,
                call_id,
            } => {
                if let Some(item) = self.item_mut(ev, ItemKind::ToolCall) {
                    item.tool_call_started.get_or_insert(ev.env.id);
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
            EventKind::ToolCallCompleted { ok, result } => {
                if let Some(item) = self.item_mut(ev, ItemKind::ToolCall) {
                    item.text.clear();
                    item.tool_result = Some(result.clone());
                    item.completed = true;
                    item.ok = Some(*ok);
                }
            }
            EventKind::ToolCallMeasured { timing } => {
                if let Some(item) = self.item_mut(ev, ItemKind::ToolCall) {
                    item.tool_timing = Some(*timing);
                }
            }
            EventKind::HostContextAppended { text } => {
                if let Some(item) = self.item_mut(ev, ItemKind::HostContext) {
                    item.text = text.clone();
                }
            }
            EventKind::DetachedExecStarted { id } => {
                self.detached_execs.entry(*id).or_insert(None);
            }
            EventKind::DetachedExecSettled { snapshot } => {
                if let Some(entry @ None) = self.detached_execs.get_mut(&snapshot.id)
                    && snapshot.outcome.is_some()
                {
                    *entry = Some(snapshot.clone());
                    if let Some(item) = self.item_mut(ev, ItemKind::DetachedExecNotification) {
                        item.text = snapshot.model_text();
                    }
                }
            }
            EventKind::QuestionRequested { request, target } => {
                self.request_question(ev, request, target)
            }
            EventKind::QuestionMemberSettled { key, answer } => {
                if let Some(pending) = self.pending_question.as_mut()
                    && ev.env.item == Some(pending.item)
                    && ev.env.turn == pending.turn
                    && ev.env.parent == Some(pending.request_event)
                    && matches!(pending.target, QuestionTarget::ForeignContinuation { .. })
                    && let crate::question::QuestionMode::Round(round) = &mut pending.request.mode
                {
                    let _ = round.complete_member(key, answer.clone());
                }
            }
            EventKind::QuestionSettled { outcome } => self.settle_question(ev, outcome),
            EventKind::PermissionRequested { escalation, .. } => {
                self.permissions.push(PermissionEvent::Requested {
                    item: ev.env.item,
                    description: escalation.to_string(),
                });
            }
            EventKind::PermissionAmended { tool } => {
                self.permissions.push(PermissionEvent::Amended {
                    item: ev.env.item,
                    tool: tool.clone(),
                });
            }
            // `by` is not reduced. State answers "what is this session's
            // shape now"; who authorised a call is a fact about one decision,
            // it is already in the log verbatim, and nothing reads
            // `permissions` to ask. Keeping it here would put an `Authority`
            // inside the snapshot for no consumer.
            EventKind::PermissionDecided { allowed, .. } => {
                if let Some(allowed) = allowed {
                    self.permissions.push(PermissionEvent::Decided {
                        item: ev.env.item,
                        allowed: *allowed,
                    });
                }
            }
            EventKind::GenerationStarted {
                attempt,
                role,
                snapshot,
            } => {
                self.start(ev, attempt, *role);
                if self.generations.contains_key(&ev.env.id)
                    && let Some(snapshot) = snapshot
                {
                    self.prompt_snapshot = Some(snapshot.clone());
                }
            }
            EventKind::FallbackDecision { .. } => {}
            EventKind::ExecutionObserved { observation, .. } => {
                self.execution_observation = Some(observation.clone());
            }
            EventKind::ExecutionMeasured { .. } => {}
            EventKind::JudgeResponse { .. } | EventKind::SandboxResidualDecided { .. } => {}
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
                window,
            } => {
                // Replace, never add: occupancy is a level, and a compaction
                // makes it go down. The window replaces too — a session that
                // moved onto a fallback model moved onto its ceiling — but a
                // record that states none leaves the last stated one standing,
                // because "this build wrote no window" is not "the window went
                // away".
                self.context = ContextWindow {
                    used_tokens: Some(*used_tokens),
                    provenance: Some(*provenance),
                    window: window.or(self.context.window),
                };
            }
            EventKind::TurnCompleted { ok, exit_reason } => {
                self.complete_turn(ev.env.turn, *ok, *exit_reason);
            }
            EventKind::CompactionSeeded {
                source,
                source_seq,
                prompt_revision,
                checkpoint,
            } => {
                self.prompt_snapshot = None;
                self.lineage = Some(SessionLineage::Compaction {
                    source: *source,
                    source_seq: *source_seq,
                    prompt_revision: *prompt_revision,
                });
                if let Some(item) = self.item_mut(ev, ItemKind::ContextCheckpoint) {
                    item.text = checkpoint.clone();
                    item.completed = true;
                    item.ok = Some(true);
                }
                self.summary = Some(synopsis_excerpt(checkpoint, 300));
            }
            EventKind::Compacted { summary } => {
                self.prompt_snapshot = None;
                self.summary = Some(synopsis_excerpt(summary, 300));
                self.compactions.push(summary.clone());
            }
            EventKind::Forked {
                source,
                at_seq,
                snapshot,
            } => {
                self.prompt_snapshot = snapshot.clone();
                self.lineage = Some(SessionLineage::Fork {
                    source: *source,
                    at_seq: *at_seq,
                });
                self.forks.push(ForkRecord {
                    source: *source,
                    at_seq: *at_seq,
                });
            }
            EventKind::HookCatalogueAdmitted { descriptors } => {
                self.hook_catalogue = descriptors.clone();
            }
            EventKind::HookLifecycleReached {
                occurrence,
                subject,
                plan,
            } => self.reach_hook(ev, occurrence, *subject, plan),
            EventKind::HookInvocationStarted { .. } => self.start_hook(ev),
            EventKind::HookInvocationSettled {
                occurrence,
                hook,
                invocation,
                result,
            } => self.settle_hook(ev, *occurrence, *hook, *invocation, result),
            EventKind::SubagentRequested {
                invocation,
                task,
                selections,
            } => self.request_subagent(ev, *invocation, task, selections),
            EventKind::SubagentSpawned { invocation, child } => {
                self.spawn_subagent(ev, *invocation, *child);
            }
            EventKind::SubagentSettled {
                invocation,
                child,
                terminal,
            } => {
                self.settle_subagent(ev, *invocation, *child, terminal);
            }
            EventKind::SkillCatalogueAdmitted {
                generation,
                descriptors,
            } => {
                self.skill_catalogue_generation = Some(*generation);
                self.skill_catalogue = descriptors.clone();
            }
            EventKind::SkillInstructionAdmitted {
                invocation,
                skill,
                catalogue_generation,
                source,
                location,
                source_digest,
                admission,
                rendered,
                model,
                effort,
            } => self
                .skill_events
                .push(SkillEvent::Instruction(Box::new(SkillInstructionEvent {
                    event: ev.env.id,
                    turn: ev.env.turn,
                    item: ev.env.item,
                    parent: ev.env.parent,
                    invocation: *invocation,
                    skill: *skill,
                    catalogue_generation: *catalogue_generation,
                    source: *source,
                    location: location.clone(),
                    source_digest: *source_digest,
                    admission: admission.clone(),
                    rendered: rendered.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                }))),
            EventKind::SkillResourceAdmitted {
                invocation,
                skill,
                location,
                content,
            } => self.skill_events.push(SkillEvent::Resource {
                invocation: *invocation,
                skill: *skill,
                location: location.clone(),
                content: content.clone(),
            }),
            // `kind` is deliberately not reduced: `errors` is a list of
            // what was said, and nothing in the reducer branches on why.
            // Adding a field no reader consults would be a second place for
            // the classification to drift from the log's own copy.
            EventKind::Error { message, .. } => self.errors.push(message.clone()),
        }
    }

    fn request_question(
        &mut self,
        ev: &DomainEvent,
        request: &crate::question::QuestionRequest,
        target: &crate::question::QuestionTarget,
    ) {
        let Some(item) = ev.env.item else {
            return;
        };
        if request.validate().is_err()
            || (matches!(request.mode, crate::question::QuestionMode::Round(_))
                && matches!(target, QuestionTarget::ModelTool { .. }))
            || target.validate().is_err()
            || self.pending_question.is_some()
            || self.settled_questions.contains_key(&item)
        {
            return;
        }
        self.turns.entry(ev.env.turn).or_default();
        let _ = self.item_mut(ev, ItemKind::Question);
        self.pending_question = Some(PendingQuestion {
            item,
            turn: ev.env.turn,
            request_event: ev.env.id,
            target: target.clone(),
            request: request.clone(),
        });
    }

    fn settle_question(&mut self, ev: &DomainEvent, outcome: &crate::question::QuestionOutcome) {
        let Some(item) = ev.env.item else {
            return;
        };
        let Some(pending) = self.pending_question.as_ref() else {
            return;
        };
        let valid = pending.item == item
            && pending.turn == ev.env.turn
            && ev.env.parent == Some(pending.request_event)
            && match outcome {
                QuestionOutcome::Answered(answer) => answer.validate_for(&pending.request).is_ok(),
                QuestionOutcome::Declined | QuestionOutcome::Cancelled => true,
                // `Unavailable` is a host result, never a settlement:
                // it is exactly the state in which the request stays pending.
                QuestionOutcome::Unavailable | QuestionOutcome::MemberCompleted { .. } => false,
            };
        if valid && let Some(pending) = self.pending_question.take() {
            if let Some(question) = self.items.get_mut(&item) {
                question.completed = true;
                question.ok = Some(matches!(outcome, QuestionOutcome::Answered(_)));
            }
            self.settled_questions.insert(
                item,
                SettledQuestion {
                    pending,
                    settlement_event: ev.env.id,
                    outcome: outcome.clone(),
                },
            );
        }
    }

    fn transition_goal(
        &mut self,
        ev: &DomainEvent,
        id: &crate::event::id::GoalId,
        transition: &GoalTransition,
    ) {
        let Some(goal) = self.goal.as_mut().filter(|goal| goal.id == *id) else {
            return;
        };
        if goal.admits(transition).is_err() {
            return;
        }
        match transition {
            GoalTransition::BlockerObserved { reason } => {
                goal.observe_blocker(ev.env.turn, reason);
            }
            GoalTransition::Achieved {
                attested_by,
                evidence,
            } => {
                goal.state = GoalState::Achieved;
                goal.achieved_by = Some(*attested_by);
                goal.achievement_evidence = evidence.clone();
            }
        }
    }

    fn request_subagent(
        &mut self,
        ev: &DomainEvent,
        invocation: SubagentInvocationId,
        task: &BlobRef,
        selections: &RunSelections,
    ) {
        if self.subagents.iter().any(|event| match event {
            SubagentEvent::Requested {
                invocation: existing,
                ..
            }
            | SubagentEvent::Spawned {
                invocation: existing,
                ..
            }
            | SubagentEvent::Settled {
                invocation: existing,
                ..
            } => *existing == invocation,
        }) {
            return self.refuse(ev, RefusalReason::SubagentAlreadyRequested);
        }
        self.subagents.push(SubagentEvent::Requested {
            invocation,
            task: task.clone(),
            selections: Box::new(selections.clone()),
        });
    }

    fn spawn_subagent(&mut self, ev: &DomainEvent, invocation: SubagentInvocationId, child: RunId) {
        let requested = self.subagents.iter().any(|event| {
            matches!(event, SubagentEvent::Requested { invocation: existing, .. } if *existing == invocation)
        });
        if !requested {
            return self.refuse(ev, RefusalReason::SubagentInvocationUnknown);
        }
        if self.subagents.iter().any(|event| {
            matches!(event, SubagentEvent::Spawned { invocation: existing, .. } if *existing == invocation)
        }) {
            return self.refuse(ev, RefusalReason::SubagentAlreadySpawned);
        }
        if self.subagents.iter().any(|event| {
            matches!(event, SubagentEvent::Settled { invocation: existing, .. } if *existing == invocation)
        }) {
            return self.refuse(ev, RefusalReason::SubagentAlreadySettled);
        }
        if self.subagents.iter().any(|event| {
            matches!(event, SubagentEvent::Spawned { child: existing, .. } if *existing == child)
        }) {
            return self.refuse(ev, RefusalReason::SubagentChildMismatch);
        }
        self.subagents
            .push(SubagentEvent::Spawned { invocation, child });
    }

    fn settle_subagent(
        &mut self,
        ev: &DomainEvent,
        invocation: SubagentInvocationId,
        child: Option<RunId>,
        terminal: &SubagentTerminal,
    ) {
        let requested = self.subagents.iter().any(|event| {
            matches!(event, SubagentEvent::Requested { invocation: existing, .. } if *existing == invocation)
        });
        if !requested {
            return self.refuse(ev, RefusalReason::SubagentInvocationUnknown);
        }
        if self.subagents.iter().any(|event| {
            matches!(event, SubagentEvent::Settled { invocation: existing, .. } if *existing == invocation)
        }) {
            return self.refuse(ev, RefusalReason::SubagentAlreadySettled);
        }
        let spawned = self.subagents.iter().find_map(|event| match event {
            SubagentEvent::Spawned {
                invocation: existing,
                child,
            } if *existing == invocation => Some(*child),
            _ => None,
        });
        let valid = match terminal {
            SubagentTerminal::Refused { .. } => spawned.is_none() && child.is_none(),
            SubagentTerminal::Failed { settlement } => {
                !matches!(settlement, crate::run::RunSettlement::Completed)
                    && spawned.is_some()
                    && spawned == child
            }
            SubagentTerminal::Returned { .. } | SubagentTerminal::ReturnBlocked { .. } => {
                spawned.is_some() && spawned == child
            }
        };
        if !valid {
            return self.refuse(ev, RefusalReason::SubagentTerminalMismatch);
        }
        self.subagents.push(SubagentEvent::Settled {
            invocation,
            child,
            terminal: terminal.clone(),
        });
    }

    fn reach_hook(
        &mut self,
        ev: &DomainEvent,
        key: &HookOccurrenceKey,
        subject: HookSubject,
        plan: &HookPlan,
    ) {
        if !ev.env.producer.may_trigger_hooks() {
            return self.refuse(ev, RefusalReason::RecursiveHookInvocation);
        }
        if self.hook_occurrence(key).is_some() {
            return self.refuse(ev, RefusalReason::HookOccurrenceAlreadyReached);
        }
        if !hook_subject_matches_envelope(subject, ev.env.turn, ev.env.item) {
            return self.refuse(ev, RefusalReason::HookSubjectMismatch);
        }
        if let Err(reason) = hook_owner_matches_envelope(self, key, subject, ev) {
            return self.refuse(ev, reason);
        }
        if let HookOccurrenceOwner::Subagent { invocation } = key.owner()
            && !self.subagents.iter().any(|event| {
                matches!(
                    event,
                    SubagentEvent::Spawned {
                        invocation: existing,
                        ..
                    } if existing == invocation
                )
            })
        {
            return self.refuse(ev, RefusalReason::HookOccurrenceUnknown);
        }
        let catalogue = self.hook_catalogue.as_slice();
        let mut last_position = None;
        for entry in plan.as_slice() {
            let Some(position) = catalogue.iter().position(|descriptor| {
                descriptor.id() == entry.hook() && descriptor.activation() == entry.activation()
            }) else {
                return self.refuse(ev, RefusalReason::HookOccurrencePlanMismatch);
            };
            if last_position.is_some_and(|last| position <= last) {
                return self.refuse(ev, RefusalReason::HookOccurrencePlanMismatch);
            }
            last_position = Some(position);
        }
        self.hook_occurrences.push(ReachedHookOccurrence {
            event: ev.env.id,
            key: key.clone(),
            subject,
            plan: plan.clone(),
            producer: ev.env.producer.clone(),
            turn: ev.env.turn,
            item: ev.env.item,
            starts: Vec::new(),
        });
    }

    fn start_hook(&mut self, ev: &DomainEvent) {
        let EventKind::HookInvocationStarted {
            occurrence,
            hook,
            invocation,
            lifecycle,
            subject,
            activation,
        } = &ev.kind
        else {
            unreachable!("start_hook is called only for HookInvocationStarted")
        };
        let (occurrence, hook, invocation, lifecycle, subject, activation) = (
            *occurrence,
            *hook,
            *invocation,
            *lifecycle,
            *subject,
            *activation,
        );
        if self.settled_hooks.contains(&invocation) {
            return self.refuse(ev, RefusalReason::HookInvocationAlreadySettled);
        }
        if self.live_hooks.contains_key(&invocation) {
            return self.refuse(ev, RefusalReason::HookInvocationAlreadyStarted);
        }
        let Some(position) = self
            .hook_occurrences
            .iter()
            .position(|reached| reached.event == occurrence)
        else {
            return self.refuse(ev, RefusalReason::HookOccurrenceUnknown);
        };
        let reached = &self.hook_occurrences[position];
        let settled_count = self
            .hook_settlements
            .iter()
            .filter(|settlement| settlement.occurrence == occurrence)
            .count();
        if reached.starts.len() != settled_count {
            return self.refuse(ev, RefusalReason::HookOccurrenceOrderMismatch);
        }
        if ev.env.parent != Some(reached.event) {
            return self.refuse(ev, RefusalReason::HookOccurrenceParentMismatch);
        }
        if ev.env.producer != reached.producer {
            return self.refuse(ev, RefusalReason::HookOccurrenceProducerMismatch);
        }
        if lifecycle != reached.key.lifecycle()
            || subject != reached.subject
            || ev.env.turn != reached.turn
            || ev.env.item != reached.item
        {
            return self.refuse(ev, RefusalReason::HookInvocationOccurrenceMismatch);
        }
        let planned = reached.plan.as_slice().get(reached.starts.len());
        if planned
            .is_none_or(|planned| planned.hook() != hook || planned.activation() != activation)
        {
            return self.refuse(ev, RefusalReason::HookOccurrenceOrderMismatch);
        }
        self.hook_occurrences[position]
            .starts
            .push(HookOccurrenceStart {
                event: ev.env.id,
                hook,
                invocation,
                activation,
            });
        self.live_hooks.insert(
            invocation,
            LiveHookInvocation {
                occurrence,
                start: ev.env.id,
                hook,
                lifecycle,
                subject,
                activation,
                turn: ev.env.turn,
                item: ev.env.item,
            },
        );
    }

    fn settle_hook(
        &mut self,
        ev: &DomainEvent,
        occurrence: EventId,
        hook: HookId,
        invocation: HookInvocationId,
        result: &HookResult,
    ) {
        if self.settled_hooks.contains(&invocation) {
            return self.refuse(ev, RefusalReason::HookInvocationAlreadySettled);
        }
        let Some(live) = self.live_hooks.get(&invocation).cloned() else {
            return self.refuse(ev, RefusalReason::HookInvocationUnknown);
        };
        if occurrence != live.occurrence {
            return self.refuse(ev, RefusalReason::HookInvocationOccurrenceMismatch);
        }
        if hook != live.hook {
            return self.refuse(ev, RefusalReason::HookInvocationHookMismatch);
        }
        if ev.env.turn != live.turn {
            return self.refuse(ev, RefusalReason::HookInvocationTurnMismatch);
        }
        if ev.env.item != live.item {
            return self.refuse(ev, RefusalReason::HookInvocationItemMismatch);
        }
        if ev.env.parent != Some(live.start) {
            return self.refuse(ev, RefusalReason::HookInvocationParentMismatch);
        }
        if ev.env.producer != Producer::Hook(live.hook) {
            return self.refuse(ev, RefusalReason::HookProducerMismatch);
        }
        if result
            .feedback()
            .is_some_and(|feedback| feedback.hook() != live.hook)
        {
            return self.refuse(ev, RefusalReason::HookFeedbackSourceMismatch);
        }
        if let HookResult::Outcome { outcome } = result {
            if outcome.decision() == HookDecision::Block && live.lifecycle.phase().is_observer() {
                return self.refuse(ev, RefusalReason::HookBlockAtObserver);
            }
            if outcome.decision() == HookDecision::RequestEscalation
                && live.lifecycle != HookLifecycle::PreToolUse
            {
                return self.refuse(ev, RefusalReason::HookEscalationOutsidePreToolUse);
            }
        }

        self.live_hooks.remove(&invocation);
        self.settled_hooks.insert(invocation);
        self.hook_settlements.push(HookSettlement {
            event: ev.env.id,
            occurrence,
            hook,
            invocation,
            result: result.clone(),
            turn: ev.env.turn,
            item: ev.env.item,
        });
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
    fn start(&mut self, ev: &DomainEvent, attempt: &AttemptRef, role: GenerationRole) {
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
                role,
                started_at: ev.env.ts,
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
            return self.push_charge(ChargeAttribution::LegacyUnattributed, *usage, cost.clone());
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
        self.push_charge(
            ChargeAttribution::Generation {
                attempt,
                role: live.role,
                started_at: live.started_at,
            },
            *usage,
            cost.clone(),
        );
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
            // A missing provider report is represented by one zero-token
            // bookkeeping entry. The cost retains whether that absence was a
            // definitely-unsent zero or an unknown possibly-sent charge.
            UsageReport::Missing if booked == Some(Usage::IDENTITY) => booked,
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
            .filter(move |(_, charge)| {
                matches!(
                    charge.attribution,
                    ChargeAttribution::Generation { attempt, .. }
                        if attempt.attempt() == id
                )
            })
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
    fn push_charge(&mut self, attribution: ChargeAttribution, usage: Usage, cost: CostDelta) {
        self.usage.add(&usage);
        self.cost.add_delta(&cost);
        self.charges.push(Charge {
            attribution,
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

/// A UTF-8 byte ceiling bounds tokenizer output without depending on a model.
fn synopsis_excerpt(text: &str, max_bytes: usize) -> String {
    let plain = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut excerpt = String::new();
    for character in plain.chars() {
        if excerpt.len() + character.len_utf8() > max_bytes {
            break;
        }
        excerpt.push(character);
    }
    excerpt
}

fn hook_subject_matches_envelope(subject: HookSubject, turn: TurnId, item: Option<ItemId>) -> bool {
    match subject {
        HookSubject::Session => item.is_none(),
        HookSubject::Turn { turn: subject_turn } => subject_turn == turn && item.is_none(),
        HookSubject::Item { item: subject_item } => item == Some(subject_item),
    }
}

fn hook_owner_matches_envelope(
    state: &SessionState,
    key: &HookOccurrenceKey,
    subject: HookSubject,
    ev: &DomainEvent,
) -> Result<(), RefusalReason> {
    match key.owner() {
        HookOccurrenceOwner::Event { event } => (ev.env.parent == Some(*event))
            .then_some(())
            .ok_or(RefusalReason::HookSubjectMismatch),
        HookOccurrenceOwner::Item { item } => (ev.env.item == Some(*item)
            && subject == HookSubject::Item { item: *item })
        .then_some(())
        .ok_or(RefusalReason::HookSubjectMismatch),
        HookOccurrenceOwner::Session { .. } | HookOccurrenceOwner::Request { .. } => ev
            .env
            .item
            .is_none()
            .then_some(())
            .ok_or(RefusalReason::HookSubjectMismatch),
        HookOccurrenceOwner::Continuation { id } => {
            let HookSubject::Item { item } = subject else {
                return Err(RefusalReason::HookSubjectMismatch);
            };
            match key.lifecycle() {
                HookLifecycle::Elicitation => {
                    let Some(tool) = state.items.get(&item) else {
                        return Err(RefusalReason::HookSubjectMismatch);
                    };
                    if tool.kind != ItemKind::ToolCall {
                        return Err(RefusalReason::HookSubjectMismatch);
                    }
                    match tool.tool_call_started {
                        Some(started) if ev.env.parent == Some(started) => Ok(()),
                        Some(_) | None => Err(RefusalReason::HookOccurrenceParentMismatch),
                    }
                }
                HookLifecycle::ElicitationResult => ev
                    .env
                    .parent
                    .filter(|settlement| {
                        state.owns_foreign_continuation_settlement(
                            id,
                            ev.env.turn,
                            item,
                            *settlement,
                        )
                    })
                    .map(|_| ())
                    .ok_or(RefusalReason::HookOccurrenceParentMismatch),
                _ => Err(RefusalReason::HookSubjectMismatch),
            }
        }
        HookOccurrenceOwner::Generation { .. } | HookOccurrenceOwner::Subagent { .. } => Ok(()),
    }
}
