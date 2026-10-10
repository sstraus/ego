//! Kimi's current beta Messages contract.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ContentBlock, Conversation, GenRequest, ThinkingLevel};
use futures::stream::{BoxStream, StreamExt};

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::anthropic_messages;
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KimiAnthropic;

pub const DIALECT: &str = "kimi-anthropic";

impl WireFormat for KimiAnthropic {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        let mut request = target.post_json("messages?beta=true", request_body(req)?);
        request.headers.push(crate::http::public_header(
            anthropic_messages::VERSION_HEADER,
            anthropic_messages::VERSION.as_bytes().to_vec(),
        ));
        Ok(request)
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        anthropic_messages::decode(response, cost)
            .map(|event| match event {
                AiEvent::ReasoningState(mut state) => {
                    state.dialect = DIALECT.to_string();
                    AiEvent::ReasoningState(state)
                }
                event => event,
            })
            .boxed()
    }
}

fn effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::XHigh => Some("xhigh"),
        ThinkingLevel::Max => Some("max"),
    }
}

pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, RequestRefused> {
    let mut projected = req.clone();
    let mut messages = projected.messages.messages().to_vec();
    for message in &mut messages {
        for block in &mut message.content {
            if let ContentBlock::Reasoning { state } = block {
                if state.dialect != DIALECT {
                    return Err(RequestRefused::new(format!(
                        "kimi-anthropic cannot carry reasoning state minted by {}",
                        state.dialect
                    )));
                }
                state.dialect = anthropic_messages::DIALECT.to_string();
            }
        }
    }
    projected.messages = Conversation::new(messages)
        .expect("changing only a reasoning dialect preserves conversation invariants");
    if projected.thinking == ThinkingLevel::Minimal {
        projected.thinking = ThinkingLevel::Low;
    }
    // Kimi ignores Claude's block breakpoints; cache writes are top-level only.
    projected.cache = None;
    let bytes = anthropic_messages::request_body(&projected)
        .map_err(|error| RequestRefused::new(error.to_string()))?;
    let mut body: serde_json::Value =
        serde_json::from_slice(&bytes).expect("the Messages request serialises a JSON object");
    let object = body
        .as_object_mut()
        .expect("the Messages request serialises a JSON object");
    if let Some(key) = super::cache::routing_key(req, "kimi") {
        object.insert("metadata".to_string(), serde_json::json!({"user_id": key}));
        let mut control = super::cache::control(req).expect("enabled cache metadata");
        if control.get("ttl").is_none() {
            control["ttl"] = "5m".into();
        }
        object.insert("cache_control".to_string(), control);
    }
    match effort(req.thinking) {
        None => {
            object.insert(
                "thinking".to_string(),
                serde_json::json!({"type": "disabled"}),
            );
            object.remove("output_config");
        }
        Some(effort) => {
            object.insert(
                "thinking".to_string(),
                serde_json::json!({"type": "enabled"}),
            );
            object.insert(
                "output_config".to_string(),
                serde_json::json!({"effort": effort}),
            );
        }
    }
    Ok(serde_json::to_vec(&body).expect("a JSON value serialises"))
}
