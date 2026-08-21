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

use crate::wire::WireFormat;
use crate::wire::ollama::Ollama;
use crate::wire::openai_completions::OpenAiCompletions;
use crate::wire::provider_cost::ProviderCost;

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
    /// reader is [`ProviderRow::headers`], and a public accessor would make
    /// the redaction above a formality.
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

/// How a provider authenticates a request — the *strategy*, never the value.
///
/// `docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §3. A row used
/// to carry a resolved [`Secret`], which was not readable outside this module
/// but was one `derive` away from being — and which made a row *impossible to
/// build* without a credential, so a missing key made a provider vanish and
/// `model.rs` had to name one provider by hand to report the true error.
/// Naming the variable instead means every provider has a row whether or not
/// anyone has configured it, and the refusal happens where it can be acted on.
#[derive(Debug, Clone)]
pub enum Auth {
    /// None at all — a local daemon that does not authenticate. A distinct
    /// variant rather than an empty variable name, so "this provider needs no
    /// key" and "the key is missing" cannot be confused.
    None,
    /// `Authorization: Bearer <the value of this variable>`.
    BearerFromEnv { var: String },
}

impl Auth {
    /// The credential this strategy needs, resolved now and returned to a
    /// caller that is about to put it in a header and drop it.
    ///
    /// `None` means the strategy needs no credential, which is not the same
    /// answer as an error and must never collapse into one.
    fn secret(&self, creds: &dyn Credentials) -> Result<Option<Secret>, MissingCredential> {
        match self {
            Auth::None => Ok(None),
            Auth::BearerFromEnv { var } => creds.bearer(var).map(Some),
        }
    }

    /// Refuse now if the credential this strategy names is not there.
    ///
    /// Called at resolution, so "no key configured" is a refusal naming the
    /// variable to set rather than a 401 from the provider several layers
    /// later — by which point the request has been built, sent, and billed to
    /// somebody's rate limit. It returns nothing on purpose: the value is
    /// read again when a request is assembled and is never stored in between.
    pub fn ensure_available(&self, creds: &dyn Credentials) -> Result<(), MissingCredential> {
        self.secret(creds).map(|_| ())
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
    /// The bearer token in `var`, or a refusal naming it.
    fn bearer(&self, var: &str) -> Result<Secret, MissingCredential>;
}

/// The process environment: the only [`Credentials`] a real run uses.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvCredentials;

impl Credentials for EnvCredentials {
    fn bearer(&self, var: &str) -> Result<Secret, MissingCredential> {
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
    pub endpoint: Url,
    pub auth: Auth,
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

/// Where OpenAI's chat-completions endpoint lives.
pub const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
/// The environment variable OpenAI's credential is read from.
pub const OPENAI_KEY_VAR: &str = "OPENAI_API_KEY";
/// Where a default Ollama install serves its openai-compatible endpoint.
pub const OLLAMA_ENDPOINT: &str = "http://localhost:11434/v1/chat/completions";

impl ProviderRow {
    /// The hosted row.
    ///
    /// Infallible, and that is the change the profile ADR's §3 argues for: a
    /// row names the variable its credential lives in and never reads it, so
    /// the row exists whether or not anyone has configured a key. When it was
    /// fallible a missing key made the provider *disappear*, and the resolver
    /// had to special-case one provider by name to report the true error.
    pub fn openai() -> Self {
        Self::openai_at(Url::parse(OPENAI_ENDPOINT).expect("a constant this file controls"))
    }

    /// The hosted row against a different address.
    ///
    /// The same pairing [`ollama`](Self::ollama) and
    /// [`ollama_default`](Self::ollama_default) already have, and for the same
    /// reason: the endpoint is the caller's knowledge, the credential strategy
    /// and the id are the row's. Azure and the compatible gateways serve this
    /// wire format at their own hostnames, and a test serves it on loopback.
    pub fn openai_at(endpoint: Url) -> Self {
        ProviderRow {
            id: "openai".to_string(),
            endpoint,
            wire: &OpenAiCompletions,
            auth: Auth::BearerFromEnv {
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
    /// this row still owes a recorded transcript and a local smoke rather
    /// than a sentence.
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
    pub fn ollama(endpoint: Url) -> Self {
        ProviderRow {
            id: "ollama".to_string(),
            endpoint,
            wire: &Ollama,
            auth: Auth::None,
            // A daemon serving its own weights charges nothing and states
            // nothing; the metering says so, not a cost field.
            cost: None,
        }
    }

    /// The local row at the default install's address.
    pub fn ollama_default() -> Self {
        Self::ollama(Url::parse(OLLAMA_ENDPOINT).expect("a constant this file controls"))
    }

    /// The headers this row contributes to a request.
    ///
    /// Only the credential. Content type and accept belong to the wire
    /// format, which adds them itself (`openai_completions::build_request`),
    /// because they are facts about the body — not about the provider.
    ///
    /// The credential is resolved *here*, per request, and the [`Secret`] it
    /// comes back in lives exactly as long as the `format!` below. Nothing
    /// holds it: that is what "no secret is reachable from `ProviderRow`"
    /// means now that it is a claim about what the type holds rather than
    /// about who can call an accessor.
    pub fn headers(
        &self,
        creds: &dyn Credentials,
    ) -> Result<Vec<(String, String)>, MissingCredential> {
        Ok(match self.auth.secret(creds)? {
            None => Vec::new(),
            Some(secret) => vec![(
                "authorization".to_string(),
                format!("Bearer {}", secret.expose()),
            )],
        })
    }
}
