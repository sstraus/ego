//! The budget gate: an unforgeable [`Reservation`] that [`Generator`] demands
//! before it will generate anything.
//!
//! See `ARCHITECTURE.md` §3.1 and the mdkb decision `ego-acp-usage-verdict`.
//!
//! ## Why the gate is in the signature, not at a call site
//!
//! `ARCHITECTURE.md` §3.1 requires that an unknown cost fail the budget
//! *closed*. A check performed on the response cannot do that: by the time a
//! provider reports what a generation cost, the money is spent and the only
//! remaining option is to report the overrun. So the check has to happen
//! before the request — and "before" has to be enforced by something
//! stronger than remembering to call a function.
//!
//! [`Generator::generate`] therefore takes a `&Reservation`, whose
//! constructor is sealed inside this module. No caller anywhere in the
//! program can build one; the only source is [`BudgetGate::reserve`], which
//! either returns a reservation or refuses. Skipping the gate is not a
//! discipline failure, it is a compile error. This is the same construction
//! `Permit` uses for effects (`crate::permit`, ARCH §3.2 R1: "effects
//! enforced, not declared") — the budget is an effect like any other.
//!
//! ## Reserve high, reconcile to the truth
//!
//! What a generation will cost is unknowable before it runs: output length
//! is the model's choice. The gate therefore reserves a deliberately
//! pessimistic [`upper bound`](PricingQuote::upper_bound) — a full context
//! window of uncached input plus the maximum output the model may produce —
//! and holds it against the limit until the real figure arrives at
//! [`BudgetGate::reconcile`]. Reserving the expected cost instead would let
//! a run of unusually long generations walk straight through the limit.
//!
//! ## The quote is resolved before the request and never recomputed
//!
//! [`PricingQuote`] is immutable and carries its [`PricingRevision`]. The
//! same quote prices the reservation and the actual usage, so a pricing
//! table that changes mid-session cannot make a generation cost one thing
//! going in and another coming out.

use std::fmt;

use crate::cost::{
    Amount, CostDelta, CurrencyCode, Money, MoneyError, PricingRevision, TokenCategory, UnknownCost,
};
use crate::generator::capabilities::ModelCapabilities;
use crate::generator::types::GenRequest;
use crate::state::Usage;

/// A capability token whose constructor only this module can reach, making
/// [`Reservation`]'s constructor equally unreachable from anywhere else.
#[derive(Debug)]
struct Sealed(());

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
    /// Which pricing table this came from, recorded on every cost it
    /// produces so a past figure stays attributable.
    pub revision: PricingRevision,
    /// The currency every price here is expressed in.
    pub currency: CurrencyCode,
    pub uncached_input: Option<Amount>,
    pub cache_read: Option<Amount>,
    pub cache_write: Option<Amount>,
    pub output: Option<Amount>,
}

impl PricingQuote {
    /// The price of one token in `category`, or the reason there is none.
    fn rate(&self, category: TokenCategory) -> Result<Amount, UnknownCost> {
        let rate = match category {
            TokenCategory::UncachedInput => self.uncached_input,
            TokenCategory::CacheRead => self.cache_read,
            TokenCategory::CacheWrite => self.cache_write,
            TokenCategory::Output => self.output,
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
            (TokenCategory::UncachedInput, usage.uncached_input_tokens),
            (TokenCategory::CacheRead, usage.cache_read_tokens),
            (TokenCategory::CacheWrite, usage.cache_write_tokens),
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
            pricing_revision: self.revision.clone(),
            fx: None,
        }
    }

    /// The dearest an input token can be under this quote.
    ///
    /// A prompt is some unknown mix of uncached, cache-read and cache-write
    /// tokens, and *which* mix is not known before the request. Pricing the
    /// whole window at the highest of the three covers every mix.
    ///
    /// It is tempting to assume uncached input is the dearest — it is the
    /// one you pay "full price" for — but that is false on real tables:
    /// Anthropic charges 1.25x the base rate to *write* the cache. An
    /// unpriced input category makes the bound unknown rather than skipped:
    /// a category this quote cannot price is one whose cost we cannot
    /// bound, and `None` means unpriced, never free (see the type's doc).
    fn dearest_input_rate(&self) -> Result<Amount, UnknownCost> {
        [
            TokenCategory::UncachedInput,
            TokenCategory::CacheRead,
            TokenCategory::CacheWrite,
        ]
        .into_iter()
        .try_fold(Amount::ZERO, |dearest, category| {
            Ok(dearest.max(self.rate(category)?))
        })
    }

    /// The most `req` could possibly cost against a model with `caps`.
    ///
    /// Deliberately crude, and crude in the safe direction: it assumes the
    /// request fills the entire context window with the dearest input the
    /// quote prices, and that the model emits its maximum permitted output.
    /// Both are true upper bounds for any request that the model will accept
    /// at all, and neither needs a tokenizer, which `ego-core` does not have
    /// and must not grow (it would have to match the provider's tokenizer
    /// exactly to be worth anything, and a bound that is *nearly* right is
    /// not a bound).
    ///
    /// A model whose context or output ceiling is unknown has no upper
    /// bound, so the gate fails closed. That is the whole point: an
    /// unbounded spend is exactly the case a budget exists to prevent.
    pub fn upper_bound(
        &self,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<Money, UnknownCost> {
        let context = caps.max_context_tokens.ok_or(UnknownCost::NoPricingQuote)?;
        // The request may cap output below the model's ceiling; the bound is
        // whichever is lower, and unknown if neither is stated.
        let output = match (req.max_output_tokens, caps.max_output_tokens) {
            (Some(requested), Some(max)) => requested.min(max),
            (Some(requested), None) => requested,
            (None, Some(max)) => max,
            (None, None) => return Err(UnknownCost::NoPricingQuote),
        };
        let context = Amount::new(u128::from(context), 0).map_err(|_| UnknownCost::Overflow)?;
        let input = self
            .dearest_input_rate()?
            .checked_mul(&context)
            .ok_or(UnknownCost::Overflow)?;
        let output = self.line(TokenCategory::Output, output)?;
        let total = input.checked_add(&output).ok_or(UnknownCost::Overflow)?;
        Ok(Money::new(total, self.currency))
    }
}

/// Every quantity that went into a "this does not fit" refusal.
///
/// A named struct rather than four fields inlined into
/// [`BudgetDenied::WouldExceed`]: four `Money` values are 192 bytes, which
/// would make every `Result<Reservation, BudgetDenied>` in the program pay
/// for the rare error path. Boxed at the variant for the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overrun {
    /// Already spent and reconciled.
    pub committed: Money,
    /// Held by reservations still in flight.
    pub outstanding: Money,
    /// This request's upper bound.
    pub requested: Money,
    pub limit: Money,
}

/// Why the gate refused to authorise a generation.
///
/// Every variant names what was refused and why, per `ARCHITECTURE.md` §3.2's
/// rule that a denial must name what was denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetDenied {
    /// The cost of the request could not be bounded, so it fails closed.
    CostUnknown(UnknownCost),
    /// The bound fits in the representation but not in the budget.
    WouldExceed(Box<Overrun>),
    /// The quote prices in a currency the budget is not denominated in.
    /// Refused rather than converted: a gate is the wrong place to invent an
    /// exchange rate.
    CurrencyMismatch {
        limit: CurrencyCode,
        quote: CurrencyCode,
    },
}

impl fmt::Display for BudgetDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BudgetDenied::CostUnknown(reason) => {
                write!(f, "budget denied: cost cannot be bounded ({reason:?})")
            }
            BudgetDenied::WouldExceed(overrun) => write!(
                f,
                "budget denied: {} committed + {} outstanding + {} requested exceeds the {} {} limit",
                overrun.committed.amount,
                overrun.outstanding.amount,
                overrun.requested.amount,
                overrun.limit.amount,
                overrun.limit.currency
            ),
            BudgetDenied::CurrencyMismatch { limit, quote } => write!(
                f,
                "budget denied: the budget is in {limit} but the quote prices in {quote}"
            ),
        }
    }
}

impl std::error::Error for BudgetDenied {}

/// Proof that a generation was authorised against a budget before it began.
///
/// Unforgeable: both fields are private and the only public constructor is
/// behind [`Sealed`], which nothing outside this module can name. Not
/// `Clone`, for the same reason [`crate::permit::Permit`] is not — one
/// authorisation must not become two, or a single approved generation could
/// be run repeatedly on one reservation.
#[derive(Debug)]
pub struct Reservation {
    upper_bound: Money,
    revision: PricingRevision,
    _sealed: Sealed,
}

impl Reservation {
    /// The bound held against the budget while this generation runs.
    pub fn upper_bound(&self) -> &Money {
        &self.upper_bound
    }

    /// The pricing revision this was authorised under. The same revision
    /// must price the actual usage, so a mid-session pricing change cannot
    /// make one generation cost two different things.
    pub fn revision(&self) -> &PricingRevision {
        &self.revision
    }
}

/// A session's spend ceiling, and the authority that admits generations
/// against it.
///
/// Holds three quantities that must not be confused: what has been spent and
/// reconciled (`committed`), what is bound up in generations still running
/// (`outstanding`), and the ceiling (`limit`). A gate that tracked only
/// `committed` would happily authorise a hundred concurrent generations that
/// each fit the remaining budget alone.
#[derive(Debug)]
pub struct BudgetGate {
    limit: Money,
    committed: Money,
    outstanding: Money,
}

impl BudgetGate {
    /// A gate that admits generations until `limit` is reached.
    pub fn new(limit: Money) -> Self {
        let currency = limit.currency;
        Self {
            limit,
            committed: Money::zero(currency),
            outstanding: Money::zero(currency),
        }
    }

    /// Spent and reconciled.
    pub fn committed(&self) -> &Money {
        &self.committed
    }

    /// Held by reservations still in flight.
    pub fn outstanding(&self) -> &Money {
        &self.outstanding
    }

    pub fn limit(&self) -> &Money {
        &self.limit
    }

    /// Authorise one generation, or refuse.
    ///
    /// Fails closed on anything it cannot bound: an unpriced token category,
    /// a model with no stated context or output ceiling, an arithmetic
    /// overflow, a currency the budget is not in. In every one of those
    /// cases the honest answer is "we do not know what this will cost", and
    /// `ARCHITECTURE.md` §3.1 says an unknown cost does not proceed.
    pub fn reserve(
        &mut self,
        quote: &PricingQuote,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<Reservation, BudgetDenied> {
        if quote.currency != self.limit.currency {
            return Err(BudgetDenied::CurrencyMismatch {
                limit: self.limit.currency,
                quote: quote.currency,
            });
        }
        let upper_bound = quote
            .upper_bound(req, caps)
            .map_err(BudgetDenied::CostUnknown)?;

        let held = self
            .committed
            .checked_add(&self.outstanding)
            .and_then(|held| held.checked_add(&upper_bound))
            .map_err(|_: MoneyError| BudgetDenied::CostUnknown(UnknownCost::Overflow))?;
        if held.amount > self.limit.amount {
            return Err(BudgetDenied::WouldExceed(Box::new(Overrun {
                committed: self.committed,
                outstanding: self.outstanding,
                requested: upper_bound,
                limit: self.limit,
            })));
        }

        self.outstanding = self
            .outstanding
            .checked_add(&upper_bound)
            .map_err(|_: MoneyError| BudgetDenied::CostUnknown(UnknownCost::Overflow))?;
        Ok(Reservation {
            upper_bound,
            revision: quote.revision.clone(),
            _sealed: Sealed(()),
        })
    }

    /// Release a reservation and commit what the generation actually cost.
    ///
    /// Consumes the `Reservation` by value: an authorisation is spent when
    /// it is reconciled, and taking it by reference would let one
    /// reservation be settled twice, double-counting the spend and releasing
    /// twice as much outstanding as was ever held.
    ///
    /// An `actual` the gate cannot apply — unknown, in the wrong currency,
    /// or priced under a different revision than the one authorised — still
    /// releases the reservation (the generation *is* over; leaving the hold
    /// in place would leak budget until the session ended) but marks the
    /// committed total as no longer trustworthy by charging the full
    /// reserved bound. Charging the pessimistic bound is the conservative
    /// reading: it can only make the gate stricter, never laxer.
    pub fn reconcile(&mut self, reservation: Reservation, actual: &CostDelta) -> Reconciled {
        let released = reservation.upper_bound;
        self.outstanding = subtract_or_zero(&self.outstanding, &released);

        let (charged, exact) = match actual {
            CostDelta::Known {
                amount,
                pricing_revision,
                ..
            } if *pricing_revision == reservation.revision
                && amount.currency == self.limit.currency =>
            {
                (*amount, true)
            }
            _ => (released, false),
        };

        // On overflow, pin the committed total at the limit. Every later
        // `reserve` then fails, which is the only safe reading of "we can no
        // longer represent what this session has spent".
        self.committed = self.committed.checked_add(&charged).unwrap_or(self.limit);
        Reconciled { charged, exact }
    }
}

/// What a [`BudgetGate::reconcile`] actually charged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    /// The amount added to the committed total.
    pub charged: Money,
    /// Whether `charged` is the generation's real cost. `false` means the
    /// gate fell back to the reserved upper bound because the actual figure
    /// was unknown, in the wrong currency, or priced under a different
    /// revision — the committed total is then an overestimate, which is the
    /// safe direction but is no longer the truth.
    pub exact: bool,
}

/// `a - b`, floored at zero.
///
/// Saturating rather than erroring because the only caller is releasing a
/// hold it previously added, so within one gate an underflow is unreachable:
/// `reserve` adds exactly the bound `reconcile` later releases, and it
/// refuses any quote whose currency differs from the limit's.
///
/// Be clear about what the floor does when it *is* reached, because the
/// obvious reading is backwards: zeroing `outstanding` frees headroom, so it
/// admits **more**, not less. It fails open. The wrapped alternative would
/// deny everything forever, which is at least loud — the floor is chosen
/// anyway because the divergence it papers over should not be representable
/// in the first place.
///
/// It currently is: a `Reservation` carries no gate identity, so one minted
/// by gate A can be reconciled against gate B, which underflows B and
/// silently widens its budget. That is a hole in the reservation contract,
/// not in this arithmetic, and closing it belongs with the generation-attempt
/// lifecycle — see `POTENTIALS.md` P9.
fn subtract_or_zero(a: &Money, b: &Money) -> Money {
    if a.currency != b.currency || a.amount < b.amount {
        return Money::zero(a.currency);
    }
    // Restate both at the wider scale, subtract exactly, and rebuild.
    let scale = a.amount.scale().max(b.amount.scale());
    let factor = |m: &Money| {
        10u128
            .checked_pow(scale - m.amount.scale())
            .and_then(|f| m.amount.coefficient().checked_mul(f))
    };
    match (factor(a), factor(b)) {
        (Some(left), Some(right)) => Amount::new(left - right, scale)
            .map(|amount| Money::new(amount, a.currency))
            .unwrap_or(Money::zero(a.currency)),
        _ => Money::zero(a.currency),
    }
}
