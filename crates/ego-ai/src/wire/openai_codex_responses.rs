//! The `openai-codex-responses` wire format: `POST /codex/responses`, streamed.
//!
//! ChatGPT/Codex subscription backend, not Platform `api.openai.com`. Pi names
//! this shape separately from `openai-responses` (the design notes
//! §4.1). The body is Responses (`input`, `instructions`, `reasoning.effort`);
//! the path and identity headers are Codex's.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, HostedSearch, Message, Role, StopReason, ThinkingLevel,
};
use ego_core::state::Usage;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::provider_cost::ProviderCost;
use super::sse::SseDecoder;
use super::{RequestRefused, RequestTarget, WireFormat};

/// The mapping a Codex subscription row names.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiCodexResponses;

impl WireFormat for OpenAiCodexResponses {
    fn supports_web_search(&self) -> bool {
        true
    }

    fn supports_default_web_search(
        &self,
        _model: &str,
        base: &url::Url,
        _credential: &crate::provider::ResolvedCredential,
        _strategy: &crate::provider::CredentialStrategy,
    ) -> bool {
        base.as_str() == crate::provider::OPENAI_CODEX_BASE
    }

    fn websocket(&self) -> bool {
        true
    }

    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        build_request(target, req).map_err(RequestRefused::from)
    }

    fn cache_request_identity(
        &self,
        body: &[u8],
        scope: String,
        previous_input_items: Option<usize>,
    ) -> Option<ego_core::usage::CacheRequestIdentity> {
        super::cache::responses_request_identity(body, scope, previous_input_items)
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        self.decode_generation(response, cost, None)
    }

    fn decode_generation(
        &self,
        response: Result<HttpResponse, HttpError>,
        _cost: Option<Arc<dyn ProviderCost>>,
        search: Option<HostedSearch>,
    ) -> BoxStream<'static, AiEvent> {
        super::decode_response(
            response,
            Box::new(StreamDecoder {
                search_limit: search.map(|search| search.max_requests),
                ..StreamDecoder::default()
            }),
        )
    }
}

/// Joined onto `https://chatgpt.com/backend-api/`.
pub const PATH: &str = "codex/responses";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inexpressible {
    AudioNotSupported,
    ReasoningStateNotCarriable,
}

impl std::fmt::Display for Inexpressible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AudioNotSupported => {
                f.write_str("this wire contract does not support audio input")
            }
            Inexpressible::ReasoningStateNotCarriable => write!(
                f,
                "openai-codex-responses requires valid encrypted reasoning items from its own dialect"
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
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    reasoning: WireReasoning,
    include: [&'static str; 1],
}

#[derive(Debug, Serialize)]
struct WireReasoning {
    effort: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WireTool<'a> {
    Function {
        #[serde(rename = "type")]
        kind: &'static str,
        name: &'a str,
        description: &'a str,
        parameters: &'a Value,
        /// Always `false`. Omitting it makes the Responses endpoint normalise
        /// the schema to strict mode, which moves every optional property
        /// into `required`; the model then sends fields the tool forbids.
        /// https://developers.openai.com/api/docs/guides/migrate-to-responses
        strict: bool,
    },
    WebSearch {
        #[serde(rename = "type")]
        kind: &'static str,
        external_web_access: bool,
    },
}

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

/// The constant `instructions` sent with any system text. GPT-6.2 Sol never
/// reads the `instructions` field from the prompt cache, so the variable system
/// text goes in the first input item, where the cached prefix covers it. See
/// the design notes.
pub const CODEX_INSTRUCTIONS: &str = "Follow the developer message at the start of the input.";

fn system_instructions(messages: &[Message]) -> Option<String> {
    let text: Vec<&str> = messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if text.is_empty() {
        None
    } else {
        Some(text.join("\n"))
    }
}

fn input_items(messages: &[Message]) -> Result<Vec<Value>, Inexpressible> {
    let mut items = Vec::new();
    for message in messages {
        if message.role == Role::System {
            continue;
        }
        for block in &message.content {
            match block {
                ContentBlock::ProviderContent { state, .. } => {
                    items.push(
                        super::responses_search::replay(state, "openai-codex-responses")
                            .ok_or(Inexpressible::ReasoningStateNotCarriable)?,
                    );
                }
                ContentBlock::Reasoning { state } => {
                    if state.dialect != "openai-codex-responses" {
                        return Err(Inexpressible::ReasoningStateNotCarriable);
                    }
                    let item: Value = serde_json::from_str(&state.blob)
                        .map_err(|_| Inexpressible::ReasoningStateNotCarriable)?;
                    if item["type"] != "reasoning" || !item["encrypted_content"].is_string() {
                        return Err(Inexpressible::ReasoningStateNotCarriable);
                    }
                    items.push(item);
                }
                ContentBlock::Audio { .. } => {
                    unreachable!("audio is refused before wire serialization")
                }
                ContentBlock::Image { image } => items.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "input_image",
                        "image_url": super::openai_completions::image_data_url(image),
                    }],
                })),
                ContentBlock::Text { text } => match message.role {
                    Role::User => items.push(serde_json::json!({
                        "role": "user",
                        "content": text,
                    })),
                    Role::Assistant => items.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    })),
                    Role::System => {}
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
    Ok(items)
}

pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    encode(req).map(|(body, _)| body)
}

/// The body and its routing key. The key covers the model, the leading
/// developer item and the tools: the prefix that new sessions share.
fn encode(req: &GenRequest) -> Result<(Vec<u8>, Option<String>), Inexpressible> {
    if super::request_has_audio(req) {
        return Err(Inexpressible::AudioNotSupported);
    }
    let mut tools: Vec<WireTool<'_>> = req
        .tools
        .iter()
        .filter(|tool| req.web_search.is_none() || tool.name != "web_search")
        .map(|tool| WireTool::Function {
            kind: "function",
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.parameters,
            strict: false,
        })
        .collect();
    if let Some(search) = req.web_search {
        // This route has no established individual-search count field.
        // Cached defaults: https://learn.chatgpt.com/docs/web-search.md,
        // checked 2026-11-16. Explicit selection retains live access.
        tools.push(WireTool::WebSearch {
            kind: "web_search",
            external_web_access: search.allow_live_access,
        });
    }
    let system = system_instructions(req.messages.messages());
    let mut input = input_items(req.messages.messages())?;
    if let Some(text) = &system {
        input.insert(
            0,
            serde_json::json!({
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": text }],
            }),
        );
    }
    let stable_prefix =
        serde_json::to_vec(&(&req.model, system.is_some().then(|| &input[0]), &tools))
            .expect("Codex stable prefix serialises");
    let key = super::cache::prefix_routing_key(req, "openai-codex", &stable_prefix);
    let wire = WireRequest {
        model: &req.model,
        stream: true,
        store: false,
        prompt_cache_key: key.clone(),
        instructions: system.is_some().then(|| CODEX_INSTRUCTIONS.to_owned()),
        input,
        parallel_tool_calls: (!tools.is_empty()).then_some(req.parallel_tool_calls),
        tools,
        include: ["reasoning.encrypted_content"],
        reasoning: WireReasoning {
            effort: reasoning_effort(req.thinking),
        },
    };
    Ok((
        serde_json::to_vec(&wire).expect("WireRequest serialises"),
        key,
    ))
}

pub fn build_request(
    target: &RequestTarget<'_>,
    req: &GenRequest,
) -> Result<HttpRequest, Inexpressible> {
    let (body, key) = encode(req)?;
    let mut request = target.post_json(PATH, body);
    // ChatGPT routes the cache by the session header, not by the body key:
    // both carry the prefix key so that new sessions reuse the cached prefix.
    // The ego session identity stays in the journal and the socket binding.
    if let Some(key) = key {
        request
            .headers
            .retain(|header| !header.name().eq_ignore_ascii_case("session-id"));
        request.headers.push(crate::http::public_header(
            "session-id",
            key.clone().into_bytes(),
        ));
        request
            .headers
            .retain(|header| !header.name().eq_ignore_ascii_case("x-client-request-id"));
        request.headers.push(crate::http::public_header(
            "x-client-request-id",
            key.into_bytes(),
        ));
    }
    if !request
        .headers
        .iter()
        .any(|header| header.name().eq_ignore_ascii_case("openai-beta"))
    {
        request.headers.push(crate::http::public_header(
            "openai-beta",
            b"responses=experimental".to_vec(),
        ));
    }
    Ok(request)
}

#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
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
    #[serde(default)]
    encrypted_content: Option<String>,
    #[serde(default)]
    summary: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct Completed {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<WireUsage>,
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

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
}

#[derive(Debug, Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

fn map_usage(usage: &WireUsage) -> Option<Usage> {
    let cache_read_tokens = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |details| details.cached_tokens);
    let cache_write_tokens = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |details| details.cache_write_tokens);
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
        web_search_requests: 0,
        reasoning_tokens: None,
    })
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

#[derive(Debug, Default)]
struct StreamDecoder {
    search: super::responses_search::SearchItems,
    search_limit: Option<NonZeroU32>,
    search_cutoff: bool,
    sse: SseDecoder,
    calls: BTreeMap<String, PartialToolCall>,
    finish_reason: Option<StopReason>,
    usage: Option<Usage>,
    usage_incoherent: bool,
}

impl super::StreamSink for StreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<AiEvent> {
        StreamDecoder::push(self, chunk)
    }

    fn finish(self: Box<Self>, transport: Option<HttpError>) -> Vec<AiEvent> {
        StreamDecoder::finish(*self, transport)
    }

    fn stop_reading(&self) -> bool {
        self.search_cutoff
    }

    fn usage_incoherent(&self) -> bool {
        self.usage_incoherent
    }
}

impl StreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<AiEvent> {
        let mut out = Vec::new();
        for frame in self.sse.push(chunk) {
            self.consume(&frame, &mut out);
            if self.search_cutoff {
                break;
            }
        }
        out
    }

    fn finish(mut self, transport: Option<HttpError>) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if let Some(error) = &transport {
            out.push(AiEvent::DeliveryObserved(error.delivery()));
        }
        self.flush_calls(&mut out);
        if let Some(usage) = self.usage.take() {
            out.push(AiEvent::Usage(usage));
        }
        match self.finish_reason {
            Some(stop) => out.push(AiEvent::Stop(stop)),
            None => {
                let kind = transport
                    .as_ref()
                    .map_or(ErrorKind::Transient, HttpError::kind);
                let message = match transport {
                    Some(error) => format!("the response stream failed: {error}"),
                    None if self.sse.has_partial_frame() => {
                        "the response stream was cut in the middle of an event".to_string()
                    }
                    None => {
                        "the response stream ended without a terminal Responses event".to_string()
                    }
                };
                out.push(AiEvent::Error { message, kind });
                out.push(AiEvent::Stop(StopReason::Error));
            }
        }
        out
    }

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
                tracing::warn!(%error, "skipping a Codex event this mapping cannot decode");
                return;
            }
        };
        if self
            .search
            .capture(frame, "openai-codex-responses", out, self.search_limit)
        {
            self.search_cutoff = true;
            return;
        }
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
            "response.output_item.done" => {
                if let Some(item) = event.item
                    && item.kind.as_deref() == Some("reasoning")
                    && let Some(encrypted) = item.encrypted_content
                {
                    out.push(AiEvent::ReasoningState(ego_core::generator::types::ReasoningState {
                        connection: None,
                        dialect: "openai-codex-responses".into(),
                        blob: serde_json::json!({"type":"reasoning", "id":item.id, "encrypted_content":encrypted, "summary":item.summary}).to_string(),
                    }));
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
                if let Some(delta) = event.delta
                    && let Some(call) = self.calls.values_mut().last()
                {
                    call.args.push_str(&delta);
                }
            }
            "response.completed" => {
                if let Some(usage) = event.response.as_ref().and_then(|done| done.usage.as_ref()) {
                    match map_usage(usage) {
                        Some(mut usage) => {
                            usage.web_search_requests = self.search.observed();
                            self.usage = Some(usage);
                        }
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
                        let class = super::classify_stream_code(
                            error.as_ref().and_then(|error| error.code.as_deref()),
                        );
                        let message = error.and_then(|error| error.message).unwrap_or_else(|| {
                            format!("the Codex backend reported response status {status}")
                        });
                        out.extend(super::streamed_failure(message, class));
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
                let class = super::classify_stream_code(code.as_deref());
                let message = error
                    .and_then(|error| error.message)
                    .or(event.message)
                    .unwrap_or_else(|| match code {
                        Some(code) => format!("the Codex backend reported streamed error {code}"),
                        None => "the Codex backend reported response.failed".to_string(),
                    });
                out.extend(super::streamed_failure(message, class));
                self.finish_reason = Some(StopReason::Error);
            }
            _ => {}
        }
    }

    fn flush_calls(&mut self, out: &mut Vec<AiEvent>) {
        // Absent identities pass on empty; the shared stage decides.
        for call in std::mem::take(&mut self.calls) {
            let id = call.1.id.unwrap_or_default();
            let name = call.1.name.unwrap_or_default();
            out.push(AiEvent::ToolCall {
                call_id: id,
                name,
                args: call.1.args,
            });
        }
    }
}
