//! Claude's Messages contract as two cloud platforms serve it.
//!
//! Amazon Bedrock serves the first-party request and SSE shape at
//! `{base}messages` (Claude in Amazon Bedrock, read 2026-11-05 from
//! <https://platform.claude.com/docs/en/build-with-claude/claude-in-amazon-bedrock>).
//! Google Cloud names the model in the URL, takes `anthropic_version` in the
//! body and streams the same SSE events from `:streamRawPredict` (Claude Models
//! on Google Cloud, Google managed, read 2026-11-05 from
//! <https://platform.claude.com/docs/en/build-with-claude/claude-models-on-google-cloud/google-managed>).
//!
//! Both reuse the Messages projection and decoder. What differs is data on the
//! value: where the model and version go, and the reasoning dialect. Each
//! platform has its own dialect, because no published source says a thinking
//! signature minted on one platform is accepted by another; a session that
//! changes platform therefore refuses the carried state instead of sending it.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ContentBlock, Conversation, GenRequest};
use futures::stream::{BoxStream, StreamExt};

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::anthropic_messages;
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat};

/// Where a platform expects the model and the contract version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The first-party shape at `{base}messages`: model in the body, version in
    /// the `anthropic-version` header.
    Messages,
    /// `{base}publishers/anthropic/models/{model}:streamRawPredict`: no model in
    /// the body, version in the body's `anthropic_version` field.
    RawPredict,
}

/// One cloud platform's Messages contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartnerMessages {
    dialect: &'static str,
    route: Route,
}

/// Claude in Amazon Bedrock, on the `bedrock-mantle` Messages endpoint.
pub static BEDROCK: PartnerMessages = PartnerMessages {
    dialect: "bedrock-messages",
    route: Route::Messages,
};

/// Claude Models on Google Cloud (Google managed), on Vertex AI.
pub static VERTEX: PartnerMessages = PartnerMessages {
    dialect: "vertex-messages",
    route: Route::RawPredict,
};

/// The body version Google Cloud documents for this contract.
pub const VERTEX_VERSION: &str = "vertex-2023-10-16";

impl PartnerMessages {
    pub const fn dialect(&self) -> &'static str {
        self.dialect
    }

    /// The request with this platform's reasoning state presented as the
    /// Messages dialect the shared projection writes. The capability gate has
    /// already refused state of another dialect.
    fn project(&self, req: &GenRequest) -> GenRequest {
        let mut projected = req.clone();
        let mut messages = projected.messages.messages().to_vec();
        for block in messages.iter_mut().flat_map(|message| &mut message.content) {
            if let ContentBlock::Reasoning { state } = block
                && state.dialect == self.dialect
            {
                state.dialect = anthropic_messages::DIALECT.to_string();
            }
        }
        projected.messages = Conversation::new(messages)
            .expect("changing only a reasoning dialect preserves conversation invariants");
        projected
    }
}

impl WireFormat for PartnerMessages {
    fn prompt_cache_policy(&self, req: &GenRequest) -> ego_core::usage::PromptCachePolicy {
        // Both platforms document the one-hour duration beside the five-minute
        // default (prompt caching page, read 2026-11-05).
        super::cache::policy(req, true, false)
    }

    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        let projected = self.project(req);
        match self.route {
            Route::Messages => {
                anthropic_messages::build_request(target, &projected).map_err(RequestRefused::from)
            }
            Route::RawPredict => {
                let path = raw_predict_path(&req.model)?;
                let bytes = anthropic_messages::request_body(&projected)?;
                let mut body: serde_json::Value = serde_json::from_slice(&bytes)
                    .expect("the Messages request serialises a JSON object");
                let object = body
                    .as_object_mut()
                    .expect("the Messages request serialises a JSON object");
                object.remove("model");
                object.insert("anthropic_version".to_string(), VERTEX_VERSION.into());
                Ok(target.post_json(
                    &path,
                    serde_json::to_vec(&body).expect("a JSON value serialises"),
                ))
            }
        }
    }

    fn cache_prefix(&self, body: &[u8]) -> Option<String> {
        // The raw-predict body has no model, so its digest could not tell two
        // models apart; it reports no continuity rather than a false one.
        (self.route == Route::Messages)
            .then(|| anthropic_messages::AnthropicMessages.cache_prefix(body))
            .flatten()
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        let dialect = self.dialect;
        anthropic_messages::decode(response, cost)
            .map(move |event| match event {
                AiEvent::ReasoningState(mut state) => {
                    state.dialect = dialect.to_string();
                    AiEvent::ReasoningState(state)
                }
                event => event,
            })
            .boxed()
    }
}

/// The model's path under the row base, or a refusal for an id that would
/// leave its path segment.
///
/// The id comes from the operator's `-m`, and `Url::join` resolves `/`, `..`,
/// `?` and `#`: an id carrying one would send the request to another resource
/// of the same host.
fn raw_predict_path(model: &str) -> Result<String, RequestRefused> {
    let safe = !model.is_empty()
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
        && !model.starts_with('.');
    if !safe {
        return Err(RequestRefused::new(format!(
            "model id `{model}` is not a Google Cloud publisher model id"
        )));
    }
    Ok(format!(
        "publishers/anthropic/models/{model}:streamRawPredict"
    ))
}
