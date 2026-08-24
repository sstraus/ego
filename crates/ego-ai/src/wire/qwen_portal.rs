//! Qwen Portal's captured current Chat Completions request projection.
//!
//! The response grammar is the shared Chat Completions grammar. The request is
//! not byte-identical: Qwen Portal normalises string message content into text
//! parts, marks the final system part with an ephemeral cache breakpoint, and
//! sends `vl_high_resolution_images: true`. This mapping owns only those
//! differences and delegates the shared decoder.
//!
//! Captured-client-derived from
//! `NousResearch/hermes-agent@a0ca7c19204e514f9590ce3b812e029b315ab9e9`,
//! read 2026-08-24. It is not represented as a live Qwen recording.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::GenRequest;
use futures::stream::BoxStream;
use serde_json::Value;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::openai_completions::{self, Inexpressible};
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QwenPortal;

impl WireFormat for QwenPortal {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        Ok(target.post_json(
            openai_completions::PATH,
            request_body(req).map_err(|refused| RequestRefused::new(refused.to_string()))?,
        ))
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        openai_completions::decode(response, cost)
    }
}

pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    let mut body: Value =
        serde_json::from_slice(&openai_completions::captured_client_request_body(req)?)
            .expect("the shared mapping serialises JSON");
    let messages = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .expect("the shared mapping always serialises messages as an array");
    let mut system_index = None;
    for (index, message) in messages.iter_mut().enumerate() {
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        if system_index.is_none() && object.get("role").and_then(Value::as_str) == Some("system") {
            system_index = Some(index);
        }
        let Some(content) = object.get_mut("content") else {
            continue;
        };
        if let Some(text) = content.as_str().map(str::to_string) {
            *content = serde_json::json!([{ "type": "text", "text": text }]);
            continue;
        }
        if let Some(parts) = content.as_array_mut() {
            for part in parts.iter_mut() {
                if let Some(text) = part.as_str().map(str::to_string) {
                    *part = serde_json::json!({ "type": "text", "text": text });
                }
            }
        }
    }
    if let Some(index) = system_index
        && let Some(last) = messages[index]
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|parts| parts.last_mut())
        && let Some(part) = last.as_object_mut()
    {
        part.insert(
            "cache_control".to_string(),
            serde_json::json!({ "type": "ephemeral" }),
        );
    }
    body.as_object_mut()
        .expect("the shared request is an object")
        .insert("vl_high_resolution_images".to_string(), Value::Bool(true));
    Ok(serde_json::to_vec(&body).expect("a JSON value serialises"))
}
