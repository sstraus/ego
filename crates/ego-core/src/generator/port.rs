//! The `Generator` port: the one seam `ego-core` uses to talk to a model.
//!
//! See `ARCHITECTURE.md` §2 ("ports belong to the consumer") and §3.1.
//! `ego-ai` implements this trait; `ego-core` never opens a socket itself.

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use super::types::{GenRequest, StopReason};
use crate::budget::Reservation;
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
    /// The `&Reservation` is the budget gate, enforced in the signature
    /// rather than by convention: its constructor is sealed inside
    /// [`crate::budget`], so the only way to obtain one is
    /// `BudgetGate::reserve`, which refuses when the cost cannot be bounded.
    /// `ARCHITECTURE.md` §3.1 requires an unknown cost to fail *closed*, and
    /// a check on the response cannot do that — the money is already spent
    /// by then. Taking the proof as an argument means a generation that
    /// skipped the gate does not compile.
    ///
    /// Borrowed rather than consumed: the reservation outlives the call and
    /// is settled afterwards by `BudgetGate::reconcile`, which needs the
    /// same pricing revision that authorised it.
    fn generate(&self, req: GenRequest, reservation: &Reservation) -> BoxStream<'static, AiEvent>;
}
