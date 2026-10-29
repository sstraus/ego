//! The `gemini-interactions` wire format, against transcripts it did not write.
//!
//! # Why this is a third mapping
//!
//! Gemini Interactions differs from both Chat Completions and Anthropic in
//! request shape and event model. These tests exercise its mapping directly.
//!
//! The structural differences, each asserted below rather than argued in prose:
//!
//! - a conversation is a list of **steps**, not messages with roles: a user
//!   turn is a `user_input` step and an assistant turn a `model_output` step;
//! - there is **no finish reason anywhere in the protocol** — the terminal
//!   signal is the interaction's `status`;
//! - reasoning is a `thought` step whose `signature` arrives as the last delta
//!   before its `step.stop`, so `AiEvent::ReasoningState` has a block boundary
//!   the wire itself draws;
//! - `thinking_level` has no `off`, so `ThinkingLevel::Off` is refused rather
//!   than mistranslated into a default that bills for reasoning nobody asked
//!   for.
//!
//! # What the goldens are, and what they are worth
//!
//! Synthetic fixtures assembled from the stable Interactions specification.
//! They pin the mapping contract, not live-server acceptance. The ignored
//! opt-in smoke test is separate evidence.
//!
//! Fields this mapping ignores are present in the transcripts on purpose
//! (`service_tier`, `object`, `created`, `input_tokens_by_modality`,
//! `interaction.status_update`, `step_usage`), so that ignoring them is tested
//! rather than assumed.

use base64::Engine as _;
use ego_ai::http::HttpResponse;
use ego_ai::wire::RequestTarget;
use ego_ai::wire::gemini_interactions::{
    DIALECT, Inexpressible, StreamDecoder, build_request, request_body,
};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, Image, ImageMediaType, Message,
    ReasoningState, Role, StopReason, ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use ego_core::state::Usage;
use url::Url;

const TEXT_TRANSCRIPT: &str = include_str!("goldens/profile/gemini/stream/text.sse");
const TOOL_TRANSCRIPT: &str = include_str!("goldens/profile/gemini/stream/tool_calls.sse");
const IMAGE_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn conversation(messages: Vec<Message>) -> Conversation {
    Conversation::new(messages).expect("the test histories here are valid")
}

/// A request with every optional field exercised at once, so a field that
/// silently stops being sent has nowhere to hide.
fn wide_request() -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "gemini-3.7-flash".to_string(),
        messages: conversation(vec![
            Message::text(Role::System, "You are terse."),
            Message::text(Role::User, "What is in ./src?"),
        ]),
        tools: vec![ToolDescriptor {
            name: "read_file".to_string(),
            description: "Read a file from the workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        }],
        thinking: ThinkingLevel::High,
        max_output_tokens: Some(4096),
        parallel_tool_calls: true,
    }
}

fn body_of(req: &GenRequest) -> serde_json::Value {
    let bytes = request_body(req).expect("this request is expressible");
    serde_json::from_slice(&bytes).expect("the body is valid JSON")
}

// --- the request ---

/// The system instruction is a top-level string, not a conversation step. Assert that no system
/// role remains in `input`.
#[test]
fn the_system_prompt_is_a_top_level_string_and_not_a_step() {
    let body = body_of(&wide_request());

    assert_eq!(
        body["system_instruction"],
        serde_json::json!("You are terse.")
    );
    let kinds: Vec<&str> = body["input"]
        .as_array()
        .expect("input is an array of steps")
        .iter()
        .map(|step| step["type"].as_str().expect("a step type is a string"))
        .collect();
    assert_eq!(
        kinds,
        vec!["user_input"],
        "the system turn must not also appear as a step"
    );
}

/// Several system turns become one instruction, in order.
#[test]
fn every_system_turn_lands_in_the_one_slot_the_api_offers() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::System, "You are terse."),
            Message::text(Role::System, "Answer in Italian."),
            Message::text(Role::User, "ok?"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        body_of(&req)["system_instruction"],
        serde_json::json!("You are terse.\nAnswer in Italian."),
    );
}

/// The system slot holds text and nothing else.
///
/// A non-text block is refused while projecting the request, before this
/// mapping can produce request bytes for a transport to observe.
#[test]
fn a_system_turn_that_is_not_text_is_refused_before_request_construction() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "go"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    call_id: "call_read".to_string(),
                    name: "read_file".to_string(),
                    args: "{}".to_string(),
                }],
            },
            Message {
                role: Role::System,
                content: vec![ContentBlock::ToolResult {
                    call_id: "call_read".to_string(),
                    output: "main.rs".to_string(),
                    is_error: false,
                }],
            },
        ]),
        ..wide_request()
    };

    let refusal = request_body(&req).expect_err("the system slot is text-only");
    assert_eq!(refusal, Inexpressible::SystemTurnIsTextOnly);
    assert_eq!(
        refusal.to_string(),
        "a system turn carried something other than text, and system_instruction is a plain string on this API"
    );
}

/// Replay a prior assistant answer as a request-side `model_output` step, not a response object
/// copied into input.
#[test]
fn a_prior_answer_replays_as_a_model_output_step() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "hello"),
            Message::text(Role::Assistant, "hi"),
            Message::text(Role::User, "again"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        body_of(&req)["input"],
        serde_json::json!([
            {"type": "user_input", "content": [{"type": "text", "text": "hello"}]},
            {"type": "model_output", "content": [{"type": "text", "text": "hi"}]},
            {"type": "user_input", "content": [{"type": "text", "text": "again"}]},
        ])
    );
}

/// Stable Interactions accepts inline image content as base64 plus its MIME
/// type. The image stays in its exact position among the user's content.
#[test]
fn an_image_is_inline_base64_content_with_its_mime_type() {
    let image = Image::new(
        base64::engine::general_purpose::STANDARD
            .decode(IMAGE_PNG)
            .expect("the fixture is base64"),
        ImageMediaType::Png,
    )
    .expect("the fixture is a valid PNG");
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "before".to_string(),
                },
                ContentBlock::Image { image },
                ContentBlock::Text {
                    text: "after".to_string(),
                },
            ],
        }]),
        ..wide_request()
    };

    let expected: serde_json::Value =
        serde_json::from_str(include_str!("goldens/profile/gemini/request/image.json"))
            .expect("the image request fixture is JSON");
    assert_eq!(body_of(&req), expected);
}

/// Replay function arguments as an object and the failed result as a `function_result` step.
/// Chat Completions uses a JSON string instead; that is not a shared wire fact.
#[test]
fn a_tool_call_and_its_result_are_two_steps_and_the_arguments_are_an_object() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "read it"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    call_id: "un6k8t18".to_string(),
                    name: "read_file".to_string(),
                    args: r#"{"path": "a.txt"}"#.to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "un6k8t18".to_string(),
                    output: "contents".to_string(),
                    is_error: true,
                }],
            },
        ]),
        ..wide_request()
    };

    assert_eq!(
        body_of(&req)["input"],
        serde_json::json!([
            {"type": "user_input", "content": [{"type": "text", "text": "read it"}]},
            {
                "type": "function_call",
                "id": "un6k8t18",
                "name": "read_file",
                "arguments": {"path": "a.txt"},
            },
            {
                "type": "function_result",
                "call_id": "un6k8t18",
                "name": "read_file",
                "result": [{"type": "text", "text": "contents"}],
                "is_error": true,
            },
        ])
    );
}

/// Arguments that are not JSON are refused, because this wire needs an object
/// and cannot be handed a string that only looks like one.
#[test]
fn tool_arguments_that_are_not_json_are_refused() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "read it"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    call_id: "un6k8t18".to_string(),
                    name: "read_file".to_string(),
                    args: "not json at all".to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "un6k8t18".to_string(),
                    output: "contents".to_string(),
                    is_error: false,
                }],
            },
        ]),
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("a non-JSON argument cannot become an object"),
        Inexpressible::ArgumentsNotJson {
            call_id: "un6k8t18".to_string()
        }
    );
}

/// Valid JSON is still not sufficient: function arguments are an object on
/// this contract, so a scalar is rejected before request construction ends.
#[test]
fn tool_arguments_that_are_not_an_object_are_refused() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "read it"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    call_id: "un6k8t18".to_string(),
                    name: "read_file".to_string(),
                    args: r#"["a.txt"]"#.to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "un6k8t18".to_string(),
                    output: "contents".to_string(),
                    is_error: false,
                }],
            },
        ]),
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("a JSON array is not an arguments object"),
        Inexpressible::ArgumentsNotObject {
            call_id: "un6k8t18".to_string()
        }
    );
}

/// Reasoning state replays as a `thought` step, byte for byte.
///
/// Google's wording is as hard as Anthropic's: "You **MUST** always resend all
/// `thought` blocks exactly as they were received". A re-serialised blob is
/// not the same bytes, so the blob travels verbatim.
#[test]
fn reasoning_state_is_replayed_exactly_as_the_provider_minted_it() {
    let blob = r#"{"type":"thought","signature":"EpoGCpcGAdHtim9","summary":[{"type":"text","text":"short"}]}"#;
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "think"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning {
                    state: ReasoningState {
                        dialect: DIALECT.to_string(),
                        blob: blob.to_string(),
                    },
                }],
            },
            Message::text(Role::User, "again"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        body_of(&req)["input"][1],
        serde_json::from_str::<serde_json::Value>(blob).expect("the blob is JSON")
    );
}

/// State another dialect minted is refused rather than sent as if it were ours.
#[test]
fn reasoning_state_from_another_dialect_is_refused() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "think"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning {
                    state: ReasoningState {
                        dialect: "anthropic-messages".to_string(),
                        blob: r#"{"type":"thinking"}"#.to_string(),
                    },
                }],
            },
            Message::text(Role::User, "again"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("another dialect's state is not replayable here"),
        Inexpressible::ReasoningStateFromAnotherDialect {
            dialect: "anthropic-messages".to_string()
        }
    );
}

/// Provider-private state is opaque but still has to be a complete JSON step;
/// malformed bytes are refused before a transport can observe the request.
#[test]
fn malformed_reasoning_state_is_refused_before_network_effects() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "think"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning {
                    state: ReasoningState {
                        dialect: DIALECT.to_string(),
                        blob: "not a JSON step".to_string(),
                    },
                }],
            },
            Message::text(Role::User, "again"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("malformed private state cannot go on the wire"),
        Inexpressible::ReasoningStateNotJson
    );
}

/// The four levels this API spells, sent verbatim, in `generation_config`.
#[test]
fn a_thinking_level_this_api_spells_is_sent_verbatim() {
    for (level, spelled) in [
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
    ] {
        let body = body_of(&GenRequest {
            web_search: None,
            cache: None,
            thinking: level,
            ..wide_request()
        });
        assert_eq!(
            body["generation_config"]["thinking_level"],
            serde_json::json!(spelled),
            "{level:?} is spelled {spelled} on this wire"
        );
        assert_eq!(
            body["generation_config"]["max_output_tokens"],
            serde_json::json!(4096),
            "the ceiling travels beside it"
        );
    }
}

/// Refuse `Off` and name the supported thinking levels. Omitting the field would select default
/// reasoning rather than disable it.
#[test]
fn off_is_refused_because_this_api_cannot_switch_thinking_off() {
    let refusal = request_body(&GenRequest {
        web_search: None,
        cache: None,
        thinking: ThinkingLevel::Off,
        ..wide_request()
    })
    .expect_err("this API has no off");

    assert_eq!(
        refusal,
        Inexpressible::ThinkingLevelUnavailable {
            level: ThinkingLevel::Off
        }
    );
    let said = refusal.to_string();
    for named in ["minimal", "low", "medium", "high"] {
        assert!(
            said.contains(named),
            "the refusal must name {named}: {said}"
        );
    }
    assert!(
        !said.contains("xhigh"),
        "and must not name a level it cannot send either: {said}"
    );
}

/// The two levels above `high` are refused for the same reason and by the same
/// rule, rather than being quietly clamped down to `high`.
#[test]
fn a_level_above_high_is_refused_rather_than_clamped() {
    for level in [ThinkingLevel::XHigh, ThinkingLevel::Max] {
        assert_eq!(
            request_body(&GenRequest {
                web_search: None,
                cache: None,
                thinking: level,
                ..wide_request()
            })
            .expect_err("this API stops at high"),
            Inexpressible::ThinkingLevelUnavailable { level },
            "{level:?} must refuse, because clamping bills for less thinking \
             than the caller asked for and says nothing"
        );
    }
}

/// A tool is declared as a `function` tool, with the field names this API uses.
#[test]
fn a_tool_is_declared_with_the_type_and_fields_this_api_names() {
    assert_eq!(
        body_of(&wide_request())["tools"],
        serde_json::json!([{
            "type": "function",
            "name": "read_file",
            "description": "Read a file from the workspace.",
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        }])
    );
}

/// The request is stateless, and says so.
///
/// `store` defaults to **true** on this API, so omitting it would opt ego into
/// server-held history — a second copy of the conversation ego already owns,
/// and one it cannot address, because no `AiEvent` could carry the interaction
/// id back. `previous_interaction_id` is absent for the same reason.
#[test]
fn the_request_is_stateless_and_states_it_rather_than_relying_on_a_default() {
    let body = body_of(&wide_request());

    assert_eq!(
        body["store"],
        serde_json::json!(false),
        "store defaults to true; a stateless client must say so"
    );
    assert!(
        body.get("previous_interaction_id").is_none(),
        "ego holds the history and never names a server-side one: {body}"
    );
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(body["model"], serde_json::json!("gemini-3.7-flash"));
}

/// Stable Interactions asks for SSE in the body and `Accept` header. It has no
/// beta path and no query-based compatibility spelling.
#[test]
fn the_stable_target_and_sse_negotiation_have_no_beta_or_query_fallback() {
    let base = Url::parse("https://generativelanguage.googleapis.com/v1/")
        .expect("a base this test controls");
    let headers = vec![HttpHeader::new("x-goog-api-key", b"k".to_vec()).unwrap()];
    let target = RequestTarget::new(&base, &headers);
    let built = build_request(&target, &wide_request()).expect("this request is expressible");

    assert_eq!(
        built.url.as_str(),
        "https://generativelanguage.googleapis.com/v1/interactions"
    );
    assert!(
        built.url.query().is_none(),
        "no query belongs on {}",
        built.url
    );
    let names: Vec<&str> = built.headers.iter().map(|header| header.name()).collect();
    assert!(names.contains(&"x-goog-api-key"), "{names:?}");
    assert!(names.contains(&"content-type"), "{names:?}");
    let accept = built
        .headers
        .iter()
        .find(|header| header.name() == "accept")
        .expect("streaming requests carry Accept");
    assert_eq!(accept.value(), b"text/event-stream");
    assert_eq!(body_of(&wide_request())["stream"], serde_json::json!(true));
}

// --- the stream ---

fn decode_at(transcript: &[u8], at: usize) -> Vec<AiEvent> {
    let mut decoder = StreamDecoder::default();
    let mut events = decoder.push(&transcript[..at]);
    events.extend(decoder.push(&transcript[at..]));
    events.extend(decoder.finish(None));
    events
}

fn decode_whole(transcript: &[u8]) -> Vec<AiEvent> {
    decode_at(transcript, transcript.len())
}

fn decode_interaction(steps: &str) -> Vec<AiEvent> {
    let transcript = format!(
        "{steps}event: interaction.completed\ndata: {{\"interaction\":{{\"status\":\"completed\"}},\"event_type\":\"interaction.completed\"}}\n\ndata: [DONE]\n\n"
    );
    decode_whole(transcript.as_bytes())
}

fn reasoning_states(events: &[AiEvent]) -> Vec<ReasoningState> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::ReasoningState(state) => Some(state.clone()),
            _ => None,
        })
        .collect()
}

fn state_json(state: &ReasoningState) -> serde_json::Value {
    serde_json::from_str(&state.blob).expect("the provider thought is JSON")
}

/// The current thinking guide's stream starts a thought with its summary and
/// an empty signature, then updates the signature in the final delta.
#[test]
fn step_start_keeps_initial_thought_summary_before_signature_update() {
    let events = decode_interaction(
        r#"event: step.start
data: {"index":0,"step":{"signature":"","summary":[{"text":"**Evaluating the clues**\n\nI'm considering...","type":"text"}],"type":"thought"},"event_type":"step.start"}

event: step.delta
data: {"index":0,"delta":{"signature":"EpoGCpcGAXLI2nx/...","type":"thought_signature"},"event_type":"step.delta"}

event: step.stop
data: {"index":0,"event_type":"step.stop"}

"#,
    );

    assert_eq!(
        events[0],
        AiEvent::ReasoningDelta {
            text: "**Evaluating the clues**\n\nI'm considering...".to_string()
        }
    );
    assert_eq!(
        state_json(&reasoning_states(&events)[0]),
        serde_json::json!({
            "type": "thought",
            "signature": "EpoGCpcGAXLI2nx/...",
            "summary": [{"text": "**Evaluating the clues**\n\nI'm considering...", "type": "text"}],
        })
    );
    assert_eq!(events.len(), 3, "reasoning delta, state, and terminal stop");
}

/// The same official `step.start` shape carries the first model-output text;
/// it must be emitted once before later `text` deltas for that step.
#[test]
fn model_output_step_start_text_precedes_later_text_delta() {
    let events = decode_interaction(
        r#"event: step.start
data: {"index":0,"step":{"content":[{"text":"Based on the clues provided, here","type":"text"}],"type":"model_output"},"event_type":"step.start"}

event: step.delta
data: {"index":0,"delta":{"text":" is the answer to your question...","type":"text"},"event_type":"step.delta"}

event: step.stop
data: {"index":0,"event_type":"step.stop"}

"#,
    );

    assert_eq!(
        events,
        vec![
            AiEvent::TextDelta {
                text: "Based on the clues provided, here".to_string()
            },
            AiEvent::TextDelta {
                text: " is the answer to your question...".to_string()
            },
            AiEvent::Stop(StopReason::Stop),
        ]
    );
}

/// Thought-summary content is an open text/image union. Text is visible to
/// the caller, while image and an as-yet-unknown union arm remain opaque in the
/// replay block instead of being dropped or guessed into text.
#[test]
fn thought_summary_deltas_retain_text_image_and_unknown_union_members() {
    let events = decode_interaction(
        r#"event: step.start
data: {"index":0,"step":{"signature":"","summary":[],"type":"thought"},"event_type":"step.start"}

event: step.delta
data: {"index":0,"delta":{"content":{"text":"text summary","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}

event: step.delta
data: {"index":0,"delta":{"content":{"data":"AQID","mime_type":"image/jpeg","type":"image"},"type":"thought_summary"},"event_type":"step.delta"}

event: step.delta
data: {"index":0,"delta":{"content":{"payload":{"kept":true},"type":"future_summary"},"type":"thought_summary"},"event_type":"step.delta"}

event: step.delta
data: {"index":0,"delta":{"signature":"sig-with-summary","type":"thought_signature"},"event_type":"step.delta"}

event: step.stop
data: {"index":0,"event_type":"step.stop"}

"#,
    );

    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                AiEvent::ReasoningDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["text summary"]
    );
    assert_eq!(
        state_json(&reasoning_states(&events)[0]),
        serde_json::json!({
            "type": "thought",
            "signature": "sig-with-summary",
            "summary": [
                {"text": "text summary", "type": "text"},
                {"data": "AQID", "mime_type": "image/jpeg", "type": "image"},
                {"payload": {"kept": true}, "type": "future_summary"},
            ],
        })
    );
}

/// A signature-only thought is a valid provider response and must stay
/// signature-only when replayed; no summary field may be invented.
#[test]
fn signature_only_thoughts_replay_without_a_synthetic_summary() {
    let events = decode_interaction(
        r#"event: step.start
data: {"index":0,"step":{"signature":"","type":"thought"},"event_type":"step.start"}

event: step.delta
data: {"index":0,"delta":{"signature":"signature-only","type":"thought_signature"},"event_type":"step.delta"}

event: step.stop
data: {"index":0,"event_type":"step.stop"}

"#,
    );

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::ReasoningDelta { .. }))
    );
    assert_eq!(
        state_json(&reasoning_states(&events)[0]),
        serde_json::json!({"type": "thought", "signature": "signature-only"})
    );
}

/// Step indices correlate deltas, but they do not permit the adapter to sort
/// completed thought blocks out of stream order.
#[test]
fn multiple_thought_steps_keep_their_summary_and_completion_order() {
    let events = decode_interaction(
        r#"event: step.start
data: {"index":0,"step":{"signature":"","summary":[{"text":"first","type":"text"}],"type":"thought"},"event_type":"step.start"}

event: step.delta
data: {"index":0,"delta":{"signature":"first-signature","type":"thought_signature"},"event_type":"step.delta"}

event: step.stop
data: {"index":0,"event_type":"step.stop"}

event: step.start
data: {"index":1,"step":{"signature":"","summary":[{"text":"second","type":"text"}],"type":"thought"},"event_type":"step.start"}

event: step.delta
data: {"index":1,"delta":{"signature":"second-signature","type":"thought_signature"},"event_type":"step.delta"}

event: step.stop
data: {"index":1,"event_type":"step.stop"}

"#,
    );

    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                AiEvent::ReasoningDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    let states = reasoning_states(&events);
    assert_eq!(state_json(&states[0])["signature"], "first-signature");
    assert_eq!(state_json(&states[0])["summary"][0]["text"], "first");
    assert_eq!(state_json(&states[1])["signature"], "second-signature");
    assert_eq!(state_json(&states[1])["summary"][0]["text"], "second");
}

/// The opaque state emitted by decoding is the same complete thought step the
/// next stateless request sends back, including the summary received at start.
#[test]
fn decoded_thought_state_replays_the_complete_provider_json() {
    let events = decode_interaction(
        r#"event: step.start
data: {"index":0,"step":{"signature":"","summary":[{"text":"keep this summary","type":"text"}],"type":"thought"},"event_type":"step.start"}

event: step.delta
data: {"index":0,"delta":{"signature":"replay-signature","type":"thought_signature"},"event_type":"step.delta"}

event: step.stop
data: {"index":0,"event_type":"step.stop"}

"#,
    );
    let state = reasoning_states(&events)
        .into_iter()
        .next()
        .expect("the thought closes with replayable state");
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "think"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning { state }],
            },
            Message::text(Role::User, "continue"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        body_of(&req)["input"][1],
        serde_json::json!({
            "type": "thought",
            "signature": "replay-signature",
            "summary": [{"text": "keep this summary", "type": "text"}],
        })
    );
}

/// The golden decodes to an exact sequence, reasoning state and all.
#[test]
fn a_golden_transcript_decodes_to_an_exact_event_sequence() {
    let events = decode_whole(TEXT_TRANSCRIPT.as_bytes());

    assert_eq!(
        events,
        vec![
            AiEvent::ReasoningDelta {
                text: "The file listing ".to_string()
            },
            AiEvent::ReasoningDelta {
                text: "is short.".to_string()
            },
            AiEvent::ReasoningState(ReasoningState {
                dialect: DIALECT.to_string(),
                blob: r#"{"type":"thought","signature":"EpoGCpcGAdHtim9","summary":[{"type":"text","text":"The file listing "},{"type":"text","text":"is short."}]}"#.to_string(),
            }),
            AiEvent::TextDelta {
                text: "main.rs".to_string()
            },
            AiEvent::TextDelta {
                text: " and lib.rs".to_string()
            },
            AiEvent::Usage(Usage {
                uncached_input_tokens: 600,
                cache_read_tokens: 400,
                cache_write_tokens: 0,
                cache_write_extended_tokens: 0,
                web_search_requests: 0,
                reasoning_tokens: Some(245),
                output_tokens: 495,
            }),
            AiEvent::Stop(StopReason::Stop),
        ]
    );
}

/// The same transcript decodes the same however the bytes are cut.
#[test]
fn a_transcript_decodes_the_same_at_every_chunk_boundary() {
    let whole = decode_whole(TEXT_TRANSCRIPT.as_bytes());
    for at in 0..TEXT_TRANSCRIPT.len() {
        assert_eq!(
            decode_at(TEXT_TRANSCRIPT.as_bytes(), at),
            whole,
            "a split at {at} changed the decoding"
        );
    }
}

/// A running total on `step.stop` is restated at completion, not spent again. The fixture
/// contains both so summing them would fail.
#[test]
fn a_running_total_restated_on_a_step_is_not_a_second_charge() {
    let usages: Vec<Usage> = decode_whole(TEXT_TRANSCRIPT.as_bytes())
        .into_iter()
        .filter_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect();

    match usages.as_slice() {
        [only] => assert_eq!(only.output_tokens, 495, "250 output + 245 thought, once"),
        other => panic!("usage was reported {} times: {other:?}", other.len()),
    }
}

/// Thought tokens and output tokens are separate summands; tool-use tokens count as input.
/// Treating thoughts as an output subset would under-report usage.
#[test]
fn thought_tokens_add_to_output_and_tool_use_tokens_add_to_input() {
    let usage = decode_whole(TOOL_TRANSCRIPT.as_bytes())
        .into_iter()
        .find_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("the tool transcript reports usage");

    // 700 input - 200 cached + 100 tool-use, and the cached figure kept apart.
    assert_eq!(usage.uncached_input_tokens, 600);
    assert_eq!(usage.cache_read_tokens, 200);
    assert_eq!(usage.output_tokens, 96, "96 output + 0 thought");
    assert_eq!(usage.reasoning_tokens, Some(0));
}

/// Tool calls arrive whole, assembled from their `arguments_delta` fragments.
#[test]
fn tool_calls_arrive_with_their_arguments_assembled() {
    let calls: Vec<(String, String, String)> = decode_whole(TOOL_TRANSCRIPT.as_bytes())
        .into_iter()
        .filter_map(|event| match event {
            AiEvent::ToolCall {
                call_id,
                name,
                args,
            } => Some((call_id, name, args)),
            _ => None,
        })
        .collect();

    assert_eq!(
        calls,
        vec![
            (
                "un6k8t18".to_string(),
                "read_file".to_string(),
                r#"{"path": "a.txt"}"#.to_string()
            ),
            (
                "un6k8t19".to_string(),
                "read_file".to_string(),
                r#"{"path": "b.txt"}"#.to_string()
            ),
        ]
    );
}

/// `requires_action` is this protocol's way of saying the model wants a tool.
#[test]
fn a_tool_transcript_ends_as_tool_use() {
    let events = decode_whole(TOOL_TRANSCRIPT.as_bytes());
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));
}

/// Every status this protocol documents becomes what it means, or says it
/// cannot.
///
/// There is no finish reason on this wire at all — the terminal signal is the
/// interaction's `status`, and `queued`/`in_progress` correctly map to nothing
/// because `StopReason` is terminal by design.
#[test]
fn each_status_becomes_what_it_means_or_says_it_cannot() {
    for (status, expected) in [
        ("completed", StopReason::Stop),
        ("requires_action", StopReason::ToolUse),
        ("incomplete", StopReason::Length),
        ("cancelled", StopReason::Aborted),
        ("failed", StopReason::Error),
    ] {
        let transcript = format!(
            "event: interaction.completed\ndata: {{\"interaction\":{{\"id\":\"v1\",\
             \"status\":\"{status}\"}},\"event_type\":\"interaction.completed\"}}\n\n"
        );
        let events = decode_whole(transcript.as_bytes());
        assert_eq!(
            events.last(),
            Some(&AiEvent::Stop(expected)),
            "{status} was mapped wrongly: {events:?}"
        );
    }
}

/// `budget_exceeded` is the provider's own budget halting the turn, and
/// collapsing it into a generic error would throw away the one signal that
/// distinguishes "we ran out" from "they ran out".
#[test]
fn a_provider_side_budget_halt_says_which_budget_ran_out() {
    let transcript = "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"v1\",\
                      \"status\":\"budget_exceeded\"},\"event_type\":\"interaction.completed\"}\n\n";
    let events = decode_whole(transcript.as_bytes());

    let explained = events.iter().any(|event| match event {
        AiEvent::Error { message, .. } => {
            message.contains("budget_exceeded") && message.contains("provider")
        }
        _ => false,
    });
    assert!(
        explained,
        "the provider's budget must be named as theirs: {events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}

/// The status vocabulary is **open** — the generated types carry an
/// `UnrecognizedStr` arm — so a status this mapping does not know must fail
/// closed and say which one it was.
#[test]
fn an_unknown_status_fails_closed_and_names_itself() {
    let transcript = "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"v1\",\
                      \"status\":\"deferred_review\"},\"event_type\":\"interaction.completed\"}\n\n";
    let events = decode_whole(transcript.as_bytes());

    let named = events.iter().any(|event| match event {
        AiEvent::Error { message, .. } => message.contains("deferred_review"),
        _ => false,
    });
    assert!(named, "an unknown status must name itself: {events:?}");
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}

/// An in-band error ends generation with the provider's diagnostic. Its `code` is a string
/// slug, not a number.
#[test]
fn an_error_event_ends_the_generation_with_what_the_provider_said() {
    let transcript = "event: error\ndata: {\"error\":{\"message\":\"Deadline expired before \
                      operation could complete.\",\"code\":\"gateway_timeout\"},\
                      \"event_type\":\"error\"}\n\n";
    let events = decode_whole(transcript.as_bytes());

    let relayed = events.iter().any(|event| match event {
        AiEvent::Error { message, kind } => {
            message.contains("Deadline expired") && *kind == ErrorKind::Transient
        }
        _ => false,
    });
    assert!(relayed, "{events:?}");
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}

/// `[DONE]` is only a framing sentinel. Without `interaction.completed`, a
/// stream that simply stops was cut and is not a short answer.
#[test]
fn a_stream_that_ends_before_the_interaction_completes_is_an_error() {
    let cut = TEXT_TRANSCRIPT
        .split_once("event: interaction.completed")
        .expect("the golden has one")
        .0;

    let events = decode_whole(cut.as_bytes());
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::Error { .. })),
        "a cut stream must say it was cut: {events:?}"
    );
}

/// A non-200 is decoded from the body the provider sent with it.
#[tokio::test]
async fn a_refused_request_relays_the_sentence_the_provider_wrote() {
    use futures::StreamExt;

    let body = include_str!("goldens/profile/gemini/error/404.json");
    let response = Ok(HttpResponse {
        status: 404,
        headers: vec![
            HttpHeader::new("content-type", b"application/json".to_vec()).expect("header"),
        ],
        body: Box::pin(futures::stream::once(async move {
            Ok(bytes::Bytes::from(body.as_bytes()))
        })),
    });

    let events: Vec<AiEvent> = ego_ai::wire::gemini_interactions::decode(response, None)
        .collect()
        .await;

    let relayed = events.iter().any(|event| match event {
        AiEvent::Error { message, .. } => message.contains("gemini-9.9-ultra"),
        _ => false,
    });
    assert!(
        relayed,
        "the provider's own sentence must survive: {events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}

/// Gemini is a selectable provider, and that is a decision rather than a
/// consequence of the count.
///
/// Gemini's implemented mapping belongs in the inventory. Omitting it would
/// report unknown provider instead of the actual credential or availability
/// state. Offline fixtures do not establish live acceptance.
///
/// Still asserted by name rather than left to `FINAL_PROFILE_COUNT`, and for
/// the reason the original test gave: the count is a constant, so a row that
/// silently fell out again would keep every other assertion green.
#[test]
fn gemini_is_among_the_providers_a_model_can_be_selected_from() {
    let rows = ego_ai::conformance::final_provider_rows(
        ego_ai::provider::ProviderRow::ollama_default(),
        ego_ai::provider::ProviderRow::openai_at(
            Url::parse("https://api.openai.com").expect("a constant this test controls"),
        ),
        ego_ai::provider::ProviderRow::anthropic_default(),
    );
    let ids: Vec<_> = rows.iter().map(ego_ai::provider::ProviderRow::id).collect();
    assert!(
        ids.contains(&"gemini"),
        "a shipped mapping with passing fixtures is absent from the inventory, \
         so `-m gemini/…` answers `unknown provider`: {ids:?}"
    );
}
