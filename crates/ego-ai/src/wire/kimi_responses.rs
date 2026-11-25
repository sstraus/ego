//! Moonshot Kimi Responses API: `POST /v1/responses`, streamed.
//!
//! Derived field by field from the public OpenAPI schema and guide at
//! `https://platform.kimi.ai/docs/api/responses` and
//! `https://platform.kimi.ai/docs/guide/context-caching`, read 2026-11-21.
//! This is vendor schema evidence, not a live service recording.
//!
//! The endpoint is stateless: `store` is always false and
//! `previous_response_id` and `conversation` are always null. Every turn
//! therefore replays the earlier output items unchanged in `input`, which is
//! also what keeps the request prefix cacheable. The request schema has no
//! `store`, `previous_response_id`, `parallel_tool_calls` or
//! `reasoning.encrypted_content` include, so none of them is sent.

use std::collections::BTreeMap;
use std::sync::Arc;

use ego_core::cost::Money;
use ego_core::generator::capabilities::CacheRetention;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, ReasoningState, Role, StopReason, ThinkingLevel,
};
use ego_core::state::Usage;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::provider_cost::ProviderCost;
use super::sse::SseDecoder;
use super::{RequestRefused, RequestTarget, WireFormat};

/// Joined onto the Moonshot `https://api.moonshot.ai/v1/` base.
pub const PATH: &str = "responses";
/// Replay dialect of reasoning items and search items this mapping mints.
pub const DIALECT: &str = "kimi-responses";
/// Documented `max_output_tokens` ceiling of the K3 series.
const MAX_OUTPUT_TOKENS: u64 = 1_048_576;
/// HMAC domain label of the cache affinity key; never sent in clear.
const CACHE_LABEL: &str = "kimi-api";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KimiResponses;

impl WireFormat for KimiResponses {
    /// Cache write is always on: an omitted `prompt_cache_options` is the
    /// documented 5m tier, so `None` retention still reports 300 seconds.
    fn prompt_cache_policy(&self, req: &GenRequest) -> ego_core::usage::PromptCachePolicy {
        let long = req
            .cache
            .as_ref()
            .is_some_and(|cache| cache.retention == CacheRetention::Long);
        ego_core::usage::PromptCachePolicy {
            ttl_seconds: std::num::NonZeroU64::new(if long { 3_600 } else { 300 }),
            refresh_on_read: true,
            lock_ttl: true,
        }
    }

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
        base.as_str() == crate::provider::KIMI_API_BASE
    }

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
        decode(response, cost)
    }
}

fn refused(why: impl Into<String>) -> RequestRefused {
    RequestRefused::new(format!("kimi-responses: {}", why.into()))
}

fn effort(level: ThinkingLevel) -> Result<&'static str, RequestRefused> {
    match level {
        ThinkingLevel::Low => Ok("low"),
        ThinkingLevel::High => Ok("high"),
        ThinkingLevel::Max => Ok("max"),
        other => Err(refused(format!(
            "reasoning effort {other:?} is not served; use low, high or max"
        ))),
    }
}

/// Replay one item this mapping minted, unchanged, as the guide requires.
fn replay(state: &ReasoningState) -> Result<Value, RequestRefused> {
    let foreign = || {
        refused(format!(
            "cannot replay provider state minted by {}; dropping it would break preserved reasoning",
            state.dialect
        ))
    };
    if state.dialect != DIALECT {
        return Err(foreign());
    }
    let item: Value = serde_json::from_str(&state.blob).map_err(|_| foreign())?;
    match item["type"].as_str() {
        Some("reasoning") if item["summary"].is_array() || item["content"].is_array() => Ok(item),
        _ => super::responses_search::replay(state, DIALECT).ok_or_else(foreign),
    }
}

/// One user message of `input_text`/`input_image` parts in block order. The
/// schema accepts data-URL images and documents no size or media-type limit.
fn flush_parts(items: &mut Vec<Value>, parts: &mut Vec<Value>) {
    if !parts.is_empty() {
        items.push(json!({"type":"message","role":"user","content":std::mem::take(parts)}));
    }
}

fn input_items(req: &GenRequest) -> Result<Vec<Value>, RequestRefused> {
    let mut items = Vec::new();
    for message in req.messages.messages() {
        let with_image = message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }));
        let mut parts = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Image { image } => {
                    parts.push(json!({
                        "type":"input_image",
                        "image_url":super::openai_completions::image_data_url(image)
                    }));
                    continue;
                }
                ContentBlock::Text { text } if with_image => {
                    parts.push(json!({"type":"input_text","text":text}));
                    continue;
                }
                _ => flush_parts(&mut items, &mut parts),
            }
            items.push(match block {
                ContentBlock::Text { text } => match message.role {
                    // `developer` is the documented system-instruction role.
                    Role::System => json!({"type":"message","role":"developer","content":text}),
                    Role::User => json!({"type":"message","role":"user","content":text}),
                    Role::Assistant => json!({
                        "type":"message","role":"assistant",
                        "content":[{"type":"output_text","text":text}]
                    }),
                },
                ContentBlock::ToolUse {
                    call_id,
                    name,
                    args,
                } => json!({"type":"function_call","call_id":call_id,"name":name,"arguments":args}),
                ContentBlock::ToolResult {
                    call_id, output, ..
                } => json!({"type":"function_call_output","call_id":call_id,"output":output}),
                ContentBlock::Reasoning { state } | ContentBlock::ProviderContent { state, .. } => {
                    replay(state)?
                }
                ContentBlock::Audio { .. } => {
                    return Err(refused("the request schema has no audio input part"));
                }
                ContentBlock::Image { .. } => unreachable!("image parts are collected above"),
            });
        }
        flush_parts(&mut items, &mut parts);
    }
    Ok(items)
}

pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, RequestRefused> {
    if req.parallel_tool_calls {
        return Err(refused(
            "the request schema has no parallel tool-call control",
        ));
    }
    if req
        .max_output_tokens
        .is_some_and(|cap| cap == 0 || cap > MAX_OUTPUT_TOKENS)
    {
        return Err(refused(format!(
            "max_output_tokens must be within 1..={MAX_OUTPUT_TOKENS}"
        )));
    }
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .filter(|tool| req.web_search.is_none() || tool.name != "web_search")
        .map(|tool| {
            json!({
                "type":"function","name":tool.name,
                "description":tool.description,"parameters":tool.parameters
            })
        })
        .collect();
    let mut body = json!({
        "model": req.model,
        "input": input_items(req)?,
        "stream": true,
        "reasoning": {"effort": effort(req.thinking)?},
    });
    let object = body.as_object_mut().expect("a JSON object literal");
    if req.web_search.is_some() {
        // No per-request search limit exists in the schema; only the tool.
        tools.push(json!({"type":"web_search"}));
        object.insert("include".into(), json!(["web_search_call.action.sources"]));
    }
    if !tools.is_empty() {
        object.insert("tools".into(), Value::Array(tools));
    }
    if let Some(cap) = req.max_output_tokens {
        object.insert("max_output_tokens".into(), cap.into());
    }
    if let Some(key) = super::cache::routing_key(req, CACHE_LABEL) {
        let ttl = match req.cache.as_ref().map(|cache| cache.retention) {
            Some(CacheRetention::Long) => "1h",
            _ => "5m",
        };
        object.insert("prompt_cache_key".into(), key.into());
        object.insert(
            "prompt_cache_options".into(),
            json!({"mode":"implicit","ttl":ttl}),
        );
    }
    Ok(serde_json::to_vec(&body).expect("a JSON value serialises"))
}

pub fn decode(
    response: Result<HttpResponse, HttpError>,
    cost: Option<Arc<dyn ProviderCost>>,
) -> BoxStream<'static, AiEvent> {
    super::decode_response(response, Box::new(StreamDecoder::new(cost)))
}

/// Partition `input_tokens`; writes at the echoed `1h` tier are extended.
/// An absent `input_tokens_details` is read as all uncached, the shared
/// budget-safe reading; the core usage type cannot state "unknown" here.
fn map_usage(usage: &Value, applied_ttl: Option<&str>) -> Option<Usage> {
    let count = |value: &Value| -> Option<u64> {
        if value.is_null() {
            Some(0)
        } else {
            value.as_u64()
        }
    };
    let input = usage["input_tokens"].as_u64()?;
    let details = &usage["input_tokens_details"];
    let read = count(&details["cached_tokens"])?;
    let written = count(&details["cache_write_tokens"])?;
    let uncached = input.checked_sub(read.checked_add(written)?)?;
    let extended = applied_ttl == Some("1h");
    Some(Usage {
        web_search_requests: 0,
        uncached_input_tokens: uncached,
        cache_read_tokens: read,
        cache_write_tokens: if extended { 0 } else { written },
        cache_write_extended_tokens: if extended { written } else { 0 },
        output_tokens: count(&usage["output_tokens"])?,
        reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"].as_u64(),
    })
}

#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

#[derive(Debug)]
struct StreamDecoder {
    search: super::responses_search::SearchItems,
    sse: SseDecoder,
    calls: BTreeMap<String, PartialCall>,
    finish: Option<StopReason>,
    failure: Option<(String, super::FailureClass)>,
    usage: Option<Usage>,
    usage_incoherent: bool,
    cost: Option<Arc<dyn ProviderCost>>,
    stated_cost: Option<Money>,
}

impl StreamDecoder {
    fn new(cost: Option<Arc<dyn ProviderCost>>) -> Self {
        Self {
            search: Default::default(),
            sse: SseDecoder::default(),
            calls: BTreeMap::new(),
            finish: None,
            failure: None,
            usage: None,
            usage_incoherent: false,
            cost,
            stated_cost: None,
        }
    }
}

impl super::StreamSink for StreamDecoder {
    fn usage_incoherent(&self) -> bool {
        self.usage_incoherent
    }

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
        if let Some((message, class)) = self.failure.take() {
            out.extend(super::streamed_failure(message, class));
        }
        for (_, call) in std::mem::take(&mut self.calls) {
            out.push(AiEvent::ToolCall {
                call_id: call.id.unwrap_or_default(),
                name: call.name.unwrap_or_default(),
                args: call.args,
            });
        }
        if let Some(usage) = self.usage.take() {
            out.push(AiEvent::Usage(usage));
        }
        if let Some(cost) = self.stated_cost.take() {
            out.push(AiEvent::ProviderCost { cost });
        }
        match self.finish {
            Some(stop) => out.push(AiEvent::Stop(stop)),
            None => {
                let kind = transport
                    .as_ref()
                    .map_or(ErrorKind::Transient, HttpError::kind);
                let message = match transport {
                    Some(error) => format!("the Kimi response stream failed: {error}"),
                    None => "the Kimi response stream ended without a terminal Responses event"
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
        let Ok(event) = serde_json::from_str::<Value>(frame) else {
            tracing::warn!("skipping a Kimi Responses event this mapping cannot decode");
            return;
        };
        self.search.capture(frame, DIALECT, out, None);
        let item = &event["item"];
        match event["type"].as_str().unwrap_or_default() {
            "response.output_item.done" if item["type"] == "reasoning" => {
                out.push(AiEvent::ReasoningState(ReasoningState {
                    connection: None,
                    dialect: DIALECT.into(),
                    blob: item.to_string(),
                }));
            }
            "response.output_item.done" if item["type"] == "web_search_call" => {
                for source in item["action"]["sources"].as_array().into_iter().flatten() {
                    if let Some(url) = source["url"].as_str() {
                        out.push(AiEvent::Citation {
                            url: url.into(),
                            title: source["title"].as_str().unwrap_or(url).into(),
                        });
                    }
                }
            }
            "response.output_text.delta" => {
                push_delta(&event, out, |text| AiEvent::TextDelta { text })
            }
            "response.reasoning_summary_text.delta" => {
                push_delta(&event, out, |text| AiEvent::ReasoningDelta { text })
            }
            "response.output_item.added" if item["type"] == "function_call" => {
                let key = item["id"]
                    .as_str()
                    .or(item["call_id"].as_str())
                    .map_or_else(|| format!("call-{}", self.calls.len()), str::to_owned);
                let call = self.calls.entry(key).or_default();
                call.id = item["call_id"].as_str().map(str::to_owned);
                call.name = item["name"].as_str().map(str::to_owned);
                call.args
                    .push_str(item["arguments"].as_str().unwrap_or_default());
            }
            "response.function_call_arguments.delta" => {
                let delta = event["delta"].as_str().unwrap_or_default();
                let call = match event["item_id"].as_str() {
                    Some(id) => self.calls.get_mut(id),
                    None => self.calls.values_mut().last(),
                };
                if let Some(call) = call {
                    call.args.push_str(delta);
                }
            }
            "response.completed" | "response.incomplete" => self.terminal(&event["response"]),
            "response.failed" | "error" => {
                let error = if event["response"]["error"].is_object() {
                    &event["response"]["error"]
                } else if event["error"].is_object() {
                    &event["error"]
                } else {
                    &event
                };
                self.fail_with(error);
            }
            _ => {}
        }
    }

    fn terminal(&mut self, response: &Value) {
        if response["usage"].is_object() {
            if let Some(cost) = self.cost.as_ref().and_then(|decoder| {
                serde_json::value::to_raw_value(&response["usage"])
                    .ok()
                    .and_then(|raw| decoder.read(&raw))
            }) {
                self.stated_cost = Some(cost);
            }
            match map_usage(
                &response["usage"],
                response["prompt_cache_options"]["ttl"].as_str(),
            ) {
                Some(mut usage) => {
                    usage.web_search_requests = self.search.observed();
                    self.usage = Some(usage);
                }
                None => self.usage_incoherent = true,
            }
        }
        match response["status"].as_str().unwrap_or("completed") {
            "incomplete" => match response["incomplete_details"]["reason"].as_str() {
                Some("content_filter") => self.fail(
                    "the provider's content filter stopped the generation".into(),
                    super::FailureClass {
                        kind: ErrorKind::Refused,
                        capacity: false,
                    },
                ),
                _ => self.finish = Some(StopReason::Length),
            },
            "failed" => self.fail_with(&response["error"]),
            _ if !self.calls.is_empty() => self.finish = Some(StopReason::ToolUse),
            _ => self.finish = Some(StopReason::Stop),
        }
    }

    fn fail_with(&mut self, error: &Value) {
        let code = error["code"].as_str();
        let message = error["message"].as_str().map_or_else(
            || match code {
                Some(code) => format!("Kimi reported streamed Responses error {code}"),
                None => "Kimi reported a streamed Responses error".to_string(),
            },
            str::to_owned,
        );
        self.fail(message, super::classify_stream_code(code));
    }

    fn fail(&mut self, message: String, class: super::FailureClass) {
        match &mut self.failure {
            None => self.failure = Some((message, class)),
            Some((_, first)) if first.kind == ErrorKind::Unclassified => *first = class,
            Some(_) => {}
        }
        self.finish = Some(StopReason::Error);
    }
}

fn push_delta(event: &Value, out: &mut Vec<AiEvent>, wrap: impl FnOnce(String) -> AiEvent) {
    if let Some(text) = event["delta"].as_str().filter(|text| !text.is_empty()) {
        out.push(wrap(text.to_owned()));
    }
}
