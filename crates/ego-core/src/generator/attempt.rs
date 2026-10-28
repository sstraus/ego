//! Generation attempts: what a retry keeps, and whether it may retry at all.
//!
//! See `ARCHITECTURE.md`.
//!
//! A *generation* is one logical request plan. An *attempt* is one journey to
//! the wire, with its own admission and settlement.
//! They are separate identities because a retry is the same logical plan
//! reaching the provider again, while usage, cost, evidence and settlement
//! remain attributable to each distinct wire journey.
//!
//! `DeliveryCertainty` lives here rather than in `ego-ai`, even though
//! `ego-ai` is what observes it (`ARCHITECTURE.md` §3.1). It has to: it is
//! carried by a `DomainEvent`, and `ego-core` cannot depend on `ego-ai`
//! without inverting the crate DAG. The same split already applies to
//! `Usage` — core owns the type, the provider layer produces the values.
//!
//! ## Who owns the authorisation, and why it is not the provider
//!
//! [`GenerationAttempt`] owns the [`GenerationAdmission`] from admission to
//! settlement. That is the piece `&GenerationAdmission` could not express:
//! consuming it at `generate` left nobody to reconcile, and borrowing it
//! left the token reusable for a second generation.
//!
//! `plans/m0b-vertical-slice.md` originally sketched
//! `generate(req, attempt) -> BoxStream<AiEvent>`, with the attempt moved in
//! and never returned. That does not work, and the reason is worth stating
//! so it is not re-proposed: [`BudgetGate::reconcile`] needs
//! `&mut BudgetGate`, a `Generator` has none and cannot obtain one, and
//! `Drop` cannot acquire one either. An attempt that goes in and never comes
//! out can never be settled by anybody — the same ownership hole, relocated.
//!
//! So the attempt rides *inside* the stream. [`AttemptStream`] owns both the
//! attempt and the provider's stream and is itself a `Stream<Item =
//! AiEvent>`, so "the admission survives until settlement" is structural
//! rather than caller discipline: the attempt cannot be dropped without
//! dropping the stream, and [`AttemptStream::settle`] is the only exit.
//!
//! For the same reason there is no `AiEvent::Settled`. A provider cannot
//! settle a gate, so an event named for it would be a claim the type system
//! cannot back. The provider reports that it *finished*
//! ([`AiEvent::Stop`](crate::generator::port::AiEvent::Stop)); only a
//! successful `reconcile` settles.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use super::capabilities::ModelCapabilities;
use super::port::AiEvent;
use super::types::{GenRequest, StopReason};
use crate::budget::{
    BudgetDenied, BudgetGate, GenerationAdmission, Metering, PricingQuote, ReconcileDenied,
    Reconciled, SettlementSource,
};
use crate::cost::{CostAttribution, CostDelta, CurrencyCode, Money, UnknownCost};
use crate::event::id::{AttemptId, GenerationId};
use crate::state::Usage;

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

    /// The generation plan this attempt belongs to.
    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    /// This attempt. Usage observations and corrections key to this.
    pub fn attempt(&self) -> AttemptId {
        self.attempt
    }
}

/// Whether a request may have reached the provider (`ARCHITECTURE.md` §2.4,
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
    /// The operation completed and therefore reached the remote peer.
    DefinitelySent,
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

/// What the provider said about token usage for one attempt.
///
/// `Missing` rather than `Option<Usage>` so that "the provider reported
/// zero" and "the provider reported nothing" are distinguishable in the code
/// and in any durable form, without a convention that a reader has to know.
/// The two settle differently: a reported zero is an exact cost, while a
/// missing figure is [`UnknownCost::ProviderReportedNoUsage`] and charges the
/// pessimistic bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageReport {
    Reported(Usage),
    Missing,
}

/// How one attempt ended: why it stopped, whether the request may have
/// reached the provider, and what it reported spending.
///
/// A struct wrapping [`StopReason`] rather than an enum beside it. The three
/// facts are orthogonal — an aborted attempt still has a delivery certainty
/// and may still have reported usage — and a parallel enum would duplicate
/// `StopReason`'s variants only to answer a different question.
///
/// Durable, and safe to decode without a guard: every [`StopReason`] is
/// terminal since `Pending` was removed
/// (`ARCHITECTURE.md`), so there is no
/// non-terminal value for a decoded outcome to hold. This type used to reject
/// one in its constructor and refuse to derive `Deserialize` because of it;
/// deleting the variant deleted both guards rather than adding a third.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    stop: StopReason,
    delivery: DeliveryCertainty,
    usage: UsageReport,
}

impl Outcome {
    /// Build a terminal outcome.
    pub fn new(stop: StopReason, delivery: DeliveryCertainty, usage: UsageReport) -> Self {
        Self {
            stop,
            delivery,
            usage,
        }
    }

    /// Why the attempt stopped.
    pub fn stop(&self) -> StopReason {
        self.stop
    }

    /// Whether the request may have reached the provider.
    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }

    /// What the provider reported spending.
    pub fn usage(&self) -> UsageReport {
        self.usage
    }

    /// Whether this attempt may be re-sent without risking a double bill.
    pub fn retry_is_safe(&self) -> bool {
        self.delivery.retry_is_safe()
    }
}

/// One generation's authorisation, owned until it settles.
///
/// Holds the [`GenerationAdmission`] and the [`Metering`] it was minted under, so the
/// same metering that authorised the generation also settles what it actually
/// cost — the invariant `crate::budget`'s module doc states, now enforced by
/// the type rather than by the caller passing the right `CostDelta`.
///
/// Not `Clone`, for the reason `GenerationAdmission` is not: one authorisation must
/// not become two.
#[must_use = "an unsettled attempt has unrecorded cost"]
pub struct GenerationAttempt {
    attempt: AttemptRef,
    metering: Metering,
    /// The rates captured for actual settlement, when any were resolved. Carried
    /// beside the metering rather than inside it for the reason
    /// `2026-08-21-a-run-with-no-ceiling.md` §3 gives: the arrangement and
    /// our knowledge of its prices are different facts.
    rates: Option<PricingQuote>,
    /// Taken by [`AttemptStream::settle`]. While it is `Some`, this attempt
    /// still needs settlement, which is exactly what [`Drop`] reports on.
    admission: Option<GenerationAdmission>,
}

impl GenerationAttempt {
    /// Take ownership of an admission for one generation, refusing it if it
    /// does not authorise this request.
    ///
    /// This is where [`GenerationAdmission::authorises`] finally has a caller, and
    /// it is the right place: `req`, `caps`, the metering and the rates are
    /// all still in hand here, and they are not once the stream is running.
    pub fn new(
        generation: GenerationId,
        admission: GenerationAdmission,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<Self, BudgetDenied> {
        Self::from_attempt(
            AttemptRef::first(generation),
            admission,
            metering,
            rates,
            req,
            caps,
        )
    }

    /// Take ownership of an admission for an already-minted attempt identity.
    pub fn from_attempt(
        attempt: AttemptRef,
        admission: GenerationAdmission,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<Self, BudgetDenied> {
        admission.authorises(metering, rates, req, caps)?;
        Ok(Self {
            attempt,
            metering: metering.clone(),
            rates: rates.cloned(),
            admission: Some(admission),
        })
    }

    /// How this generation is metered. Read by the loop to decide what a
    /// usage report is worth in money, and by [`AttemptStream::settle`].
    pub fn metering(&self) -> &Metering {
        &self.metering
    }

    /// The rates this generation was priced with, if any were.
    pub fn rates(&self) -> Option<&PricingQuote> {
        self.rates.as_ref()
    }

    /// Which generation this is, and which attempt at it.
    pub fn attempt(&self) -> AttemptRef {
        self.attempt
    }
}

impl std::fmt::Debug for GenerationAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenerationAttempt")
            .field("attempt", &self.attempt)
            .field("settled", &self.admission.is_none())
            .finish()
    }
}

impl Drop for GenerationAttempt {
    /// Report lost settlement evidence without inventing a monetary hold.
    fn drop(&mut self) {
        if let Some(admission) = &self.admission {
            tracing::error!(
                generation = ?self.attempt.generation(),
                attempt = ?self.attempt.attempt(),
                gate = ?admission.gate(),
                "generation attempt dropped without settling; actual cost is not recorded"
            );
        }
    }
}

/// What one wire attempt cost, from what the stream observed and how the row
/// is metered.
///
/// One function rather than one rule in [`AttemptStream::settle`] and another
/// in the turn loop's log: the gate's charge and the log's `cost_delta` are
/// the same figure, and two spellings of it are two places for it to
/// disagree.
///
/// The three cases, and why each is not the other
/// (`ARCHITECTURE.md` §5):
///
/// - **Quote settlement** prices the observed usage. No usage frame means no
///   quantity to price, so the cost is unknown and the gate charges its
///   pessimistic bound. **No rates** means the same thing one step earlier —
///   [`UnknownCost::NoPricingQuote`], the ordinary settlement of a run that
///   asked for no ceiling and therefore looked no prices up. Its session
///   total is `CumulativeCost::Unknown` and stays there, because the cost is
///   unknown; the alternative is a fabricated zero.
/// - **Provider-reported settlement** takes the provider's own figure and
///   never consults the usage at all. A stated total settles the *money* even
///   when the usage frame never arrived — the tokens are still missing and
///   `Usage` still records nothing, but the cost is not unknown. A provider
///   that declared it would state a cost and did not is
///   [`UnknownCost::ProviderReportedCostMissing`], named rather than
///   collapsed into a bare `exact: false`.
/// - **A row not metered in currency** costs zero, minted from its policy in
///   `settlement` — the currency the zero will be compared against, since
///   neither variant states one.
pub fn settled_cost(
    metering: &Metering,
    rates: Option<&PricingQuote>,
    usage: UsageReport,
    provider_cost: Option<Money>,
    settlement: CurrencyCode,
) -> CostDelta {
    match metering {
        Metering::Currency {
            settlement: SettlementSource::Quote,
        } => match (rates, usage) {
            (Some(quote), UsageReport::Reported(usage)) => quote.price(&usage),
            (Some(_), UsageReport::Missing) => {
                CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage)
            }
            (None, _) => CostDelta::Unknown(UnknownCost::NoPricingQuote),
        },
        Metering::Currency {
            settlement: SettlementSource::ProviderReported { revision },
        } => match provider_cost {
            Some(amount) => CostDelta::Known {
                amount,
                attribution: CostAttribution::ProviderReported(revision.clone()),
                fx: None,
            },
            None => CostDelta::Unknown(UnknownCost::ProviderReportedCostMissing),
        },
        Metering::Subscription {
            revision,
            equivalent,
        } => CostDelta::free(
            settlement,
            CostAttribution::MeteringPolicy {
                revision: revision.clone(),
                equivalent: match usage {
                    UsageReport::Reported(usage) => {
                        equivalent.as_ref().and_then(|price| price.estimate(&usage))
                    }
                    UsageReport::Missing => None,
                },
            },
        ),
        Metering::Unmetered { revision } => CostDelta::free(
            settlement,
            CostAttribution::MeteringPolicy {
                revision: revision.clone(),
                equivalent: None,
            },
        ),
    }
}

/// Settle missing accounting using the delivery evidence for this attempt.
pub fn settled_cost_for_delivery(
    metering: &Metering,
    rates: Option<&PricingQuote>,
    usage: UsageReport,
    provider_cost: Option<Money>,
    settlement: CurrencyCode,
    delivery: DeliveryCertainty,
) -> CostDelta {
    if matches!(usage, UsageReport::Missing) && delivery == DeliveryCertainty::DefinitelyUnsent {
        return CostDelta::free(settlement, CostAttribution::DefinitelyUnsent);
    }
    settled_cost(metering, rates, usage, provider_cost, settlement)
}

/// An attempt settled against its gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    pub attempt: AttemptRef,
    pub outcome: Outcome,
    pub reconciled: Reconciled,
}

/// Transient identity of the request through its last marked cache boundary.
/// Never journaled or sent to a public event stream. A provider may report a
/// hit at an earlier breakpoint; this identity does not attribute that hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePrefix {
    pub session: crate::event::id::SessionId,
    pub provider: String,
    pub model: String,
    pub digest: String,
}

/// A provider's stream with the authorisation riding inside it.
///
/// The reason for the wrapper rather than two independently owned values:
/// an attempt that can be dropped while its stream is still running makes
/// "the admission survives until settlement" a rule the caller must
/// remember. Here it is a consequence of ownership — the only way to reach
/// the attempt is [`settle`](Self::settle), which consumes both.
///
/// It also folds what the stream says as it passes: delivery certainty
/// (monotonically), incremental usage (by addition), and the first terminal
/// stop reason. That is why `settle` needs no arguments beyond the gate.
#[must_use = "dropping the stream loses actual-cost settlement"]
pub struct AttemptStream {
    attempt: GenerationAttempt,
    inner: BoxStream<'static, AiEvent>,
    cache_prefix: Option<CachePrefix>,
    delivery: DeliveryCertainty,
    usage: UsageReport,
    /// What the provider said this request will cost, when it says so at all.
    ///
    /// Replaced rather than accumulated, and that is the opposite rule to
    /// `usage` one field up. A [`Usage`] frame is an *increment*; a stated
    /// cost is a *total for the request*, so a provider that restates it is
    /// restating it and adding the two would bill the sum of the
    /// restatements. Same reasoning the wire decoder applies to its own
    /// cumulative usage chunk.
    provider_cost: Option<Money>,
    /// The *first* terminal stop reason seen. Exactly one outcome per
    /// attempt: a provider that emits two is not believed twice.
    terminal: Option<StopReason>,
}

impl AttemptStream {
    /// Wrap a provider's stream around the attempt that authorised it.
    ///
    /// The only constructor a `Generator` implementation needs, and the
    /// reason `generate` can take the attempt by value at all.
    pub fn new(attempt: GenerationAttempt, inner: BoxStream<'static, AiEvent>) -> Self {
        Self {
            attempt,
            inner,
            cache_prefix: None,
            delivery: DeliveryCertainty::default(),
            usage: UsageReport::Missing,
            provider_cost: None,
            terminal: None,
        }
    }

    pub fn with_cache_prefix(mut self, prefix: CachePrefix) -> Self {
        self.cache_prefix = Some(prefix);
        self
    }

    pub fn cache_prefix(&self) -> Option<&CachePrefix> {
        self.cache_prefix.as_ref()
    }

    /// Which generation is streaming, and which attempt at it.
    ///
    /// Stamped by `ego-core` when the attempt was built, never taken from
    /// anything the provider says: an id supplied by the provider layer
    /// would be attribution the core cannot check.
    pub fn attempt(&self) -> AttemptRef {
        self.attempt.attempt()
    }

    /// Fold one event into what this attempt is known to have done.
    fn observe(&mut self, event: &AiEvent) {
        match event {
            // The transport's own reading, folded monotonically.
            AiEvent::DeliveryObserved(certainty) => self.delivery.observe(*certainty),

            // Usage arrives as increments, so it accumulates.
            AiEvent::Usage(delta) => {
                let mut total = match self.usage {
                    UsageReport::Reported(usage) => usage,
                    UsageReport::Missing => Usage::IDENTITY,
                };
                total.add(delta);
                self.usage = UsageReport::Reported(total);
            }

            // A total, not an increment: see the field's own doc.
            AiEvent::ProviderCost { cost } => self.provider_cost = Some(*cost),

            // First terminal wins: a provider that sends two is not believed
            // twice.
            AiEvent::Stop(stop) => {
                self.terminal.get_or_insert(*stop);
            }

            AiEvent::ProviderLimits { .. }
            | AiEvent::ProviderContent { .. }
            | AiEvent::WebSearch { .. }
            | AiEvent::Citation { .. }
            | AiEvent::TextDelta { .. }
            | AiEvent::ReasoningDelta { .. }
            | AiEvent::ReasoningState(_)
            | AiEvent::ToolCall { .. } => {}

            // Deliberately not treated as proof of arrival: an `Error` can
            // be the provider answering *or* the transport failing locally,
            // and the two are opposite readings of retry safety. The
            // transport says which, with `DeliveryObserved`.
            AiEvent::Error { .. } => {}
            // Only a provider that answered emits one, so it is content below.
            AiEvent::ProviderFailure { .. } => {}
        }

        // Anything that is provider *content* proves the request arrived,
        // whatever the transport did or did not report. This is the backstop
        // that keeps retry safety correct for a provider that forgets to
        // emit `DeliveryObserved` at all.
        //
        // `Stop` is deliberately **not** in this list, and leaving it in was
        // a bug that inverted the guarantee this type exists to give. A
        // terminal frame is not provider content: a decoder synthesises
        // `Stop(StopReason::Error)` locally for a request that never left —
        // an egress refusal, a DNS refusal, a connector that never opened —
        // and every one of those arrives as
        // `DeliveryObserved(DefinitelyUnsent)`, `Error`, `Stop(Error)`. With
        // `Stop` here the backstop overwrote the transport's own reading on
        // the last of the three, so a request that provably never went out
        // settled as `PossiblySent` and `retry_is_safe` said no.
        //
        // It never helped, either: a `Stop` with no `DeliveryObserved` and no
        // content before it is a provider that sent a terminal frame and
        // nothing else, which is not a stream any wire format produces. The
        // events that do prove arrival are below, and they are the ones a
        // forgetful adapter would actually emit.
        if matches!(
            event,
            AiEvent::ProviderContent { .. }
                | AiEvent::WebSearch { .. }
                | AiEvent::Citation { .. }
                | AiEvent::TextDelta { .. }
                | AiEvent::ReasoningDelta { .. }
                | AiEvent::ReasoningState(_)
                | AiEvent::ToolCall { .. }
                | AiEvent::Usage(_)
                | AiEvent::ProviderCost { .. }
                | AiEvent::ProviderFailure { .. }
        ) {
            self.delivery.observe(DeliveryCertainty::PossiblySent);
        }
    }

    /// What this attempt is known to have done.
    fn outcome(&self) -> Outcome {
        Outcome {
            // No terminal event means nobody told us it finished — the
            // caller stopped polling, or the stream ended mid-flight. Both
            // are `Aborted` as far as the budget is concerned.
            stop: self.terminal.unwrap_or(StopReason::Aborted),
            delivery: self.delivery,
            usage: self.usage,
        }
    }

    /// Record what the attempt actually cost.
    ///
    /// One method rather than a `settle`/`cancel` pair. Settling *is* the
    /// end of the generation — it consumes the stream — so calling it before
    /// a terminal event is not an error to report, it is a cancellation, and
    /// the outcome says so with [`StopReason::Aborted`]. A second method
    /// would only add a way to get the same thing wrong.
    ///
    /// A refusal (the gate did not mint this admission) propagates with
    /// the admission inside it, so the authorisation is not lost — this
    /// attempt's `Drop` stays quiet precisely because the token now lives in
    /// the error, not because it was dropped on the floor.
    pub fn settle(mut self, gate: &mut BudgetGate) -> Result<Settlement, Box<ReconcileDenied>> {
        let outcome = self.outcome();
        let actual = settled_cost_for_delivery(
            &self.attempt.metering,
            self.attempt.rates.as_ref(),
            self.usage,
            self.provider_cost,
            gate.currency(),
            outcome.delivery(),
        );
        let admission = self
            .attempt
            .admission
            .take()
            .expect("an AttemptStream is consumed by settle, so this runs at most once");
        let reconciled = gate.reconcile(admission, &actual)?;
        Ok(Settlement {
            attempt: self.attempt.attempt(),
            outcome,
            reconciled,
        })
    }
}

impl Stream for AttemptStream {
    type Item = AiEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<AiEvent>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(event)) => {
                this.observe(&event);
                Poll::Ready(Some(event))
            }
            other => other,
        }
    }
}
