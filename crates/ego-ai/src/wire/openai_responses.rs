//! OpenAI's public Responses API: `POST /v1/responses`, streamed, unstored.
//!
//! Deliberately a third Responses mapping rather than a branch in either
//! existing one, because
//! `docs/decisions/2026-09-03-public-provider-wires-require-live-promotion.md`
//! says so and gives the reason: "Private Codex Responses differs in endpoint,
//! identity, headers, and request/stream behavior; it is not evidence for
//! public OpenAI." [`openai_codex_responses`](super::openai_codex_responses)
//! speaks ChatGPT's private backend and carries its identity;
//! [`xai_responses`](super::xai_responses) speaks xAI's. All three decode
//! overlapping slices of the same *event vocabulary*, which is a shared shape
//! and not a shared contract — the distinction
//! `2026-08-21-a-shared-shape-is-not-a-shared-contract.md` exists to keep.
//!
//! # What the evidence for this file actually is
//!
//! Derived field by field from OpenAI's published Responses reference, the
//! reasoning guide and the streaming guide, read **2026-09-07**. It is vendor
//! schema evidence and **not a live server recording**, which is why the
//! `openai` row is not promoted (`ProviderRow::openai_at`): this repository
//! holds no OpenAI key, so conformance check five has never been made for
//! these bytes. `crates/ego-cli/tests/smoke.rs` is where that closes, on a
//! machine that has a key and names the provider.
//!
//! # Two facts worth stating because they look like omissions
//!
//! **`store: false` on every request.** The Responses API defaults `store` to
//! `true`, which would leave a copy of every prompt and completion in
//! OpenAI's retention. A harness that governs what leaves a machine does not
//! opt into that silently, and the ADR names retention as part of the contract
//! this mapping follows.
//!
//! **No per-model effort list here.** The vendor's accepted `reasoning.effort`
//! values differ per model — gpt-6-astra rejects `none` with a 400 while
//! gpt-5.6 accepts six levels — so a list written into this file would be
//! wrong for some model the day it was written. Every [`ThinkingLevel`] has an
//! OpenAI spelling, so this mapping projects all seven and lets
//! `ModelCapabilities::reasoning_efforts` refuse the wrong one *before* a
//! request is built, naming both sides. That is where per-model facts belong
//! and it is already checked; duplicating a guess here would be the second
//! answer to a question the catalogue answers.

use std::collections::BTreeMap;
use std::sync::Arc;

use ego_core::cost::Money;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, ImageMediaType, Role, StopReason, ThinkingLevel,
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
pub struct OpenAiResponses;

impl WireFormat for OpenAiResponses {
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

/// Joined onto OpenAI's public `https://api.openai.com/v1/` base.
///
/// The Chat Completions path this row used to send is `chat/completions`, and
/// a guard test asserts no public OpenAI request can still reach it.
pub const PATH: &str = "responses";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inexpressible {
    ReasoningStateNotCarriable,
    ImageMediaTypeNotSupported(ImageMediaType),
    ImageRoleNotSupported,
}

impl std::fmt::Display for Inexpressible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReasoningStateNotCarriable => write!(
                f,
                "openai-responses has no established projection for ego's opaque reasoning state; dropping it is forbidden"
            ),
            Self::ImageMediaTypeNotSupported(media_type) => write!(
                f,
                "openai-responses image input accepts PNG, JPEG, WEBP and non-animated GIF, not {media_type:?}"
            ),
            Self::ImageRoleNotSupported => write!(
                f,
                "openai-responses accepts image input only in user messages"
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
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    model: &'a str,
    stream: bool,
    /// Always `false`. See this module's header: the default is `true`.
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

/// OpenAI's spelling for every level [`ThinkingLevel`] can produce.
///
/// Total, and that is the point — see this module's header. Which of these a
/// given model accepts is a per-model fact the catalogue already holds.
fn reasoning_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "none",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn carries_reasoning(req: &GenRequest) -> bool {
    req.messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Reasoning { .. }))
}

fn input_items(req: &GenRequest) -> Result<Vec<Value>, Inexpressible> {
    let mut items = Vec::new();
    for message in req.messages.messages() {
        let has_image = message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }));
        if has_image {
            if message.role != Role::User {
                return Err(Inexpressible::ImageRoleNotSupported);
            }
            // One user message carrying every image and every text part of
            // that turn, images first: the vendor's own examples put the
            // picture before the question about it, and a stable order is
            // what makes the request fixture pinnable.
            let mut content = Vec::new();
            for block in &message.content {
                if let ContentBlock::Image { image } = block {
                    let media_type = image.media_type();
                    if !matches!(
                        media_type,
                        ImageMediaType::Png
                            | ImageMediaType::Jpeg
                            | ImageMediaType::Webp
                            | ImageMediaType::Gif
                    ) {
                        return Err(Inexpressible::ImageMediaTypeNotSupported(media_type));
                    }
                    content.push(serde_json::json!({
                        "type": "input_image",
                        "image_url": super::openai_completions::image_data_url(image),
                    }));
                }
            }
            for block in &message.content {
                if let ContentBlock::Text { text } = block {
                    content.push(serde_json::json!({
                        "type": "input_text",
                        "text": text,
                    }));
                }
            }
            items.push(serde_json::json!({ "role": "user", "content": content }));
        }
        for block in &message.content {
            match block {
                // Never sent: `carries_reasoning` already refused the request
                // that contains one, so this arm is unreachable for reasoning
                // ego minted. It stays explicit rather than falling into a
                // wildcard so that adding a block kind is a compile error
                // here rather than a silently dropped field.
                ContentBlock::Reasoning { .. } => {}
                // Handled above when the message had one, and refused above
                // when the role was wrong.
                ContentBlock::Image { .. } => {}
                ContentBlock::Text { text } if !has_image => match message.role {
                    Role::System => items.push(serde_json::json!({
                        "role": "system",
                        "content": text,
                    })),
                    Role::User => items.push(serde_json::json!({
                        "role": "user",
                        "content": text,
                    })),
                    Role::Assistant => items.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    })),
                },
                ContentBlock::Text { .. } => {}
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
    Ok(items)
}

/// The exact JSON bytes this format would send for `req`.
///
/// Separate from [`build_request`] so a golden can pin the body without a URL
/// or a credential in it, exactly as the Chat mapping this replaces did.
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
        prompt_cache_key: super::cache::routing_key(req, "openai"),
        model: &req.model,
        stream: true,
        store: false,
        input: input_items(req)?,
        parallel_tool_calls: (!tools.is_empty()).then_some(req.parallel_tool_calls),
        tools,
        reasoning: WireReasoning {
            effort: reasoning_effort(req.thinking),
        },
        max_output_tokens: req.max_output_tokens,
    };
    Ok(serde_json::to_vec(&wire).expect("WireRequest has no non-string map keys and no NaN"))
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
    #[serde(default)]
    code: Option<String>,
}

/// OpenAI's Responses usage object.
///
/// `input_tokens_details` carries **two** cache figures where xAI carries one:
/// `cached_tokens` are the input tokens served from cache, and
/// `cache_write_tokens` are the ones written into it. Both are reported;
/// neither is inferred from the other.
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
    #[serde(default)]
    cache_write_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct OutputDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

/// `None` when the provider's own figures do not add up.
///
/// The one subtraction is `input_tokens - cached_tokens`, and it is
/// `checked_sub` because a cached count above the input total is incoherent
/// rather than a small number: silently saturating it would bill a caller for
/// tokens under a name that never occurred. `cache_write_tokens` is *not*
/// subtracted — the vendor does not state that written tokens are excluded
/// from `input_tokens`, and subtracting on a guess is how a usage line stops
/// being evidence.
fn map_usage(raw: &RawValue) -> Option<Usage> {
    let usage: WireUsage = serde_json::from_str(raw.get()).ok()?;
    let details = usage.input_tokens_details.as_ref();
    let cache_read_tokens = details.map_or(0, |details| details.cached_tokens);
    let cache_write_tokens = details.map_or(0, |details| details.cache_write_tokens);
    // Both are subtracted, because `input_tokens` is the whole input and the
    // two detail figures are *disjoint parts of it*. The prompt-caching guide
    // (read 2026-09-07) states the arithmetic outright — ordinary input is
    // `input_tokens - cached_tokens - cache_write_tokens`, priced at 1×, with
    // the cached part at 0.1× and the written part at 1.25×. Subtracting only
    // the read would count every written token twice, once at the full rate
    // and once at the write rate, and `Usage`'s parts are required to
    // partition the stated total rather than overlap it.
    let uncached_input_tokens = usage
        .input_tokens
        .checked_sub(cache_read_tokens)?
        .checked_sub(cache_write_tokens)?;
    Some(Usage {
        uncached_input_tokens,
        cache_read_tokens,
        cache_write_tokens,
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
                message:
                    "OpenAI's cached input count exceeded its input total; usage was discarded"
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
                    Some(error) => format!("the OpenAI response stream failed: {error}"),
                    None if self.sse.has_partial_frame() => {
                        "the OpenAI response stream was cut in the middle of an event".to_string()
                    }
                    None => "the OpenAI response stream ended without a terminal Responses event"
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
                tracing::warn!(
                    %error,
                    "skipping an OpenAI Responses event this mapping cannot decode"
                );
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
            "response.completed" | "response.incomplete" => {
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
                        let error = event.response.and_then(|done| done.error);
                        let kind = super::classify_stream_code(
                            error.as_ref().and_then(|error| error.code.as_deref()),
                        );
                        let message = error
                            .and_then(|error| error.message)
                            .unwrap_or_else(|| format!("OpenAI reported response status {status}"));
                        out.extend(super::streamed_failure(message, kind));
                        self.finish_reason = Some(StopReason::Error);
                    }
                    _ if !self.calls.is_empty() => self.finish_reason = Some(StopReason::ToolUse),
                    _ => self.finish_reason = Some(StopReason::Stop),
                }
            }
            "response.failed" | "error" => {
                let error = event
                    .response
                    .and_then(|response| response.error)
                    .or(event.error);
                let code = error
                    .as_ref()
                    .and_then(|error| error.code.clone())
                    .or(event.code);
                let kind = super::classify_stream_code(code.as_deref());
                let message = error
                    .and_then(|error| error.message)
                    .or(event.message)
                    .unwrap_or_else(|| match code {
                        Some(code) => format!("OpenAI reported streamed Responses error {code}"),
                        None => "OpenAI reported a streamed Responses error".to_string(),
                    });
                out.extend(super::streamed_failure(message, kind));
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
                        "OpenAI streamed a function_call at slot {index} with no call_id"
                    ),
                    kind: ErrorKind::Unclassified,
                });
                continue;
            };
            let Some(name) = call.name.filter(|name| !name.is_empty()) else {
                out.push(AiEvent::Error {
                    message: format!("OpenAI streamed a function_call {id} with no name"),
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
