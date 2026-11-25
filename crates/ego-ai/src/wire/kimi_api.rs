//! Exact Kimi K3.1 API-key Chat contract (official schema read 2026-11-18).
//! Subscription identity and the retired K2 thinking controls are not this wire.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{GenRequest, ThinkingLevel};
use futures::stream::BoxStream;
use serde::Serialize;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::openai_completions::{self, CommonRequest};
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat};

pub const KIMI_API_REASONING_DIALECT: &str = "kimi-api-k3-chat";
pub const PATH: &str = "chat/completions";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KimiApi;

impl WireFormat for KimiApi {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        Ok(target.post_json(PATH, request_body(req)?))
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        openai_completions::decode_replaying_content(response, cost, KIMI_API_REASONING_DIALECT)
    }
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    #[serde(flatten)]
    common: CommonRequest<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
}

pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, RequestRefused> {
    let effort = match req.thinking {
        ThinkingLevel::Low => "low",
        ThinkingLevel::High => "high",
        ThinkingLevel::Max => "max",
        _ => {
            return Err(RequestRefused::new(
                "Kimi K3.1 always reasons and supports only low, high and max effort",
            ));
        }
    };
    if req
        .max_output_tokens
        .is_some_and(|cap| cap == 0 || cap > 1_048_576)
    {
        return Err(RequestRefused::new(
            "Kimi K3.1 max_completion_tokens must be within 1..=1048576",
        ));
    }
    if req.parallel_tool_calls {
        return Err(RequestRefused::new(
            "Kimi K3.1 has no admitted parallel tool-call control",
        ));
    }
    if req.web_search.is_some()
        || req.cache.as_ref().is_some_and(|cache| {
            cache.retention != ego_core::generator::capabilities::CacheRetention::None
        })
    {
        return Err(RequestRefused::new(
            "this Kimi API contract does not implement hosted search or explicit cache controls",
        ));
    }
    if super::request_has_images(req) || super::request_has_audio(req) {
        return Err(RequestRefused::new(
            "this Kimi API contract supports text and tools only",
        ));
    }
    let wire = WireRequest {
        common: openai_completions::common_request_preserving_reasoning(
            req,
            KIMI_API_REASONING_DIALECT,
        )?
        .without_parallel_tool_calls()
        .with_reasoning_effort(Some(effort)),
        max_completion_tokens: req.max_output_tokens,
    };
    Ok(serde_json::to_vec(&wire).expect("WireRequest serialises JSON"))
}
