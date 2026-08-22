//! Asking xAI what it will charge, instead of writing a price down.
//!
//! `docs/decisions/2026-08-21-grok-as-a-provider.md` §3. xAI is the one
//! provider among the six that publishes a **price** endpoint, and it
//! publishes it *per authenticating key* — so a quote here is not a table this
//! repository maintains and hopes is current, it is what the party that will
//! send the invoice says it charges. That honours
//! `2026-08-21-what-a-model-resolves-to.md` §1's principle (never trust a
//! price this build cannot verify) better than refusing does, which is why
//! this endpoint gets a resolver and no other hosted provider does.
//!
//! **Capabilities stay supplied.** `language-models` carries no context
//! window, no max-completion-tokens and no reasoning-effort field — checked
//! against the response schema, not assumed. So an xAI model still needs a
//! supplied [`HostedModel`], and only its *price* comes off the wire.
//!
//! # The three ways a published number is not a price
//!
//! Each of these is a way for a discovered quote to be *worse* than a
//! hand-written one, which is the argument that a discovered price is only
//! safer if its sentinels are decoded:
//!
//! 1. **The unit.** Figures are USD cents per 100M tokens and [`PricingQuote`]
//!    holds a rate per *single* token, so every figure is scaled by 10⁻¹⁰.
//!    Exact in an [`Amount`] (`MAX_SCALE` is 12), with no division anywhere.
//! 2. **The tier.** Every category has a `_long_context` twin and a quote
//!    holds one rate per category, so six figures collapse into three — and
//!    the collapse must go *upward*. See [`collapse`].
//! 3. **The zero.** A zero in a `_long_context` field means "the base figure
//!    applies at all lengths", and a zero `long_context_threshold` means "this
//!    model has no long-context tier". Decoding either as `Amount::ZERO` would
//!    assert the provider charges **nothing** for exactly the requests that
//!    cost the most.

use std::sync::Arc;

use ego_core::budget::{PricingQuote, ResolvedRates};
use ego_core::cost::{Amount, CurrencyCode, PricingRevision, RateSource};
use serde::Deserialize;

use crate::catalogue::{ResolveError, ResolvedModel};
use crate::http::{Http, HttpRequest, Method, fetch};
use crate::provider::ResolvedCredential;

/// The path, joined onto the row's base like every other path in this crate.
const PATH: &str = "language-models";

/// How many decimal places take a published figure to a rate per token.
///
/// The figures are **USD cents per 100M tokens**, so the conversion is
/// `/100` to reach dollars and `/100_000_000` to reach one token: 10⁻¹⁰
/// exactly, and therefore a scale rather than a division.
const CENTS_PER_HUNDRED_MILLION: u32 = 10;

/// One entry of `GET /v1/language-models`, trimmed to what a quote needs.
///
/// Every price is an integer count, and `#[serde(default)]` is deliberately
/// **absent** on the three base figures: a response missing them is a response
/// this decoder does not understand, and defaulting would turn that into a
/// free model. The `_long_context` twins default to zero because zero is the
/// schema's own spelling for "absent" there, which is a different fact.
#[derive(Debug, Deserialize)]
struct LanguageModel {
    id: String,
    prompt_text_token_price: u128,
    cached_prompt_text_token_price: u128,
    completion_text_token_price: u128,
    #[serde(default)]
    prompt_text_token_price_long_context: u128,
    #[serde(default)]
    cached_prompt_text_token_price_long_context: u128,
    #[serde(default)]
    completion_text_token_price_long_context: u128,
    #[serde(default)]
    long_context_threshold: u64,
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LanguageModels {
    #[serde(default)]
    models: Vec<LanguageModel>,
}

/// Fill in an xAI resolution's price by asking xAI, when nobody wrote one down.
///
/// Takes the resolution the supplied table already produced rather than the
/// table itself, which puts two things beyond doubt. Capabilities were found
/// **first**, so a model nobody configured is refused before a request is sent
/// — the refusal names the missing configuration instead of costing a round
/// trip to price a model that could not have been used anyway. And the metering
/// is the operator's: discovering a per-token price says nothing about whether
/// this key is billed per token, and a row an operator declared
/// [`Subscription`](ego_core::budget::Metering::Subscription) does not stop
/// being one because a public table quotes it.
///
/// **An operator's rates win, and no request is sent at all.**
/// `2026-08-21-a-run-with-no-ceiling.md` §5 orders the three sources with the
/// operator first, first hit wins, and the reason survives contact with a live
/// endpoint: a published figure is what xAI charges the public, and an operator
/// who wrote a rate down may hold a negotiated one that no endpoint will admit
/// to.
///
/// # A known defect, stated at the line that causes it
///
/// **`rates.is_some()` is the wrong question**, and `POTENTIALS.md` P28 is the
/// entry. `ego_cli::facts` produces `rates: None` for two different facts:
/// nobody supplied a price, *and* somebody did but this run has no ceiling
/// that needs one, so it was deliberately stripped. This function reads that
/// absence as the first alone, so on an unbudgeted run it would open a
/// connection the operator asked not to be opened, and attach
/// [`RateSource::ProviderPublished`] to a resolution meant to carry no rates —
/// contradicting `2026-08-21-a-run-with-no-ceiling.md` §5, which `facts.rs`'s
/// own comment states correctly and this function was written without reading.
///
/// It cannot fire today: xAI is a candidate, its row is not in
/// `ego_cli::model::rows`, and the test pinning that names this defect in its
/// failure message so a promotion cannot miss it. The fix belongs on
/// `Resolution`, which should carry whether this run needs rates the same way
/// it already carries the credential — **inferring a decision from the absence
/// of its output is the general shape of the bug**, not an xAI quirk. Story
/// 034 owns that path; a gate built for xAI alone is one thrown away when it
/// lands.
pub async fn price(
    supplied: ResolvedModel,
    http: &Arc<dyn Http>,
    credential: &ResolvedCredential,
) -> Result<ResolvedModel, ResolveError> {
    if supplied.rates.is_some() {
        return Ok(supplied);
    }

    let url = supplied
        .row
        .base
        .join(PATH)
        .map_err(|err| ResolveError::EndpointUnreachable {
            endpoint: format!("{}{PATH}", supplied.row.base),
            message: format!("the row's base does not make a URL: {err}"),
        })?;

    let mut headers = credential.headers();
    headers.push(("accept".to_string(), "application/json".to_string()));
    let body = fetch(
        http,
        HttpRequest {
            url: url.clone(),
            method: Method::Get,
            headers,
            body: None,
        },
    )
    .await
    .map_err(|message| ResolveError::EndpointUnreachable {
        endpoint: url.to_string(),
        message,
    })?;

    let listed: LanguageModels =
        serde_json::from_slice(&body).map_err(|err| ResolveError::EndpointUnreachable {
            endpoint: url.to_string(),
            message: format!("its price list did not parse: {err}"),
        })?;

    let published = listed
        .models
        .iter()
        .find(|model| model.id == supplied.model)
        .ok_or_else(|| ResolveError::EndpointUnreachable {
            endpoint: url.to_string(),
            message: format!(
                "it publishes no price for `{}`; it lists {}",
                supplied.model,
                listed
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;

    let quote = quote(published, supplied.capabilities.max_context_tokens).ok_or_else(|| {
        ResolveError::EndpointUnreachable {
            endpoint: url.to_string(),
            message: format!("its price for `{}` does not fit an Amount", supplied.model),
        }
    })?;

    Ok(ResolvedModel {
        rates: Some(ResolvedRates {
            quote,
            // Not `operator_rates`: nobody here wrote these figures down. The
            // distinction is the whole reason `RateSource` exists — a run that
            // reported enforcing a ceiling against a provider's published
            // rates while holding a third party's table would be making a
            // false statement about its own bound.
            source: RateSource::ProviderPublished,
        }),
        ..supplied
    })
}

/// The quote a published entry becomes, at the tier this model can reach.
fn quote(published: &LanguageModel, context: Option<u64>) -> Option<PricingQuote> {
    let rate = |figure: u128| Amount::new(figure, CENTS_PER_HUNDRED_MILLION).ok();

    let uncached_input = rate(collapse(
        published.prompt_text_token_price,
        published.prompt_text_token_price_long_context,
        published.long_context_threshold,
        context,
    ))?;
    let cache_read = rate(collapse(
        published.cached_prompt_text_token_price,
        published.cached_prompt_text_token_price_long_context,
        published.long_context_threshold,
        context,
    ))?;
    let output = rate(collapse(
        published.completion_text_token_price,
        published.completion_text_token_price_long_context,
        published.long_context_threshold,
        context,
    ))?;

    Some(PricingQuote {
        revision: revision(published),
        currency: CurrencyCode::new("USD").expect("USD is valid"),
        uncached_input: Some(uncached_input),
        cache_read: Some(cache_read),
        // **The uncached rate, and specifically not zero and not `None`.**
        // xAI neither reports cache-write tokens nor prices them, so a cache
        // write is billed as ordinary input. `None` would fail the gate closed
        // for every xAI request, because `dearest_input_rate` consults all
        // three input categories unconditionally; zero would assert xAI writes
        // caches for free, which nothing published says.
        cache_write: Some(uncached_input),
        // Nothing on this wire distinguishes an extended cache write, and a
        // category with a zero count is never consulted. `None` is the honest
        // "unpriced", which is what an absent category is.
        cache_write_extended: None,
        output: Some(output),
    })
}

/// Which of a category's two published figures this model can be charged at.
///
/// **Upward unless provably not**, and the asymmetry is the point: taking the
/// base figure because the endpoint lists it first reserves half the real
/// maximum charge on a long-context model, and nothing downstream notices.
/// `PricingQuote::upper_bound` already assumes a full context window at the
/// dearest rate, so the tier that matches that assumption is the long one.
///
/// The two zeros are the schema's own spellings for *absent* and are decoded
/// as such rather than as a price:
///
/// - a zero `threshold` means the model has no long-context tier at all;
/// - a zero `long` figure means the base figure applies at every length.
///
/// The one case that takes the base figure on purpose is a context window
/// *provably* below the threshold. Unknown is not below: an unsupplied window
/// cannot reach this function, and a supplied `None` takes the dearer tier,
/// because "we do not know how big this model's context is" is not evidence
/// that it is small.
fn collapse(base: u128, long: u128, threshold: u64, context: Option<u64>) -> u128 {
    if threshold == 0 || long == 0 {
        return base;
    }
    match context {
        Some(window) if window < threshold => base,
        _ => long,
    }
}

/// What this quote is attributable to, from what xAI stamps on it.
///
/// `fingerprint` and `version` are the real referent `PricingRevision` always
/// wanted: a date says when a figure was copied, and these say *which
/// published table* it came from. When the endpoint states neither, the
/// revision says so rather than inventing one — a revision that lied about
/// being pinned would be worse than one that admits it is not.
fn revision(published: &LanguageModel) -> PricingRevision {
    let stamp = match (&published.fingerprint, &published.version) {
        (Some(fingerprint), Some(version)) => format!("{fingerprint}/{version}"),
        (Some(only), None) | (None, Some(only)) => only.clone(),
        (None, None) => "unstamped".to_string(),
    };
    PricingRevision(format!("xai/language-models/{}/{stamp}", published.id))
}
