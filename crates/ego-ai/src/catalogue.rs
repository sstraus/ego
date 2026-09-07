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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use ego_core::budget::{Metering, PricingQuote, ResolvedRates, SettlementSource};
use ego_core::cost::{MeteringRevision, RateSource};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::public::MeteringKind;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use crate::generator::StaticRequestProfile;
use crate::http::Http;
use crate::provider::{MissingCredential, ProviderRow, ResolvedCredential};
use crate::resolver::{Resolution, StaticResolution};
use crate::wire::WireFormat;
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
/// *together*: capabilities bound the reservation that the rates price, and
/// both are resolved against the row the request is then sent to. A caller
/// that could fetch them separately could mix a quote with another model's
/// ceilings, which is the confusion `BudgetBinding` exists to catch one layer
/// down.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// The model id as the wire format must spell it — the part after the
    /// provider, never the whole slug.
    pub model: String,
    pub row: ProviderRow,
    pub wire: &'static dyn WireFormat,
    pub capabilities: ModelCapabilities,
    pub metering: Metering,
    /// What this build knows the row costs, which may be nothing.
    ///
    /// Split from `metering` along the line
    /// `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §3 draws: the
    /// metering is a fact about the billing arrangement, the rates are a fact
    /// about our knowledge, and a run that configured no ceiling looks no
    /// rates up at all.
    pub rates: Option<ResolvedRates>,
}

/// A hosted model's facts, which nobody in this repository is in a position
/// to know.
///
/// **Capabilities stay required and rates do not**, and the asymmetry is the
/// ruling's, not an oversight. maintainer ruled about prices: a run that asks for
/// no ceiling needs none, so demanding one refuses a working run to buy
/// nothing. The argument for supplied *capabilities* was always a separate
/// one and survives intact — a context window written from memory is the same
/// unverifiable claim a price is, and it feeds far more than the budget:
/// `CLI.md` §4.1 publishes context occupancy from it, compaction triggers on
/// it, and `fallback_gaps` compares it.
#[derive(Debug, Clone)]
pub struct HostedModel {
    pub capabilities: ModelCapabilities,
    /// The configuration layer whose winning row supplied these facts.
    pub authority: SuppliedAuthority,
    /// How the row is metered — currency, or a flat fee with no per-token
    /// price at all. This is the slot every operator-supplied metering enters
    /// through, and the reason `Metering::Subscription` has a producer.
    pub metering: Metering,
    /// What the operator says the row costs, if they said.
    ///
    /// Optional, and **not** a reason to reach for `Metering::Unmetered`. An
    /// OpenAI row is metered in currency whether or not this build holds
    /// rates for it — the provider will charge — and spelling ignorance as
    /// "nobody is charging" would under-reserve every metered row the moment
    /// somebody did ask for a ceiling.
    pub rates: Option<ResolvedRates>,
}

/// A model reference that has been split but not yet resolved.
///
/// `CLI.md` spells `--model` as `P/M`, and the model half may itself contain
/// slashes (`openrouter/z-ai/glm-4.7`), so the split is on the *first*
/// separator only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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

impl Serialize for ModelSlug {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ModelSlug {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let slug = String::deserialize(deserializer)?;
        Self::parse(&slug).map_err(serde::de::Error::custom)
    }
}

/// The only layers allowed to author hosted catalogue facts, weakest first.
///
/// The order is the override order and is derived rather than written down
/// twice: `Builtin < System < User`, so a merge that keeps the greater
/// authority per slug is the merge the layering describes.
///
/// [`Builtin`](Self::Builtin) is not a configuration layer, which is why the
/// name of this type is now a little wide for it. It is the catalogue
/// compiled into the binary, and it exists because
/// `docs/decisions/2026-09-07-a-builtin-catalogue-layer-below-system-and-user.md`
/// amended the ban on built-in facts: an installation with an API key and no
/// files could not run a hosted model at all, and "write a `models.toml`
/// first" is not an out-of-the-box path. It stays *below* both files, so
/// nothing it says can override what an operator states — which is the
/// property that made the amendment safe rather than a reversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppliedAuthority {
    Builtin,
    System,
    User,
}

/// Who authoritatively stated that a model is present in the catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ModelFactProvenance {
    Supplied { authority: SuppliedAuthority },
    Discovered { provider: String },
}

impl ModelFactProvenance {
    pub fn supplied(authority: SuppliedAuthority) -> Self {
        Self::Supplied { authority }
    }

    pub fn discovered(provider: impl Into<String>) -> Self {
        Self::Discovered {
            provider: provider.into(),
        }
    }
}

/// Whether this invocation observed a fact or retained a prior observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactFreshness {
    Current,
    Cached,
}

/// A closed, machine-readable reason why a row cannot currently be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAvailabilityCause {
    MissingCredential,
    SourceUnreachable,
    SourceMalformed,
    IncompleteFacts,
    MappingUnavailable,
}

impl FromStr for ModelAvailabilityCause {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "missing_credential" => Ok(Self::MissingCredential),
            "source_unreachable" => Ok(Self::SourceUnreachable),
            "source_malformed" => Ok(Self::SourceMalformed),
            "incomplete_facts" => Ok(Self::IncompleteFacts),
            "mapping_unavailable" => Ok(Self::MappingUnavailable),
            _ => Err(format!("unknown model availability cause `{value}`")),
        }
    }
}

/// Whether a known catalogue row may currently be selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ModelAvailability {
    Available,
    Unavailable {
        cause: ModelAvailabilityCause,
        message: String,
    },
}

/// One stable, public row shared by every catalogue consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogueEntry {
    pub slug: ModelSlug,
    pub capabilities: ModelCapabilities,
    pub metering: MeteringKind,
    pub provenance: ModelFactProvenance,
    pub freshness: FactFreshness,
    pub availability: ModelAvailability,
}

/// The latest observation status for one model source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogueSourceStatus {
    pub provider: String,
    pub provenance: ModelFactProvenance,
    pub freshness: FactFreshness,
    pub availability: ModelAvailability,
}

impl ModelCatalogueSourceStatus {
    pub fn unavailable(
        provider: impl Into<String>,
        provenance: ModelFactProvenance,
        freshness: FactFreshness,
        cause: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            provenance,
            freshness,
            availability: ModelAvailability::Unavailable {
                cause: cause
                    .parse()
                    .expect("catalogue source status uses a closed cause"),
                message: message.into(),
            },
        }
    }
}

/// Version 1 of the ordered public catalogue projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogueSnapshot {
    pub v: u32,
    pub models: Vec<ModelCatalogueEntry>,
    pub sources: Vec<ModelCatalogueSourceStatus>,
}

impl ModelCatalogueSnapshot {
    pub fn from_entries_and_sources(
        mut models: Vec<ModelCatalogueEntry>,
        mut sources: Vec<ModelCatalogueSourceStatus>,
    ) -> Self {
        models.sort_by(|left, right| left.slug.cmp(&right.slug));
        sources.sort_by(|left, right| left.provider.cmp(&right.provider));
        Self {
            v: 1,
            models,
            sources,
        }
    }
}

/// One successfully enumerated row before freshness and availability overlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDiscoveryEntry {
    pub slug: ModelSlug,
    pub capabilities: ModelCapabilities,
    pub metering: MeteringKind,
    pub provenance: ModelFactProvenance,
}

/// The subset of availability causes an enumerator itself can establish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDiscoveryFailureCause {
    SourceUnreachable,
    SourceMalformed,
    IncompleteFacts,
}

impl FromStr for ModelDiscoveryFailureCause {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "source_unreachable" => Ok(Self::SourceUnreachable),
            "source_malformed" => Ok(Self::SourceMalformed),
            "incomplete_facts" => Ok(Self::IncompleteFacts),
            _ => Err(format!("unknown model discovery failure cause `{value}`")),
        }
    }
}

/// A failed row decode, or an unnamed failure of the source as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDiscoveryFailure {
    pub model: Option<String>,
    pub cause: ModelDiscoveryFailureCause,
    pub message: String,
}

impl ModelDiscoveryFailure {
    pub fn named(model: impl Into<String>, cause: &str, message: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
            cause: cause
                .parse()
                .expect("named discovery failure uses a closed cause"),
            message: message.into(),
        }
    }

    pub fn source(cause: &str, message: impl Into<String>) -> Self {
        Self {
            model: None,
            cause: cause
                .parse()
                .expect("source discovery failure uses a closed cause"),
            message: message.into(),
        }
    }
}

/// The resolver-owned authority kind for one enumeration result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelDiscoverySource {
    Supplied,
    Discovered { provider: String },
}

/// A validated enumeration result. Private fields preserve its completeness invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDiscoveryReport {
    source: ModelDiscoverySource,
    models: Vec<ModelDiscoveryEntry>,
    failures: Vec<ModelDiscoveryFailure>,
    complete: bool,
}

impl ModelDiscoveryReport {
    pub fn try_new(
        source: ModelDiscoverySource,
        mut models: Vec<ModelDiscoveryEntry>,
        failures: Vec<ModelDiscoveryFailure>,
        complete: bool,
    ) -> Result<Self, String> {
        match &source {
            ModelDiscoverySource::Supplied => {
                if models
                    .iter()
                    .any(|entry| !matches!(&entry.provenance, ModelFactProvenance::Supplied { .. }))
                {
                    return Err("a supplied discovery report can contain only supplied rows".into());
                }
            }
            ModelDiscoverySource::Discovered { provider } => {
                if provider.is_empty() {
                    return Err("a discovered report requires a non-empty provider id".into());
                }
                if let Some(entry) = models.iter().find(|entry| {
                    entry.slug.provider.as_str() != provider
                        || !matches!(
                            &entry.provenance,
                            ModelFactProvenance::Discovered { provider: discovered }
                                if discovered == provider
                        )
                }) {
                    return Err(format!(
                        "discovery row `{}` was not discovered by `{provider}`",
                        entry.slug
                    ));
                }
            }
        }
        if !complete && !failures.iter().any(|failure| failure.model.is_none()) {
            return Err("an incomplete discovery report requires an unnamed source failure".into());
        }
        if complete && failures.iter().any(|failure| failure.model.is_none()) {
            return Err(
                "a complete discovery report cannot carry an unnamed source failure".into(),
            );
        }
        if failures.iter().any(|failure| {
            failure
                .model
                .as_deref()
                .is_some_and(|model| model.is_empty())
        }) {
            return Err("a named discovery failure requires a non-empty model name".into());
        }
        models.sort_by(|left, right| left.slug.cmp(&right.slug));
        let mut seen = BTreeSet::new();
        if models.iter().any(|entry| !seen.insert(entry.slug.clone())) {
            return Err("a discovery report cannot contain duplicate model slugs".into());
        }
        Ok(Self {
            source,
            models,
            failures,
            complete,
        })
    }

    pub fn source(&self) -> &ModelDiscoverySource {
        &self.source
    }

    pub fn models(&self) -> &[ModelDiscoveryEntry] {
        &self.models
    }

    pub fn failures(&self) -> &[ModelDiscoveryFailure] {
        &self.failures
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

/// One cached discovered row and any failure attached to that retained fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedDiscoveryModel {
    pub entry: ModelDiscoveryEntry,
    pub failure: Option<ModelDiscoveryFailure>,
}

/// The last coherent discovery state for one provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedDiscoverySlice {
    pub models: Vec<CachedDiscoveryModel>,
    pub source_failure: Option<ModelDiscoveryFailure>,
}

impl CachedDiscoverySlice {
    pub fn merge(self, report: ModelDiscoveryReport) -> Self {
        let source_failure = report
            .failures
            .iter()
            .find(|failure| failure.model.is_none())
            .cloned();

        let mut current: BTreeMap<ModelSlug, CachedDiscoveryModel> = report
            .models
            .into_iter()
            .filter(|entry| matches!(&entry.provenance, ModelFactProvenance::Discovered { .. }))
            .map(|entry| {
                (
                    entry.slug.clone(),
                    CachedDiscoveryModel {
                        entry,
                        failure: None,
                    },
                )
            })
            .collect();

        let named: BTreeMap<&str, &ModelDiscoveryFailure> = report
            .failures
            .iter()
            .filter_map(|failure| failure.model.as_deref().map(|model| (model, failure)))
            .collect();
        for cached in self.models {
            if current.contains_key(&cached.entry.slug) {
                continue;
            }
            let named_failure = named
                .get(cached.entry.slug.model.as_str())
                .map(|failure| (*failure).clone());
            if named_failure.is_some() || !report.complete {
                let failure = named_failure.or_else(|| source_failure.clone());
                current.insert(
                    cached.entry.slug.clone(),
                    CachedDiscoveryModel {
                        entry: cached.entry,
                        failure,
                    },
                );
            }
        }

        Self {
            models: current.into_values().collect(),
            source_failure: if report.complete {
                None
            } else {
                source_failure
            },
        }
    }

    fn validate(&self, provider: &str) -> Result<(), String> {
        if self
            .source_failure
            .as_ref()
            .is_some_and(|failure| failure.model.is_some())
        {
            return Err(format!(
                "cached source failure for `{provider}` names a model"
            ));
        }
        let mut seen = BTreeSet::new();
        for model in &self.models {
            if model.entry.slug.provider != provider {
                return Err(format!(
                    "cached provider `{provider}` contains `{}`",
                    model.entry.slug
                ));
            }
            match &model.entry.provenance {
                ModelFactProvenance::Discovered {
                    provider: discovered,
                } if discovered == provider => {}
                _ => {
                    return Err(format!(
                        "cached row `{}` is not discovered by `{provider}`",
                        model.entry.slug
                    ));
                }
            }
            if !seen.insert(model.entry.slug.clone()) {
                return Err(format!(
                    "cached provider `{provider}` contains a duplicate row"
                ));
            }
            if let Some(failure) = &model.failure {
                if let Some(name) = &failure.model {
                    if name != &model.entry.slug.model {
                        return Err(format!(
                            "cached row failure does not belong to `{}`",
                            model.entry.slug
                        ));
                    }
                } else if self.source_failure.as_ref() != Some(failure) {
                    return Err(format!(
                        "cached row `{}` carries an unnamed failure other than its source failure",
                        model.entry.slug
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Private, durable version 1 discovery cache document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryCacheV1 {
    pub v: u32,
    pub providers: BTreeMap<String, CachedDiscoverySlice>,
}

impl Default for DiscoveryCacheV1 {
    fn default() -> Self {
        Self {
            v: 1,
            providers: BTreeMap::new(),
        }
    }
}

impl DiscoveryCacheV1 {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let cache: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        cache.validate()?;
        Ok(cache)
    }

    pub fn merge_provider(
        &mut self,
        provider: &str,
        report: ModelDiscoveryReport,
    ) -> Result<(), String> {
        match report.source() {
            ModelDiscoverySource::Discovered {
                provider: discovered,
            } if discovered == provider => {}
            ModelDiscoverySource::Discovered {
                provider: discovered,
            } => {
                return Err(format!(
                    "discovery report for `{discovered}` does not belong in provider cache `{provider}`"
                ));
            }
            ModelDiscoverySource::Supplied => {
                return Err(format!(
                    "a supplied report does not belong in provider cache `{provider}`"
                ));
            }
        }
        for entry in report.models() {
            match &entry.provenance {
                ModelFactProvenance::Discovered {
                    provider: discovered,
                } if discovered == provider && entry.slug.provider == provider => {}
                _ => {
                    return Err(format!(
                        "discovery row `{}` does not belong in provider cache `{provider}`",
                        entry.slug
                    ));
                }
            }
        }
        let baseline = self.providers.get(provider).cloned().unwrap_or_default();
        let merged = baseline.merge(report);
        merged.validate(provider)?;
        self.providers.insert(provider.to_string(), merged);
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.v != 1 {
            return Err(format!("unsupported discovery cache version {}", self.v));
        }
        for (provider, slice) in &self.providers {
            if provider.is_empty() {
                return Err("a cached provider id cannot be empty".into());
            }
            slice.validate(provider)?;
        }
        Ok(())
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
    /// Authentication exists, but the provider's required wire mapping does
    /// not. Kept distinct so callers cannot mistake login success for a
    /// license to substitute a superficially similar protocol.
    ProviderMappingUnavailable { provider: String, mapping: String },
    /// The mapping exists and works; the evidence that it works *against the
    /// provider* does not.
    ///
    /// Deliberately not [`ProviderMappingUnavailable`], which renders "mapping
    /// is not implemented". For a row waiting on conformance check five that
    /// sentence is false in the one direction that matters: it sends an
    /// operator to look for missing code, when what is missing is a sanitized
    /// live recording only their own key can produce
    /// (`docs/decisions/2026-09-03-public-provider-wires-require-live-promotion.md`).
    ///
    /// [`ProviderMappingUnavailable`]: ResolveError::ProviderMappingUnavailable
    LivePromotionPending { provider: String, mapping: String },
    /// A hosted model with no supplied **capabilities**.
    ///
    /// Narrowed from "no pricing or capabilities" by
    /// `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §3: pricing is no
    /// longer required, and "cannot be bounded" is no longer the reason,
    /// because a run that asked for no ceiling does not need bounding.
    HostedModelNotConfigured { slug: String },
    /// A slug the provider has switched off.
    ///
    /// Distinct from [`HostedModelNotConfigured`] because the two send an
    /// operator to opposite places. "No capabilities are configured" means
    /// *supply them*, and doing that for a retired model produces a file that
    /// will never work: the endpoint is gone, not unstated. This says so, and
    /// names the replacement the vendor recommends.
    ///
    /// Every field is data the caller supplies. This crate holds no list of
    /// retired models and should not — the tombstones live beside the
    /// catalogue that has a revision to name (`ego-cli`'s `facts`), and a
    /// second copy here would be a second thing to keep current.
    ///
    /// [`HostedModelNotConfigured`]: Self::HostedModelNotConfigured
    ModelRetired {
        slug: String,
        replacement: String,
        retired: String,
        /// Where an operator may state the model anyway, in override order.
        overrides: Vec<String>,
        /// The catalogue revision that knows it as retired.
        revision: String,
    },
    /// The provider needs a credential that is not set.
    MissingCredential(MissingCredential),
    /// The provider received a credential but rejected its account authority.
    RejectedCredential { provider: String, status: u16 },
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
    /// A control-plane endpoint could not be reached, or answered without
    /// the fact resolution went there for.
    ///
    /// Named for the endpoint rather than for a daemon, because the two
    /// resolvers that produce it are asking different kinds of server the
    /// same kind of question: Ollama's `/api/tags` is a daemon on loopback,
    /// and xAI's `/language-models` is a hosted price table. Both refusals
    /// have to name a URL an operator can go and try, which is the whole
    /// content of the variant.
    EndpointUnreachable { endpoint: String, message: String },
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
            ResolveError::ProviderMappingUnavailable { provider, mapping } => write!(
                f,
                "provider `{provider}` is authenticated but cannot generate in this build: mapping `{mapping}` is not implemented"
            ),
            ResolveError::LivePromotionPending { provider, mapping } => write!(
                f,
                "`{provider}` is not selectable yet: its `{mapping}` mapping has never been \
                 recorded against a live model, and conformance check five needs an \
                 operator's own key to make. Run `EGO_SMOKE={provider} EGO_SMOKE_RECORD=1 \
                 cargo test -p ego-cli --test smoke -- --ignored` with that key set to \
                 establish it"
            ),
            ResolveError::HostedModelNotConfigured { slug } => write!(
                f,
                "no capabilities are configured for `{slug}`: a hosted provider \
                 publishes no capability endpoint, so its context window and \
                 output ceiling have to be supplied"
            ),
            ResolveError::ModelRetired {
                slug,
                replacement,
                retired,
                overrides,
                revision,
            } => write!(
                f,
                "`{slug}` was retired by its provider on {retired}; it recommends \
                 `{replacement}`. Catalogue revision {revision}. To use it anyway, \
                 state its capabilities in {}",
                overrides.join(" or ")
            ),
            ResolveError::MissingCredential(missing) => write!(f, "{missing}"),
            ResolveError::RejectedCredential { provider, status } => write!(
                f,
                "provider `{provider}` rejected its credential with HTTP {status}; run `ego auth login {provider}`"
            ),
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
            ResolveError::EndpointUnreachable { endpoint, message } => {
                write!(
                    f,
                    "{endpoint} did not answer with what was needed: {message}"
                )
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

/// A row metered in currency and settled from whatever table priced it.
///
/// The ordinary hosted case. It takes no quote: under
/// `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §3 the rates travel
/// beside the metering rather than inside it, so "this row is billed in
/// money" and "we hold rates for it" stay two separate assertions. A row with
/// no rates is still this — the provider still charges — and settles as
/// `UnknownCost::NoPricingQuote` rather than as a fabricated zero.
pub fn tabulated() -> Metering {
    Metering::Currency {
        settlement: SettlementSource::Quote,
    }
}

/// The public, price-free class of a resolved metering policy.
pub(crate) fn metering_kind(metering: &Metering) -> MeteringKind {
    match metering {
        Metering::Currency { .. } => MeteringKind::Currency,
        Metering::Subscription { .. } => MeteringKind::Subscription,
        Metering::Unmetered { .. } => MeteringKind::Unmetered,
    }
}

/// Rates an operator wrote down, the first of §5's three sources.
///
/// Named rather than constructed inline so the provenance and the quote are
/// minted together: a `ResolvedRates` whose `source` disagreed with where its
/// quote actually came from would make `MeteringSelected` publish a false
/// statement about the strength of a guarantee.
pub fn operator_rates(quote: PricingQuote) -> ResolvedRates {
    ResolvedRates {
        quote,
        source: RateSource::Operator,
    }
}

/// Rates read out of the cached third-party table, the last of §5's three.
///
/// Beside [`operator_rates`] and minted the same way, for the same reason: the
/// provenance and the figures are built together so they cannot describe two
/// different sources.
///
/// This one has a second thing that must not drift.
/// [`RateSource::Table`](ego_core::cost::RateSource::Table) carries the table
/// revision — §10 puts it there so an operator can see *which* table their
/// ceiling rests on — and the quote carries one too. They are taken from the
/// quote rather than passed in separately, exactly as
/// [`provider_reported`] takes its revision from the decoder: a signature that
/// accepted both would be a signature that admits a caller pairing one table's
/// prices with another table's name.
pub fn table_rates(quote: PricingQuote) -> ResolvedRates {
    ResolvedRates {
        source: RateSource::Table {
            revision: quote.revision.clone(),
        },
        quote,
    }
}

/// A row priced from a supplied table, and settled from the provider's own
/// statement of what it will charge.
///
/// The best accounting available: the provider's own figure settles what the
/// generation cost, replacing anything a table would have computed.
///
/// Takes no quote, for the reason [`tabulated`] does not — and the change is
/// a real one rather than a rename. A provider-reported row **with no rates**
/// is now legal and useful: with no ceiling configured it runs unbounded and
/// still settles exactly, from the figure that will appear on the invoice.
/// What it cannot do is bound a ceiling, and that requirement now lives at
/// `BudgetGate::reserve`, which refuses a `Limited` ledger with no rates
/// whatever the settlement source is.
///
/// The [`ProviderCostRevision`](ego_core::cost::ProviderCostRevision) is
/// taken from `cost` rather than written out here, so the declaration
/// recorded at resolution and the attribution stamped on the settled figure
/// are one value and cannot drift apart.
pub fn provider_reported(cost: &dyn ProviderCost) -> Metering {
    Metering::Currency {
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

/// The refusal a row owes when it has not cleared the live-promotion gate.
///
/// One function with three call sites — [`Catalogue::resolve`],
/// [`Catalogue::static_request_profile`], and the CLI's listing projection —
/// because "may this row be offered" must have exactly one answer everywhere
/// a model is offered. `2026-09-03-public-provider-wires-require-live-promotion.md`
/// assigns promotion to the catalogue ("The shared catalogue owns promotion
/// and selectability from recorded evidence"), and a gate applied only where a
/// request is finally built would let `ego models` list the row as available
/// and a picker offer it, so the refusal would arrive after the choice instead
/// of instead of it.
///
/// It runs **before any credential is read**, on both paths. A row that cannot
/// be selected at all should not cause a keychain prompt or a
/// `credential_command` to run, and "missing key" must not be reported for a
/// row whose key would change nothing.
pub fn live_promotion_refusal(row: &ProviderRow) -> Option<ResolveError> {
    row.pending_live_evidence()
        .map(|mapping| ResolveError::LivePromotionPending {
            provider: row.id().to_string(),
            mapping: mapping.to_string(),
        })
}

impl Catalogue {
    /// A catalogue with rows and no hosted models, so every hosted slug
    /// refuses until one is supplied.
    pub fn new(rows: impl IntoIterator<Item = ProviderRow>) -> Self {
        Catalogue {
            rows: rows
                .into_iter()
                .map(|row| (row.id().to_owned(), row))
                .collect(),
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

    /// Project only request model, capabilities and row-selected wire facts
    /// when the resolver says all three are operator-supplied.
    ///
    /// `None` is an explicit deferral to full resolution; this method never
    /// opens a transport, resolves a credential or discovers a local model.
    pub fn static_request_profile(
        &self,
        slug: &ModelSlug,
    ) -> Result<Option<StaticRequestProfile>, ResolveError> {
        let row = self.row(&slug.provider)?;
        if let Some(refusal) = live_promotion_refusal(row) {
            return Err(refusal);
        }
        row.resolver.supplied_request_mapping(&StaticResolution {
            catalogue: self,
            slug,
        })
    }

    pub(crate) fn static_request_profile_with_mapping(
        &self,
        slug: &ModelSlug,
        wire: &'static dyn WireFormat,
    ) -> Result<StaticRequestProfile, ResolveError> {
        let key = slug.to_string();
        let hosted = self
            .hosted
            .get(&key)
            .ok_or(ResolveError::HostedModelNotConfigured { slug: key })?;
        Ok(StaticRequestProfile::new(
            slug.model.clone(),
            hosted.capabilities.clone(),
            wire,
        ))
    }

    /// Resolve any slug, by asking whatever its row says resolves it.
    ///
    /// The one entry point, and the reason there is one:
    /// `crates/ego-cli/src/model.rs` used to ask
    /// `if slug.provider == catalogue::OLLAMA` and call one of two functions,
    /// which is a caller that must know which providers are daemons in order
    /// to resolve any of them. The row knows
    /// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §4), so the branch is gone and the second daemon costs a constructor
    /// rather than an arm in another crate.
    ///
    /// The transport and the credential are taken by every call, including
    /// the ones that use neither. Making either optional would put the branch
    /// back — the caller would decide whether this resolution is the kind that
    /// needs a network or a key, which is the thing it must not know.
    ///
    /// The credential is the one this run already resolved, passed rather than
    /// read again: a resolver whose endpoint authenticates (xAI publishes
    /// prices per key) must send the same credential the generation will, and
    /// a second read of the environment could disagree with the first
    /// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §6).
    pub async fn resolve(
        &self,
        slug: &ModelSlug,
        http: &Arc<dyn Http>,
        credential: &ResolvedCredential,
    ) -> Result<ResolvedModel, ResolveError> {
        let row = self.row(&slug.provider)?;
        if let Some(refusal) = live_promotion_refusal(row) {
            return Err(refusal);
        }
        let at = Resolution {
            catalogue: self,
            row,
            slug,
            http,
            credential,
        };
        row.resolver.resolve(&at).await
    }

    /// Resolve a hosted slug from what was supplied.
    ///
    /// Local slugs are not resolvable here: their facts come from the daemon,
    /// which needs a transport this type deliberately does not hold. Public
    /// because [`Supplied`](crate::resolver::Supplied) is what calls it and
    /// because a test that wants the supplied half alone should not have to
    /// build a transport to reach it.
    pub fn resolve_hosted(
        &self,
        slug: &ModelSlug,
        wire: &'static dyn WireFormat,
    ) -> Result<ResolvedModel, ResolveError> {
        let row = self.row(&slug.provider)?;
        let key = slug.to_string();
        let hosted = self
            .hosted
            .get(&key)
            .ok_or(ResolveError::HostedModelNotConfigured { slug: key })?;
        Ok(ResolvedModel {
            model: slug.model.clone(),
            row: row.clone(),
            wire,
            capabilities: hosted.capabilities.clone(),
            metering: hosted.metering.clone(),
            rates: hosted.rates.clone(),
        })
    }

    /// Current supplied rows owned by `provider`, without rates or credentials.
    pub(crate) fn enumerate_hosted(
        &self,
        provider: &str,
        metering: Option<MeteringKind>,
    ) -> Result<ModelDiscoveryReport, String> {
        let prefix = format!("{provider}/");
        let models = self
            .hosted
            .iter()
            .filter(|(slug, _)| slug.starts_with(&prefix))
            .map(|(slug, hosted)| {
                let slug = ModelSlug::parse(slug).map_err(|error| error.to_string())?;
                Ok(ModelDiscoveryEntry {
                    slug,
                    capabilities: hosted.capabilities.clone(),
                    metering: metering.unwrap_or_else(|| metering_kind(&hosted.metering)),
                    provenance: ModelFactProvenance::supplied(hosted.authority),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        ModelDiscoveryReport::try_new(ModelDiscoverySource::Supplied, models, Vec::new(), true)
    }
}

/// The endpoint a local daemon's control plane lives on, derived from the
/// base the row already carries.
///
/// Derived rather than configured twice: two URLs that must agree about which
/// daemon they mean is two chances to point at different ones.
///
/// **Not general, and the signature flatters it.** `set_path` is absolute, so
/// this discards any prefix the base carries — correct for Ollama, whose
/// control plane really does live at the host's root (`/api/tags`, `/api/ps`)
/// while its openai-compatible surface lives under `/v1/`, and wrong for any
/// provider served behind a path prefix. Ollama is the only caller and the
/// only daemon with a control plane at all; a second one is the point at which
/// this stops being a function and becomes the daemon resolver's business
/// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §2
/// names this as the one casualty of the base/path split).
pub fn control_plane(row: &ProviderRow, path: &str) -> Result<Url, ResolveError> {
    let mut url = row.base().clone();
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
