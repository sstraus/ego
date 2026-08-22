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

use crate::resolver::{ModelResolver, OllamaDaemon, Supplied, XaiLanguageModels};
use crate::wire::WireFormat;
use crate::wire::ollama::Ollama;
use crate::wire::openai_completions::OpenAiCompletions;
use crate::wire::provider_cost::{OpenRouterUsd, ProviderCost, XaiUsdTicks};

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
        let header = match self {
            // Not an error and not an empty header: a provider that
            // authenticates nothing is a different fact from a key that is
            // missing, and collapsing them would make a misconfigured hosted
            // row look like a local daemon.
            CredentialStrategy::None => None,
            CredentialStrategy::BearerFromEnv { var } => Some((
                "authorization".to_string(),
                Secret::from(format!("Bearer {}", creds.secret(var)?.expose())),
            )),
            CredentialStrategy::HeaderFromEnv { header, var } => {
                Some((header.clone(), creds.secret(var)?))
            }
        };
        Ok(ResolvedCredential { header })
    }
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
    header: Option<(String, Secret)>,
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
        match &self.header {
            None => Vec::new(),
            Some((name, secret)) => vec![(name.clone(), secret.expose().to_string())],
        }
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
    pub id: String,
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
    pub base: Url,
    pub credential: CredentialStrategy,
    /// Which API shape this provider speaks.
    ///
    /// A `&'static dyn` and not an `Arc<dyn>` like [`cost`](Self::cost),
    /// because the two are different kinds of thing. A cost decoder is chosen
    /// per row and could in principle be built from configuration; a mapping
    /// is code, every value of it is a unit struct with no state, and there
    /// are exactly as many as this crate compiles. A `&'static` says that.
    ///
    /// This is the field `wire/mod.rs` used to argue against: a row that named
    /// a format it could not select would have been a `WireFormat` trait in
    /// disguise. It can select one now — that is precisely what the ADR
    /// unblocked — so the field is the trait in the open.
    pub wire: &'static dyn WireFormat,
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

/// Which chat-completions schema the cost decoders were written against.
///
/// One string for both because it is one fact: the date this repository read
/// the two response schemas and wrote decoders for the fields they document. A
/// `ProviderCostRevision` exists to make a decoded figure attributable, and
/// attributing it to a version number neither provider publishes would be
/// worse than attributing it to the day somebody looked.
const READ_ON: &str = "2026-08-21";

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

impl ProviderRow {
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
        ProviderRow {
            id: "openai".to_string(),
            base: as_base(base),
            wire: &OpenAiCompletions,
            resolver: &Supplied,
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
        ProviderRow {
            id: "ollama".to_string(),
            base: as_base(base),
            wire: &Ollama,
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

    /// The xAI row: the family's mapping, and a price asked rather than
    /// written down.
    ///
    /// **A candidate, not a member.**
    /// `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §7 puts xAI on `openai-completions` from its published schema, and §9
    /// is explicit that a provider whose conformance is read rather than
    /// measured is not runnable: this row is deliberately absent from the
    /// table `crates/ego-cli/src/model.rs` builds, so `--model xai/...` does
    /// not resolve. It exists so the offline evidence — a schema-derived
    /// conformance profile, and the price decoder's tests — has something to
    /// be evidence *about*, and it becomes selectable when a live recording
    /// exists.
    ///
    /// [`XaiLanguageModels`] is what makes this row different from every other
    /// family member: alone among the six, xAI publishes what it will charge
    /// the key that asks.
    pub fn xai(base: Url) -> Self {
        ProviderRow {
            id: "xai".to_string(),
            base: as_base(base),
            wire: &OpenAiCompletions,
            resolver: &XaiLanguageModels,
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

    /// The OpenRouter row: the family's mapping, and a charge it states itself.
    ///
    /// **A candidate, and the question that made it a sharp one is answered.**
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
    /// That is a document read, which is what `SchemaDerived` means, so the
    /// row is still built, tested offline and unreachable from `--model`. What
    /// changed is which gap remains: the ordinary schema-derived one every
    /// candidate has, rather than a question about membership.
    ///
    /// [`Supplied`] and not a discovering resolver: OpenRouter's models
    /// endpoint publishes a *public* table rather than what this key is
    /// charged, which is the property that earned xAI a resolver. A public
    /// table read live is a third-party table with extra steps, and §5 ranks
    /// it below the operator's own figures either way.
    pub fn openrouter(base: Url) -> Self {
        ProviderRow {
            id: "openrouter".to_string(),
            base: as_base(base),
            wire: &OpenAiCompletions,
            resolver: &Supplied,
            credential: CredentialStrategy::BearerFromEnv {
                var: OPENROUTER_KEY_VAR.to_string(),
            },
            cost: Some(Arc::new(OpenRouterUsd::new(READ_ON))),
        }
    }

    /// The OpenRouter row at OpenRouter's own address.
    pub fn openrouter_default() -> Self {
        Self::openrouter(Url::parse(OPENROUTER_BASE).expect("a constant this file controls"))
    }
}
