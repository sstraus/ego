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

use ego_core::budget::{Metering, PricingQuote, SettlementSource};
use ego_core::cost::MeteringRevision;
use ego_core::generator::capabilities::ModelCapabilities;
use url::Url;

use crate::provider::{MissingCredential, ProviderRow};
use crate::wire::provider_cost::ProviderCost;

/// The provider id of the hosted row this milestone ships.
pub const OPENAI: &str = "openai";
/// The provider id of the local daemon this milestone ships.
pub const OLLAMA: &str = "ollama";

/// The policy under which a locally served model is not metered at all.
///
/// It names the *policy*, not a date it was copied on. A revision exists to
/// make a past figure attributable — and a date attributes a built-in policy
/// to nothing anyone can go and check. This string says exactly what was
/// asserted and under what condition, which is checkable.
///
/// A [`MeteringRevision`] and deliberately not a `PricingRevision`: there is
/// no price list here to attribute anything to, and the all-zero quote this
/// used to be spelled through claimed there was
/// (`docs/decisions/2026-08-21-how-a-model-is-metered.md` §4).
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
    pub metering: Metering,
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
    /// How the row is metered — a supplied table, a table plus the
    /// provider's own figure, or a flat fee with no per-token price at all.
    /// This is the slot every operator-supplied metering enters through, and
    /// the reason `Metering::Subscription` has a producer.
    pub metering: Metering,
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

/// Nobody is charging for this model, under [`LOCAL_UNMETERED`].
///
/// Takes no currency, and losing that parameter is the point. The old
/// all-zero [`PricingQuote`] had to be denominated in something because a
/// price list is denominated in something; this states that there is no price
/// list, so the only currency in sight is the one the budget gate mints its
/// zero reservation in — which is the gate's own limit currency, the only
/// currency the zero could be compared against.
///
/// It is still a claim, and still only ever built for a model the daemon
/// reports as served locally — never for a provider id. What changed is that
/// the claim is now "not metered", which the daemon's own answer establishes,
/// instead of "the provider charges zero per token", which nothing did.
pub fn unmetered() -> Metering {
    Metering::Unmetered {
        revision: MeteringRevision(LOCAL_UNMETERED.to_string()),
    }
}

/// A row whose access is paid for by a flat fee, under the policy `revision`
/// names.
///
/// The revision is supplied because the policy is: this repository knows of
/// no subscription it could name, and inventing a label for someone else's
/// billing arrangement would be the same fabrication the all-zero quote was.
/// An operator who pays a flat fee for a row supplies this the way they
/// supply a [`HostedModel`] — the "explicitly operator-supplied, pinned
/// source" slot of `docs/decisions/2026-08-21-how-a-model-is-metered.md` §7.
///
/// What it buys them: the gate reserves zero, so a *monetary* `--budget`
/// admits every generation and says so through
/// [`PublicEvent::MeteringSelected`](ego_core::public::PublicEvent::MeteringSelected)
/// rather than appearing to cap something. The *token* form of `--budget`
/// binds such a row exactly as it binds any other.
pub fn subscription(revision: &str) -> Metering {
    Metering::Subscription {
        revision: MeteringRevision(revision.to_string()),
    }
}

/// A row priced from a supplied table, and settled from the same table.
///
/// The ordinary hosted case: the quote bounds the reservation before the
/// request and prices the observed usage after it.
pub fn tabulated(reservation: PricingQuote) -> Metering {
    Metering::Currency {
        reservation,
        settlement: SettlementSource::Quote,
    }
}

/// A row priced from a supplied table, and settled from the provider's own
/// statement of what it will charge.
///
/// The best accounting available: an estimate computed from a table for the
/// reservation, replaced at settlement by the figure that will appear on the
/// invoice. It **removes no requirement** — `reservation` is not optional,
/// because no ex-post figure supplies an ex-ante bound, however exact it is.
/// A provider-reported row with no rates has nothing to reserve against and
/// refuses at resolution rather than running unbounded.
///
/// The [`ProviderCostRevision`](ego_core::cost::ProviderCostRevision) is
/// taken from `cost` rather than written out here, so the declaration
/// recorded at resolution and the attribution stamped on the settled figure
/// are one value and cannot drift apart.
pub fn provider_reported(reservation: PricingQuote, cost: &dyn ProviderCost) -> Metering {
    Metering::Currency {
        reservation,
        settlement: SettlementSource::ProviderReported {
            revision: cost.revision().clone(),
        },
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
            metering: hosted.metering.clone(),
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
