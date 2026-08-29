//! Nous Portal's captured current Chat Completions projection.
//!
//! Hermes omits reasoning controls when thinking is off; OpenAI's current
//! Platform mapping sends its explicit `none` spelling. The response grammar
//! remains the shared Chat Completions grammar, so only request construction
//! differs and decoding is delegated.
//!
//! Captured-client-derived from
//! `NousResearch/hermes-agent@a0ca7c19204e514f9590ce3b812e029b315ab9e9`,
//! read 2026-08-24. It is not represented as a live Nous recording.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::GenRequest;
use futures::stream::BoxStream;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::openai_completions;
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat, request_has_images};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NousPortal;

impl WireFormat for NousPortal {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        if request_has_images(req) {
            return Err(RequestRefused::new(
                "image input is unknown for the captured Nous Portal contract",
            ));
        }
        let body = openai_completions::captured_client_request_body(req)
            .map_err(|refused| RequestRefused::new(refused.to_string()))?;
        Ok(target.post_json(openai_completions::PATH, body))
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        openai_completions::decode(response, cost)
    }
}
