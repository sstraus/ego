//! The `mistral-conversations` wire format: `POST /v1/conversations`.
//!
//! # Why a separate mapping
//!
//! Native `web_search` exists only on this route. The official Web Search
//! page excludes Chat Completions because "Chat Completions responses don't
//! include the search result references", and the public `/v1/responses`
//! schema accepts only function and namespace tools. The request is a list of
//! *entries*, not chat messages, and the stream is a typed event model. A
//! Chat adapter cannot express either.
//!
//! # Sources and evidence class
//!
//! Public `openapi.yaml`, SHA-256 `7652f1307b3bc6cb…dfae`, components
//! `ConversationRequestBase`, `InputEntries`, `ConversationEvents`,
//! `MessageOutputEvent`, `ToolExecution*Event`, `FunctionCallEvent`,
//! `ResponseDoneEvent`, `ResponseErrorEvent`, `ToolReferenceChunk`,
//! `ThinkChunk`, `CompletionArgs` and `ConversationUsageInfo`. Prose: the
//! Web Search, Agents & Conversations and migration pages on
//! `docs.mistral.ai`. No recorded byte capture exists, so every decoder
//! fixture is schema-derived.
//!
//! # Stateless, and it says so
//!
//! `store` is `true` unless the request says otherwise. ego rebuilds the whole
//! history from its own log on every turn, so each request states
//! `store: false`, posts to `conversations` and never names a
//! `conversation_id`. The schema accepts prior `message.output`,
//! `function.call` and `function.result` entries in `inputs`.
//!
//! # What is replayed
//!
//! Each completed `message.output` entry is kept as provider content and
//! replayed as an entry, so its thinking chunks and `tool_reference` citations
//! survive the next turn. Hosted `tool.execution` entries are not replayed,
//! which follows the migration guide: its history helper "leaves out the
//! entries of hosted tools and handoffs; the model still reads the replies that
//! used them".
//!
//! # Prompt cache
//!
//! `ConversationRequestBase` has no `prompt_cache_key`, retention or
//! breakpoint field, and `ConversationUsageInfo` (`additionalProperties:
//! false`) has no cached-token counter. This mapping therefore sends no cache
//! control, claims no cache request identity, and reports no cache reads. The
//! zero in [`Usage::cache_read_tokens`] is the shared type's only spelling and
//! is not a measurement.

use std::collections::BTreeSet;
use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, Message, ReasoningState, Role, StopReason, ThinkingLevel,
};
use ego_core::state::Usage;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::provider_cost::ProviderCost;
use super::sse::SseDecoder;
use super::{RequestRefused, RequestTarget, WireFormat};

/// The `mistral-conversations` mapping, as a value a profile can name.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MistralConversations;

/// The official API base a provider row joins [`PATH`] onto.
pub const BASE: &str = "https://api.mistral.ai/v1/";

/// The Conversations path. Streaming is negotiated by `stream: true`.
pub const PATH: &str = "conversations";

/// Which dialect this mapping's provider-content blobs are written in.
pub const DIALECT: &str = "mistral-conversations";

impl WireFormat for MistralConversations {
    fn supports_web_search(&self) -> bool {
        true
    }

    /// Offered by default only on the official base, as the schema documents
    /// the tool, its events and its references; a custom gateway opts in.
    fn supports_default_web_search(
        &self,
        _model: &str,
        base: &url::Url,
        _credential: &crate::provider::ResolvedCredential,
        _strategy: &crate::provider::CredentialStrategy,
    ) -> bool {
        base.as_str() == BASE
    }

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

// --- request ---

/// A `GenRequest` this format cannot express without lying about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inexpressible {
    AudioNotSupported,
    /// The schema has `image_url` chunks, but no source shows inline image
    /// bytes accepted on this route.
    ImageNotSupported,
    /// A system turn carrying something other than text.
    SystemTurnIsTextOnly,
    /// Provider content minted by a different wire format.
    ProviderContentFromAnotherDialect {
        dialect: String,
    },
    /// Provider content that is not JSON, so it cannot be an entry.
    ProviderContentNotJson,
}

impl std::fmt::Display for Inexpressible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AudioNotSupported => {
                f.write_str("the Conversations wire contract does not support audio input")
            }
            Self::ImageNotSupported => f.write_str(
                "the Conversations mapping does not send images: no official source \
                 shows inline image bytes accepted on this route",
            ),
            Self::SystemTurnIsTextOnly => f.write_str(
                "a system turn carried something other than text, and `instructions` \
                 is a plain string on this API",
            ),
            Self::ProviderContentFromAnotherDialect { dialect } => write!(
                f,
                "provider content was minted by {dialect} and this mapping speaks \
                 {DIALECT}: an entry must be replayed as it was received, so content \
                 from another wire cannot be sent"
            ),
            Self::ProviderContentNotJson => {
                f.write_str("provider content is not JSON, so it cannot be replayed as an entry")
            }
        }
    }
}

impl From<Inexpressible> for RequestRefused {
    fn from(refused: Inexpressible) -> Self {
        RequestRefused::new(refused.to_string())
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    inputs: Vec<Entry<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    completion_args: CompletionArgs,
    stream: bool,
    /// Stated rather than defaulted — see this module's own doc.
    store: bool,
}

/// `CompletionArgs` is `additionalProperties: false`, so `max_tokens` lives
/// here and never at the top level. It has no `parallel_tool_calls`.
#[derive(Serialize)]
struct CompletionArgs {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    reasoning_effort: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireTool<'a> {
    Function { function: WireFunction<'a> },
    WebSearch,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

/// One input entry. `InputEntries` is an untagged union, so every entry
/// states its `object` and `type` instead of relying on their defaults.
#[derive(Serialize)]
#[serde(untagged)]
enum Entry<'a> {
    Message {
        object: &'static str,
        #[serde(rename = "type")]
        kind: &'static str,
        role: &'static str,
        content: Vec<TextChunk<'a>>,
    },
    Call {
        object: &'static str,
        #[serde(rename = "type")]
        kind: &'static str,
        tool_call_id: &'a str,
        name: &'a str,
        /// `FunctionCallEntryArguments` accepts a string; the model's own
        /// string goes back unchanged.
        arguments: &'a str,
    },
    /// `FunctionResultEntry` has no error flag, so a failed result is sent as
    /// its text, as on chat-completions.
    Result {
        object: &'static str,
        #[serde(rename = "type")]
        kind: &'static str,
        tool_call_id: &'a str,
        result: &'a str,
    },
    /// A `message.output` entry, replayed as this mapping recorded it.
    Replayed(&'a RawValue),
}

#[derive(Serialize)]
struct TextChunk<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

/// The API-wide `ReasoningEffort` enum. Which levels one model accepts is a
/// catalogue fact; this mapping does not clamp.
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

fn project<'a>(messages: &'a [Message]) -> Result<(Option<String>, Vec<Entry<'a>>), Inexpressible> {
    let mut system = String::new();
    let mut inputs = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == Role::System {
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !system.is_empty() {
                            system.push('\n');
                        }
                        system.push_str(text);
                    }
                    _ => return Err(Inexpressible::SystemTurnIsTextOnly),
                }
            }
            continue;
        }
        let role = match message.role {
            Role::Assistant => "assistant",
            _ => "user",
        };
        let mut pending: Vec<TextChunk<'a>> = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => pending.push(TextChunk { kind: "text", text }),
                ContentBlock::Image { .. } => return Err(Inexpressible::ImageNotSupported),
                ContentBlock::Audio { .. } => return Err(Inexpressible::AudioNotSupported),
                // This mapping mints no reasoning state: thinking travels
                // inside the replayed `message.output` entry.
                ContentBlock::Reasoning { .. } => {}
                ContentBlock::ProviderContent { state, .. } => {
                    flush(role, &mut pending, &mut inputs);
                    if state.dialect != DIALECT {
                        return Err(Inexpressible::ProviderContentFromAnotherDialect {
                            dialect: state.dialect.clone(),
                        });
                    }
                    inputs.push(Entry::Replayed(
                        serde_json::from_str::<&RawValue>(&state.blob)
                            .map_err(|_| Inexpressible::ProviderContentNotJson)?,
                    ));
                }
                ContentBlock::ToolUse {
                    call_id,
                    name,
                    args,
                } => {
                    flush(role, &mut pending, &mut inputs);
                    inputs.push(Entry::Call {
                        object: "entry",
                        kind: "function.call",
                        tool_call_id: call_id,
                        name,
                        arguments: args,
                    });
                }
                ContentBlock::ToolResult {
                    call_id, output, ..
                } => {
                    flush(role, &mut pending, &mut inputs);
                    inputs.push(Entry::Result {
                        object: "entry",
                        kind: "function.result",
                        tool_call_id: call_id,
                        result: output,
                    });
                }
            }
        }
        flush(role, &mut pending, &mut inputs);
    }
    Ok(((!system.is_empty()).then_some(system), inputs))
}

/// Close the text entry that was accumulating, if any.
fn flush<'a>(role: &'static str, pending: &mut Vec<TextChunk<'a>>, inputs: &mut Vec<Entry<'a>>) {
    if !pending.is_empty() {
        inputs.push(Entry::Message {
            object: "entry",
            kind: "message.input",
            role,
            content: std::mem::take(pending),
        });
    }
}

/// The exact JSON bytes this format would send for `req`.
///
/// `req.cache` is read nowhere: the route has no cache control to carry it.
/// `HostedSearch::max_requests` and `allow_live_access` have no field on
/// `WebSearchTool`, which accepts only `type` and `tool_configuration`.
pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    if super::request_has_audio(req) {
        return Err(Inexpressible::AudioNotSupported);
    }
    if super::request_has_images(req) {
        return Err(Inexpressible::ImageNotSupported);
    }
    let (instructions, inputs) = project(req.messages.messages())?;
    let body = WireRequest {
        model: &req.model,
        instructions,
        inputs,
        tools: req
            .tools
            .iter()
            .filter(|tool| req.web_search.is_none() || tool.name != "web_search")
            .map(|tool| WireTool::Function {
                function: WireFunction {
                    name: &tool.name,
                    description: &tool.description,
                    parameters: &tool.parameters,
                },
            })
            .chain(req.web_search.map(|_| WireTool::WebSearch))
            .collect(),
        completion_args: CompletionArgs {
            max_tokens: req.max_output_tokens,
            reasoning_effort: reasoning_effort(req.thinking),
        },
        stream: true,
        store: false,
    };
    Ok(serde_json::to_vec(&body).expect("a request this mapping built serialises"))
}

/// The complete HTTP request for `req` against `target`.
pub fn build_request(
    target: &RequestTarget<'_>,
    req: &GenRequest,
) -> Result<HttpRequest, Inexpressible> {
    Ok(target.post_json(PATH, request_body(req)?))
}

// --- response ---

/// One `ConversationEvents` data object. Keyed off `type`, which every event
/// schema carries; the `event:` line is redundant with it.
#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    content: Option<Box<RawValue>>,
    name: Option<String>,
    arguments: Option<String>,
    tool_call_id: Option<String>,
    usage: Option<WireUsage>,
    message: Option<String>,
    code: Option<i64>,
}

/// `ConversationUsageInfo`. `connector_tokens` and `connectors` are read
/// nowhere: their meaning is undocumented, so they prove neither input
/// tokens nor billed searches.
#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// One output content chunk: `TextChunk`, `ThinkChunk` or
/// `ToolReferenceChunk` by the fields read here.
#[derive(Deserialize)]
struct ChunkEnvelope {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<Vec<Box<RawValue>>>,
    signature: Option<String>,
    url: Option<String>,
    title: Option<String>,
}

enum Part {
    Text(String),
    Thinking {
        items: Vec<ThinkItem>,
        signature: Option<String>,
    },
    /// `tool_reference` and any chunk this mapping does not read, unchanged.
    Raw(Box<RawValue>),
}

enum ThinkItem {
    Text(String),
    Raw(Box<RawValue>),
}

struct OpenMessage {
    id: String,
    parts: Vec<Part>,
}

struct OpenCall {
    call_id: String,
    name: String,
    args: String,
}

struct OpenSearch {
    id: String,
    args: String,
}

#[derive(Serialize)]
struct OutputEntry<'a> {
    object: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    role: &'static str,
    content: Vec<PartOut<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum PartOut<'a> {
    Text {
        #[serde(rename = "type")]
        kind: &'static str,
        text: &'a str,
    },
    Thinking {
        #[serde(rename = "type")]
        kind: &'static str,
        thinking: Vec<ThinkOut<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<&'a str>,
    },
    Raw(&'a RawValue),
}

#[derive(Serialize)]
#[serde(untagged)]
enum ThinkOut<'a> {
    Text {
        #[serde(rename = "type")]
        kind: &'static str,
        text: &'a str,
    },
    Raw(&'a RawValue),
}

/// The `message.output` entry assembled from its deltas. Adjacent text is
/// joined; no `id` or timestamp is kept, because the schema requires only
/// `content` and a server id has no meaning under `store: false`.
fn entry_blob(parts: &[Part]) -> String {
    let content = parts
        .iter()
        .map(|part| match part {
            Part::Text(text) => PartOut::Text { kind: "text", text },
            Part::Thinking { items, signature } => PartOut::Thinking {
                kind: "thinking",
                thinking: items
                    .iter()
                    .map(|item| match item {
                        ThinkItem::Text(text) => ThinkOut::Text { kind: "text", text },
                        ThinkItem::Raw(raw) => ThinkOut::Raw(raw),
                    })
                    .collect(),
                signature: signature.as_deref(),
            },
            Part::Raw(raw) => PartOut::Raw(raw),
        })
        .collect();
    serde_json::to_string(&OutputEntry {
        object: "entry",
        kind: "message.output",
        role: "assistant",
        content,
    })
    .expect("an entry this mapping built serialises")
}

fn push_text(parts: &mut Vec<Part>, text: &str) {
    if let Some(Part::Text(last)) = parts.last_mut() {
        last.push_str(text);
    } else {
        parts.push(Part::Text(text.to_owned()));
    }
}

fn is_search(name: &str) -> bool {
    matches!(name, "web_search" | "web_search_premium")
}

/// A display label only. The search arguments are an undocumented string.
fn search_query(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return "web search".to_owned();
    };
    if let Some(query) = value["query"].as_str() {
        return query.to_owned();
    }
    let queries: Vec<&str> = value["queries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    if queries.is_empty() {
        "web search".to_owned()
    } else {
        queries.join("; ")
    }
}

/// The running state of one Conversations response.
#[derive(Default)]
pub struct StreamDecoder {
    sse: SseDecoder,
    message: Option<OpenMessage>,
    calls: Vec<OpenCall>,
    open_searches: Vec<OpenSearch>,
    searches: BTreeSet<String>,
    usage: Option<WireUsage>,
    done: bool,
    tool_use: bool,
    provider_error: Option<(String, ErrorKind)>,
}

impl super::StreamSink for StreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<AiEvent> {
        StreamDecoder::push(self, chunk)
    }
    fn finish(self: Box<Self>, transport: Option<HttpError>) -> Vec<AiEvent> {
        StreamDecoder::finish(*self, transport)
    }
}

impl StreamDecoder {
    /// Feed bytes; get the events that completed within them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AiEvent> {
        let mut out = Vec::new();
        for frame in self.sse.push(chunk) {
            self.consume(&frame, &mut out);
        }
        out
    }

    fn consume(&mut self, frame: &str, out: &mut Vec<AiEvent>) {
        // A frame this mapping cannot parse is not one it may guess at; the
        // absence of `conversation.response.done` is what ends the turn.
        let Ok(frame) = serde_json::from_str::<Envelope>(frame) else {
            return;
        };
        match frame.kind.as_str() {
            "message.output.delta" => {
                if let Some(content) = frame.content {
                    self.content_delta(frame.id.unwrap_or_default(), content, out);
                }
            }
            "tool.execution.started" | "tool.execution.delta" => {
                self.close_message(out);
                if let (Some(id), Some(name)) = (frame.id, frame.name)
                    && is_search(&name)
                {
                    let args = frame.arguments.unwrap_or_default();
                    match self.open_searches.iter_mut().find(|open| open.id == id) {
                        Some(open) => open.args.push_str(&args),
                        None => self.open_searches.push(OpenSearch { id, args }),
                    }
                }
            }
            "tool.execution.done" => {
                self.close_message(out);
                if let (Some(id), Some(name)) = (frame.id, frame.name)
                    && is_search(&name)
                {
                    self.observe_search(&id, out);
                }
            }
            "function.call.delta" => {
                self.close_message(out);
                if let (Some(call_id), Some(name)) = (frame.tool_call_id, frame.name) {
                    let args = frame.arguments.unwrap_or_default();
                    match self.calls.iter_mut().find(|call| call.call_id == call_id) {
                        Some(call) => call.args.push_str(&args),
                        None => self.calls.push(OpenCall {
                            call_id,
                            name,
                            args,
                        }),
                    }
                }
            }
            "conversation.response.done" => {
                self.close_message(out);
                for call in std::mem::take(&mut self.calls) {
                    self.tool_use = true;
                    out.push(AiEvent::ToolCall {
                        call_id: call.call_id,
                        name: call.name,
                        args: call.args,
                    });
                }
                self.observe_open_searches(out);
                self.usage = frame.usage;
                self.done = true;
            }
            "conversation.response.error" => {
                let message = frame.message.unwrap_or_else(|| {
                    "the provider reported conversation.response.error without a message".to_owned()
                });
                self.provider_error = Some((message, classify_error(frame.code)));
            }
            // `conversation.response.started`, `agent.handoff.*` and any type
            // added later carry nothing this mapping reads.
            _ => {}
        }
    }

    fn content_delta(&mut self, id: String, content: Box<RawValue>, out: &mut Vec<AiEvent>) {
        if self.message.as_ref().is_none_or(|open| open.id != id) {
            self.close_message(out);
            self.message = Some(OpenMessage {
                id,
                parts: Vec::new(),
            });
        }
        let Some(message) = self.message.as_mut() else {
            return;
        };
        if content.get().trim_start().starts_with('"') {
            if let Ok(text) = serde_json::from_str::<String>(content.get())
                && !text.is_empty()
            {
                push_text(&mut message.parts, &text);
                out.push(AiEvent::TextDelta { text });
            }
            return;
        }
        let Ok(chunk) = serde_json::from_str::<ChunkEnvelope>(content.get()) else {
            return;
        };
        match chunk.kind.as_str() {
            "text" => {
                if let Some(text) = chunk.text.filter(|text| !text.is_empty()) {
                    push_text(&mut message.parts, &text);
                    out.push(AiEvent::TextDelta { text });
                }
            }
            "thinking" => {
                if !matches!(message.parts.last(), Some(Part::Thinking { .. })) {
                    message.parts.push(Part::Thinking {
                        items: Vec::new(),
                        signature: None,
                    });
                }
                let Some(Part::Thinking { items, signature }) = message.parts.last_mut() else {
                    return;
                };
                for item in chunk.thinking.unwrap_or_default() {
                    match serde_json::from_str::<ChunkEnvelope>(item.get()) {
                        Ok(ChunkEnvelope {
                            kind,
                            text: Some(text),
                            ..
                        }) if kind == "text" => {
                            if !text.is_empty() {
                                out.push(AiEvent::ReasoningDelta { text: text.clone() });
                            }
                            if let Some(ThinkItem::Text(last)) = items.last_mut() {
                                last.push_str(&text);
                            } else {
                                items.push(ThinkItem::Text(text));
                            }
                        }
                        _ => items.push(ThinkItem::Raw(item)),
                    }
                }
                if chunk.signature.is_some() {
                    *signature = chunk.signature;
                }
            }
            "tool_reference" => {
                if let Some(url) = chunk.url {
                    let title = chunk
                        .title
                        .filter(|title| !title.is_empty())
                        .unwrap_or_else(|| url.clone());
                    out.push(AiEvent::Citation { url, title });
                }
                message.parts.push(Part::Raw(content));
            }
            _ => message.parts.push(Part::Raw(content)),
        }
    }

    /// One completed `message.output` entry becomes one provider-content
    /// item. It is visible when it showed text, so it replaces that text in
    /// history; a thinking-only entry rides the reasoning item instead.
    fn close_message(&mut self, out: &mut Vec<AiEvent>) {
        let Some(message) = self.message.take() else {
            return;
        };
        if message.parts.is_empty() {
            return;
        }
        let visible = message
            .parts
            .iter()
            .any(|part| matches!(part, Part::Text(text) if !text.is_empty()));
        out.push(AiEvent::ProviderContent {
            state: ReasoningState {
                connection: None,
                dialect: DIALECT.to_owned(),
                blob: entry_blob(&message.parts),
            },
            visible,
        });
    }

    /// A search observed once, by its entry id, whatever events repeat it.
    fn observe_search(&mut self, id: &str, out: &mut Vec<AiEvent>) {
        let args = match self.open_searches.iter().position(|open| open.id == id) {
            Some(index) => self.open_searches.remove(index).args,
            None => String::new(),
        };
        if self.searches.insert(id.to_owned()) {
            out.push(AiEvent::WebSearch {
                call_id: id.to_owned(),
                query: search_query(&args),
            });
        }
    }

    /// A started search is provider activity even when its `done` is missing.
    fn observe_open_searches(&mut self, out: &mut Vec<AiEvent>) {
        let ids: Vec<String> = self
            .open_searches
            .iter()
            .map(|open| open.id.clone())
            .collect();
        for id in ids {
            self.observe_search(&id, out);
        }
    }

    /// End the stream, and emit the one terminal event.
    ///
    /// The protocol has no finish reason. A response that delivered a function
    /// call ends as `ToolUse`; any other completed response ends as `Stop`. A
    /// `max_tokens` cut cannot be told apart from a natural stop on this wire.
    pub fn finish(mut self, transport: Option<HttpError>) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if let Some(error) = &transport {
            out.push(AiEvent::DeliveryObserved(error.delivery()));
        }
        if !self.done {
            self.observe_open_searches(&mut out);
        }
        if let Some(usage) = self.usage.take() {
            out.push(AiEvent::Usage(into_usage(
                &usage,
                self.searches.len() as u64,
            )));
        }
        if let Some((message, kind)) = self.provider_error.take() {
            out.push(AiEvent::Error { message, kind });
            out.push(AiEvent::Stop(StopReason::Error));
            return out;
        }
        if self.done {
            if let Some(error) = &transport {
                tracing::warn!(
                    %error,
                    "the response stream failed after conversation.response.done; \
                     the turn is complete"
                );
            }
            out.push(AiEvent::Stop(if self.tool_use {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            }));
            return out;
        }
        let message = match &transport {
            Some(error) => error.to_string(),
            None => "the conversation never reported conversation.response.done: the \
                     stream ended before the provider said the response was complete"
                .to_owned(),
        };
        out.push(AiEvent::Error {
            message,
            kind: transport
                .as_ref()
                .map_or(ErrorKind::Transient, HttpError::kind),
        });
        out.push(AiEvent::Stop(StopReason::Error));
        out
    }
}

/// Only the two codes the official error glossary names are classified.
fn classify_error(code: Option<i64>) -> ErrorKind {
    match code {
        // Unsupported reasoning effort for the model.
        Some(3051) => ErrorKind::Invalid,
        // Prompt longer than the model context.
        Some(3059) => ErrorKind::ContextOverflow,
        _ => ErrorKind::Unclassified,
    }
}

/// `prompt_tokens` is all input; the schema states no cached or reasoning
/// split, so none is invented. Searches are the executions observed in this
/// response, not `usage.connectors`, whose meaning is undocumented.
fn into_usage(usage: &WireUsage, searches: u64) -> Usage {
    Usage {
        web_search_requests: searches,
        uncached_input_tokens: usage.prompt_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_extended_tokens: 0,
        output_tokens: usage.completion_tokens,
        reasoning_tokens: None,
    }
}

/// Decode one Conversations response into the generation's events.
pub fn decode(
    response: Result<HttpResponse, HttpError>,
    cost: Option<Arc<dyn ProviderCost>>,
) -> BoxStream<'static, AiEvent> {
    debug_assert!(
        cost.is_none(),
        "ConversationUsageInfo states no monetary figure, so a row on this mapping \
         carries no ProviderCost"
    );
    super::decode_response(response, Box::new(StreamDecoder::default()))
}
