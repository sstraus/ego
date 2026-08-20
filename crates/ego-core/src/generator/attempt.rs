//! Generation attempts: what a retry keeps, and whether it may retry at all.
//!
//! See `docs/decisions/2026-08-20-generation-attempt-contract.md`.
//!
//! A *generation* is one intent — this prompt, this model, this quote — and
//! the budget settles once for it. An *attempt* is one journey to the wire.
//! They are separate identities because a retry is the same generation
//! reaching the provider a second time, while the provider's usage figures
//! are reported per attempt: summing two attempts of one generation would
//! bill the caller twice for work they asked for once.
//!
//! `DeliveryCertainty` lives here rather than in `ego-ai`, even though
//! `ego-ai` is what observes it (`ARCHITECTURE.md` §3.1). It has to: it is
//! carried by a `DomainEvent`, and `ego-core` cannot depend on `ego-ai`
//! without inverting the crate DAG. The same split already applies to
//! `Usage` — core owns the type, the provider layer produces the values.

use serde::{Deserialize, Serialize};

use crate::event::id::{AttemptId, GenerationId};

/// Which generation an attempt belongs to, and which attempt it is.
///
/// Deliberately a pair rather than two loose parameters: every call that
/// takes both would otherwise be one argument-swap away from attributing a
/// usage figure to the wrong generation, and both are UUIDs, so the compiler
/// would not notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttemptRef {
    generation: GenerationId,
    attempt: AttemptId,
}

impl AttemptRef {
    /// The first attempt at `generation`.
    pub fn first(generation: GenerationId) -> Self {
        Self {
            generation,
            attempt: AttemptId::new(),
        }
    }

    /// The next attempt at the *same* generation.
    ///
    /// Whether retrying is safe is a separate question, answered by
    /// [`DeliveryCertainty::retry_is_safe`] — this only says what the retry
    /// is called. Keeping the two apart means a caller cannot obtain a retry
    /// identity as a side effect of deciding to retry.
    pub fn retry(&self) -> Self {
        Self {
            generation: self.generation,
            attempt: AttemptId::new(),
        }
    }

    /// The generation this attempt belongs to. The budget settles on this.
    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    /// This attempt. Usage observations and corrections key to this.
    pub fn attempt(&self) -> AttemptId {
        self.attempt
    }
}

/// Whether a request may have reached the provider (`docs/01-fx.md` §2.4,
/// `ARCHITECTURE.md` §3.1).
///
/// Ordered deliberately: `DefinitelyUnsent` is the *more* certain state, and
/// certainty is only ever lost. The derived `Ord` puts `DefinitelyUnsent`
/// first, so [`observe`](Self::observe) is a `max` — see its doc comment for
/// why that direction is the safe one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryCertainty {
    /// The bytes did not leave. A retry cannot double-bill.
    #[default]
    DefinitelyUnsent,
    /// The bytes may have arrived — a timeout, a reset mid-write, a stream
    /// cut before its terminal frame. A retry may be billed twice.
    PossiblySent,
}

impl DeliveryCertainty {
    /// Fold in a new observation. **Monotonic**: certainty is only ever lost,
    /// never regained.
    ///
    /// This is the whole safety property. A connection reset *after* the
    /// request bytes went out looks locally identical to one that happened
    /// before them — the local side genuinely cannot tell — so an
    /// implementation that reported the optimistic reading would licence a
    /// retry for a request the provider has already charged for. Once
    /// `PossiblySent`, an attempt stays `PossiblySent` for good.
    pub fn observe(&mut self, next: DeliveryCertainty) {
        *self = (*self).max(next);
    }

    /// Whether the request may be sent again without risking a double bill.
    ///
    /// True only for [`DeliveryCertainty::DefinitelyUnsent`]. Retrying a
    /// `PossiblySent` attempt is not forbidden by this type — sometimes it is
    /// the right call, and unattended scripting mode has to make it — but it
    /// is a decision the caller has to take knowingly rather than one the
    /// transport can take on their behalf.
    pub fn retry_is_safe(&self) -> bool {
        matches!(self, DeliveryCertainty::DefinitelyUnsent)
    }
}
