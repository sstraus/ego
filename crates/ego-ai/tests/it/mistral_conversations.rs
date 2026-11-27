//! The `mistral-conversations` wire format, against artificial transcripts.
//!
//! # What the fixtures are, and what they are worth
//!
//! **Schema-derived, artificial.** Every stream below is hand-assembled from
//! the public `openapi.yaml` (SHA-256 `7652f1307b3bc6cb…dfae`, component
//! `ConversationEvents` and the event, chunk and usage schemas it references).
//! No recorded Mistral capture exists in this repository and none was made.
//! These tests prove the codec against the published schema; they do not prove
//! the live ordering of events, how `function.call.delta` fragments its
//! arguments, or that the server accepts a stateless replay.

use base64::Engine as _;
use ego_ai::wire::mistral_conversations::{
    BASE, DIALECT, Inexpressible, MistralConversations, StreamDecoder, build_request, request_body,
};
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::capabilities::CacheRetention;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, HostedSearch, Image, ImageMediaType,
    Message, PromptCache, ReasoningState, Role, StopReason, ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use ego_core::state::Usage;
use serde_json::{Value, json};
use url::Url;

const IMAGE_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

/// Thinking, text, a hosted search, then a cited answer. Schema-derived.
const SEARCH_TRANSCRIPT: &str = r#"event: conversation.response.started
data: {"type":"conversation.response.started","conversation_id":"conv_schema_1"}

event: message.output.delta
data: {"type":"message.output.delta","id":"msg_a","output_index":0,"content_index":0,"role":"assistant","content":{"type":"thinking","thinking":[{"type":"text","text":"Need current "}]}}

event: message.output.delta
data: {"type":"message.output.delta","id":"msg_a","output_index":0,"content_index":0,"role":"assistant","content":{"type":"thinking","thinking":[{"type":"text","text":"facts."}]}}

event: message.output.delta
data: {"type":"message.output.delta","id":"msg_a","output_index":0,"content_index":1,"role":"assistant","content":"Searching."}

event: tool.execution.started
data: {"type":"tool.execution.started","id":"tool_1","output_index":1,"name":"web_search","arguments":"{\"query\":\"ego release\"}"}

event: tool.execution.done
data: {"type":"tool.execution.done","id":"tool_1","output_index":1,"name":"web_search","info":{}}

event: message.output.delta
data: {"type":"message.output.delta","id":"msg_b","output_index":2,"content_index":0,"role":"assistant","content":"It shipped"}

event: message.output.delta
data: {"type":"message.output.delta","id":"msg_b","output_index":2,"content_index":1,"role":"assistant","content":{"type":"tool_reference","tool":"web_search","title":"Release notes","url":"https://example.com/notes","favicon":null,"description":null}}

event: message.output.delta
data: {"type":"message.output.delta","id":"msg_b","output_index":2,"content_index":2,"role":"assistant","content":{"type":"tool_reference","tool":"web_search","title":"No link"}}

event: message.output.delta
data: {"type":"message.output.delta","id":"msg_b","output_index":2,"content_index":3,"role":"assistant","content":"."}

event: conversation.response.done
data: {"type":"conversation.response.done","usage":{"prompt_tokens":120,"completion_tokens":30,"total_tokens":150,"connector_tokens":40,"connectors":{"web_search":3}}}

"#;

/// Text, then a client function call in two argument fragments. Schema-derived.
const FUNCTION_TRANSCRIPT: &str = r#"event: message.output.delta
data: {"type":"message.output.delta","id":"msg_a","output_index":0,"content_index":0,"role":"assistant","content":"Reading."}

event: function.call.delta
data: {"type":"function.call.delta","id":"fc_1","output_index":1,"name":"read_file","tool_call_id":"call_1","arguments":"{\"path\":"}

event: function.call.delta
data: {"type":"function.call.delta","id":"fc_1","output_index":1,"name":"read_file","tool_call_id":"call_1","arguments":"\"src\"}"}

event: conversation.response.done
data: {"type":"conversation.response.done","usage":{"prompt_tokens":50,"completion_tokens":9,"total_tokens":59}}

"#;

fn conversation(messages: Vec<Message>) -> Conversation {
    Conversation::new(messages).expect("the test histories here are valid")
}

fn read_file() -> ToolDescriptor {
    ToolDescriptor {
        name: "read_file".to_string(),
        description: "Read a file from the workspace.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    }
}

fn request(messages: Vec<Message>) -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "mistral-large-4-0".to_string(),
        messages: conversation(messages),
        tools: vec![read_file()],
        thinking: ThinkingLevel::High,
        max_output_tokens: Some(2048),
        parallel_tool_calls: true,
    }
}

fn first_turn() -> Vec<Message> {
    vec![
        Message::text(Role::System, "You are terse."),
        Message::text(Role::User, "Did ego ship?"),
    ]
}

fn search() -> Option<HostedSearch> {
    Some(HostedSearch {
        max_requests: std::num::NonZeroU32::new(3).unwrap(),
        allow_live_access: true,
    })
}

fn body_of(req: &GenRequest) -> Value {
    let bytes = request_body(req).expect("this request is expressible");
    serde_json::from_slice(&bytes).expect("the body is valid JSON")
}

fn decode_at(transcript: &[u8], at: usize) -> Vec<AiEvent> {
    let mut decoder = StreamDecoder::default();
    let mut events = decoder.push(&transcript[..at]);
    events.extend(decoder.push(&transcript[at..]));
    events.extend(decoder.finish(None));
    events
}

fn decode_whole(transcript: &str) -> Vec<AiEvent> {
    decode_at(transcript.as_bytes(), transcript.len())
}

fn provider_blocks(events: &[AiEvent]) -> Vec<ContentBlock> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::ProviderContent { state, .. } => Some(ContentBlock::ProviderContent {
                state: state.clone(),
                text: None,
            }),
            _ => None,
        })
        .collect()
}

// --- the request ---

/// Catches a request that relies on the server default `store: true` or
/// names a server-side conversation ego cannot address.
#[test]
fn the_request_is_stateless_and_streams_to_the_conversations_route() {
    let req = request(first_turn());
    let body = body_of(&req);
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["model"], json!("mistral-large-4-0"));
    for absent in ["conversation_id", "agent_id", "handoff_execution"] {
        assert!(
            body.get(absent).is_none(),
            "{absent} must be absent: {body}"
        );
    }

    let base = Url::parse(BASE).expect("the official base parses");
    let headers = vec![HttpHeader::new("authorization", b"Bearer k".to_vec()).unwrap()];
    let built = build_request(&RequestTarget::new(&base, &headers), &req).expect("expressible");
    assert_eq!(
        built.url.as_str(),
        "https://api.mistral.ai/v1/conversations"
    );
    let accept = built
        .headers
        .iter()
        .find(|header| header.name() == "accept")
        .expect("streaming requests carry Accept");
    assert_eq!(accept.value(), b"text/event-stream");
}

/// Catches a system turn sent as an entry instead of `instructions`.
#[test]
fn system_text_is_instructions_and_never_an_entry() {
    let body = body_of(&request(first_turn()));
    assert_eq!(body["instructions"], json!("You are terse."));
    assert_eq!(
        body["inputs"],
        json!([{
            "object": "entry",
            "type": "message.input",
            "role": "user",
            "content": [{"type": "text", "text": "Did ego ship?"}]
        }])
    );
}

/// Catches `max_tokens` at the top level, which `additionalProperties: false`
/// on the request rejects, and an effort spelled with ego's own vocabulary.
#[test]
fn output_limit_and_effort_live_in_completion_args() {
    let mut req = request(first_turn());
    let body = body_of(&req);
    assert_eq!(
        body["completion_args"],
        json!({"max_tokens": 2048, "reasoning_effort": "high"})
    );
    assert!(body.get("max_tokens").is_none(), "{body}");
    assert!(body.get("max_completion_tokens").is_none(), "{body}");

    req.thinking = ThinkingLevel::Off;
    assert_eq!(
        body_of(&req)["completion_args"]["reasoning_effort"],
        json!("none")
    );
}

/// Catches native search sent as a local function, a duplicate `web_search`
/// name, or a hosted tool declared when nobody asked for it.
#[test]
fn native_search_is_a_hosted_tool_beside_functions() {
    let mut req = request(first_turn());
    let function = json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file from the workspace.",
            "parameters": read_file().parameters
        }
    });
    assert_eq!(body_of(&req)["tools"], json!([function.clone()]));

    req.tools.push(ToolDescriptor {
        name: "web_search".to_string(),
        description: "Local search.".to_string(),
        parameters: json!({"type": "object"}),
    });
    req.web_search = search();
    assert_eq!(
        body_of(&req)["tools"],
        json!([function, {"type": "web_search"}])
    );
}

/// Catches an undocumented cache field on a route whose schema has none, and
/// a cache identity claimed where no counter can confirm it.
#[test]
fn prompt_cache_metadata_changes_nothing_on_this_route() {
    let plain = request(first_turn());
    let mut cached = plain.clone();
    cached.cache = Some(PromptCache {
        verifier: None,
        session_id: ego_core::event::id::SessionId::new(),
        lineage_root: ego_core::event::id::SessionId::new(),
        retention: CacheRetention::Long,
        stable_messages: 1,
    });
    let bytes = request_body(&cached).expect("expressible");
    assert_eq!(bytes, request_body(&plain).expect("expressible"));
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    for absent in [
        "prompt_cache_key",
        "prompt_cache_retention",
        "prompt_cache_breakpoint",
    ] {
        assert!(body.get(absent).is_none(), "{absent}: {body}");
    }
    let mapping = MistralConversations;
    assert!(mapping.cache_prefix(&bytes).is_none());
    assert!(
        mapping
            .cache_request_identity(&bytes, "scope".to_string(), None)
            .is_none()
    );
    assert!(mapping.supports_web_search());
}

/// Catches a function round trip whose call and result lose their shared
/// `tool_call_id`, or a replayed answer rebuilt instead of sent unchanged.
#[test]
fn a_function_round_trip_replays_as_documented_entries() {
    let blob = r#"{"object":"entry","type":"message.output","role":"assistant","content":[{"type":"text","text":"Reading."}]}"#;
    let req = request(vec![
        Message::text(Role::System, "You are terse."),
        Message::text(Role::User, "List src."),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ProviderContent {
                    state: ReasoningState {
                        connection: None,
                        dialect: DIALECT.to_string(),
                        blob: blob.to_string(),
                    },
                    text: Some("Reading.".to_string()),
                },
                ContentBlock::ToolUse {
                    call_id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    args: r#"{"path":"src"}"#.to_string(),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "call_1".to_string(),
                output: "a.rs".to_string(),
                is_error: false,
            }],
        },
    ]);
    let body = body_of(&req);
    assert_eq!(
        body["inputs"],
        json!([
            {"object": "entry", "type": "message.input", "role": "user",
             "content": [{"type": "text", "text": "List src."}]},
            serde_json::from_str::<Value>(blob).unwrap(),
            {"object": "entry", "type": "function.call", "tool_call_id": "call_1",
             "name": "read_file", "arguments": "{\"path\":\"src\"}"},
            {"object": "entry", "type": "function.result", "tool_call_id": "call_1",
             "result": "a.rs"}
        ])
    );
}

/// Catches prefix drift across a searched turn: re-encoded entries, reordered
/// tools or a changed instruction would defeat any provider-side prefix reuse
/// and alter what the model reads.
#[test]
fn an_earlier_request_is_an_unchanged_prefix_of_the_next() {
    let mut first = request(first_turn());
    first.web_search = search();
    let body1 = body_of(&first);

    let mut messages = first_turn();
    messages.push(Message {
        role: Role::Assistant,
        content: provider_blocks(&decode_whole(SEARCH_TRANSCRIPT)),
    });
    messages.push(Message::text(Role::User, "Thanks."));
    let mut second = request(messages);
    second.web_search = search();
    let body2 = body_of(&second);

    let inputs1 = body1["inputs"].as_array().unwrap();
    let inputs2 = body2["inputs"].as_array().unwrap();
    assert_eq!(&inputs2[..inputs1.len()], inputs1.as_slice());
    assert_eq!(inputs2.len(), inputs1.len() + 3, "{body2}");
    assert_eq!(body1["instructions"], body2["instructions"]);
    assert_eq!(body1["tools"], body2["tools"]);
    assert_eq!(body1["completion_args"], body2["completion_args"]);
}

/// Catches provider content from another wire sent as if Mistral minted it,
/// and an image sent through an unverified chunk shape.
#[test]
fn unsendable_history_is_refused_before_any_request() {
    let foreign = request(vec![
        Message::text(Role::User, "Hi."),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ProviderContent {
                state: ReasoningState {
                    connection: None,
                    dialect: "openai-responses".to_string(),
                    blob: "{}".to_string(),
                },
                text: Some("Hello.".to_string()),
            }],
        },
        Message::text(Role::User, "Again."),
    ]);
    assert_eq!(
        request_body(&foreign).unwrap_err(),
        Inexpressible::ProviderContentFromAnotherDialect {
            dialect: "openai-responses".to_string()
        }
    );

    let image = Image::new(
        base64::engine::general_purpose::STANDARD
            .decode(IMAGE_PNG)
            .unwrap(),
        ImageMediaType::Png,
    )
    .unwrap();
    let pictured = request(vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Image { image }],
    }]);
    assert_eq!(
        request_body(&pictured).unwrap_err(),
        Inexpressible::ImageNotSupported
    );
}

// --- the stream ---

/// Catches a dropped citation, an invented citation for a reference without
/// a URL, a search counted twice, a lost thinking chunk on replay, and a
/// cache or reasoning figure the schema never stated.
#[test]
fn a_searched_answer_decodes_to_an_exact_event_sequence() {
    let events = decode_whole(SEARCH_TRANSCRIPT);
    let before = json!({"object": "entry", "type": "message.output", "role": "assistant",
    "content": [
        {"type": "thinking", "thinking": [{"type": "text", "text": "Need current facts."}]},
        {"type": "text", "text": "Searching."}
    ]});
    let after = json!({"object": "entry", "type": "message.output", "role": "assistant",
    "content": [
        {"type": "text", "text": "It shipped"},
        {"type": "tool_reference", "tool": "web_search", "title": "Release notes",
         "url": "https://example.com/notes", "favicon": null, "description": null},
        {"type": "tool_reference", "tool": "web_search", "title": "No link"},
        {"type": "text", "text": "."}
    ]});
    let blob = |event: &AiEvent| match event {
        AiEvent::ProviderContent {
            state,
            visible: true,
        } if state.dialect == DIALECT => Some(serde_json::from_str::<Value>(&state.blob).unwrap()),
        _ => None,
    };

    assert_eq!(events.len(), 11, "{events:#?}");
    assert_eq!(
        events[0],
        AiEvent::ReasoningDelta {
            text: "Need current ".into()
        }
    );
    assert_eq!(
        events[1],
        AiEvent::ReasoningDelta {
            text: "facts.".into()
        }
    );
    assert_eq!(
        events[2],
        AiEvent::TextDelta {
            text: "Searching.".into()
        }
    );
    assert_eq!(blob(&events[3]), Some(before));
    assert_eq!(
        events[4],
        AiEvent::WebSearch {
            call_id: "tool_1".into(),
            query: "ego release".into()
        }
    );
    assert_eq!(
        events[5],
        AiEvent::TextDelta {
            text: "It shipped".into()
        }
    );
    assert_eq!(
        events[6],
        AiEvent::Citation {
            url: "https://example.com/notes".into(),
            title: "Release notes".into()
        }
    );
    assert_eq!(events[7], AiEvent::TextDelta { text: ".".into() });
    assert_eq!(blob(&events[8]), Some(after));
    assert_eq!(
        events[9],
        AiEvent::Usage(Usage {
            web_search_requests: 1,
            uncached_input_tokens: 120,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_extended_tokens: 0,
            output_tokens: 30,
            reasoning_tokens: None,
        })
    );
    assert_eq!(events[10], AiEvent::Stop(StopReason::Stop));
}

/// Catches state that depends on where the transport split the bytes.
#[test]
fn a_transcript_decodes_the_same_at_every_chunk_boundary() {
    let whole = decode_whole(SEARCH_TRANSCRIPT);
    for at in 0..=SEARCH_TRANSCRIPT.len() {
        assert_eq!(
            decode_at(SEARCH_TRANSCRIPT.as_bytes(), at),
            whole,
            "split at {at}"
        );
    }
}

/// Catches argument fragments emitted as separate calls, and a function turn
/// that ends as `Stop` so the loop never runs the tool.
#[test]
fn function_call_fragments_assemble_into_one_call_and_end_as_tool_use() {
    let events = decode_whole(FUNCTION_TRANSCRIPT);
    let calls: Vec<&AiEvent> = events
        .iter()
        .filter(|event| matches!(event, AiEvent::ToolCall { .. }))
        .collect();
    assert_eq!(
        calls,
        vec![&AiEvent::ToolCall {
            call_id: "call_1".into(),
            name: "read_file".into(),
            args: r#"{"path":"src"}"#.into(),
        }]
    );
    let content = events
        .iter()
        .position(|event| matches!(event, AiEvent::ProviderContent { .. }))
        .expect("the answer text is kept for replay");
    let call = events
        .iter()
        .position(|event| matches!(event, AiEvent::ToolCall { .. }))
        .unwrap();
    assert!(
        content < call,
        "history order must be text, then call: {events:#?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));
}

/// Catches `usage.connectors` read as billed searches, and one search counted
/// once per event that repeats its id.
#[test]
fn searches_are_counted_once_per_observed_execution() {
    let stream = r#"data: {"type":"tool.execution.started","id":"s1","name":"web_search","arguments":""}

data: {"type":"tool.execution.done","id":"s1","name":"web_search"}

data: {"type":"tool.execution.done","id":"s1","name":"web_search"}

data: {"type":"tool.execution.started","id":"s2","name":"web_search_premium","arguments":"{\"queries\":[\"a\",\"b\"]}"}

data: {"type":"tool.execution.started","id":"x1","name":"code_interpreter","arguments":""}

data: {"type":"conversation.response.done","usage":{"prompt_tokens":1,"completion_tokens":1,"connectors":{"web_search":9}}}

"#;
    let events = decode_whole(stream);
    let searches: Vec<(String, String)> = events
        .iter()
        .filter_map(|event| match event {
            AiEvent::WebSearch { call_id, query } => Some((call_id.clone(), query.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        searches,
        vec![
            ("s1".to_string(), "web search".to_string()),
            ("s2".to_string(), "a; b".to_string())
        ]
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::Usage(usage) if usage.web_search_requests == 2))
    );
}

/// Catches a cut stream settled as a finished answer.
#[test]
fn a_stream_without_response_done_is_an_error() {
    let cut = SEARCH_TRANSCRIPT
        .split_once("event: conversation.response.done")
        .unwrap()
        .0;
    let events = decode_whole(cut);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::Usage(_)))
    );
    let tail = &events[events.len() - 2..];
    assert!(
        matches!(
            tail[0],
            AiEvent::Error {
                kind: ErrorKind::Transient,
                ..
            }
        ),
        "{tail:?}"
    );
    assert_eq!(tail[1], AiEvent::Stop(StopReason::Error));
}

/// Catches a streamed provider error lost or classified as retryable.
#[test]
fn a_streamed_error_ends_with_the_provider_sentence() {
    let events = decode_whole(
        "data: {\"type\":\"conversation.response.error\",\"message\":\"Prompt is too long\",\"code\":3059}\n\n",
    );
    assert_eq!(
        events,
        vec![
            AiEvent::Error {
                message: "Prompt is too long".into(),
                kind: ErrorKind::ContextOverflow
            },
            AiEvent::Stop(StopReason::Error)
        ]
    );
}
