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

/// How a provider authenticates a request.
#[derive(Debug, Clone)]
pub enum Auth {
    /// None at all — a local daemon that does not authenticate. A distinct
    /// variant rather than an empty [`Secret`], so "this provider needs no
    /// key" and "the key is missing" cannot be confused.
    None,
    /// `Authorization: Bearer <token>`.
    Bearer(Secret),
}

impl Auth {
    /// Read a bearer token from the environment.
    ///
    /// Fallible, and separate from building the row, so that "no key
    /// configured" is a refusal naming the variable to set rather than a 401
    /// from the provider several layers later — by which point the request
    /// has been built, sent, and billed to somebody's rate limit.
    pub fn bearer_from_env(var: &str) -> Result<Self, MissingCredential> {
        match std::env::var(var) {
            Ok(value) if !value.trim().is_empty() => Ok(Auth::Bearer(value.into())),
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

/// One provider: an id, an endpoint, and how to authenticate against it.
///
/// No wire format field. `wire/mod.rs` states why there is no `WireFormat`
/// trait yet — extracting one from a single implementation means inventing
/// the second implementation's requirements from imagination — and a row that
/// named a format it could not select would be that trait in disguise.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub id: String,
    pub endpoint: Url,
    pub auth: Auth,
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
    /// The credential is read here rather than per request, so "no key
    /// configured" is a refusal naming the variable to set instead of a 401
    /// several layers later — by which point the request has been built,
    /// sent, and charged to somebody's rate limit.
    pub fn openai() -> Result<Self, MissingCredential> {
        Self::openai_at(Url::parse(OPENAI_ENDPOINT).expect("a constant this file controls"))
    }

    /// The hosted row against a different address.
    ///
    /// The same pairing [`ollama`](Self::ollama) and
    /// [`ollama_default`](Self::ollama_default) already have, and for the same
    /// reason: the endpoint is the caller's knowledge, the credential and the
    /// id are the row's. Azure and the compatible gateways serve this wire
    /// format at their own hostnames, and a test serves it on loopback.
    pub fn openai_at(endpoint: Url) -> Result<Self, MissingCredential> {
        Ok(ProviderRow {
            id: "openai".to_string(),
            endpoint,
            auth: Auth::bearer_from_env(OPENAI_KEY_VAR)?,
            // OpenAI reports token usage and no monetary total
            // (`docs/decisions/2026-08-21-how-a-model-is-metered.md` §6), so
            // this row is priced from a supplied table or not at all.
            cost: None,
        })
    }

    /// The local row, on the same wire format as the hosted one.
    ///
    /// Measured against a live Ollama 0.32.14 rather than assumed: it accepts
    /// the exact body `openai_completions` builds and honours
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
    /// This row differs from the hosted one only in endpoint and [`Auth`].
    pub fn ollama(endpoint: Url) -> Self {
        ProviderRow {
            id: "ollama".to_string(),
            endpoint,
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
    pub fn headers(&self) -> Vec<(String, String)> {
        match &self.auth {
            Auth::None => Vec::new(),
            Auth::Bearer(secret) => vec![(
                "authorization".to_string(),
                format!("Bearer {}", secret.expose()),
            )],
        }
    }
}
