//! The `Generator` port: the one seam `ego-core` uses to talk to a model.
//!
//! See `ARCHITECTURE.md` §2 ("ports belong to the consumer") and §3.1.
//! `ego-ai` implements this trait; `ego-core` never opens a socket itself.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::attempt::{AttemptStream, DeliveryCertainty, GenerationAttempt};
use super::types::{ErrorKind, GenRequest, ReasoningState, StopReason};
use crate::cost::Money;
use crate::state::Usage;

/// One increment of a generation's output stream.
///
/// Reuses `crate::state::Usage` rather than defining a second usage type:
/// both represent the same shape (input/output token counts), and
/// `AiEvent::Usage` is exactly the incremental value that
/// `EventKind::UsageUpdated` carries and `SessionState::apply` folds into the
/// running total — there is no semantic gap to justify a distinct type, and
/// one competing definition in the same crate would be a duplication bug,
/// not a design choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AiEvent {
    /// Provider-owned block; visible blocks replace the matching assistant history item.
    ProviderContent {
        state: ReasoningState,
        visible: bool,
    },
    /// An observed provider-executed search, never a local tool request.
    WebSearch { call_id: String, query: String },
    /// A provider citation displayed with the assistant answer.
    Citation { url: String, title: String },
    /// A streamed increment of the assistant's visible text.
    TextDelta { text: String },
    /// A streamed increment of the model's reasoning/thinking.
    ///
    /// What a human reads. Not the same value as
    /// [`ReasoningState`](Self::ReasoningState) below, and the two are not
    /// interchangeable: three of the six wires publish readable reasoning and
    /// mint nothing replayable, and a block sent with `display: "omitted"` —
    /// or any `redacted_thinking` block — is the opposite, state with no
    /// readable text at all.
    ReasoningDelta { text: String },
    /// One reasoning block's opaque continuation state, complete.
    ///
    /// Emitted when the block *closes*, which is why this is not a delta:
    /// every wire that carries such state delivers it whole, at the end of
    /// the block it belongs to — Anthropic's `signature_delta` arrives
    /// immediately before `content_block_stop`, Gemini's `thought_signature`
    /// is the last delta before `step.stop`.
    ///
    /// Being the close, it is also the block *boundary*, which is the whole
    /// of ego's need for one: `turn/drain` closes the current reasoning item
    /// when this arrives and opens a new one for the next
    /// [`ReasoningDelta`](Self::ReasoningDelta). No separate boundary event
    /// and no index field — ordering is already `ItemState::seq`'s.
    ReasoningState(ReasoningState),
    /// The model decided to call a tool, with fully assembled arguments.
    /// Provider streams typically deliver a tool call's arguments as JSON
    /// fragments; assembling those fragments into one JSON string is
    /// `ego-ai`'s job (`WireFormat::parse_stream`, ARCH §3.1) — by the time
    /// an `AiEvent` reaches `ego-core`, `args` is complete.
    ToolCall {
        call_id: String,
        name: String,
        args: String,
    },
    /// Incremental usage for this generation.
    ///
    /// An **increment**, never a running total. `AttemptStream::observe` adds
    /// every one it sees into a total, so a mapping that forwarded a
    /// provider's cumulative figure on each frame would be billed the sum of
    /// its own restatements — and three of the six wires do report
    /// cumulatively. The obligation is therefore the mapping's, and it is
    /// stated here rather than in one mapping's source where only that
    /// mapping's author reads it: **buffer, and emit at a clean close**, a
    /// later report replacing the held one rather than adding to it.
    ///
    /// More than one is legal, and sometimes better. A wire that states its
    /// input tokens before generating can report them the moment they arrive,
    /// so a stream cut before any output settles against a real figure
    /// instead of `UsageReport::Missing` and its pessimistic bound. What must
    /// hold is not "exactly one" but that the events **sum to the provider's
    /// final total**.
    Usage(Usage),
    /// What the provider says it will charge for this request.
    ///
    /// A **total for the request**, not an increment — the opposite of
    /// [`Usage`](Self::Usage) one variant up, which is why it is a separate
    /// variant and not a field on it: `Usage` is summed with
    /// `saturating_add` and folded into a context-occupancy proxy, and a
    /// currency figure has no business in either.
    ///
    /// This is the one landing site for every provider that states a cost.
    /// OpenRouter's `usage.cost` and xAI's `usage.cost_in_usd_ticks` differ
    /// in the unit, the field name and the decoder that reads them; they do
    /// not differ past this port, and nothing downstream of it can tell which
    /// produced a given figure except by reading the
    /// [`ProviderCostRevision`](crate::cost::ProviderCostRevision) that the
    /// *metering* carries. Adding a third decoder must not change this file.
    /// See `ARCHITECTURE.md` §8.
    ProviderCost { cost: Money },
    /// The transport's reading of whether the request bytes reached the
    /// provider.
    ///
    /// Reported as it changes rather than only at the end, because the case
    /// that matters most is a stream dropped *after* the bytes may have left
    /// and *before* any terminal frame — precisely when a terminal-only
    /// report would never arrive and a retry would double-bill.
    /// `AttemptStream` folds these monotonically.
    DeliveryObserved(DeliveryCertainty),
    /// The generation reached a terminal state.
    Stop(StopReason),
    /// The provider reported an error mid-stream.
    ///
    /// The `kind` is what a caller can act on; the `message` is what a human
    /// reads. Both are needed and neither substitutes for the other — a
    /// message is not matchable without matching on a provider's prose, and a
    /// kind cannot say which parameter was wrong.
    Error { message: String, kind: ErrorKind },
    /// What a host may be told about the failure the provider just reported.
    ///
    /// Beside [`Error`](Self::Error) and not a field on it, because the two
    /// have different readers. `Error.message` goes to the session log and
    /// keeps whatever the provider sent, a raw non-JSON body included, so an
    /// operator can debug an outage. `cause` is built only from typed provider
    /// fields — a streamed error's `message`, an error envelope's
    /// `error.message`, an HTTP status — so it may cross to an ACP host
    /// without relaying an upstream body.
    ///
    /// Emitted only for a failure the provider itself reported: an error
    /// status, or an error event in an answered stream. A transport that lost
    /// the exchange emits none, and that absence is what lets the turn tell
    /// "the provider said it failed" from "nobody knows what the provider did
    /// with the rest" once output has already arrived.
    ProviderFailure {
        cause: String,
        /// The provider's `Retry-After`, when it stated one in seconds.
        retry_after: Option<std::time::Duration>,
    },
}

/// A permanent, local inability to encode one exact generation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestExpressibilityError(String);

impl RequestExpressibilityError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for RequestExpressibilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RequestExpressibilityError {}

/// The provider-neutral seam for generating the next assistant turn.
///
/// `ego-core` owns this trait and the types it moves (`GenRequest`,
/// `AiEvent`); `ego-ai` implements it per wire format/provider and owns
/// `Http`, credentials and the provider table. The dependency arrow is
/// `ego-ai -> ego-core`, never the reverse.
pub trait Generator: Send + Sync {
    /// Whether this resolved provider owns a native hosted search contract.
    fn supports_web_search(&self) -> bool {
        false
    }

    /// Prove locally that the adapter can encode this exact request.
    ///
    /// This performs no I/O. It is required, with no default, so a new adapter
    /// cannot silently opt out of the proof callers need before reservation.
    fn ensure_expressible(&self, req: &GenRequest) -> Result<(), RequestExpressibilityError>;

    /// Generate the next assistant turn.
    ///
    /// The [`GenerationAttempt`] is the budget gate, enforced in the
    /// signature rather than by convention: it can only be built from a
    /// [`Reservation`](crate::budget::Reservation), whose constructor is
    /// sealed inside [`crate::budget`], so the only source is
    /// `BudgetGate::reserve` — which refuses when the cost cannot be
    /// bounded. `ARCHITECTURE.md` §3.1 requires an unknown cost to fail
    /// *closed*, and a check on the response cannot do that, because the
    /// money is already spent by then. A generation that skipped the gate
    /// does not compile.
    ///
    /// Taken **by value**, and handed back inside the returned
    /// [`AttemptStream`]. Not borrowed: a borrowed token stays usable for a
    /// second generation. Not consumed and discarded either — settling needs
    /// `&mut BudgetGate`, which no implementor of this trait has, so an
    /// attempt that did not come back out could never be settled by anyone.
    /// Riding inside the stream is what makes "the hold survives until the
    /// stream ends" a fact about ownership rather than a rule to remember.
    ///
    /// Implementors build the return value with [`AttemptStream::new`]; they
    /// do not need to touch the attempt otherwise.
    /// Release connection-local continuation state before an unbounded human wait.
    ///
    /// Called only after the session's attempt has settled. Stateless generators
    /// have nothing to release; retained transports must not tie the next request
    /// to a connection that can expire while the person is answering.
    fn release_idle_connection(&self, _session: crate::event::id::SessionId) {}

    fn generate(&self, req: GenRequest, attempt: GenerationAttempt) -> AttemptStream;
}
