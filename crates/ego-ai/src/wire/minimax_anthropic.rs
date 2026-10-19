//! MiniMax's current Anthropic-compatible text-only contract.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::GenRequest;
use futures::stream::BoxStream;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::anthropic_messages::AnthropicMessages;
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat, request_has_images};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MiniMaxAnthropic;

impl WireFormat for MiniMaxAnthropic {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        if super::request_has_audio(req) {
            return Err(RequestRefused::new(
                "this wire contract does not support audio input",
            ));
        }
        if request_has_images(req) {
            return Err(RequestRefused::new(
                "image input is unsupported by the current MiniMax Anthropic-compatible contract",
            ));
        }
        // MiniMax documents only five-minute explicit cache writes.
        let mut projected = req.clone();
        if let Some(cache) = &mut projected.cache
            && cache.retention == ego_core::generator::capabilities::CacheRetention::Long
        {
            cache.retention = ego_core::generator::capabilities::CacheRetention::Short;
        }
        AnthropicMessages.build_request(target, &projected)
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        AnthropicMessages.decode(response, cost)
    }
}
