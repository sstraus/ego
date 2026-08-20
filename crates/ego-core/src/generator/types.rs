//! The provider-neutral conversation model: `GenRequest`, `Message`,
//! `ToolDescriptor`, `StopReason`, `ThinkingLevel`, `ReasoningState`.
//!
//! See `ARCHITECTURE.md` §3.1 and `docs/02-pi.md` §2 (the pi taxonomy this
//! borrows from).

use serde::{Deserialize, Serialize};

/// Reasoning effort, pi's scale (`docs/02-pi.md` §2) plus `Off`: pi actually
/// carries two related types (`ThinkingLevel` without `off`, and
/// `ModelThinkingLevel` adding it for per-model defaults). A `GenRequest`
/// needs to be able to say "no thinking" as well as an effort level, and the
/// plan asks for one `ThinkingLevel` type, so this merges the two rather than
/// introducing a second type the acceptance criteria doesn't ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

/// Why a generation stopped, pi's scale (`docs/02-pi.md` §2) minus `Pending`.
///
/// The one deliberate deviation from pi, decided in
/// `docs/decisions/2026-08-21-attempt-accounting-ledger.md`: pi's `Pending`
/// means "no terminal state yet", which is the *absence* of the answer this
/// type exists to give. Every variant here is terminal, so an
/// [`Outcome`](super::attempt::Outcome) cannot be built around a value that
/// says nothing ended — and it can be durable without a guard, because a
/// decoded `StopReason` has no non-terminal spelling to decode into.
///
/// "Not finished yet" is expressed by not having a `StopReason`: the stream
/// is still running, or `AttemptStream::terminal` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// The model completed its turn normally.
    Stop,
    /// The output was truncated at a length limit.
    Length,
    /// The model stopped to call a tool.
    ToolUse,
    /// The provider reported an error.
    Error,
    /// The generation was cancelled.
    Aborted,
    /// The provider deferred delivery of the result (fetched later, not
    /// streamed) — `docs/02-pi.md` §2's `DeferredHandle`.
    Deferred,
}

/// Who authored a `Message`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One turn of conversation history sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// The tool call this message answers. Only meaningful for `Role::Tool`
    /// messages — without it a tool result can't be matched back to the call
    /// that produced it.
    pub tool_call_id: Option<String>,
}

/// A tool the model may call: name, description and JSON Schema for its
/// parameters — the same shape `Tool::descriptor` returns (ARCH §3.2), so a
/// `GenRequest` can be built directly from a tool registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Opaque, provider-private reasoning-continuation state (e.g. an encrypted
/// reasoning signature or a deferred-response handle). `ego-core` never
/// interprets the contents — only whether a target `ModelCapabilities`
/// declares it can carry this over (`capabilities::ReasoningCarryover`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningState(pub String);

/// A request to generate the next assistant turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenRequest {
    /// The model to generate with, as a provider-catalogue id (e.g.
    /// `"claude-opus-5"`). `ego-core` treats this as an opaque string; the
    /// catalogue and provider table live in `ego-ai` (ARCH §3.1).
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    pub thinking: ThinkingLevel,
    pub max_output_tokens: Option<u64>,
    pub parallel_tool_calls: bool,
    /// Reasoning state carried over from a prior generation, if any. Present
    /// only when continuing a chain of thought across generations (e.g. a
    /// fallback or a follow-up call) — see
    /// `capabilities::ModelCapabilities::fallback_gaps`.
    pub reasoning_state: Option<ReasoningState>,
}
