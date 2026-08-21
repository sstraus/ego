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
//! There is deliberately no provider *table* here yet. Which row ships first
//! is `plans/m0b-vertical-slice.md` Open Question 3 and is maintainer's to answer;
//! the shape below holds either way, and inventing a table before the answer
//! would be inventing the answer.

use std::fmt;

use url::Url;

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
}

impl ProviderRow {
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
