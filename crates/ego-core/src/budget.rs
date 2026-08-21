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
//!
//! ## A reservation names the gate that minted it and what it authorised
//!
//! Unforgeability is not the same as authorisation. [`BudgetGate::new`] is
//! public, so anyone can stand up a gate with a limit of their choosing and
//! obtain a perfectly genuine [`Reservation`] from it. What that reservation
//! must not be able to do is spend *someone else's* budget, and until it
//! carried a [`GateId`] it could: settling it against another gate released
//! a hold that gate never took, and [`subtract_or_zero`] floors at zero, so
//! the release did not even underflow visibly — it just handed the second
//! gate headroom out of thin air.
//!
//! It also carries a [`BudgetBinding`]: the model, the two token ceilings
//! the bound was computed from, and the pricing revision. Deliberately not
//! the prompt — see the type's own doc for why binding request bytes would
//! refuse substitutions that cannot overspend.

use std::fmt;

use crate::cost::{
    Amount, Ceiling, CostDelta, CurrencyCode, Money, MoneyError, PricingRevision, TokenCategory,
    UnknownCost,
};
use crate::event::id::GateId;
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
        let (context, output) = ceilings(req, caps)?;
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

/// The two token ceilings a bound is computed from: the model's context
/// window, and the most output this particular request may produce.
///
/// Shared by [`PricingQuote::upper_bound`] and [`BudgetBinding::of`] rather
/// than written twice. If the two ever disagreed about how a ceiling is
/// resolved, a reservation would bind a shape its own bound was not computed
/// from — which is exactly the confusion the binding exists to prevent.
///
/// The request may cap output below the model's ceiling; the bound uses
/// whichever is lower, and is unknown if neither is stated.
fn ceilings(req: &GenRequest, caps: &ModelCapabilities) -> Result<(u64, u64), UnknownCost> {
    let context = caps
        .max_context_tokens
        .ok_or(UnknownCost::UnstatedCeiling {
            ceiling: Ceiling::Context,
        })?;
    let output = match (req.max_output_tokens, caps.max_output_tokens) {
        (Some(requested), Some(max)) => requested.min(max),
        (Some(requested), None) => requested,
        (None, Some(max)) => max,
        (None, None) => {
            return Err(UnknownCost::UnstatedCeiling {
                ceiling: Ceiling::Output,
            });
        }
    };
    Ok((context, output))
}

/// What a reservation authorised, in the only terms a budget understands.
///
/// Held by value rather than digested. A digest could report only
/// "mismatch"; this names the fact that moved, which `ARCHITECTURE.md` §3.2
/// requires of a denial — and, unlike a hash, it cannot collide, so it needs
/// no canonical-encoding argument and no golden to pin one.
///
/// Deliberately **not** the request. Prompt bytes, tools and thinking level
/// are absent because [`PricingQuote::upper_bound`] already assumes a full
/// context window at the dearest input rate: no rewording of a prompt can
/// cost more than what was already authorised. Binding them would refuse
/// substitutions that cannot overspend, and a reservation is budget
/// authority, not content authority — whatever polices *what* is sent is a
/// different mechanism, and pretending this one does it would be worse than
/// not doing it at all.
///
/// `model` is bound even though the bound's arithmetic never reads it:
/// authorising a generation against one model and running another with
/// coincidentally identical ceilings is a substitution the operator would
/// call a bug, and the binding is the only place that can notice.
///
/// What this cannot prove on its own is that the quote presented later is
/// the one that priced it: [`PricingRevision`] is an opaque public string,
/// so a fabricated cheap quote can wear a genuine revision label.
/// [`Reservation::authorises`] closes that separately, by recomputing the
/// bound from the presented quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetBinding {
    model: String,
    context_ceiling: u64,
    output_ceiling: u64,
    revision: PricingRevision,
}

impl BudgetBinding {
    /// The budget-relevant shape of `req` against `caps` under `revision`,
    /// or the reason it has none — the same reason
    /// [`PricingQuote::upper_bound`] would refuse, because it is the same
    /// resolution.
    pub fn of(
        req: &GenRequest,
        caps: &ModelCapabilities,
        revision: &PricingRevision,
    ) -> Result<Self, UnknownCost> {
        let (context_ceiling, output_ceiling) = ceilings(req, caps)?;
        Ok(Self {
            model: req.model.clone(),
            context_ceiling,
            output_ceiling,
            revision: revision.clone(),
        })
    }

    /// The model this authorises a generation against.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The context window the bound assumed was filled.
    pub fn context_ceiling(&self) -> u64 {
        self.context_ceiling
    }

    /// The most output tokens the bound allowed for.
    pub fn output_ceiling(&self) -> u64 {
        self.output_ceiling
    }

    /// The pricing table this was authorised under.
    pub fn revision(&self) -> &PricingRevision {
        &self.revision
    }
}

/// A reservation presented for a generation it did not authorise, with both
/// shapes kept so the denial can name which fact moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingMismatch {
    pub authorised: BudgetBinding,
    pub presented: BudgetBinding,
}

impl BindingMismatch {
    /// Every fact that differs, as `(what, authorised, presented)`. Empty is
    /// unreachable from a real denial: the mismatch is constructed only when
    /// the two bindings compare unequal.
    fn differences(&self) -> Vec<(&'static str, String, String)> {
        let (a, p) = (&self.authorised, &self.presented);
        let mut out = Vec::new();
        if a.model != p.model {
            out.push(("model", a.model.clone(), p.model.clone()));
        }
        if a.context_ceiling != p.context_ceiling {
            out.push((
                "context ceiling",
                a.context_ceiling.to_string(),
                p.context_ceiling.to_string(),
            ));
        }
        if a.output_ceiling != p.output_ceiling {
            out.push((
                "output ceiling",
                a.output_ceiling.to_string(),
                p.output_ceiling.to_string(),
            ));
        }
        if a.revision != p.revision {
            out.push((
                "pricing revision",
                a.revision.0.clone(),
                p.revision.0.clone(),
            ));
        }
        out
    }
}

/// A settled generation that cost more than the bound which admitted it.
///
/// [`PricingQuote::upper_bound`] claims to be an *absolute* ceiling, so this
/// is not an overspend to note and move past: it is proof that the quote,
/// the capability record or the provider's reported usage is wrong, and
/// therefore that every bound this gate has computed is unreliable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundBreach {
    /// What the generation actually cost.
    pub charged: Money,
    /// The bound that was supposed to cover it.
    pub bound: Money,
}

impl BoundBreach {
    /// How far past the bound the real cost went. Derived rather than
    /// stored: a third field would be a fact that could disagree with the
    /// two it is computed from.
    pub fn excess(&self) -> Money {
        subtract_or_zero(&self.charged, &self.bound)
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
    /// The reservation was minted by a different gate. Settling it here
    /// would release a hold this gate never took, handing it headroom it
    /// never reserved.
    WrongGate {
        minted_by: GateId,
        presented_to: GateId,
    },
    /// The reservation authorised a different generation than the one
    /// presented.
    WrongBinding(Box<BindingMismatch>),
    /// The presented quote does not bound this generation at the figure that
    /// was authorised. Caught separately from the binding because
    /// [`PricingRevision`] is an opaque public string: a quote with cheaper
    /// rates can carry a genuine revision label, and only recomputing the
    /// bound notices.
    QuoteChanged { authorised: Money, presented: Money },
    /// A generation this gate settled cost more than the bound that admitted
    /// it, so the gate's bounds are not bounds. It admits nothing further.
    BoundBreached(BoundBreach),
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
            BudgetDenied::WrongGate {
                minted_by,
                presented_to,
            } => write!(
                f,
                "budget denied: this reservation was minted by gate {minted_by:?} \
                 and presented to gate {presented_to:?}"
            ),
            BudgetDenied::WrongBinding(mismatch) => {
                write!(f, "budget denied: this reservation authorised")?;
                for (what, authorised, presented) in mismatch.differences() {
                    write!(f, " {what} {authorised}, not {presented};")?;
                }
                Ok(())
            }
            BudgetDenied::QuoteChanged {
                authorised,
                presented,
            } => write!(
                f,
                "budget denied: the presented quote bounds this generation at {} {}, \
                 not the {} that was authorised",
                presented.amount, presented.currency, authorised.amount
            ),
            BudgetDenied::BoundBreached(breach) => write!(
                f,
                "budget denied: a settled generation cost {} against a {} bound \
                 (over by {}), so this gate's bounds are not bounds",
                breach.charged.amount,
                breach.bound.amount,
                breach.excess().amount
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
    gate: GateId,
    upper_bound: Money,
    binding: BudgetBinding,
    _sealed: Sealed,
}

impl Reservation {
    /// The bound held against the budget while this generation runs.
    pub fn upper_bound(&self) -> &Money {
        &self.upper_bound
    }

    /// Which gate is holding this bound, and therefore the only gate that
    /// may release it.
    pub fn gate(&self) -> GateId {
        self.gate
    }

    /// What this authorised.
    pub fn binding(&self) -> &BudgetBinding {
        &self.binding
    }

    /// The pricing revision this was authorised under. The same revision
    /// must price the actual usage, so a mid-session pricing change cannot
    /// make one generation cost two different things.
    pub fn revision(&self) -> &PricingRevision {
        self.binding.revision()
    }

    /// Refuse unless this reservation is the one that authorised exactly
    /// this generation.
    ///
    /// Two checks, because neither implies the other. The [`BudgetBinding`]
    /// catches a substituted model or moved ceilings and names which; the
    /// recomputed bound catches a substituted *quote*, which the binding
    /// cannot see because [`PricingRevision`] is an opaque public string and
    /// a fabricated cheap table can wear a genuine label.
    ///
    /// Not called by [`BudgetGate::reconcile`], which has no request to
    /// check against: this is the check at the moment of *use*, and it takes
    /// the same `(quote, req, caps)` as [`BudgetGate::reserve`] so a caller
    /// that has one has the other.
    pub fn authorises(
        &self,
        quote: &PricingQuote,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<(), BudgetDenied> {
        let presented =
            BudgetBinding::of(req, caps, &quote.revision).map_err(BudgetDenied::CostUnknown)?;
        if presented != self.binding {
            return Err(BudgetDenied::WrongBinding(Box::new(BindingMismatch {
                authorised: self.binding.clone(),
                presented,
            })));
        }
        let recomputed = quote
            .upper_bound(req, caps)
            .map_err(BudgetDenied::CostUnknown)?;
        if recomputed != self.upper_bound {
            return Err(BudgetDenied::QuoteChanged {
                authorised: self.upper_bound,
                presented: recomputed,
            });
        }
        Ok(())
    }
}

/// A reconciliation the gate refused, with the authorisation handed back.
///
/// The reservation is returned rather than dropped because the refusing gate
/// never released it: consuming it here would strand the hold on whichever
/// gate did mint it, for the rest of that gate's life, which is the same
/// budget leak the refusal exists to prevent.
///
/// Handing it back grants nothing. The caller held exactly one linear token
/// before the call and holds the same one after — [`Reservation`] is not
/// `Clone`, so no second copy can appear — and presenting it to every gate
/// in reach still only settles at the one whose [`GateId`] it names.
#[derive(Debug)]
pub struct ReconcileDenied {
    /// The unspent authorisation, returned intact.
    pub reservation: Reservation,
    pub reason: BudgetDenied,
}

impl fmt::Display for ReconcileDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(f)
    }
}

impl std::error::Error for ReconcileDenied {}

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
    id: GateId,
    limit: Money,
    committed: Money,
    outstanding: Money,
    /// Set once a settled generation has cost more than its bound. From that
    /// moment the gate admits nothing: see [`BudgetGate::reserve`].
    breached: Option<BoundBreach>,
}

impl BudgetGate {
    /// A gate that admits generations until `limit` is reached.
    pub fn new(limit: Money) -> Self {
        let currency = limit.currency;
        Self {
            id: GateId::new(),
            limit,
            committed: Money::zero(currency),
            outstanding: Money::zero(currency),
            breached: None,
        }
    }

    /// This gate's identity, stamped on every reservation it mints.
    pub fn id(&self) -> GateId {
        self.id
    }

    /// The breach that closed this gate, if one has happened.
    pub fn breached(&self) -> Option<&BoundBreach> {
        self.breached.as_ref()
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
    ///
    /// A gate that has already seen a generation cost more than its bound
    /// refuses everything, permanently. That looks severe for one mispriced
    /// generation, and it is the same rule: a breach proves the bound is not
    /// a bound, so every admission after it would be computed from a premise
    /// this gate has watched fail. Continuing to admit would be exactly the
    /// "we do not know what this will cost, but proceed anyway" the gate
    /// exists to refuse — only worse, because the evidence is in hand.
    pub fn reserve(
        &mut self,
        quote: &PricingQuote,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<Reservation, BudgetDenied> {
        if let Some(breach) = self.breached {
            return Err(BudgetDenied::BoundBreached(breach));
        }
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

        let binding =
            BudgetBinding::of(req, caps, &quote.revision).map_err(BudgetDenied::CostUnknown)?;
        self.outstanding = self
            .outstanding
            .checked_add(&upper_bound)
            .map_err(|_: MoneyError| BudgetDenied::CostUnknown(UnknownCost::Overflow))?;
        Ok(Reservation {
            gate: self.id,
            upper_bound,
            binding,
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
    ///
    /// Refuses, without touching a single figure, a reservation another gate
    /// minted — and hands it back, because this gate is not the one holding
    /// it (see [`ReconcileDenied`]). The identity check comes first for that
    /// reason: releasing and then discovering the mismatch would already
    /// have widened this gate's budget.
    ///
    /// An actual *above* the reserved bound is charged in full and reported
    /// in [`Reconciled::over_bound`] — the money is spent, and the bound was
    /// the larger of the two only in the cases where it is charged instead.
    /// It also closes the gate permanently; see [`BudgetGate::reserve`].
    pub fn reconcile(
        &mut self,
        reservation: Reservation,
        actual: &CostDelta,
    ) -> Result<Reconciled, Box<ReconcileDenied>> {
        if reservation.gate != self.id {
            return Err(Box::new(ReconcileDenied {
                reason: BudgetDenied::WrongGate {
                    minted_by: reservation.gate,
                    presented_to: self.id,
                },
                reservation,
            }));
        }

        let released = reservation.upper_bound;
        self.outstanding = subtract_or_zero(&self.outstanding, &released);

        let (charged, exact) = match actual {
            CostDelta::Known {
                amount,
                pricing_revision,
                ..
            } if pricing_revision == reservation.revision()
                && amount.currency == self.limit.currency =>
            {
                (*amount, true)
            }
            _ => (released, false),
        };

        // On overflow, pin the committed total at the limit. Every later
        // `reserve` then fails, which is the only safe reading of "we can no
        // longer represent what this session has spent".
        // `exact` stays true here: the charge *is* the real cost. What the
        // excess reports is a broken bound, not an unknown figure, and
        // conflating the two would tell the operator the opposite of what
        // happened.
        let over_bound = (charged.amount > released.amount).then_some(BoundBreach {
            charged,
            bound: released,
        });
        if let Some(breach) = over_bound {
            self.breached.get_or_insert(breach);
        }

        self.committed = self.committed.checked_add(&charged).unwrap_or(self.limit);
        Ok(Reconciled {
            charged,
            exact,
            over_bound,
        })
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
    ///
    /// Independent of [`over_bound`](Self::over_bound): a breach is charged
    /// from a figure that is perfectly well known, so `exact` stays `true`
    /// there.
    pub exact: bool,
    /// Set when the real cost exceeded the bound that admitted it — proof
    /// that the bound was not one. The gate is closed from here on; this
    /// field exists so the caller can say why rather than discovering it at
    /// the next refused `reserve`.
    pub over_bound: Option<BoundBreach>,
}

/// `a - b`, floored at zero.
///
/// Saturating rather than erroring because both callers subtract a quantity
/// the same gate already knows about: `reserve` adds exactly the bound
/// `reconcile` later releases, and it refuses any quote whose currency
/// differs from the limit's. The excess computed for a [`BoundBreach`] is
/// guarded by the comparison that decides there is one.
///
/// Be clear about what the floor does when it *is* reached, because the
/// obvious reading is backwards: zeroing `outstanding` frees headroom, so it
/// admits **more**, not less. It fails open. The wrapped alternative would
/// deny everything forever, which is at least loud — the floor is chosen
/// anyway because the divergence it papers over should not be representable
/// in the first place.
///
/// It no longer is. A reservation carries the [`GateId`] that minted it and
/// `reconcile` refuses one it did not mint *before* releasing anything, so
/// the cross-gate release that used to reach this floor — underflowing the
/// second gate and silently widening its budget — cannot be expressed. That
/// was `POTENTIALS.md` P9.
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
