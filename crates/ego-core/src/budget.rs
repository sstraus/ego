//! Billed-spend admission for generation attempts.
//!
//! A configured cap refuses the next request only after recorded spend exceeds
//! the limit. There is no pre-request monetary hold or worst-case estimate.
//! In-flight concurrent requests may still settle after the cap is crossed;
//! a sequential stream can overshoot by one request. Unknown costs remain unknown.
//!
//! The linear admission token binds the gate, model, metering and exact quote
//! used for settlement. It holds no money and cannot be forged or cloned.

use crate::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money, PricingRevision,
    ProviderCostRevision, RateSource, TokenCategory, UnknownCost,
};
use crate::event::id::GateId;
use crate::generator::attempt::{AttemptStream, Settlement};
use crate::generator::capabilities::ModelCapabilities;
use crate::generator::types::GenRequest;
use crate::state::Usage;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::{Arc, Mutex};

/// An immutable price list, resolved before a request and persisted with the
/// cost it produces.
///
/// Prices are per *single token*, not per million: a per-million rate would
/// force a division somewhere in the pricing arithmetic, and division is
/// where exactness goes to die. Callers converting a published `$3.00 / MTok`
/// rate divide once, at the edge, into an [`Amount`] with enough scale to
/// hold the result exactly.
///
/// A category priced `None` is not free — it is *unpriced*, and pricing any
/// usage that touches it yields [`UnknownCost::UnpricedCategory`]. A provider
/// that genuinely does not charge for something reports `Some(zero)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingQuote {
    /// Exact price per provider-side web search, when quoted.
    pub web_search: Option<Amount>,
    /// Which pricing table this came from, recorded on every cost it
    /// produces so a past figure stays attributable.
    pub revision: PricingRevision,
    /// The currency every price here is expressed in.
    pub currency: CurrencyCode,
    pub uncached_input: Option<Amount>,
    pub cache_read: Option<Amount>,
    pub cache_write: Option<Amount>,
    /// The rate for a write at an extended retention.
    ///
    /// Absent on every quote today: no provider ego speaks to publishes one,
    /// and `None` here means unpriced rather than free, exactly as it does
    /// for the categories above. A generation that wrote nothing at extended
    /// retention has a zero count and never consults this, so an absent rate
    /// costs nothing until a request actually asks for one.
    pub cache_write_extended: Option<Amount>,
    pub output: Option<Amount>,
}

impl PricingQuote {
    /// The price of one token in `category`, or the reason there is none.
    fn rate(&self, category: TokenCategory) -> Result<Amount, UnknownCost> {
        let rate = match category {
            TokenCategory::UncachedInput => self.uncached_input,
            TokenCategory::CacheRead => self.cache_read,
            TokenCategory::CacheWrite => self.cache_write,
            TokenCategory::CacheWriteExtended => self.cache_write_extended,
            TokenCategory::Output => self.output,
            TokenCategory::WebSearch => self.web_search,
        };
        rate.ok_or(UnknownCost::UnpricedCategory { category })
    }

    /// `count` tokens of `category`, priced exactly.
    fn line(&self, category: TokenCategory, count: u64) -> Result<Amount, UnknownCost> {
        let rate = self.rate(category)?;
        let count = Amount::new(u128::from(count), 0).map_err(|_| UnknownCost::Overflow)?;
        rate.checked_mul(&count).ok_or(UnknownCost::Overflow)
    }

    /// Price one generation's observed usage.
    ///
    /// Every category with a non-zero count must be priced; a category the
    /// quote does not cover makes the whole figure unknown rather than an
    /// undercount presented as a fact. A category with a *zero* count is not
    /// consulted, so a quote that omits `cache_write` still prices an
    /// uncached generation exactly.
    pub fn price(&self, usage: &Usage) -> CostDelta {
        let lines = [
            (TokenCategory::WebSearch, usage.web_search_requests),
            (TokenCategory::UncachedInput, usage.uncached_input_tokens),
            (TokenCategory::CacheRead, usage.cache_read_tokens),
            (TokenCategory::CacheWrite, usage.cache_write_tokens),
            (
                TokenCategory::CacheWriteExtended,
                usage.cache_write_extended_tokens,
            ),
            (TokenCategory::Output, usage.output_tokens),
        ];
        let mut total = Amount::ZERO;
        for (category, count) in lines {
            if count == 0 {
                continue;
            }
            match self.line(category, count) {
                Ok(line) => match total.checked_add(&line) {
                    Some(sum) => total = sum,
                    None => return CostDelta::Unknown(UnknownCost::Overflow),
                },
                Err(reason) => return CostDelta::Unknown(reason),
            }
        }
        CostDelta::Known {
            amount: Money::new(total, self.currency),
            attribution: CostAttribution::Table {
                revision: self.revision.clone(),
            },
            fx: None,
        }
    }
}

/// A settlement quote and its rate-source provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRates {
    pub quote: PricingQuote,
    pub source: RateSource,
}

/// Where a response's actual charge comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettlementSource {
    /// The quote captured at admission prices the observed usage.
    Quote,
    /// The provider states the charge for the request outright, and that
    /// figure settles it — strictly better than any table, because it is
    /// what will appear on the invoice.
    ProviderReported { revision: ProviderCostRevision },
}

impl fmt::Display for SettlementSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettlementSource::Quote => f.write_str("the quote"),
            SettlementSource::ProviderReported { revision } => {
                write!(f, "{}'s {}", revision.provider, revision.field)
            }
        }
    }
}

/// The billing arrangement, independent of whether settlement rates are known.
/// Currency responses use a captured quote or a provider-stated charge;
/// subscription and unmetered policies settle at attributable zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Metering {
    /// Metered in money, per token. What that money *is* comes from the
    /// rates, which may be absent — see the type's own doc.
    Currency { settlement: SettlementSource },
    /// Access is paid for by a flat fee, so no per-token currency charge
    /// exists to quote. **Not** an all-zero [`PricingQuote`]: a price list
    /// saying "zero per token" and a row that has no per-token price at all
    /// are different facts, and once they share a spelling nothing downstream
    /// can tell them apart.
    ///
    /// Also **not** the answer to "we do not know this row's rates". That is
    /// `Currency` with no rates: this variant asserts *the provider charges
    /// nothing per token*, a claim about the world that has to be earned.
    Subscription {
        revision: MeteringRevision,
        equivalent: Option<Box<crate::cost::EquivalentPricing>>,
    },
    /// Nobody is charging: a daemon serving its own weights. Same reasoning
    /// as [`Subscription`](Self::Subscription), one row over.
    Unmetered { revision: MeteringRevision },
}

impl Metering {
    /// The labels this metering is authorised under, without its figures.
    pub fn identity(&self) -> MeteringIdentity {
        match self {
            Metering::Currency { settlement } => MeteringIdentity::Currency {
                settlement: settlement.clone(),
            },
            Metering::Subscription { revision, .. } => MeteringIdentity::Subscription {
                revision: revision.clone(),
            },
            Metering::Unmetered { revision } => MeteringIdentity::Unmetered {
                revision: revision.clone(),
            },
        }
    }

    /// Whether currency settlement needs rates when a cap is configured.
    pub fn ceiling_needs_rates(&self, ledger: &Ledger) -> bool {
        matches!(self, Metering::Currency { .. }) && matches!(ledger, Ledger::Limited { .. })
    }

    /// What a settled figure under this metering is attributable to, when it
    /// is the metering itself that produced it.
    ///
    /// `None` for a currency row: there the figure comes from the quote or
    /// from the provider, and both mint their own attribution.
    pub fn policy_attribution(&self) -> Option<CostAttribution> {
        match self {
            Metering::Currency { .. } => None,
            Metering::Subscription { revision, .. } | Metering::Unmetered { revision } => {
                Some(CostAttribution::MeteringPolicy {
                    revision: revision.clone(),
                    equivalent: None,
                })
            }
        }
    }
}

/// The billing identity bound to one generation admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MeteringIdentity {
    Currency { settlement: SettlementSource },
    Subscription { revision: MeteringRevision },
    Unmetered { revision: MeteringRevision },
}

impl fmt::Display for MeteringIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeteringIdentity::Currency { settlement } => {
                write!(f, "currency settled from {settlement}")
            }
            MeteringIdentity::Subscription { revision } => {
                write!(f, "a subscription under {}", revision.0)
            }
            MeteringIdentity::Unmetered { revision } => write!(f, "unmetered under {}", revision.0),
        }
    }
}

/// The settlement facts captured at admission. No token ceilings or estimated cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetBinding {
    model: String,
    metering: MeteringIdentity,
    rates: Option<PricingQuote>,
}

impl BudgetBinding {
    fn of(req: &GenRequest, metering: &Metering, rates: Option<&PricingQuote>) -> Self {
        Self {
            model: req.model.clone(),
            metering: metering.identity(),
            rates: if matches!(metering, Metering::Currency { .. }) {
                rates.cloned()
            } else {
                None
            },
        }
    }

    fn settles(&self, attribution: &CostAttribution, amount: &Money) -> bool {
        match (&self.metering, attribution) {
            (_, CostAttribution::DefinitelyUnsent) => amount.amount.is_zero(),
            (_, CostAttribution::ProviderReported(_)) => true,
            (MeteringIdentity::Currency { .. }, CostAttribution::Table { revision }) => self
                .rates
                .as_ref()
                .is_some_and(|quote| &quote.revision == revision),
            (
                MeteringIdentity::Subscription { revision }
                | MeteringIdentity::Unmetered { revision },
                CostAttribution::MeteringPolicy {
                    revision: settled, ..
                },
            ) => revision == settled,
            _ => false,
        }
    }
}

/// Why admission or settlement was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetDenied {
    CostUnknown(UnknownCost),
    SpendExceeded {
        spent: Money,
        limit: Money,
    },
    WrongGate {
        minted_by: GateId,
        presented_to: GateId,
    },
    WrongModel {
        authorised: String,
        presented: String,
    },
    WrongMetering {
        authorised: Box<MeteringIdentity>,
        presented: Box<MeteringIdentity>,
    },
    QuoteChanged,
}

impl fmt::Display for BudgetDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CostUnknown(reason) => {
                write!(f, "budget denied: spend cannot be represented ({reason:?})")
            }
            Self::SpendExceeded { spent, limit } => write!(
                f,
                "budget denied: {} {} spent exceeds the {} {} limit",
                spent.amount, spent.currency, limit.amount, limit.currency
            ),
            Self::WrongGate {
                minted_by,
                presented_to,
            } => write!(
                f,
                "budget denied: admission minted by gate {minted_by:?}, presented to {presented_to:?}"
            ),
            Self::WrongModel {
                authorised,
                presented,
            } => write!(
                f,
                "budget denied: admission authorised {authorised}, not {presented}"
            ),
            Self::WrongMetering {
                authorised,
                presented,
            } => write!(
                f,
                "budget denied: admission authorised {authorised}, not {presented}"
            ),
            Self::QuoteChanged => {
                f.write_str("budget denied: the settlement quote changed after admission")
            }
        }
    }
}
impl std::error::Error for BudgetDenied {}

/// Linear proof that this gate admitted one generation. Holds no money.
#[derive(Debug)]
pub struct GenerationAdmission {
    gate: GateId,
    binding: BudgetBinding,
}
impl GenerationAdmission {
    pub fn gate(&self) -> GateId {
        self.gate
    }
    pub fn model(&self) -> &str {
        &self.binding.model
    }
    pub fn authorises(
        &self,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        _caps: &ModelCapabilities,
    ) -> Result<(), BudgetDenied> {
        let presented = BudgetBinding::of(req, metering, rates);
        if presented.model != self.binding.model {
            return Err(BudgetDenied::WrongModel {
                authorised: self.binding.model.clone(),
                presented: presented.model,
            });
        }
        if presented.metering != self.binding.metering {
            return Err(BudgetDenied::WrongMetering {
                authorised: Box::new(self.binding.metering.clone()),
                presented: Box::new(presented.metering),
            });
        }
        if presented.rates != self.binding.rates {
            return Err(BudgetDenied::QuoteChanged);
        }
        Ok(())
    }
}

/// A rejected settlement returns its linear admission to the caller.
#[derive(Debug)]
pub struct ReconcileDenied {
    pub admission: GenerationAdmission,
    pub reason: BudgetDenied,
}
impl fmt::Display for ReconcileDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(f)
    }
}
impl std::error::Error for ReconcileDenied {}

/// A configured spend limit, or no limit. No money is held in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ledger {
    Unlimited,
    Limited {
        limit: Money,
        clamped_from: Option<Money>,
        committed: Money,
    },
}

#[derive(Debug)]
pub struct BudgetGate {
    id: GateId,
    currency: CurrencyCode,
    ledger: Ledger,
    // Representation overflow is a trust boundary: never turn it into headroom.
    overflowed: bool,
}
impl BudgetGate {
    pub fn new(limit: Money) -> Self {
        Self::bounded(limit, None)
    }
    pub fn clamped(limit: Money, requested: Money) -> Self {
        Self::bounded(limit, Some(requested))
    }
    fn bounded(limit: Money, clamped_from: Option<Money>) -> Self {
        Self {
            id: GateId::new(),
            currency: limit.currency,
            ledger: Ledger::Limited {
                limit,
                clamped_from,
                committed: Money::zero(limit.currency),
            },
            overflowed: false,
        }
    }
    pub fn unlimited(currency: CurrencyCode) -> Self {
        Self {
            id: GateId::new(),
            currency,
            ledger: Ledger::Unlimited,
            overflowed: false,
        }
    }
    pub fn id(&self) -> GateId {
        self.id
    }
    pub fn currency(&self) -> CurrencyCode {
        self.currency
    }
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }
    pub fn committed(&self) -> Option<&Money> {
        match &self.ledger {
            Ledger::Limited { committed, .. } => Some(committed),
            Ledger::Unlimited => None,
        }
    }
    pub fn limit(&self) -> Option<&Money> {
        match &self.ledger {
            Ledger::Limited { limit, .. } => Some(limit),
            Ledger::Unlimited => None,
        }
    }

    /// Check only recorded spend. Equality admits; the next response may cross the cap.
    pub fn admit(
        &mut self,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        _caps: &ModelCapabilities,
    ) -> Result<GenerationAdmission, BudgetDenied> {
        if self.overflowed {
            return Err(BudgetDenied::CostUnknown(UnknownCost::Overflow));
        }
        if let Ledger::Limited {
            limit, committed, ..
        } = self.ledger
            && committed.amount > limit.amount
        {
            return Err(BudgetDenied::SpendExceeded {
                spent: committed,
                limit,
            });
        }
        Ok(GenerationAdmission {
            gate: self.id,
            binding: BudgetBinding::of(req, metering, rates),
        })
    }

    /// Charge only usable actual cost; never invent a charge for missing cost.
    pub fn reconcile(
        &mut self,
        admission: GenerationAdmission,
        actual: &CostDelta,
    ) -> Result<Reconciled, Box<ReconcileDenied>> {
        if admission.gate != self.id {
            return Err(Box::new(ReconcileDenied {
                reason: BudgetDenied::WrongGate {
                    minted_by: admission.gate,
                    presented_to: self.id,
                },
                admission,
            }));
        }
        let charged = match actual {
            CostDelta::Known {
                amount,
                attribution,
                ..
            } if amount.currency == self.currency
                && admission.binding.settles(attribution, amount) =>
            {
                Some(*amount)
            }
            _ => None,
        };
        if let (Some(cost), Ledger::Limited { committed, .. }) = (charged, &mut self.ledger) {
            if let Ok(total) = committed.checked_add(&cost) {
                *committed = total;
            } else {
                self.overflowed = true;
            }
        }
        Ok(Reconciled {
            charged,
            exact: charged.is_some(),
        })
    }
}

/// An immutable shared-gate accounting view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetSnapshot {
    id: GateId,
    currency: CurrencyCode,
    ledger: Ledger,
}
impl BudgetSnapshot {
    pub fn id(&self) -> GateId {
        self.id
    }
    pub fn currency(&self) -> CurrencyCode {
        self.currency
    }
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }
}

/// A poisoned shared-budget lock. Every operation fails closed after this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSynchronizationError;

impl fmt::Display for BudgetSynchronizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the shared budget gate lock is poisoned")
    }
}

impl std::error::Error for BudgetSynchronizationError {}

/// Why a shared budget mutation could not complete.
#[derive(Debug)]
pub enum SharedBudgetError {
    Synchronization(BudgetSynchronizationError),
    Denied(BudgetDenied),
    Reconcile(Box<ReconcileDenied>),
}

impl fmt::Display for SharedBudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synchronization(error) => error.fmt(f),
            Self::Denied(error) => error.fmt(f),
            Self::Reconcile(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SharedBudgetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Synchronization(error) => Some(error),
            Self::Denied(error) => Some(error),
            Self::Reconcile(error) => Some(error.as_ref()),
        }
    }
}

struct SharedBudgetAuthority {
    id: GateId,
    gate: Mutex<BudgetGate>,
}

/// Cloneable authority over one root budget gate.
///
/// GenerationAdmissions and attempt streams remain linear values. The lock is held
/// only while admitting a request, taking a snapshot, or consuming a fully
/// polled [`AttemptStream`] into settlement.
#[derive(Clone)]
pub struct SharedBudgetGate {
    inner: Arc<SharedBudgetAuthority>,
}

impl SharedBudgetGate {
    /// Wrap one root gate in cloneable shared authority.
    pub fn new(gate: BudgetGate) -> Self {
        Self {
            inner: Arc::new(SharedBudgetAuthority {
                id: gate.id(),
                gate: Mutex::new(gate),
            }),
        }
    }

    /// The immutable identity shared by every clone.
    pub fn id(&self) -> GateId {
        self.inner.id
    }

    /// Copy the gate's immutable accounting view without exposing its lock.
    pub fn snapshot(&self) -> Result<BudgetSnapshot, BudgetSynchronizationError> {
        let gate = self
            .inner
            .gate
            .lock()
            .map_err(|_| BudgetSynchronizationError)?;
        Ok(BudgetSnapshot {
            id: gate.id,
            currency: gate.currency,
            ledger: gate.ledger.clone(),
        })
    }

    /// Admit one generation without holding the lock across provider I/O.
    pub fn admit(
        &self,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<GenerationAdmission, SharedBudgetError> {
        self.inner
            .gate
            .lock()
            .map_err(|_| SharedBudgetError::Synchronization(BudgetSynchronizationError))?
            .admit(metering, rates, req, caps)
            .map_err(SharedBudgetError::Denied)
    }

    /// Consume and settle a provider stream after polling has completed.
    pub fn settle(&self, stream: AttemptStream) -> Result<Settlement, SharedBudgetError> {
        let mut gate = self
            .inner
            .gate
            .lock()
            .map_err(|_| SharedBudgetError::Synchronization(BudgetSynchronizationError))?;
        stream
            .settle(&mut gate)
            .map_err(SharedBudgetError::Reconcile)
    }

    /// Whether two handles carry the same root budget authority.
    pub fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// The actual charge recorded for one response. Unknown is not zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub charged: Option<Money>,
    pub exact: bool,
}
