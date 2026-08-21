//! The provider-neutral conversation model: `GenRequest`, `Conversation`,
//! `Message`, `ContentBlock`, `ToolDescriptor`, `StopReason`, `ErrorKind`,
//! `ThinkingLevel`, `ReasoningState`.
//!
//! See `ARCHITECTURE.md` §3.1 and `docs/02-pi.md` §2 (the pi taxonomy this
//! borrows from), and `docs/decisions/2026-08-21-tool-use-history.md` for why
//! a message is a list of blocks rather than a string plus an optional id.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Reasoning effort, pi's scale (`docs/02-pi.md` §2) plus `Off`: pi actually
/// carries two related types (`ThinkingLevel` without `off`, and
/// `ModelThinkingLevel` adding it for per-model defaults). A `GenRequest`
/// needs to be able to say "no thinking" as well as an effort level, and the
/// plan asks for one `ThinkingLevel` type, so this merges the two rather than
/// introducing a second type the acceptance criteria doesn't ask for.
/// The ordering is the effort ordering, not an accident of declaration: it is
/// derived, and the variants are written from least to most so that deriving
/// it says something true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

/// Why a provider error happened, in the only terms a caller can act on.
///
/// Four values, each with a distinct action. Not the twenty-four of
/// OpenRouter's `error_type`: that is a fine vocabulary and it is
/// OpenRouter's, and six of its values describe image generation this
/// repository does not do. The adapter maps its provider's spelling onto
/// these; the mapping is data, and the branch stays in core.
///
/// Carrying the provider's own code as a string beside the message was
/// rejected for the reason that a string only helps if something matches on
/// it, and the only thing that would is `turn/` — matching on a provider's
/// spelling in `turn/` being exactly what
/// `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §5's
/// name-absence test exists to forbid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Momentarily unable: overload, rate limit, timeout, upstream
    /// unavailability. A retry may succeed.
    Transient,
    /// The request is wrong and will be wrong again: schema, context length,
    /// an unsupported parameter. No retry changes it.
    Invalid,
    /// The account or the credential is refused: unauthenticated, forbidden,
    /// out of credit. No retry changes it, and the request was not the
    /// problem.
    Refused,
    /// The provider said something this mapping does not classify. Treated as
    /// non-retryable.
    ///
    /// The default, and it fails closed on the precedent
    /// [`ReasoningEffortSupport::Unknown`](super::capabilities::ReasoningEffortSupport::Unknown)
    /// set: a provider that stated nothing is not a provider that stated
    /// "retryable". Failing closed costs one attempt; failing open costs
    /// `--retry N` attempts and a wrong exit code.
    #[default]
    Unclassified,
}

/// Who authored a `Message`.
///
/// Three roles, not four: a tool *result* is a [`ContentBlock`] inside the
/// turn that carries it, not an author. The providers disagree about which
/// role that is — chat-completions gives a result its own `role: "tool"`
/// message, anthropic-messages puts it in a `user` message — so a `Tool` role
/// here would be one of them borrowed and imposed on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// One piece of a message's content.
///
/// Ordered, because order is part of what makes a request valid: Anthropic
/// requires every `tool_result` of a turn to be grouped ahead of any text in
/// it, and requires a turn's thinking to precede its `tool_use`. A shape that
/// kept calls in a field beside the text would throw that ordering away and
/// leave each adapter to invent it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Visible text.
    Text { text: String },
    /// Reasoning the model produced, kept only so it can be handed back.
    ///
    /// Carries no text: what a human reads rides `AiEvent::ReasoningDelta`
    /// and the journal, and the two are not the same value — a block sent
    /// with `display: "omitted"`, and every `redacted_thinking` block, has
    /// state and no readable text at all.
    ///
    /// A block rather than a field beside the content for the reason this
    /// type's own doc gives one line up: reasoning state is *positional*.
    /// Anthropic requires a turn's thinking to precede its `tool_use`, and
    /// documents a hard `400` when the blocks are not replayed exactly,
    /// "including blocks whose `thinking` field is empty". A second carrier
    /// beside the ordered list would be two places for one fact.
    Reasoning { state: ReasoningState },
    /// The assistant asked for a tool to be run.
    ToolUse {
        /// The correlation token **of the protocol that produced this call**.
        ///
        /// Opaque and unguessable where the provider issues one, because the
        /// provider validates the id it issued — so a locally minted
        /// substitute is a rejected request. Where the provider issues none
        /// (Ollama's `api.ToolCall.ID` is `json:"id,omitempty"` and is
        /// populated by per-model parsers; Gemini's legacy `FunctionCall.id`
        /// is optional), the mapping mints one, and may do so **only because
        /// it is also the code that will send it back**.
        ///
        /// Both of those are properties of a protocol, not of this field, and
        /// stating one of them here as though it were the port's was the
        /// original error. What the port requires is only that the token
        /// exists: it is the key
        /// [`Conversation`]'s three relational invariants are defined over.
        call_id: String,
        name: String,
        /// The arguments, JSON or not.
        ///
        /// A `String` and not a `serde_json::Value` on purpose: a tool call
        /// truncated mid-stream arrives as invalid JSON, and that is
        /// *evidence about the stream*. Parsing here would discard it at the
        /// one seam that can still see it.
        ///
        /// "The verbatim bytes the model produced" is a property of protocols
        /// that transmit **string fragments**, not of this field — Gemini
        /// sends `functionCall.args` as a JSON object, and its mapping
        /// serialises it, which loses the original textual representation and
        /// not the value. Same correction as `call_id` above, same original
        /// mistake: one protocol's guarantee documented as the port's.
        args: String,
    },
    /// A tool's answer to one call.
    ToolResult {
        /// The `ToolUse::call_id` this answers.
        call_id: String,
        output: String,
        /// Whether the tool failed. Not the same as "no answer": a failure
        /// still answers the call, and a call that was never executed at all
        /// must still be answered this way — Anthropic requires a
        /// `tool_result` "marked with `is_error` set to true" for it.
        is_error: bool,
    },
}

/// One turn of conversation history sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A message whose whole content is one block of text — every system
    /// prompt, every user turn that is not answering a tool.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

/// Why a list of messages is not a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryError {
    /// A result names a call that no earlier message made.
    ResultWithoutCall { call_id: String },
    /// Two results name one call.
    CallAnsweredTwice { call_id: String },
    /// Two calls share one id, so no result can say which it answers.
    DuplicateCallId { call_id: String },
    /// A call was made and never answered. Every provider rejects this.
    CallUnanswered { call_id: String },
    /// A call appears in a turn the assistant did not author. Only the model
    /// asks for a tool to run, on every wire there is.
    CallFromNonAssistant { call_id: String },
    /// A result appears in an assistant turn. The assistant asks; something
    /// else answers — chat-completions gives the answer its own `tool` role
    /// and anthropic-messages puts it in a `user` turn, and neither lets the
    /// assistant answer itself.
    ResultFromAssistant { call_id: String },
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::ResultWithoutCall { call_id } => {
                write!(f, "a result answers {call_id}, which nothing called")
            }
            HistoryError::CallAnsweredTwice { call_id } => {
                write!(f, "{call_id} was answered twice")
            }
            HistoryError::DuplicateCallId { call_id } => {
                write!(f, "two calls share the id {call_id}")
            }
            HistoryError::CallUnanswered { call_id } => {
                write!(f, "{call_id} was called and never answered")
            }
            HistoryError::CallFromNonAssistant { call_id } => {
                write!(
                    f,
                    "{call_id} was called by a turn the assistant did not author"
                )
            }
            HistoryError::ResultFromAssistant { call_id } => {
                write!(f, "the assistant cannot answer its own call {call_id}")
            }
        }
    }
}

impl std::error::Error for HistoryError {}

/// A message history that every wire format can express.
///
/// Holds the three relational invariants the providers agree on — a result
/// answers a call that came earlier, exactly once, and no call is left
/// hanging — and nothing else. Rules only one provider has (Anthropic's
/// grouping and block ordering, chat-completions' encodability) stay in the
/// wire formats, which keep refusing defensively: the two disagree about what
/// is valid, so a check here would have to pick one and be wrong for the
/// other.
///
/// It exists as a type rather than a convention because the invalid history
/// is not hypothetical. A session killed mid-turn resumes holding a call with
/// no result; with a bare `Vec` the loop can build that and learn about it
/// from a provider's 400, and with this it has to decide first — which is
/// what [`crate::generator::history`] does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Message>", into = "Vec<Message>")]
pub struct Conversation(Vec<Message>);

impl Conversation {
    /// Check `messages` and take ownership of them, or say what is wrong.
    pub fn new(messages: Vec<Message>) -> Result<Self, HistoryError> {
        // `false` until the call has been answered. A `BTreeMap` and not a
        // `HashMap`: the unanswered-call error names *a* call, and which one
        // it names should not depend on hash order.
        let mut calls: BTreeMap<&str, bool> = BTreeMap::new();
        let blocks = messages.iter().flat_map(|message| {
            message
                .content
                .iter()
                .map(move |block| (message.role, block))
        });
        for (role, block) in blocks {
            match block {
                // Neither takes part in the call/result relation these
                // invariants are about. Reasoning is ordered, and the block
                // list is what carries that; nothing here needs to know it.
                ContentBlock::Text { .. } | ContentBlock::Reasoning { .. } => {}
                ContentBlock::ToolUse { call_id, .. } => {
                    if role != Role::Assistant {
                        return Err(HistoryError::CallFromNonAssistant {
                            call_id: call_id.clone(),
                        });
                    }
                    if calls.insert(call_id, false).is_some() {
                        return Err(HistoryError::DuplicateCallId {
                            call_id: call_id.clone(),
                        });
                    }
                }
                ContentBlock::ToolResult { call_id, .. } if role == Role::Assistant => {
                    return Err(HistoryError::ResultFromAssistant {
                        call_id: call_id.clone(),
                    });
                }
                ContentBlock::ToolResult { call_id, .. } => match calls.get_mut(call_id.as_str()) {
                    // Walking in order is what makes this also catch a result
                    // that precedes its call: the call is not in the map yet.
                    None => {
                        return Err(HistoryError::ResultWithoutCall {
                            call_id: call_id.clone(),
                        });
                    }
                    Some(true) => {
                        return Err(HistoryError::CallAnsweredTwice {
                            call_id: call_id.clone(),
                        });
                    }
                    Some(answered) => *answered = true,
                },
            }
        }
        if let Some((call_id, _)) = calls.iter().find(|(_, answered)| !**answered) {
            return Err(HistoryError::CallUnanswered {
                call_id: (*call_id).to_string(),
            });
        }
        Ok(Conversation(messages))
    }

    /// The messages, in order.
    pub fn messages(&self) -> &[Message] {
        &self.0
    }
}

impl TryFrom<Vec<Message>> for Conversation {
    type Error = HistoryError;

    fn try_from(messages: Vec<Message>) -> Result<Self, Self::Error> {
        Conversation::new(messages)
    }
}

impl From<Conversation> for Vec<Message> {
    fn from(conversation: Conversation) -> Self {
        conversation.0
    }
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
/// reasoning signature). `ego-core` never interprets the contents — only
/// whether a target `ModelCapabilities` declares it can carry this over
/// (`capabilities::ReasoningCarryover`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningState {
    /// Opaque, provider-private. Never parsed, never modified, never
    /// reordered — three separate documented `400`s.
    pub blob: String,
    /// Which dialect minted it, filled by the mapping that decoded it.
    ///
    /// Data, not a branch: compared for equality against the target model's
    /// own tag, never matched on. It exists because
    /// [`ReasoningCarryover::SameProviderOnly`](super::capabilities::ReasoningCarryover::SameProviderOnly)
    /// is a property of the *target* and says nothing about who minted the
    /// state — so without a tag, Anthropic state passes the check for a
    /// Gemini target and gets sent.
    pub dialect: String,
}

/// A request to generate the next assistant turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenRequest {
    /// The model to generate with, as a provider-catalogue id (e.g.
    /// `"claude-opus-5"`). `ego-core` treats this as an opaque string; the
    /// catalogue and provider table live in `ego-ai` (ARCH §3.1).
    pub model: String,
    /// The history, already checked. A `Conversation` and not a
    /// `Vec<Message>` so that an unvalidated history cannot reach a
    /// [`Generator`](super::port::Generator) at all.
    ///
    /// Reasoning state carried over from a prior generation rides here too,
    /// as [`ContentBlock::Reasoning`]. It used to be a second field beside
    /// this one — the same fact in two places, and an input nothing in the
    /// tree could produce.
    pub messages: Conversation,
    pub tools: Vec<ToolDescriptor>,
    pub thinking: ThinkingLevel,
    pub max_output_tokens: Option<u64>,
    pub parallel_tool_calls: bool,
}
