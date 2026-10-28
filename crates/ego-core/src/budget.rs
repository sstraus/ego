//! The budget gate: an unforgeable [`Reservation`] that [`Generator`] demands
//! before it will generate anything.
//!
//! See `ARCHITECTURE.md` §3.1 and the mdkb decision `ego-acp-usage-verdict`.
//!
//! ## Why the gate is in the signature, not at a call site
//!
//! A configured cap applies to known and estimable spend before generation.
//! Missing prices do not stop generation: they leave an explicit gap in cap
//! coverage. Callers publish a durable anomaly notice rather than invent cost.
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
//! ## A ceiling is optional; the lifecycle is not
//!
//! `ARCHITECTURE.md` §1: nobody has to
//! configure a ceiling, and a run without one needs no prices at all. So the
//! gate holds a [`Ledger`], which is [`Unlimited`](Ledger::Unlimited) when no
//! ceiling was configured and [`Limited`](Ledger::Limited) when one was, and
//! [`reserve`](BudgetGate::reserve) takes the rates as an `Option`. What a
//! run without a ceiling does **not** lose is the lifecycle: it still
//! reserves, streams, settles and reconciles, because that is where usage,
//! cost attribution and the durable ledger come from. It simply has nothing
//! to enforce.
//!
//! The reservation it gets is [`Reserved::Unbounded`] — a named absence and
//! not a zero. A zero is a *claim* about a quantity, indistinguishable from
//! the genuine zero a subscription row mints; a variant is the absence of a
//! claim. The distinction is the one that ADR's §2 turns on, and it is the
//! same distinction [`PricingQuote`]'s `None` rates make one type over.
//!
//! ## The quote is resolved before the request and never recomputed
//!
//! [`PricingQuote`] is immutable and carries its [`PricingRevision`]. The
//! same quote prices the reservation and the actual usage, so a pricing
//! table that changes mid-session cannot make a generation cost one thing
//! going in and another coming out.
//!
//! It is deliberately **not** part of [`Metering`]. A metering records the
//! **billing arrangement** — a fact about the world — while the presence of
//! rates records **our knowledge**, and a design that conflates the two has
//! to invent a variant for ignorance. See that ADR's §3.
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
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::cost::{
    Amount, Ceiling, CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money, MoneyError,
    PricingRevision, ProviderCostRevision, RateSource, TokenCategory, UnknownCost,
};
use crate::event::id::GateId;
use crate::generator::attempt::{AttemptStream, Settlement};
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
    ///
    /// Extended cache writes are reachable only when this request selects
    /// long retention. Include that rate then, and fail closed if unpriced.
    fn dearest_input_rate(&self, req: &GenRequest) -> Result<Amount, UnknownCost> {
        let mut categories = vec![
            TokenCategory::UncachedInput,
            TokenCategory::CacheRead,
            TokenCategory::CacheWrite,
        ];
        if req.cache.as_ref().is_some_and(|cache| {
            cache.retention == crate::generator::capabilities::CacheRetention::Long
        }) {
            categories.push(TokenCategory::CacheWriteExtended);
        }
        categories
            .into_iter()
            .try_fold(Amount::ZERO, |dearest, category| {
                Ok(dearest.max(self.rate(category)?))
            })
    }

    /// The most `req` could possibly cost against a model with `caps`.
    ///
    /// Text requests reserve a conservative UTF-8 byte bound, including
    /// message framing and tool schemas, capped by the context window. Image
    /// and opaque reasoning requests retain the full context bound. Input
    /// uses the dearest reachable rate, including cache creation. Output uses
    /// the explicit request cap, or the model maximum when none is set.
    ///
    /// A model whose context or output ceiling is unknown has no upper
    /// bound, so the gate fails closed. That is the whole point: an
    /// unbounded spend is exactly the case a budget exists to prevent.
    pub fn upper_bound(
        &self,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<Money, UnknownCost> {
        if req
            .messages
            .messages()
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, crate::generator::types::ContentBlock::Audio { .. }))
        {
            return Err(UnknownCost::AudioPricingUnavailable);
        }
        let (context, output) = ceilings(req, caps)?;
        let input_bound = match req.web_search {
            // Each search can re-enter the model with a full context.
            Some(search) => context
                .checked_mul(u64::from(search.max_requests.get()) + 1)
                .ok_or(UnknownCost::Overflow)?,
            None => request_input_bound(req, context)?,
        };
        let context = Amount::new(u128::from(input_bound), 0).map_err(|_| UnknownCost::Overflow)?;
        let input = self
            .dearest_input_rate(req)?
            .checked_mul(&context)
            .ok_or(UnknownCost::Overflow)?;
        let output = self.line(TokenCategory::Output, output)?;
        let mut total = input.checked_add(&output).ok_or(UnknownCost::Overflow)?;
        if let Some(search) = req.web_search {
            total = total
                .checked_add(&self.line(
                    TokenCategory::WebSearch,
                    u64::from(search.max_requests.get()),
                )?)
                .ok_or(UnknownCost::Overflow)?;
        }
        Ok(Money::new(total, self.currency))
    }
}

/// A quote with the fact of where it was found.
///
/// One value and not two parameters, because the two must not drift: a run
/// that reports enforcing a ceiling against the provider's own published
/// rates while actually holding a third-party table's is making the exact
/// false statement `2026-08-21-a-run-with-no-ceiling.md` §10 adds
/// [`RateSource`] to prevent. Resolution (§5) mints them together.
///
/// The provenance never reaches the arithmetic: [`BudgetGate::reserve`] takes
/// the [`PricingQuote`] alone, because where a rate came from must not change
/// what it computes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRates {
    pub quote: PricingQuote,
    pub source: RateSource,
}

/// Where the settled figure for a currency-metered generation comes from.
///
/// A property of the **billing arrangement**, which is why it survives inside
/// [`Metering::Currency`] while the quote does not: whether a provider states
/// its own charge is a fact about that provider, true whether or not this
/// build holds a rate table for it. A stated cost arrives after the tokens
/// are spent, so it can improve the record and can never gate the spend —
/// which is why the reservation reads the rates and the settlement reads
/// this.
///
/// See `ARCHITECTURE.md` §1 and §2, as
/// narrowed by `2026-08-21-a-run-with-no-ceiling.md` §3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettlementSource {
    /// The same quote that minted the reservation prices the observed usage.
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

/// How a model is metered, and therefore what may settle a generation
/// against it.
///
/// **A fact about the billing arrangement, and nothing about what this build
/// knows.** The rates that price a reservation are a separate, optional value
/// resolved beside a `Metering` and handed to [`BudgetGate::reserve`]. An
/// OpenAI row is metered in currency whether or not anybody has configured a
/// table for it — the provider will charge — and folding "we have rates" into
/// this vocabulary would force a fourth variant meaning *ignorance*, inside a
/// type whose other three variants are claims about the world. See
/// `ARCHITECTURE.md` §3.
///
/// The `Currency` variant keeps `settlement` for the same reason it lost the
/// quote: whether a provider states its own charge is a property of that
/// provider. See `2026-08-21-how-a-model-is-metered.md` §1 for why the
/// settlement axis is a discriminant at all.
///
/// `ego-core` gains no constant and opens no file for any of this.
/// `Metering` is a shape; every number that fills it enters through the
/// resolver in `ego-ai`.
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

    /// The most `req` could cost against a model with `caps`, under this
    /// metering and whatever rates were resolved for it.
    ///
    /// For a currency row this is [`PricingQuote::upper_bound`] — and
    /// `Ok(None)` when no rates were resolved, which is *not* an error: a run
    /// with no ceiling never looks any up, and §1 of the no-ceiling ADR
    /// forbids refusing for want of a price nobody asked for. Whether an
    /// absent bound is fatal is the *gate's* decision, taken against its
    /// [`Ledger`], and it is taken in exactly one place.
    ///
    /// For the other two it is **a zero monetary reservation minted from the
    /// policy**, in `settlement` — the currency it will be compared against,
    /// because neither variant states one of its own. The rates are ignored
    /// there even when present: a flat-fee row costs nothing per token
    /// whatever some table says about it.
    ///
    /// The ceilings are still resolved, and still refuse when they are
    /// missing. A row that is not metered in currency is not thereby a row
    /// whose shape nobody has to know: the binding says what was authorised,
    /// and "against a model with an unstated context window" is not an
    /// authorisation anyone can check later.
    pub fn upper_bound(
        &self,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        caps: &ModelCapabilities,
        settlement: CurrencyCode,
    ) -> Result<Option<Money>, UnknownCost> {
        match self {
            Metering::Currency { .. } => match rates {
                Some(quote) => quote.upper_bound(req, caps).map(Some),
                None => Ok(None),
            },
            Metering::Subscription { .. } | Metering::Unmetered { .. } => {
                policy_ceilings(req, caps)?;
                Ok(Some(Money::zero(settlement)))
            }
        }
    }

    /// Whether a ceiling held on `ledger` will rest on per-token rates —
    /// which is exactly when it is worth going and finding some.
    ///
    /// The gate does not need this: [`upper_bound`](Self::upper_bound) above
    /// already tolerates every case it excludes, minting a zero for a
    /// policy-metered row and `None` for a currency row with no quote.
    /// *Resolution* needs it. `2026-08-21-a-run-with-no-ceiling.md` §5 makes
    /// the three rate sources conditional — "none of them is consulted at all
    /// unless a ceiling is configured" — and one of the three reads a cached
    /// file while another opens a socket. So the condition has to be askable
    /// before any of that happens, by something that holds no rates yet.
    ///
    /// It answers the same question [`CeilingEffect`](crate::public::CeilingEffect)
    /// publishes, and `PublicEvent::metering_selected` is written in terms of
    /// it for that reason: a build where the event said a ceiling binds while
    /// resolution had skipped the lookup would be enforcing nothing and
    /// reporting that it was.
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

/// A [`Metering`] with its figures removed: the labels a reservation binds
/// itself to, and nothing that could be substituted cheaply behind one.
///
/// Deliberately not the `Metering` itself, for exactly the reason
/// [`BudgetBinding`] has always held a [`PricingRevision`] rather than a
/// quote. A fabricated cheap table wearing a genuine label is caught by
/// *recomputing the bound* in [`Reservation::authorises`], not by comparing
/// labels; folding the rates into the binding would collapse two checks that
/// catch different things into one, and leave the cheaper-quote substitution
/// reported as a moved binding rather than as the changed quote it is.
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

// Non-currency policies need no output quantity to prove a zero monetary
// reservation. Zero records an unstated output ceiling without inventing one.
fn policy_ceilings(req: &GenRequest, caps: &ModelCapabilities) -> Result<(u64, u64), UnknownCost> {
    let mut policy = req.clone();
    if policy.max_output_tokens.is_none() && caps.max_output_tokens.is_none() {
        policy.max_output_tokens = Some(0);
    }
    ceilings(&policy, caps)
}

/// Text is bounded by UTF-8 bytes, including serialized message framing and
/// tool schemas. Images and opaque reasoning have no model-independent token
/// bound here, so retain the context ceiling for those requests.
fn request_input_bound(req: &GenRequest, context: u64) -> Result<u64, UnknownCost> {
    use crate::generator::types::ContentBlock;
    if req.messages.messages().iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Image { .. }
                    | ContentBlock::Audio { .. }
                    | ContentBlock::Reasoning { .. }
                    | ContentBlock::ProviderContent { .. }
            )
        })
    }) {
        return Ok(context);
    }
    let bytes =
        serde_json::to_vec(&(&req.messages, &req.tools)).map_err(|_| UnknownCost::Overflow)?;
    let bound = u64::try_from(bytes.len()).map_err(|_| UnknownCost::Overflow)?;
    Ok(bound.min(context))
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
///
/// `metering` and `rates` are two fields and not one for the reason the ADR
/// gives one type up: the arrangement and our knowledge of its prices are
/// different facts, and a reservation has to bind both — the arrangement so a
/// switched metering is caught, the revision so a mid-session table change
/// cannot price one generation two ways.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetBinding {
    model: String,
    context_ceiling: u64,
    output_ceiling: u64,
    metering: MeteringIdentity,
    /// Which table priced the bound, when a table did. `None` on a row with
    /// no rates and on every row that is not metered in currency.
    rates: Option<PricingRevision>,
}

impl BudgetBinding {
    /// The budget-relevant shape of `req` against `caps` under `metering`
    /// priced by `rates`, or the reason it has none — the same reason
    /// [`Metering::upper_bound`] would refuse, because it is the same
    /// resolution.
    pub fn of(
        req: &GenRequest,
        caps: &ModelCapabilities,
        metering: &Metering,
        rates: Option<&PricingQuote>,
    ) -> Result<Self, UnknownCost> {
        let (context_ceiling, output_ceiling) = match metering {
            Metering::Currency { .. } => ceilings(req, caps)?,
            _ => policy_ceilings(req, caps)?,
        };
        Ok(Self {
            model: req.model.clone(),
            context_ceiling,
            output_ceiling,
            metering: metering.identity(),
            // A table revision is only meaningful where a table is what
            // settles: on a flat-fee row the policy revision is the binding
            // fact and the rates are ignored (see `Metering::upper_bound`),
            // so recording one here would bind a figure nothing consults.
            rates: match metering {
                Metering::Currency { .. } => rates.map(|quote| quote.revision.clone()),
                Metering::Subscription { .. } | Metering::Unmetered { .. } => None,
            },
        })
    }

    /// Whether a figure attributed this way may be charged as the real cost
    /// of the generation this authorised.
    ///
    /// A **table** figure must have been computed under the very revision the
    /// reservation was authorised with: that check exists to stop a
    /// mid-session table change from pricing one generation two ways. A row
    /// authorised with *no* rates has no revision to match, so no table
    /// figure settles it — which is right, since no table produced the bound.
    ///
    /// A **provider-reported** figure is accepted whatever table the
    /// reservation used, and that is a deliberate exception rather than an
    /// oversight. It carries no table revision at all, so the revision check
    /// would reject it on every single generation and the gate would silently
    /// discard the best figure it will ever receive in favour of a
    /// pessimistic bound. A provider's own invoice figure is not a table
    /// change — the reason for the check does not reach it. See
    /// `ARCHITECTURE.md` §5.
    ///
    /// A **policy** figure must name the policy that was authorised, for the
    /// same reason as a table.
    fn settles(&self, attribution: &CostAttribution, amount: &Money) -> bool {
        match (&self.metering, attribution) {
            (_, CostAttribution::DefinitelyUnsent) => amount.amount.is_zero(),
            (_, CostAttribution::ProviderReported(_)) => true,
            (MeteringIdentity::Currency { .. }, CostAttribution::Table { revision: settled }) => {
                self.rates.as_ref() == Some(settled)
            }
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

    /// The metering this was authorised under.
    pub fn metering(&self) -> &MeteringIdentity {
        &self.metering
    }

    /// The table revision that priced the bound, when a table did.
    pub fn rates(&self) -> Option<&PricingRevision> {
        self.rates.as_ref()
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
        if a.metering != p.metering {
            out.push(("metering", a.metering.to_string(), p.metering.to_string()));
        }
        if a.rates != p.rates {
            let name = |r: &Option<PricingRevision>| {
                r.as_ref().map_or("no rates".to_string(), |r| r.0.clone())
            };
            out.push(("rates", name(&a.rates), name(&p.rates)));
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
    /// An unbounded reservation was presented for a different model. The
    /// thin counterpart of [`WrongBinding`](Self::WrongBinding): a model is
    /// the only fact an unbounded reservation holds, so it is the only one
    /// that can be checked.
    WrongModel {
        authorised: String,
        presented: String,
    },
    /// The presented quote does not bound this generation at the figure that
    /// was authorised. Caught separately from the binding because
    /// [`PricingRevision`] is an opaque public string: a quote with cheaper
    /// rates can carry a genuine revision label, and only recomputing the
    /// bound notices.
    QuoteChanged { authorised: Money, presented: Money },
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
            BudgetDenied::WrongModel {
                authorised,
                presented,
            } => write!(
                f,
                "budget denied: this reservation authorised {authorised}, not {presented}"
            ),
            BudgetDenied::QuoteChanged {
                authorised,
                presented,
            } => write!(
                f,
                "budget denied: the presented quote bounds this generation at {} {}, \
                 not the {} that was authorised",
                presented.amount, presented.currency, authorised.amount
            ),
        }
    }
}

impl std::error::Error for BudgetDenied {}

/// What a reservation actually holds.
///
/// Two shapes rather than a `Money` that might be zero, because the two
/// available zeros are both wrong. A **zero bound** is a claim about a
/// quantity, and one indistinguishable from the genuine zero a subscription
/// row mints; **the computed bound** requires rates that a run with no
/// ceiling was never obliged to have. There is no third `Money`, so there is
/// a third shape. See `2026-08-21-a-run-with-no-ceiling.md` §2.
///
/// [`Unbounded`](Self::Unbounded) cannot leak into a real ceiling's
/// accounting, and that needs no new mechanism: a [`Reservation`] carries the
/// [`GateId`] that minted it and [`BudgetGate::reconcile`] refuses one it did
/// not mint before releasing anything, so an unbounded reservation can only
/// ever be settled at the unlimited gate that issued it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reserved {
    /// A hold against a ceiling: what was bounded, and what it was computed
    /// from.
    Bounded {
        upper_bound: Money,
        binding: BudgetBinding,
    },
    /// No ceiling was configured and no rates were in hand, so nothing was
    /// bounded and nothing is held.
    ///
    /// What this loses, stated rather than glossed: no conservative fallback
    /// at reconcile, and no [`BoundBreach`] detection. Both losses are
    /// confined to runs where no claim was made — there is no ceiling to
    /// protect and no quote to falsify — and an unpriceable settlement there
    /// produces [`CumulativeCost::Unknown`](crate::cost::CumulativeCost),
    /// which is the honest answer rather than a fabricated total.
    Unbounded { model: String },
}

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
    reserved: Reserved,
    _sealed: Sealed,
}

impl Reservation {
    /// What this holds — a bound, or the absence of one.
    pub fn reserved(&self) -> &Reserved {
        &self.reserved
    }

    /// The bound held against the budget while this generation runs, or
    /// `None` when nothing was bounded.
    pub fn upper_bound(&self) -> Option<&Money> {
        match &self.reserved {
            Reserved::Bounded { upper_bound, .. } => Some(upper_bound),
            Reserved::Unbounded { .. } => None,
        }
    }

    /// Which gate is holding this bound, and therefore the only gate that
    /// may release it.
    pub fn gate(&self) -> GateId {
        self.gate
    }

    /// What this authorised, when there was a bound to authorise it at.
    pub fn binding(&self) -> Option<&BudgetBinding> {
        match &self.reserved {
            Reserved::Bounded { binding, .. } => Some(binding),
            Reserved::Unbounded { .. } => None,
        }
    }

    /// The model this authorises a generation against — the one fact both
    /// shapes carry, and the only one an unbounded reservation can be
    /// checked on.
    pub fn model(&self) -> &str {
        match &self.reserved {
            Reserved::Bounded { binding, .. } => binding.model(),
            Reserved::Unbounded { model } => model,
        }
    }

    /// The metering this was authorised under, when a bound recorded one.
    /// The same metering must settle the actual usage, so a mid-session
    /// pricing change cannot make one generation cost two different things.
    pub fn metering(&self) -> Option<&MeteringIdentity> {
        self.binding().map(BudgetBinding::metering)
    }

    /// Refuse unless this reservation is the one that authorised exactly
    /// this generation.
    ///
    /// Two checks, because neither implies the other. The [`BudgetBinding`]
    /// catches a substituted model, moved ceilings or a switched metering and
    /// names which; the recomputed bound catches a substituted *quote*, which
    /// the binding cannot see because [`PricingRevision`] is an opaque public
    /// string and a fabricated cheap table can wear a genuine label.
    ///
    /// An [`Unbounded`](Reserved::Unbounded) reservation is checked on the
    /// model alone, which is every fact it holds. There is nothing weaker
    /// about that than about the value itself: it authorised no figure, so no
    /// substitution can overspend one.
    ///
    /// Not called by [`BudgetGate::reconcile`], which has no request to
    /// check against: this is the check at the moment of *use*, and it takes
    /// the same `(metering, rates, req, caps)` as [`BudgetGate::reserve`] so
    /// a caller that has one has the other.
    pub fn authorises(
        &self,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<(), BudgetDenied> {
        let (upper_bound, binding) = match &self.reserved {
            Reserved::Bounded {
                upper_bound,
                binding,
            } => (upper_bound, binding),
            Reserved::Unbounded { model } => {
                return if model == &req.model {
                    Ok(())
                } else {
                    Err(BudgetDenied::WrongModel {
                        authorised: model.clone(),
                        presented: req.model.clone(),
                    })
                };
            }
        };

        let presented =
            BudgetBinding::of(req, caps, metering, rates).map_err(BudgetDenied::CostUnknown)?;
        if &presented != binding {
            return Err(BudgetDenied::WrongBinding(Box::new(BindingMismatch {
                authorised: binding.clone(),
                presented,
            })));
        }
        // Recomputed in the currency the authorised bound is already stated
        // in: a metering that is not in currency mints its zero against
        // whatever it will be compared to, and that is the only currency this
        // reservation was ever measured in.
        let recomputed = metering
            .upper_bound(rates, req, caps, upper_bound.currency)
            .map_err(BudgetDenied::CostUnknown)?
            // Unreachable from a matching binding: the binding records
            // whether rates priced it, so equal bindings had equal rates and
            // the recomputation took the same arm. Refused rather than
            // unwrapped, because "the bound vanished" must never read as
            // "the bound agreed".
            .ok_or(BudgetDenied::CostUnknown(UnknownCost::NoPricingQuote))?;
        if recomputed != *upper_bound {
            return Err(BudgetDenied::QuoteChanged {
                authorised: *upper_bound,
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

/// What a gate holds generations against, when it holds them against
/// anything.
///
/// `2026-08-21-a-run-with-no-ceiling.md` §4: ego imposes no ceiling of its
/// own, so a run without one is the ordinary case rather than the exception.
/// The running totals live inside [`Limited`](Self::Limited) rather than
/// beside the ledger because they are meaningless without it: `outstanding`
/// exists to be compared against a limit, and a gate with no limit that kept
/// counting would publish a number nothing consults and nothing checks.
///
/// The size difference between the two variants is deliberate and not worth
/// boxing away: there is exactly one `Ledger` per session, it lives inside
/// the gate for the whole run, and it is never collected. Boxing the totals
/// would buy a smaller enum nobody allocates in bulk, at the cost of a heap
/// indirection on the hot path every reservation takes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Ledger {
    /// No ceiling was configured. Nothing is enforced and nothing is held.
    Unlimited,
    /// A ceiling, with the two quantities held against it.
    ///
    /// They must not be confused: `committed` is what has been spent and
    /// reconciled, `outstanding` is what is bound up in generations still
    /// running. A gate that tracked only `committed` would happily authorise
    /// a hundred concurrent generations that each fit the remaining budget
    /// alone.
    Limited {
        limit: Money,
        /// The higher figure that was asked for, when an administrator's
        /// non-relaxable ceiling reduced it.
        ///
        /// Held here rather than passed around beside the gate so that "what
        /// is in force" and "what was asked for" cannot be reported by two
        /// values that disagree. §4 requires the clamp be *stated*, and this
        /// is the only thing that knows both figures.
        clamped_from: Option<Money>,
        committed: Money,
        outstanding: Money,
    },
}

/// A session's spend ceiling, if it has one, and the authority that admits
/// generations against it.
#[derive(Debug)]
pub struct BudgetGate {
    id: GateId,
    /// What this session settles in, ceiling or no ceiling.
    ///
    /// Deliberately outside the [`Ledger`]: a subscription row costs a real,
    /// attributable zero whether or not anybody set a budget, and a zero has
    /// to be denominated in something. The settlement currency is a fact
    /// about the *session*, which is why it survives `Unlimited`.
    currency: CurrencyCode,
    ledger: Ledger,
    /// Evidence that a settled generation cost more than its estimate.
    /// The actual charge still counts against a configured cap.
    breached: Option<BoundBreach>,
}

impl BudgetGate {
    /// A gate that admits generations until `limit` is reached.
    pub fn new(limit: Money) -> Self {
        Self::bounded(limit, None)
    }

    /// The same, where `requested` was reduced to `limit` by an
    /// administrator's non-relaxable ceiling.
    ///
    /// A separate constructor rather than an `Option` on `new`, so that the
    /// ordinary case cannot accidentally claim a clamp and the clamped case
    /// cannot silently forget one.
    pub fn clamped(limit: Money, requested: Money) -> Self {
        Self::bounded(limit, Some(requested))
    }

    fn bounded(limit: Money, clamped_from: Option<Money>) -> Self {
        let currency = limit.currency;
        Self {
            id: GateId::new(),
            currency,
            ledger: Ledger::Limited {
                limit,
                clamped_from,
                committed: Money::zero(currency),
                outstanding: Money::zero(currency),
            },
            breached: None,
        }
    }

    /// A gate with no ceiling, settling in `currency`.
    ///
    /// Not `new` with an enormous limit. That spelling was in the tree —
    /// `Amount::new(u128::MAX, 0)` — and it is the surrogate this variant
    /// exists to delete: a ceiling nobody set, wearing the largest number the
    /// representation can hold, is still a ceiling as far as every arithmetic
    /// path is concerned, and it makes "no budget was configured"
    /// indistinguishable from "a budget of ten undecillion was".
    pub fn unlimited(currency: CurrencyCode) -> Self {
        Self {
            id: GateId::new(),
            currency,
            ledger: Ledger::Unlimited,
            breached: None,
        }
    }

    /// This gate's identity, stamped on every reservation it mints.
    pub fn id(&self) -> GateId {
        self.id
    }

    /// What this session settles in.
    pub fn currency(&self) -> CurrencyCode {
        self.currency
    }

    /// What this gate enforces, if anything.
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The first estimate breach, if one has happened.
    pub fn breached(&self) -> Option<&BoundBreach> {
        self.breached.as_ref()
    }

    /// Spent and reconciled, when there is a ceiling to spend against.
    pub fn committed(&self) -> Option<&Money> {
        match &self.ledger {
            Ledger::Limited { committed, .. } => Some(committed),
            Ledger::Unlimited => None,
        }
    }

    /// Held by reservations still in flight, when there is a ceiling to hold
    /// against.
    pub fn outstanding(&self) -> Option<&Money> {
        match &self.ledger {
            Ledger::Limited { outstanding, .. } => Some(outstanding),
            Ledger::Unlimited => None,
        }
    }

    /// The ceiling, when one was configured.
    pub fn limit(&self) -> Option<&Money> {
        match &self.ledger {
            Ledger::Limited { limit, .. } => Some(limit),
            Ledger::Unlimited => None,
        }
    }

    /// Admit one generation, enforcing a configured cap on known and estimable
    /// spend. An unusable price grants an unbounded reservation; callers must
    /// announce that the cap cannot cover it. Unknown actual cost charges the
    /// reserved estimate when one exists, without inventing actual billed cost.
    pub fn reserve(
        &mut self,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<Reservation, BudgetDenied> {
        let Ledger::Limited {
            limit,
            clamped_from,
            committed,
            outstanding,
        } = self.ledger
        else {
            return Ok(self.mint(self.unenforced(metering, rates, req, caps)));
        };

        // Known and estimable spend is capped. Missing or unusable prices
        // grant no monetary reservation; the journal announces that gap.
        let reserved = self.unenforced(metering, rates, req, caps);
        let Reserved::Bounded {
            upper_bound,
            binding,
        } = reserved
        else {
            if committed
                .checked_add(&outstanding)
                .map_or(true, |held| held.amount >= limit.amount)
            {
                return Err(BudgetDenied::WouldExceed(Box::new(Overrun {
                    committed,
                    outstanding,
                    requested: Money::zero(self.currency),
                    limit,
                })));
            }
            return Ok(self.mint(reserved));
        };

        let held = committed
            .checked_add(&outstanding)
            .and_then(|held| held.checked_add(&upper_bound))
            .map_err(|_: MoneyError| BudgetDenied::CostUnknown(UnknownCost::Overflow))?;
        if held.amount > limit.amount {
            return Err(BudgetDenied::WouldExceed(Box::new(Overrun {
                committed,
                outstanding,
                requested: upper_bound,
                limit,
            })));
        }

        self.ledger = Ledger::Limited {
            limit,
            clamped_from,
            committed,
            outstanding: outstanding
                .checked_add(&upper_bound)
                .map_err(|_: MoneyError| BudgetDenied::CostUnknown(UnknownCost::Overflow))?,
        };
        Ok(self.mint(Reserved::Bounded {
            upper_bound,
            binding,
        }))
    }

    /// What an `Unlimited` gate reserves: a bound if one falls out, and the
    /// absence of one otherwise. Never a refusal — see [`reserve`](Self::reserve).
    fn unenforced(
        &self,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Reserved {
        let unbounded = || Reserved::Unbounded {
            model: req.model.clone(),
        };
        if matches!(metering, Metering::Currency { .. })
            && rates.is_some_and(|quote| quote.currency != self.currency)
        {
            return unbounded();
        }
        let Ok(Some(upper_bound)) = metering.upper_bound(rates, req, caps, self.currency) else {
            return unbounded();
        };
        match BudgetBinding::of(req, caps, metering, rates) {
            Ok(binding) => Reserved::Bounded {
                upper_bound,
                binding,
            },
            Err(_) => unbounded(),
        }
    }

    /// Stamp this gate's identity on an authorisation. The one place a
    /// [`Reservation`] is constructed, so [`Sealed`] is named exactly once.
    fn mint(&self, reserved: Reserved) -> Reservation {
        Reservation {
            gate: self.id,
            reserved,
            _sealed: Sealed(()),
        }
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
    /// Later admissions still enforce the configured cap against that charge.
    /// An [`Unbounded`](Reserved::Unbounded) reservation released nothing, so
    /// there is nothing to release and no bound to fall back to. It charges
    /// the actual figure when there is one and reports
    /// [`charged: None`](Reconciled::charged) when there is not — an unknown
    /// cost stays unknown rather than becoming a zero that reads as free.
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

        // A figure settles this reservation if it is in the currency this
        // session settles in, and was produced by the arrangement the
        // reservation authorised.
        //
        // An `Unbounded` reservation authorised no arrangement — there is no
        // binding, because there was no bound to bind. The attribution check
        // is therefore not *skipped* here so much as vacuous: there is no
        // authorised revision for a figure to contradict, so any known cost
        // in the settlement currency is the cost. Requiring a binding
        // instead would make `charged` unconditionally `None` on every
        // unbounded generation, turning an exact figure into an unknown one
        // and poisoning a run's total that has a perfectly good answer.
        let settles = |attribution: &CostAttribution, amount: &Money| {
            amount.currency == self.currency
                && (!matches!(attribution, CostAttribution::DefinitelyUnsent)
                    || amount.amount.is_zero())
                && match reservation.binding() {
                    Some(binding) => binding.settles(attribution, amount),
                    None => true,
                }
        };
        let stated = match actual {
            CostDelta::Known {
                amount,
                attribution,
                ..
            } if settles(attribution, amount) => Some(*amount),
            _ => None,
        };

        let Some(released) = reservation.upper_bound().copied() else {
            // Nothing was held. A known actual still counts against a cap;
            // an unknown actual stays unknown, never a fabricated zero.
            if let (
                Some(charged),
                Ledger::Limited {
                    limit, committed, ..
                },
            ) = (stated, &mut self.ledger)
            {
                *committed = committed.checked_add(&charged).unwrap_or(*limit);
            }
            return Ok(Reconciled {
                charged: stated,
                exact: stated.is_some(),
                over_bound: None,
            });
        };

        let (charged, exact) = match stated {
            Some(amount) => (amount, true),
            None => (released, false),
        };

        // `exact` stays true on a breach: the charge *is* the real cost. What
        // the excess reports is a broken bound, not an unknown figure, and
        // conflating the two would tell the operator the opposite of what
        // happened.
        let over_bound = (charged.amount > released.amount).then_some(BoundBreach {
            charged,
            bound: released,
        });
        if let Some(breach) = over_bound {
            self.breached.get_or_insert(breach);
        }

        if let Ledger::Limited {
            limit,
            clamped_from,
            committed,
            outstanding,
        } = self.ledger
        {
            self.ledger = Ledger::Limited {
                limit,
                clamped_from,
                // On overflow, pin the committed total at the limit. Every
                // later `reserve` then fails, which is the only safe reading
                // of "we can no longer represent what this session has spent".
                committed: committed.checked_add(&charged).unwrap_or(limit),
                outstanding: subtract_or_zero(&outstanding, &released),
            };
        }

        Ok(Reconciled {
            charged: Some(charged),
            exact,
            over_bound,
        })
    }
}

/// An immutable view of one shared gate's accounting state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetSnapshot {
    id: GateId,
    currency: CurrencyCode,
    ledger: Ledger,
    breached: Option<BoundBreach>,
}

impl BudgetSnapshot {
    /// The gate whose immutable state this captures.
    pub fn id(&self) -> GateId {
        self.id
    }

    /// The session settlement currency.
    pub fn currency(&self) -> CurrencyCode {
        self.currency
    }

    /// The ledger at the instant the snapshot was taken.
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The breach that had closed the gate, if any.
    pub fn breached(&self) -> Option<&BoundBreach> {
        self.breached.as_ref()
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
/// Reservations and attempt streams remain linear values. The lock is held
/// only while minting a reservation, taking a snapshot, or consuming a fully
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
            breached: gate.breached,
        })
    }

    /// Reserve one generation while holding the gate only for admission.
    pub fn reserve(
        &self,
        metering: &Metering,
        rates: Option<&PricingQuote>,
        req: &GenRequest,
        caps: &ModelCapabilities,
    ) -> Result<Reservation, SharedBudgetError> {
        self.inner
            .gate
            .lock()
            .map_err(|_| SharedBudgetError::Synchronization(BudgetSynchronizationError))?
            .reserve(metering, rates, req, caps)
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

/// What a [`BudgetGate::reconcile`] actually charged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    /// The amount charged for this generation, and the amount added to a
    /// `Limited` ledger's committed total.
    ///
    /// `None` only on an [`Unbounded`](Reserved::Unbounded) reservation whose
    /// cost could not be established: there is no bound to fall back to and
    /// no ceiling that a zero would be safe against, so the honest answer is
    /// that nothing is known. A `Some(zero)` here would read as *free*.
    pub charged: Option<Money>,
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
