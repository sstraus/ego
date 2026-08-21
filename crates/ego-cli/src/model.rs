//! Turning `--model P/M` into something a turn can be run against.
//!
//! The split is `ego-ai`'s and this module does not reopen it: a hosted
//! model's price and ceilings are *supplied*, a local daemon's are
//! *discovered*. What lives here is the part that needs the process — reading
//! the environment for a credential and for a daemon address, and choosing
//! which of the two resolvers to ask.

use std::sync::Arc;

use ego_ai::catalogue::{self, Catalogue, ModelSlug, ResolveError, ResolvedModel};
use ego_ai::http::{EgressPolicy, Http, ReqwestHttp};
use ego_ai::ollama;
use ego_ai::provider::{Credentials, OLLAMA_ENDPOINT, OPENAI_ENDPOINT, ProviderRow};
use url::Url;

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

/// The chat-completions endpoint an address variable names, or the default.
///
/// `OLLAMA_HOST` is conventionally a bare `host:port` and `OPENAI_BASE_URL` is
/// conventionally a base URL without the path; both spellings are accepted for
/// both, and anything else is refused rather than half-parsed into a request to
/// somewhere unintended. The path is always set, so a variable that already
/// carries one and a variable that does not resolve to the same address.
fn endpoint(var: &str, fallback: &str) -> Result<Url, ResolveError> {
    let Ok(given) = std::env::var(var) else {
        return Ok(Url::parse(fallback).expect("a constant this file controls"));
    };
    let given = given.trim();
    let base = if given.contains("://") {
        given.to_string()
    } else {
        format!("http://{given}")
    };
    let mut url = Url::parse(&base).map_err(|err| ResolveError::DaemonUnreachable {
        endpoint: given.to_string(),
        message: format!("{var} is not an address: {err}"),
    })?;
    url.set_path("/v1/chat/completions");
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
        .map_err(|err| ResolveError::DaemonUnreachable {
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
fn rows() -> Result<Vec<ProviderRow>, ResolveError> {
    Ok(vec![
        ProviderRow::ollama(endpoint(OLLAMA_HOST, OLLAMA_ENDPOINT)?),
        ProviderRow::openai_at(endpoint(OPENAI_BASE_URL, OPENAI_ENDPOINT)?),
    ])
}

/// Resolve `slug` against the daemon or against what was supplied.
///
/// `hosted` comes from [`facts::supplied`] and is passed in rather than read
/// here, so that a `models.toml` which cannot be parsed is reported as the file
/// error it is. Folding it into [`ResolveError`] would make "your TOML has a
/// stray bracket" arrive as a statement about a model.
///
/// `creds` is passed in for the same reason and one more: it is the *same*
/// source the generator will read from when it assembles a request, so the
/// check below and the header built later cannot disagree about whether a key
/// exists.
pub async fn resolve(
    slug: &str,
    hosted: Vec<(String, facts::Hosted)>,
    http: &Arc<dyn Http>,
    creds: &dyn Credentials,
) -> Result<ResolvedModel, ResolveError> {
    let slug = ModelSlug::parse(slug)?;
    let mut catalogue = Catalogue::new(rows()?);
    for (supplied, model) in hosted {
        catalogue = catalogue.with_hosted(&supplied, model);
    }
    let row = catalogue.row(&slug.provider)?.clone();
    // Refused here, where the operator can act on it, rather than as a 401
    // several layers later. No per-provider branch: the row names its own
    // variable, so this one line covers every provider that has one and says
    // nothing about the ones that do not.
    row.auth.ensure_available(creds)?;
    if slug.provider == catalogue::OLLAMA {
        return ollama::resolve(&row, &slug.model, http).await;
    }
    catalogue.resolve_hosted(&slug)
}

/// Whether a resolution failure is the network's fault or the caller's.
///
/// `CLI.md` §4.3 gives them different exit codes on purpose — a script retries
/// a 4 and never retries a 2 — and this is the only place the distinction is
/// drawn. A model that is not installed is a 2 however long the daemon took to
/// say so.
pub fn is_provider_failure(err: &ResolveError) -> bool {
    matches!(err, ResolveError::DaemonUnreachable { .. })
}
