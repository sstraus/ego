//! A provider row: where to send a request, and what to authenticate it with.
//!
//! See `ARCHITECTURE.md` §3.1 and
//! `docs/decisions/2026-08-21-a-generator-that-actually-reaches-a-provider.md`.
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

use url::Url;

use crate::http::AdmittedHttpProfile;
use crate::resolver::{
    KimiManaged, ModelResolver, OllamaDaemon, SubscriptionSupplied, Supplied, XaiLanguageModels,
};
use crate::wire::anthropic_messages::AnthropicMessages;
use crate::wire::gemini_interactions::GeminiInteractions;
use crate::wire::github_copilot_completions::GithubCopilotCompletions;
use crate::wire::minimax_anthropic::MiniMaxAnthropic;
use crate::wire::nous_portal::NousPortal;
use crate::wire::openai_codex_responses::OpenAiCodexResponses;
use crate::wire::openai_completions::OpenAiCompletions;
use crate::wire::provider_cost::{OpenRouterUsd, ProviderCost, XaiUsdTicks};
use crate::wire::xai_responses::XaiResponses;

/// A credential. Prints as `<redacted>`, in every formatter, forever.
///
/// The redaction is on the secret and not on its container on purpose. A
/// `#[derive(Debug)]` on some future type that happens to hold one is the
/// shortest path to a key in a `tracing` field, and the milestone checklist
/// already carries "no credentials reachable from `PublicEvent`". A container
/// that forgets is harmless if the value refuses.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The credential itself. Private to this module: the only legitimate
    /// reader is [`ResolvedCredential::headers`], and a public accessor would
    /// make the redaction above a formality.
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Secret(value.to_string())
    }
}

/// How a provider presents a credential, and where the credential is read
/// from — the *strategy*, never the value.
///
/// `docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §3. A row used
/// to carry a resolved [`Secret`], which was not readable outside this module
/// but was one `derive` away from being — and which made a row *impossible to
/// build* without a credential, so a missing key made a provider vanish and
/// `model.rs` had to name one provider by hand to report the true error.
/// Naming the variable instead means every provider has a row whether or not
/// anyone has configured it, and the refusal happens where it can be acted on.
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
    /// **Once**, and that is the whole of
    /// `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §6. The Step 13.5 ADR weighed resolving *in the row* against resolving
    /// *per request* and rejected the second, correctly: it makes every
    /// generation fallible for a condition decided at startup, and it would
    /// change behaviour mid-run if the variable were rotated. There was a
    /// third option it did not consider — resolve once, **outside** the row —
    /// and this is it. The row keeps a strategy with nothing in it to leak,
    /// and exactly one [`ResolvedCredential`] exists, held by the one
    /// generator that must send it.
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
            CredentialStrategy::BearerFromEnv { var } => (
                vec![(
                    "authorization".to_string(),
                    Secret::from(format!("Bearer {}", creds.secret(var)?.expose())),
                )],
                None,
            ),
            CredentialStrategy::HeaderFromEnv { header, var } => {
                (vec![(header.clone(), creds.secret(var)?)], None)
            }
            CredentialStrategy::OAuth { provider } => {
                let secret =
                    creds
                        .secret(&format!("oauth:{provider}"))
                        .map_err(|_| MissingCredential {
                            var: format!("ego auth login {provider}"),
                        })?;
                let base_override = crate::oauth::provider_base(provider, secret.expose())
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
}

fn oauth_headers(
    provider: &str,
    secret: &Secret,
) -> Result<Vec<(String, Secret)>, MissingCredential> {
    crate::oauth::provider_auth(provider, secret.expose())
        .map_err(|_| MissingCredential {
            var: format!("ego auth login {provider}"),
        })
        .map(|auth| {
            auth.headers()
                .into_iter()
                .map(|(name, value)| (name, Secret::from(value)))
                .collect()
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
/// `Debug` is derived and safe because [`Secret`] refuses to print — the belt
/// the profile ADR §3 argues for, still fastened now that the braces are
/// structural.
#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    headers: Vec<(String, Secret)>,
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
    /// This is the one place the secret is exposed, and it is exposed into a
    /// value the caller is about to put in a header.
    pub fn headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(name, secret)| (name.clone(), secret.expose().to_string()))
            .collect()
    }

    pub fn base_override(&self) -> Option<&Url> {
        self.base_override.as_ref()
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
            return write!(f, "no stored subscription credential: run `{}`", self.var);
        }
        write!(
            f,
            "no credential in {}: set it to the provider's API key",
            self.var
        )
    }
}

impl std::error::Error for MissingCredential {}

/// One provider profile: who it is, where it lives, how it authenticates,
/// which shape its bytes take, and whether it states a charge.
///
/// `docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §2. Everything
/// here is configuration that varies per deployment, per key or per account;
/// the code that varies per API shape is behind [`wire`](Self::wire) and
/// nowhere else.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    id: String,
    /// Scheme, host and any prefix — and **no path this row owns**.
    ///
    /// A base and not a full endpoint, because a path is a fact about the API
    /// shape rather than about a vendor: every provider speaking a given
    /// mapping serves it at that mapping's own path, which is why `PATH` is
    /// declared beside the mapping and joined on by
    /// [`RequestTarget::url`](crate::wire::RequestTarget::url). A row carrying
    /// the whole endpoint would be a row that could disagree with its own
    /// mapping about where the mapping lives
    /// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §2).
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
    /// The distinction it carries is real and not a tidy-up:
    /// `2026-08-21-what-a-model-resolves-to.md` argues that a hosted model's
    /// ceilings and prices are facts nobody can discover, and a local model's
    /// are facts nobody can write down.
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
}

/// The base OpenAI serves its API under.
pub const OPENAI_BASE: &str = "https://api.openai.com/v1/";
/// Codex subscription backend (Pi's `openai-codex` base, path joined by the mapping).
pub const OPENAI_CODEX_BASE: &str = "https://chatgpt.com/backend-api/";
/// The environment variable OpenAI's credential is read from.
pub const OPENAI_KEY_VAR: &str = "OPENAI_API_KEY";
/// The base a default Ollama install serves its openai-compatible API under.
pub const OLLAMA_BASE: &str = "http://localhost:11434/v1/";
/// The base xAI serves its API under.
pub const XAI_BASE: &str = "https://api.x.ai/v1/";
/// The environment variable xAI's credential is read from.
pub const XAI_KEY_VAR: &str = "XAI_API_KEY";
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
pub const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/";
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

static OPENAI_SUPPLIED: Supplied = Supplied::new(&OpenAiCompletions);
static ANTHROPIC_SUPPLIED: Supplied = Supplied::new(&AnthropicMessages);
static GEMINI_SUPPLIED: Supplied = Supplied::new(&GeminiInteractions);
static XAI_LANGUAGE_MODELS: XaiLanguageModels = XaiLanguageModels::new(&OpenAiCompletions);
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

    /// The normalized base URL bound into this row's HTTP profile.
    pub fn base(&self) -> &Url {
        &self.base
    }

    /// The hosted row.
    ///
    /// Infallible, and that is the change the profile ADR's §3 argues for: a
    /// row names the variable its credential lives in and never reads it, so
    /// the row exists whether or not anyone has configured a key. When it was
    /// fallible a missing key made the provider *disappear*, and the resolver
    /// had to special-case one provider by name to report the true error.
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
            resolver: &OPENAI_SUPPLIED,
            credential: CredentialStrategy::BearerFromEnv {
                var: OPENAI_KEY_VAR.to_string(),
            },
            // OpenAI reports token usage and no monetary total
            // (`docs/decisions/2026-08-21-how-a-model-is-metered.md` §6), so
            // this row is priced from a supplied table or not at all.
            cost: None,
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
    /// It is not the hosted row with a different address. The same
    /// measurement discipline found the one place the daemon's answers differ
    /// from the family's — it reads `max_tokens` and discards
    /// `max_completion_tokens` — and
    /// `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §8 spends that fact rather than excusing it: a member that fails a
    /// conformance check is not a member. So this row names its own mapping,
    /// which shares the family's request projection and its whole decoder and
    /// differs in one field.
    pub fn ollama(base: Url) -> Self {
        let (base, http_profile) = admitted_base("ollama", base);
        ProviderRow {
            id: "ollama".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            resolver: &OllamaDaemon,
            credential: CredentialStrategy::None,
            // A daemon serving its own weights charges nothing and states
            // nothing; the metering says so, not a cost field.
            cost: None,
        }
    }

    /// The local row at the default install's address.
    pub fn ollama_default() -> Self {
        Self::ollama(Url::parse(OLLAMA_BASE).expect("a constant this file controls"))
    }

    /// The old API-key xAI candidate: legacy Chat Completions, never selectable.
    ///
    /// **A candidate, not a member.**
    /// `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §7 records that xAI has since deprecated this endpoint. This constructor
    /// remains solely for the historical candidate audit and price-decoder
    /// evidence. It is deliberately absent from the table
    /// `crates/ego-cli/src/model.rs` builds, so `--model xai/...` does not
    /// resolve; a live recording cannot promote a deprecated contract. The
    /// selectable `xai-oauth` row below owns the current Responses mapping and
    /// this constructor is not its fallback.
    ///
    /// [`XaiLanguageModels`] is what makes this row different from every other
    /// family member: alone among the six, xAI publishes what it will charge
    /// the key that asks.
    pub fn xai(base: Url) -> Self {
        let (base, http_profile) = admitted_base("xai", base);
        ProviderRow {
            id: "xai".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            resolver: &XAI_LANGUAGE_MODELS,
            credential: CredentialStrategy::BearerFromEnv {
                var: XAI_KEY_VAR.to_string(),
            },
            cost: Some(Arc::new(XaiUsdTicks::new(READ_ON))),
        }
    }

    /// The xAI row at xAI's own address.
    pub fn xai_default() -> Self {
        Self::xai(Url::parse(XAI_BASE).expect("a constant this file controls"))
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
            extra_headers: vec![
                ("originator".to_string(), "ego".to_string()),
                ("user-agent".to_string(), "ego".to_string()),
            ],
            resolver: &OPENAI_CODEX_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "openai-codex".to_string(),
            },
            cost: None,
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
            extra_headers: vec![
                (
                    "user-agent".to_string(),
                    "GitHubCopilotChat/0.35.0".to_string(),
                ),
                ("editor-version".to_string(), "vscode/1.107.0".to_string()),
                (
                    "editor-plugin-version".to_string(),
                    "copilot-chat/0.35.0".to_string(),
                ),
                (
                    "copilot-integration-id".to_string(),
                    "vscode-chat".to_string(),
                ),
            ],
            resolver: &GITHUB_COPILOT_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "github-copilot".to_string(),
            },
            cost: None,
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
            resolver: &XAI_OAUTH_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "xai-oauth".to_string(),
            },
            // Telemetry from the current Responses usage object. Resolution
            // still installs subscription metering, so this never invents or
            // settles a monetary subscription price.
            cost: Some(Arc::new(XaiUsdTicks::new(XAI_RESPONSES_READ_ON))),
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
            resolver: &KimiManaged,
            credential: CredentialStrategy::OAuth {
                provider: "kimi-coding".to_string(),
            },
            cost: None,
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
            resolver: &MINIMAX_OAUTH_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "minimax-oauth".to_string(),
            },
            cost: None,
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
            resolver: &NOUS_SUBSCRIPTION,
            credential: CredentialStrategy::OAuth {
                provider: "nous".to_string(),
            },
            cost: None,
        }
    }

    /// The OpenRouter row: the family's mapping, and a charge it states itself.
    ///
    /// The subscription-login decision promotes this row for both an
    /// environment key and a key minted by built-in PKCE.
    /// The ADR's §7 flagged an open question about conformance check 1 that xAI
    /// does not raise: whether `usage.cost` arrives automatically or requires
    /// `usage: {include: true}` in the request body — because if it requires
    /// one, the family's request projection does not send it and this row is
    /// not a family member at all but a fifth mapping. OpenRouter's own
    /// usage-accounting page now answers it: that field **and** the
    /// `stream_options.include_usage` this family already sends are
    /// "deprecated and have no effect", with full usage returned automatically
    /// in every response. So the projection is compatible, and the inert flag
    /// stays — it is current on the contract the mapping is derived from
    /// (`docs/15-provider-contract-audit.md` §3).
    ///
    /// [`Supplied`] and not a discovering resolver: OpenRouter's models
    /// endpoint publishes a *public* table rather than what this key is
    /// charged, which is the property that earned xAI a resolver. A public
    /// table read live is a third-party table with extra steps, and §5 ranks
    /// it below the operator's own figures either way.
    pub fn openrouter(base: Url) -> Self {
        let (base, http_profile) = admitted_base("openrouter", base);
        ProviderRow {
            id: "openrouter".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            resolver: &OPENAI_SUPPLIED,
            credential: CredentialStrategy::OAuthOrEnv {
                provider: "openrouter".to_string(),
                var: OPENROUTER_KEY_VAR.to_string(),
            },
            cost: Some(Arc::new(OpenRouterUsd::new(READ_ON))),
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
        }
    }

    /// The Anthropic row at Anthropic's own address.
    pub fn anthropic_default() -> Self {
        Self::anthropic(Url::parse(ANTHROPIC_BASE).expect("a constant this file controls"))
    }

    /// The Gemini row, on the Interactions API.
    ///
    /// **A candidate, not a member**, for the same §9 reason as the other
    /// three: absent from `crates/ego-cli/src/model.rs`'s table, so
    /// `--model gemini/…` does not resolve.
    ///
    /// The third wire format, and the one that turns two into a seam. It also
    /// carries the one cost `docs/14` §3.6 accepted with its eyes open:
    /// `thinking_level` has no `off`, so a row on this mapping cannot offer
    /// every level ego names and `ThinkingLevel::Off` is refused at the
    /// mapping rather than mistranslated.
    ///
    /// `x-goog-api-key`, like Anthropic's `x-api-key`, is a header the
    /// provider names rather than a scheme — which is why
    /// [`CredentialStrategy::HeaderFromEnv`] is data and not a second enum.
    pub fn gemini(base: Url) -> Self {
        let (base, http_profile) = admitted_base("gemini", base);
        ProviderRow {
            id: "gemini".to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            resolver: &GEMINI_SUPPLIED,
            credential: CredentialStrategy::HeaderFromEnv {
                header: "x-goog-api-key".to_string(),
                var: GEMINI_KEY_VAR.to_string(),
            },
            // "No monetary figure, ever" — `docs/14` §3.3, confirmed against a
            // recorded fixture. `None` is that assertion, not an omission.
            cost: None,
        }
    }

    /// The Gemini row at Google's own address.
    pub fn gemini_default() -> Self {
        Self::gemini(Url::parse(GEMINI_BASE).expect("a constant this file controls"))
    }
}
