//! What `--model P/M` resolves to: an endpoint, what the model can do, and
//! what it costs.
//!
//! See `docs/decisions/2026-08-21-what-a-model-resolves-to.md`,
//! `ARCHITECTURE.md` §3.1 and `CLI.md`'s `--model` contract.
//!
//! Two sources, and the split is the whole design. A **hosted** model's
//! pricing and capabilities are *supplied* — this repository writes down no
//! price it cannot verify, because a stale rate does not merely mislead a
//! report, it mints a reservation below what the provider will actually
//! charge. A **local** model's are *discovered* from the daemon, because
//! `CLI.md` says local daemons are "catalogued live from the daemon" and
//! because the number that matters most cannot be written down at all: the
//! context Ollama serves depends on a server-side setting no API exposes.
//!
//! Nothing here guesses. A fact the daemon does not state stays unstated and
//! the budget gate refuses, naming what was missing.

use std::collections::BTreeMap;
use std::fmt;

use ego_core::budget::PricingQuote;
use ego_core::cost::{Amount, CurrencyCode, PricingRevision};
use ego_core::generator::capabilities::ModelCapabilities;
use url::Url;

use crate::provider::{MissingCredential, ProviderRow};

/// The provider id of the hosted row this milestone ships.
pub const OPENAI: &str = "openai";
/// The provider id of the local daemon this milestone ships.
pub const OLLAMA: &str = "ollama";

/// The revision stamped on a local model's zero quote.
///
/// It names the *policy* that produced the figure, not a date it was copied
/// on. `PricingRevision` exists to make a past cost attributable — "this
/// number came from that table" — and a date attributes a built-in policy to
/// nothing anyone can go and check. This string says exactly what was
/// asserted and under what condition, which is checkable.
pub const LOCAL_UNMETERED: &str = "ego/builtin/ollama-local-unmetered/v1";

/// Everything a generation needs to know about the model it is about to run.
///
/// One value rather than three lookups, because the three are only correct
/// *together*: capabilities bound the reservation that the pricing prices,
/// and both are resolved against the row the request is then sent to. A
/// caller that could fetch them separately could mix a quote with another
/// model's ceilings, which is the confusion `BudgetBinding` exists to catch
/// one layer down.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// The model id as the wire format must spell it — the part after the
    /// provider, never the whole slug.
    pub model: String,
    pub row: ProviderRow,
    pub capabilities: ModelCapabilities,
    pub pricing: PricingQuote,
}

/// A hosted model's facts, which nobody in this repository is in a position
/// to know.
///
/// Both halves are supplied for the same reason. A price written from memory
/// is a number the budget gate will trust and nothing can check; a context
/// window written from memory is the identical claim one field over, since
/// no hosted provider publishes a capability endpoint either.
#[derive(Debug, Clone)]
pub struct HostedModel {
    pub capabilities: ModelCapabilities,
    pub pricing: PricingQuote,
}

/// A model reference that has been split but not yet resolved.
///
/// `CLI.md` spells `--model` as `P/M`, and the model half may itself contain
/// slashes (`openrouter/z-ai/glm-4.7`), so the split is on the *first*
/// separator only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSlug {
    pub provider: String,
    pub model: String,
}

impl ModelSlug {
    /// Split `provider/model`, or say why it is not one.
    pub fn parse(slug: &str) -> Result<Self, ResolveError> {
        match slug.split_once('/') {
            Some((provider, model)) if !provider.is_empty() && !model.is_empty() => Ok(ModelSlug {
                provider: provider.to_string(),
                model: model.to_string(),
            }),
            _ => Err(ResolveError::MalformedSlug {
                given: slug.to_string(),
            }),
        }
    }
}

impl fmt::Display for ModelSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// Why a model reference did not become a [`ResolvedModel`].
///
/// Named causes rather than prose, for the same reason `UnknownCost` has
/// them: "configure a price" and "that model is not installed" send an
/// operator to two different places, and a string makes every caller guess by
/// substring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Not a `provider/model` slug at all.
    MalformedSlug { given: String },
    /// No row is configured under that provider id.
    UnknownProvider {
        provider: String,
        known: Vec<String>,
    },
    /// A hosted model with no supplied quote or capabilities. The final
    /// behaviour, not a placeholder: M5's generated catalogue becomes another
    /// producer of the same value and does not change this refusal.
    HostedModelNotConfigured { slug: String },
    /// The provider needs a credential that is not set.
    MissingCredential(MissingCredential),
    /// The daemon does not list that model.
    NotInstalled {
        model: String,
        installed: Vec<String>,
    },
    /// A local identifier that the daemon may relay upstream, which is
    /// therefore not unmetered and cannot take the zero quote. Refused rather
    /// than priced at zero: under-reserving a metered call to nothing is the
    /// free-first table's exact failure mode.
    NotLocallyServed { model: String },
    /// The daemon answered, but not with a fact resolution needs.
    Undiscoverable { model: String, fact: &'static str },
    /// The daemon could not be reached or refused the exchange.
    DaemonUnreachable { endpoint: String, message: String },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::MalformedSlug { given } => write!(
                f,
                "`{given}` is not a provider/model slug, e.g. ollama/qwen3"
            ),
            ResolveError::UnknownProvider { provider, known } => write!(
                f,
                "no provider `{provider}` is configured; known: {}",
                known.join(", ")
            ),
            ResolveError::HostedModelNotConfigured { slug } => write!(
                f,
                "no pricing or capabilities are configured for `{slug}`: a hosted \
                 model cannot be bounded from figures this build does not have"
            ),
            ResolveError::MissingCredential(missing) => write!(f, "{missing}"),
            ResolveError::NotInstalled { model, installed } => write!(
                f,
                "the daemon does not serve `{model}`; it serves: {}",
                if installed.is_empty() {
                    "nothing".to_string()
                } else {
                    installed.join(", ")
                }
            ),
            ResolveError::NotLocallyServed { model } => write!(
                f,
                "`{model}` is relayed upstream by the local daemon, so it is not \
                 unmetered and has no price this build can state"
            ),
            ResolveError::Undiscoverable { model, fact } => write!(
                f,
                "the daemon serves `{model}` but does not state its {fact}"
            ),
            ResolveError::DaemonUnreachable { endpoint, message } => {
                write!(f, "the daemon at {endpoint} did not answer: {message}")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<MissingCredential> for ResolveError {
    fn from(missing: MissingCredential) -> Self {
        ResolveError::MissingCredential(missing)
    }
}

/// A quote asserting the provider charges nothing, in `currency`.
///
/// Every category is `Some(zero)` and never `None`: `None` means *unpriced*,
/// and `upper_bound` consults all three input categories plus output, so one
/// `None` would fail the gate closed before the network and run nothing.
///
/// This is a price claim, not the absence of one (`cost.rs`: "zero is a
/// price"), so it is only ever built for a model the daemon reports as served
/// locally — never for a provider id.
pub fn unmetered(currency: CurrencyCode) -> PricingQuote {
    PricingQuote {
        revision: PricingRevision(LOCAL_UNMETERED.to_string()),
        currency,
        uncached_input: Some(Amount::ZERO),
        cache_read: Some(Amount::ZERO),
        cache_write: Some(Amount::ZERO),
        output: Some(Amount::ZERO),
    }
}

/// The provider rows and hosted facts an invocation was configured with.
///
/// Holds no local model names. `CLI.md` says local daemons are catalogued
/// live from the daemon, and a model id written here would be a snapshot of
/// one developer's disk presented as a fact about every installation.
#[derive(Debug, Clone)]
pub struct Catalogue {
    rows: BTreeMap<String, ProviderRow>,
    hosted: BTreeMap<String, HostedModel>,
}

impl Catalogue {
    /// A catalogue with rows and no hosted models, so every hosted slug
    /// refuses until one is supplied.
    pub fn new(rows: impl IntoIterator<Item = ProviderRow>) -> Self {
        Catalogue {
            rows: rows.into_iter().map(|row| (row.id.clone(), row)).collect(),
            hosted: BTreeMap::new(),
        }
    }

    /// Supply what this build cannot know about one hosted model, keyed by
    /// its full `provider/model` slug.
    pub fn with_hosted(mut self, slug: &str, model: HostedModel) -> Self {
        self.hosted.insert(slug.to_string(), model);
        self
    }

    /// The row for `provider`, or the ids that do exist.
    pub fn row(&self, provider: &str) -> Result<&ProviderRow, ResolveError> {
        self.rows
            .get(provider)
            .ok_or_else(|| ResolveError::UnknownProvider {
                provider: provider.to_string(),
                known: self.rows.keys().cloned().collect(),
            })
    }

    /// Resolve a hosted slug from what was supplied.
    ///
    /// Local slugs are not resolvable here: their facts come from the daemon,
    /// which needs a transport this type deliberately does not hold.
    pub fn resolve_hosted(&self, slug: &ModelSlug) -> Result<ResolvedModel, ResolveError> {
        let row = self.row(&slug.provider)?;
        let key = slug.to_string();
        let hosted = self
            .hosted
            .get(&key)
            .ok_or(ResolveError::HostedModelNotConfigured { slug: key })?;
        Ok(ResolvedModel {
            model: slug.model.clone(),
            row: row.clone(),
            capabilities: hosted.capabilities.clone(),
            pricing: hosted.pricing.clone(),
        })
    }
}

/// The endpoint a local daemon's control plane lives on, derived from the
/// generation endpoint the row already carries.
///
/// Derived rather than configured twice: two URLs that must agree about which
/// daemon they mean is two chances to point at different ones.
pub fn control_plane(row: &ProviderRow, path: &str) -> Result<Url, ResolveError> {
    let mut url = row.endpoint.clone();
    url.set_path(path);
    url.set_query(None);
    Ok(url)
}

/// Whether a local identifier names a model the daemon runs itself.
///
/// Ollama's `cloudPassthroughMiddleware` proxies a `:cloud` model from
/// `localhost:11434` to `ollama.com` (see `http.rs`), so a loopback endpoint
/// is not evidence of local execution and `auth: None` is not evidence of
/// free. Only the identifier distinguishes them before the request.
pub fn is_locally_served(model: &str) -> bool {
    !model.ends_with("-cloud") && !model.contains(":cloud")
}
