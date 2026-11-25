//! Native Converse JSON and checksummed ConverseStream, for the bounded
//! text/client-tool contract documented in the implementation report.
//!
//! Tool-bearing turns use synchronous Converse: the selected Llama contract
//! rejects ConverseStream with toolConfig. This mapping owns bytes, not model
//! selection, credentials, sockets, admission, cancellation or prices.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, Role, StopReason, ThinkingLevel,
};
use ego_core::permit::HttpHeader;
use ego_core::state::Usage;
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::http::{HttpError, HttpRequest, HttpResponse, Method};

use super::provider_cost::ProviderCost;
use super::{FailureClass, RequestRefused, RequestTarget, StreamSink, WireFormat};

/// Text streaming, with non-streaming client-tool turns and continuations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BedrockConverse;

impl WireFormat for BedrockConverse {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        let body = request_body(req)?;
        // A historical tool result must not accidentally switch to streaming
        // when the caller has removed the current tool definitions.
        let synchronous = !req.tools.is_empty()
            || req.messages.messages().iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
                    )
                })
            });
        // Smithy serializes modelId as a single URI label. Escape reserved
        // bytes here, before SigV4 canonicalizes the completed URL.
        const MODEL_ID_ESCAPES: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
            .remove(b'-')
            .remove(b'.')
            .remove(b'_')
            .remove(b'~');
        let model = percent_encoding::utf8_percent_encode(&req.model, MODEL_ID_ESCAPES);
        let operation = if synchronous {
            "converse"
        } else {
            "converse-stream"
        };
        let path = format!("model/{model}/{operation}");
        let mut headers = vec![
            HttpHeader::new("content-type", b"application/json".to_vec()).expect("static header"),
            HttpHeader::new(
                "accept",
                if synchronous {
                    b"application/json".to_vec()
                } else {
                    b"application/vnd.amazon.eventstream".to_vec()
                },
            )
            .expect("static header"),
        ];
        headers.extend(target.headers().iter().cloned());
        Ok(HttpRequest {
            url: target.url(&path),
            method: Method::Post,
            headers,
            body: Some(body),
        })
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        _cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        let mode = response.as_ref().ok().and_then(|response| {
            response
                .headers
                .iter()
                .find(|header| header.name().eq_ignore_ascii_case("content-type"))
                .and_then(|header| std::str::from_utf8(header.value()).ok())
                .map(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
        });
        let mode = match mode.as_deref() {
            Some("application/json") => Mode::Json,
            Some("application/vnd.amazon.eventstream") => Mode::Events,
            _ => Mode::Unsupported,
        };
        super::decode_response(response, Box::new(Decoder::new(mode)))
    }
}

fn refused(reason: &str) -> RequestRefused {
    RequestRefused::new(format!("native Converse: {reason}"))
}

fn identifier(value: &str, tool_name: bool) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || byte == b'_'
                || byte == b'-'
                || (!tool_name && matches!(byte, b'.' | b':'))
        })
}

/// Schema-derived text/client-tool request; no model ID belongs in the body.
pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, RequestRefused> {
    if req.model.is_empty() || req.model.len() > 2048 || req.model == "." || req.model == ".." {
        return Err(refused("a nonempty model resource ID is required"));
    }
    if req.thinking != ThinkingLevel::Off {
        return Err(refused("this contract does not express thinking controls"));
    }
    if req.web_search.is_some()
        || req.cache.as_ref().is_some_and(|cache| {
            cache.retention != ego_core::generator::capabilities::CacheRetention::None
        })
    {
        return Err(refused(
            "this contract does not express hosted search or prompt-cache controls",
        ));
    }
    if req
        .max_output_tokens
        .is_some_and(|ceiling| ceiling == 0 || ceiling > i32::MAX as u64)
    {
        return Err(refused(
            "maxTokens must be a positive signed 32-bit integer",
        ));
    }
    // This flag is a model fact in the production request, not an independent
    // at-most-one-call demand. Converse has no parallel_tool_calls control to
    // emit; accepting false does not promise server-side serial cardinality.
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in req.messages.messages() {
        let mut content = Vec::new();
        for block in &message.content {
            content.push(match block {
                ContentBlock::Text { text } => {
                    if text.is_empty() {
                        return Err(refused("empty text is not supported"));
                    }
                    json!({ "text": text })
                }
                ContentBlock::ToolUse {
                    call_id,
                    name,
                    args,
                } if message.role == Role::Assistant => {
                    if !identifier(call_id, false) || !identifier(name, true) {
                        return Err(refused("tool name or toolUseId violates the native schema"));
                    }
                    let input: Value = serde_json::from_str(args)
                        .map_err(|_| refused("tool input must be complete JSON"))?;
                    json!({ "toolUse": { "toolUseId": call_id, "name": name, "input": input } })
                }
                ContentBlock::ToolResult {
                    call_id,
                    output,
                    is_error,
                } if message.role == Role::User => {
                    if !identifier(call_id, false) {
                        return Err(refused("invalid toolUseId"));
                    }
                    // Llama does not support the native status field. This is
                    // our explicit model-visible text projection, not a claim
                    // that AWS defines an error marker inside content.text.
                    let text = if *is_error {
                        format!("The tool failed:\n{output}")
                    } else {
                        output.clone()
                    };
                    if text.is_empty() {
                        return Err(refused("empty tool-result text is not supported"));
                    }
                    json!({ "toolResult": { "toolUseId": call_id, "content": [{ "text": text }] } })
                }
                _ => return Err(refused("unsupported role, opaque state or input modality")),
            });
        }
        if content.is_empty() {
            return Err(refused("empty message content is not supported"));
        }
        if message.role == Role::System {
            if !messages.is_empty() {
                return Err(refused(
                    "system instructions must precede conversation messages",
                ));
            }
            system.extend(content);
        } else {
            messages.push(json!({ "role": if message.role == Role::User { "user" } else { "assistant" }, "content": content }));
        }
    }
    if messages.is_empty() {
        return Err(refused("at least one conversation message is required"));
    }
    let mut body = json!({ "messages": messages });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if let Some(max_tokens) = req.max_output_tokens {
        body["inferenceConfig"] = json!({ "maxTokens": max_tokens });
    }
    if !req.tools.is_empty() {
        let mut tools = Vec::new();
        for tool in &req.tools {
            if !identifier(&tool.name, true)
                || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
            {
                return Err(refused(
                    "tool names and top-level object input schemas must satisfy the native schema",
                ));
            }
            let mut spec = json!({ "name": tool.name, "inputSchema": { "json": tool.parameters } });
            if !tool.description.is_empty() {
                spec["description"] = json!(tool.description);
            }
            tools.push(json!({ "toolSpec": spec }));
        }
        body["toolConfig"] = json!({ "tools": tools });
    }
    serde_json::to_vec(&body).map_err(|_| refused("request serialization failed"))
}

// A local allocation budget, not enforcement of Smithy's service-side limits.
const MAX_BUFFER: usize = 64 * 1024 * 1024;
const MAX_BLOCKS: usize = 65_536;
type DecodeResult<T> = Result<T, &'static str>;

#[derive(Clone, Copy)]
enum Mode {
    Json,
    Events,
    Unsupported,
}

struct Decoder {
    mode: Mode,
    buffer: Vec<u8>,
    failed: bool,
    started: bool,
    closed_blocks: BTreeSet<u64>,
    active_block: Option<u64>,
    stop: Option<StopReason>,
    metadata: bool,
}

impl Decoder {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            buffer: Vec::new(),
            failed: false,
            started: false,
            closed_blocks: BTreeSet::new(),
            active_block: None,
            stop: None,
            metadata: false,
        }
    }

    fn fail(&mut self, message: &str) -> Vec<AiEvent> {
        self.failed = true;
        self.buffer.clear();
        vec![AiEvent::Error {
            message: format!("native Converse: {message}"),
            kind: ErrorKind::Unclassified,
        }]
    }

    fn event(&mut self, frame: &[u8]) -> DecodeResult<Vec<AiEvent>> {
        let headers_len =
            u32::from_be_bytes(frame[4..8].try_into().expect("validated prelude")) as usize;
        let headers = headers(&frame[12..12 + headers_len])?;
        let get = |name: &str| headers.get(name).and_then(Option::as_deref);
        let payload = &frame[12 + headers_len..frame.len() - 4];
        match get(":message-type") {
            Some("exception") => {
                let code = get(":exception-type").ok_or("missing exception type")?;
                let value: Value =
                    serde_json::from_slice(payload).map_err(|_| "invalid exception JSON")?;
                let message = value.get("message").and_then(Value::as_str).unwrap_or(code);
                self.failed = true;
                Ok(super::streamed_failure(message.to_string(), exception_class(code)).to_vec())
            }
            Some("error") => {
                let code = get(":error-code").ok_or("missing error code")?;
                let message = get(":error-message").ok_or("missing error message")?;
                self.failed = true;
                Ok(super::streamed_failure(message.to_string(), exception_class(code)).to_vec())
            }
            Some("event") => {
                if get(":content-type") != Some("application/json") {
                    return Err("unsupported event payload type");
                }
                let kind = get(":event-type").ok_or("missing event type")?;
                let value: Value =
                    serde_json::from_slice(payload).map_err(|_| "invalid event JSON")?;
                self.message_event(kind, &value)
            }
            _ => Err("missing or unsupported message type"),
        }
    }

    fn message_event(&mut self, kind: &str, value: &Value) -> DecodeResult<Vec<AiEvent>> {
        match kind {
            "messageStart" if !self.started => {
                if value.get("role").and_then(Value::as_str) != Some("assistant") {
                    return Err("response role is not assistant");
                }
                self.started = true;
                Ok(Vec::new())
            }
            "contentBlockDelta" if self.started && self.stop.is_none() => {
                let index = block_index(value)?;
                if self.closed_blocks.len() >= MAX_BLOCKS {
                    return Err("response exceeds local content-block budget");
                }
                if self.closed_blocks.contains(&index)
                    || self.active_block.is_some_and(|active| active != index)
                {
                    return Err("out-of-order content block");
                }
                let delta = value.get("delta").ok_or("missing content delta")?;
                let text = single_member(delta, "text")?
                    .as_str()
                    .ok_or("text delta is not a string")?;
                self.active_block = Some(index);
                Ok(vec![AiEvent::TextDelta {
                    text: text.to_string(),
                }])
            }
            "contentBlockStop"
                if self.started && self.stop.is_none() && self.active_block.is_some() =>
            {
                let index = block_index(value)?;
                if self.active_block != Some(index) {
                    return Err("out-of-order block stop");
                }
                self.closed_blocks.insert(index);
                self.active_block = None;
                Ok(Vec::new())
            }
            // Tool start/partial JSON is deliberately not a streaming contract
            // of this mapping. A synchronous turn decodes whole toolUse blocks.
            "messageStop" if self.started && self.stop.is_none() && self.active_block.is_none() => {
                let (stop, events) = stop_reason(
                    value
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .ok_or("missing stop reason")?,
                )?;
                if stop == StopReason::ToolUse {
                    return Err("tool use is unsupported in this stream contract");
                }
                self.stop = Some(stop);
                Ok(events)
            }
            "metadata" if self.stop.is_some() && !self.metadata => {
                validate_metrics(value.get("metrics").ok_or("missing metrics")?)?;
                let usage = usage(value.get("usage").ok_or("missing usage")?)?;
                self.metadata = true;
                Ok(vec![AiEvent::Usage(usage)])
            }
            _ => Err("unsupported event or invalid event order"),
        }
    }
}

impl StreamSink for Decoder {
    fn push(&mut self, mut chunk: &[u8]) -> Vec<AiEvent> {
        if self.failed {
            return Vec::new();
        }
        if matches!(self.mode, Mode::Unsupported) {
            return self.fail("unsupported or missing response content type");
        }
        if matches!(self.mode, Mode::Json) {
            if chunk.len() > MAX_BUFFER.saturating_sub(self.buffer.len()) {
                return self.fail("response exceeds local buffer budget");
            }
            self.buffer.extend_from_slice(chunk);
            return Vec::new();
        }
        let mut events = Vec::new();
        // Copy only the current frame, so coalesced frames do not multiply the
        // allocation budget and a bogus length cannot request an allocation.
        while !chunk.is_empty() && !self.failed {
            let needed = if self.buffer.len() < 12 {
                12
            } else {
                match frame_length(&self.buffer) {
                    Ok(length) => length,
                    Err(message) => {
                        events.extend(self.fail(message));
                        break;
                    }
                }
            };
            let take = (needed - self.buffer.len()).min(chunk.len());
            self.buffer.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.buffer.len() < 12 {
                continue;
            }
            let length = match frame_length(&self.buffer) {
                Ok(length) => length,
                Err(message) => {
                    events.extend(self.fail(message));
                    break;
                }
            };
            if self.buffer.len() < length {
                continue;
            }
            if crc32(&self.buffer[..length - 4])
                != u32::from_be_bytes(self.buffer[length - 4..].try_into().expect("CRC size"))
            {
                events.extend(self.fail("message CRC mismatch"));
                break;
            }
            let frame = std::mem::take(&mut self.buffer);
            match self.event(&frame) {
                Ok(decoded) => events.extend(decoded),
                Err(message) => events.extend(self.fail(message)),
            }
        }
        events
    }

    fn finish(mut self: Box<Self>, transport: Option<HttpError>) -> Vec<AiEvent> {
        let mut events = Vec::new();
        if let Some(error) = transport {
            self.failed = true;
            events.push(AiEvent::DeliveryObserved(error.delivery()));
            events.push(AiEvent::Error {
                message: error.to_string(),
                kind: error.kind(),
            });
        }
        if !self.failed {
            match self.mode {
                Mode::Json => match synchronous_response(&self.buffer) {
                    Ok((decoded, stop)) => {
                        events.extend(decoded);
                        self.stop = Some(stop);
                    }
                    Err(message) => events.extend(self.fail(message)),
                },
                Mode::Events if !self.buffer.is_empty() => {
                    events.extend(self.fail("truncated event frame"))
                }
                Mode::Events if self.stop.is_none() || !self.metadata => {
                    events.extend(self.fail("incomplete response event sequence"))
                }
                Mode::Events => {}
                Mode::Unsupported => {
                    events.extend(self.fail("unsupported or missing response content type"))
                }
            }
        }
        events.push(AiEvent::Stop(if self.failed {
            StopReason::Error
        } else {
            self.stop.unwrap_or(StopReason::Error)
        }));
        events
    }
}

fn frame_length(prelude: &[u8]) -> DecodeResult<usize> {
    let total = u32::from_be_bytes(prelude[..4].try_into().expect("prelude size")) as usize;
    let headers = u32::from_be_bytes(prelude[4..8].try_into().expect("prelude size")) as usize;
    if crc32(&prelude[..8]) != u32::from_be_bytes(prelude[8..12].try_into().expect("prelude size"))
    {
        return Err("prelude CRC mismatch");
    }
    if total < 16 || headers > total - 16 {
        return Err("invalid event frame lengths");
    }
    if total > MAX_BUFFER {
        return Err("event frame exceeds local buffer budget");
    }
    Ok(total)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn take<'a>(bytes: &mut &'a [u8], length: usize) -> DecodeResult<&'a [u8]> {
    if length > bytes.len() {
        return Err("truncated event header");
    }
    let (value, rest) = bytes.split_at(length);
    *bytes = rest;
    Ok(value)
}

// Validate every header's native type, including unknown extension names.
// Only string-valued semantic headers are retained for the event projection.
fn headers(mut bytes: &[u8]) -> DecodeResult<BTreeMap<String, Option<String>>> {
    let mut result = BTreeMap::new();
    let mut names = BTreeSet::new();
    while !bytes.is_empty() {
        let length = usize::from(take(&mut bytes, 1)?[0]);
        if length == 0 {
            return Err("empty event header name");
        }
        let name = std::str::from_utf8(take(&mut bytes, length)?)
            .map_err(|_| "invalid header name UTF-8")?
            .to_string();
        if !names.insert(name.clone()) {
            return Err("duplicate event header");
        }
        let kind = take(&mut bytes, 1)?[0];
        let value = match kind {
            0 | 1 => None,
            2..=5 | 8 | 9 => {
                let length = match kind {
                    2 => 1,
                    3 => 2,
                    4 => 4,
                    5 | 8 => 8,
                    _ => 16,
                };
                take(&mut bytes, length)?;
                None
            }
            6 | 7 => {
                let length = u16::from_be_bytes(
                    take(&mut bytes, 2)?.try_into().expect("header length size"),
                ) as usize;
                let value = take(&mut bytes, length)?;
                if kind == 7 {
                    Some(
                        std::str::from_utf8(value)
                            .map_err(|_| "invalid string header UTF-8")?
                            .to_string(),
                    )
                } else {
                    None
                }
            }
            _ => return Err("unknown event header type"),
        };
        result.insert(name, value);
    }
    Ok(result)
}

fn block_index(value: &Value) -> DecodeResult<u64> {
    value
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .filter(|index| *index <= i32::MAX as u64)
        .ok_or("invalid content block index")
}

fn single_member<'a>(value: &'a Value, member: &str) -> DecodeResult<&'a Value> {
    let object = value.as_object().ok_or("expected union object")?;
    if object.len() != 1 {
        return Err("content union must have exactly one member");
    }
    object.get(member).ok_or("unsupported content union member")
}

fn stop_reason(reason: &str) -> DecodeResult<(StopReason, Vec<AiEvent>)> {
    let stop = match reason {
        "end_turn" | "stop_sequence" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        "model_context_window_exceeded"
        | "guardrail_intervened"
        | "content_filtered"
        | "malformed_model_output"
        | "malformed_tool_use" => {
            return Ok((
                StopReason::Error,
                super::streamed_failure(
                    format!("native Converse stopped: {reason}"),
                    FailureClass {
                        kind: if reason == "model_context_window_exceeded" {
                            ErrorKind::ContextOverflow
                        } else {
                            ErrorKind::Unclassified
                        },
                        capacity: false,
                    },
                )
                .to_vec(),
            ));
        }
        _ => return Err("unknown stop reason"),
    };
    Ok((stop, Vec::new()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
}

fn usage(value: &Value) -> DecodeResult<Usage> {
    let usage: TokenUsage =
        serde_json::from_value(value.clone()).map_err(|_| "invalid token usage")?;
    // Cache accounting is not part of this selected non-cache model contract.
    if usage.cache_read_input_tokens != 0 || usage.cache_write_input_tokens != 0 {
        return Err("cache usage is outside this contract");
    }
    if usage.input_tokens.checked_add(usage.output_tokens) != Some(usage.total_tokens) {
        return Err("inconsistent token usage total");
    }
    Ok(Usage {
        uncached_input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_extended_tokens: 0,
        web_search_requests: 0,
        reasoning_tokens: None,
    })
}

fn validate_metrics(value: &Value) -> DecodeResult<()> {
    if value.get("latencyMs").and_then(Value::as_u64).is_none() {
        return Err("invalid latency metrics");
    }
    Ok(())
}

fn synchronous_response(bytes: &[u8]) -> DecodeResult<(Vec<AiEvent>, StopReason)> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| "invalid or truncated Converse JSON")?;
    let message = single_member(value.get("output").ok_or("missing output")?, "message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err("response role is not assistant");
    }
    let mut events = Vec::new();
    let mut calls = false;
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .ok_or("missing content")?
    {
        let object = block.as_object().ok_or("expected content object")?;
        if object.len() != 1 {
            return Err("content union must have exactly one member");
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            events.push(AiEvent::TextDelta {
                text: text.to_string(),
            });
        } else if let Some(tool) = block.get("toolUse") {
            let call_id = tool
                .get("toolUseId")
                .and_then(Value::as_str)
                .ok_or("missing toolUseId")?;
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or("missing tool name")?;
            if !identifier(call_id, false) || !identifier(name, true) || tool.get("type").is_some()
            {
                return Err("unsupported tool identity or server tool");
            }
            let input = tool.get("input").ok_or("missing tool input")?;
            events.push(AiEvent::ToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                args: input.to_string(),
            });
            calls = true;
        } else {
            return Err("unsupported response content modality or state");
        }
    }
    let (stop, failure) = stop_reason(
        value
            .get("stopReason")
            .and_then(Value::as_str)
            .ok_or("missing stop reason")?,
    )?;
    if stop != StopReason::Error && calls != (stop == StopReason::ToolUse) {
        return Err("tool calls and stop reason disagree");
    }
    events.extend(failure);
    validate_metrics(value.get("metrics").ok_or("missing metrics")?)?;
    events.push(AiEvent::Usage(usage(
        value.get("usage").ok_or("missing usage")?,
    )?));
    Ok((events, stop))
}

fn exception_class(code: &str) -> FailureClass {
    match code {
        "throttlingException" | "serviceUnavailableException" => FailureClass {
            kind: ErrorKind::Transient,
            capacity: true,
        },
        "internalServerException" | "modelStreamErrorException" => FailureClass {
            kind: ErrorKind::Transient,
            capacity: false,
        },
        "validationException" => FailureClass {
            kind: ErrorKind::Invalid,
            capacity: false,
        },
        _ => FailureClass {
            kind: ErrorKind::Unclassified,
            capacity: false,
        },
    }
}
