//! The `openai-completions` wire format: `POST /chat/completions`, streamed.
//!
//! The shape behind OpenAI, OpenRouter, Groq, Together, Fireworks, DeepSeek,
//! the API-key xAI candidate, and local daemons that advertise an
//! "OpenAI-compatible" endpoint
//! (`ARCHITECTURE.md` §3.1). Every field name, enum spelling and nullability
//! below was read from `openai/openai-openapi` rather than recalled — the
//! constraint `plans/m0b-vertical-slice.md` Step 9 puts on this step, because
//! a golden pinned from memory pins the memory, not the contract.
//!
//! # What the schema settled, and what it ruled out
//!
//! - **The usage breakdown is a partition, not overlapping subsets.** OpenAI's
//!   own aggregate schema says `input_tokens` is "including cached and
//!   cache-write tokens" while `input_uncached_tokens` is "excluding
//!   cache-write tokens", and both published examples add up exactly
//!   (400+100+500=1000; 1800+200+3000=5000). So `uncached = prompt_tokens −
//!   cached_tokens − cache_write_tokens`, and
//!   [`Usage::total_tokens`](ego_core::state::Usage::total_tokens) comes back
//!   to `prompt + completion` with nothing counted twice. Subtracting only
//!   `cached_tokens` — the obvious reading, and the one the per-request schema
//!   alone would leave you with — would double-count every cache-write token.
//! - **There is no reasoning text in *OpenAI's* schema.** The streaming delta
//!   has `content`, `refusal`, `tool_calls`, `role` and `function_call`, and
//!   nothing else; reasoning appears only as a *count*
//!   (`completion_tokens_details.reasoning_tokens`).
//!
//!   But this format serves a family, and other members of it do stream the
//!   text — as `reasoning` or as `reasoning_content`, in the same `delta`
//!   object. Those are not a different row's problem: `ARCHITECTURE.md`'s
//!   wire decision §4 rules that an extension field is decoded
//!   **opportunistically and uniformly for every member**, because the field
//!   *name* is the contract and the vendor is not. A member that never sends
//!   it simply leaves the field absent, so there is nothing to branch on and
//!   no provider identity to consult.
//!
//!   `Delta` decodes both names, so this is where
//!   [`AiEvent::ReasoningDelta`] gets its first producer in the workspace.
//!   The measured cost of not decoding them is in
//!   `tests/ollama_reasoning.rs`: 117 of 118 output tokens billed and
//!   dropped. **The tokens are still not separable** — this family's usage
//!   chunk has no `completion_tokens_details` on the rows that send reasoning
//!   — and that half stays open as `POTENTIALS.md` P24.
//! - **`finish_reason` has five spellings**, and one of them —
//!   `content_filter` — has no [`StopReason`] to land on. It is reported as
//!   [`StopReason::Error`] with a named [`AiEvent::Error`] beside it rather
//!   than mapped onto `Stop`, which would claim a filtered answer finished
//!   normally.
//!
//!   `consume_finish_reason` maps a sixth, `end_turn`, which is *not* in the
//!   schema and arrives anyway: a gateway passing its upstream's reason
//!   through untranslated. Decoded for the whole family and not for the row
//!   that sends it, on the same rule as the reasoning fields above — the
//!   string is the contract and the vendor is not.
//!
//! # The one event ordering rule
//!
//! **`Stop` is last, always, exactly once.** Not at the `finish_reason` chunk,
//! where it would arrive before the usage chunk that follows it — OpenAI sends
//! usage *after* the choice that ends the turn and before `data: [DONE]`. So
//! the finish reason is remembered when it arrives and emitted when the byte
//! stream ends, whatever ended it.
//!
//! Which gives the truncation table a single line: **the `finish_reason` is
//! the answer.** If it arrived, the generation ended the way it says, and a
//! stream cut afterwards costs only the usage report — which
//! [`UsageReport::Missing`](ego_core::generator::attempt::UsageReport) already
//! charges pessimistically, and which OpenAI's own schema warns about ("If the
//! stream is interrupted, you may not receive the final usage chunk"). If it
//! never arrived, the generation ended in `Error`, and the error says which of
//! the ways it was: cut mid-frame, ended between frames, or `[DONE]` with
//! nothing before it.

use std::collections::BTreeMap;
use std::sync::Arc;

use ego_core::cost::Money;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, Message, Role, StopReason, ThinkingLevel,
};
use ego_core::state::Usage;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::provider_cost::ProviderCost;
use super::sse::SseDecoder;
use super::{RequestRefused, RequestTarget, WireFormat};

/// The `openai-completions` mapping, as a value a profile can name.
///
/// A unit struct: everything that varies between the providers speaking this
/// shape — the address, the credential, whether a monetary total is stated —
/// is on the profile and arrives as a parameter, so there is nothing left for
/// an instance to hold. That emptiness is the ADR §4 claim made structural: a
/// mapping with no per-provider state has nowhere to keep a per-provider
/// branch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiCompletions;

impl WireFormat for OpenAiCompletions {
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
/// Held here rather than in the provider table because it is a fact about the
/// API shape, not about any one vendor: every provider that speaks this format
/// serves it at this path.
pub const PATH: &str = "chat/completions";

// --- request ---

/// A `GenRequest` this format cannot express without lying about it.
///
/// Every variant is a case where the alternative would be to drop something
/// the caller asked for and send the request anyway. `docs/08-codex-critique.md`
/// I5 is about exactly that: a shared HTTP shape is not equivalent semantics,
/// and a silent mistranslation is worse than a refusal because nothing
/// downstream can tell it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inexpressible {
    /// The request carries reasoning state, and this format has nowhere to put
    /// it: the chat-completions request schema has no field for a reasoning
    /// signature. Dropping it silently would be
    /// [`ReasoningCarryover::Unsupported`](ego_core::generator::capabilities::ReasoningCarryover::Unsupported)
    /// pretending to be support.
    ReasoningStateNotCarriable,
}

impl std::fmt::Display for Inexpressible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Inexpressible::ReasoningStateNotCarriable => write!(
                f,
                "openai-completions has no field for reasoning state, so \
                 carrying it over would mean dropping it"
            ),
        }
    }
}

impl std::error::Error for Inexpressible {}

/// The refusal, rendered for the seam.
///
/// The sentence is the whole payload, and it already names this format — which
/// is why [`Unexpressible`] carries no separate discriminant. This mapping's
/// own tests keep matching the enum; only the caller that renders loses the
/// variant, and rendering is all it ever did with it.
impl From<Inexpressible> for RequestRefused {
    fn from(refused: Inexpressible) -> Self {
        RequestRefused::new(refused.to_string())
    }
}

/// Everything in this family's request body that does not depend on which
/// member is being addressed.
///
/// Split out because exactly one member's body differs, in exactly one field:
/// Ollama reads `max_tokens` and discards `max_completion_tokens`
/// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §8).
/// §4 place 3 makes that an adapter difference and offers no third option, so
/// the difference is carried by *which mapping serialises this*, and never by
/// a flag one shared mapping reads. `pub(crate)` for that reason and no other:
/// [`super::ollama`] is the second serialiser, not a second copy.
#[derive(Debug, Serialize)]
pub(crate) struct CommonRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
    stream_options: StreamOptions,
    /// Always sent, including `"none"`.
    ///
    /// Omitting it when the caller said [`ThinkingLevel::Off`] would silently
    /// select the schema's documented default of `medium`: the model would
    /// reason after being told not to, and the caller would be billed for
    /// reasoning tokens they refused. A provider that rejects the field is a
    /// provider row that needs a different mapping, not a reason for this one
    /// to send a value nobody asked for.
    reasoning_effort: &'static str,
    /// Omitted when empty, together with `parallel_tool_calls`, which OpenAI
    /// rejects when there are no tools to parallelise.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
}

/// The body this mapping sends: the common part, plus the ceiling in the
/// field `openai/openai-openapi` defines for it.
///
/// A wrapper and not a field on [`CommonRequest`], so that the ceiling's
/// *name* is a property of the type doing the serialising. `max_tokens` is
/// then not a field this mapping declines to set — it is a field this mapping
/// does not have, which is a difference a `serde` derive enforces and a
/// convention does not.
#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    #[serde(flatten)]
    common: CommonRequest<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    /// Buys one extra chunk before `data: [DONE]` carrying the whole request's
    /// usage. Without it the settlement has no figures at all and every
    /// generation reconciles at its pessimistic bound.
    ///
    /// **One member of this family calls the field deprecated, and it is sent
    /// anyway.** OpenRouter states that this flag and its own
    /// `usage: {include: true}` "are deprecated and have no effect", returning
    /// full usage automatically. That is not a reason to drop it:
    /// `ChatCompletionStreamOptions.include_usage` is undeprecated on the
    /// contract *this mapping is derived from*, where it is the only way usage
    /// arrives at all; xAI requires it on the REST path and Ollama honours it.
    /// OpenRouter retired a **precondition, not a spelling** — nothing
    /// succeeded it — so there is no second spelling to be carrying, which is
    /// the shape `AGENTS.md` forbids. Audited in
    /// `docs/15-provider-contract-audit.md` §3 and guarded by
    /// `tests/deprecated_spellings.rs`.
    include_usage: bool,
}

/// One message as this format spells it.
///
/// The fields are optional because chat-completions uses one object shape for
/// three different messages: a plain turn has `content`; an assistant turn
/// that called tools may have `tool_calls` and no `content` ("Required unless
/// `tool_calls` or `function_call` is specified"); and a tool result is its
/// own message with `tool_call_id` and `content`.
#[derive(Debug, Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireCallFunction<'a>,
}

#[derive(Debug, Serialize)]
struct WireCallFunction<'a> {
    name: &'a str,
    /// The arguments verbatim, as a JSON *string* — which is what the schema
    /// asks for, and what a streamed call arrives as. Not re-serialised, so a
    /// call that arrived malformed leaves exactly as it arrived.
    arguments: &'a str,
}

#[derive(Debug, Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Debug, Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

/// `ThinkingLevel` to `reasoning_effort`.
///
/// One-to-one, because the schema's enum is `none | minimal | low | medium |
/// high | xhigh | max` and `ThinkingLevel` is that list with `Off` spelled
/// differently. Nothing is approximated here, so nothing has to be documented
/// as approximate.
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

/// A [`Conversation`] as this format's `messages` array.
///
/// Total, and it has to be: [`Conversation`] already holds every invariant
/// this projection needs (a call is the assistant's, a result is not, every
/// result answers an earlier call exactly once), so there is nothing left
/// here to refuse.
///
/// Two block kinds change shape on the way out. A tool result becomes its own
/// `role: "tool"` message rather than staying inside the turn that carried it
/// — this format has a role for it where anthropic-messages has a block —
/// which is why the walk is per *block* and flushes in block order rather
/// than per message. And `is_error` has nowhere to go: chat-completions gives
/// a tool message a `content` and nothing else, so a failure reaches the
/// model as whatever the tool said about it. That is this format's own
/// convention, not a loss invented here.
fn wire_messages(messages: &[Message]) -> Vec<WireMessage<'_>> {
    let mut wire = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        // The turn's own message, built up until a tool result interrupts it.
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in &message.content {
            match block {
                // Unreachable in practice: `request_body` refuses a history
                // carrying one before this runs, because this schema has
                // nowhere to put it. Dropped rather than refused a second
                // time so that the refusal stays in one place.
                ContentBlock::Reasoning { .. } => {}
                ContentBlock::Text { text: chunk } => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(chunk);
                }
                ContentBlock::ToolUse {
                    call_id,
                    name,
                    args,
                } => tool_calls.push(WireToolCall {
                    id: call_id,
                    kind: "function",
                    function: WireCallFunction {
                        name,
                        arguments: args,
                    },
                }),
                ContentBlock::ToolResult {
                    call_id, output, ..
                } => {
                    flush(&mut wire, role, &mut text, &mut tool_calls);
                    wire.push(WireMessage {
                        role: "tool",
                        content: Some(output.clone()),
                        tool_calls: Vec::new(),
                        tool_call_id: Some(call_id),
                    });
                }
            }
        }
        flush(&mut wire, role, &mut text, &mut tool_calls);
    }
    wire
}

/// Emit whatever has accumulated for the current turn, if anything has.
///
/// `content` is omitted only when the turn is nothing but tool calls: the
/// schema permits that for an assistant turn and requires content otherwise,
/// and an empty string is a message that says nothing rather than one that is
/// not there.
fn flush<'a>(
    wire: &mut Vec<WireMessage<'a>>,
    role: &'static str,
    text: &mut String,
    tool_calls: &mut Vec<WireToolCall<'a>>,
) {
    if text.is_empty() && tool_calls.is_empty() {
        return;
    }
    let content = (!text.is_empty() || tool_calls.is_empty()).then(|| std::mem::take(text));
    text.clear();
    wire.push(WireMessage {
        role,
        content,
        tool_calls: std::mem::take(tool_calls),
        tool_call_id: None,
    });
}

/// The exact JSON bytes this format would send for `req`.
///
/// Separate from [`build_request`] so the golden can pin the body without a
/// URL or a credential in it — the body is the contract with the provider;
/// the endpoint and the auth header are the provider row's business.
pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    let wire = WireRequest {
        common: common_request(req)?,
        max_completion_tokens: req.max_output_tokens,
    };
    Ok(serde_json::to_vec(&wire).expect("WireRequest has no non-string map keys and no NaN"))
}

/// The captured Pi/Hermes client projection for compatible endpoints.
///
/// Copilot, Qwen Portal, and Nous Portal agree on two omissions: they do not
/// send a reasoning field when thinking is off, and their pinned clients do
/// not send `parallel_tool_calls`. Keeping those omissions in one helper pins
/// the shared bytes without claiming that any provider was recorded live.
/// Their mapping values still own the request differences that do not agree.
pub(crate) fn captured_client_request_body(req: &GenRequest) -> Result<Vec<u8>, Inexpressible> {
    let bytes = request_body(req)?;
    let mut body: serde_json::Value =
        serde_json::from_slice(&bytes).expect("WireRequest serialises JSON");
    let object = body
        .as_object_mut()
        .expect("WireRequest serialises an object");
    if req.thinking == ThinkingLevel::Off {
        object.remove("reasoning_effort");
    }
    object.remove("parallel_tool_calls");
    Ok(serde_json::to_vec(&body).expect("a JSON value serialises"))
}

/// The part of the body every member of this family shares.
///
/// The refusal lives here rather than in either caller: a request carrying
/// reasoning state is unexpressible on this *shape*, and both members send the
/// shape. `pub(crate)` for [`super::ollama`], which supplies the other
/// ceiling field and nothing else.
pub(crate) fn common_request(req: &GenRequest) -> Result<CommonRequest<'_>, Inexpressible> {
    let carries_reasoning = req
        .messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Reasoning { .. }));
    if carries_reasoning {
        return Err(Inexpressible::ReasoningStateNotCarriable);
    }
    let messages = wire_messages(req.messages.messages());
    let tools: Vec<WireTool<'_>> = req
        .tools
        .iter()
        .map(|tool| WireTool {
            kind: "function",
            function: WireFunction {
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.parameters,
            },
        })
        .collect();
    let parallel_tool_calls = (!tools.is_empty()).then_some(req.parallel_tool_calls);
    Ok(CommonRequest {
        model: &req.model,
        messages,
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        reasoning_effort: reasoning_effort(req.thinking),
        tools,
        parallel_tool_calls,
    })
}

/// A complete HTTP request for `req` against `target`.
///
/// The target's headers come from the provider row — the auth header above all
/// — because credentials are not this module's to know
/// (`ARCHITECTURE.md` §3.1), and its base is joined with [`PATH`] here rather
/// than carried whole by the row, because a path is a fact about the API shape
/// and the row is not allowed to know one.
pub fn build_request(
    target: &RequestTarget<'_>,
    req: &GenRequest,
) -> Result<HttpRequest, Inexpressible> {
    Ok(target.post_json(PATH, request_body(req)?))
}

// --- response ---

#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    /// Kept as unparsed bytes rather than deserialised in place.
    ///
    /// Two readers want this object and they want different things from it:
    /// [`map_usage`] wants the four token counts, and the row's
    /// [`ProviderCost`] decoder wants whichever money field *that provider*
    /// declares. Parsing it here would decide for both — and would put an
    /// OpenRouter cost through an `f64` on the way, which is the one thing
    /// `ego-core`'s money contract exists to prevent.
    #[serde(default)]
    usage: Option<Box<RawValue>>,
    /// Some compatible providers report a post-200 failure as an SSE data
    /// object with the ordinary OpenAI error envelope.
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    message: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    /// Reasoning text, spelling one. Ollama sends this
    /// (`tests/goldens/ollama_reasoning_high.sse`, measured), and so does
    /// OpenRouter.
    ///
    /// Not in `openai/openai-openapi`, and read anyway: the shared-shape ADR
    /// §4 rules that an extension field is decoded opportunistically and
    /// uniformly for every member of the family, because the *field name* is
    /// the contract and the vendor is not. A member that never sends it
    /// leaves it absent, which decodes to `None` and changes nothing — so
    /// there is no branch here, and nothing to consult a provider id about.
    #[serde(default)]
    reasoning: Option<String>,
    /// Reasoning text, spelling two. xAI sends this one
    /// (`POTENTIALS.md` P24, from xAI's own schema).
    ///
    /// Two fields and not one aliased field: `#[serde(alias)]` would decode
    /// either name into one slot and silently keep the last one written if a
    /// provider ever sent both. These are two members of an object, and the
    /// object is allowed to carry both; reading them as two and concatenating
    /// in order is the reading that cannot lose text.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallChunk>,
}

#[derive(Debug, Deserialize)]
struct ToolCallChunk {
    /// The only field the schema marks required, and the only one that
    /// identifies which call a fragment belongs to.
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionChunk>,
}

#[derive(Debug, Deserialize)]
struct FunctionChunk {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// Absent on rows that publish no reasoning breakdown, which is why the
    /// `Option` survives all the way to
    /// [`Usage::reasoning_tokens`](ego_core::state::Usage::reasoning_tokens)
    /// rather than being flattened to a zero here.
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

/// The four-way split, or nothing.
///
/// A daemon that reports no `prompt_tokens_details` at all is not hiding a
/// cache: it is saying every prompt token was billed at the full rate, which
/// is `uncached_input_tokens = prompt_tokens` and two zeros. Discarding a
/// perfectly good `prompt_tokens`/`completion_tokens` pair over a missing
/// optional object would send every generation against every local daemon —
/// the majority of what speaks this shape — to settle at its pessimistic
/// bound, narrowing the budget on every turn for no reason.
///
/// A breakdown that does not *fit*, though, is a different thing: the schema
/// makes the three a partition of `prompt_tokens`, so `cached + written >
/// prompt` is a payload no conforming provider produces, and there is no way
/// to tell which of the three numbers is the wrong one. Returning `None`
/// leaves the attempt with
/// [`UsageReport::Missing`](ego_core::generator::attempt::UsageReport::Missing)
/// and settles it at the pessimistic bound — the failure direction that costs
/// budget rather than the one that leaks it. Clamping instead would under-
/// report the uncached tokens, which are the dearest ones, and the repaired
/// number would be indistinguishable from a real one.
fn map_usage(usage: &WireUsage) -> Option<Usage> {
    let details = usage.prompt_tokens_details.as_ref();
    let cache_read_tokens = details.map_or(0, |d| d.cached_tokens);
    let cache_write_tokens = details.map_or(0, |d| d.cache_write_tokens);
    let breakdown = cache_read_tokens.saturating_add(cache_write_tokens);
    let uncached_input_tokens = usage.prompt_tokens.checked_sub(breakdown)?;
    Some(Usage {
        uncached_input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        // A measured zero, not an absent figure. This wire's schema
        // partitions `prompt_tokens` into cached and cache-write and has no
        // extended-retention category at all, so every provider speaking it
        // wrote nothing at an extended retention.
        cache_write_extended_tokens: 0,
        output_tokens: usage.completion_tokens,
        // Read where it is published and left unknown where it is not — the
        // same opportunistic, uniform decode the sibling ADR §4 rules for
        // every extension field, with no provider id anywhere near it.
        // OpenRouter and xAI fill this in; Ollama sends no
        // `completion_tokens_details` at all, and `None` says so rather than
        // claiming it reasoned for nothing.
        reasoning_tokens: usage
            .completion_tokens_details
            .as_ref()
            .map(|details| details.reasoning_tokens),
    })
}

/// Records `incoming` as an identity, or notes that it disagrees with the one
/// already held.
fn settle_identity(
    held: &mut Option<String>,
    incoming: Option<&str>,
    what: &str,
    index: u32,
    conflict: &mut Option<String>,
) {
    let Some(incoming) = incoming else { return };
    match held.as_deref() {
        Some(existing) if existing != incoming => {
            *conflict = Some(format!(
                "two {what} ({existing} and {incoming}) at tool call index {index}"
            ));
        }
        _ => *held = Some(incoming.to_string()),
    }
}

/// One tool call, being assembled from the fragments it arrives in.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    args: String,
    /// Set when two fragments at this index disagree about which call they
    /// belong to. A poisoned slot is reported and dropped rather than
    /// dispatched: whether the provider meant to correct an id or to start a
    /// second call cannot be told from the wire, and a tool call is not
    /// something to run on a guess.
    conflict: Option<String>,
}

/// Why a stream ended without the model saying why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Incomplete {
    /// Bytes stopped in the middle of an event.
    CutMidFrame,
    /// Bytes stopped cleanly between events.
    EndedBetweenFrames,
    /// The provider said `[DONE]` having never sent a `finish_reason`.
    DoneWithoutFinishReason,
}

/// Turns chunks of `text/event-stream` bytes into [`AiEvent`]s.
///
/// Separate from the byte stream that feeds it, and pure, so a transcript can
/// be replayed at any chunk boundary — which is how the goldens prove that a
/// cut mid-frame is handled rather than merely survived.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    /// How this provider states the money it will charge, when it states it.
    ///
    /// Carried by the provider row, not chosen here: this format is spoken by
    /// OpenAI, OpenRouter, the unselectable API-key xAI candidate, Groq and
    /// local daemons, and only some report a cost. A `match` on the provider id
    /// in this file would be exactly the special case the metering ADR §8
    /// forbids.
    cost: Option<Arc<dyn ProviderCost>>,
    /// The last figure the cost decoder read, held rather than forwarded, for
    /// the same reason `usage` below is: a stated cost is a total for the
    /// request, and a gateway that restates it must not be billed the sum.
    stated_cost: Option<Money>,
    sse: SseDecoder,
    /// Ordered by index so a flush is deterministic regardless of the order
    /// the provider interleaved the fragments in.
    calls: BTreeMap<u32, PartialToolCall>,
    finish_reason: Option<StopReason>,
    saw_done: bool,
    /// The last usage object the provider sent, held rather than forwarded.
    ///
    /// [`AiEvent::Usage`] is an *increment*: `AttemptStream` adds every one it
    /// sees into a running total. This wire's usage chunk is a *total for the
    /// whole request*. One of those is safe to emit as it arrives and the
    /// other is not — a gateway that restates the cumulative figure would be
    /// billed for the sum of its restatements. So exactly one is emitted, at
    /// the end, and a second report replaces the first instead of adding to
    /// it.
    usage: Option<Usage>,
    /// A usage object whose parts did not fit its total. Held so it can be
    /// reported once, at the end, beside everything else that went wrong.
    usage_incoherent: bool,
    /// An explicit provider error carried after the response status was
    /// already 200. It outranks any finish reason in the same stream.
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
    /// A decoder for a provider that states what it will charge.
    ///
    /// [`Default`] is the decoder for one that does not, which is most of
    /// them — and the difference has to be configured rather than inferred:
    /// a row that expects a cost and never receives one settles at its bound
    /// and says so, which is only correct if something actually declared the
    /// expectation.
    pub fn reporting_cost(cost: Arc<dyn ProviderCost>) -> Self {
        Self {
            cost: Some(cost),
            ..Self::default()
        }
    }

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
    /// `transport` is whatever killed the byte stream, if anything, so that
    /// its [`DeliveryCertainty`] reaches the attempt — the reading only
    /// [`HttpError`] is in a position to take.
    pub fn finish(mut self, transport: Option<HttpError>) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if let Some(error) = &transport {
            out.push(AiEvent::DeliveryObserved(error.delivery()));
        }
        if self.usage_incoherent {
            // `Unclassified` on purpose, and it is the honest answer rather
            // than an unconsidered default. The generation itself did not
            // fail — it completes and settles at its bound — so `Transient`
            // would invite a retry of a turn that succeeded, and neither the
            // request nor the account is what went wrong. None of the other
            // three describes "the provider contradicted itself about a
            // number", which is precisely what `Unclassified` is for.
            out.push(AiEvent::Error {
                message: "the provider's prompt cache breakdown was larger than \
                          the prompt it broke down, so no usage figure could be \
                          trusted; this generation settles at its bound"
                    .to_string(),
                kind: ErrorKind::Unclassified,
            });
        }
        if let Some((message, kind)) = self.provider_error.take() {
            self.calls.clear();
            out.push(AiEvent::Error { message, kind });
            out.push(AiEvent::Stop(StopReason::Error));
            return out;
        }
        // Before the usage frame, deliberately. The two are one observation
        // as far as the log is concerned, and a reader that folds them in
        // arrival order should see the money already in hand when the tokens
        // land. Nothing downstream *depends* on the order — `AttemptStream`
        // and the turn loop both hold whichever arrives first — but an order
        // that has to be explained is worse than one that reads correctly.
        if let Some(cost) = self.stated_cost.take() {
            out.push(AiEvent::ProviderCost { cost });
        }
        if let Some(usage) = self.usage.take() {
            out.push(AiEvent::Usage(usage));
        }
        match self.finish_reason {
            // The model said why it stopped, so it stopped that way. Anything
            // that happened to the framing afterwards costs at most the usage
            // chunk, which settles pessimistically on its own.
            Some(stop) => {
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
            None => {
                // Fragments of a call the model never finished asking for are
                // not a call. Dropping them is named in the error rather than
                // done quietly.
                let abandoned = self.calls.len();
                self.calls.clear();
                // A stream that stopped without saying why. When a transport
                // failure killed it, that failure's own class is the answer.
                // When nothing killed it — the body simply ended mid-protocol
                // — `Transient` is still the honest reading: the request was
                // accepted (this arm is only reachable past a 200) and
                // partly answered, so neither the request nor the account is
                // what went wrong, and the same bytes may well complete next
                // time. This is not the fail-open the `Unclassified` default
                // guards against, because that rule is about a provider that
                // *stated* nothing about retryability; here the provider
                // stated nothing at all, which is a different fact. Nor does
                // it authorise a double-billed retry: `PossiblySent` is
                // already on the stream and `--retry` gates on both.
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
                Incomplete::DoneWithoutFinishReason => {
                    "the provider ended the stream with [DONE] and never sent a \
                     finish_reason"
                        .to_string()
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
        if self.saw_done {
            Incomplete::DoneWithoutFinishReason
        } else if self.sse.has_partial_frame() {
            Incomplete::CutMidFrame
        } else {
            Incomplete::EndedBetweenFrames
        }
    }

    fn consume(&mut self, frame: &str, out: &mut Vec<AiEvent>) {
        if frame == "[DONE]" {
            self.saw_done = true;
            return;
        }
        let chunk: Chunk = match serde_json::from_str(frame) {
            Ok(chunk) => chunk,
            Err(error) => {
                // Not fatal on its own: a proxy that injects a frame this
                // format does not know has not necessarily broken the answer,
                // and a missing `finish_reason` is caught at the end anyway.
                tracing::warn!(%error, "skipping an event this wire format cannot decode");
                return;
            }
        };
        if let Some(error) = chunk.error {
            let kind = match error.kind.as_deref() {
                Some("authentication_error" | "permission_error" | "billing_error") => {
                    ErrorKind::Refused
                }
                Some("invalid_request_error" | "not_found_error") => ErrorKind::Invalid,
                Some("rate_limit_error" | "overloaded_error" | "api_error") => ErrorKind::Transient,
                _ => ErrorKind::Unclassified,
            };
            self.provider_error = Some((error.message, kind));
            return;
        }
        for choice in &chunk.choices {
            if choice.index != 0 {
                // `n` is never sent, so a second choice is a completion nobody
                // asked for. Folding it into the first would interleave two
                // answers into one.
                tracing::warn!(
                    index = choice.index,
                    "ignoring a completion choice beyond the one requested"
                );
                continue;
            }
            self.consume_delta(&choice.delta, out);
            if let Some(reason) = &choice.finish_reason {
                self.consume_finish_reason(reason, out);
            }
        }
        if let Some(raw) = &chunk.usage {
            self.consume_usage(raw);
        }
    }

    /// The `usage` object, read twice: once for the token counts, and once
    /// for whatever money field this provider declares.
    ///
    /// The two readings are independent on purpose. A provider that reports a
    /// cost and an incoherent token breakdown still stated the cost, and
    /// under a provider-reported settlement that figure is the one that
    /// settles the generation — discarding it because the *tokens* did not
    /// add up would throw away the better number over the worse one.
    fn consume_usage(&mut self, raw: &RawValue) {
        if let Some(cost) = self.cost.as_ref().and_then(|decoder| decoder.read(raw)) {
            self.stated_cost = Some(cost);
        }
        let Ok(usage) = serde_json::from_str::<WireUsage>(raw.get()) else {
            // Not the incoherent-breakdown case below: this is a `usage` that
            // is not a usage object at all, which is the same kind of thing
            // as a frame this format cannot decode.
            tracing::warn!("skipping a usage object this wire format cannot decode");
            return;
        };
        match map_usage(&usage) {
            Some(mapped) => self.usage = Some(mapped),
            None => {
                tracing::warn!(
                    prompt_tokens = usage.prompt_tokens,
                    "the reported prompt cache breakdown exceeds the prompt; \
                     discarding the whole usage report rather than repairing it"
                );
                self.usage = None;
                self.usage_incoherent = true;
            }
        }
    }

    fn consume_delta(&mut self, delta: &Delta, out: &mut Vec<AiEvent>) {
        // Before the visible text, because that is the order a chunk carrying
        // both means: the model reasoned and then answered. Both spellings,
        // in a fixed order, because a provider sending both is a provider
        // whose text would otherwise be interleaved by whichever `if` came
        // first — and neither of the two that send one has ever sent the
        // other.
        for text in [&delta.reasoning, &delta.reasoning_content]
            .into_iter()
            .flatten()
            .filter(|text| !text.is_empty())
        {
            out.push(AiEvent::ReasoningDelta { text: text.clone() });
        }
        if let Some(text) = &delta.content
            && !text.is_empty()
        {
            out.push(AiEvent::TextDelta { text: text.clone() });
        }
        // A refusal is the assistant's visible answer, arriving on a second
        // channel: `finish_reason` is still `stop`, and the turn succeeded.
        // Reported as text because that is what it is — dropping it would
        // leave the user with an empty, successful turn.
        if let Some(text) = &delta.refusal
            && !text.is_empty()
        {
            out.push(AiEvent::TextDelta { text: text.clone() });
        }
        for fragment in &delta.tool_calls {
            self.consume_tool_fragment(fragment);
        }
    }

    fn consume_tool_fragment(&mut self, fragment: &ToolCallChunk) {
        let index = fragment.index;
        let call = self.calls.entry(index).or_default();
        // `id` and `name` identify the call; `arguments` is its payload. Only
        // the payload accumulates. Repeating an identity is how a provider
        // that restates its metadata looks, and appending it would produce
        // `call_acall_a`; *changing* one is a disagreement about which call
        // the arguments so far belong to, and neither reading may be run.
        settle_identity(
            &mut call.id,
            fragment.id.as_deref(),
            "ids",
            index,
            &mut call.conflict,
        );
        if let Some(function) = &fragment.function {
            settle_identity(
                &mut call.name,
                function.name.as_deref(),
                "names",
                index,
                &mut call.conflict,
            );
            if let Some(args) = &function.arguments {
                call.args.push_str(args);
            }
        }
    }

    fn consume_finish_reason(&mut self, reason: &str, out: &mut Vec<AiEvent>) {
        // Flushed here rather than at the end so tool calls precede the usage
        // chunk that follows this one — and because this is the point at which
        // the arguments are known to be whole.
        for (index, call) in std::mem::take(&mut self.calls) {
            Self::emit_call(index, call, out);
        }
        if self.finish_reason.is_some() {
            tracing::warn!(reason, "ignoring a second finish_reason");
            return;
        }
        let stop = match reason {
            "stop" => StopReason::Stop,
            "length" => StopReason::Length,
            "tool_calls" => StopReason::ToolUse,
            // Deprecated in the schema, still emitted by older gateways, and
            // the same event: the model stopped in order to call something.
            "function_call" => StopReason::ToolUse,
            // Anthropic's spelling of `stop`, reaching this wire because a
            // gateway passes its upstream's reason through untranslated
            // rather than mapping it (survey row 45; xAI is the measured
            // case). It is not this format's vocabulary and it arrives in
            // this format's stream anyway, which is the whole reason the
            // table is widened here rather than made conditional on a
            // provider id — a `match` on the row inside a wire format is what
            // the sibling ADR §5's name-absence test forbids.
            //
            // Widening is safe in the direction that matters: `end_turn` has
            // exactly one meaning wherever it is minted, so no row that does
            // *not* emit it can be harmed by an arm it never reaches.
            "end_turn" => StopReason::Stop,
            "content_filter" => {
                out.push(AiEvent::Error {
                    message: "the provider's content filter stopped the generation".to_string(),
                    // Refused and not `Invalid`: the request was well formed
                    // and the provider declined to answer it. No retry of the
                    // same request changes that either way, but the two are
                    // different facts and only one of them blames the schema.
                    kind: ErrorKind::Refused,
                });
                StopReason::Error
            }
            // Load-bearing, and it fails closed: an unrecognised reason ends
            // the turn as an error that names the string, rather than being
            // read as a clean stop. That is the right default for a wire
            // spoken by six implementations with no shared enum — a reason
            // nobody mapped is not a reason that said "finished".
            //
            // What this arm does *not* catch is the empty string, and the
            // distinction is worth stating because it is the opposite
            // failure. Ollama's `DoneReasonConnectionClosed` renders as `""`
            // through `String()`, and its compat layer emits
            // `cmp.Or(r.DoneReason, "stop")` — so a connection that died is
            // laundered into a clean `"stop"` before it is ever sent, and
            // never reaches this arm at all. That one **fails open**: a torn
            // generation reports as a completed one.
            //
            // It is latent, not live. `DoneReasonConnectionClosed` has no
            // assignment in Ollama's tree, so nothing produces it today, and
            // handling it here would mean writing a branch for a string that
            // cannot arrive — which is worse than recording it. The trigger
            // is the first assignment of that constant upstream; the fix is
            // upstream's too, because by the time the bytes reach ego the
            // evidence has already been erased.
            other => {
                out.push(AiEvent::Error {
                    message: format!(
                        "the provider reported a finish_reason this wire format \
                         does not know: {other}"
                    ),
                    kind: ErrorKind::Unclassified,
                });
                StopReason::Error
            }
        };
        self.finish_reason = Some(stop);
    }

    fn emit_call(index: u32, call: PartialToolCall, out: &mut Vec<AiEvent>) {
        if let Some(conflict) = call.conflict {
            out.push(AiEvent::Error {
                message: format!(
                    "the provider streamed {conflict}, so which call the \
                     arguments belong to cannot be told; it was not dispatched"
                ),
                kind: ErrorKind::Unclassified,
            });
            return;
        }
        // Both of these are unanswerable rather than merely incomplete. A
        // result message needs the `tool_call_id` to send back, and a dispatch
        // needs a name; inventing either produces a request the provider
        // rejects, or a call to the wrong tool.
        //
        // `""` is treated as absent, and this guard is fail-closed rather
        // than an accommodation for a row that needs it. No implementation
        // speaking this wire is known to emit an empty id — Ollama's compat
        // layer, the row the survey suspected, overwrites the field
        // unconditionally and so cannot (measured, `server/routes.go`;
        // the survey's original claim was retracted). The guard exists
        // because an empty id is *unusable* if one ever arrives: it would be
        // carried into `ContentBlock::ToolUse` as a legitimate key, where
        // `Conversation`'s uniqueness invariant would then read two
        // unidentified calls as one duplicate. Refusing it costs a call that
        // could not have been answered anyway.
        let Some(call_id) = call.id.filter(|id| !id.is_empty()) else {
            out.push(AiEvent::Error {
                message: format!(
                    "the provider streamed a tool call at index {index} with no id, \
                     so its result could never be attributed back to it"
                ),
                kind: ErrorKind::Unclassified,
            });
            return;
        };
        let Some(name) = call.name.filter(|name| !name.is_empty()) else {
            out.push(AiEvent::Error {
                message: format!("tool call {call_id} arrived with no function name"),
                kind: ErrorKind::Unclassified,
            });
            return;
        };
        // `args` is passed through exactly as assembled, including when it is
        // not valid JSON. The schema warns that "the model does not always
        // generate valid JSON", the tool owns the schema that would judge it,
        // and a truncated argument string is already explained by the
        // `Length` stop reason that accompanies it.
        out.push(AiEvent::ToolCall {
            call_id,
            name,
            args: call.args,
        });
    }
}

/// Decode one provider response into the generation's events.
///
/// The state machine is [`super::decode_response`]'s, shared with every other
/// streaming mapping: what a *response* means is `AttemptStream`'s contract
/// and not this format's, and only what a *frame* means belongs here.
///
/// `cost` is the provider row's own declaration of where it states a charge,
/// carried through rather than decided here — see [`StreamDecoder::cost`].
pub fn decode(
    response: Result<HttpResponse, HttpError>,
    cost: Option<Arc<dyn ProviderCost>>,
) -> BoxStream<'static, AiEvent> {
    let decoder = match cost {
        Some(cost) => StreamDecoder::reporting_cost(cost),
        None => StreamDecoder::default(),
    };
    super::decode_response(response, Box::new(decoder))
}
