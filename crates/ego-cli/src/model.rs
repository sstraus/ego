//! Turning `--model P/M` into something a turn can be run against.
//!
//! The split is `ego-ai`'s and this module does not reopen it: a hosted
//! model's price and ceilings are *supplied*, a local daemon's are
//! *discovered*, and which of the two a provider gets is a fact its row
//! carries (`ego_ai::resolver`). What lives here is only the part that needs
//! the process — reading the environment for a credential and for a daemon
//! address.

use std::sync::{Arc, Mutex, RwLock};

use ego_ai::catalogue::{Catalogue, ModelSlug, ResolveError, ResolvedModel};
use ego_ai::generator::StaticRequestProfile;
use ego_ai::http::Http;
use ego_ai::provider::{
    ANTHROPIC_BASE, Credentials, OLLAMA_BASE, OPENAI_BASE, ProviderRow, ResolvedCredential,
};
use ego_core::ceiling::Ceilings;
use ego_core::config::{AuthProviderLayer, ModelRef};
use ego_core::generation_model_source::{
    GenerationModelError, GenerationModelErrorClass, GenerationModelRequest, GenerationModelSource,
    GenerationProfile, GenerationRole,
};
use ego_core::generator::port::Generator;
use ego_core::generator::types::ThinkingLevel;
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;
use url::Url;

use crate::auth::overlay_headers;
use crate::facts;

/// The environment variable Ollama's own client reads to find the daemon.
///
/// Honoured rather than reinvented under an `EGO_` name: a machine that has
/// moved its daemon has already set this, and a second variable meaning the
/// same thing is a second thing to get wrong. It is also what lets a test
/// point the binary at a loopback stub without a network.
pub const OLLAMA_HOST: &str = "OLLAMA_HOST";

/// The environment variable every openai-compatible client reads to be pointed
/// somewhere other than `api.openai.com`.
///
/// Same argument as [`OLLAMA_HOST`]: Azure deployments and the compatible
/// gateways serve this wire format at their own hostnames, and the machine
/// that uses one has already set this. It is also how a test reaches a
/// loopback stub without a network.
pub const OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
/// Override the admitted OpenRouter API base, including its version path.
pub const OPENROUTER_BASE_URL: &str = "OPENROUTER_BASE_URL";

/// The variable every Anthropic client reads to be pointed somewhere other
/// than `api.anthropic.com`.
///
/// It exists for the reason the other two do, and it became *necessary* on
/// 2026-09-07. Until then `openai` was the only hosted row a test could point
/// at a stub, so it carried every offline assertion about hosted behaviour;
/// `2026-09-03-public-provider-wires-require-live-promotion.md` moved that row
/// to Responses and made it unselectable pending live evidence.
///
/// `anthropic` and not `openrouter` is the row that inherits those tests,
/// because the two settle identically: both declare
/// [`CostExpectation::Positive`] with no `ProviderCost` decoder, so both are
/// priced from a supplied rate table. OpenRouter reports its own monetary
/// total, so moving a budget assertion there would change what it proves
/// rather than where it runs.
pub const ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";

/// The API base an address variable names, or the default.
///
/// `OLLAMA_HOST` is conventionally a bare `host:port` and `OPENAI_BASE_URL`
/// and `ANTHROPIC_BASE_URL` are conventionally base URLs without the path; all
/// spellings are accepted for all three, and anything else is refused rather
/// than half-parsed into a request to somewhere unintended. The path is always
/// set, so a variable that already carries one and a variable that does not
/// resolve to the same address.
///
/// It sets `/v1/` and not `/v1/chat/completions`: a row carries a base and the
/// mapping joins its own path onto it
/// (`ARCHITECTURE.md`
/// §2). Both variables name a *server*, which is what a base is, so this is
/// also the spelling an operator already has in them.
fn base(var: &str, fallback: &str) -> Result<Url, ResolveError> {
    let Ok(given) = std::env::var(var) else {
        return Ok(Url::parse(fallback).expect("a constant this file controls"));
    };
    let given = given.trim();
    let base = if given.contains("://") {
        given.to_string()
    } else {
        format!("http://{given}")
    };
    let mut url = Url::parse(&base).map_err(|err| ResolveError::EndpointUnreachable {
        endpoint: given.to_string(),
        message: format!("{var} is not an address: {err}"),
    })?;
    url.set_path("/v1/");
    Ok(url)
}

/// The rows this build knows how to reach.
///
/// Every row, unconditionally. A row names the variable its credential lives
/// in and never reads it
/// (`ARCHITECTURE.md` §3), so a
/// machine with no `OPENAI_API_KEY` still *has* an OpenAI row — it simply
/// cannot authenticate against it yet, which is a different sentence and a
/// different error.
///
/// It used to be conditional, because a row could not be built without a key.
/// That made a missing credential indistinguishable from an unknown provider
/// and forced [`resolve`] to name one provider by hand to say the true thing.
///
/// The API-key `xai` candidate remains absent. The distinct `xai-oauth` row
/// is present because the Pi ∪ Hermes inclusion decision explicitly promotes
/// that subscription contract without collapsing its metering into the API
/// key row. Gemini used to be absent for want of acceptance evidence and is
/// now present:
/// `ARCHITECTURE.md`.
pub(crate) fn rows(
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
) -> Result<Vec<ProviderRow>, ResolveError> {
    let mut rows = mapped_rows(
        ProviderRow::ollama(base(OLLAMA_HOST, OLLAMA_BASE)?),
        ProviderRow::openai_at(base(OPENAI_BASE_URL, OPENAI_BASE)?),
        ProviderRow::anthropic(base(ANTHROPIC_BASE_URL, ANTHROPIC_BASE)?),
    );
    for row in &mut rows {
        if row.id() == "openrouter" {
            let address = std::env::var(OPENROUTER_BASE_URL)
                .unwrap_or_else(|_| ego_ai::provider::OPENROUTER_BASE.to_owned());
            let url = Url::parse(&address).map_err(|error| ResolveError::EndpointUnreachable {
                endpoint: "OpenRouter API base".to_owned(),
                message: error.to_string(),
            })?;
            *row = ProviderRow::openrouter(url);
        }
        if let Some(config) = auth.get(row.id()) {
            let overlay = config
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            row.extra_headers = overlay_headers(&row.extra_headers, &overlay).map_err(|err| {
                ResolveError::EndpointUnreachable {
                    endpoint: format!("auth.{}.headers", row.id()),
                    message: err.to_string(),
                }
            })?;
        }
    }
    Ok(rows)
}

fn mapped_rows(
    ollama: ProviderRow,
    openai: ProviderRow,
    anthropic: ProviderRow,
) -> Vec<ProviderRow> {
    ego_ai::conformance::final_provider_rows(ollama, openai, anthropic).into()
}

/// Assemble the one resolver-selected catalogue from current rows and facts.
pub(crate) fn catalogue(
    hosted: Vec<(String, facts::Hosted)>,
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
) -> Result<Catalogue, ResolveError> {
    Ok(catalogue_from_rows(hosted, rows(auth)?))
}

fn catalogue_from_rows(hosted: Vec<(String, facts::Hosted)>, rows: Vec<ProviderRow>) -> Catalogue {
    let mut catalogue = Catalogue::new(rows);
    for (slug, model) in hosted {
        catalogue = catalogue.with_hosted(&slug, model);
    }
    catalogue
}

/// Project only the request-shape facts already supplied for this model.
///
/// This path resolves no credential and owns no transport. An empty ceiling
/// prevents the supplied-facts decoder from attaching rates to the temporary
/// catalogue value; the returned profile cannot carry metering or pricing.
pub(crate) fn supplied_request_profile(
    identity: &ModelRef,
) -> Result<Option<StaticRequestProfile>, GenerationModelError> {
    let hosted = facts::supplied(&Ceilings::default()).map_err(|error| {
        GenerationModelError::new(GenerationModelErrorClass::Usage, error.to_string())
    })?;
    let slug = ModelSlug::parse(&identity.0).map_err(generation_error)?;
    let catalogue = catalogue_from_rows(
        hosted,
        mapped_rows(
            ProviderRow::ollama_default(),
            ProviderRow::openai_at(
                Url::parse(OPENAI_BASE).expect("the built-in OpenAI base is a URL"),
            ),
            ProviderRow::anthropic_default(),
        ),
    );
    catalogue
        .static_request_profile(&slug)
        .map_err(generation_error)
}

/// Explain the conservative OpenRouter default without weakening explicit requests.
pub(crate) fn reasoning_default_notice(
    identity: &ModelRef,
    capabilities: &ego_core::generator::capabilities::ModelCapabilities,
    requested: Option<ThinkingLevel>,
) -> Option<&'static str> {
    use ego_core::generator::capabilities::ReasoningEffortSupport;
    (identity.0.starts_with("openrouter/")
        && requested.is_none()
        && matches!(&capabilities.reasoning_efforts, ReasoningEffortSupport::Only(levels)
            if levels.len() == 1 && levels.contains(&ThinkingLevel::Off)))
    .then_some(
        "This OpenRouter model has no stated non-off reasoning support; using reasoning off.",
    )
}

/// Use the least effort the resolved judge explicitly supports.
/// Startup and generation must validate the same effort policy.
pub(crate) fn judge_thinking(
    capabilities: &ego_core::generator::capabilities::ModelCapabilities,
) -> Result<ThinkingLevel, ego_core::generator::capabilities::ReasoningSelectionError> {
    use ego_core::generator::capabilities::ReasoningEffortSupport;
    let effort = match &capabilities.reasoning_efforts {
        ReasoningEffortSupport::Only(levels) => levels.first().copied(),
        ReasoningEffortSupport::Any | ReasoningEffortSupport::Unknown => None,
    }
    .unwrap_or(ThinkingLevel::Off);
    capabilities.resolve_reasoning_effort(Some(effort))
}

/// Resolve `slug` against the daemon or against what was supplied.
///
/// `hosted` comes from [`facts::supplied`] and is passed in rather than read
/// here, so that a `models.toml` which cannot be parsed is reported as the file
/// error it is. Folding it into [`ResolveError`] would make "your TOML has a
/// stray bracket" arrive as a statement about a model.
///
/// `creds` is passed in for the same reason and one more: this is the *only*
/// place a credential is read. What comes back beside the model is the
/// resolved value itself, so the check here and the header sent later cannot
/// disagree about whether a key exists — they are the same read
/// (`ARCHITECTURE.md`
/// §6).
///
/// Returned beside the [`ResolvedModel`] rather than inside it, and that is
/// the point of the pair: a resolved model is `Clone` and `Debug` and travels
/// to the budget gate and the ledger, and a credential must travel to exactly
/// one place.
pub async fn resolve(
    slug: &str,
    hosted: Vec<(String, facts::Hosted)>,
    http: &Arc<dyn Http>,
    creds: &dyn Credentials,
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
) -> Result<(ResolvedModel, ResolvedCredential), ResolveError> {
    let slug = ModelSlug::parse(slug)?;
    let catalogue = catalogue(hosted, auth)?;
    let row = catalogue.row(&slug.provider)?;
    // Before the credential, deliberately: a row nobody may select should not
    // make a keychain prompt or a `credential_command` run, and "no key" is
    // the wrong thing to tell somebody whose key would change nothing.
    if let Some(refusal) = ego_ai::catalogue::live_promotion_refusal(row) {
        return Err(refusal);
    }
    // Read here, where a refusal can name the variable to set, rather than as
    // a 401 several layers later. No per-provider branch: the row names its
    // own variable, so this one line covers every provider that has one and
    // says nothing about the ones that do not.
    let credential = row.credential.resolve(creds)?;
    // No branch on the provider id. The row names what resolves it
    // (`ego_ai::resolver`), so this crate does not have to know which
    // providers are daemons in order to resolve any of them.
    let model = catalogue
        .resolve(&slug, http, &credential)
        .await
        .map_err(retirement_instead)?;
    Ok((model, credential))
}

/// Replace "no capabilities are configured" with the retirement, if there is
/// one.
///
/// Only that one error is reconsidered. Every other refusal already knows more
/// than a tombstone could add — a missing credential is missing whether or not
/// the model still exists, and an unreachable endpoint is not a fact about the
/// slug. Widening this to every error would let a stale tombstone mask a live
/// failure, which is worse than the unhelpful message it replaces.
///
/// It runs *after* resolution rather than before it, and that ordering is the
/// point: a slug that resolves is one an operator has supplied capabilities
/// for in their own file, and the builtin catalogue does not get to overrule
/// them about a model they can evidently still reach.
fn retirement_instead(error: ResolveError) -> ResolveError {
    let ResolveError::HostedModelNotConfigured { slug } = &error else {
        return error;
    };
    facts::retirement(slug).unwrap_or(error)
}

#[derive(Debug, Clone)]
struct MainSelection {
    identity: ModelRef,
    thinking: ThinkingLevel,
    explicit_thinking: Option<ThinkingLevel>,
}

/// Read-only view of the model authority currently installed for main turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationSelectionSnapshot {
    pub identity: ModelRef,
    pub thinking: ThinkingLevel,
    pub explicit_thinking: Option<ThinkingLevel>,
}

/// Mutable session selection observed afresh at every main generation boundary.
#[derive(Debug)]
pub struct GenerationSelection {
    current: RwLock<Option<MainSelection>>,
}

impl GenerationSelection {
    pub fn new(identity: ModelRef, thinking: ThinkingLevel) -> Self {
        Self {
            current: RwLock::new(Some(MainSelection {
                identity,
                thinking,
                explicit_thinking: Some(thinking),
            })),
        }
    }

    fn unresolved() -> Self {
        Self {
            current: RwLock::new(None),
        }
    }

    /// Install the selection committed by the interactive picker.
    pub fn install(&self, identity: ModelRef, thinking: ThinkingLevel) -> Result<(), String> {
        let mut current = self
            .current
            .write()
            .map_err(|_| "the model selection lock is poisoned".to_string())?;
        *current = Some(MainSelection {
            identity,
            thinking,
            explicit_thinking: Some(thinking),
        });
        Ok(())
    }

    /// Install the configured main selection once, after its model facts have
    /// resolved. A concurrent human choice always wins the race.
    fn initialize(
        &self,
        identity: ModelRef,
        thinking: ThinkingLevel,
        explicit_thinking: Option<ThinkingLevel>,
    ) -> Result<(), GenerationModelError> {
        let mut current = self.current.write().map_err(|_| selection_lock_error())?;
        if current.is_none() {
            *current = Some(MainSelection {
                identity,
                thinking,
                explicit_thinking,
            });
        }
        Ok(())
    }

    /// Observe the same mutable authority read at every generation boundary.
    pub fn snapshot(&self) -> Result<GenerationSelectionSnapshot, GenerationModelError> {
        let current = self.read()?.ok_or_else(unresolved_selection_error)?;
        Ok(GenerationSelectionSnapshot {
            identity: current.identity,
            thinking: current.thinking,
            explicit_thinking: current.explicit_thinking,
        })
    }

    fn read(&self) -> Result<Option<MainSelection>, GenerationModelError> {
        self.current
            .read()
            .map(|selection| selection.clone())
            .map_err(|_| selection_lock_error())
    }
}

fn selection_lock_error() -> GenerationModelError {
    GenerationModelError::new(
        GenerationModelErrorClass::Internal,
        "the model selection lock is poisoned",
    )
}

fn unresolved_selection_error() -> GenerationModelError {
    GenerationModelError::new(
        GenerationModelErrorClass::Usage,
        "the main model selection has no resolved thinking level; pass --thinking \
         off|minimal|low|medium|high|xhigh|max",
    )
}

/// CLI adapter for the core-owned fresh generation-model port.
pub struct CliGenerationModelSource {
    selection: Arc<GenerationSelection>,
    initial_identity: ModelRef,
    initial_thinking: Option<ThinkingLevel>,
    output_limit: Option<std::num::NonZeroU64>,
    offline: bool,
    judge: Option<ModelRef>,
    ceilings: Ceilings,
    auth: std::collections::HashMap<String, AuthProviderLayer>,
    credential_commands: Arc<dyn crate::auth::CredentialCommandPort>,
}

impl CliGenerationModelSource {
    pub fn new(
        identity: ModelRef,
        thinking: Option<ThinkingLevel>,
        judge: Option<ModelRef>,
        ceilings: Ceilings,
        auth: std::collections::HashMap<String, AuthProviderLayer>,
    ) -> Self {
        Self {
            selection: Arc::new(GenerationSelection::unresolved()),
            initial_identity: identity,
            initial_thinking: thinking,
            output_limit: None,
            offline: false,
            judge,
            ceilings,
            auth,
            credential_commands: Arc::new(crate::auth::OsCredentialCommand),
        }
    }

    pub fn with_output_limit(mut self, limit: Option<std::num::NonZeroU64>) -> Self {
        self.output_limit = limit;
        self
    }

    pub fn with_offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    pub fn selection(&self) -> Arc<GenerationSelection> {
        Arc::clone(&self.selection)
    }

    /// Bind durable model configuration to one actual effect scope.
    pub fn bind(
        self: &Arc<Self>,
        http: Arc<dyn Http>,
        cancel: CancelToken,
        run_deadline: Option<std::time::Instant>,
    ) -> Arc<dyn GenerationModelSource> {
        Arc::new(BoundGenerationModelSource {
            source: Arc::clone(self),
            http,
            cancel,
            run_deadline,
            generator: Mutex::new(None),
        })
    }

    async fn resolve_role(
        &self,
        request: GenerationModelRequest,
        http: &Arc<dyn Http>,
        cancel: &CancelToken,
        run_deadline: Option<std::time::Instant>,
        retained: &Mutex<Option<RetainedGenerator>>,
    ) -> Result<GenerationProfile, GenerationModelError> {
        let role = request.role();
        let has_model_overlay = request.model().is_some();
        let has_thinking_overlay = request.thinking().is_some();
        let initialize_main =
            role == GenerationRole::Main && !has_model_overlay && !has_thinking_overlay;
        let current = self.selection.read()?;
        let (mut identity, requested_thinking, resolved_thinking) = match role {
            GenerationRole::Main
            | GenerationRole::Compaction
            | GenerationRole::Subagent
            | GenerationRole::Hook => {
                let identity = request
                    .model()
                    .cloned()
                    .or_else(|| current.as_ref().map(|selection| selection.identity.clone()))
                    .unwrap_or_else(|| self.initial_identity.clone());
                let explicit = request.thinking().or_else(|| {
                    current
                        .as_ref()
                        .and_then(|selection| selection.explicit_thinking)
                        .or(self.initial_thinking)
                });
                let concrete = if !has_model_overlay && !has_thinking_overlay {
                    current.as_ref().map(|selection| selection.thinking)
                } else {
                    None
                };
                (identity, explicit, concrete)
            }
            GenerationRole::GoalCheck | GenerationRole::Judge => {
                let identity = match self
                    .judge
                    .as_ref()
                    .filter(|_| role == GenerationRole::Judge)
                {
                    Some(model) if model.0 == "none" => {
                        return Err(GenerationModelError::new(
                            GenerationModelErrorClass::Usage,
                            "permission judge explicitly disabled",
                        ));
                    }
                    Some(model) => model.clone(),
                    None => current
                        .as_ref()
                        .map(|selection| selection.identity.clone())
                        .unwrap_or_else(|| self.initial_identity.clone()),
                };
                (identity, Some(ThinkingLevel::Off), None)
            }
        };
        let provider_id = identity
            .0
            .split_once('/')
            .map(|(provider, _)| provider)
            .unwrap_or(identity.0.as_str());
        let hosted = facts::supplied(&self.ceilings).map_err(|error| {
            GenerationModelError::new(GenerationModelErrorClass::Usage, error.to_string())
        })?;
        if role == GenerationRole::GoalCheck
            || (role == GenerationRole::Judge && self.judge.is_none())
        {
            let preferred = match provider_id {
                "openai-codex" => Some("openai-codex/codex-auto-review"),
                "openai" => Some("openai/gpt-6-luna"),
                "anthropic" => Some("anthropic/claude-haiku-5-5"),
                _ => None,
            };
            if let Some(preferred) =
                preferred.filter(|preferred| hosted.iter().any(|(slug, _)| slug == preferred))
            {
                identity = ModelRef(preferred.to_owned());
            }
        }
        // Prefer the cheapest fully quoted model on the current provider.
        // Without operator pricing, the provider's small-model default above
        // is the only supported cost signal; never invent prices.
        if role == GenerationRole::GoalCheck {
            let provider = identity
                .0
                .split_once('/')
                .map(|(provider, _)| provider)
                .unwrap_or(&identity.0);
            if let Some((slug, _)) = hosted
                .iter()
                .filter(|(slug, _)| {
                    slug.split_once('/')
                        .is_some_and(|(candidate, _)| candidate == provider)
                })
                .filter_map(|(slug, model)| {
                    let quote = &model.rates.as_ref()?.quote;
                    Some((slug, (quote.output?, quote.uncached_input?)))
                })
                .min_by_key(|(_, prices)| *prices)
            {
                identity = ModelRef(slug.clone());
            }
        }
        let provider_id = identity
            .0
            .split_once('/')
            .map(|(provider, _)| provider)
            .unwrap_or(identity.0.as_str());
        let credentials = crate::auth::request_credentials_scoped(
            provider_id,
            self.auth
                .get(provider_id)
                .and_then(|auth| auth.credential_command.as_deref()),
            http.as_ref(),
            self.credential_commands.as_ref(),
            cancel,
            run_deadline,
        )
        .await
        .map_err(auth_generation_error)?;
        let (mut resolved, credential) =
            resolve(&identity.0, hosted, http, credentials.as_ref(), &self.auth)
                .await
                .map_err(generation_error)?;
        let ledger = self.ceilings.money.map(|limit| {
            crate::budget::gate(&self.ceilings, None, limit.currency)
                .ledger()
                .clone()
        });
        if let Some(ledger) = ledger.filter(|ledger| {
            resolved.metering.ceiling_needs_rates(ledger) && resolved.rates.is_none()
        }) {
            use ego_core::event::time::Clock;
            let home = crate::config::ego_home().ok_or_else(|| {
                GenerationModelError::new(
                    GenerationModelErrorClass::Usage,
                    "a money ceiling needs EGO_HOME or HOME for the price cache",
                )
            })?;
            resolved.rates = crate::budget::TableSource {
                io: &ego_tools::store::OsIo,
                ego_home: &home,
                http,
                offline: self.offline,
                now: ego_tools::clock::SystemClock.now(),
                max_age: ego_tools::pricing::MAX_AGE,
            }
            .rates(&resolved, &ledger)
            .await
            .map_err(|error| {
                GenerationModelError::new(GenerationModelErrorClass::Usage, error.to_string())
            })?;
        }
        if matches!(provider_id, "openai" | "anthropic")
            && let Some(rates) = &mut resolved.rates
        {
            // The published tariff is USD. Never relabel it as a foreign-currency price.
            rates.quote.web_search = (rates.quote.currency.as_str() == "USD")
                .then(|| "0.01".parse().expect("published USD search tariff"));
            rates.quote.revision.0.push_str("+web-search-2026-10-16");
            if let ego_core::cost::RateSource::Table { revision } = &mut rates.source {
                *revision = rates.quote.revision.clone();
            }
        }
        let thinking = if matches!(role, GenerationRole::Judge | GenerationRole::GoalCheck) {
            judge_thinking(&resolved.capabilities)
        } else {
            resolved
                .capabilities
                .resolve_reasoning_effort(resolved_thinking.or(requested_thinking))
        }
        .map_err(|error| {
            GenerationModelError::new(GenerationModelErrorClass::Usage, error.to_string())
        })?;
        if initialize_main {
            self.selection
                .initialize(identity.clone(), thinking, self.initial_thinking)?;
        }
        let request_model = resolved.model.clone();
        let mut retained = retained.lock().map_err(|_| selection_lock_error())?;
        let generator: Arc<dyn Generator> = if role == GenerationRole::Main
            && let Some(previous) = retained.as_ref()
            && previous.model == request_model
            && previous
                .generator
                .matches_resolution(&resolved.row, resolved.wire, &credential)
        {
            previous.generator.clone()
        } else {
            let generator = Arc::new(ego_ai::generator::WireGenerator::new(
                resolved.row,
                resolved.wire,
                Arc::clone(http),
                credential,
            ));
            if role == GenerationRole::Main {
                *retained = Some(RetainedGenerator {
                    model: request_model.clone(),
                    generator: Arc::clone(&generator),
                });
            }
            generator
        };
        Ok(GenerationProfile {
            output_limit: self.output_limit,
            identity,
            request_model,
            thinking,
            generator,
            capabilities: resolved.capabilities,
            metering: resolved.metering,
            rates: resolved.rates,
        })
    }
}

struct RetainedGenerator {
    model: String,
    generator: Arc<ego_ai::generator::WireGenerator>,
}

struct BoundGenerationModelSource {
    source: Arc<CliGenerationModelSource>,
    http: Arc<dyn Http>,
    cancel: CancelToken,
    run_deadline: Option<std::time::Instant>,
    generator: Mutex<Option<RetainedGenerator>>,
}

impl GenerationModelSource for BoundGenerationModelSource {
    fn resolve<'a>(
        &'a self,
        request: GenerationModelRequest,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
        Box::pin(self.source.resolve_role(
            request,
            &self.http,
            &self.cancel,
            self.run_deadline,
            &self.generator,
        ))
    }
}

fn generation_error(error: ResolveError) -> GenerationModelError {
    let class = if is_provider_failure(&error) {
        GenerationModelErrorClass::Provider
    } else {
        GenerationModelErrorClass::Usage
    };
    GenerationModelError::new(class, error.to_string())
}

fn auth_generation_error(error: crate::auth::AuthError) -> GenerationModelError {
    let class = match &error {
        crate::auth::AuthError::Temporary { .. } => GenerationModelErrorClass::Provider,
        crate::auth::AuthError::Io { .. } | crate::auth::AuthError::CorruptStore { .. } => {
            GenerationModelErrorClass::Internal
        }
        crate::auth::AuthError::InvalidConfig(_)
        | crate::auth::AuthError::MissingCredential { .. }
        | crate::auth::AuthError::CredentialCommand { .. }
        | crate::auth::AuthError::ReloginRequired { .. } => GenerationModelErrorClass::Usage,
    };
    GenerationModelError::new(class, error.to_string())
}

/// Whether a resolution failure is the network's fault or the caller's.
///
/// `CLI.md` §4.3 gives them different exit codes on purpose — a script retries
/// a 4 and never retries a 2 — and this is the only place the distinction is
/// drawn. A model that is not installed is a 2 however long the daemon took to
/// say so.
pub fn is_provider_failure(err: &ResolveError) -> bool {
    matches!(err, ResolveError::EndpointUnreachable { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_ai::catalogue::{HostedModel, SuppliedAuthority, subscription};
    use ego_ai::http::{HttpCall, PreparedHttpCall};
    use ego_ai::provider::{Credentials, MissingCredential, Secret};
    use ego_ai::wire::RequestTarget;
    use ego_core::generator::capabilities::{
        CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
        ReasoningEffortSupport, Transport,
    };
    use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
    use ego_core::permit::{OneHopResponse, TransportError, TransportFailure};
    use futures::future::BoxFuture;

    struct CodexCredentials;

    impl Credentials for CodexCredentials {
        fn secret(&self, var: &str) -> Result<Secret, MissingCredential> {
            (var == "oauth:openai-codex")
                .then_some(
                    "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig"
                        .into(),
                )
                .ok_or_else(|| MissingCredential {
                    var: var.to_owned(),
                })
        }
    }

    struct AnthropicCredentials;

    impl Credentials for AnthropicCredentials {
        fn secret(&self, var: &str) -> Result<Secret, MissingCredential> {
            (var == ego_ai::provider::ANTHROPIC_KEY_VAR)
                .then(|| "sk-ant-test".into())
                .ok_or_else(|| MissingCredential {
                    var: var.to_owned(),
                })
        }
    }

    struct NoHttp;

    impl ego_ai::http::Http for NoHttp {
        fn begin_call(&self) -> Box<dyn HttpCall> {
            Box::new(NoHttpCall)
        }
    }

    struct NoHttpCall;

    impl HttpCall for NoHttpCall {
        fn execute(
            &mut self,
            _call: PreparedHttpCall,
        ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
            Box::pin(async {
                Err(TransportError::definitely_unsent(
                    TransportFailure::Exchange(
                        "the subscription resolver must not ask HTTP".into(),
                    ),
                ))
            })
        }
    }

    fn codex_hosted() -> HostedModel {
        HostedModel {
            capabilities: ModelCapabilities {
                schema_version: ModelCapabilities::SCHEMA_VERSION,
                max_context_tokens: Some(200_000),
                max_output_tokens: Some(64_000),
                max_tools: Some(128),
                supports_parallel_tool_calls: true,
                reasoning_carryover: ReasoningCarryover::Portable,
                reasoning_efforts: ReasoningEffortSupport::Any,
                default_reasoning_effort: None,
                reasoning_state_dialect: None,
                cancel: CancelSemantics::Cooperative,
                cache: CacheRetention::Short,
                transport: Transport::Sse,
                image_input: ImageInputSupport::Unknown,
            },
            authority: SuppliedAuthority::System,
            metering: subscription("ego/test/codex-subscription/v1"),
            rates: None,
        }
    }

    // Catches: the conservative discovered default becomes silent or overrides explicit effort.
    #[test]
    fn openrouter_missing_reasoning_notice_preserves_explicit_requests() {
        let mut capabilities = codex_hosted().capabilities;
        capabilities.reasoning_efforts = ReasoningEffortSupport::Only([ThinkingLevel::Off].into());
        capabilities.default_reasoning_effort = Some(ThinkingLevel::Off);
        let identity = ModelRef("openrouter/fixture/model".into());
        assert!(
            reasoning_default_notice(&identity, &capabilities, None)
                .unwrap()
                .contains("reasoning off")
        );
        assert!(
            reasoning_default_notice(&identity, &capabilities, Some(ThinkingLevel::Low)).is_none()
        );
        assert!(
            capabilities
                .resolve_reasoning_effort(Some(ThinkingLevel::Low))
                .is_err()
        );
        assert!(
            reasoning_default_notice(&ModelRef("openai/model".into()), &capabilities, None)
                .is_none()
        );
    }

    // Catches: a listed chat-latest alias refuses the measured Medium effort at admission.
    #[test]
    fn chat_latest_admission_accepts_measured_effort_without_guessing_other_levels() {
        let profile = supplied_request_profile(&ModelRef("openai/chat-latest".into()))
            .unwrap()
            .expect("the listed alias has a request profile");
        let capabilities = profile.capabilities();
        assert_eq!(
            capabilities.resolve_reasoning_effort(Some(ThinkingLevel::Medium)),
            Ok(ThinkingLevel::Medium)
        );
        for effort in [ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High] {
            assert!(capabilities.resolve_reasoning_effort(Some(effort)).is_err());
        }
        let mut unknown = capabilities.clone();
        unknown.reasoning_efforts = ReasoningEffortSupport::Unknown;
        assert!(
            unknown
                .resolve_reasoning_effort(Some(ThinkingLevel::Medium))
                .is_err()
        );
    }

    fn selectable() -> Vec<String> {
        rows(&Default::default())
            .expect("the built-in bases parse")
            .into_iter()
            .map(|row| row.id().to_owned())
            .collect()
    }

    #[test]
    fn public_generation_selection_constructors_record_explicit_thinking() {
        let selection =
            GenerationSelection::new(ModelRef("ollama/qwen3".to_owned()), ThinkingLevel::Medium);
        let snapshot = selection
            .snapshot()
            .expect("explicit selection is readable");
        assert_eq!(snapshot.identity, ModelRef("ollama/qwen3".to_owned()));
        assert_eq!(snapshot.thinking, ThinkingLevel::Medium);
        assert_eq!(snapshot.explicit_thinking, Some(ThinkingLevel::Medium));

        selection
            .install(ModelRef("openai/gpt-5.6".to_owned()), ThinkingLevel::High)
            .expect("explicit installation succeeds");
        let snapshot = selection
            .snapshot()
            .expect("installed selection is readable");
        assert_eq!(snapshot.identity, ModelRef("openai/gpt-5.6".to_owned()));
        assert_eq!(snapshot.thinking, ThinkingLevel::High);
        assert_eq!(snapshot.explicit_thinking, Some(ThinkingLevel::High));
    }

    #[test]
    fn source_with_no_initial_choice_stays_typed_unresolved_until_initialized() {
        let source = CliGenerationModelSource::new(
            ModelRef("ollama/qwen3".to_owned()),
            None,
            None,
            Ceilings::default(),
            std::collections::HashMap::new(),
        );
        let error = source
            .selection()
            .snapshot()
            .expect_err("a source without an initial effort is unresolved");
        assert!(error.to_string().contains("thinking"));
    }

    // Catches: fresh model resolution discards the previous tool response and socket.
    #[tokio::test]
    async fn codex_generation_boundaries_preserve_incremental_tool_continuation() {
        use ego_ai::http::HttpWebSocket;
        use ego_core::budget::BudgetGate;
        use ego_core::cost::CurrencyCode;
        use ego_core::event::id::{GenerationId, SessionId};
        use ego_core::generator::attempt::GenerationAttempt;
        use ego_core::generator::types::{ContentBlock, PromptCache};
        use futures::StreamExt;
        use std::sync::Mutex;

        const CHILD: &str = "EGO_TEST_CODEX_GENERATION_BOUNDARY";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            // Synthetic subscription state; never inspect the developer's credentials.
            std::fs::write(home.path().join("auth.json"), serde_json::json!({
                "openai-codex": {"type":"o_auth", "access":
                    "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig"}
            }).to_string()).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "model::tests::codex_generation_boundaries_preserve_incremental_tool_continuation", "--nocapture"])
                .env(CHILD, "1")
                .env("EGO_HOME", home.path())
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        #[derive(Default)]
        struct RecordingHttp(Arc<Mutex<Vec<serde_json::Value>>>);
        struct Socket {
            requests: Arc<Mutex<Vec<serde_json::Value>>>,
            events: std::collections::VecDeque<serde_json::Value>,
        }
        impl ego_ai::http::Http for RecordingHttp {
            fn begin_call(&self) -> Box<dyn HttpCall> {
                Box::new(NoHttpCall)
            }
            fn open_websocket(
                &self,
                _call: PreparedHttpCall,
            ) -> BoxFuture<'static, Result<Box<dyn HttpWebSocket>, TransportError>> {
                let socket = Socket {
                    requests: self.0.clone(),
                    events: Default::default(),
                };
                Box::pin(async move { Ok(Box::new(socket) as Box<dyn HttpWebSocket>) })
            }
        }
        impl HttpWebSocket for Socket {
            fn send(
                &mut self,
                call: PreparedHttpCall,
            ) -> BoxFuture<'_, Result<(), TransportError>> {
                let (_, _, _, request) = call.into_parts();
                let mut requests = self.requests.lock().unwrap();
                requests.push(serde_json::from_slice(request.body()).unwrap());
                let tool = serde_json::json!({"type":"function_call", "id":"fc1", "call_id":"c1", "name":"read", "arguments":"{}"});
                if requests.len() == 1 {
                    self.events.push_back(
                        serde_json::json!({"type":"response.output_item.added", "item":tool}),
                    );
                    self.events.push_back(serde_json::json!({"type":"response.completed", "response":{"id":"resp_1", "status":"completed", "output":[tool]}}));
                } else {
                    self.events.push_back(serde_json::json!({"type":"response.completed", "response":{"id":"resp_2", "status":"completed", "output":[]}}));
                }
                Box::pin(async { Ok(()) })
            }
            fn receive(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
                let event = self
                    .events
                    .pop_front()
                    .map(|event| serde_json::to_vec(&event).unwrap());
                Box::pin(async move { Ok(event) })
            }
        }

        let recording = Arc::new(RecordingHttp::default());
        let http = recording.clone() as Arc<dyn Http>;
        let source = Arc::new(CliGenerationModelSource::new(
            ModelRef("openai-codex/gpt-6.1-sol".into()),
            Some(ThinkingLevel::Low),
            None,
            Ceilings::default(),
            Default::default(),
        ))
        .bind(http, CancelToken::new(), None);
        let id = SessionId::new();
        let mut messages = vec![Message::text(Role::User, "read")];
        let mut previous_generator = None;
        for generation in 0..2 {
            let profile = source
                .resolve(GenerationModelRequest::new(GenerationRole::Main))
                .await
                .unwrap();
            if let Some(previous) = previous_generator.as_ref() {
                assert!(Arc::ptr_eq(previous, &profile.generator));
            }
            previous_generator = Some(profile.generator.clone());
            let req = GenRequest {
                web_search: None,
                cache: Some(PromptCache {
                    session_id: id,
                    lineage_root: id,
                    retention: CacheRetention::Short,
                    stable_messages: 0,
                }),
                model: profile.request_model.clone(),
                messages: Conversation::new(messages.clone()).unwrap(),
                tools: vec![],
                thinking: profile.thinking,
                max_output_tokens: Some(16),
                parallel_tool_calls: false,
            };
            let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").unwrap());
            let reservation = gate
                .reserve(&profile.metering, None, &req, &profile.capabilities)
                .unwrap();
            let attempt = GenerationAttempt::new(
                GenerationId::new(),
                reservation,
                &profile.metering,
                None,
                &req,
                &profile.capabilities,
            )
            .unwrap();
            let events: Vec<_> = profile.generator.generate(req, attempt).collect().await;
            assert!(
                matches!(
                    events.last(),
                    Some(ego_core::generator::port::AiEvent::Stop(_))
                ),
                "{events:?}"
            );
            if generation == 0 {
                messages.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        call_id: "c1".into(),
                        name: "read".into(),
                        args: "{}".into(),
                    }],
                });
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        call_id: "c1".into(),
                        output: "ok".into(),
                        is_error: false,
                    }],
                });
            }
        }
        // Catches: stale generator reuse after model selection changes.
        let changed = source
            .resolve(
                GenerationModelRequest::new(GenerationRole::Main)
                    .with_model(ModelRef("openai-codex/gpt-6-sol".into())),
            )
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(
            previous_generator.as_ref().unwrap(),
            &changed.generator
        ));
        // Auxiliary resolution must not discard the main tool continuation owner.
        source
            .resolve(GenerationModelRequest::new(GenerationRole::Compaction))
            .await
            .unwrap();
        let returned = source
            .resolve(GenerationModelRequest::new(GenerationRole::Main))
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&changed.generator, &returned.generator));
        let requests = recording.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1]["previous_response_id"], "resp_1",
            "model resolution must retain the completed response across tool generations"
        );
        assert_eq!(
            requests[1]["input"],
            serde_json::json!([{"type":"function_call_output","call_id":"c1","output":"ok"}])
        );
    }

    #[test]
    fn codex_login_never_turns_into_a_chat_completions_row() {
        let row = ProviderRow::openai_codex_default();
        assert_eq!(row.id(), "openai-codex");

        let catalogue =
            Catalogue::new(vec![row.clone()]).with_hosted("openai-codex/gpt-5", codex_hosted());
        let slug = ModelSlug::parse("openai-codex/gpt-5").expect("valid Codex model slug");
        let credential = row
            .credential
            .resolve(&CodexCredentials)
            .expect("Codex fixture credential");
        let http: Arc<dyn Http> = Arc::new(NoHttp);
        let resolved = futures::executor::block_on(catalogue.resolve(&slug, &http, &credential))
            .expect("Codex subscription model resolves");
        let headers = credential.headers();
        let target = RequestTarget::new(resolved.row.base(), &headers);
        let request = GenRequest {
            web_search: None,
            cache: None,
            model: resolved.model.clone(),
            messages: Conversation::new(vec![Message::text(Role::User, "hello")])
                .expect("valid Codex conversation"),
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(16),
            parallel_tool_calls: false,
        };
        let built = resolved
            .wire
            .build_request(&target, &request)
            .expect("Codex Responses request is expressible");
        assert_eq!(
            built.url.as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let body: serde_json::Value = serde_json::from_slice(
            built
                .body
                .as_deref()
                .expect("Codex request has a JSON body"),
        )
        .expect("Codex request body is JSON");
        assert!(body.get("input").is_some(), "Codex uses Responses input");
        assert!(
            body.get("messages").is_none(),
            "Codex must not use Chat Completions messages"
        );
        assert!(selectable().contains(&"openai-codex".to_owned()));
    }

    /// A Codex login creates a subscription credential, not a `models.toml`.
    ///
    /// The builtin row must therefore take its current default through the
    /// normal subscription resolver as far as request assembly without either
    /// file layer.  This is deliberately distinct from the mapping-only test
    /// above: that one supplies synthetic capabilities, while this one proves
    /// the release-pinned facts are present and complete enough for preflight.
    #[test]
    fn an_openai_codex_request_assembles_from_the_builtin_catalogue_alone() {
        let hosted = crate::facts::builtin_only();
        let slug = ModelSlug::parse("openai-codex/gpt-6.1-sol").expect("a valid Codex slug");
        assert!(
            hosted
                .iter()
                .any(|(key, _)| key == "openai-codex/gpt-6.1-sol"),
            "the builtin catalogue does not carry the Codex default",
        );

        let catalogue = catalogue_from_rows(
            hosted,
            mapped_rows(
                ProviderRow::ollama_default(),
                ProviderRow::openai(),
                ProviderRow::anthropic_default(),
            ),
        );
        let row = catalogue
            .row("openai-codex")
            .expect("openai-codex is selectable");
        let credential = row
            .credential
            .resolve(&CodexCredentials)
            .expect("the Codex fixture credential resolves");
        let http: Arc<dyn Http> = Arc::new(NoHttp);
        let resolved = futures::executor::block_on(catalogue.resolve(&slug, &http, &credential))
            .expect("the builtin facts resolve the Codex default");
        assert_eq!(resolved.capabilities.max_context_tokens, Some(272_000));
        assert_eq!(resolved.capabilities.max_output_tokens, Some(128_000));
        assert!(matches!(
            resolved.metering,
            ego_core::budget::Metering::Subscription { .. }
        ));

        let charge = ego_core::generator::attempt::settled_cost(
            &resolved.metering,
            None,
            ego_core::generator::attempt::UsageReport::Reported(ego_core::state::Usage {
                uncached_input_tokens: 1_000,
                output_tokens: 100,
                ..ego_core::state::Usage::IDENTITY
            }),
            None,
            ego_core::cost::CurrencyCode::new("USD").unwrap(),
        );
        let ego_core::cost::CostDelta::Known {
            amount,
            attribution:
                ego_core::cost::CostAttribution::MeteringPolicy {
                    equivalent: Some(estimate),
                    ..
                },
            ..
        } = charge
        else {
            panic!("Codex resolution lost the sourced API equivalent");
        };
        assert!(
            amount.amount.is_zero(),
            "the comparison is never a budget charge"
        );
        let ego_core::cost::SubscriptionEquivalent::V1 {
            model,
            source,
            amount,
            ..
        } = *estimate;
        assert_eq!(model, "openai-codex/gpt-6.1-sol");
        assert_eq!(amount.amount.to_string(), "0.003");
        assert!(
            source
                .0
                .contains("developers.openai.com/api/docs/models/gpt-6.1-sol")
        );

        let headers = credential.headers();
        let target = RequestTarget::new(resolved.row.base(), &headers);
        let built = resolved
            .wire
            .build_request(
                &target,
                &GenRequest {
                    web_search: None,
                    cache: None,
                    model: resolved.model.clone(),
                    messages: Conversation::new(vec![Message::text(Role::User, "hello")])
                        .expect("a valid Codex conversation"),
                    tools: Vec::new(),
                    thinking: ThinkingLevel::Low,
                    max_output_tokens: Some(16),
                    parallel_tool_calls: false,
                },
            )
            .expect("the builtin Codex default is expressible");
        assert_eq!(
            built.url.as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    /// The API-key candidate is still not runnable.
    ///
    /// `ARCHITECTURE.md`
    /// §9 draws the line at selectability: a provider assembled from a
    /// published document has offline evidence and no acceptance evidence, and
    /// its legacy endpoint is now vendor-deprecated. Adding that candidate to
    /// [`rows`] is one line and looks like progress, so the current-contract
    /// rule needs a test rather than a comment. A live recording cannot promote
    /// the retired mapping; an API-key row would first need the current
    /// Responses mapping.
    ///
    /// `gemini` used to be in this list and is not a counter-example to it.
    /// The bar §9 sets is a **current** contract, and
    /// `2026-09-07-a-published-contract-is-enough-to-offer-a-row.md` says a
    /// current published contract with passing fixtures clears it. `xai`
    /// cannot: its mapping is the vendor-deprecated endpoint, so what it lacks
    /// is not a recording but a contract worth recording against.
    #[test]
    fn a_candidate_provider_cannot_be_selected() {
        let ids = selectable();

        let candidate = "xai";
        assert!(
            !ids.contains(&candidate.to_string()),
            "{candidate} has no selectable current row: {ids:?}"
        );
        let catalogue = Catalogue::new(rows(&Default::default()).unwrap());
        assert!(matches!(
            catalogue.row(candidate),
            Err(ResolveError::UnknownProvider { provider, .. }) if provider == candidate
        ));
        assert!(catalogue.row("xai-oauth").is_ok());
    }

    /// An Anthropic request assembles from the builtin catalogue alone.
    ///
    /// The other half of story `regression_case`'s fourth criterion, kept as an
    /// in-process assertion after story `regression_case` moved the child-process
    /// half onto this same row.
    ///
    /// The layers are assembled here, with both file layers explicitly empty
    /// — which is a *stronger* statement about where the facts came from than
    /// an absent file would be, and reads nothing from the environment,
    /// including the `ANTHROPIC_BASE_URL` the harness now sets. What it cannot
    /// say is that the real code path skips the files; the child-process test
    /// in `tests/run.rs` says that.
    ///
    /// It goes as far as `build_request`, because "reaches request assembly"
    /// is the criterion and a `ResolvedModel` that never produced bytes has
    /// not demonstrated that its capabilities are usable.
    #[test]
    fn an_anthropic_request_assembles_from_the_builtin_catalogue_alone() {
        let hosted = crate::facts::builtin_only();
        let slug = ModelSlug::parse("anthropic/claude-opus-5-5").expect("a valid slug");
        assert!(
            hosted
                .iter()
                .any(|(key, _)| key == "anthropic/claude-opus-5-5"),
            "the builtin catalogue does not carry the row this asserts on",
        );

        let catalogue = catalogue_from_rows(
            hosted,
            mapped_rows(
                ProviderRow::ollama_default(),
                ProviderRow::openai(),
                ProviderRow::anthropic_default(),
            ),
        );
        let row = catalogue.row("anthropic").expect("anthropic is selectable");
        let credential = row
            .credential
            .resolve(&AnthropicCredentials)
            .expect("the fixture credential resolves");
        let http: Arc<dyn Http> = Arc::new(NoHttp);
        let resolved = futures::executor::block_on(catalogue.resolve(&slug, &http, &credential))
            .expect("the builtin facts are enough to resolve");
        assert_eq!(
            resolved.capabilities.max_context_tokens,
            Some(1_000_000),
            "the capabilities did not come from the builtin catalogue",
        );

        let headers = credential.headers();
        let target = RequestTarget::new(resolved.row.base(), &headers);
        let built = resolved
            .wire
            .build_request(
                &target,
                &GenRequest {
                    web_search: None,
                    cache: None,
                    model: resolved.model.clone(),
                    messages: Conversation::new(vec![Message::text(Role::User, "hello")])
                        .expect("a valid conversation"),
                    tools: Vec::new(),
                    thinking: ThinkingLevel::High,
                    max_output_tokens: Some(16),
                    parallel_tool_calls: true,
                },
            )
            .expect("the request is expressible against the builtin facts");
        let body: serde_json::Value =
            serde_json::from_slice(built.body.as_deref().expect("a JSON body"))
                .expect("the body is JSON");
        assert_eq!(body["model"], "claude-opus-5-5");
    }

    /// The exact selectable set. Any accidental promotion or omission fails
    /// and names the whole table.
    #[test]
    fn the_selectable_provider_table_includes_subscription_rows() {
        assert_eq!(
            selectable(),
            [
                "ollama",
                "openai",
                "openai-codex",
                "anthropic",
                "github-copilot",
                "xai-oauth",
                "kimi-coding",
                "minimax-oauth",
                "nous",
                "openrouter",
                "gemini",
            ],
            "the selectable provider table changed"
        );
    }
}
