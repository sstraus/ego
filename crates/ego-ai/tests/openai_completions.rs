//! The `openai-completions` wire format, against transcripts it did not write.
//!
//! # What the goldens are, and what they are worth
//!
//! `tests/goldens/*.sse` are hand-assembled, not captured from a live account,
//! and that is a limitation worth naming rather than hiding behind the word
//! "golden". What makes them evidence and not decoration is that **every field
//! in them comes from `openai/openai-openapi`** — `CreateChatCompletionStream
//! Response`, `ChatCompletionStreamResponseDelta`,
//! `ChatCompletionMessageToolCallChunk` and `CompletionUsage` — including the
//! ones this format ignores (`logprobs`, `obfuscation`, `system_fingerprint`,
//! `audio_tokens`), which are there precisely so that ignoring them is tested.
//! A transcript written from memory would pin the memory; this one pins the
//! published shape.
//!
//! What they cannot prove is that OpenAI's servers behave as their schema
//! says. Nothing offline can. That is a job for a live smoke test against a
//! real key, and it is not this file's.
//!
//! # The request golden is compared as JSON, not as bytes
//!
//! Deliberately. A JSON object is unordered, so pinning the serialiser's key
//! order would pin an implementation detail that no provider can observe —
//! the failure Step 9 warns about, one level down. What is pinned is what the
//! provider actually reads: which fields are present, which are absent, and
//! what they say.

use ego_ai::http::{HttpError, HttpResponse};
use ego_ai::wire::RequestTarget;
use ego_ai::wire::openai_completions::{
    Inexpressible, StreamDecoder, build_request, decode, request_body,
};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, Message, ReasoningState, Role, StopReason,
    ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use ego_core::state::Usage;
use futures::StreamExt;
use url::Url;

const TEXT_TRANSCRIPT: &str = include_str!("goldens/chat_completions/stream/text.sse");
const TOOL_TRANSCRIPT: &str = include_str!("goldens/chat_completions/stream/tool_calls.sse");

fn conversation(messages: Vec<Message>) -> Conversation {
    Conversation::new(messages).expect("the test histories here are valid")
}

/// The request the golden pins: a system turn, a user turn, one tool, an
/// output ceiling and a non-default effort — every optional field exercised at
/// once, so a field that silently stops being sent has nowhere to hide.
fn golden_request() -> GenRequest {
    GenRequest {
        cache: None,
        model: "gpt-5.1".to_string(),
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

#[test]
fn the_request_body_matches_the_committed_golden() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("goldens/chat_completions_request.json"))
            .expect("the golden is valid JSON");
    assert_eq!(body_of(&golden_request()), expected);
}

/// `stream_options.include_usage` is not a detail: without it the provider
/// sends no usage at all, every generation reconciles at its pessimistic bound
/// and the budget narrows on every turn for no reason.
#[test]
fn usage_is_always_asked_for() {
    let body = body_of(&golden_request());
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(
        body["stream_options"],
        serde_json::json!({ "include_usage": true })
    );
}

/// The one place where sending nothing and sending a value are not the same
/// thing. The schema's default is `medium`, so an omitted field is a request
/// for medium-effort reasoning — the opposite of what `Off` asked for, billed
/// as reasoning tokens the caller declined.
#[test]
fn off_is_sent_as_none_rather_than_left_to_the_providers_default() {
    for (level, expected) in [
        (ThinkingLevel::Off, "none"),
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ] {
        let mut req = golden_request();
        req.thinking = level;
        assert_eq!(
            body_of(&req)["reasoning_effort"],
            serde_json::json!(expected),
            "{level:?} must be sent explicitly"
        );
    }
}

/// `parallel_tool_calls` without `tools` is a 400 from OpenAI, and it is also
/// meaningless — there is nothing to parallelise. `max_completion_tokens` is
/// omitted rather than sent as a guessed ceiling, and `max_tokens`, which the
/// schema deprecates in its favour, is never sent at all.
#[test]
fn fields_that_have_nothing_to_say_are_omitted() {
    let mut req = golden_request();
    req.tools.clear();
    req.max_output_tokens = None;
    let body = body_of(&req);
    let object = body.as_object().expect("the body is an object");
    for absent in ["tools", "parallel_tool_calls", "max_completion_tokens"] {
        assert!(
            !object.contains_key(absent),
            "{absent} should be omitted when there is nothing to say: {body}"
        );
    }
    assert!(
        !object.contains_key("max_tokens"),
        "max_tokens is deprecated in favour of max_completion_tokens: {body}"
    );
}

/// The round trip that used to be unbuildable: an assistant turn that called
/// two tools, and the results answering them. OpenAI wants the calls attached
/// to the assistant message and each result in its own `tool` message naming
/// the id — a shape `Message` could not express at all before
/// `ARCHITECTURE.md`.
#[test]
fn a_tool_round_trip_reaches_the_wire_in_the_shape_the_schema_asks_for() {
    let mut req = golden_request();
    req.messages = conversation(vec![
        Message::text(Role::User, "What is in ./src?"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Reading both.".to_string(),
                },
                ContentBlock::ToolUse {
                    call_id: "call_a".to_string(),
                    name: "read_file".to_string(),
                    args: r#"{"path":"src/main.rs"}"#.to_string(),
                },
                ContentBlock::ToolUse {
                    call_id: "call_b".to_string(),
                    name: "read_file".to_string(),
                    args: r#"{"path":"src/lib.rs"}"#.to_string(),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    call_id: "call_a".to_string(),
                    output: "fn main() {}".to_string(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    call_id: "call_b".to_string(),
                    output: "no such file".to_string(),
                    is_error: true,
                },
            ],
        },
    ]);
    assert_eq!(
        body_of(&req)["messages"],
        serde_json::json!([
            { "role": "user", "content": "What is in ./src?" },
            {
                "role": "assistant",
                "content": "Reading both.",
                "tool_calls": [
                    {
                        "id": "call_a",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}" }
                    },
                    {
                        "id": "call_b",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"src/lib.rs\"}" }
                    }
                ]
            },
            { "role": "tool", "tool_call_id": "call_a", "content": "fn main() {}" },
            { "role": "tool", "tool_call_id": "call_b", "content": "no such file" }
        ])
    );
}

/// An assistant turn that is nothing but calls sends no `content` at all —
/// the schema makes it "Required unless `tool_calls` … is specified", and an
/// empty string is a message that says nothing rather than one that is absent.
#[test]
fn an_assistant_turn_of_pure_tool_calls_sends_no_content_key() {
    let mut req = golden_request();
    req.messages = conversation(vec![
        Message::text(Role::User, "read it"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                call_id: "call_a".to_string(),
                name: "read_file".to_string(),
                args: "{}".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "call_a".to_string(),
                output: "ok".to_string(),
                is_error: false,
            }],
        },
    ]);
    let body = body_of(&req);
    let assistant = body["messages"][1].as_object().expect("an object");
    assert!(
        !assistant.contains_key("content"),
        "content is omitted, not empty: {assistant:?}"
    );
    assert!(assistant.contains_key("tool_calls"));
}

/// The chat-completions request schema has no field for a reasoning
/// signature. Sending the request without it would be a silent downgrade of
/// exactly the kind `ModelCapabilities::fallback_gaps` exists to catch.
#[test]
fn reasoning_state_is_refused_rather_than_dropped() {
    let req = GenRequest {
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "What is in ./src?"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning {
                    state: ReasoningState {
                        blob: "opaque".to_string(),
                        dialect: "anthropic-messages".to_string(),
                    },
                }],
            },
        ]),
        ..golden_request()
    };
    assert_eq!(
        request_body(&req),
        Err(Inexpressible::ReasoningStateNotCarriable)
    );
}

#[test]
fn the_request_declares_what_it_sends_and_what_it_expects_back() {
    let base = Url::parse("https://api.openai.com/v1/").expect("a URL");
    let headers = vec![HttpHeader::new("authorization", b"Bearer sk-test".to_vec()).unwrap()];
    let target = RequestTarget::new(&base, &headers);
    let req = build_request(&target, &golden_request()).expect("this request is expressible");

    // The row carries the base; the mapping owns the path and joins it on.
    assert_eq!(
        req.url,
        Url::parse("https://api.openai.com/v1/chat/completions").expect("a URL")
    );
    let header = |name: &str| {
        req.headers
            .iter()
            .find(|header| header.name() == name)
            .and_then(|header| std::str::from_utf8(header.value()).ok())
    };
    assert_eq!(header("content-type"), Some("application/json"));
    assert_eq!(
        header("accept"),
        Some("text/event-stream"),
        "the response is SSE, and a provider that content-negotiates needs telling"
    );
    assert_eq!(
        header("authorization"),
        Some("Bearer sk-test"),
        "the provider row's headers must survive"
    );
    assert!(req.body.is_some());
}

// --- the response ---

/// Drives the decoder over a transcript split at `at`.
///
/// Indexes **bytes**, not characters, deliberately: a chunk boundary is a
/// network event and lands wherever it lands, including between the two bytes
/// of a CRLF and in the middle of a multi-byte character. The text transcript
/// carries an accented word and an emoji so that the second of those is a real
/// case here and not a hypothetical one.
fn decode_at(transcript: &[u8], at: usize) -> Vec<AiEvent> {
    let at = at.min(transcript.len());
    let mut decoder = StreamDecoder::default();
    let mut events = decoder.push(&transcript[..at]);
    events.extend(decoder.push(&transcript[at..]));
    events.extend(decoder.finish(None));
    events
}

fn decode_whole(transcript: &[u8]) -> Vec<AiEvent> {
    decode_at(transcript, 0)
}

#[test]
fn a_golden_transcript_decodes_to_an_exact_event_sequence() {
    assert_eq!(
        decode_whole(TEXT_TRANSCRIPT.as_bytes()),
        vec![
            AiEvent::TextDelta {
                text: "Due ".to_string()
            },
            AiEvent::TextDelta {
                text: "file — perché no? 😀".to_string()
            },
            // 1000 prompt tokens = 400 cached + 100 written + 500 uncached.
            // The 180 reasoning tokens are *inside* the 250 output tokens,
            // not beside them: this golden's own `total_tokens` is 1250, and
            // the breakdown is read because the transcript publishes one.
            AiEvent::Usage(Usage {
                uncached_input_tokens: 500,
                cache_read_tokens: 400,
                cache_write_tokens: 100,
                cache_write_extended_tokens: 0,
                output_tokens: 250,
                reasoning_tokens: Some(180),
            }),
            AiEvent::Stop(StopReason::Stop),
        ],
        "the empty opening `content` must not become an empty delta, the \
         keep-alive comment and the unknown `obfuscation` field must be \
         ignored, and Stop must come after the usage chunk"
    );
}

/// The mapping the per-request schema alone would get wrong.
///
/// `prompt_tokens_details` says only that `cached_tokens` are "present in the
/// prompt" and `cache_write_tokens` are "written to cache"; it does not say
/// whether they overlap. OpenAI's aggregate usage schema does: `input_tokens`
/// is "including cached and cache-write tokens", `input_uncached_tokens` is
/// "excluding cache-write tokens", and both of its published examples add up.
/// So subtracting only `cached_tokens` would count every cache-write token
/// twice — once at the full rate and once at the write rate.
#[test]
fn the_cache_breakdown_partitions_the_prompt_instead_of_overlapping_it() {
    let usage = decode_whole(TEXT_TRANSCRIPT.as_bytes())
        .into_iter()
        .find_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("the transcript carries a usage chunk");
    assert_eq!(
        usage.total_tokens(),
        1250,
        "the four categories must add back up to prompt + completion, or the \
         occupancy proxy and every price computed from it are inflated"
    );
}

/// A daemon that reports no cache breakdown is not hiding one: it is saying
/// every prompt token was billed at the full rate.
#[test]
fn a_usage_report_without_a_cache_breakdown_is_all_uncached() {
    const BARE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":7,\"total_tokens\":37}}\n\n\
         data: [DONE]\n\n";
    assert_eq!(
        decode_whole(BARE.as_bytes()),
        vec![
            AiEvent::Usage(Usage {
                uncached_input_tokens: 30,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_extended_tokens: 0,
                output_tokens: 7,
                reasoning_tokens: None,
            }),
            AiEvent::Stop(StopReason::Stop),
        ]
    );
}

/// The breakdown that cannot be true. 80 cached plus 40 written is more than
/// the 100 tokens they are meant to be part of, and nothing in the payload
/// says which of the three numbers is wrong.
///
/// Clamping the difference to zero would produce a number that reads exactly
/// like a correct one while under-reporting the dearest category — the failure
/// direction that leaks budget. Reporting nothing sends the attempt to
/// `UsageReport::Missing`, which settles at its pessimistic bound: the
/// direction that costs budget instead.
#[test]
fn a_usage_report_whose_parts_exceed_its_total_is_discarded_not_repaired() {
    const IMPOSSIBLE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\
         \"total_tokens\":110,\"prompt_tokens_details\":{\"cached_tokens\":80,\"cache_write_tokens\":40}}}\n\n\
         data: [DONE]\n\n";
    let events = decode_whole(IMPOSSIBLE.as_bytes());
    assert!(
        !events.iter().any(|e| matches!(e, AiEvent::Usage(_))),
        "a figure that cannot be true must not be reported as one: {events:?}"
    );
    assert!(
        events.iter().any(|event| match event {
            AiEvent::Error { message, .. } => message.contains("breakdown"),
            _ => false,
        }),
        "and the caller has to be told why the turn settles at its bound: {events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
}

/// `AiEvent::Usage` is an *increment* — `AttemptStream` adds every one it sees
/// into a running total — while this wire's usage chunk is the total for the
/// whole request. A gateway that restates that total on its way out would be
/// billed for the sum of its restatements, which is why exactly one usage
/// event leaves this decoder and a later report replaces the earlier one.
#[test]
fn a_restated_usage_total_is_not_added_to_itself() {
    let chunk = |prompt: u32, completion: u32| {
        format!(
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt},\
             \"completion_tokens\":{completion},\"total_tokens\":{}}}}}\n\n",
            prompt + completion
        )
    };
    let transcript = chunk(100, 10)
        + &chunk(100, 25)
        + "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        + &chunk(100, 40)
        + "data: [DONE]\n\n";
    assert_eq!(
        decode_whole(transcript.as_bytes()),
        vec![
            AiEvent::Usage(Usage {
                uncached_input_tokens: 100,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_extended_tokens: 0,
                output_tokens: 40,
                reasoning_tokens: None,
            }),
            AiEvent::Stop(StopReason::Stop),
        ],
        "three cumulative reports are one figure, not 300 prompt tokens"
    );
}

/// Two calls, their fragments interleaved, arriving out of index order — which
/// is the whole reason assembly keys on `index` and not on arrival. Both
/// argument strings must come back whole and un-spliced.
#[test]
fn tool_call_arguments_are_assembled_by_index_across_interleaved_fragments() {
    assert_eq!(
        decode_whole(TOOL_TRANSCRIPT.as_bytes()),
        vec![
            AiEvent::ToolCall {
                call_id: "call_a".to_string(),
                name: "read_file".to_string(),
                args: "{\"path\":\"src/lib.rs\"}".to_string(),
            },
            AiEvent::ToolCall {
                call_id: "call_b".to_string(),
                name: "glob".to_string(),
                args: "{\"pattern\":\"*.rs\"}".to_string(),
            },
            AiEvent::Usage(Usage {
                uncached_input_tokens: 800,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_extended_tokens: 0,
                output_tokens: 64,
                reasoning_tokens: None,
            }),
            AiEvent::Stop(StopReason::ToolUse),
        ],
        "calls must be emitted in index order, before the usage chunk that \
         follows the finish_reason, and with complete arguments"
    );
}

/// Where a chunk boundary falls is the network's choice, not the provider's,
/// so it may not change a single event. **Every** byte offset in both
/// transcripts is tried: inside `data:`, inside a field name, inside a JSON
/// string, inside the `\n\n` that dispatches a frame, and inside the multi-byte
/// characters the text transcript carries for exactly this reason.
#[test]
fn where_the_bytes_are_split_changes_nothing() {
    for (name, transcript) in [
        ("text", TEXT_TRANSCRIPT.as_bytes()),
        ("tools", TOOL_TRANSCRIPT.as_bytes()),
    ] {
        let whole = decode_whole(transcript);
        for at in 0..=transcript.len() {
            assert_eq!(
                decode_at(transcript, at),
                whole,
                "splitting the {name} transcript at byte {at} changed the events"
            );
        }
    }
}

/// The plan's requirement, stated as the property rather than as one case:
/// **every** ending produces exactly one terminal `Stop`, and truncation is
/// never one of the endings that produces none.
#[test]
fn every_ending_produces_exactly_one_terminal_stop() {
    let finish_reason = |reason: &str| {
        format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}\n\n"
        )
    };
    let endings: Vec<(String, String)> = vec![
        ("a normal stop".to_string(), finish_reason("stop")),
        ("an output ceiling".to_string(), finish_reason("length")),
        ("a tool call".to_string(), finish_reason("tool_calls")),
        (
            "the deprecated function_call spelling".to_string(),
            finish_reason("function_call"),
        ),
        (
            "a content filter".to_string(),
            finish_reason("content_filter"),
        ),
        (
            "an upstream spelling passed through".to_string(),
            finish_reason("end_turn"),
        ),
        (
            "a finish_reason nobody has heard of".to_string(),
            finish_reason("solar_flare"),
        ),
        (
            "two finish_reasons".to_string(),
            finish_reason("stop") + &finish_reason("length"),
        ),
        ("nothing at all".to_string(), String::new()),
        (
            "[DONE] and nothing else".to_string(),
            "data: [DONE]\n\n".to_string(),
        ),
        (
            "a frame cut in half".to_string(),
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par".to_string(),
        ),
        (
            "a frame this format cannot parse".to_string(),
            "data: not json at all\n\n".to_string(),
        ),
    ];
    for (what, transcript) in endings {
        let events = decode_whole(transcript.as_bytes());
        let stops: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, AiEvent::Stop(_)))
            .collect();
        assert_eq!(stops.len(), 1, "{what} produced {stops:?}");
        assert!(
            matches!(events.last(), Some(AiEvent::Stop(_))),
            "{what} left events after the terminal one: {events:?}"
        );
    }
}

#[test]
fn a_finish_reason_maps_to_the_stop_it_names() {
    let cases = [
        ("stop", StopReason::Stop),
        ("length", StopReason::Length),
        ("tool_calls", StopReason::ToolUse),
        ("function_call", StopReason::ToolUse),
        // Not in this format's schema, and it arrives on this format's wire
        // anyway — a gateway passing its upstream's spelling through. Before
        // it was mapped it fell into the unknown arm and ended a perfectly
        // normal turn as an error.
        ("end_turn", StopReason::Stop),
        // No `StopReason` says "a filter stopped this". `Stop` would claim it
        // finished normally, so it is reported as an error that names itself.
        ("content_filter", StopReason::Error),
        ("solar_flare", StopReason::Error),
    ];
    for (reason, expected) in cases {
        let transcript = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}\n\n"
        );
        let events = decode_whole(transcript.as_bytes());
        assert_eq!(
            events.last(),
            Some(&AiEvent::Stop(expected)),
            "{reason} mapped wrongly: {events:?}"
        );
        if expected == StopReason::Error {
            let named = events.iter().any(|event| match event {
                AiEvent::Error { message, .. } => {
                    message.contains(reason) || reason == "content_filter"
                }
                _ => false,
            });
            assert!(
                named,
                "{reason} must be explained, not just failed: {events:?}"
            );
        }
    }
}

/// Half-assembled arguments are not arguments. Dispatching a tool with a
/// truncated JSON string would run it on whatever the fragment happened to
/// parse as; the fragments are dropped, and the error says they were.
#[test]
fn a_tool_call_cut_before_its_finish_reason_is_not_dispatched() {
    let upto = TOOL_TRANSCRIPT
        .find("\"finish_reason\":\"tool_calls\"")
        .expect("the transcript calls tools");
    let events = decode_whole(&TOOL_TRANSCRIPT.as_bytes()[..upto]);

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::ToolCall { .. })),
        "no call may be dispatched from fragments: {events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
    let counted = events.iter().any(|event| match event {
        AiEvent::Error { message, .. } => message.contains("discarded"),
        _ => false,
    });
    assert!(counted, "dropping them must be said out loud: {events:?}");
}

/// A call with no id can never be answered: the tool result message needs a
/// `tool_call_id` to send back, and inventing one produces a request the
/// provider rejects.
#[test]
fn a_tool_call_the_provider_never_named_is_reported_rather_than_invented() {
    const NAMELESS: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":\
         [{\"index\":0,\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\
         \"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
    let events = decode_whole(NAMELESS.as_bytes());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::ToolCall { .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|event| match event {
            AiEvent::Error { message, .. } => message.contains("no id"),
            _ => false,
        }),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));
}

/// An empty-string id is no id. Nothing that speaks this wire is known to
/// send one — Ollama's compat layer, the row this was suspected of, sets the
/// field unconditionally — so this is a fail-closed guard rather than an
/// accommodation. What it prevents is worse than a dropped call: `""` would
/// pass into `ContentBlock::ToolUse` as a legitimate key, and two of them in
/// one turn would read as `DuplicateCallId` — a history error blaming the
/// conversation for a provider's omission.
#[test]
fn an_empty_tool_call_id_is_refused_the_same_way_a_missing_one_is() {
    const EMPTY_ID: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":\
         [{\"index\":0,\"id\":\"\",\"type\":\"function\",\
         \"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\
         \"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
    let events = decode_whole(EMPTY_ID.as_bytes());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::ToolCall { .. })),
        "an unanswerable call must not be dispatched: {events:?}"
    );
    assert!(
        events.iter().any(|event| match event {
            AiEvent::Error { message, .. } => message.contains("no id"),
            _ => false,
        }),
        "and dropping it must be said out loud: {events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));
}

/// Two different ids at one index. Whether the provider meant to correct the
/// id of one call or to start a second one at the same slot cannot be told
/// from the wire — and the two readings produce different argument strings.
///
/// A tool call is not something to run on a guess: it has side effects, and
/// the caller cannot tell a spliced argument string from a real one. So the
/// slot is reported and dropped. Restating the *same* id, by contrast, is what
/// a provider that repeats its metadata looks like, and must not be mistaken
/// for a conflict.
#[test]
fn two_ids_at_one_tool_call_index_are_reported_rather_than_spliced() {
    let fragment = |id: &str, args: &str| {
        format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":\
             [{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\
             \"function\":{{\"name\":\"read_file\",\"arguments\":\"{args}\"}}}}]}},\
             \"finish_reason\":null}}]}}\n\n"
        )
    };
    const END: &str =
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";

    // Restated metadata: one ordinary call, assembled and dispatched.
    let restated = fragment("call_a", "{}") + &fragment("call_a", "") + END;
    assert_eq!(
        decode_whole(restated.as_bytes()),
        vec![
            AiEvent::ToolCall {
                call_id: "call_a".to_string(),
                name: "read_file".to_string(),
                args: "{}".to_string(),
            },
            AiEvent::Stop(StopReason::ToolUse),
        ]
    );

    // Conflicting metadata: nothing runs.
    let conflicting = fragment("call_a", "{}") + &fragment("call_b", "{}") + END;
    let events = decode_whole(conflicting.as_bytes());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::ToolCall { .. })),
        "neither reading may be dispatched: {events:?}"
    );
    assert!(
        events.iter().any(|event| match event {
            AiEvent::Error { message, .. } =>
                message.contains("call_a") && message.contains("call_b"),
            _ => false,
        }),
        "the error must name both ids it could not choose between: {events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));
}

/// A refusal is the assistant's answer arriving on a second channel, with
/// `finish_reason: "stop"` — the turn succeeded. Dropping it would leave the
/// user staring at an empty, successful response.
#[test]
fn a_refusal_is_the_answer_and_not_a_missing_one() {
    const REFUSAL: &str = "data: {\"choices\":[{\"index\":0,\"delta\":\
         {\"refusal\":\"I can't help with that.\"},\"finish_reason\":null}]}\n\n\
         data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n";
    assert_eq!(
        decode_whole(REFUSAL.as_bytes()),
        vec![
            AiEvent::TextDelta {
                text: "I can't help with that.".to_string()
            },
            AiEvent::Stop(StopReason::Stop),
        ]
    );
}

/// The schema has `content`, `refusal`, `tool_calls`, `role` and the
/// deprecated `function_call` in a streaming delta — and no reasoning text at
/// all. Reasoning appears only as a count in `completion_tokens_details`.
///
/// This test asserted the opposite until the shared-shape ADR §4 settled the
/// question the other way, and the sentence it used to carry — "this format
/// guessing at one would be inventing a contract" — was right about *guessing*
/// and wrong about *reading*. Both spellings the family uses are read, for
/// every member, with nothing consulted about who sent them; a member that
/// sends neither is unaffected, which is what makes this uniform rather than a
/// branch. The measured cost of not reading them is in
/// `tests/ollama_reasoning.rs`: 117 of 118 output tokens billed and dropped.
#[test]
fn both_of_the_familys_reasoning_spellings_are_read_and_neither_is_invented() {
    const VENDOR: &str = "data: {\"choices\":[{\"index\":0,\"delta\":\
         {\"reasoning_content\":\"hmm\",\"content\":\"42\"},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n";
    assert_eq!(
        decode_whole(VENDOR.as_bytes()),
        vec![
            AiEvent::ReasoningDelta {
                text: "hmm".to_string()
            },
            AiEvent::TextDelta {
                text: "42".to_string()
            },
            AiEvent::Stop(StopReason::Stop),
        ]
    );

    const OTHER: &str = "data: {\"choices\":[{\"index\":0,\"delta\":\
         {\"reasoning\":\"hmm\",\"content\":\"42\"},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n";
    assert_eq!(
        decode_whole(OTHER.as_bytes()),
        decode_whole(VENDOR.as_bytes()),
        "one concept, two member names, and a reader that treats them \
         differently makes the vendor the contract again"
    );

    // And nothing is invented: a delta with neither member produces no
    // reasoning event at all, which is the half of the old test that was
    // always true.
    const PLAIN: &str = "data: {\"choices\":[{\"index\":0,\"delta\":\
         {\"content\":\"42\"},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n";
    assert!(
        !decode_whole(PLAIN.as_bytes())
            .iter()
            .any(|e| matches!(e, AiEvent::ReasoningDelta { .. })),
        "a delta with no reasoning member produced one anyway"
    );
}

// --- and the same, driven by a real byte stream ---

fn responding(status: u16, body: &'static str) -> Result<HttpResponse, HttpError> {
    let chunks: Vec<Result<bytes::Bytes, HttpError>> = body
        .as_bytes()
        .chunks(7)
        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
        .collect();
    Ok(HttpResponse {
        status,
        headers: vec![
            HttpHeader::new("content-type", b"text/event-stream".to_vec()).expect("header"),
        ],
        body: Box::pin(futures::stream::iter(chunks)),
    })
}

/// A response of any status proves the request arrived, and that reading is
/// taken before anything can cut the body and leave nobody to report it.
#[tokio::test]
async fn a_streamed_response_reports_that_it_arrived_before_it_decodes_anything() {
    let events: Vec<AiEvent> = decode(responding(200, TEXT_TRANSCRIPT), None)
        .collect()
        .await;
    assert_eq!(
        events.first(),
        Some(&AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent))
    );
    assert_eq!(events[1..], decode_whole(TEXT_TRANSCRIPT.as_bytes())[..]);
}

/// An error status is not an event stream. The body is a JSON envelope, the
/// message inside it is what the operator reads, and the status is what the
/// harness acts on.
#[tokio::test]
async fn an_error_status_is_reported_with_what_the_provider_said() {
    const BODY: &str = "{\"error\":{\"message\":\"Rate limit reached for gpt-5.1\",\
         \"type\":\"rate_limit_error\",\"code\":null}}";
    let events: Vec<AiEvent> = decode(responding(429, BODY), None).collect().await;
    assert_eq!(
        events,
        vec![
            AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent),
            AiEvent::Error {
                message: "the provider returned HTTP 429: Rate limit reached for gpt-5.1"
                    .to_string(),
                kind: ErrorKind::Transient,
            },
            AiEvent::ProviderFailure {
                cause: "the provider returned HTTP 429: Rate limit reached for gpt-5.1".to_string(),
                retry_after: None,
            },
            AiEvent::Stop(StopReason::Error),
        ]
    );
}

/// Every status this mapping claims to know, and the classification it
/// claims. Table-driven because the value of the table is that it is
/// exhaustive over what it asserts, and one status per test would let a row
/// go missing without anything noticing.
///
/// `402` and `403` are `Refused` beside `401` and not folded into it: they
/// are three different ways for an account to be turned away and one answer
/// to what a caller should do about any of them.
#[tokio::test]
async fn each_status_is_classified_by_what_it_says_about_retrying() {
    const CASES: &[(u16, ErrorKind)] = &[
        (400, ErrorKind::Invalid),
        (401, ErrorKind::Refused),
        (402, ErrorKind::Refused),
        (403, ErrorKind::Refused),
        (404, ErrorKind::Invalid),
        (408, ErrorKind::Transient),
        (413, ErrorKind::Invalid),
        (422, ErrorKind::Invalid),
        (429, ErrorKind::Transient),
        (500, ErrorKind::Transient),
        (502, ErrorKind::Transient),
        (503, ErrorKind::Transient),
        // Anthropic's overload status, which is a 5xx outside the block the
        // other four sit in — the range arm is what covers it.
        (529, ErrorKind::Transient),
    ];
    for (status, expected) in CASES {
        let events: Vec<AiEvent> = decode(responding(*status, "{}"), None).collect().await;
        let kind = events.iter().find_map(|event| match event {
            AiEvent::Error { kind, .. } => Some(*kind),
            _ => None,
        });
        assert_eq!(kind, Some(*expected), "HTTP {status}");
    }
}

/// A status nobody mapped fails closed. `418` stands in for the real case: a
/// proxy or a gateway inventing a status the provider never returns. Reading
/// it as `Transient` would spend `--retry N` attempts on it and then report a
/// provider outage; `Unclassified` spends one and exits 4.
#[tokio::test]
async fn an_unmapped_status_is_unclassified_rather_than_assumed_retryable() {
    let events: Vec<AiEvent> = decode(responding(418, "not json at all"), None)
        .collect()
        .await;
    assert_eq!(
        events,
        vec![
            AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent),
            AiEvent::Error {
                message: "the provider returned HTTP 418: not json at all".to_string(),
                kind: ErrorKind::Unclassified,
            },
            // A body that is no envelope stays in the operator's message and
            // out of the host-safe cause.
            AiEvent::ProviderFailure {
                cause: "the provider returned HTTP 418".to_string(),
                retry_after: None,
            },
            AiEvent::Stop(StopReason::Error),
        ]
    );
}

/// regression case. OpenRouter wraps an upstream failure as a generic
/// "Provider returned error" and puts the upstream's name and body in
/// `error.metadata`. Dropping them left a live 400 impossible to attribute.
#[tokio::test]
async fn an_openrouter_wrapped_400_names_the_upstream_and_a_bounded_excerpt() {
    let body = r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\"detail\":\"unsupported parameter\"} token sk-or-v1-abc123 extra","provider_name":"Darkbloom"}}}"#;
    let events: Vec<AiEvent> = decode(responding(400, body), None).collect().await;
    let (message, kind) = events
        .iter()
        .find_map(|event| match event {
            AiEvent::Error { message, kind } => Some((message.clone(), *kind)),
            _ => None,
        })
        .expect("an error event");
    assert_eq!(kind, ErrorKind::Invalid);
    assert!(message.starts_with("the provider returned HTTP 400: Provider returned error"));
    assert!(message.contains("Darkbloom"), "{message}");
    assert!(message.contains("unsupported parameter"), "{message}");
    assert!(!message.contains("sk-or-v1-abc123"), "{message}");
    let cause = events
        .iter()
        .find_map(|event| match event {
            AiEvent::ProviderFailure { cause, .. } => Some(cause.clone()),
            _ => None,
        })
        .expect("a provider failure");
    assert!(cause.contains("Darkbloom"), "{cause}");
    assert!(!cause.contains("unsupported parameter"), "{cause}");

    let long = "x".repeat(5000);
    let body = format!(
        r#"{{"error":{{"message":"Provider returned error","metadata":{{"raw":"{long}","provider_name":"P"}}}}}}"#
    );
    let events: Vec<AiEvent> = decode(responding(400, Box::leak(body.into_boxed_str())), None)
        .collect()
        .await;
    let message = events
        .iter()
        .find_map(|event| match event {
            AiEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("an error event");
    assert!(message.len() < 400, "{}", message.len());
}

/// A stream that ended without saying why, after a status that proved the
/// request was accepted. Neither the request nor the account is what went
/// wrong and the same bytes may well complete next time, so it is
/// `Transient` — the reading `Unclassified` would have withheld while
/// claiming to be cautious.
#[test]
fn a_stream_that_stops_without_a_finish_reason_is_transient() {
    const CUT: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par";
    let events = decode_whole(CUT.as_bytes());
    assert_eq!(
        events.iter().find_map(|event| match event {
            AiEvent::Error { kind, .. } => Some(*kind),
            _ => None,
        }),
        Some(ErrorKind::Transient),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}
