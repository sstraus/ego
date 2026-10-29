//! The accounting contract: exact money, priced generations, and the
//! cumulative session cost.
//!
//! See `ARCHITECTURE.md` §3.1 and the mdkb decision `ego-acp-usage-verdict`.
//!
//! ## No floating-point money in the domain
//!
//! ACP's `Cost.amount` is an `f64`, so a lossy conversion has to happen
//! *somewhere*. It happens once, at the ACP adapter edge
//! ([`Amount::to_f64_lossy`]), and never in the log: the durable
//! representation is an exact decimal, because a session's cumulative cost is
//! a sum of many small per-generation deltas and binary floating point
//! cannot represent `0.03` at all, let alone add three thousand of them
//! without drift.
//!
//! ## Unknown is a value, not a missing value
//!
//! [`CostDelta`] is an explicit `Known | Unknown` enum rather than
//! `Option<Money>`. `Option` invites `unwrap_or_default()`, which silently
//! prices an unpriced generation at zero — the exact bug the public-event
//! contract already rejected once (mdkb `ego-usage-public-contract`). An
//! `Unknown` that carries its reason is auditable; a `None` is not.
//!
//! ## History is never repriced
//!
//! A [`CostDelta::Known`] carries the [`PricingRevision`] it was computed
//! under, and an [`FxQuote`] carries the moment it was quoted. Both are
//! persisted with the event. Rates and pricing dimensions change (model
//! revision, service tier, geography, long-context tier, cache TTL), so
//! recomputing a past cost from today's table would mutate history and break
//! replay reproducibility: the same log would reduce to a different total
//! tomorrow.

use std::fmt;

use serde::de::{Error as _, Unexpected};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::event::time::Timestamp;

/// Credential-safe reason a price source could not supply a quote.
///
/// Closed variants keep arbitrary upstream bodies, URLs, model keys and local
/// filesystem errors out of durable, public and exported notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingUnavailable {
    OfflineNoCache,
    OfflineStaleCache,
    OfflineInvalidCache,
    FetchFailed,
    InvalidTable,
    InvalidRate,
    ConflictingRates,
    MissingRow,
    CacheUnavailable,
    UnidentifiedTable,
}

impl fmt::Display for PricingUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OfflineNoCache => {
                "offline with no cached price table; fetch prices online or configure a quote"
            }
            Self::OfflineStaleCache => {
                "offline with an expired price table; refresh prices online or configure a quote"
            }
            Self::OfflineInvalidCache => {
                "offline with an unusable pricing cache; repair the cache or configure a quote"
            }
            Self::FetchFailed => {
                "price table fetch failed; check network access or configure a quote"
            }
            Self::InvalidTable => {
                "price source is not a valid table; refresh the table or configure a quote"
            }
            Self::InvalidRate => {
                "price table contains an invalid rate; correct the table or configure a quote"
            }
            Self::ConflictingRates => {
                "price table contains conflicting rates; correct the table or configure a quote"
            }
            Self::MissingRow => {
                "price table has no matching model row; update the table or configure a quote"
            }
            Self::CacheUnavailable => {
                "pricing cache cannot be read or written; check cache access or configure a quote"
            }
            Self::UnidentifiedTable => {
                "price table has no upstream validator; check the proxy or configure a quote"
            }
        })
    }
}

/// The most fractional decimal digits an [`Amount`] may carry.
///
/// Twelve places is far below the point where `u128` overflows and far above
/// what pricing needs: the smallest quantity that occurs in practice is a
/// single token at a per-million-token rate, around `1e-9` of a currency
/// unit, so twelve places leaves three orders of magnitude of headroom
/// without inviting an amount whose precision is pure noise.
pub const MAX_SCALE: u32 = 12;

/// Why a money value could not be built or combined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoneyError {
    /// Not three ASCII uppercase letters, as ISO 4217 requires.
    InvalidCurrency { found: String },
    /// More fractional digits than [`MAX_SCALE`].
    ScaleTooLarge { scale: u32 },
    /// Not a plain non-negative decimal: empty, signed, exponential, or
    /// containing something other than digits and a single decimal point.
    NotADecimal { found: String },
    /// The exact result does not fit in the representation.
    Overflow,
    /// Two amounts in different currencies cannot be added.
    CurrencyMismatch {
        left: CurrencyCode,
        right: CurrencyCode,
    },
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoneyError::InvalidCurrency { found } => {
                write!(f, "not an ISO 4217 currency code: {found:?}")
            }
            MoneyError::ScaleTooLarge { scale } => {
                write!(f, "scale {scale} exceeds the maximum of {MAX_SCALE}")
            }
            MoneyError::NotADecimal { found } => {
                write!(f, "not a non-negative decimal: {found:?}")
            }
            MoneyError::Overflow => write!(f, "the exact result does not fit"),
            MoneyError::CurrencyMismatch { left, right } => {
                write!(f, "cannot add {} to {}", right.as_str(), left.as_str())
            }
        }
    }
}

impl std::error::Error for MoneyError {}

/// An ISO 4217 currency code: exactly three ASCII uppercase letters.
///
/// Validated on construction rather than kept as a `String`, so a currency
/// that reaches the ledger has already been checked once instead of at every
/// comparison. Stored as bytes because the length is fixed and known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CurrencyCode([u8; 3]);

impl CurrencyCode {
    /// Validate and build a currency code.
    pub fn new(code: &str) -> Result<Self, MoneyError> {
        let bytes = code.as_bytes();
        let valid = bytes.len() == 3 && bytes.iter().all(u8::is_ascii_uppercase);
        if !valid {
            return Err(MoneyError::InvalidCurrency {
                found: code.to_owned(),
            });
        }
        Ok(Self([bytes[0], bytes[1], bytes[2]]))
    }

    /// The three-letter code.
    pub fn as_str(&self) -> &str {
        // Checked ASCII in `new`, and there is no other constructor: the
        // bytes are always valid UTF-8.
        std::str::from_utf8(&self.0).unwrap_or("???")
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for CurrencyCode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CurrencyCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        CurrencyCode::new(&raw)
            .map_err(|_| D::Error::invalid_value(Unexpected::Str(&raw), &"an ISO 4217 code"))
    }
}

/// An exact, non-negative decimal: `coefficient * 10^-scale`.
///
/// Non-negative because every quantity this type carries — a price, a cost
/// delta, an exchange rate, a cumulative total — is a magnitude. A signed
/// amount would only be needed for a refund or a correction, and neither is
/// modelled by this contract: a correction is a *reconciliation* that
/// replaces the total (see [`CumulativeCost::reconcile`]), not a negative
/// number added to it.
///
/// Always canonical: trailing fractional zeros are stripped on construction,
/// so `1.50` and `1.5` are the same value, compare equal, and have exactly
/// one durable spelling. That matters because these bytes are checksummed
/// into the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Amount {
    coefficient: u128,
    scale: u32,
}

/// Ordering compares *values*, which a derive cannot do: the fields are a
/// coefficient and a scale, and comparing those lexicographically makes
/// `1.5` — stored as `(15, 1)` — greater than `2`, stored as `(2, 0)`. That
/// would be a silent budget hole rather than a cosmetic wrong answer, since
/// the gate decides admission with `>`.
impl Ord for Amount {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.parts().cmp(&other.parts())
    }
}

impl PartialOrd for Amount {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Amount {
    /// Zero, at scale zero.
    pub const ZERO: Amount = Amount {
        coefficient: 0,
        scale: 0,
    };

    /// Build `coefficient * 10^-scale`, canonicalised.
    pub fn new(coefficient: u128, scale: u32) -> Result<Self, MoneyError> {
        if scale > MAX_SCALE {
            return Err(MoneyError::ScaleTooLarge { scale });
        }
        Ok(Self { coefficient, scale }.canonical())
    }

    /// Strip trailing fractional zeros, so equal values have equal fields.
    fn canonical(mut self) -> Self {
        while self.scale > 0 && self.coefficient.is_multiple_of(10) {
            self.coefficient /= 10;
            self.scale -= 1;
        }
        if self.coefficient == 0 {
            self.scale = 0;
        }
        self
    }

    /// The unscaled integer.
    pub fn coefficient(&self) -> u128 {
        self.coefficient
    }

    /// The number of fractional digits.
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// Whether this is exactly zero.
    pub fn is_zero(&self) -> bool {
        self.coefficient == 0
    }

    /// The integer part, and the fractional part restated at [`MAX_SCALE`].
    ///
    /// Comparing these two never overflows, whatever the amounts: the
    /// integer parts are compared as they are, and a fractional part is by
    /// definition below `10^scale`, so restating it costs at most
    /// `10^MAX_SCALE`. Restating the whole coefficient instead — the obvious
    /// implementation — can overflow for a large amount at a small scale,
    /// and an overflow inside a comparison has nowhere to report itself.
    fn parts(&self) -> (u128, u128) {
        let unit = 10u128.pow(self.scale);
        let fraction = (self.coefficient % unit) * 10u128.pow(MAX_SCALE - self.scale);
        (self.coefficient / unit, fraction)
    }

    /// This amount's coefficient, restated at `scale`.
    ///
    /// `None` when `scale` is below this amount's own (which would discard
    /// digits) or when the restated coefficient overflows.
    fn coefficient_at(&self, scale: u32) -> Option<u128> {
        let shift = scale.checked_sub(self.scale)?;
        let factor = 10u128.checked_pow(shift)?;
        self.coefficient.checked_mul(factor)
    }

    /// Exact addition. `None` on overflow — never a wrap and never a
    /// saturate, because a spend total pinned at `u128::MAX` would read as a
    /// real number to a budget gate. The caller decides what an
    /// unrepresentable total means (see [`CumulativeCost::add_delta`], which
    /// turns it into `Unknown`).
    pub fn checked_add(&self, other: &Amount) -> Option<Amount> {
        let scale = self.scale.max(other.scale);
        let sum = self
            .coefficient_at(scale)?
            .checked_add(other.coefficient_at(scale)?)?;
        Some(
            Amount {
                coefficient: sum,
                scale,
            }
            .canonical(),
        )
    }

    /// Multiplication, rounded half-up to [`MAX_SCALE`] when the exact
    /// product needs more digits than that.
    ///
    /// Rounding is confined to this one operation on purpose: it exists for
    /// currency conversion, where the product of an amount and a rate is
    /// genuinely not representable in general. Addition stays exact.
    pub fn checked_mul(&self, other: &Amount) -> Option<Amount> {
        let coefficient = self.coefficient.checked_mul(other.coefficient)?;
        let scale = self.scale + other.scale;
        if scale <= MAX_SCALE {
            return Some(Amount { coefficient, scale }.canonical());
        }
        let shift = scale - MAX_SCALE;
        let divisor = 10u128.checked_pow(shift)?;
        let quotient = coefficient / divisor;
        let remainder = coefficient % divisor;
        // Half-up: `remainder * 2 >= divisor` without the multiplication,
        // which could overflow for a large remainder.
        let round_up = remainder >= divisor.div_ceil(2) && remainder * 2 >= divisor;
        let coefficient = quotient.checked_add(u128::from(round_up))?;
        Some(
            Amount {
                coefficient,
                scale: MAX_SCALE,
            }
            .canonical(),
        )
    }

    /// The nearest `f64`, for the ACP adapter edge and nothing else.
    ///
    /// Deliberately named `_lossy`: `0.1` has no exact `f64`, and a total
    /// that has been through this function can no longer be added to
    /// anything without drift. Convert at the boundary, never before it.
    pub fn to_f64_lossy(&self) -> f64 {
        self.coefficient as f64 / 10f64.powi(self.scale as i32)
    }
}

impl fmt::Display for Amount {
    /// The canonical decimal spelling, which is also the durable one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.coefficient);
        }
        let digits = self.scale as usize;
        let unit = 10u128.pow(self.scale);
        write!(
            f,
            "{}.{:0width$}",
            self.coefficient / unit,
            self.coefficient % unit,
            width = digits
        )
    }
}

impl std::str::FromStr for Amount {
    type Err = MoneyError;

    /// Parse a plain non-negative decimal: digits, optionally a single `.`
    /// and more digits. No sign, no exponent, no separators — one spelling
    /// per value, so parsing a serialised amount cannot introduce a form the
    /// serialiser would never emit.
    fn from_str(s: &str) -> Result<Self, MoneyError> {
        let invalid = || MoneyError::NotADecimal {
            found: s.to_owned(),
        };
        let (int, frac) = match s.split_once('.') {
            Some((int, frac)) => (int, frac),
            None => (s, ""),
        };
        let digits_only = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if !digits_only(int) || (s.contains('.') && !digits_only(frac)) {
            return Err(invalid());
        }
        let scale = frac.len() as u32;
        if scale > MAX_SCALE {
            return Err(MoneyError::ScaleTooLarge { scale });
        }
        let coefficient: u128 = format!("{int}{frac}")
            .parse()
            .map_err(|_| MoneyError::Overflow)?;
        Amount::new(coefficient, scale)
    }
}

impl Serialize for Amount {
    /// As a decimal *string*, not a JSON number.
    ///
    /// A JSON number is exactly the trap this type exists to avoid: most
    /// readers parse one into an `f64` and the exactness is gone before the
    /// value is ever used. A string survives any reader intact and keeps the
    /// log human-readable, which is a stated goal of the JSONL format
    /// (`ARCHITECTURE.md` §5).
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse()
            .map_err(|_| D::Error::invalid_value(Unexpected::Str(&raw), &"a non-negative decimal"))
    }
}

/// An exact amount in a named currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub amount: Amount,
    pub currency: CurrencyCode,
}

impl Money {
    pub fn new(amount: Amount, currency: CurrencyCode) -> Self {
        Self { amount, currency }
    }

    /// Zero in `currency`.
    pub fn zero(currency: CurrencyCode) -> Self {
        Self::new(Amount::ZERO, currency)
    }

    /// Exact addition, refusing to add across currencies.
    ///
    /// There is no "convert on the fly" fallback here: an implicit
    /// conversion needs a rate, a rate needs a quote, and a quote needs to be
    /// persisted or the sum is not reproducible on replay. Conversion is
    /// therefore an explicit step through [`FxQuote::convert`].
    pub fn checked_add(&self, other: &Money) -> Result<Money, MoneyError> {
        if self.currency != other.currency {
            return Err(MoneyError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            });
        }
        let amount = self
            .amount
            .checked_add(&other.amount)
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(amount, self.currency))
    }
}

/// A recorded exchange rate, persisted with the event that needed it.
///
/// `quoted_at` is evidence, not an input to the arithmetic: it records when
/// the rate was obtained so an audit can tell a stale quote from a fresh one.
/// The reducer never compares it to anything — replay must produce the same
/// total forever, so an expiry evaluated at read time would make history
/// depend on when it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FxQuote {
    pub from: CurrencyCode,
    pub to: CurrencyCode,
    /// Units of `to` per one unit of `from`.
    pub rate: Amount,
    pub quoted_at: Timestamp,
}

impl FxQuote {
    /// Convert `money` into this quote's `to` currency.
    ///
    /// Fails when `money` is not in the quote's `from` currency: a quote is
    /// not a general-purpose converter, and applying `EUR→USD` to a `GBP`
    /// amount would produce a plausible number that means nothing.
    pub fn convert(&self, money: &Money) -> Result<Money, MoneyError> {
        if money.currency != self.from {
            return Err(MoneyError::CurrencyMismatch {
                left: self.from,
                right: money.currency,
            });
        }
        let amount = money
            .amount
            .checked_mul(&self.rate)
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(amount, self.to))
    }
}

/// Which immutable pricing table a cost was computed under.
///
/// Opaque on purpose: core neither parses nor compares these beyond
/// equality. Its whole job is to make a past cost attributable — "this
/// number came from *that* table" — so that a pricing change is visible in
/// the log instead of silently changing what the log means.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PricingRevision(pub String);

/// Where the rates that priced a reservation came from.
///
/// Not decoration: a ceiling enforced against rates an operator wrote down
/// and one enforced against nothing at all are materially different
/// guarantees, and a consumer that cannot tell them apart cannot report
/// honestly on either. See `2026-08-21-a-run-with-no-ceiling.md` §5 and
/// §Consequences.
///
/// **Exactly the sources that exist.** A bounded run first uses an operator's
/// quote, then the cached third-party table from story `regression`; `None`
/// records that no lookup was needed. Every variant has a producer. The
/// provider-published source was removed with its sole xAI API-key resolver by
/// Story `regression`, because a variant nothing can produce is a claim the code
/// cannot back.
///
/// **The wire form is the variant's name** (`#[serde(tag = "source")]`), so an
/// arm added between two others changes no encoding any consumer has already
/// seen. That is why [`PUBLIC_EVENT_VERSION`](crate::public::PUBLIC_EVENT_VERSION)
/// does not move for it: the constant tracks the shape a consumer parses, and
/// a consumer that never met a newly added source cannot have parsed it
/// differently before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum RateSource {
    /// Nobody looked any up. The ordinary case for a run with no ceiling, and
    /// a *property* rather than an omission: no third party was contacted.
    None,
    /// Explicit operator configuration, which may hold a negotiated rate no
    /// public table knows about. First in §5's order for that reason.
    Operator,
    /// A cached third-party table — litellm's — naming what it believes the
    /// provider charges.
    ///
    /// Last, and the doc above says why: it is a summary of prices its author
    /// is not party to and will not be invoicing. It is still worth having,
    /// because the alternative for a row whose operator wrote nothing down is
    /// no ceiling at all.
    ///
    /// The only arm that carries a payload, and it is not decoration either.
    /// `None` and `Operator` are self-describing; this arm names a file that is
    /// replaced as prices change, so "a table said so" is unattributable until
    /// it says which. Minted from the
    /// quote's own `revision` rather than restated beside it (see
    /// `ego_ai::catalogue::table_rates`), for the reason
    /// [`SettlementSource::ProviderReported`](crate::budget::SettlementSource)
    /// takes its revision from the decoder: two values describing one source,
    /// assembled in two places, are two values that can disagree once they
    /// are bytes on disk.
    Table { revision: PricingRevision },
}

/// Which metering policy established that a row has no per-token price at
/// all.
///
/// Beside [`PricingRevision`], opaque for the same reason, and deliberately
/// *not* the same type. A `PricingRevision` names a price list; a
/// subscription and a locally served model have none, and labelling their
/// figures as though a table produced them is the confusion
/// `docs/architecture.md` §4 refused. This
/// names the policy that says "not metered in currency" — which is
/// checkable, where a fabricated all-zero price list is not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MeteringRevision(pub String);

/// A provider's declaration that it states the charge for a request itself.
///
/// Three facts rather than one string, because a figure is only re-derivable
/// from a captured response if you know *where on the wire* it was read.
/// `field` is data — the name of a place — never something anything
/// dispatches on: `ego-core` cannot name a wire format
/// (`docs/architecture.md` §8), and every
/// decoder that can read a cost feeds the same
/// [`AiEvent::ProviderCost`](crate::generator::port::AiEvent::ProviderCost).
///
/// The same value is carried by the resolution-time
/// [`SettlementSource::ProviderReported`](crate::budget::SettlementSource)
/// and by the settlement-time [`CostAttribution::ProviderReported`], and the
/// settlement record is minted *from* the declaration rather than assembled
/// beside it. Two values describing one provider contract, built in two
/// places, are two values that can disagree once they are bytes on disk.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProviderCostRevision {
    /// Whose API stated the figure.
    pub provider: String,
    /// Where in the response body it was read — `usage.cost`,
    /// `usage.cost_in_usd_ticks`.
    pub field: String,
    /// Which revision of that provider's response schema was decoded, so a
    /// change in what the field means is visible in the log.
    pub schema_revision: String,
}

/// Where a settled figure came from, recorded on every [`CostDelta::Known`]
/// so a past cost stays attributable.
///
/// Each arm is a distinct answer to "who produced this number". A table
/// revision points at an artefact; a provider statement points at a response
/// field; a metering policy names the arrangement; definitely-unsent names
/// transport evidence and is valid only for zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum CostAttribution {
    /// Computed from a price list, under this revision.
    Table { revision: PricingRevision },
    /// Stated outright by the provider. Deliberately **not** a
    /// [`PricingRevision`] wearing a provider's name: a provider-stated total
    /// came from no table, and labelling it as though it did would make the
    /// log claim a pricing source exists to be checked when none does.
    ProviderReported(ProviderCostRevision),
    /// Neither: the row is not metered in currency, and this policy is what
    /// established that.
    MeteringPolicy {
        revision: MeteringRevision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        equivalent: Option<Box<SubscriptionEquivalent>>,
    },
    /// The transport proved that no request bytes left this process.
    DefinitelyUnsent,
}

/// Resolution-time facts for a display-only Standard API estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquivalentPricing {
    pub model: String,
    pub service_tier: String,
    pub quote: crate::budget::PricingQuote,
    pub long_context_threshold: u64,
}

impl EquivalentPricing {
    pub fn estimate(&self, usage: &crate::state::Usage) -> Option<Box<SubscriptionEquivalent>> {
        let input = usage
            .uncached_input_tokens
            .checked_add(usage.cache_read_tokens)?
            .checked_add(usage.cache_write_tokens)?
            .checked_add(usage.cache_write_extended_tokens)?;
        let mut quote = self.quote.clone();
        if input > self.long_context_threshold {
            let double = Amount::new(2, 0).ok()?;
            for value in [
                &mut quote.uncached_input,
                &mut quote.cache_read,
                &mut quote.cache_write,
                &mut quote.cache_write_extended,
            ]
            .into_iter()
            .flatten()
            {
                *value = value.checked_mul(&double)?;
            }
            if let Some(output) = &mut quote.output {
                *output = output.checked_mul(&Amount::new(15, 1).ok()?)?;
            }
        }
        let CostDelta::Known { amount, .. } = quote.price(usage) else {
            return None;
        };
        Some(Box::new(SubscriptionEquivalent::V1 {
            model: self.model.clone(),
            source: quote.revision,
            service_tier: self.service_tier.clone(),
            amount,
        }))
    }
}

/// Versioned display-only API equivalent. This is never a budget charge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version", rename_all = "snake_case")]
pub enum SubscriptionEquivalent {
    V1 {
        model: String,
        source: PricingRevision,
        /// API comparison service tier, not the subscription's billing tier.
        service_tier: String,
        amount: Money,
    },
}

/// Why a generation's cost is not known.
///
/// Distinguishable causes rather than a free-text string, because these are
/// read by a budget gate that must fail closed: "we have no pricing for this
/// model" and "the provider reported no usage" call for different operator
/// action, and a string forces every consumer to guess by substring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum UnknownCost {
    /// No pricing quote was resolved before the request, so there is nothing
    /// to compute a cost from and nothing to reconcile against.
    NoPricingQuote,
    /// The text-token quote cannot bound audio-specific token prices.
    AudioPricingUnavailable,
    /// The pricing quote exists but does not price one of the token
    /// categories the provider reported, so any total would be an undercount
    /// presented as a fact.
    UnpricedCategory { category: TokenCategory },
    /// The quote prices everything it was asked to, but the model does not
    /// say how many tokens there could be, so there is no quantity to price.
    ///
    /// Distinct from [`NoPricingQuote`](Self::NoPricingQuote), which this
    /// case used to be reported as, because they send an operator to two
    /// different places: a missing quote means "configure pricing", a missing
    /// ceiling means "this model's capabilities were never resolved". The
    /// confusion is worst exactly where it matters most — a free local model
    /// has a complete, honest, all-zero quote, so being told its *price* is
    /// unknown sends the reader hunting for a table that is already there.
    /// `ARCHITECTURE.md` §3.2 requires a denial to name the fact that moved.
    UnstatedCeiling { ceiling: Ceiling },
    /// The provider returned no usage for this generation, so the quantity
    /// to price is itself missing.
    ProviderReportedNoUsage,
    /// The provider declared that it states the charge for a request and
    /// then stated none.
    ///
    /// Distinct from [`ProviderReportedNoUsage`](Self::ProviderReportedNoUsage)
    /// for the reason [`UnstatedCeiling`](Self::UnstatedCeiling) was split out
    /// of [`NoPricingQuote`](Self::NoPricingQuote): "this provider said it
    /// would state a cost and did not" and "the provider reported no usage"
    /// send an operator to two different places. Under a provider-reported
    /// settlement the tokens are beside the point — the figure that was
    /// promised is the one that is missing.
    ProviderReportedCostMissing,
    /// A cost was reported in a currency the session cannot settle, with no
    /// quote to convert it.
    UnconvertibleCurrency { found: CurrencyCode },
    /// The exact total no longer fits the representation. Vanishingly
    /// unlikely with a `u128` coefficient, but the alternative to naming it
    /// is a saturating total that lies to the budget gate.
    Overflow,
}

/// Which token ceiling a bound could not be computed without.
///
/// Two variants rather than one flag because they are fixed in different
/// places: a context window comes from the model's capabilities and nothing
/// the caller writes can supply it, while an output ceiling can come from
/// either the capabilities *or* the request's own `max_output_tokens`. An
/// operator told only "a ceiling is missing" cannot tell which of those two
/// they are able to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ceiling {
    /// The model states no context window.
    Context,
    /// Neither the request nor the model states an output ceiling.
    Output,
}

/// The token categories that are priced separately.
///
/// Cache reads and cache writes are their own categories, not shades of
/// "input": providers bill them at materially different rates (a cache read
/// is typically a tenth of an uncached input token, a cache write more than
/// one), so folding them together makes the cost of a cached conversation
/// unknowable. `SessionState::usage` used to carry only input/output, which
/// is why even the industry-standard context-occupancy proxy could not be
/// computed from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenCategory {
    /// Provider-side web searches, not tokens.
    WebSearch,
    /// Input tokens billed at the full rate — not served from cache.
    UncachedInput,
    /// Input tokens served from the provider's prompt cache.
    CacheRead,
    /// Input tokens written into the provider's prompt cache at its standard
    /// retention.
    CacheWrite,
    /// Input tokens written at an extended retention the request asked for.
    ///
    /// Its own category rather than a multiplier on [`Self::CacheWrite`],
    /// because it is a rate a table states rather than one ego derives:
    /// Anthropic prices a 1-hour write at 2x base against a 5-minute write's
    /// 1.25x, and nothing guarantees the ratio holds for the next provider or
    /// the next revision. A derived price would be a guess wearing a
    /// measurement's clothes.
    CacheWriteExtended,
    /// Generated tokens, including reasoning tokens the provider bills as
    /// output.
    Output,
}

/// The cost of one wire attempt.
///
/// `CostDelta`, never `cost`: `ARCHITECTURE.md` §3.1 produces a
/// per-generation figure while ACP wants a cumulative session total, and a
/// field named `cost` on an event would be read as the latter. The
/// cumulative total is [`CumulativeCost`], derived by the reducer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cost", rename_all = "snake_case")]
pub enum CostDelta {
    /// Priced, and attributable to whatever produced the figure.
    Known {
        amount: Money,
        /// Who produced this number, and how it can be checked.
        attribution: CostAttribution,
        /// Present only when `amount`'s currency differs from the session's
        /// settlement currency. Persisted with the event so the conversion
        /// replays identically forever.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        fx: Option<FxQuote>,
    },
    /// Not priced, and why.
    Unknown(UnknownCost),
}

impl CostDelta {
    /// A generation that cost nothing — a locally served model, a row paid
    /// for by a flat fee. Emitted as `Known(0)`, never `Unknown`: the figure
    /// is known, and reporting it as unknown would poison the session total
    /// for no reason.
    ///
    /// `currency` comes from whatever the zero will be compared against —
    /// the budget gate's own limit currency — because a metering with no
    /// per-token price states none of its own.
    pub fn free(currency: CurrencyCode, attribution: CostAttribution) -> Self {
        CostDelta::Known {
            amount: Money::zero(currency),
            attribution,
            fx: None,
        }
    }
}

/// The running cost of a session.
///
/// Three states, not `Option<Money>`, because "nothing has been priced yet"
/// and "something could not be priced" must not collapse into one value: the
/// first is a total of zero, the second is a total nobody knows, and a budget
/// gate has to treat them oppositely.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CumulativeCost {
    /// No priced generation has entered the session yet. There is no total
    /// and, just as importantly, no settlement currency: the first `Known`
    /// delta establishes it.
    #[default]
    Unstarted,
    /// Every generation so far was priced, and they summed exactly.
    Known { total: Money },
    /// At least one generation could not be priced, so the total is not a
    /// number. It stays this way until [`CumulativeCost::reconcile`]
    /// supplies an authoritative figure — adding a later known delta to an
    /// unknown base would produce a total that looks exact and is not.
    Unknown(UnknownCost),
}

impl CumulativeCost {
    /// Fold one generation's cost into the running total.
    ///
    /// The rules, in one place because they are the whole contract:
    /// - an `Unknown` delta poisons the total, whatever it was;
    /// - once `Unknown`, the total stays `Unknown` — later known deltas
    ///   cannot repair a sum with a hole in it;
    /// - the first `Known` delta fixes the settlement currency;
    /// - a later delta in another currency is converted through its own
    ///   persisted [`FxQuote`], or poisons the total. It is never added as
    ///   if the currencies matched.
    pub fn add_delta(&mut self, delta: &CostDelta) {
        let (amount, fx) = match delta {
            CostDelta::Unknown(reason) => {
                // Keep the reason that *broke* the total. A total already
                // unknown cannot become more unknown, and the first cause is
                // the one an operator can act on; later ones are the next
                // symptoms, and overwriting would bury the diagnosis under
                // them.
                if !matches!(self, CumulativeCost::Unknown(_)) {
                    *self = CumulativeCost::Unknown(reason.clone());
                }
                return;
            }
            CostDelta::Known { amount, fx, .. } => (amount, fx),
        };

        let total = match self {
            CumulativeCost::Unknown(_) => return,
            CumulativeCost::Unstarted => {
                // The first priced generation sets the settlement currency,
                // so there is nothing to convert to yet.
                *self = CumulativeCost::Known { total: *amount };
                return;
            }
            CumulativeCost::Known { total } => *total,
        };

        let settled = if amount.currency == total.currency {
            *amount
        } else {
            match fx.as_ref().map(|q| q.convert(amount)) {
                Some(Ok(converted)) if converted.currency == total.currency => converted,
                _ => {
                    *self = CumulativeCost::Unknown(UnknownCost::UnconvertibleCurrency {
                        found: amount.currency,
                    });
                    return;
                }
            }
        };

        *self = match total.checked_add(&settled) {
            Ok(total) => CumulativeCost::Known { total },
            Err(_) => CumulativeCost::Unknown(UnknownCost::Overflow),
        };
    }

    /// Replace the total with an authoritative figure, clearing an
    /// `Unknown`.
    ///
    /// The only way out of `Unknown`, and deliberately a *replacement*: the
    /// gap that made the total unknown cannot be filled by addition, because
    /// nobody knows how big it was. Something that does know — a provider's
    /// billing API, an operator — states the total outright.
    pub fn reconcile(&mut self, total: Money) {
        *self = CumulativeCost::Known { total };
    }

    /// The total, when it is known.
    pub fn known(&self) -> Option<&Money> {
        match self {
            CumulativeCost::Known { total } => Some(total),
            _ => None,
        }
    }
}
