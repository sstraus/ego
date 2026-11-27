//! Explicit text/tool Chat contract with a `max_tokens` output ceiling.
//!
//! The reviewed payload and tool-message schemas establish this subset, not universal
//! reasoning, parallel-call, streaming-usage or modality controls. Models bound
//! to this contract must be declared non-reasoning. See the dated implementation
//! report for public-schema provenance and remaining deployment assumptions.

use std::sync::Arc;

use ego_core::generator::capabilities::CacheRetention;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{GenRequest, ThinkingLevel};
use futures::stream::BoxStream;
use serde::Serialize;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::openai_completions::{self, CommonRequest};
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatMaxTokens;

pub(crate) static BEDROCK_CHAT: BedrockChat = BedrockChat;

/// The same bounded Chat body and decoder on Bedrock Runtime's exact path.
#[derive(Debug)]
pub(crate) struct BedrockChat;

impl WireFormat for BedrockChat {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        Ok(target.post_json("openai/v1/chat/completions", request_body(req)?))
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        openai_completions::decode(response, cost)
    }
}

impl WireFormat for ChatMaxTokens {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        Ok(target.post_json("chat/completions", request_body(req)?))
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        // Display extensions remain readable, but no opaque replay state is minted.
        openai_completions::decode(response, cost)
    }
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    #[serde(flatten)]
    common: CommonRequest<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
}

/// Refuse controls absent from this exact contract rather than silently drop them.
pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, RequestRefused> {
    if req.thinking != ThinkingLevel::Off {
        return Err(RequestRefused::new(
            "chat-max-tokens supports only off reasoning on a declared non-reasoning model",
        ));
    }
    if req.parallel_tool_calls {
        return Err(RequestRefused::new(
            "chat-max-tokens does not support parallel tool-call control",
        ));
    }
    if req.web_search.is_some()
        || req
            .cache
            .as_ref()
            .is_some_and(|cache| cache.retention != CacheRetention::None)
    {
        return Err(RequestRefused::new(
            "chat-max-tokens does not support hosted search or explicit prompt-cache controls",
        ));
    }
    if super::request_has_images(req) || super::request_has_audio(req) {
        return Err(RequestRefused::new(
            "chat-max-tokens supports text and tools, not image or audio input",
        ));
    }
    let wire = WireRequest {
        common: openai_completions::common_request(req, None)?
            .without_stream_options()
            .without_parallel_tool_calls()
            .with_reasoning_effort(None),
        max_tokens: req.max_output_tokens,
    };
    Ok(serde_json::to_vec(&wire).expect("WireRequest has no non-string map keys and no NaN"))
}
