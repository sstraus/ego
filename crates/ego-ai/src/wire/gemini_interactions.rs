//! Gemini's Interactions API: a conversation is a list of steps.
//!
//! `docs/14-six-wire-protocols.md` §3 chose this over the legacy
//! `models:generateContent`, and §3.8 resolved the shape ego actually needs.
//! The three facts that make this a mapping of its own rather than a variant
//! of another:
//!
//! - **A conversation is steps, not roles.** A user turn is a `user_input`
//!   step and a prior answer a `model_output` step; a tool call and its result
//!   are steps beside them rather than blocks inside a turn. There is no
//!   `role` field anywhere in the request.
//! - **There is no finish reason in this protocol at all.** The terminal
//!   signal is the interaction's `status`, which is why [`ending`] reads a
//!   status and the family's `consume_finish_reason` has no counterpart here.
//! - **`thinking_level` has no `off`.** The floor is `minimal`, and omitting
//!   the field selects a per-model default that thinks — so `Off` is refused
//!   rather than translated into a request that bills for reasoning the caller
//!   switched off.
//!
//! # Stateless, and it has to say so
//!
//! `store` defaults to **true** on this API and multi-turn is documented as
//! `previous_interaction_id`. ego is stateless by construction: it rebuilds
//! the whole history from its own log every turn. Worse, the stateful mode is
//! *inexpressible* at this seam — [`WireFormat::build_request`] is a pure
//! function of the request, and no `AiEvent` could carry an interaction id
//! back out of [`decode`]. So every request states `store: false` and names no
//! previous interaction, and that is a decision recorded rather than a default
//! relied upon.
//!
//! # Usage is four summands, not a partition
//!
//! `total_thought_tokens` is a **separate summand** beside
//! `total_output_tokens`, not a breakdown of it, and `total_tool_use_tokens`
//! belongs to the input side. §3.3 checks the arithmetic against Google's own
//! example. A decoder that treated thought tokens as a subset would
//! under-report every reasoning turn.
//!
//! `step.stop` may restate the running total beside its per-step figure, and
//! restating is not spending: usage is reported once, from the provider's last
//! word.

use std::collections::BTreeMap;
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

/// The `gemini-interactions` mapping, as a value a profile can name.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GeminiInteractions;

impl WireFormat for GeminiInteractions {
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

/// The path a provider row appends to its base URL to reach this format.
///
/// The query is part of the path because it is part of the *framing*:
/// `?alt=sse` is how this API is asked for an event stream, exactly as
/// `accept: text/event-stream` is on the others, and both are facts about what
/// is wanted back rather than about who is serving it.
pub const PATH: &str = "interactions?alt=sse";

/// Which dialect this mapping's [`ReasoningState`] blobs are written in.
pub const DIALECT: &str = "gemini-interactions";

/// The four levels `thinking_level` accepts, in the order a refusal lists them.
const LEVELS: [&str; 4] = ["minimal", "low", "medium", "high"];

// --- request ---

/// A `GenRequest` this format cannot express without lying about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inexpressible {
    /// `thinking_level` has no spelling for this level.
    ThinkingLevelUnavailable { level: ThinkingLevel },
    /// Reasoning state minted by a different wire format.
    ReasoningStateFromAnotherDialect { dialect: String },
    /// Reasoning state that is not JSON, so it cannot be a step.
    ReasoningStateNotJson,
    /// Tool arguments that are not JSON. This API wants an object, and a
    /// string that only looks like one cannot become it.
    ArgumentsNotJson { call_id: String },
    /// A system turn carrying something that is not text.
    SystemTurnIsTextOnly,
}

impl std::fmt::Display for Inexpressible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Inexpressible::ThinkingLevelUnavailable { level } => write!(
                f,
                "the Interactions API has no spelling for thinking level {level:?}: \
                 thinking_level accepts {} and there is no way to switch thinking \
                 off — omitting it selects a model default that thinks",
                LEVELS.join(", ")
            ),
            Inexpressible::ReasoningStateFromAnotherDialect { dialect } => write!(
                f,
                "reasoning state was minted by {dialect} and this mapping speaks \
                 {DIALECT}: a thought step must be replayed exactly as it was \
                 received, so state from another wire cannot be sent"
            ),
            Inexpressible::ReasoningStateNotJson => write!(
                f,
                "reasoning state is not JSON, so it cannot be replayed as a step"
            ),
            Inexpressible::ArgumentsNotJson { call_id } => write!(
                f,
                "the arguments of call {call_id} are not JSON: this API takes an \
                 object where chat-completions takes a string"
            ),
            Inexpressible::SystemTurnIsTextOnly => write!(
                f,
                "a system turn carried something other than text, and \
                 system_instruction is a plain string on this API"
            ),
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
    input: Vec<Step<'a>>,
    stream: bool,
    /// Stated rather than defaulted — see this module's own doc.
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    thinking_level: &'static str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

/// One step of the conversation, in the request's own vocabulary.
#[derive(Serialize)]
#[serde(untagged)]
enum Step<'a> {
    Turn {
        #[serde(rename = "type")]
        kind: &'static str,
        content: Vec<TextContent<'a>>,
    },
    Call {
        #[serde(rename = "type")]
        kind: &'static str,
        id: &'a str,
        name: &'a str,
        arguments: &'a RawValue,
    },
    Result {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        name: &'a str,
        result: Vec<TextContent<'a>>,
        is_error: bool,
    },
    /// A `thought` step, replayed byte for byte as the provider minted it.
    Replayed(&'a RawValue),
}

#[derive(Serialize)]
struct TextContent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

fn text_content(text: &str) -> TextContent<'_> {
    TextContent { kind: "text", text }
}

/// The spelling this API gives a level, or a refusal naming the four it has.
fn thinking_level(level: ThinkingLevel) -> Result<&'static str, Inexpressible> {
    match level {
        ThinkingLevel::Minimal => Ok("minimal"),
        ThinkingLevel::Low => Ok("low"),
        ThinkingLevel::Medium => Ok("medium"),
        ThinkingLevel::High => Ok("high"),
        // `Off` has no spelling, and `XHigh`/`Max` are above this API's
        // ceiling. Clamping either down to `high` would bill for less thinking
        // than the caller asked for and say nothing about having done so.
        ThinkingLevel::Off | ThinkingLevel::XHigh | ThinkingLevel::Max => {
            Err(Inexpressible::ThinkingLevelUnavailable { level })
        }
    }
}

/// One walk producing the instruction and the steps, because the split *is*
/// the projection: a system turn is not a step here.
fn project<'a>(messages: &'a [Message]) -> Result<(Option<String>, Vec<Step<'a>>), Inexpressible> {
    let mut system = String::new();
    let mut steps = Vec::with_capacity(messages.len());

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

        // Text accumulates into one turn step; anything else is a step of its
        // own and must flush the text before it so the order the caller built
        // survives. `ContentBlock`'s own doc makes that ordering the caller's
        // contract.
        let turn = match message.role {
            Role::Assistant => "model_output",
            _ => "user_input",
        };
        let mut pending: Vec<TextContent<'_>> = Vec::new();

        for block in &message.content {
            match block {
                ContentBlock::Text { text } => pending.push(text_content(text)),
                ContentBlock::Reasoning { state } => {
                    flush(turn, &mut pending, &mut steps);
                    if state.dialect != DIALECT {
                        return Err(Inexpressible::ReasoningStateFromAnotherDialect {
                            dialect: state.dialect.clone(),
                        });
                    }
                    steps.push(Step::Replayed(
                        serde_json::from_str::<&RawValue>(&state.blob)
                            .map_err(|_| Inexpressible::ReasoningStateNotJson)?,
                    ));
                }
                ContentBlock::ToolUse {
                    call_id,
                    name,
                    args,
                } => {
                    flush(turn, &mut pending, &mut steps);
                    steps.push(Step::Call {
                        kind: "function_call",
                        id: call_id,
                        name,
                        arguments: serde_json::from_str::<&RawValue>(args).map_err(|_| {
                            Inexpressible::ArgumentsNotJson {
                                call_id: call_id.clone(),
                            }
                        })?,
                    });
                }
                ContentBlock::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    flush(turn, &mut pending, &mut steps);
                    steps.push(Step::Result {
                        kind: "function_result",
                        call_id,
                        // The name is optional on the wire and ego does not
                        // carry it on a result, so the call it answers is the
                        // only honest source. Absent when the call is not in
                        // this history, which `Conversation` already forbids.
                        name: name_of_call(messages, call_id),
                        result: vec![text_content(output)],
                        is_error: *is_error,
                    });
                }
            }
        }
        flush(turn, &mut pending, &mut steps);
    }

    Ok(((!system.is_empty()).then_some(system), steps))
}

/// Close the text turn that was accumulating, if any.
///
/// A free function and not a closure: it borrows the same lifetime the steps
/// carry, and a closure capturing `turn` cannot name that lifetime.
fn flush<'a>(kind: &'static str, pending: &mut Vec<TextContent<'a>>, steps: &mut Vec<Step<'a>>) {
    if !pending.is_empty() {
        steps.push(Step::Turn {
            kind,
            content: std::mem::take(pending),
        });
    }
}

/// The name of the call `call_id` identifies, which `Conversation`'s own
/// invariant guarantees appears earlier in this history.
fn name_of_call<'a>(messages: &'a [Message], call_id: &str) -> &'a str {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolUse {
                call_id: id, name, ..
            } if id == call_id => Some(name.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The exact JSON bytes this format would send for `req`.
pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    let (system_instruction, input) = project(req.messages.messages())?;
    let body = WireRequest {
        model: &req.model,
        input,
        stream: true,
        store: false,
        system_instruction,
        tools: req
            .tools
            .iter()
            .map(|tool| WireTool {
                kind: "function",
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.parameters,
            })
            .collect(),
        generation_config: GenerationConfig {
            max_output_tokens: req.max_output_tokens,
            thinking_level: thinking_level(req.thinking)?,
        },
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

/// One SSE frame, discriminated by the `event_type` the data repeats.
///
/// Keyed off `event_type` and not the `event:` line: the doc's own examples
/// carry both and the body is the one a non-SSE transport would still have.
#[derive(Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
enum Frame {
    #[serde(rename = "step.start")]
    StepStart {
        index: u32,
        step: StartStep,
    },
    #[serde(rename = "step.delta")]
    StepDelta {
        index: u32,
        delta: Delta,
    },
    #[serde(rename = "step.stop")]
    StepStop {
        index: u32,
    },
    #[serde(rename = "interaction.completed")]
    Completed {
        interaction: Interaction,
    },
    Error {
        error: WireError,
    },
    /// Everything this mapping does not read: `interaction.created`,
    /// `interaction.status_update`, and any event type added later. The
    /// vocabulary is open — the generated types carry an `UNKNOWN` arm — so an
    /// unrecognised *event* is ignored while an unrecognised *status* is not.
    /// The difference is that a status is terminal and an event is not.
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct StartStep {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    Text {
        text: String,
    },
    ThoughtSummary {
        content: SummaryContent,
    },
    ThoughtSignature {
        signature: String,
    },
    /// The wire calls this `arguments_delta`; the variant drops the suffix
    /// because every arm of this enum is a delta and repeating it in one name
    /// says nothing the enum has not said already.
    #[serde(rename = "arguments_delta")]
    Arguments {
        arguments: String,
    },
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct SummaryContent {
    text: String,
}

#[derive(Deserialize)]
struct Interaction {
    status: String,
    usage: Option<WireUsage>,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
    total_cached_tokens: u64,
    #[serde(default)]
    total_output_tokens: u64,
    #[serde(default)]
    total_thought_tokens: u64,
    #[serde(default)]
    total_tool_use_tokens: u64,
}

#[derive(Deserialize)]
struct WireError {
    message: String,
    #[serde(default)]
    code: String,
}

/// The `thought` step a [`ReasoningState`] blob replays as.
///
/// A struct and not `serde_json::json!`, because that macro builds a map that
/// orders its keys alphabetically and would mint `{"signature":…,"type":…}`.
/// The blob is bytes ego stores and sends back unread, so the order it is
/// written in is the order the provider will be handed — and a `type`
/// discriminator that follows the field it discriminates is a needless way to
/// differ from every example Google publishes.
#[derive(Serialize)]
struct ThoughtBlob<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    signature: &'a str,
}

/// What a step was, and what it accumulated before it stopped.
enum StepState {
    Thought {
        signature: Option<String>,
    },
    Call {
        id: String,
        name: String,
        args: String,
    },
    Other,
}

/// How the interaction ended, in this protocol's own vocabulary.
enum Ending {
    Mapped(StopReason),
    /// A status that is terminal and has no `StopReason`, or one this mapping
    /// has never seen. Both fail closed and name themselves.
    Unmapped(String),
}

/// The running state of one Interactions response.
#[derive(Default)]
pub struct StreamDecoder {
    sse: SseDecoder,
    steps: BTreeMap<u32, StepState>,
    usage: Option<WireUsage>,
    ending: Option<Ending>,
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
        let Ok(frame) = serde_json::from_str::<Frame>(frame) else {
            // A frame this mapping cannot parse is not a frame it may guess
            // at. Ignoring it keeps the stream's other events intact, and the
            // absence of a terminal status is what will end the turn.
            return;
        };
        match frame {
            Frame::StepStart { index, step } => {
                let state = match step.kind.as_str() {
                    "thought" => StepState::Thought { signature: None },
                    "function_call" => StepState::Call {
                        // The id and name arrive whole here; only the
                        // arguments stream.
                        id: step.id.unwrap_or_default(),
                        name: step.name.unwrap_or_default(),
                        args: String::new(),
                    },
                    _ => StepState::Other,
                };
                self.steps.insert(index, state);
            }
            Frame::StepDelta { index, delta } => match delta {
                Delta::Text { text } => out.push(AiEvent::TextDelta { text }),
                Delta::ThoughtSummary { content } => {
                    out.push(AiEvent::ReasoningDelta { text: content.text })
                }
                Delta::ThoughtSignature { signature } => {
                    if let Some(StepState::Thought { signature: held }) = self.steps.get_mut(&index)
                    {
                        *held = Some(signature);
                    }
                }
                Delta::Arguments { arguments } => {
                    if let Some(StepState::Call { args, .. }) = self.steps.get_mut(&index) {
                        args.push_str(&arguments);
                    }
                }
                Delta::Ignored => {}
            },
            Frame::StepStop { index } => match self.steps.remove(&index) {
                // The signature is the last delta before the stop, so the
                // step's own boundary is the state's boundary — no heuristic
                // decides when a thought is complete.
                Some(StepState::Thought {
                    signature: Some(signature),
                }) => out.push(AiEvent::ReasoningState(ReasoningState {
                    dialect: DIALECT.to_string(),
                    blob: serde_json::to_string(&ThoughtBlob {
                        kind: "thought",
                        signature: &signature,
                    })
                    .expect("two strings serialise"),
                })),
                Some(StepState::Call { id, name, args }) => out.push(AiEvent::ToolCall {
                    call_id: id,
                    name,
                    args,
                }),
                // A thought that closed with no signature minted nothing
                // replayable, and inventing one would be worse than replaying
                // none.
                Some(StepState::Thought { signature: None }) | Some(StepState::Other) | None => {}
            },
            Frame::Completed { interaction } => {
                // The last word wins: `step.stop` may restate the running
                // total, and this is the figure the provider settled on.
                if let Some(usage) = interaction.usage {
                    self.usage = Some(usage);
                }
                self.ending = Some(ending(&interaction.status));
            }
            Frame::Error { error } => {
                self.provider_error = Some((error.message, classify_error(&error.code)));
            }
            Frame::Ignored => {}
        }
    }

    /// End the stream, and emit the one terminal event.
    pub fn finish(mut self, transport: Option<HttpError>) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if let Some(error) = &transport {
            out.push(AiEvent::DeliveryObserved(error.delivery()));
        }
        if let Some(usage) = self.usage.take() {
            out.push(AiEvent::Usage(into_usage(&usage)));
        }
        if let Some((message, kind)) = self.provider_error.take() {
            out.push(AiEvent::Error { message, kind });
            out.push(AiEvent::Stop(StopReason::Error));
            return out;
        }
        match self.ending.take() {
            Some(Ending::Mapped(stop)) => {
                if let Some(error) = &transport {
                    tracing::warn!(
                        %error,
                        "the response stream failed after the interaction had \
                         already reported its status; the turn is complete"
                    );
                }
                out.push(AiEvent::Stop(stop));
            }
            Some(Ending::Unmapped(status)) => {
                out.push(AiEvent::Error {
                    message: unmapped_status(&status),
                    kind: ErrorKind::Unclassified,
                });
                out.push(AiEvent::Stop(StopReason::Error));
            }
            // There is no `[DONE]` sentinel and no finish reason on this wire,
            // so a stream that simply stopped was cut.
            None => {
                let message = match &transport {
                    Some(error) => error.to_string(),
                    None => "the interaction never reported a status: the stream \
                             ended before interaction.completed, so the model \
                             never said whether it had finished"
                        .to_string(),
                };
                out.push(AiEvent::Error {
                    message,
                    kind: ErrorKind::Transient,
                });
                out.push(AiEvent::Stop(StopReason::Error));
            }
        }
        out
    }
}

/// What the provider's own sentence should say about a status ego cannot map.
fn unmapped_status(status: &str) -> String {
    if status == "budget_exceeded" {
        return "the provider halted the interaction: its own token budget was \
                exceeded (status \"budget_exceeded\"), which is the provider's \
                budget and not ego's"
            .to_string();
    }
    format!(
        "the interaction reported status {status:?}, which this mapping has no \
         reading for: the status vocabulary is open and a status ego cannot \
         name is not a status it may assume is success"
    )
}

/// The interaction's status, which is this protocol's only terminal signal.
fn ending(status: &str) -> Ending {
    match status {
        "completed" => Ending::Mapped(StopReason::Stop),
        "requires_action" => Ending::Mapped(StopReason::ToolUse),
        // "e.g. hitting max_tokens" is as precise as the reference gets, and
        // `Length` is the only reading it supports.
        "incomplete" => Ending::Mapped(StopReason::Length),
        "cancelled" => Ending::Mapped(StopReason::Aborted),
        "failed" => Ending::Mapped(StopReason::Error),
        // `budget_exceeded` among them: it is terminal and it is *not* a
        // generic error, so it is named rather than collapsed.
        other => Ending::Unmapped(other.to_string()),
    }
}

/// Whether a retry could plausibly do better, from the slug the provider sent.
fn classify_error(code: &str) -> ErrorKind {
    match code {
        "gateway_timeout" | "deadline_exceeded" | "unavailable" | "resource_exhausted"
        | "internal" => ErrorKind::Transient,
        "invalid_argument" | "not_found" | "failed_precondition" | "out_of_range" => {
            ErrorKind::Invalid
        }
        "unauthenticated" | "permission_denied" => ErrorKind::Refused,
        _ => ErrorKind::Unclassified,
    }
}

/// Four summands folded onto ego's two sides.
///
/// `total_thought_tokens` joins the output because it *is* output the caller
/// paid for, and `total_tool_use_tokens` joins the input because it is prompt
/// the provider assembled. Neither is subtracted from anything: this API
/// reports summands, and the only figure it partitions is `total_tokens`.
fn into_usage(usage: &WireUsage) -> Usage {
    Usage {
        uncached_input_tokens: usage
            .total_input_tokens
            .saturating_sub(usage.total_cached_tokens)
            .saturating_add(usage.total_tool_use_tokens),
        cache_read_tokens: usage.total_cached_tokens,
        // This API states no cache-write figure at all, at either retention.
        cache_write_tokens: 0,
        cache_write_extended_tokens: 0,
        reasoning_tokens: Some(usage.total_thought_tokens),
        output_tokens: usage
            .total_output_tokens
            .saturating_add(usage.total_thought_tokens),
    }
}

/// Decode one Interactions response into the generation's events.
pub fn decode(
    response: Result<HttpResponse, HttpError>,
    cost: Option<Arc<dyn ProviderCost>>,
) -> BoxStream<'static, AiEvent> {
    debug_assert!(
        cost.is_none(),
        "the Interactions API states no monetary figure, ever, so a row on this \
         mapping carries no ProviderCost"
    );
    super::decode_response(response, Box::new(StreamDecoder::default()))
}
