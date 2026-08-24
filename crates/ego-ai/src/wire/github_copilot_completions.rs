//! GitHub Copilot's current Chat Completions subset.
//!
//! The JSON and SSE grammar are the shared Chat Completions contract. The two
//! dynamic Copilot headers are not: Pi derives `X-Initiator` from the last
//! message and always sends `Openai-Intent: conversation-edits`. That request
//! difference owns this thin mapping instead of a provider-name branch inside
//! [`openai_completions`](super::openai_completions).
//!
//! Captured-client-derived from
//! `earendil-works/pi@4af9d21d3b4d664e4a29fcabfec85171077248e3`, read
//! 2026-08-24. It is not represented as a live Copilot recording.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ContentBlock, GenRequest, Role};
use futures::stream::BoxStream;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::openai_completions;
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GithubCopilotCompletions;

impl WireFormat for GithubCopilotCompletions {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        let body = openai_completions::captured_client_request_body(req)
            .map_err(|refused| RequestRefused::new(refused.to_string()))?;
        let mut built = target.post_json(openai_completions::PATH, body);
        let initiator = req.messages.messages().last().map_or("user", |message| {
            if message.role != Role::User
                || message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            {
                "agent"
            } else {
                "user"
            }
        });
        for (name, value) in [
            ("x-initiator", initiator),
            ("openai-intent", "conversation-edits"),
        ] {
            built
                .headers
                .retain(|(present, _)| !present.eq_ignore_ascii_case(name));
            built.headers.push((name.to_string(), value.to_string()));
        }
        Ok(built)
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        openai_completions::decode(response, cost)
    }
}
