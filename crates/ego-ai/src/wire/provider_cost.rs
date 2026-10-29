//! Reading the money a provider states it will charge for one request.
//!

//! Cost decoders normalize wire-specific locations and units into [`Money`]
//! and [`AiEvent::ProviderCost`](ego_core::generator::port::AiEvent::ProviderCost).
//! New decoders must not change core budget or accounting code. Parse raw
//! JSON decimal digits, not `f64`, to preserve exact monetary values and avoid
//! scientific notation that [`Amount`] deliberately rejects.

use std::fmt;

use ego_core::cost::{Amount, CurrencyCode, Money, ProviderCostRevision};
use serde::Deserialize;
use serde_json::value::RawValue;

/// One provider's declaration that it states the charge for a request, and
/// the reader that turns its wire figure into [`Money`].
///
/// A trait rather than an enum on purpose: an enum would put every provider
/// in one `match`, and the third decoder would edit a file the first two
/// share. Here a decoder is a self-contained value, and the only thing that
/// has to know it exists is the provider row that carries it.
pub trait ProviderCost: fmt::Debug + Send + Sync {
    /// The declaration this decoder settles under.
    ///
    /// The *same* value reaches
    /// [`SettlementSource::ProviderReported`](ego_core::budget::SettlementSource)
    /// at resolution and
    /// [`CostAttribution::ProviderReported`](ego_core::cost::CostAttribution)
    /// at settlement, minted from here rather than written down twice: two
    /// values describing one provider contract, built in two places, are two
    /// values that can disagree once they are bytes on disk.
    fn revision(&self) -> &ProviderCostRevision;

    /// Read the stated charge out of the response's `usage` object.
    ///
    /// `None` when the provider stated none, or stated something this
    /// decoder cannot read exactly. Both settle as
    /// [`UnknownCost::ProviderReportedCostMissing`](ego_core::cost::UnknownCost::ProviderReportedCostMissing),
    /// which charges the full reserved bound — the failure direction that
    /// costs budget rather than the one that leaks it.
    fn read(&self, usage: &RawValue) -> Option<Money>;
}

/// USD, which both shipped decoders report in.
///
/// Not a fallback or a default: each decoder's own schema says the figure is
/// in US dollars, and a decoder for a provider that reported another currency
/// would say so itself. Nothing here infers a currency from silence.
fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("USD is three ASCII uppercase letters")
}

/// Pull one named field out of a `usage` object as unparsed JSON text.
///
/// Shared by the decoders rather than written per provider: "find this key,
/// keep its bytes" is not a provider-specific step, and two copies of it
/// would be two chances to parse a number on the way past.
fn field<'a>(usage: &'a RawValue, name: &str) -> Option<&'a RawValue> {
    #[derive(Deserialize)]
    struct Object<'a>(#[serde(borrow)] std::collections::BTreeMap<&'a str, &'a RawValue>);

    let object: Object<'a> = serde_json::from_str(usage.get()).ok()?;
    object.0.get(name).copied()
}

/// The field xAI states a request's charge in.
pub const XAI_COST_FIELD: &str = "usage.cost_in_usd_ticks";

/// How many decimal places one tick is: xAI defines `1 USD = 10^10` ticks,
/// so a tick is `1e-10` USD.
///
/// Exact in an [`Amount`], whose `MAX_SCALE` is 12 — two digits to spare and
/// no rounding at the edge. Checked rather than assumed, because a provider
/// quoting a finer unit would have to round in the decoder, and that would be
/// a decision to record.
const XAI_TICK_SCALE: u32 = 10;

/// xAI: `usage.cost_in_usd_ticks`, an integer count of `1e-10 USD`.
#[derive(Debug, Clone)]
pub struct XaiUsdTicks {
    revision: ProviderCostRevision,
}

impl XaiUsdTicks {
    /// A decoder that attributes what it reads to `schema_revision` of xAI's
    /// current Responses usage schema.
    ///
    /// The schema revision is supplied rather than baked in: it names *which
    /// version of the provider's contract* was decoded, and this repository
    /// is not in a position to know that a given deployment still matches the
    /// one it was written against.
    pub fn new(schema_revision: &str) -> Self {
        Self {
            revision: ProviderCostRevision {
                provider: "xai".to_string(),
                field: XAI_COST_FIELD.to_string(),
                schema_revision: schema_revision.to_string(),
            },
        }
    }
}

impl ProviderCost for XaiUsdTicks {
    fn revision(&self) -> &ProviderCostRevision {
        &self.revision
    }

    /// Restate whole ticks as USD at scale 10. Fractional ticks are refused rather than rounded
    /// because they indicate an unknown unit. A provider-reported zero charge is real zero, not
    /// a missing pricing-table rate.
    fn read(&self, usage: &RawValue) -> Option<Money> {
        let ticks: u128 = field(usage, "cost_in_usd_ticks")?.get().parse().ok()?;
        Amount::new(ticks, XAI_TICK_SCALE)
            .ok()
            .map(|amount| Money::new(amount, usd()))
    }
}

/// The field OpenRouter states a request's charge in.
pub const OPENROUTER_COST_FIELD: &str = "usage.cost";

/// OpenRouter: `usage.cost`, a plain decimal USD figure.
#[derive(Debug, Clone)]
pub struct OpenRouterUsd {
    revision: ProviderCostRevision,
}

impl OpenRouterUsd {
    /// A decoder that attributes what it reads to `schema_revision` of
    /// OpenRouter's usage-accounting response schema.
    pub fn new(schema_revision: &str) -> Self {
        Self {
            revision: ProviderCostRevision {
                provider: "openrouter".to_string(),
                field: OPENROUTER_COST_FIELD.to_string(),
                schema_revision: schema_revision.to_string(),
            },
        }
    }
}

impl ProviderCost for OpenRouterUsd {
    fn revision(&self) -> &ProviderCostRevision {
        &self.revision
    }

    /// The decimal exactly as it was sent.
    ///
    /// [`Amount`]'s parser is the whole validation: it takes digits and at
    /// most one point, and refuses a sign, an exponent, or more than
    /// `MAX_SCALE` fractional digits. A figure finer than twelve places would
    /// have to be rounded to be represented, and rounding a charge is a
    /// decision this decoder is not authorised to take — so it reports
    /// nothing and the generation settles at its bound.
    fn read(&self, usage: &RawValue) -> Option<Money> {
        let amount: Amount = field(usage, "cost")?.get().parse().ok()?;
        Some(Money::new(amount, usd()))
    }
}
