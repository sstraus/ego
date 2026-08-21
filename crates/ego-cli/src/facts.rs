//! `models.toml`: where a hosted model's price and ceilings are *supplied*.
//!
//! `docs/decisions/2026-08-21-what-a-model-resolves-to.md` §1 forbids this
//! repository from writing down a hosted price — "a price written from memory
//! is not a cheaper version of a price, it is a number the budget gate will
//! trust and that nothing can check" — and says the hosted path "works as soon
//! as a quote is configured". This module is the *configuring*. Without it
//! `Catalogue::with_hosted` has no caller and `--model openai/…` can only ever
//! refuse, which would make the refusal a dead end rather than a contract.
//!
//! ## Why its own file, and only two of them
//!
//! Two locations, `/etc/ego/models.toml` then `$EGO_HOME/models.toml`, with the
//! user's winning — §7's system-then-user order, and pricing is exactly the
//! fleet fact an administrator states once.
//!
//! **The project and project-local layers are not read here, in any posture.**
//! A repository that could state its own model's price could state a low one,
//! and a low rate mints a reservation below what the provider will really
//! charge — the precise budget unsoundness the ADR was written to prevent. It
//! is a stronger rule than the trust gate, so it is not expressed through it.
//!
//! It is a separate file rather than a `[model."…"]` table inside
//! `config.toml` because `ConfigLayer` is `deny_unknown_fields` over a schema
//! that mirrors the flags, and a price mirrors no flag: it is a fact about the
//! world, not an instruction to this run.
//!
//! ## Why a run with no money ceiling ignores the prices in it
//!
//! `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §5 gates every rate
//! source on a ceiling existing: "none of them is consulted at all unless a
//! ceiling is configured". This file is the first of the three, so the gate
//! starts here.
//!
//! It looks like a no-op today, because reading an operator's own file leaks
//! nothing. It is not one. §5's other two sources are a provider pricing
//! endpoint and a fetched third-party table, and §7's first row — "no fetch is
//! attempted, offline or not" — is a *property* of the ceiling gate rather
//! than of the fetch code: an ordinary run contacts nobody but the model
//! provider. A gate that is only added once there is something to leak
//! through has already been wrong once.
//!
//! It is also honest in its own right. A `MeteringSelected` announcing
//! `rates: operator` on a run with no ceiling would claim figures were
//! consulted that nothing consulted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ego_ai::catalogue::{self, HostedModel};
use ego_core::budget::PricingQuote;
use ego_core::ceiling::Ceilings;
use ego_core::cost::{Amount, CurrencyCode, PricingRevision};
use ego_core::generator::capabilities::ModelCapabilities;
use serde::Deserialize;

use crate::cli::UsageError;
use crate::config;

/// The file name looked for in both locations.
pub const MODELS_FILE: &str = "models.toml";
/// The administrator's copy.
const SYSTEM_DIR: &str = "/etc/ego";

/// One `models.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Facts {
    /// Keyed by the full `provider/model` slug, because that is what
    /// `--model` says and what `Catalogue::resolve_hosted` looks up. Keying
    /// by the model half alone would make the same name under two providers
    /// one entry.
    #[serde(default)]
    model: BTreeMap<String, Supplied>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Supplied {
    capabilities: ModelCapabilities,
    /// Optional, per `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §3.
    /// A run that configures no ceiling needs no prices, so a `[model."x"]`
    /// entry with capabilities alone is a complete entry and resolves.
    /// Capabilities are not optional beside it, and the asymmetry is the
    /// ruling's: maintainer ruled about prices.
    #[serde(default)]
    pricing: Option<Quote>,
}

/// A price list as an operator writes it.
///
/// Spelled out here rather than deriving `Deserialize` on `PricingQuote`
/// itself: the quote is a value the budget gate consumes, and a type that can
/// be *decoded* from anything a file says is a type that can be decoded from
/// anywhere. This is the one door, and it is in the binary that owns the
/// files.
///
/// Every category is optional and none defaults: `None` is *unpriced* and
/// makes `upper_bound` refuse, which is the correct answer for a category an
/// operator did not state. A missing field must never read as free.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Quote {
    revision: PricingRevision,
    currency: CurrencyCode,
    uncached_input: Option<Amount>,
    cache_read: Option<Amount>,
    cache_write: Option<Amount>,
    output: Option<Amount>,
}

impl From<Quote> for PricingQuote {
    fn from(quote: Quote) -> Self {
        PricingQuote {
            revision: quote.revision,
            currency: quote.currency,
            uncached_input: quote.uncached_input,
            cache_read: quote.cache_read,
            cache_write: quote.cache_write,
            output: quote.output,
        }
    }
}

/// Read one file, or nothing if it is not there.
///
/// A file that exists and cannot be parsed is a hard error naming it (§4.5:
/// invalid config is never skipped silently). Falling back to "no facts" would
/// turn a typo in a rate into `HostedModelNotConfigured`, which sends an
/// operator to look for a file they have already written.
fn read(path: &Path) -> Result<Facts, UsageError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map_err(|err: toml::de::Error| UsageError(format!("{}: {err}", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Facts::default()),
        Err(err) => Err(UsageError(format!(
            "{} could not be read: {err}",
            path.display()
        ))),
    }
}

/// What one entry resolves to: `ego-ai`'s own supplied-facts type, re-exported
/// so callers name it once.
pub type Hosted = HostedModel;

/// Every supplied hosted model, keyed by slug, user's winning over system's.
///
/// `ceilings` decides whether the *prices* come with them. Capabilities always
/// do — a request needs the model's shape whatever it costs — but the rates
/// are §5's first source and are consulted only when a ceiling needs them.
/// The file is read either way, because a malformed `models.toml` is a hard
/// error at any ceiling: an operator who typed a rate wrong should be told so
/// rather than have the typo become invisible on runs that do not read it.
pub fn supplied(ceilings: &Ceilings) -> Result<Vec<(String, HostedModel)>, UsageError> {
    let mut paths = vec![PathBuf::from(SYSTEM_DIR).join(MODELS_FILE)];
    if let Some(home) = config::ego_home() {
        paths.push(home.join(MODELS_FILE));
    }

    let mut merged: BTreeMap<String, Supplied> = BTreeMap::new();
    for path in &paths {
        merged.extend(read(path)?.model);
    }

    Ok(merged
        .into_iter()
        .map(|(slug, supplied)| {
            (
                slug,
                HostedModel {
                    capabilities: supplied.capabilities,
                    // A `models.toml` row is billed in money whether or not
                    // the operator wrote its rates down: the provider will
                    // charge either way, and this file is not where that
                    // fact is decided.
                    metering: catalogue::tabulated(),
                    // §5: the first of the three rate sources, and consulted
                    // only when a ceiling needs one. A run with no money
                    // ceiling resolves this row with no rates and runs
                    // unbounded, which is §1's ordinary case.
                    rates: ceilings
                        .needs_rates()
                        .then_some(supplied.pricing)
                        .flatten()
                        .map(|quote| catalogue::operator_rates(quote.into())),
                },
            )
        })
        .collect())
}
