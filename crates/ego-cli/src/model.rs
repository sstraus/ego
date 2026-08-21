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
use ego_ai::provider::{ProviderRow, OLLAMA_ENDPOINT};
use ego_core::cost::CurrencyCode;
use url::Url;

/// The environment variable Ollama's own client reads to find the daemon.
///
/// Honoured rather than reinvented under an `EGO_` name: a machine that has
/// moved its daemon has already set this, and a second variable meaning the
/// same thing is a second thing to get wrong. It is also what lets a test
/// point the binary at a loopback stub without a network.
pub const OLLAMA_HOST: &str = "OLLAMA_HOST";

/// Where the local daemon serves its openai-compatible endpoint.
///
/// `OLLAMA_HOST` is conventionally a bare `host:port`, and is also written as
/// a full URL; both are accepted, and anything else is refused rather than
/// half-parsed into a request to somewhere unintended.
fn ollama_endpoint() -> Result<Url, ResolveError> {
    let Ok(host) = std::env::var(OLLAMA_HOST) else {
        return Ok(Url::parse(OLLAMA_ENDPOINT).expect("a constant this file controls"));
    };
    let host = host.trim();
    let base = if host.contains("://") {
        host.to_string()
    } else {
        format!("http://{host}")
    };
    let mut url = Url::parse(&base).map_err(|err| ResolveError::DaemonUnreachable {
        endpoint: host.to_string(),
        message: format!("{OLLAMA_HOST} is not an address: {err}"),
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
/// The hosted row is only added when its credential is present. That is not
/// leniency: `Auth::bearer_from_env` refuses without one, and a catalogue
/// built eagerly would make *every* invocation fail for want of a key, even
/// `--model ollama/qwen3`.
fn rows() -> Result<Vec<ProviderRow>, ResolveError> {
    let mut rows = vec![ProviderRow::ollama(ollama_endpoint()?)];
    if let Ok(openai) = ProviderRow::openai() {
        rows.push(openai);
    }
    Ok(rows)
}

/// Resolve `slug` against the daemon or against what was supplied.
pub async fn resolve(
    slug: &str,
    currency: CurrencyCode,
    http: &Arc<dyn Http>,
) -> Result<ResolvedModel, ResolveError> {
    let slug = ModelSlug::parse(slug)?;
    let catalogue = Catalogue::new(rows()?);
    if slug.provider == catalogue::OLLAMA {
        let row = catalogue.row(&slug.provider)?.clone();
        return ollama::resolve(&row, &slug.model, currency, http).await;
    }
    // The hosted path. `row` is consulted first so that a missing credential
    // is reported as a missing credential — the thing an operator can fix —
    // rather than as "no facts are configured for that model".
    if slug.provider == catalogue::OPENAI && catalogue.row(&slug.provider).is_err() {
        return Err(ResolveError::MissingCredential(
            ProviderRow::openai().expect_err("the row is absent, so the credential is"),
        ));
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
