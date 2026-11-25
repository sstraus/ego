//! Exact GLM-5.3 API-key Chat contract (official schema read 2026-11-18).
//! The streamed tool contract uses string argument fragments. The synchronous
//! reference's object-valued arguments are not admitted by this streaming wire.
//! Native search follows the Chat API reference (read 2026-11-21), not the
//! guide or the standalone search endpoint where they disagree.

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ErrorKind, GenRequest, StopReason, ThinkingLevel};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::{Value, json};

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::openai_completions::{self, CommonRequest, StreamDecoder};
use super::provider_cost::ProviderCost;
use super::sse::SseDecoder;
use super::{RequestRefused, RequestTarget, StreamSink, WireFormat};

pub const ZAI_REASONING_DIALECT: &str = "zai-glm53-chat";
pub const PATH: &str = "chat/completions";

/// The only `search_engine` value in the Chat API reference's
/// `WebSearchToolSchema` (read 2026-11-21). The guide and the standalone
/// `/web_search` endpoint spell `search-prime`; the canonical Chat schema wins.
pub const SEARCH_ENGINE: &str = "search_pro_jina";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Zai;

impl WireFormat for Zai {
    // Encodes the documented in-chat tool. The Chat reference publishes the
    // `web_search` source array only on the non-streaming response, so this
    // SSE wire decodes no sources or search counts and fabricates none. No
    // default opt-in for that reason: `supports_default_web_search` stays false.
    fn supports_web_search(&self) -> bool {
        true
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
        // Keep shared transport certainty, policy, retry timing, safe diagnostic
        // rendering and failure capture. Only the documented dialect differs.
        futures::stream::once(async move {
            let (response, error_kind) = normalize_error(response).await;
            let inner = match cost {
                Some(cost) => StreamDecoder::reporting_cost(cost),
                None => StreamDecoder::default(),
            }
            .replaying_reasoning_content(ZAI_REASONING_DIALECT);
            super::decode_response(response, Box::new(ZaiStream::new(inner))).map(
                move |mut event| {
                    match (&mut event, error_kind) {
                        (AiEvent::Error { kind, .. }, Some(native)) => *kind = native,
                        (AiEvent::ProviderFailure { capacity, .. }, Some(_)) => *capacity = false,
                        _ => {}
                    }
                    event
                },
            )
        })
        .flatten()
        .boxed()
    }
}

#[derive(Debug, Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
    clear_thinking: bool,
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    #[serde(flatten)]
    common: CommonRequest<'a>,
    thinking: Thinking,
    tool_stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
}

pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, RequestRefused> {
    let effort = match req.thinking {
        ThinkingLevel::Low => "low",
        ThinkingLevel::High => "high",
        ThinkingLevel::Max => "max",
        _ => {
            return Err(RequestRefused::new(
                "GLM-5.3 always reasons and supports only low, high and max effort",
            ));
        }
    };
    if req
        .max_output_tokens
        .is_some_and(|cap| cap == 0 || cap > 131_072)
    {
        return Err(RequestRefused::new(
            "GLM-5.3 max_tokens must be within 1..=131072",
        ));
    }
    if req.parallel_tool_calls {
        return Err(RequestRefused::new(
            "GLM-5.3 has no admitted parallel tool-call control",
        ));
    }
    if req.cache.as_ref().is_some_and(|cache| {
        cache.retention != ego_core::generator::capabilities::CacheRetention::None
    }) {
        return Err(RequestRefused::new(
            "this Z.ai contract has only implicit caching and no explicit cache controls",
        ));
    }
    if super::request_has_images(req) || super::request_has_audio(req) {
        return Err(RequestRefused::new(
            "this GLM-5.3 contract supports text and tools only",
        ));
    }
    // The catalogue's hosted-search slot is answered by the native tool, never
    // declared as a local function the model could call.
    let hosted_slot = |name: &str| req.web_search.is_some() && name == "web_search";
    let wire = WireRequest {
        common: openai_completions::common_request_preserving_reasoning(
            req,
            ZAI_REASONING_DIALECT,
        )?
        .without_parallel_tool_calls()
        .without_stream_options()
        .with_reasoning_effort(Some(effort)),
        thinking: Thinking {
            kind: "enabled",
            clear_thinking: false,
        },
        // Documented for streamed function calls only.
        tool_stream: req
            .tools
            .iter()
            .any(|tool| !hosted_slot(tool.name.as_str())),
        max_tokens: req.max_output_tokens,
    };
    if req.web_search.is_none() {
        return Ok(serde_json::to_vec(&wire).expect("WireRequest serialises JSON"));
    }
    // `max_requests` and `allow_live_access` have no Chat field: `count` is
    // results per search, and the route has no cached-only search mode. The
    // native tool is appended last so the function-tool prefix stays stable.
    let mut body = serde_json::to_value(&wire).expect("WireRequest serialises JSON");
    let tools = body
        .as_object_mut()
        .expect("WireRequest is a JSON object")
        .entry("tools")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("tools is a JSON array");
    tools.retain(|tool| !tool["function"]["name"].as_str().is_some_and(hosted_slot));
    tools.push(json!({
        "type": "web_search",
        "web_search": {"enable": true, "search_engine": SEARCH_ENGINE}
    }));
    Ok(serde_json::to_vec(&body).expect("request body serialises JSON"))
}

// The endpoint reference publishes a flat numeric error, while the errors guide
// publishes a nested string-code envelope. Admit both documented spellings;
// forward only typed fields into the existing bounded/redacted diagnostic path.
// Code 1113 is a terminal empty balance, NOT a retryable HTTP 429 rate limit.
async fn normalize_error(
    response: Result<HttpResponse, HttpError>,
) -> (Result<HttpResponse, HttpError>, Option<ErrorKind>) {
    let mut response = match response {
        Ok(response) => response,
        Err(error) => return (Err(error), None),
    };
    if response.status == 200 {
        return (Ok(response), None);
    }
    let mut error_kind = None;
    let mut raw = Vec::new();
    let mut body_error = None;
    while let Some(chunk) = response.body.next().await {
        match chunk {
            Ok(bytes) => {
                let room = (64 * 1024_usize).saturating_sub(raw.len());
                raw.extend_from_slice(&bytes[..bytes.len().min(room)]);
                if bytes.len() >= room {
                    break;
                }
            }
            Err(error) => {
                body_error = Some(error);
                break;
            }
        }
    }
    if let Ok(value) = serde_json::from_slice::<Value>(&raw) {
        let error = value.get("error").unwrap_or(&value);
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            let code = error.get("code").cloned().unwrap_or(Value::Null);
            let number = code.as_u64().or_else(|| code.as_str()?.parse().ok());
            let quota_code = match number {
                Some(1113) => "credit_balance_exhausted",
                Some(1308 | 1310 | 1316..=1321) => "usage_limit_reached",
                _ => "",
            };
            error_kind = match number {
                Some(1261) => Some(ErrorKind::ContextOverflow),
                Some(1000 | 1001 | 1003 | 1005 | 1113 | 1220 | 1308..=1311 | 1313..=1321) => {
                    Some(ErrorKind::Refused)
                }
                _ => None,
            };
            // Bound only after redaction, so a credential is never truncated
            // into an unrecognizable fragment before the safe host boundary.
            let message: String = ego_core::permit::shell::redact_credentials(message)
                .chars()
                .take(1024)
                .collect();
            let code = match code {
                Value::String(text) => Value::String(
                    ego_core::permit::shell::redact_credentials(&text)
                        .chars()
                        .take(64)
                        .collect(),
                ),
                Value::Number(number) => Value::Number(number),
                _ => Value::Null,
            };
            raw = serde_json::to_vec(&json!({"error": {
                "message": message,
                "code": code,
                "details": {"error_code": quota_code}
            }}))
            .expect("typed error serialises JSON");
        }
    }
    // Preserve body/filter failures for the shared decoder instead of making a
    // failed error-body read look like a successful empty response.
    if body_error
        .as_ref()
        .is_some_and(HttpError::is_protocol_filter)
    {
        error_kind = None;
    }
    response.body = futures::stream::iter(
        std::iter::once(Ok(bytes::Bytes::from(raw))).chain(body_error.map(Err)),
    )
    .boxed();
    (Ok(response), error_kind)
}

/// A small exact frame projection, not a second tool/usage decoder. Failure
/// terminals are intercepted BEFORE shared Chat can seal reasoning/dispatch
/// pending tool calls. Only successful frames reach its existing assembler.
struct ZaiStream {
    sse: SseDecoder,
    inner: StreamDecoder,
    failure: Option<(String, ErrorKind)>,
    terminal: bool,
    bytes_since_dispatch: usize,
}

impl ZaiStream {
    fn new(inner: StreamDecoder) -> Self {
        Self {
            sse: SseDecoder::default(),
            inner,
            failure: None,
            terminal: false,
            bytes_since_dispatch: 0,
        }
    }

    fn frame(&mut self, frame: &str) -> Vec<AiEvent> {
        if frame == "[DONE]" {
            return self.inner.push(b"data: [DONE]\n\n");
        }
        let Ok(mut value) = serde_json::from_str::<Value>(frame) else {
            self.failure = Some((
                "Z.ai returned a malformed Chat event".into(),
                ErrorKind::Unclassified,
            ));
            return Vec::new();
        };
        let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
            self.failure = Some((
                "Z.ai returned a Chat event without choices".into(),
                ErrorKind::Unclassified,
            ));
            return Vec::new();
        };
        if !choices.iter().all(valid_choice) {
            self.failure = Some((
                "Z.ai returned a malformed Chat delta".into(),
                ErrorKind::Unclassified,
            ));
            return Vec::new();
        }
        for choice in choices
            .iter_mut()
            .filter(|choice| choice.get("index").and_then(Value::as_u64).unwrap_or(0) == 0)
        {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if self.terminal {
                    // A second terminal cannot supersede a completed turn.
                    choice["finish_reason"] = Value::Null;
                    continue;
                }
                self.terminal = true;
                let kind = match reason {
                    "stop" | "length" | "tool_calls" => None,
                    "network_error" => Some(ErrorKind::Transient),
                    "model_context_window_exceeded" => Some(ErrorKind::ContextOverflow),
                    "sensitive" => Some(ErrorKind::Refused),
                    _ => Some(ErrorKind::Unclassified),
                };
                if let Some(kind) = kind {
                    let message = match reason {
                        "network_error" | "model_context_window_exceeded" | "sensitive" => {
                            format!("Z.ai ended generation with finish_reason {reason}")
                        }
                        _ => "Z.ai ended generation with an unknown finish_reason".into(),
                    };
                    self.failure = Some((message, kind));
                    break;
                }
            }
        }
        if self.failure.is_some() {
            // Keep genuine terminal usage, but do not feed more deltas/calls
            // after a failed terminal into a decoder that could mint state.
            value["choices"] = json!([]);
        }
        let frame = format!("data: {value}\n\n");
        self.inner.push(frame.as_bytes())
    }
}

// Reject a malformed known field before the shared permissive family decoder
// could skip it and later seal an incomplete reasoning/tool payload. This checks
// shape only; identities, fragment assembly and usage remain shared.
fn valid_choice(choice: &Value) -> bool {
    let optional_string =
        |value: Option<&Value>| value.is_none_or(|value| value.is_null() || value.is_string());
    if !choice.is_object()
        || choice.get("index").is_some_and(|value| {
            !value
                .as_u64()
                .is_some_and(|index| u32::try_from(index).is_ok())
        })
        || !optional_string(choice.get("finish_reason"))
    {
        return false;
    }
    let Some(delta) = choice.get("delta") else {
        return true;
    };
    if !delta.is_object()
        || !["content", "reasoning_content", "reasoning", "refusal"]
            .iter()
            .all(|field| optional_string(delta.get(field)))
    {
        return false;
    }
    let Some(calls) = delta.get("tool_calls") else {
        return true;
    };
    let Some(calls) = calls.as_array() else {
        return false;
    };
    calls.iter().all(|call| {
        call.is_object()
            && call.get("index").is_none_or(|index| {
                index
                    .as_u64()
                    .is_some_and(|index| u32::try_from(index).is_ok())
            })
            && optional_string(call.get("id"))
            && call
                .get("type")
                .is_none_or(|kind| kind.as_str() == Some("function"))
            && call.get("function").is_none_or(|function| {
                function.is_null()
                    || (function.is_object()
                        && optional_string(function.get("name"))
                        && optional_string(function.get("arguments")))
            })
    })
}

impl StreamSink for ZaiStream {
    fn push(&mut self, chunk: &[u8]) -> Vec<AiEvent> {
        let mut events = Vec::new();
        // Bound a single uncompleted frame without rejecting a large HTTP
        // chunk that merely contains many small complete SSE events.
        for piece in chunk.chunks(8192) {
            self.bytes_since_dispatch = self.bytes_since_dispatch.saturating_add(piece.len());
            if self.bytes_since_dispatch > 1024 * 1024 {
                self.failure = Some((
                    "Z.ai Chat event exceeded the 1 MiB frame bound".into(),
                    ErrorKind::Unclassified,
                ));
                break;
            }
            let frames = self.sse.push(piece);
            if !frames.is_empty() {
                // At most this piece is a tail following its last dispatch.
                self.bytes_since_dispatch = piece.len();
            }
            for frame in frames {
                events.extend(self.frame(&frame));
            }
        }
        events
    }

    fn finish(self: Box<Self>, transport: Option<HttpError>) -> Vec<AiEvent> {
        let partial = self.sse.has_partial_frame();
        let failed = self.failure;
        let mut events = self.inner.finish(transport);
        if let Some((message, kind)) = failed {
            events.retain(|event| {
                !matches!(
                    event,
                    AiEvent::Error { .. }
                        | AiEvent::Stop(_)
                        | AiEvent::ToolCall { .. }
                        | AiEvent::ReasoningState(_)
                )
            });
            events.extend(super::streamed_failure(
                message,
                super::FailureClass {
                    kind,
                    capacity: false,
                },
            ));
            events.push(AiEvent::Stop(StopReason::Error));
        } else if partial && !self.terminal {
            for event in &mut events {
                if let AiEvent::Error { message, .. } = event {
                    *message =
                        "Z.ai stream ended in a partial Chat event before a finish_reason".into();
                }
            }
        }
        events
    }

    fn usage_incoherent(&self) -> bool {
        self.inner.usage_incoherent()
    }
}
