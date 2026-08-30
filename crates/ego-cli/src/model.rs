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
use ego_ai::generator::StaticRequestProfile;
use ego_ai::http::Http;
use ego_ai::provider::{Credentials, OLLAMA_BASE, OPENAI_BASE, ProviderRow, ResolvedCredential};
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
    let mut rows = mapped_rows(
        ProviderRow::ollama(base(OLLAMA_HOST, OLLAMA_BASE)?),
        ProviderRow::openai_at(base(OPENAI_BASE_URL, OPENAI_BASE)?),
    );
    for row in &mut rows {
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

fn mapped_rows(ollama: ProviderRow, openai: ProviderRow) -> Vec<ProviderRow> {
    vec![
        ollama,
        openai,
        ProviderRow::openai_codex_default(),
        ProviderRow::anthropic_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
        ProviderRow::openrouter_default(),
    ]
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
    let provider_id = identity
        .0
        .split_once('/')
        .map(|(provider, _)| provider)
        .unwrap_or(identity.0.as_str());
    ensure_provider_mapping(provider_id).map_err(generation_error)?;
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
        ),
    );
    catalogue
        .static_request_profile(&slug)
        .map_err(generation_error)
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
    credential_commands: Arc<dyn crate::auth::CredentialCommandPort>,
}

impl CliGenerationModelSource {
    pub fn new(
        identity: ModelRef,
        thinking: ThinkingLevel,
        judge: Option<ModelRef>,
        ceilings: Ceilings,
        auth: std::collections::HashMap<String, AuthProviderLayer>,
    ) -> Self {
        Self {
            selection: Arc::new(GenerationSelection::new(identity, thinking)),
            judge,
            ceilings,
            auth,
            credential_commands: Arc::new(crate::auth::OsCredentialCommand),
        }
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
        })
    }

    async fn resolve_role(
        &self,
        request: GenerationModelRequest,
        http: &Arc<dyn Http>,
        cancel: &CancelToken,
        run_deadline: Option<std::time::Instant>,
    ) -> Result<GenerationProfile, GenerationModelError> {
        let (identity, thinking) = match request.role() {
            GenerationRole::Main
            | GenerationRole::Compaction
            | GenerationRole::Subagent
            | GenerationRole::Hook => {
                let selection = self.selection.read()?;
                (
                    request.model().cloned().unwrap_or(selection.identity),
                    request.thinking().unwrap_or(selection.thinking),
                )
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
        let (resolved, credential) =
            resolve(&identity.0, hosted, http, credentials.as_ref(), &self.auth)
                .await
                .map_err(generation_error)?;
        let request_model = resolved.model.clone();
        let generator: Arc<dyn Generator> = Arc::new(ego_ai::generator::WireGenerator::new(
            resolved.row,
            resolved.wire,
            Arc::clone(http),
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

struct BoundGenerationModelSource {
    source: Arc<CliGenerationModelSource>,
    http: Arc<dyn Http>,
    cancel: CancelToken,
    run_deadline: Option<std::time::Instant>,
}

impl GenerationModelSource for BoundGenerationModelSource {
    fn resolve<'a>(
        &'a self,
        request: GenerationModelRequest,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
        Box::pin(
            self.source
                .resolve_role(request, &self.http, &self.cancel, self.run_deadline),
        )
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

    fn selectable() -> Vec<String> {
        rows(&Default::default())
            .expect("the built-in bases parse")
            .into_iter()
            .map(|row| row.id().to_owned())
            .collect()
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
                "minimax-oauth",
                "nous",
                "openrouter",
            ],
            "the selectable provider table changed"
        );
    }
}
