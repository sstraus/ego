//! A provider row: where to send a request, and what to authenticate it with.
//!
//! See the provider guide at `docs/guides/providers.md`.
//!
//! A row is configuration. The credential inside it is not, and the
//! difference is enforced by [`Secret`] rather than by field privacy: a row
//! is worth logging, and the value that must never appear in a log is the
//! one that refuses to print itself wherever it goes.
//!
//! The two rows this milestone ships are constructors, not a static table.
//! A row needs a credential read from the environment and an endpoint an
//! operator may point elsewhere, and neither is a constant. What a model
//! *resolves to* — capabilities and price — lives in `catalogue.rs`, because
//! a row is where to send a request and says nothing about what the model on
//! the other end can do.

use std::fmt;
use std::sync::Arc;

use ego_core::credentials::{ProviderSecret, SecretMaterial};
use ego_core::permit::HttpHeader;
use url::Url;

use crate::http::{AdmittedHttpProfile, bearer_header};
use crate::resolver::{KimiManaged, ModelResolver, OllamaDaemon, SubscriptionSupplied, Supplied};
use crate::wire::anthropic_messages::AnthropicMessages;
use crate::wire::gemini_interactions::GeminiInteractions;
use crate::wire::github_copilot_completions::GithubCopilotCompletions;
use crate::wire::minimax_anthropic::MiniMaxAnthropic;
use crate::wire::nous_portal::NousPortal;
use crate::wire::openai_codex_responses::OpenAiCodexResponses;
use crate::wire::provider_cost::{OpenRouterUsd, ProviderCost, XaiUsdTicks};
use crate::wire::xai_responses::XaiResponses;

mod cloud;
pub use cloud::{
    BEDROCK, BEDROCK_DEFAULT_REGION, BEDROCK_KEY_VAR, VERTEX, VERTEX_BASE, VERTEX_DEFAULT_LOCATION,
    VERTEX_LOCATION_VAR, VERTEX_PROJECT_VAR, VertexAddressPart, bedrock_base, vertex_base,
};

/// The single core-owned provider secret type.
///
/// Kept under the established public name so credential sources do not need a
/// second wrapper. The aliased owner is non-`Clone`, zeroizes on drop, and
/// redacts its formatter.
pub type Secret = ProviderSecret;

/// A host-supplied Codex access token. Its owner, not ego, controls renewal.
pub const OPENAI_CODEX_ACCESS_TOKEN_ENV: &str = "OPENAI_CODEX_ACCESS_TOKEN";

/// How a provider presents a credential, and where the credential is read
/// from — the *strategy*, never the value.
///
/// Rows name credential variables instead of holding secrets. Missing keys
/// therefore do not remove providers from the catalogue; authentication
/// refusals occur where the operator can act on them.
#[derive(Debug, Clone)]
pub enum CredentialStrategy {
    /// None at all — a local daemon that does not authenticate. A distinct
    /// variant rather than an empty variable name, so "this provider needs no
    /// key" and "the key is missing" cannot be confused.
    None,
    /// `Authorization: Bearer <the value of this variable>`.
    BearerFromEnv { var: String },
    /// `<header>: <the value of this variable>`, **verbatim and with no
    /// scheme**.
    ///
    /// Anthropic reads `x-api-key` and Gemini reads `x-goog-api-key`, and
    /// neither is an `Authorization` header with a different spelling: there
    /// is no `Bearer ` to prepend, and prepending one sends a key the provider
    /// cannot read. A second variant rather than a nullable prefix on the
    /// first, because "which header" and "which scheme" are one fact per
    /// provider and a mapping never sees either.
    HeaderFromEnv { header: String, var: String },
    /// A rotating credential in the caller-owned subscription store.
    OAuth { provider: String },
    /// A permanent key minted through OAuth, with an environment fallback.
    OAuthOrEnv { provider: String, var: String },
    /// A Google Cloud OAuth access token that `ego auth login PROVIDER` mints
    /// from Application Default Credentials, sent as a bearer token to the
    /// project and location the two variables name.
    ///
    /// The address is part of the credential's resolution and not of the row:
    /// a Vertex AI endpoint names the caller's project in its path, so a
    /// missing project is refused here, where the variable can be named.
    GoogleCloud {
        provider: String,
        project_var: String,
        location_var: String,
    },
    /// Anthropic's API key and Claude subscription token are two wire
    /// presentations selected by the credential prefix.
    Anthropic {
        provider: String,
        api_key_var: String,
        oauth_var: String,
    },
}

impl CredentialStrategy {
    /// The credential this strategy names, read once and turned into the
    /// header that carries it.
    ///
    /// Resolve once outside the row. Per-request resolution could fail after
    /// startup or silently change identity if the environment rotates. The row
    /// retains a secret-free strategy; the generator holds the resolved value.
    pub fn resolve(
        &self,
        creds: &dyn Credentials,
    ) -> Result<ResolvedCredential, MissingCredential> {
        let (headers, base_override) = match self {
            // Not an error and not an empty header: a provider that
            // authenticates nothing is a different fact from a key that is
            // missing, and collapsing them would make a misconfigured hosted
            // row look like a local daemon.
            CredentialStrategy::None => (Vec::new(), None),
            CredentialStrategy::BearerFromEnv { var } => {
                let secret = creds.secret(var)?;
                (vec![bearer_header(secret.expose_secret_bytes())], None)
            }
            CredentialStrategy::HeaderFromEnv { header, var } => {
                let header = creds
                    .secret(var)?
                    .into_secret_header(header)
                    .map_err(|_| MissingCredential { var: var.clone() })?;
                (vec![header], None)
            }
            CredentialStrategy::OAuth { provider } => {
                // Select the external credential before consulting the store. An
                // invalid supplied token must fail, never fall back to a login.
                let external = if provider == "openai-codex" {
                    creds.secret(OPENAI_CODEX_ACCESS_TOKEN_ENV).ok()
                } else {
                    None
                };
                let secret = match external {
                    Some(secret) => secret,
                    None => creds.secret(&format!("oauth:{provider}")).map_err(|_| {
                        MissingCredential {
                            var: format!("ego auth login {provider}"),
                        }
                    })?,
                };
                let base_override = crate::oauth::provider_base(provider, secret_text(&secret)?)
                    .map_err(|_| MissingCredential {
                        var: format!("ego auth login {provider}"),
                    })?;
                (oauth_headers(provider, &secret)?, base_override)
            }
            CredentialStrategy::OAuthOrEnv { provider, var } => {
                let secret = creds
                    .secret(&format!("oauth:{provider}"))
                    .or_else(|_| creds.secret(var))
                    .map_err(|_| MissingCredential {
                        var: format!("ego auth login {provider}"),
                    })?;
                (oauth_headers(provider, &secret)?, None)
            }
            CredentialStrategy::GoogleCloud {
                provider,
                project_var,
                location_var,
            } => cloud::google_cloud_credential(creds, provider, project_var, location_var)?,
            CredentialStrategy::Anthropic {
                provider,
                api_key_var,
                oauth_var,
            } => {
                let secret = creds
                    .secret(&format!("oauth:{provider}"))
                    .or_else(|_| creds.secret(oauth_var))
                    .or_else(|_| creds.secret(api_key_var))
                    .map_err(|_| MissingCredential {
                        var: format!("ego auth login {provider}"),
                    })?;
                (oauth_headers("anthropic", &secret)?, None)
            }
        };
        Ok(ResolvedCredential {
            headers,
            base_override,
        })
    }

    /// The environment variable a key may be supplied in, if this strategy
    /// reads one.
    ///
    /// It exists so a *refusal* can name the variable the row already names.
    /// `ego auth login openai` used to answer "configure
    /// auth.openai.credential_command or install ego-auth-openai" — two
    /// remedies, neither of them the one that works, because the login path
    /// had no way to learn that this row reads `OPENAI_API_KEY`. A row that
    /// knows which variable it reads and a message that cannot say so is the
    /// fact and the sentence being kept in different places.
    ///
    /// The two OAuth-backed strategies report their variable too. That is not
    /// a claim login is pointless for them — it is the fallback the strategy
    /// itself already tries, so an operator who has a key and no browser has
    /// somewhere to put it.
    pub fn environment_var(&self) -> Option<&str> {
        match self {
            CredentialStrategy::None
            | CredentialStrategy::OAuth { .. }
            | CredentialStrategy::GoogleCloud { .. } => None,
            CredentialStrategy::BearerFromEnv { var }
            | CredentialStrategy::HeaderFromEnv { var, .. }
            | CredentialStrategy::OAuthOrEnv { var, .. } => Some(var),
            CredentialStrategy::Anthropic { api_key_var, .. } => Some(api_key_var),
        }
    }
}

fn oauth_headers(provider: &str, secret: &Secret) -> Result<Vec<HttpHeader>, MissingCredential> {
    crate::oauth::provider_auth(provider, secret_text(secret)?)
        .map_err(|_| MissingCredential {
            var: format!("ego auth login {provider}"),
        })
        .map(|auth| auth.headers())
}

fn secret_text(secret: &SecretMaterial) -> Result<&str, MissingCredential> {
    std::str::from_utf8(secret.expose_secret_bytes()).map_err(|_| MissingCredential {
        var: "credential contains non-UTF-8 bytes".to_string(),
    })
}

/// One provider's credential, resolved: the header it becomes, or nothing.
///
/// The value the row deliberately does not hold. It exists once per run, in
/// the generator that sends it, and it is reachable from no record: a
/// [`ResolvedModel`](crate::catalogue::ResolvedModel) is `Clone` and `Debug`
/// and travels to the budget gate and the ledger, which is precisely why the
/// credential does not travel with it.
///
/// Its custom `Debug` prints header names and the optional base only; the
/// secret owner independently redacts itself as defense in depth.
pub struct ResolvedCredential {
    headers: Vec<HttpHeader>,
    base_override: Option<Url>,
}

impl ResolvedCredential {
    /// The headers this credential contributes to a request.
    ///
    /// Only the credential. Content type and accept belong to the wire
    /// format, which adds them itself
    /// ([`RequestTarget::post_json`](crate::wire::RequestTarget::post_json)),
    /// because they are facts about the body — not about the provider.
    ///
    /// The raw value remains inaccessible here. Cloning the typed header only
    /// shares its zeroizing owner with the request about to be sent.
    pub fn headers(&self) -> Vec<HttpHeader> {
        self.headers.clone()
    }

    pub fn base_override(&self) -> Option<&Url> {
        self.base_override.as_ref()
    }
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedCredential")
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(HttpHeader::name)
                    .collect::<Vec<_>>(),
            )
            .field("base_override", &self.base_override)
            .finish()
    }
}

/// Where a credential named by an [`Auth`] actually comes from.
///
/// A seam rather than a direct `std::env::var`, for two reasons that point the
/// same way. `AGENTS.md` puts the process environment in `ego-cli`'s ownership,
/// and a library reaching for it behind the caller's back is the ambient
/// dependency that makes a resolution impossible to reason about. And it is
/// the only way this is testable at all: `std::env::set_var` is `unsafe` in
/// edition 2024 and this workspace denies `unsafe_code`, so a test that had to
/// set a variable to exercise the success path could not be written.
pub trait Credentials: Send + Sync {
    /// The secret in `var`, or a refusal naming it.
    ///
    /// `secret` and not `bearer`: what a variable holds is a key, and whether
    /// it is presented as a bearer token or as an `x-api-key` is
    /// [`CredentialStrategy`]'s business. The method was named for the one
    /// caller it had, and the second caller made the name false.
    fn secret(&self, var: &str) -> Result<Secret, MissingCredential>;
}

/// The process environment: the only [`Credentials`] a real run uses.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvCredentials;

impl Credentials for EnvCredentials {
    fn secret(&self, var: &str) -> Result<Secret, MissingCredential> {
        match std::env::var(var) {
            Ok(value) if !value.trim().is_empty() => Ok(value.into()),
            // A variable that is set to whitespace is not a credential, and
            // treating it as one would send `Bearer ` and blame the provider.
            _ => Err(MissingCredential {
                var: var.to_string(),
            }),
        }
    }
}

/// No credential where one was required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCredential {
    pub var: String,
}

impl fmt::Display for MissingCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.var.starts_with("ego auth login ") {
            return write!(
                f,
                "not signed in to the subscription; sign in with '{}'",
                self.var
            );
        }
        if self.var == VERTEX_PROJECT_VAR || self.var == VERTEX_LOCATION_VAR {
            return write!(
                f,
                "no valid Google Cloud value in {}; set this environment variable",
                self.var
            );
        }
        write!(
            f,
            "no API key in {}; set this environment variable to the provider's API key",
            self.var
        )
    }
}

impl std::error::Error for MissingCredential {}

/// The final charge behavior one provider profile promises to conformance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostExpectation {
    Positive,
    NoCharge,
}

/// One provider profile: who it is, where it lives, how it authenticates,
/// which shape its bytes take, and whether it states a charge.
///
/// Deployment, key and account configuration live here. API-shape variation
/// lives behind [`wire`](Self::wire), not in provider-specific shared branches.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    id: String,
    /// Scheme, host and any prefix — and **no path this row owns**.
    ///
    /// The row carries a base URL; the mapping owns its API path.
    /// [`RequestTarget::url`](crate::wire::RequestTarget::url) joins them so the
    /// provider row cannot disagree with its mapping about the endpoint path.
    ///
    /// **Always ends in `/`**, and the constructors guarantee it rather than
    /// asking callers to. `Url::join` treats a base without a trailing slash
    /// as a *file* and replaces its last segment, so
    /// `https://gateway.example/openai/v1` would resolve
    /// `chat/completions` to `https://gateway.example/openai/chat/completions`
    /// — a silently wrong address for a deployment shape that really exists.
    base: Url,
    /// Exact network routes this immutable provider row admits.
    pub(crate) http_profile: AdmittedHttpProfile,
    pub credential: CredentialStrategy,
    /// Non-secret identity headers declared by this profile. Credential and
    /// wire-format headers are excluded and validated at configuration load.
    pub extra_headers: Vec<(String, String)>,
    cost_expectation: CostExpectation,
    /// What turns one of this provider's model ids into the facts a turn
    /// needs.
    ///
    /// A `&'static dyn` for exactly the reasons [`wire`](Self::wire) is one,
    /// and it removes the same kind of thing: `crates/ego-cli/src/model.rs`
    /// used to ask `if slug.provider == catalogue::OLLAMA` and call one of two
    /// functions, which is a caller that has to know which providers are
    /// daemons in order to resolve any of them. The row knows; nobody else
    /// needs to.
    ///
    /// Supplied hosted facts and live daemon discovery are different resolution
    /// contracts; neither should be inferred from a provider name by callers.
    pub resolver: &'static dyn ModelResolver,
    /// How this provider states the money it will charge, when it states it
    /// at all.
    ///
    /// Not the wire format field the doc above rules out, and the difference
    /// is the point: a wire format is a shape several providers share, while
    /// this is one provider's own statement about one field in its own
    /// responses. OpenAI and Anthropic report billing *dimensions* and no
    /// monetary total, so their rows carry `None`; OpenRouter and xAI report
    /// a figure, and theirs carry the decoder that reads it.
    ///
    /// `None` is not "we have not got round to it": it is the row asserting
    /// that no cost will be stated, which is what makes
    /// [`UnknownCost::ProviderReportedCostMissing`](ego_core::cost::UnknownCost::ProviderReportedCostMissing)
    /// meaningful on the rows that do carry one.
    ///
    /// Behind an `Arc` because a row is `Clone` and a decoder is immutable
    /// configuration shared by every generation against that row.
    pub cost: Option<Arc<dyn ProviderCost>>,
    /// Where the third-party price table files this row's models, when it
    /// does not file them under the row's own id (`cloud.rs`).
    price_table: Option<PriceTableName>,
    /// Whether this row's wire mapping has cleared the live-promotion gate.
    ///
    /// Each constructor states promotion explicitly so a new row cannot
    /// silently inherit selectability.
    pub promotion: Promotion,
}

/// A row's namespace and key prefix in the third-party price table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceTableName {
    pub provider: &'static str,
    pub key_prefix: &'static str,
}

/// Whether a row may be offered, or is waiting on evidence only an operator
/// can produce.
/// Live acceptance requires a provider key and recording.
/// [`Selectable`](Self::Selectable) states only that no promotion gate is
/// outstanding; it does not claim that a live acceptance check was performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    /// No outstanding gate. May be resolved, listed as available, and picked.
    Selectable,
    /// The mapping is implemented and its deterministic fixtures pass; check
    /// five has never been run for it. Refused on every surface that offers a
    /// model, before any credential is read.
    PendingLiveEvidence {
        /// The mapping awaiting the recording, named so the refusal says what
        /// would lift it rather than only that something is missing.
        mapping: &'static str,
    },
}

/// The base OpenAI serves its API under.
pub const OPENAI_BASE: &str = "https://api.openai.com/v1/";
/// Codex subscription backend (base path joined by the mapping).
pub const OPENAI_CODEX_BASE: &str = "https://chatgpt.com/backend-api/";
/// The environment variable OpenAI's credential is read from.
pub const OPENAI_KEY_VAR: &str = "OPENAI_API_KEY";
/// The base a default Ollama install serves its openai-compatible API under.
pub const OLLAMA_BASE: &str = "http://localhost:11434/v1/";
/// The base xAI serves its API under.
pub const XAI_BASE: &str = "https://api.x.ai/v1/";
/// The base OpenRouter serves its API under.
pub const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1/";
/// The environment variable OpenRouter's credential is read from.
pub const OPENROUTER_KEY_VAR: &str = "OPENROUTER_API_KEY";
/// The base Anthropic serves its API under.
pub const ANTHROPIC_BASE: &str = "https://api.anthropic.com/v1/";
/// The environment variable Anthropic's credential is read from.
pub const ANTHROPIC_KEY_VAR: &str = "ANTHROPIC_API_KEY";
pub const GITHUB_COPILOT_BASE: &str = "https://api.individual.githubcopilot.com/";
pub const KIMI_CODING_BASE: &str = "https://api.kimi.com/coding/v1/";
pub const MINIMAX_OAUTH_BASE: &str = "https://api.minimax.io/anthropic/v1/";
pub const NOUS_BASE: &str = "https://inference-api.nousresearch.com/v1/";
/// The base Google serves the Gemini developer API under.
pub const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1/";
/// The environment variable Gemini's credential is read from.
pub const GEMINI_KEY_VAR: &str = "GEMINI_API_KEY";

/// Which chat-completions schema the cost decoders were written against.
///
/// One string for both because it is one fact: the date this repository read
/// the two response schemas and wrote decoders for the fields they document. A
/// `ProviderCostRevision` exists to make a decoded figure attributable, and
/// attributing it to a version number neither provider publishes would be
/// worse than attributing it to the day somebody looked.
const READ_ON: &str = "2026-08-21";
const XAI_RESPONSES_READ_ON: &str = "2026-08-24";

/// OpenAI's public row selects its exact model contract.
static OPENAI_SUPPLIED: crate::resolver::OpenAiSupplied = crate::resolver::OpenAiSupplied;

/// Gateways that publish an OpenAI-*compatible* chat-completions surface.
///
/// Separate from [`OPENAI_SUPPLIED`]: OpenRouter serves Chat Completions,
/// not OpenAI's Responses endpoint. Sharing a static would silently retarget
/// compatible gateways when the OpenAI row changes mappings.
static OPENROUTER_MODELS: crate::resolver::OpenRouter = crate::resolver::OpenRouter;
static ANTHROPIC_SUPPLIED: Supplied = Supplied::new(&AnthropicMessages);
static GEMINI_SUPPLIED: Supplied = Supplied::new(&GeminiInteractions);
static OPENAI_CODEX_SUBSCRIPTION: SubscriptionSupplied = SubscriptionSupplied::new(
    "ego/builtin/openai-codex-subscription/v1",
    &OpenAiCodexResponses,
);
static GITHUB_COPILOT_SUBSCRIPTION: SubscriptionSupplied = SubscriptionSupplied::new(
    "ego/builtin/github-copilot-subscription/v1",
    &GithubCopilotCompletions,
);
static XAI_OAUTH_SUBSCRIPTION: SubscriptionSupplied =
    SubscriptionSupplied::new("ego/builtin/xai-oauth-subscription/v1", &XaiResponses);
static MINIMAX_OAUTH_SUBSCRIPTION: SubscriptionSupplied = SubscriptionSupplied::new(
    "ego/builtin/minimax-oauth-subscription/v1",
    &MiniMaxAnthropic,
);
static NOUS_SUBSCRIPTION: SubscriptionSupplied =
    SubscriptionSupplied::new("ego/builtin/nous-subscription/v1", &NousPortal);

/// A URL made safe to [`Url::join`] a mapping's path onto.
///
/// One line, and it is the difference between a request reaching a gateway's
/// `/openai/v1/chat/completions` and reaching its `/openai/chat/completions`.
/// Applied in the constructors rather than asserted in a doc, because the
/// caller supplying the base is an operator with an environment variable and
/// a trailing slash is not a thing an operator should have to know about.
fn as_base(mut base: Url) -> Url {
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    base
}

fn admitted_base(id: &str, base: Url) -> (Url, AdmittedHttpProfile) {
    let base = as_base(base);
    let profile = AdmittedHttpProfile::from_urls(id, std::slice::from_ref(&base))
        .expect("a provider base already parsed as an HTTP URL is an admitted route");
    (base, profile)
}

impl ProviderRow {
    /// The stable provider identifier bound into this row's HTTP profile.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The mapping this row is waiting on live evidence for, if it is waiting.
    ///
    /// The whole of the promotion question as anything outside this module
    /// needs to ask it: `Some` means refuse and name this mapping, `None`
    /// means no gate is outstanding. Callers do not match on [`Promotion`]
    /// themselves, so adding a state cannot silently mean "selectable" at a
    /// call site that forgot to handle it.
    pub const fn pending_live_evidence(&self) -> Option<&'static str> {
        match self.promotion {
            Promotion::Selectable => None,
            Promotion::PendingLiveEvidence { mapping } => Some(mapping),
        }
    }

    /// The normalized base URL bound into this row's HTTP profile.
    pub fn base(&self) -> &Url {
        &self.base
    }

    pub fn cost_expectation(&self) -> CostExpectation {
        self.cost_expectation
    }

    /// This row's namespace and key prefix in the third-party price table.
    pub fn price_table_name(&self) -> (&str, &str) {
        match self.price_table {
            Some(name) => (name.provider, name.key_prefix),
            None => (&self.id, &self.id),
        }
    }

    /// The hosted row.
    /// Construction is infallible because rows name credential variables without
    /// reading them. Missing keys must not make providers disappear.
    pub fn openai() -> Self {
        Self::openai_at(Url::parse(OPENAI_BASE).expect("a constant this file controls"))
    }

    /// The hosted row against a different address.
    ///
    /// The same pairing [`ollama`](Self::ollama) and
    /// [`ollama_default`](Self::ollama_default) already have, and for the same
    /// reason: the base is the caller's knowledge, the credential strategy
    /// and the id are the row's. Azure and the compatible gateways serve this
    /// wire format at their own hostnames, and a test serves it on loopback.
    pub fn openai_at(base: Url) -> Self {
        let (base, http_profile) = admitted_base("openai", base);
        ProviderRow {
            id: "openai".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::Positive,
            resolver: &OPENAI_SUPPLIED,
            credential: CredentialStrategy::BearerFromEnv {
                var: OPENAI_KEY_VAR.to_string(),
            },
            // OpenAI reports tokens, not a monetary total. This row uses supplied
            // rates when available and otherwise leaves cost unknown.
            cost: None,
            // Selectable on its published contract and deterministic offline fixtures.
            // These fixtures do not establish live provider acceptance.
            promotion: Promotion::Selectable,
        }
    }

    /// The local row, on its own mapping.
    ///
    /// Measured against a live Ollama 0.32.14 rather than assumed: it accepts
    /// the exact body [`wire::ollama`](crate::wire::ollama) builds and honours
    /// `stream_options.include_usage`.
    ///
    /// **"Accepts" is the whole claim, and it is narrower than it reads.** An
    /// earlier version of this comment added that Ollama "validates
    /// `reasoning_effort` against a strict superset of what `ThinkingLevel`
    /// can produce", and [`ollama::capabilities`](crate::ollama) reported
    /// `ReasoningEffortSupport::Any` on the strength of it. Both were true
    /// about acceptance and false about behaviour: `thinkFromReasoningEffort`
    /// clamps `minimal` to `low` and `xhigh` to `max` and answers `200`. The
    /// capability now names the five levels that survive the round trip.
    ///
    /// The general lesson is worth more than the fix: **a 200 proves the
    /// server took the request, never that it did what the request said.**
    /// Every remaining claim in this comment is an acceptance claim, so none
    /// of them may be read as a claim about honouring either — which is why
    /// this row owed a recorded transcript and a local smoke rather than a
    /// sentence. Both exist now: `tests/goldens/profile/ollama/` holds a
    /// request, two transcripts, an error and two usage frames taken from a
    /// live daemon, and `EGO_SMOKE=ollama` re-takes them.
    ///
    /// Ollama reads `max_tokens` rather than `max_completion_tokens`. Its own
    /// mapping shares the request projection and decoder but changes that field.
    /// The row selects the mapping; shared code does not branch on provider name.
    pub fn ollama(base: Url) -> Self {
        let (base, http_profile) = admitted_base("ollama", base);
        ProviderRow {
            id: "ollama".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::NoCharge,
            resolver: &OllamaDaemon,
            credential: CredentialStrategy::None,
            // A daemon serving its own weights charges nothing and states
            // nothing; the metering says so, not a cost field.
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    /// The local row at the default install's address.
    pub fn ollama_default() -> Self {
        Self::ollama(Url::parse(OLLAMA_BASE).expect("a constant this file controls"))
    }

    pub fn openai_codex_default() -> Self {
        let (base, http_profile) = admitted_base(
            "openai-codex",
            Url::parse(OPENAI_CODEX_BASE).expect("a constant this file controls"),
        );
        ProviderRow {
            id: "openai-codex".to_string(),
            base,
            http_profile,
            // The Codex backend reads `originator` and a client name; both
            // say ego, and the `user-agent` carries the build so a refusal can
            // be attributed to a version rather than to "some third party".
            extra_headers: vec![
                ("originator".to_string(), "ego".to_string()),
                (
                    "user-agent".to_string(),
                    crate::oauth::EGO_USER_AGENT.to_string(),
                ),
            ],
            price_table: None,
            cost_expectation: CostExpectation::NoCharge,
            resolver: &OPENAI_CODEX_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "openai-codex".to_string(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    pub fn github_copilot_default() -> Self {
        let (base, http_profile) = admitted_base(
            "github-copilot",
            Url::parse(GITHUB_COPILOT_BASE).expect("a constant this file controls"),
        );
        ProviderRow {
            id: "github-copilot".to_string(),
            base,
            http_profile,
            // Default headers do not impersonate the Copilot Chat extension. An operator
            // who accepts GitHub's terms can configure `[auth.github-copilot.headers]`;
            // the overlay replaces default identity headers by name.
            extra_headers: vec![(
                "user-agent".to_string(),
                crate::oauth::EGO_USER_AGENT.to_string(),
            )],
            price_table: None,
            cost_expectation: CostExpectation::NoCharge,
            resolver: &GITHUB_COPILOT_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "github-copilot".to_string(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    pub fn xai_oauth_default() -> Self {
        let (base, http_profile) = admitted_base(
            "xai-oauth",
            Url::parse(XAI_BASE).expect("a constant this file controls"),
        );
        ProviderRow {
            id: "xai-oauth".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::NoCharge,
            resolver: &XAI_OAUTH_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "xai-oauth".to_string(),
            },
            // Telemetry from the current Responses usage object. Resolution
            // still installs subscription metering, so this never invents or
            // settles a monetary subscription price.
            cost: Some(Arc::new(XaiUsdTicks::new(XAI_RESPONSES_READ_ON))),
            promotion: Promotion::Selectable,
        }
    }

    pub fn kimi_coding_default() -> Self {
        let (base, http_profile) = admitted_base(
            "kimi-coding",
            Url::parse(KIMI_CODING_BASE).expect("a constant this file controls"),
        );
        ProviderRow {
            id: "kimi-coding".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::NoCharge,
            resolver: &KimiManaged,
            credential: CredentialStrategy::OAuth {
                provider: "kimi-coding".to_string(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    pub fn minimax_oauth_default() -> Self {
        let (base, http_profile) = admitted_base(
            "minimax-oauth",
            Url::parse(MINIMAX_OAUTH_BASE).expect("a constant this file controls"),
        );
        ProviderRow {
            id: "minimax-oauth".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::NoCharge,
            resolver: &MINIMAX_OAUTH_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "minimax-oauth".to_string(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    pub fn nous_default() -> Self {
        let (base, http_profile) = admitted_base(
            "nous",
            Url::parse(NOUS_BASE).expect("a constant this file controls"),
        );
        ProviderRow {
            id: "nous".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::NoCharge,
            resolver: &NOUS_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "nous".to_string(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    /// The OpenRouter row: the family's mapping, and a charge it states itself.
    ///
    /// OpenRouter documents automatic full usage and treats `usage.include` and
    /// `stream_options.include_usage` as inert. The latter remains in this shared
    /// mapping because it is required by the mapping's originating contract.
    ///
    /// The public models endpoint supplies current inventory and stated limits.
    /// Prices are never imported; actual charges come only from response usage.
    pub fn openrouter(base: Url) -> Self {
        let (base, http_profile) = admitted_base("openrouter", base);
        ProviderRow {
            id: "openrouter".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::Positive,
            resolver: &OPENROUTER_MODELS,
            credential: CredentialStrategy::OAuthOrEnv {
                provider: "openrouter".to_string(),
                var: OPENROUTER_KEY_VAR.to_string(),
            },
            cost: Some(Arc::new(OpenRouterUsd::new(READ_ON))),
            promotion: Promotion::Selectable,
        }
    }

    /// The OpenRouter row at OpenRouter's own address.
    pub fn openrouter_default() -> Self {
        Self::openrouter(Url::parse(OPENROUTER_BASE).expect("a constant this file controls"))
    }

    /// The Anthropic row: its own mapping, and a header rather than a scheme.
    ///
    /// Selectable for both API keys and Claude subscription tokens. The
    /// credential prefix chooses their different wire presentations.
    ///
    /// The first row on a wire format that is not the family's, which is what
    /// it was added to prove: `anthropic-messages` and `openai-completions`
    /// share no request field, and the seam carries the difference without a
    /// per-provider branch on either side of it.
    ///
    /// `anthropic-version` is **not** here: it is a fact
    /// about the mapping's contract, not about this row's address or its
    /// credential, and [`wire::anthropic_messages`](crate::wire::anthropic_messages)
    /// owns it.
    pub fn anthropic(base: Url) -> Self {
        let (base, http_profile) = admitted_base("anthropic", base);
        ProviderRow {
            id: "anthropic".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::Positive,
            resolver: &ANTHROPIC_SUPPLIED,
            credential: CredentialStrategy::Anthropic {
                provider: "anthropic".to_string(),
                api_key_var: ANTHROPIC_KEY_VAR.to_string(),
                oauth_var: "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            },
            // Anthropic reports the billing dimensions — four token
            // categories, including the cache-write split this row is the
            // first to exercise — and no monetary total. See this field's own
            // doc: `None` is the assertion that none will be stated.
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    /// The Anthropic row at Anthropic's own address.
    pub fn anthropic_default() -> Self {
        Self::anthropic(Url::parse(ANTHROPIC_BASE).expect("a constant this file controls"))
    }

    /// The Gemini provider uses Interactions and supplied capabilities. Its mapping has no
    /// `thinking_level` spelling for `Off`, so that choice is refused rather than
    /// mistranslated. The named API-key header is represented by
    /// `CredentialStrategy::HeaderFromEnv`.
    pub fn gemini(base: Url) -> Self {
        let (base, http_profile) = admitted_base("gemini", base);
        ProviderRow {
            id: "gemini".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: None,
            cost_expectation: CostExpectation::Positive,
            resolver: &GEMINI_SUPPLIED,
            credential: CredentialStrategy::HeaderFromEnv {
                header: "x-goog-api-key".to_string(),
                var: GEMINI_KEY_VAR.to_string(),
            },
            // The stable v1 contract states usage dimensions and no monetary
            // figure. `None` is that schema-derived assertion, not an omission.
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    /// The Gemini row at Google's own address.
    pub fn gemini_default() -> Self {
        Self::gemini(Url::parse(GEMINI_BASE).expect("a constant this file controls"))
    }
}
