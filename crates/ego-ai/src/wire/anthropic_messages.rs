//! The `anthropic-messages` wire format: `POST /messages`, streamed.
//!
//! Anthropic's own shape, and no one else's.
//! `ARCHITECTURE.md` §4
//! allows a provider to differ from a family in exactly three places — a model
//! fact, a transport fact, or another mapping — and this differs in the
//! request's *shape* and the stream's *event model*, which is neither of the
//! first two. So it is the third, and this file is it.
//!
//! Every field name and enum spelling below was read from the published
//! Messages API reference on 2026-08-22, not recalled. What that is worth is
//! `[D]+[S]` in `ARCHITECTURE.md` §0's evidence classes: no
//! `ANTHROPIC_API_KEY` exists in this repository, so nothing here has been
//! observed against a live server, and the row this mapping serves is a
//! **candidate** — built, tested offline, and absent from the table
//! `crates/ego-cli/src/model.rs` assembles.
//!
//! # The three structural differences, and why none of them is a flag
//!
//! - **The system prompt is a top-level parameter.** There is no `role:
//!   "system"` message, so the family's `Role::System → "system"` projection
//!   has nowhere to land.
//! - **A tool result is a content block inside a `user` message**, not a
//!   message with a role of its own — which the family's own projection
//!   already anticipated in prose it could not act on.
//! - **`max_tokens` is required** and [`GenRequest::max_output_tokens`] is an
//!   `Option`, so a request without a ceiling is refused rather than given one
//!   this file invented.
//!
//! # Thinking is an effort level, and `budget_tokens` is not sent
//!
//! `thinking: {type: "enabled", budget_tokens: N}` is deprecated on Claude 4.6
//! and returns a `400` on 4.7 and later. Under `AGENTS.md`'s current-models-only
//! rule it is not a spelling this build carries. Its replacement is
//! `thinking: {type: "adaptive"}` beside `output_config: {effort: …}`, whose
//! vocabulary is `low | medium | high | xhigh | max` — five of
//! [`ThinkingLevel`]'s seven variants, spelled identically. So there is nothing
//! to approximate and nothing to document as approximate. `Off` is the
//! *different field* `thinking: {type: "disabled"}`, and `Minimal` is not
//! published at all and is refused by name.
//!
//! # What this mapping does not reorder
//!
//! Anthropic requires a turn's `tool_result` blocks to be grouped ahead of its
//! text, and its thinking to precede its `tool_use`. [`ContentBlock`] is an
//! ordered list precisely so that a caller can express that, and this
//! projection **preserves the order it is given** rather than sorting it into
//! compliance. Reordering here would be the mapping deciding what the caller
//! meant, and would hide the one case where it mattered.
//! `ego_core::generator::history` builds histories in that order and merges
//! consecutive same-role turns as it goes, so the alternation this API expects
//! holds by construction rather than by repair here.
//!
//! # The one event ordering rule
//!
//! **`Stop` is last, always, exactly once** — the same rule the family keeps,
//! for a different reason. There is no `[DONE]` sentinel here: `message_stop`
//! is a real event, and `message_delta` carries both the stop reason and a
//! *cumulative* output count. So the reason is remembered when it arrives and
//! emitted when the bytes run out, and the usage figures are the provider's
//! last word rather than the sum of its restatements.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
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

/// The `anthropic-messages` mapping, as a value a profile can name.
///
/// A unit struct for the reason every mapping is one: what varies between
/// deployments — the address, the credential — is on the profile and arrives
/// as a parameter, so there is nothing for an instance to hold and nowhere to
/// keep a per-provider branch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicMessages;

impl WireFormat for AnthropicMessages {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        build_request(target, req).map_err(RequestRefused::from)
    }

    fn cache_prefix(&self, body: &[u8]) -> Option<String> {
        cache_prefix(body)
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
pub const PATH: &str = "messages";

/// The header carrying the contract version this mapping was written against.
///
/// The *mapping's*, like `content-type`, and not the row's: a version is a
/// fact about which contract these bytes conform to, so a row that carried it
/// would be a row that could disagree with its own mapping about what it
/// speaks.
pub const VERSION_HEADER: &str = "anthropic-version";

/// The contract version. Anthropic's versioning is by date and this is the one
/// every current model serves.
pub const VERSION: &str = "2023-06-01";

/// Which dialect this mapping's [`ReasoningState`] blobs are written in.
///
/// Compared for equality against a target model's own tag and never matched
/// on. It exists because
/// [`ReasoningCarryover::SameProviderOnly`](ego_core::generator::capabilities::ReasoningCarryover::SameProviderOnly)
/// is a property of the *target* and says nothing about who minted the state.
pub const DIALECT: &str = "anthropic-messages";

// --- request ---

/// A `GenRequest` this format cannot express without lying about it.
///
/// Every variant is a case where the alternative would be to drop something
/// the caller asked for, or invent something they did not, and send the
/// request anyway. `ARCHITECTURE.md` I5 is about exactly that: a
/// silent mistranslation is worse than a refusal because nothing downstream
/// can tell it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inexpressible {
    AudioNotSupported,
    /// `max_tokens` is required and the request set no ceiling.
    ///
    /// A number invented here would be a silent choice about what the caller
    /// is billed, and — worse — about where their answer gets cut off.
    CeilingRequired,
    /// A [`ThinkingLevel`] Anthropic does not publish.
    ///
    /// Only `Minimal` reaches this. Substituting the nearest published level
    /// would bill for reasoning at a depth nobody asked for, silently.
    ThinkingLevelUnavailable {
        level: ThinkingLevel,
    },
    /// Reasoning state another protocol minted.
    ///
    /// It cannot be posted into a `content` array that has no shape for it,
    /// and dropping it produces the documented `400` on the next turn rather
    /// than on this one — which is the worst of both.
    ReasoningStateFromAnotherDialect {
        dialect: String,
    },
    /// Reasoning state this mapping minted and cannot read back.
    ///
    /// Unreachable from a stream decoded here and reachable from a durable log
    /// that was truncated, which is a real way for a resumed session to arrive.
    ReasoningStateNotJson,
    /// A tool call whose arguments are not JSON.
    ///
    /// [`ContentBlock::ToolUse::args`] is a `String` so that a call truncated
    /// mid-stream survives as *evidence about the stream*; `tool_use.input` is
    /// an object, so this is the seam at which that evidence stops being
    /// sendable.
    ArgumentsNotJson {
        call_id: String,
    },
    /// A system turn carrying something other than text.
    ///
    /// [`Conversation`](ego_core::generator::types::Conversation) permits a
    /// tool result in a system turn — only the assistant is forbidden from
    /// answering itself — and a top-level `system` parameter has nowhere to
    /// put one.
    SystemTurnIsTextOnly,
}

impl std::fmt::Display for Inexpressible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AudioNotSupported => {
                f.write_str("this wire contract does not support audio input")
            }
            Inexpressible::CeilingRequired => write!(
                f,
                "anthropic-messages requires max_tokens on every request, and \
                 this request set no output ceiling; a default invented here \
                 would be a silent choice about your bill and about where your \
                 answer is cut off"
            ),
            Inexpressible::ThinkingLevelUnavailable { level } => write!(
                f,
                "anthropic-messages does not publish the thinking level {level:?} \
                 in lowercase; it offers off, low, medium, high, xhigh and max"
            ),
            Inexpressible::ReasoningStateFromAnotherDialect { dialect } => write!(
                f,
                "this history carries reasoning state minted by {dialect}, and \
                 anthropic-messages will only accept blocks it minted itself"
            ),
            Inexpressible::ReasoningStateNotJson => write!(
                f,
                "this history carries a reasoning block that is not valid JSON, \
                 so it cannot be replayed as the provider requires"
            ),
            Inexpressible::ArgumentsNotJson { call_id } => write!(
                f,
                "the arguments of tool call {call_id} are not valid JSON, and \
                 anthropic-messages carries them as an object rather than as a \
                 string; the call was most likely cut short mid-stream"
            ),
            Inexpressible::SystemTurnIsTextOnly => write!(
                f,
                "anthropic-messages carries the system prompt as a top-level \
                 parameter, which holds text and nothing else"
            ),
        }
    }
}

impl std::error::Error for Inexpressible {}

/// The refusal, rendered for the seam. The sentence already names this format,
/// which is why no discriminant survives the conversion.
impl From<Inexpressible> for RequestRefused {
    fn from(refused: Inexpressible) -> Self {
        RequestRefused::new(refused.to_string())
    }
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    /// Required by the API, which is why it is a `u64` here and an `Option` on
    /// [`GenRequest`]: the refusal happens once, before this struct exists.
    max_tokens: u64,
    stream: bool,
    /// The whole system prompt, or nothing. A top-level parameter and not a
    /// message — the first of the three reasons this is not the family.
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    /// Sent only to *decline* parallel calls, because allowing them is the
    /// vendor's default. A `tool_choice` on a request offering no tools would
    /// be a parameter about nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    thinking: Thinking,
    /// Omitted exactly when thinking is disabled. An explicit effort above
    /// `high` turns a `disabled` thinking block into a `400`, and the default
    /// is `high` — so saying nothing is the one spelling in range whatever
    /// model the caller named.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
}

#[derive(Debug, Serialize)]
struct ToolChoice {
    #[serde(rename = "type")]
    kind: &'static str,
    disable_parallel_tool_use: bool,
}

#[derive(Debug, Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
    /// Always sent when thinking is on, and the reason is the same one that
    /// makes the family send `reasoning_effort: "none"`: the default is
    /// `omitted`, which streams thinking blocks whose text is empty. Relying
    /// on it would make [`AiEvent::ReasoningDelta`] a producer of nothing and
    /// present the caller with a long silence instead of visible progress.
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct OutputConfig {
    effort: &'static str,
}

#[derive(Debug, Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    content: Vec<WireBlock<'a>>,
}

/// One content block, as this API spells it.
///
/// Untagged with each variant carrying its own `type`, rather than an
/// internally tagged enum, because one variant is not a shape at all:
/// [`WireBlock::Replayed`] is the provider's own bytes going back unaltered,
/// and a tag serde added would be a byte this mapping wrote into a block the
/// provider requires to be untouched.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WireBlock<'a> {
    Text {
        #[serde(rename = "type")]
        kind: &'static str,
        text: &'a str,
    },
    Image {
        #[serde(rename = "type")]
        kind: &'static str,
        source: ImageSource,
    },
    ToolUse {
        #[serde(rename = "type")]
        kind: &'static str,
        id: &'a str,
        name: &'a str,
        /// An object, not the JSON *string* chat-completions asks for — and
        /// borrowed rather than re-serialised, so the bytes the model produced
        /// are the bytes it gets back.
        input: &'a RawValue,
    },
    ToolResult {
        #[serde(rename = "type")]
        kind: &'static str,
        tool_use_id: &'a str,
        content: &'a str,
        /// Sent always, including `false`. A result that omits it reads as
        /// success, and this API — unlike chat-completions, which has nowhere
        /// to put the fact — can say otherwise.
        is_error: bool,
    },
    /// A block the provider minted, going back exactly as it arrived.
    Replayed(&'a RawValue),
}

#[derive(Debug, Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: &'static str,
    data: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WireTool<'a> {
    WebSearch {
        #[serde(rename = "type")]
        kind: &'static str,
        name: &'static str,
        max_uses: u32,
    },
    Function {
        name: &'a str,
        description: &'a str,
        /// `input_schema`, where chat-completions says `function.parameters`. The
        /// same JSON Schema under a different name, which is why the difference is
        /// a mapping and not a translation.
        input_schema: &'a serde_json::Value,
    },
}

/// `ThinkingLevel` to `output_config.effort`.
///
/// `Ok(None)` is thinking *off*, which is a different field rather than an
/// effort value: the vendor is explicit that `adaptive` is a thinking mode and
/// must not be passed as an effort. The five that do map are spelled
/// identically, so nothing is approximated.
fn effort(level: ThinkingLevel) -> Result<Option<&'static str>, Inexpressible> {
    match level {
        ThinkingLevel::Off => Ok(None),
        // The fourth time the three-state `ReasoningEffortSupport` design has
        // paid for itself, after xAI's per-model subset, Ollama's silent clamp
        // and Gemini's missing `off`.
        ThinkingLevel::Minimal => Err(Inexpressible::ThinkingLevelUnavailable { level }),
        ThinkingLevel::Low => Ok(Some("low")),
        ThinkingLevel::Medium => Ok(Some("medium")),
        ThinkingLevel::High => Ok(Some("high")),
        ThinkingLevel::XHigh => Ok(Some("xhigh")),
        ThinkingLevel::Max => Ok(Some("max")),
    }
}

/// The history, split into the system prompt and the messages.
///
/// One walk producing both, because the split *is* the projection: a system
/// turn is not a message here, so deciding which turns are messages and
/// building them are the same decision.
fn project(messages: &[Message]) -> Result<(Option<String>, Vec<WireMessage<'_>>), Inexpressible> {
    let mut system = String::new();
    let mut wire = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == Role::System {
            for block in &message.content {
                let ContentBlock::Text { text } = block else {
                    return Err(Inexpressible::SystemTurnIsTextOnly);
                };
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(text);
            }
            continue;
        }
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            // Handled above; matching exhaustively rather than with a
            // catch-all so a fourth role would fail to compile here.
            Role::System => unreachable!("a system turn was consumed above"),
        };
        let mut content = Vec::with_capacity(message.content.len());
        for block in &message.content {
            content.push(wire_block(block)?);
        }
        // A turn with nothing in it is not a turn. The API rejects an empty
        // `content`, and a message ego never had anything to say in is not
        // worth a round trip to be told so.
        if !content.is_empty() {
            wire.push(WireMessage { role, content });
        }
    }
    let system = (!system.is_empty()).then_some(system);
    Ok((system, wire))
}

fn wire_block(block: &ContentBlock) -> Result<WireBlock<'_>, Inexpressible> {
    Ok(match block {
        ContentBlock::Text { text } => WireBlock::Text { kind: "text", text },
        ContentBlock::Audio { .. } => unreachable!("audio is refused before wire serialization"),
        ContentBlock::Image { image } => WireBlock::Image {
            kind: "image",
            source: ImageSource {
                kind: "base64",
                media_type: image.media_type().mime(),
                data: base64::engine::general_purpose::STANDARD.encode(image.bytes()),
            },
        },
        ContentBlock::Reasoning { state } | ContentBlock::ProviderContent { state } => {
            if state.dialect != DIALECT {
                return Err(Inexpressible::ReasoningStateFromAnotherDialect {
                    dialect: state.dialect.clone(),
                });
            }
            WireBlock::Replayed(
                serde_json::from_str::<&RawValue>(&state.blob)
                    .map_err(|_| Inexpressible::ReasoningStateNotJson)?,
            )
        }
        ContentBlock::ToolUse {
            call_id,
            name,
            args,
        } => WireBlock::ToolUse {
            kind: "tool_use",
            id: call_id,
            name,
            input: serde_json::from_str::<&RawValue>(args).map_err(|_| {
                Inexpressible::ArgumentsNotJson {
                    call_id: call_id.clone(),
                }
            })?,
        },
        ContentBlock::ToolResult {
            call_id,
            output,
            is_error,
        } => WireBlock::ToolResult {
            kind: "tool_result",
            tool_use_id: call_id,
            content: output,
            is_error: *is_error,
        },
    })
}

/// The exact JSON bytes this format would send for `req`.
///
/// Separate from [`build_request`] so a golden can pin the body without a URL
/// or a credential in it — the body is the contract with the provider; the
/// endpoint and the auth header are the provider row's business.
pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    if super::request_has_audio(req) {
        return Err(Inexpressible::AudioNotSupported);
    }
    let max_tokens = req
        .max_output_tokens
        .ok_or(Inexpressible::CeilingRequired)?;
    let effort = effort(req.thinking)?;
    let (system, messages) = project(req.messages.messages())?;
    let mut tools: Vec<WireTool<'_>> = req
        .tools
        .iter()
        .filter(|tool| req.web_search.is_none() || tool.name != "web_search")
        .map(|tool| WireTool::Function {
            name: &tool.name,
            description: &tool.description,
            input_schema: &tool.parameters,
        })
        .collect();
    if let Some(search) = req.web_search {
        tools.push(WireTool::WebSearch {
            kind: "web_search_20250305",
            name: "web_search",
            max_uses: search.max_requests.get(),
        });
    }
    let tool_choice = (!tools.is_empty() && !req.parallel_tool_calls).then_some(ToolChoice {
        kind: "auto",
        disable_parallel_tool_use: true,
    });
    let wire = WireRequest {
        model: &req.model,
        max_tokens,
        stream: true,
        system,
        messages,
        tools,
        tool_choice,
        thinking: match effort {
            Some(_) => Thinking {
                kind: "adaptive",
                display: Some("summarized"),
            },
            None => Thinking {
                kind: "disabled",
                display: None,
            },
        },
        output_config: effort.map(|effort| OutputConfig { effort }),
    };
    if super::cache::control(req).is_none() {
        return Ok(serde_json::to_vec(&wire).expect("WireRequest serialises JSON"));
    }
    let mut body = serde_json::to_value(&wire).expect("WireRequest serialises JSON");
    if let Some(control) = super::cache::control(req) {
        if let Some(system) = body.get_mut("system") {
            super::cache::mark_content(system, &control);
        }
        let stable = req
            .messages
            .messages()
            .iter()
            .take(
                req.cache
                    .as_ref()
                    .expect("cache control has metadata")
                    .stable_messages,
            )
            .filter(|message| message.role != Role::System && !message.content.is_empty())
            .count();
        if let Some(messages) = body["messages"].as_array_mut() {
            for message in messages.iter_mut().take(stable).rev() {
                if super::cache::mark_content(&mut message["content"], &control) {
                    break;
                }
            }
        }
    }
    Ok(serde_json::to_vec(&body).expect("a JSON value serialises"))
}

/// Fingerprint the longest explicitly marked prefix of the encoded request.
/// Later, volatile messages cannot change this identity; changes to the
/// marked prefix, tools, or generation settings do change it.
fn cache_prefix(body: &[u8]) -> Option<String> {
    let mut request: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = request.get_mut("messages")?.as_array_mut()?;
    let marked = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            message["content"]
                .as_array()?
                .iter()
                .rposition(|block| block.get("cache_control").is_some())
                .map(|block| (index, block))
        });
    if let Some((index, block)) = marked {
        messages.truncate(index + 1);
        messages[index]["content"]
            .as_array_mut()?
            .truncate(block + 1);
    } else {
        let system_marked = request["system"].as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("cache_control").is_some())
        });
        if !system_marked {
            return None;
        }
        request["messages"].as_array_mut()?.clear();
    }
    request.as_object_mut()?.remove("max_tokens");
    request.as_object_mut()?.remove("stream");
    Some(ego_core::digest::sha256_hex(
        &serde_json::to_vec(&request).ok()?,
    ))
}

#[cfg(test)]
mod cache_prefix_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn digest_tracks_the_marked_request_prefix_not_volatile_messages() {
        let body = |marked, volatile| {
            json!({
                "model": "model-a", "max_tokens": 100, "stream": true,
                "system": [{"type":"text", "text":"system", "cache_control":{"type":"ephemeral"}}],
                "messages": [
                    {"role":"user", "content":[{"type":"text", "text":marked,
                        "cache_control":{"type":"ephemeral"}}]},
                    {"role":"user", "content":[{"type":"text", "text":volatile}]}
                ]
            })
        };
        let digest = |body: serde_json::Value| cache_prefix(&serde_json::to_vec(&body).unwrap());
        let baseline = digest(body("stable", "first"));
        assert!(baseline.is_some());
        assert_eq!(baseline, digest(body("stable", "second")));
        assert_ne!(baseline, digest(body("changed", "first")));
        let mut other_model = body("stable", "first");
        other_model["model"] = json!("model-b");
        assert_ne!(baseline, digest(other_model));
        let mut unmarked = body("stable", "first");
        unmarked["system"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        unmarked["messages"][0]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert_eq!(digest(unmarked), None);
    }
}

/// A complete HTTP request for `req` against `target`.
///
/// The version header is pushed on after [`RequestTarget::post_json`] rather
/// than instead of it: the two headers that call adds are facts about the body
/// being JSON and the answer being a stream, which are true of every streaming
/// mapping, and this one is a fact about *which contract* — true of this
/// mapping alone. Building the request by hand to add it would have meant this
/// file remembering the other two, and a mapping that has to remember them is
/// a mapping that can forget one.
pub fn build_request(
    target: &RequestTarget<'_>,
    req: &GenRequest,
) -> Result<HttpRequest, Inexpressible> {
    let mut request = target.post_json(PATH, request_body(req)?);
    request.headers.push(crate::http::public_header(
        VERSION_HEADER,
        VERSION.as_bytes().to_vec(),
    ));
    Ok(request)
}

// --- response ---

/// One event's `data` payload.
///
/// `type` is inside the JSON as well as on the `event:` line, so the
/// discriminator survives an SSE reader that keeps only the data — which
/// [`SseDecoder`] is, and deliberately: a mapping that needed the event name
/// would need a second framing contract beside the one every other wire uses.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    MessageStart {
        message: StartMessage,
    },
    ContentBlockStart {
        index: u32,
        content_block: StartBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: BlockDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: StopDelta,
        #[serde(default)]
        usage: Option<DeltaUsage>,
    },
    MessageStop,
    Error {
        error: WireError,
    },
    /// `ping`, and anything the vendor adds after this was written.
    ///
    /// Fails *open* here and only here: an unrecognised frame in a stream that
    /// is otherwise proceeding normally is a frame this mapping has no use
    /// for, not evidence the turn went wrong. What fails closed is the
    /// terminal vocabulary — an unrecognised `stop_reason` — because that one
    /// decides what the caller is told happened.
    #[serde(other)]
    Ignored,
}

#[derive(Debug, Deserialize)]
struct StartMessage {
    #[serde(default)]
    usage: Option<StartUsage>,
}

#[derive(Debug, Deserialize)]
struct StartUsage {
    #[serde(default)]
    server_tool_use: Option<ServerToolUsage>,
    /// **Already the uncached figure.** The SDK states the identity verbatim:
    /// "Total input tokens in a request is the summation of `input_tokens`,
    /// `cache_creation_input_tokens`, and `cache_read_input_tokens`." So this
    /// partition is *additive*, where the family's is subtractive — and
    /// applying the family's arithmetic here would underreport the dearest
    /// category, silently, by exactly the cache.
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    /// The cache-write total split by retention, when the request asked for
    /// one. Anthropic prices the two differently, which is why
    /// [`Usage::cache_write_extended_tokens`] exists as its own category.
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ServerToolUsage {
    #[serde(default)]
    web_search_requests: u64,
}

#[derive(Debug, Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct DeltaUsage {
    #[serde(default)]
    server_tool_use: Option<ServerToolUsage>,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct StopDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StartBlock {
    Text,
    Thinking,
    ToolUse {
        id: String,
        name: String,
    },
    /// Server-side tool calls and their results, container uploads, and the
    /// twelve-and-growing rest of the block union. Opened and closed with
    /// nothing emitted: ego runs none of them, and inventing a `ToolCall` for
    /// a call the *provider* executes would have the harness run it twice.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    /// `citations_delta`, and whatever follows it.
    #[serde(other)]
    Other,
}

/// One content block, being assembled from the deltas it arrives in.
#[derive(Debug)]
enum Block {
    Text,
    Thinking {
        thinking: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        args: String,
    },
    Other,
}

/// A thinking block, rebuilt for replay.
///
/// The field order is the wire's and is load bearing in the weak sense that a
/// reader diffing a replayed block against a captured one should see no
/// difference. What is load bearing in the strong sense is that all three
/// fields are here: the provider requires the block back "exactly as
/// received", and a blob holding only the signature could not produce that.
#[derive(Debug, Serialize)]
struct ThinkingBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    thinking: &'a str,
    signature: &'a str,
}

/// Why a stream ended without the model saying why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Incomplete {
    CutMidFrame,
    EndedBetweenFrames,
}

/// How a turn ended, as this protocol says it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ending {
    /// A reason with an exact counterpart in [`StopReason`].
    Mapped(StopReason),
    /// The provider's exact context-window exhaustion discriminator.
    ContextOverflow,
    /// The model declined. `Error` loses the policy category `stop_details`
    /// carries, and that loss is named rather than hidden.
    Refusal,
    /// A reason with no meaning here — `pause_turn` above all, which means
    /// "send the response back as-is to continue" and is therefore an
    /// instruction to the *loop*, not a description of an ending. Both
    /// available readings destroy it: `Stop` truncates an answer the provider
    /// intended to resume, and `Error` aborts it. So it is reported as what it
    /// is, and the operator learns the truth instead of a mistranslation.
    Unmappable(String),
}

/// Turns chunks of `text/event-stream` bytes into [`AiEvent`]s.
///
/// Separate from the byte stream that feeds it, and pure, so a transcript can
/// be replayed at any chunk boundary — which is how the goldens prove that a
/// cut mid-frame is handled rather than merely survived.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    sse: SseDecoder,
    /// Open blocks by index. Ordered so a flush is deterministic whatever
    /// order the provider interleaved them in.
    blocks: BTreeMap<u32, Block>,
    /// The input figures, which arrive once, in `message_start`.
    ///
    /// `None` means the provider never stated them, and no [`AiEvent::Usage`]
    /// is emitted in that case: a zero here would be a *measured* zero for the
    /// dearest category, and settling at the pessimistic bound is the failure
    /// direction that costs budget rather than the one that leaks it.
    input: Option<StartUsage>,
    /// The output count, **overwritten and never summed**.
    ///
    /// `message_delta.usage` is cumulative and there can be several. Adding
    /// them would bill the caller for the sum of the provider's restatements
    /// of one figure — the double-count [`AiEvent::Usage`]'s own doc warns
    /// three of the six wires can cause.
    output_tokens: u64,
    web_search_requests: u64,
    hosted: bool,
    provider_blocks: BTreeMap<u32, serde_json::Value>,
    provider_inputs: BTreeMap<u32, String>,
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

    /// End the stream, and emit the one terminal event.
    ///
    /// `transport` is whatever killed the byte stream, if anything, so that its
    /// [`DeliveryCertainty`] reaches the attempt — the reading only
    /// [`HttpError`] is in a position to take.
    pub fn finish(mut self, transport: Option<HttpError>) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if let Some(error) = &transport {
            out.push(AiEvent::DeliveryObserved(error.delivery()));
        }
        if let Some(usage) = self.usage() {
            out.push(AiEvent::Usage(usage));
        }
        // An error the provider sent in-band outranks everything else it said:
        // it arrives after a 200 has been committed, which is precisely when
        // the stop reason that came before it stopped being true.
        if let Some((message, kind)) = self.provider_error.take() {
            out.push(AiEvent::Error { message, kind });
            out.push(AiEvent::Stop(StopReason::Error));
            return out;
        }
        match self.ending.take() {
            // The model said why it stopped, so it stopped that way — even if
            // the framing failed afterwards, which costs at most the usage
            // report and that settles pessimistically on its own.
            Some(Ending::Mapped(stop)) => {
                if let Some(error) = &transport {
                    tracing::warn!(
                        %error,
                        "the response stream failed after the model had already \
                         reported why it stopped; the turn is complete and the \
                         usage report may be missing"
                    );
                }
                out.push(AiEvent::Stop(stop));
            }
            Some(Ending::ContextOverflow) => {
                out.push(AiEvent::Error {
                    message: "the provider reported that the model context window is exhausted"
                        .to_string(),
                    kind: ErrorKind::ContextOverflow,
                });
                out.push(AiEvent::Stop(StopReason::Error));
            }
            Some(Ending::Refusal) => {
                out.push(AiEvent::Error {
                    message: "the model declined to answer: the provider reported \
                              stop_reason \"refusal\", whose policy category ego \
                              has nowhere to record"
                        .to_string(),
                    // The request will be declined again, so nothing about a
                    // retry changes it.
                    kind: ErrorKind::Invalid,
                });
                out.push(AiEvent::Stop(StopReason::Error));
            }
            Some(Ending::Unmappable(spelled)) => {
                out.push(AiEvent::Error {
                    message: format!(
                        "the provider ended the turn with stop_reason \"{spelled}\", \
                         which this build has no meaning for; it is reported rather \
                         than mapped onto a reason it does not have"
                    ),
                    kind: ErrorKind::Unclassified,
                });
                out.push(AiEvent::Stop(StopReason::Error));
            }
            None => {
                // Fragments of a call the model never finished asking for are
                // not a call. Dropping them is named in the error rather than
                // done quietly.
                let abandoned = self
                    .blocks
                    .values()
                    .filter(|block| matches!(block, Block::ToolUse { .. }))
                    .count();
                self.blocks.clear();
                let kind = transport
                    .as_ref()
                    .map_or(ErrorKind::Transient, HttpError::kind);
                out.push(AiEvent::Error {
                    message: self.incomplete_message(transport.as_ref(), abandoned),
                    kind,
                });
                out.push(AiEvent::Stop(StopReason::Error));
            }
        }
        out
    }

    /// The one usage report, or none.
    ///
    /// No subtraction anywhere, which is the whole difference from the
    /// family's [`map_usage`](super::openai_completions): this wire's input
    /// figures are already disjoint, so there is nothing to underflow and no
    /// incoherent-breakdown case to report.
    fn usage(&self) -> Option<Usage> {
        let input = self.input.as_ref()?;
        let (cache_write_tokens, cache_write_extended_tokens) = match &input.cache_creation {
            Some(split) => (
                split.ephemeral_5m_input_tokens,
                split.ephemeral_1h_input_tokens,
            ),
            // A request that asked for no extended retention wrote nothing at
            // one. Zero is that request's measurement, not an absent figure —
            // which is why the field is a `u64` and not an `Option`.
            None => (input.cache_creation_input_tokens, 0),
        };
        Some(Usage {
            uncached_input_tokens: input.input_tokens,
            cache_read_tokens: input.cache_read_input_tokens,
            cache_write_tokens,
            cache_write_extended_tokens,
            output_tokens: self.output_tokens,
            // Anthropic bills thinking inside `output_tokens` and publishes no
            // separate count, so this is unknown rather than zero. `None` says
            // so instead of claiming the model reasoned for nothing.
            web_search_requests: self.web_search_requests,
            reasoning_tokens: None,
        })
    }

    fn incomplete_message(&self, transport: Option<&HttpError>, abandoned: usize) -> String {
        let cause = match transport {
            Some(error) => format!("the response stream failed: {error}"),
            None => match self.incomplete() {
                Incomplete::CutMidFrame => {
                    "the response stream was cut in the middle of an event".to_string()
                }
                Incomplete::EndedBetweenFrames => {
                    "the response stream ended between events".to_string()
                }
            },
        };
        let mut message =
            format!("{cause}, so the model never reported why the generation stopped");
        if abandoned > 0 {
            message.push_str(&format!(
                "; {abandoned} half-assembled tool call(s) were discarded rather \
                 than dispatched with truncated arguments"
            ));
        }
        message
    }

    fn incomplete(&self) -> Incomplete {
        if self.sse.has_partial_frame() {
            Incomplete::CutMidFrame
        } else {
            Incomplete::EndedBetweenFrames
        }
    }

    fn consume(&mut self, frame: &str, out: &mut Vec<AiEvent>) {
        let decoded: Frame = match serde_json::from_str(frame) {
            Ok(decoded) => decoded,
            Err(error) => {
                // Skipped rather than fatal, on the same reading the family
                // takes: one unreadable frame in a stream that is otherwise
                // proceeding is a gap in the output, and the terminal frame
                // still decides how the turn ended.
                tracing::warn!(%error, "an event in the response stream could not be decoded");
                return;
            }
        };
        self.capture_provider_frame(frame, out);
        match decoded {
            Frame::MessageStart { message } => self.consume_start(message),
            Frame::ContentBlockStart {
                index,
                content_block,
            } => {
                self.blocks.insert(index, open(content_block));
            }
            Frame::ContentBlockDelta { index, delta } => self.consume_delta(index, delta, out),
            Frame::ContentBlockStop { index } => self.close(index, out),
            Frame::MessageDelta { delta, usage } => {
                if let Some(usage) = usage {
                    self.output_tokens = usage.output_tokens;
                    if let Some(server) = usage.server_tool_use {
                        self.web_search_requests = server.web_search_requests;
                    }
                }
                if let Some(spelled) = delta.stop_reason {
                    self.ending = Some(ending(&spelled));
                }
            }
            // The terminal marker, and there is nothing to do at it. What ends
            // the generation is the bytes running out, because the stop reason
            // arrives in the frame *before* this one and the usage figures can
            // still be restated after it.
            Frame::MessageStop | Frame::Ignored => {}
            Frame::Error { error } => {
                self.provider_error = Some((error.message, classify_error(&error.kind)));
            }
        }
    }

    fn capture_provider_frame(&mut self, frame: &str, out: &mut Vec<AiEvent>) {
        use serde_json::{Value, json};
        let Ok(raw) = serde_json::from_str::<Value>(frame) else {
            return;
        };
        let Some(index) = raw["index"].as_u64().and_then(|n| u32::try_from(n).ok()) else {
            return;
        };
        match raw["type"].as_str() {
            Some("content_block_start") => {
                let block = &raw["content_block"];
                let kind = block["type"].as_str().unwrap_or_default();
                if kind == "text" {
                    if block["citations"]
                        .as_array()
                        .is_some_and(|citations| !citations.is_empty())
                    {
                        self.hosted = true;
                    }
                    if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                        out.push(AiEvent::TextDelta { text: text.into() });
                    }
                }
                if kind == "web_search_tool_result"
                    || (kind == "server_tool_use" && block["name"] == "web_search")
                {
                    self.hosted = true;
                }
                if kind == "text"
                    || kind == "web_search_tool_result"
                    || (kind == "server_tool_use" && block["name"] == "web_search")
                {
                    self.provider_blocks.insert(index, block.clone());
                }
            }
            Some("content_block_delta") => {
                let Some(block) = self.provider_blocks.get_mut(&index) else {
                    return;
                };
                let delta = &raw["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let mut text = block["text"].as_str().unwrap_or_default().to_owned();
                        text.push_str(delta["text"].as_str().unwrap_or_default());
                        block["text"] = json!(text);
                    }
                    Some("input_json_delta") => self
                        .provider_inputs
                        .entry(index)
                        .or_default()
                        .push_str(delta["partial_json"].as_str().unwrap_or_default()),
                    Some("citations_delta") => {
                        self.hosted = true;
                        if !block["citations"].is_array() {
                            block["citations"] = json!([]);
                        }
                        block["citations"]
                            .as_array_mut()
                            .expect("array")
                            .push(delta["citation"].clone());
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let Some(mut block) = self.provider_blocks.remove(&index) else {
                    return;
                };
                if let Some(input) = self.provider_inputs.remove(&index) {
                    let Ok(input) = serde_json::from_str::<Value>(&input) else {
                        return;
                    };
                    block["input"] = input;
                }
                if !self.hosted {
                    return;
                }
                let visible = block["type"] == "text";
                if block["type"] == "server_tool_use" {
                    out.push(AiEvent::WebSearch {
                        call_id: block["id"].as_str().unwrap_or_default().into(),
                        query: block["input"]["query"]
                            .as_str()
                            .unwrap_or("web search")
                            .into(),
                    });
                }
                for citation in block["citations"].as_array().into_iter().flatten() {
                    if let Some(url) = citation["url"].as_str() {
                        out.push(AiEvent::Citation {
                            url: url.into(),
                            title: citation["title"].as_str().unwrap_or(url).into(),
                        });
                    }
                }
                out.push(AiEvent::ProviderContent {
                    state: ReasoningState {
                        blob: serde_json::to_string(&block).expect("JSON value"),
                        dialect: DIALECT.into(),
                    },
                    visible,
                });
            }
            _ => {}
        }
    }

    fn consume_start(&mut self, message: StartMessage) {
        let Some(usage) = message.usage else { return };
        self.output_tokens = usage.output_tokens;
        self.web_search_requests = usage
            .server_tool_use
            .as_ref()
            .map_or(0, |server| server.web_search_requests);
        self.input = Some(usage);
    }

    fn consume_delta(&mut self, index: u32, delta: BlockDelta, out: &mut Vec<AiEvent>) {
        let Some(block) = self.blocks.get_mut(&index) else {
            tracing::warn!(
                index,
                "a delta arrived for a content block that never opened"
            );
            return;
        };
        match (block, delta) {
            (Block::Text, BlockDelta::TextDelta { text }) => {
                out.push(AiEvent::TextDelta { text });
            }
            (Block::Thinking { thinking, .. }, BlockDelta::ThinkingDelta { thinking: chunk }) => {
                thinking.push_str(&chunk);
                // Emitted as it arrives *and* accumulated, because the two are
                // different values with different readers: this is what a
                // human sees, and the accumulation is what the provider
                // requires back verbatim.
                out.push(AiEvent::ReasoningDelta { text: chunk });
            }
            (Block::Thinking { signature, .. }, BlockDelta::SignatureDelta { signature: sig }) => {
                signature.push_str(&sig);
            }
            (Block::ToolUse { args, .. }, BlockDelta::InputJsonDelta { partial_json }) => {
                args.push_str(&partial_json);
            }
            // A delta whose type does not belong to the block it names —
            // `citations_delta` on a text block, or anything added later.
            // There is nothing to accumulate it into.
            _ => {}
        }
    }

    fn close(&mut self, index: u32, out: &mut Vec<AiEvent>) {
        let Some(block) = self.blocks.remove(&index) else {
            return;
        };
        match block {
            Block::Text | Block::Other => {}
            Block::Thinking {
                thinking,
                signature,
            } => {
                let blob = serde_json::to_string(&ThinkingBlock {
                    kind: "thinking",
                    thinking: &thinking,
                    signature: &signature,
                })
                .expect("a struct of strings serialises");
                out.push(AiEvent::ReasoningState(ReasoningState {
                    blob,
                    dialect: DIALECT.to_string(),
                }));
            }
            Block::ToolUse { id, name, args } => out.push(AiEvent::ToolCall {
                call_id: id,
                name,
                args,
            }),
        }
    }
}

fn open(block: StartBlock) -> Block {
    match block {
        StartBlock::Text => Block::Text,
        StartBlock::Thinking => Block::Thinking {
            thinking: String::new(),
            signature: String::new(),
        },
        StartBlock::ToolUse { id, name } => Block::ToolUse {
            id,
            name,
            args: String::new(),
        },
        StartBlock::Other => Block::Other,
    }
}

/// One `stop_reason`, as what it means here.
///
/// Fails **closed**: a reason nobody has mapped is not a reason that said "the
/// turn finished normally". The cost of being wrong that way is one reported
/// error; the cost of guessing `Stop` is a truncated answer presented as a
/// complete one.
fn ending(spelled: &str) -> Ending {
    match spelled {
        "pause_turn" => Ending::Mapped(StopReason::Continue),
        "end_turn" | "stop_sequence" => Ending::Mapped(StopReason::Stop),
        "max_tokens" => Ending::Mapped(StopReason::Length),
        "model_context_window_exceeded" => Ending::ContextOverflow,
        "tool_use" => Ending::Mapped(StopReason::ToolUse),
        "refusal" => Ending::Refusal,
        other => Ending::Unmappable(other.to_string()),
    }
}

/// Retryability, read from `error.type` rather than from the prose.
///
/// The prose is for a human and changes without notice; the type is the field
/// the vendor documents as the discriminator. The docs are explicit that the
/// list will grow, which is why the fall-through is
/// [`ErrorKind::Unclassified`] and not a guess in either direction.
fn classify_error(kind: &str) -> ErrorKind {
    match kind {
        "overloaded_error" | "rate_limit_error" | "api_error" | "timeout_error" => {
            ErrorKind::Transient
        }
        "invalid_request_error" | "not_found_error" | "request_too_large" => ErrorKind::Invalid,
        "authentication_error" | "permission_error" | "billing_error" => ErrorKind::Refused,
        _ => ErrorKind::Unclassified,
    }
}

/// Decode one provider response into the generation's events.
///
/// The state machine is [`super::decode_response`]'s, shared with every other
/// streaming mapping: what a *response* means is `AttemptStream`'s contract
/// and not this format's, and only what a *frame* means belongs here.
///
/// `cost` is accepted and unused, and that is the row's claim rather than an
/// omission here: Anthropic reports tokens and never a monetary figure
/// (`ARCHITECTURE.md` §2.2, from a recorded fixture), so its
/// row carries `cost: None` and conformance check 5 fails if that ever stops
/// matching what its transcripts hold.
pub fn decode(
    response: Result<HttpResponse, HttpError>,
    cost: Option<Arc<dyn ProviderCost>>,
) -> BoxStream<'static, AiEvent> {
    debug_assert!(
        cost.is_none(),
        "anthropic-messages states no monetary total, so a cost decoder here \
         would read a field that is never sent"
    );
    super::decode_response(response, Box::new(StreamDecoder::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_http_error;
    use ego_core::generator::attempt::DeliveryCertainty;
    use ego_core::permit::{TransportError, TransportFailure};

    const TEXT_TRANSCRIPT: &str =
        include_str!("../../tests/goldens/profile/anthropic/stream/text.sse");

    #[test]
    fn a_transport_failure_keeps_what_the_transport_observed() {
        let cut = TEXT_TRANSCRIPT
            .split_once("event: message_delta")
            .expect("the golden has one")
            .0;
        let mut decoder = StreamDecoder::default();
        let mut events = decoder.push(cut.as_bytes());
        events.extend(
            decoder.finish(Some(test_http_error(TransportError::possibly_sent(
                TransportFailure::Exchange("anthropic decoder marker".to_owned()),
            )))),
        );

        assert!(
            events.iter().any(|event| matches!(
                event,
                AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent)
            )),
            "{events:?}"
        );
        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
    }

    #[test]
    fn a_cut_after_the_stop_reason_is_a_complete_turn() {
        let upto = TEXT_TRANSCRIPT
            .find(r#""stop_reason":"end_turn""#)
            .map(|at| TEXT_TRANSCRIPT[at..].find("\n\n").expect("the frame ends") + at + 2)
            .expect("the golden has one");
        let mut decoder = StreamDecoder::default();
        let mut events = decoder.push(&TEXT_TRANSCRIPT.as_bytes()[..upto]);
        events.extend(
            decoder.finish(Some(test_http_error(TransportError::possibly_sent(
                TransportFailure::Exchange("anthropic decoder marker".to_owned()),
            )))),
        );

        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::Error { .. })),
            "the model finished; the transport failing afterwards is not the turn \
             failing: {events:?}"
        );
    }
}
