//! Ollama's OpenAI-compatible endpoint, as its own mapping.
//!
//! It looks like [`openai_completions`](super::openai_completions) and it is
//! not that format.
//! `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §8
//! settles the question from a measurement rather than from the resemblance:
//! a live daemon honours `max_tokens` and discards `max_completion_tokens`, so
//! Ollama fails the family's first conformance check and is not a member of
//! it. §4 then leaves exactly three places an adaptation may live — a model
//! fact, a transport fact, or a different mapping — and "no fourth option and
//! no flag". A ceiling is none of the first two, so it is this file.
//!
//! What this mapping does *not* do is duplicate the family. The request body
//! is [`openai_completions::common_request`] plus the one field that differs;
//! the response side is [`openai_completions::decode`] verbatim, because the
//! measurements say the responses are the same shape: the recordings under
//! `crates/ego-ai/tests/goldens/ollama_*.sse` decode with the family's decoder
//! and always have. A mapping is a claim about a wire, and the claim here is
//! narrow: one request field differs, nothing in the response does.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::GenRequest;
use futures::stream::BoxStream;
use serde::Serialize;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::openai_completions::{self, CommonRequest, Inexpressible};
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat};

/// The `ollama` mapping, as a value a profile can name.
///
/// A unit struct for the same reason
/// [`OpenAiCompletions`](openai_completions::OpenAiCompletions) is one: the
/// address and the credential are the profile's, so a mapping has no
/// per-provider state and therefore nowhere to keep a per-provider branch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Ollama;

impl WireFormat for Ollama {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        build_request(target, req).map_err(RequestRefused::from)
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        openai_completions::decode(response, cost)
    }
}

/// The path this mapping joins onto a row's base URL.
///
/// The same path the family serves, and stated here anyway: it is Ollama's
/// compatibility surface that happens to agree, not a constant inherited from
/// a format this daemon does not implement.
pub const PATH: &str = "chat/completions";

/// The body this mapping sends: the family's common part, plus the ceiling in
/// the field this daemon actually reads.
///
/// `max_tokens` is deprecated at OpenAI and is not deprecated here, because
/// here it is the only one there is. Recorded in
/// `crates/ego-ai/tests/goldens/ollama_ceiling_*`: the run that sent
/// `max_completion_tokens` overran the ceiling; the run that sent `max_tokens`
/// stopped at it with `finish_reason: "length"`.
#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    #[serde(flatten)]
    common: CommonRequest<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
}

/// The exact JSON bytes this mapping would send for `req`.
///
/// Separate from [`build_request`] for the same reason its counterpart is: the
/// body is the contract with the daemon, and pinning it needs no URL.
pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    let wire = WireRequest {
        common: openai_completions::common_request(req)?,
        max_tokens: req.max_output_tokens,
    };
    Ok(serde_json::to_vec(&wire).expect("WireRequest has no non-string map keys and no NaN"))
}

/// A complete HTTP request for `req` against `target`.
///
/// The headers and the path are the family's, because the difference this
/// mapping exists for is in the body and nowhere else.
pub fn build_request(
    target: &RequestTarget<'_>,
    req: &GenRequest,
) -> Result<HttpRequest, Inexpressible> {
    Ok(target.post_json(PATH, request_body(req)?))
}
