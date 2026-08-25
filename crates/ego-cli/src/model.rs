//! Turning `--model P/M` into something a turn can be run against.
//!
//! The split is `ego-ai`'s and this module does not reopen it: a hosted
//! model's price and ceilings are *supplied*, a local daemon's are
//! *discovered*, and which of the two a provider gets is a fact its row
//! carries (`ego_ai::resolver`). What lives here is only the part that needs
//! the process — reading the environment for a credential and for a daemon
//! address.

use std::sync::{Arc, RwLock};

use ego_ai::catalogue::{Catalogue, ModelSlug, ResolveError, ResolvedModel};
use ego_ai::http::{EgressPolicy, Http, ReqwestHttp};
use ego_ai::provider::{Credentials, OLLAMA_BASE, OPENAI_BASE, ProviderRow, ResolvedCredential};
use ego_core::ceiling::Ceilings;
use ego_core::config::{AuthProviderLayer, ModelRef};
use ego_core::generation_model_source::{
    GenerationModelError, GenerationModelErrorClass, GenerationModelSource, GenerationProfile,
    GenerationRole,
};
use ego_core::generator::port::Generator;
use ego_core::generator::types::ThinkingLevel;
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

/// The API base an address variable names, or the default.
///
/// `OLLAMA_HOST` is conventionally a bare `host:port` and `OPENAI_BASE_URL` is
/// conventionally a base URL without the path; both spellings are accepted for
/// both, and anything else is refused rather than half-parsed into a request to
/// somewhere unintended. The path is always set, so a variable that already
/// carries one and a variable that does not resolve to the same address.
///
/// It sets `/v1/` and not `/v1/chat/completions`: a row carries a base and the
/// mapping joins its own path onto it
/// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
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

/// The transport this run generates over.
///
/// Built once and shared: `--offline` is a property of the transport
/// (`ego-ai/src/http.rs`), so a second client built without the policy would
/// be a hole in it.
pub fn transport(offline: bool) -> Result<Arc<dyn Http>, ResolveError> {
    let policy = if offline {
        EgressPolicy::LoopbackOnly
    } else {
        EgressPolicy::Unrestricted
    };
    ReqwestHttp::new(policy)
        .map(|http| Arc::new(http) as Arc<dyn Http>)
        .map_err(|err| ResolveError::EndpointUnreachable {
            endpoint: "the local transport".to_string(),
            message: err.to_string(),
        })
}

/// The rows this build knows how to reach.
///
/// Every row, unconditionally. A row names the variable its credential lives
/// in and never reads it
/// (`docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §3), so a
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
/// key row. Gemini remains absent because this batch adds no current login or
/// acceptance evidence for it.
pub(crate) fn rows(
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
) -> Result<Vec<ProviderRow>, ResolveError> {
    let mut rows = vec![
        ProviderRow::ollama(base(OLLAMA_HOST, OLLAMA_BASE)?),
        ProviderRow::openai_at(base(OPENAI_BASE_URL, OPENAI_BASE)?),
        ProviderRow::openai_codex_default(),
        ProviderRow::anthropic_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::qwen_oauth_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
        ProviderRow::openrouter_default(),
    ];
    for row in &mut rows {
        if let Some(config) = auth.get(&row.id) {
            let overlay = config
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            row.extra_headers = overlay_headers(&row.extra_headers, &overlay).map_err(|err| {
                ResolveError::EndpointUnreachable {
                    endpoint: format!("auth.{}.headers", row.id),
                    message: err.to_string(),
                }
            })?;
        }
    }
    Ok(rows)
}

/// Assemble the one resolver-selected catalogue from current rows and facts.
pub(crate) fn catalogue(
    hosted: Vec<(String, facts::Hosted)>,
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
) -> Result<Catalogue, ResolveError> {
    let mut catalogue = Catalogue::new(rows(auth)?);
    for (slug, model) in hosted {
        catalogue = catalogue.with_hosted(&slug, model);
    }
    Ok(catalogue)
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
/// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
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
    // Read here, where a refusal can name the variable to set, rather than as
    // a 401 several layers later. No per-provider branch: the row names its
    // own variable, so this one line covers every provider that has one and
    // says nothing about the ones that do not.
    let credential = row.credential.resolve(creds)?;
    // No branch on the provider id. The row names what resolves it
    // (`ego_ai::resolver`), so this crate does not have to know which
    // providers are daemons in order to resolve any of them.
    let model = catalogue.resolve(&slug, http, &credential).await?;
    Ok((model, credential))
}

#[derive(Debug, Clone)]
struct MainSelection {
    identity: ModelRef,
    thinking: ThinkingLevel,
}

/// Read-only view of the model authority currently installed for main turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationSelectionSnapshot {
    pub identity: ModelRef,
    pub thinking: ThinkingLevel,
}

/// Mutable session selection observed afresh at every main generation boundary.
#[derive(Debug)]
pub struct GenerationSelection {
    current: RwLock<MainSelection>,
}

impl GenerationSelection {
    pub fn new(identity: ModelRef, thinking: ThinkingLevel) -> Self {
        Self {
            current: RwLock::new(MainSelection { identity, thinking }),
        }
    }

    /// Install the selection committed by the interactive picker.
    pub fn install(&self, identity: ModelRef, thinking: ThinkingLevel) -> Result<(), String> {
        let mut current = self
            .current
            .write()
            .map_err(|_| "the model selection lock is poisoned".to_string())?;
        *current = MainSelection { identity, thinking };
        Ok(())
    }

    /// Observe the same mutable authority read at every generation boundary.
    pub fn snapshot(&self) -> Result<GenerationSelectionSnapshot, GenerationModelError> {
        let current = self.read()?;
        Ok(GenerationSelectionSnapshot {
            identity: current.identity,
            thinking: current.thinking,
        })
    }

    fn read(&self) -> Result<MainSelection, GenerationModelError> {
        self.current
            .read()
            .map(|selection| selection.clone())
            .map_err(|_| {
                GenerationModelError::new(
                    GenerationModelErrorClass::Internal,
                    "the model selection lock is poisoned",
                )
            })
    }
}

/// CLI adapter for the core-owned fresh generation-model port.
pub struct CliGenerationModelSource {
    selection: Arc<GenerationSelection>,
    judge: Option<ModelRef>,
    ceilings: Ceilings,
    auth: std::collections::HashMap<String, AuthProviderLayer>,
    offline: bool,
    http: Arc<dyn Http>,
}

impl CliGenerationModelSource {
    pub fn new(
        identity: ModelRef,
        thinking: ThinkingLevel,
        judge: Option<ModelRef>,
        ceilings: Ceilings,
        auth: std::collections::HashMap<String, AuthProviderLayer>,
        offline: bool,
    ) -> Result<Self, ResolveError> {
        Ok(Self {
            selection: Arc::new(GenerationSelection::new(identity, thinking)),
            judge,
            ceilings,
            auth,
            offline,
            http: transport(offline)?,
        })
    }

    pub fn selection(&self) -> Arc<GenerationSelection> {
        Arc::clone(&self.selection)
    }

    async fn resolve_role(
        &self,
        role: GenerationRole,
    ) -> Result<GenerationProfile, GenerationModelError> {
        let (identity, thinking) = match role {
            GenerationRole::Main | GenerationRole::Compaction => {
                let selection = self.selection.read()?;
                (selection.identity.clone(), selection.thinking)
            }
            GenerationRole::Judge => (
                self.judge.clone().ok_or_else(|| {
                    GenerationModelError::new(
                        GenerationModelErrorClass::Usage,
                        "no judge model is configured",
                    )
                })?,
                ThinkingLevel::Off,
            ),
        };
        let provider_id = identity
            .0
            .split_once('/')
            .map(|(provider, _)| provider)
            .unwrap_or(identity.0.as_str());
        ensure_provider_mapping(provider_id).map_err(generation_error)?;
        let hosted = facts::supplied(&self.ceilings).map_err(|error| {
            GenerationModelError::new(GenerationModelErrorClass::Usage, error.to_string())
        })?;
        let credentials = crate::auth::request_credentials(
            provider_id,
            self.auth
                .get(provider_id)
                .and_then(|auth| auth.credential_command.as_deref()),
            self.offline,
        )
        .await
        .map_err(auth_generation_error)?;
        let (resolved, credential) = resolve(
            &identity.0,
            hosted,
            &self.http,
            credentials.as_ref(),
            &self.auth,
        )
        .await
        .map_err(generation_error)?;
        let request_model = resolved.model.clone();
        let generator: Arc<dyn Generator> = Arc::new(ego_ai::generator::WireGenerator::new(
            resolved.row,
            Arc::clone(&self.http),
            credential,
        ));
        Ok(GenerationProfile {
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

impl GenerationModelSource for CliGenerationModelSource {
    fn resolve<'a>(
        &'a self,
        role: GenerationRole,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
        Box::pin(self.resolve_role(role))
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
        | crate::auth::AuthError::ReloginRequired { .. } => GenerationModelErrorClass::Usage,
    };
    GenerationModelError::new(class, error.to_string())
}

fn unavailable_mapping(provider: &str) -> Option<ResolveError> {
    match provider {
        "xai" => Some(ResolveError::ProviderMappingUnavailable {
            provider: provider.to_string(),
            mapping: "xai-responses for API-key credentials; use `xai-oauth/<model>` after `ego auth login xai-oauth`"
                .to_string(),
        }),
        _ => None,
    }
}

pub(crate) fn ensure_provider_mapping(provider: &str) -> Result<(), ResolveError> {
    unavailable_mapping(provider).map_or(Ok(()), Err)
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

    fn selectable() -> Vec<String> {
        rows(&Default::default())
            .expect("the built-in bases parse")
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    #[test]
    fn codex_login_never_turns_into_a_chat_completions_row() {
        let row = ProviderRow::openai_codex_default();
        assert_eq!(row.id, "openai-codex");
        assert_eq!(format!("{:?}", row.wire), "OpenAiCodexResponses");
        assert_ne!(format!("{:?}", row.wire), "OpenAiCompletions");
        assert!(unavailable_mapping("openai-codex").is_none());
    }

    /// The API-key candidate is still not runnable.
    ///
    /// `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §9 draws the line at selectability: a provider assembled from a
    /// published document has offline evidence and no acceptance evidence, and
    /// its legacy endpoint is now vendor-deprecated. Adding that candidate to
    /// [`rows`] is one line and looks like progress, so the current-contract
    /// rule needs a test rather than a comment. A live recording cannot promote
    /// the retired mapping; an API-key row would first need the current
    /// Responses mapping.
    #[test]
    fn a_candidate_provider_cannot_be_selected() {
        let ids = selectable();

        for candidate in ["xai", "gemini"] {
            assert!(
                !ids.contains(&candidate.to_string()),
                "{candidate} has no selectable current row: {ids:?}"
            );
        }
        let candidate = "xai";
        let refusal = unavailable_mapping(candidate).expect("the legacy id is actionable");
        assert!(matches!(
            &refusal,
            ResolveError::ProviderMappingUnavailable { provider, mapping }
                if provider == candidate && mapping.contains("xai-responses")
        ));
        assert!(
            refusal.to_string().contains("xai-oauth"),
            "the refusal must name the selectable current alternative: {refusal}"
        );
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
                "qwen-oauth",
                "minimax-oauth",
                "nous",
                "openrouter",
            ],
            "the selectable provider table changed"
        );
    }
}
