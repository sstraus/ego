//! The `Generator` port: the one seam `ego-core` uses to talk to a model.
//!
//! See `ARCHITECTURE.md` §2 ("ports belong to the consumer") and §3.1.
//! `ego-ai` implements this trait; `ego-core` never opens a socket itself.

use serde::{Deserialize, Serialize};

use super::attempt::{AttemptStream, DeliveryCertainty, GenerationAttempt};
use super::types::{GenRequest, StopReason};
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
    /// A streamed increment of the assistant's visible text.
    TextDelta { text: String },
    /// A streamed increment of the model's reasoning/thinking.
    ReasoningDelta { text: String },
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
    Usage(Usage),
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
    Error { message: String },
}

/// The provider-neutral seam for generating the next assistant turn.
///
/// `ego-core` owns this trait and the types it moves (`GenRequest`,
/// `AiEvent`); `ego-ai` implements it per wire format/provider and owns
/// `Http`, credentials and the provider table. The dependency arrow is
/// `ego-ai -> ego-core`, never the reverse.
pub trait Generator: Send + Sync {
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
    fn generate(&self, req: GenRequest, attempt: GenerationAttempt) -> AttemptStream;
}
