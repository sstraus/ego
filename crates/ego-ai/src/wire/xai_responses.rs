//! xAI's current Responses API: `POST /v1/responses`, streamed.
//!
//! This mapping is intentionally distinct from
//! [`openai_codex_responses`](super::openai_codex_responses). The public xAI
//! contract and ChatGPT's private Codex backend share parts of the Responses
//! event vocabulary, but not endpoint ownership, request projection, headers,
//! supported reasoning efforts, usage dimensions, or cost declaration.
//!
//! Derived field-by-field from xAI's Responses, streaming, function-calling,
//! reasoning, and cost-tracking documentation, read 2026-08-24. It is vendor
//! schema evidence, not a live server recording.

use std::collections::BTreeMap;
use std::sync::Arc;

use ego_core::cost::Money;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, Role, StopReason, ThinkingLevel,
};
use ego_core::state::Usage;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::provider_cost::ProviderCost;
use super::sse::SseDecoder;
use super::{RequestRefused, RequestTarget, WireFormat};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct XaiResponses;

impl WireFormat for XaiResponses {
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
        decode(response, cost)
    }
}

/// Joined onto xAI's public `https://api.x.ai/v1/` base.
pub const PATH: &str = "responses";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inexpressible {
    ReasoningStateNotCarriable,
    ReasoningEffortNotSupported(ThinkingLevel),
}

impl std::fmt::Display for Inexpressible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReasoningStateNotCarriable => write!(
                f,
                "xai-responses has no established projection for ego's opaque reasoning state; dropping it is forbidden"
            ),
            Self::ReasoningEffortNotSupported(level) => write!(
                f,
                "xai-responses cannot express {level:?} on the current grok-4.6 reasoning contract; use low, medium, high, or xhigh"
            ),
        }
    }
}

impl std::error::Error for Inexpressible {}

impl From<Inexpressible> for RequestRefused {
    fn from(refused: Inexpressible) -> Self {
        RequestRefused::new(refused.to_string())
    }
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    stream: bool,
    store: bool,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    reasoning: WireReasoning,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
}

#[derive(Debug, Serialize)]
struct WireReasoning {
    effort: &'static str,
}

#[derive(Debug, Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

fn reasoning_effort(level: ThinkingLevel) -> Result<&'static str, Inexpressible> {
    match level {
        ThinkingLevel::Low => Ok("low"),
        ThinkingLevel::Medium => Ok("medium"),
        ThinkingLevel::High => Ok("high"),
        ThinkingLevel::XHigh => Ok("xhigh"),
        other => Err(Inexpressible::ReasoningEffortNotSupported(other)),
    }
}

fn carries_reasoning(req: &GenRequest) -> bool {
    req.messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Reasoning { .. }))
}

fn input_items(req: &GenRequest) -> Vec<Value> {
    let mut items = Vec::new();
    for message in req.messages.messages() {
        for block in &message.content {
            match block {
                ContentBlock::Reasoning { .. } => {}
                ContentBlock::Text { text } => match message.role {
                    Role::System | Role::User => items.push(serde_json::json!({
                        "role": match message.role { Role::System => "system", _ => "user" },
                        "content": text,
                    })),
                    Role::Assistant => items.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    })),
                },
                ContentBlock::ToolUse {
                    call_id,
                    name,
                    args,
                } => items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": args,
                })),
                ContentBlock::ToolResult {
                    call_id, output, ..
                } => items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                })),
            }
        }
    }
    items
}

pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    if carries_reasoning(req) {
        return Err(Inexpressible::ReasoningStateNotCarriable);
    }
    let tools: Vec<WireTool<'_>> = req
        .tools
        .iter()
        .map(|tool| WireTool {
            kind: "function",
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.parameters,
        })
        .collect();
    let wire = WireRequest {
        model: &req.model,
        stream: true,
        store: false,
        input: input_items(req),
        parallel_tool_calls: (!tools.is_empty()).then_some(req.parallel_tool_calls),
        tools,
        reasoning: WireReasoning {
            effort: reasoning_effort(req.thinking)?,
        },
        max_output_tokens: req.max_output_tokens,
    };
    Ok(serde_json::to_vec(&wire).expect("WireRequest serialises"))
}

pub fn build_request(
    target: &RequestTarget<'_>,
    req: &GenRequest,
) -> Result<HttpRequest, Inexpressible> {
    Ok(target.post_json(PATH, request_body(req)?))
}

pub fn decode(
    response: Result<HttpResponse, HttpError>,
    cost: Option<Arc<dyn ProviderCost>>,
) -> BoxStream<'static, AiEvent> {
    super::decode_response(response, Box::new(StreamDecoder::new(cost)))
}

#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    item: Option<OutputItem>,
    #[serde(default)]
    response: Option<Completed>,
    #[serde(default)]
    error: Option<EventError>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OutputItem {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Completed {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<EventError>,
}

#[derive(Debug, Deserialize)]
struct EventError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Debug, Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct OutputDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

fn map_usage(raw: &RawValue) -> Option<Usage> {
    let usage: WireUsage = serde_json::from_str(raw.get()).ok()?;
    let cache_read_tokens = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |details| details.cached_tokens);
    let uncached_input_tokens = usage.input_tokens.checked_sub(cache_read_tokens)?;
    Some(Usage {
        uncached_input_tokens,
        cache_read_tokens,
        cache_write_tokens: 0,
        cache_write_extended_tokens: 0,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage
            .output_tokens_details
            .as_ref()
            .map(|details| details.reasoning_tokens),
    })
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

#[derive(Debug)]
struct StreamDecoder {
    sse: SseDecoder,
    calls: BTreeMap<String, PartialToolCall>,
    finish_reason: Option<StopReason>,
    usage: Option<Usage>,
    usage_incoherent: bool,
    cost: Option<Arc<dyn ProviderCost>>,
    stated_cost: Option<Money>,
}

impl StreamDecoder {
    fn new(cost: Option<Arc<dyn ProviderCost>>) -> Self {
        Self {
            sse: SseDecoder::default(),
            calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            usage_incoherent: false,
            cost,
            stated_cost: None,
        }
    }
}

impl super::StreamSink for StreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<AiEvent> {
        let mut out = Vec::new();
        for frame in self.sse.push(chunk) {
            self.consume(&frame, &mut out);
        }
        out
    }

    fn finish(mut self: Box<Self>, transport: Option<HttpError>) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if let Some(error) = &transport {
            out.push(AiEvent::DeliveryObserved(error.delivery()));
        }
        if self.usage_incoherent {
            out.push(AiEvent::Error {
                message: "xAI's cached input count exceeded its input total; usage was discarded"
                    .to_string(),
                kind: ErrorKind::Unclassified,
            });
        }
        if self.finish_reason != Some(StopReason::Error) {
            self.flush_calls(&mut out);
        } else {
            self.calls.clear();
        }
        if let Some(usage) = self.usage.take() {
            out.push(AiEvent::Usage(usage));
        }
        if let Some(cost) = self.stated_cost.take() {
            out.push(AiEvent::ProviderCost { cost });
        }
        match self.finish_reason {
            Some(stop) => out.push(AiEvent::Stop(stop)),
            None => {
                let kind = transport
                    .as_ref()
                    .map_or(ErrorKind::Transient, HttpError::kind);
                let message = match transport {
                    Some(error) => format!("the xAI response stream failed: {error}"),
                    None if self.sse.has_partial_frame() => {
                        "the xAI response stream was cut in the middle of an event".to_string()
                    }
                    None => "the xAI response stream ended without a terminal Responses event"
                        .to_string(),
                };
                out.push(AiEvent::Error { message, kind });
                out.push(AiEvent::Stop(StopReason::Error));
            }
        }
        out
    }
}

impl StreamDecoder {
    fn consume(&mut self, frame: &str, out: &mut Vec<AiEvent>) {
        if frame == "[DONE]" {
            if self.finish_reason.is_none() {
                self.finish_reason = Some(if self.calls.is_empty() {
                    StopReason::Stop
                } else {
                    StopReason::ToolUse
                });
            }
            return;
        }
        let event: Event = match serde_json::from_str(frame) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, "skipping an xAI Responses event this mapping cannot decode");
                return;
            }
        };
        match event.kind.as_str() {
            "response.output_text.delta" => {
                if let Some(text) = event.delta.filter(|text| !text.is_empty()) {
                    out.push(AiEvent::TextDelta { text });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = event.delta.filter(|text| !text.is_empty()) {
                    out.push(AiEvent::ReasoningDelta { text });
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.item
                    && item.kind.as_deref() == Some("function_call")
                {
                    let key = item
                        .id
                        .clone()
                        .or(item.call_id.clone())
                        .unwrap_or_else(|| format!("call-{}", self.calls.len()));
                    let call = self.calls.entry(key).or_default();
                    if let Some(id) = item.call_id.or(item.id) {
                        call.id = Some(id);
                    }
                    if let Some(name) = item.name {
                        call.name = Some(name);
                    }
                    if let Some(args) = item.arguments {
                        call.args.push_str(&args);
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.delta {
                    if let Some(item_id) = event.item_id.as_ref() {
                        if let Some(call) = self.calls.get_mut(item_id) {
                            call.args.push_str(&delta);
                        }
                    } else if let Some(call) = self.calls.values_mut().last() {
                        call.args.push_str(&delta);
                    }
                }
            }
            "response.completed" => {
                if let Some(raw) = event
                    .response
                    .as_ref()
                    .and_then(|done| done.usage.as_deref())
                {
                    if let Some(cost) = self.cost.as_ref().and_then(|decoder| decoder.read(raw)) {
                        self.stated_cost = Some(cost);
                    }
                    match map_usage(raw) {
                        Some(usage) => self.usage = Some(usage),
                        None => self.usage_incoherent = true,
                    }
                }
                let status = event
                    .response
                    .as_ref()
                    .and_then(|done| done.status.as_deref())
                    .unwrap_or("completed")
                    .to_string();
                match status.as_str() {
                    "incomplete" => self.finish_reason = Some(StopReason::Length),
                    "failed" | "cancelled" => {
                        let message = event
                            .response
                            .and_then(|done| done.error)
                            .and_then(|error| error.message)
                            .unwrap_or_else(|| format!("xAI reported response status {status}"));
                        out.push(AiEvent::Error {
                            message,
                            kind: ErrorKind::Unclassified,
                        });
                        self.finish_reason = Some(StopReason::Error);
                    }
                    _ if !self.calls.is_empty() => self.finish_reason = Some(StopReason::ToolUse),
                    _ => self.finish_reason = Some(StopReason::Stop),
                }
            }
            "response.failed" | "error" => {
                let message = event
                    .response
                    .and_then(|response| response.error)
                    .and_then(|error| error.message)
                    .or_else(|| event.error.and_then(|error| error.message))
                    .or(event.message)
                    .unwrap_or_else(|| match event.code {
                        Some(code) => format!("xAI reported streamed Responses error {code}"),
                        None => "xAI reported a streamed Responses error".to_string(),
                    });
                out.push(AiEvent::Error {
                    message,
                    kind: ErrorKind::Unclassified,
                });
                self.finish_reason = Some(StopReason::Error);
            }
            _ => {}
        }
    }

    fn flush_calls(&mut self, out: &mut Vec<AiEvent>) {
        for (index, (_, call)) in std::mem::take(&mut self.calls).into_iter().enumerate() {
            let Some(id) = call.id.filter(|id| !id.is_empty()) else {
                out.push(AiEvent::Error {
                    message: format!(
                        "xAI streamed a function_call at slot {index} with no call_id"
                    ),
                    kind: ErrorKind::Unclassified,
                });
                continue;
            };
            let Some(name) = call.name.filter(|name| !name.is_empty()) else {
                out.push(AiEvent::Error {
                    message: format!("xAI streamed a function_call {id} with no name"),
                    kind: ErrorKind::Unclassified,
                });
                continue;
            };
            out.push(AiEvent::ToolCall {
                call_id: id,
                name,
                args: call.args,
            });
        }
    }
}
