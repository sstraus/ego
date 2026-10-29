//! The `anthropic-messages` wire format, against transcripts it did not write.
//!
//! # Why this is a second mapping and not a fourth profile
//!
//! Anthropic's request shape and event model require a separate mapping.
//! The following tests assert the structural differences:
//!
//! - the system prompt is a top-level parameter, so there is no `role:
//!   "system"` message to build;
//! - a tool result is a content block inside a **user** message, not a message
//!   with a role of its own;
//! - `max_tokens` is required, and `GenRequest::max_output_tokens` is an
//!   `Option` — so a request with no ceiling is refused rather than given one
//!   this mapping invented.
//!
//! # What the goldens are, and what they are worth
//!
//! Synthetic transcripts assembled from the published Messages API and streaming
//! specification. These are contract fixtures, not live-server evidence.
//!
//! Fields this mapping ignores are present in the transcripts on purpose
//! (`service_tier`, `inference_geo`, `ping`, `stop_sequence`), so that
//! ignoring them is tested rather than assumed.

use ego_ai::http::HttpResponse;
use ego_ai::wire::RequestTarget;
use ego_ai::wire::anthropic_messages::{
    DIALECT, Inexpressible, StreamDecoder, VERSION, VERSION_HEADER, build_request, request_body,
};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, Message, ReasoningState, Role, StopReason,
    ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use ego_core::state::Usage;
use url::Url;

const TEXT_TRANSCRIPT: &str = include_str!("goldens/profile/anthropic/stream/text.sse");
const TOOL_TRANSCRIPT: &str = include_str!("goldens/profile/anthropic/stream/tool_calls.sse");

fn conversation(messages: Vec<Message>) -> Conversation {
    Conversation::new(messages).expect("the test histories here are valid")
}

/// A request with every optional field exercised at once, so a field that
/// silently stops being sent has nowhere to hide.
fn wide_request() -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "claude-opus-5".to_string(),
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

// --- the request: the three structural differences ---

/// The system prompt is a top-level parameter, and no message carries it.
///
/// Both halves. Asserting only that `system` is set would pass a mapping that
/// *also* left a `role: "system"` message in the array — which is a `400`, and
/// one the family's projection would produce if it were reused here.
#[test]
fn the_system_prompt_is_a_top_level_parameter_and_not_a_message() {
    let body = body_of(&wide_request());

    assert_eq!(body["system"], serde_json::json!("You are terse."));
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .map(|message| message["role"].as_str().expect("a role is a string"))
        .collect();
    assert_eq!(
        roles,
        vec!["user"],
        "the system turn must not also appear as a message"
    );
}

/// Several system turns become one system prompt, in order.
///
/// The Messages API has one slot for it, so a history carrying two is a
/// question this mapping has to answer rather than a case that cannot arise:
/// `Conversation` permits it and the turn loop can build it.
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
        body_of(&req)["system"],
        serde_json::json!("You are terse.\nAnswer in Italian."),
    );
}

/// A tool result is a block inside a user message, and the call it answers is
/// a block inside the assistant's.
///
/// This is the difference that cost the family its projection: chat-completions
/// gives a result its own `role: "tool"` message, and there is no such role
/// here. `wire_messages`'s own doc in `openai_completions.rs` anticipated it —
/// "this format has a role for it where anthropic-messages has a block".
#[test]
fn a_tool_result_is_a_block_inside_a_user_turn() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "What is in ./src?"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    call_id: "toolu_01T1xRead".to_string(),
                    name: "read_file".to_string(),
                    args: r#"{"path": "./src"}"#.to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "toolu_01T1xRead".to_string(),
                    output: "main.rs lib.rs".to_string(),
                    is_error: false,
                }],
            },
        ]),
        ..wide_request()
    };

    assert_eq!(
        body_of(&req)["messages"],
        serde_json::json!([
            {"role": "user", "content": [{"type": "text", "text": "What is in ./src?"}]},
            {"role": "assistant", "content": [{
                "type": "tool_use",
                "id": "toolu_01T1xRead",
                "name": "read_file",
                // An object, not the JSON *string* chat-completions asks for,
                // and passed through verbatim rather than re-serialised.
                "input": {"path": "./src"},
            }]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01T1xRead",
                "content": "main.rs lib.rs",
                // Stated, not left to the default. `ContentBlock::ToolResult`
                // carries a definite boolean, and spelling `false` as absence
                // would make this body's meaning depend on Anthropic's default
                // staying `false` — and on presence carrying information the
                // value already carries.
                "is_error": false,
            }]},
        ])
    );
}

/// A tool that failed says so, under the name this API reads it by. The
/// successful case above states `false` for the same reason: the flag is always
/// present, so its value is the whole of what it says.
#[test]
fn a_failed_tool_result_carries_the_flag_that_says_so() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "read it"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    call_id: "toolu_01T1xRead".to_string(),
                    name: "read_file".to_string(),
                    args: "{}".to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "toolu_01T1xRead".to_string(),
                    output: "no such file".to_string(),
                    is_error: true,
                }],
            },
        ]),
        ..wide_request()
    };

    assert_eq!(
        body_of(&req)["messages"][2]["content"][0]["is_error"],
        serde_json::json!(true),
    );
}

/// No ceiling is a refusal, not a number this mapping picked.
///
/// The Messages API requires `max_tokens`; `GenRequest::max_output_tokens`
/// is optional. Inventing a default would silently choose a billing limit.
#[test]
fn a_request_with_no_ceiling_is_refused_rather_than_given_one() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        max_output_tokens: None,
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("the API requires a ceiling"),
        Inexpressible::CeilingRequired,
    );
}

#[test]
fn the_ceiling_is_sent_under_the_name_the_api_reads() {
    assert_eq!(
        body_of(&wide_request())["max_tokens"],
        serde_json::json!(4096)
    );
}

// --- the request: thinking is an effort level ---

/// Five of `ThinkingLevel`'s seven are spelled identically by Anthropic, so
/// nothing is approximated.
///
/// `budget_tokens` is not consulted and not sent: it returns a `400` on every
/// current model (`AGENTS.md`, current models only), and its replacement is
/// this pair of fields.
#[test]
fn an_effort_level_is_sent_verbatim_beside_adaptive_thinking() {
    for (level, spelled) in [
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ] {
        let body = body_of(&GenRequest {
            web_search: None,
            cache: None,
            thinking: level,
            ..wide_request()
        });
        assert_eq!(
            body["output_config"],
            serde_json::json!({"effort": spelled}),
            "{level:?} is spelled {spelled} by the vendor and must travel verbatim"
        );
        assert_eq!(
            body["thinking"],
            // `display` is explicit because its default is `omitted`, which
            // streams thinking blocks whose text is empty — a silent default
            // that would make `AiEvent::ReasoningDelta` a producer of nothing.
            serde_json::json!({"type": "adaptive", "display": "summarized"}),
        );
        assert!(
            !body.to_string().contains("budget_tokens"),
            "budget_tokens is a 400 on every current model"
        );
    }
}

/// `Off` is a different field, not an effort value.
///
/// The vendor is explicit that `adaptive` is a thinking *mode* and must not be
/// passed as an effort. No `output_config` is sent with it either: an explicit
/// `disabled` is accepted only at effort `high` or below, and the default is
/// `high`, so saying nothing is the one spelling that is in range whatever the
/// caller's model.
#[test]
fn off_disables_thinking_and_states_no_effort_at_all() {
    let body = body_of(&GenRequest {
        web_search: None,
        cache: None,
        thinking: ThinkingLevel::Off,
        ..wide_request()
    });

    assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
    assert_eq!(
        body.get("output_config"),
        None,
        "an explicit effort above `high` turns a disabled thinking block into a 400"
    );
}

/// `Minimal` is not a level Anthropic publishes, so it is refused by name.
///
/// The fourth time the three-state `ReasoningEffortSupport` design has paid
/// for itself. The refusal names what does work, because an operator can act
/// on that and cannot act on "unsupported".
#[test]
fn minimal_is_refused_and_the_refusal_names_the_six_that_work() {
    let refused = request_body(&GenRequest {
        web_search: None,
        cache: None,
        thinking: ThinkingLevel::Minimal,
        ..wide_request()
    })
    .expect_err("anthropic publishes no minimal");

    assert_eq!(
        refused,
        Inexpressible::ThinkingLevelUnavailable {
            level: ThinkingLevel::Minimal
        }
    );
    let sentence = refused.to_string();
    for offered in ["off", "low", "medium", "high", "xhigh", "max"] {
        assert!(
            sentence.contains(offered),
            "the refusal must name {offered}: {sentence}"
        );
    }
}

// --- the request: tools ---

#[test]
fn a_tool_is_declared_with_the_schema_field_this_api_names() {
    assert_eq!(
        body_of(&wide_request())["tools"],
        serde_json::json!([{
            "name": "read_file",
            "description": "Read a file from the workspace.",
            // `input_schema`, not chat-completions' `function.parameters`.
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            },
        }])
    );
}

/// Declining parallel calls is spelled on `tool_choice`, and only ever sent
/// when there is something to decline.
///
/// Anthropic's default is to allow them, so the field is sent only to turn
/// them off — sending `tool_choice` on a request with no tools is a parameter
/// about nothing.
#[test]
fn declining_parallel_calls_is_spelled_on_tool_choice() {
    let allowed = body_of(&wide_request());
    assert_eq!(
        allowed.get("tool_choice"),
        None,
        "allowing them is the vendor's default and needs no field"
    );

    let declined = body_of(&GenRequest {
        web_search: None,
        cache: None,
        parallel_tool_calls: false,
        ..wide_request()
    });
    assert_eq!(
        declined["tool_choice"],
        serde_json::json!({"type": "auto", "disable_parallel_tool_use": true}),
    );

    let toolless = body_of(&GenRequest {
        web_search: None,
        cache: None,
        tools: Vec::new(),
        parallel_tool_calls: false,
        ..wide_request()
    });
    assert_eq!(
        toolless.get("tool_choice"),
        None,
        "there is nothing to decline when no tool was offered"
    );
    assert_eq!(toolless.get("tools"), None);
}

// --- the request: reasoning state is replayed, not dropped ---

/// The provider's own thinking block goes back exactly as it arrived.
///
/// Not a nicety: sending back a modified, reordered, filtered or reconstructed
/// `thinking` block is a documented `400`, "including blocks whose `thinking`
/// field is empty". This is the property that makes an Anthropic row able to
/// use thinking *with tools* at all — its second generation replays the first
/// one's blocks or it fails.
///
/// The blob is the whole block and not just the signature, which is why the
/// round trip below is byte-exact. `ReasoningState::blob` is opaque to
/// `ego-core` by contract, so the mapping that mints it chooses what it holds,
/// and the only choice that satisfies "exactly as received" is all of it.
#[test]
fn reasoning_state_is_replayed_exactly_as_the_provider_minted_it() {
    let block = r#"{"type":"thinking","thinking":"Due file, quindi.","signature":"EpoGCpcGCkgIBRABGAIiQL2f"}"#;
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "What is in ./src?"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Reasoning {
                        state: ReasoningState {
                            blob: block.to_string(),
                            dialect: DIALECT.to_string(),
                        },
                    },
                    ContentBlock::Text {
                        text: "Due file.".to_string(),
                    },
                ],
            },
            Message::text(Role::User, "E in ./tests?"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        body_of(&req)["messages"][1],
        serde_json::json!({
            "role": "assistant",
            "content": [
                serde_json::from_str::<serde_json::Value>(block).expect("the block is JSON"),
                {"type": "text", "text": "Due file."},
            ],
        }),
        "the block must survive verbatim, and must still precede the text it led to"
    );
}

/// State another provider minted is refused rather than sent.
///
/// This is what `ReasoningState::dialect` is for. Without the check the blob
/// would be posted into a `content` array that cannot hold it, and the reply
/// would be a `400` naming a field the operator never set.
#[test]
fn reasoning_state_from_another_dialect_is_refused_rather_than_sent() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "hi"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning {
                    state: ReasoningState {
                        blob: "{\"kind\":\"thought\"}".to_string(),
                        dialect: "gemini-interactions".to_string(),
                    },
                }],
            },
            Message::text(Role::User, "hi again"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("that state was minted elsewhere"),
        Inexpressible::ReasoningStateFromAnotherDialect {
            dialect: "gemini-interactions".to_string()
        },
    );
}

/// A blob that is not JSON is refused, not posted.
///
/// Unreachable from a stream this mapping decoded, and reachable from a
/// durable log that was truncated or corrupted — which is a real way for a
/// resumed session to arrive here.
#[test]
fn reasoning_state_that_is_not_json_is_refused() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "hi"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning {
                    state: ReasoningState {
                        blob: r#"{"type":"thinking","thin"#.to_string(),
                        dialect: DIALECT.to_string(),
                    },
                }],
            },
            Message::text(Role::User, "hi again"),
        ]),
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("a half-written block is not a block"),
        Inexpressible::ReasoningStateNotJson,
    );
}

/// Tool arguments that are not JSON are refused rather than posted.
///
/// `ContentBlock::ToolUse::args` is a `String` precisely so that a call
/// truncated mid-stream survives as evidence; `tool_use.input` is an object,
/// so this is the seam where that evidence stops being sendable.
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
                    call_id: "toolu_01T1xRead".to_string(),
                    name: "read_file".to_string(),
                    args: r#"{"path": "./sr"#.to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "toolu_01T1xRead".to_string(),
                    output: "cut".to_string(),
                    is_error: true,
                }],
            },
        ]),
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("a truncated call is not an object"),
        Inexpressible::ArgumentsNotJson {
            call_id: "toolu_01T1xRead".to_string()
        },
    );
}

/// The system slot holds text and nothing else.
///
/// `Conversation` permits a tool result in a system turn — only the assistant
/// is forbidden from answering — and there is nowhere in a top-level `system`
/// parameter for one to go. Dropping it silently would break the call/result
/// pairing the provider then rejects.
#[test]
fn a_system_turn_that_is_not_text_is_refused() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        messages: conversation(vec![
            Message::text(Role::User, "go"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    call_id: "toolu_01T1xRead".to_string(),
                    name: "read_file".to_string(),
                    args: "{}".to_string(),
                }],
            },
            Message {
                role: Role::System,
                content: vec![ContentBlock::ToolResult {
                    call_id: "toolu_01T1xRead".to_string(),
                    output: "main.rs".to_string(),
                    is_error: false,
                }],
            },
        ]),
        ..wide_request()
    };

    assert_eq!(
        request_body(&req).expect_err("the system slot is text"),
        Inexpressible::SystemTurnIsTextOnly,
    );
}

// --- the request: the endpoint and its headers ---

/// The version header is the *mapping's*, like `content-type`, and it travels
/// with every request.
///
/// Not the row's: `anthropic-version` is a fact about the format rather than
/// about a vendor's deployment, and a row that carried it would be a row that
/// could disagree with its own mapping about which contract it speaks.
#[test]
fn every_request_carries_the_version_this_mapping_was_written_against() {
    let base = Url::parse("https://api.anthropic.com/v1/").expect("a valid base");
    let headers = vec![HttpHeader::new("x-api-key", b"sk-test".to_vec()).unwrap()];
    let target = RequestTarget::new(&base, &headers);
    let req = build_request(&target, &wide_request()).expect("this request is expressible");

    assert_eq!(req.url.as_str(), "https://api.anthropic.com/v1/messages");
    let header = |name: &str| {
        req.headers
            .iter()
            .find(|header| header.name() == name)
            .and_then(|header| std::str::from_utf8(header.value()).ok())
    };
    assert_eq!(header(VERSION_HEADER), Some(VERSION));
    assert_eq!(header("content-type"), Some("application/json"));
    assert_eq!(header("accept"), Some("text/event-stream"));
    assert_eq!(
        header("x-api-key"),
        Some("sk-test"),
        "the provider row's headers must survive"
    );
}

#[test]
fn the_stream_is_asked_for_in_the_body_as_well_as_in_the_accept_header() {
    assert_eq!(body_of(&wide_request())["stream"], serde_json::json!(true));
}

// --- the response ---

/// Drives the decoder over a transcript split at `at`.
///
/// Indexes **bytes**, not characters: a chunk boundary is a network event and
/// lands wherever it lands, including inside a multi-byte character. The text
/// transcript carries an accented word and an emoji so that is a real case.
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
            AiEvent::ReasoningDelta {
                text: "Due file, ".to_string()
            },
            AiEvent::ReasoningDelta {
                text: "quindi.".to_string()
            },
            AiEvent::ReasoningState(ReasoningState {
                blob: r#"{"type":"thinking","thinking":"Due file, quindi.","signature":"EpoGCpcGCkgIBRABGAIiQL2f"}"#.to_string(),
                dialect: DIALECT.to_string(),
            }),
            AiEvent::TextDelta {
                text: "Due ".to_string()
            },
            AiEvent::TextDelta {
                text: "file — perché no? 😀".to_string()
            },
            AiEvent::Usage(Usage {
                // Anthropic's partition runs the other way from OpenAI's:
                // `input_tokens` is *already* the uncached figure, so it is
                // passed through rather than reduced by the cache numbers.
                uncached_input_tokens: 500,
                cache_read_tokens: 400,
                cache_write_tokens: 60,
                cache_write_extended_tokens: 40,
                output_tokens: 250,
                web_search_requests: 0,
                reasoning_tokens: None,
            }),
            AiEvent::Stop(StopReason::Stop),
        ]
    );
}

/// The transcript decodes the same however the bytes are cut.
#[test]
fn a_transcript_decodes_the_same_at_every_chunk_boundary() {
    let transcript = TEXT_TRANSCRIPT.as_bytes();
    let whole = decode_whole(transcript);
    for at in 0..=transcript.len() {
        assert_eq!(
            decode_at(transcript, at),
            whole,
            "a cut at byte {at} changed the decoding"
        );
    }
}

/// The usage figures are the provider's last word, not the sum of its
/// restatements.
///
/// `message_delta.usage` is **cumulative** and there is more than one of them
/// in the golden — 120 then 250. A decoder that added them would report 370 and
/// bill the caller for a number the provider never stated. This is the
/// double-count `AiEvent::Usage`'s own doc warns three of the six wires can
/// cause, made falsifiable.
#[test]
fn a_restated_running_total_is_not_added_to_the_one_it_restates() {
    let usages: Vec<Usage> = decode_whole(TEXT_TRANSCRIPT.as_bytes())
        .into_iter()
        .filter_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect();

    match usages.as_slice() {
        [only] => assert_eq!(only.output_tokens, 250, "the last figure, not 120 + 250"),
        other => panic!("usage was reported {} times: {other:?}", other.len()),
    }
}

/// A cache-write figure with no TTL split is all standard retention.
///
/// The tool transcript reports `cache_creation_input_tokens` and no
/// `cache_creation` object, which is what a request that asked for no extended
/// retention gets back. Zero is that request's measurement, not an absent
/// figure.
#[test]
fn a_cache_write_with_no_split_is_all_at_standard_retention() {
    let usage = decode_whole(TOOL_TRANSCRIPT.as_bytes())
        .into_iter()
        .find_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("the transcript reports usage");

    assert_eq!(
        usage,
        Usage {
            uncached_input_tokens: 700,
            cache_read_tokens: 200,
            cache_write_tokens: 100,
            cache_write_extended_tokens: 0,
            output_tokens: 96,
            web_search_requests: 0,
            reasoning_tokens: None,
        }
    );
}

/// Both calls arrive whole, with the arguments assembled from their fragments.
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
                "toolu_01T1xRead".to_string(),
                "read_file".to_string(),
                r#"{"path": "./src/main.rs"}"#.to_string(),
            ),
            (
                "toolu_01T2yRead".to_string(),
                "read_file".to_string(),
                r#"{"path": "./src/lib.rs"}"#.to_string(),
            ),
        ]
    );
}

/// A turn that ended to call a tool says so, once and last.
#[test]
fn a_tool_transcript_ends_as_tool_use() {
    let events = decode_whole(TOOL_TRANSCRIPT.as_bytes());
    assert!(matches!(
        events.last(),
        Some(AiEvent::Stop(StopReason::ToolUse))
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AiEvent::Stop(_)))
            .count(),
        1
    );
}

/// Every stop reason this protocol publishes, and what it becomes.
///
/// `pause_turn` resumes the provider loop without a local tool dispatch.
/// Unknown reasons still fail closed and name the unsupported discriminator.
#[test]
fn each_stop_reason_becomes_what_it_means_or_says_it_cannot() {
    for (spelled, expected) in [
        ("end_turn", StopReason::Stop),
        ("stop_sequence", StopReason::Stop),
        ("max_tokens", StopReason::Length),
        ("tool_use", StopReason::ToolUse),
        ("refusal", StopReason::Error),
        ("model_context_window_exceeded", StopReason::Error),
        ("pause_turn", StopReason::Continue),
        (
            "a_reason_invented_after_this_was_written",
            StopReason::Error,
        ),
    ] {
        let transcript = format!(
            "event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{spelled}\"}},\"usage\":{{\"output_tokens\":3}}}}\n\
             \n\
             event: message_stop\n\
             data: {{\"type\":\"message_stop\"}}\n\n"
        );
        let events = decode_whole(transcript.as_bytes());
        assert_eq!(
            events.last(),
            Some(&AiEvent::Stop(expected)),
            "{spelled} decoded to {events:?}"
        );
    }
}

/// A stop reason with no meaning here is reported rather than guessed at.
#[test]
fn an_unmappable_stop_reason_says_which_one_it_was() {
    let transcript = "event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"future_stop_reason\"},\"usage\":{\"output_tokens\":3}}\n\
         \n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n\n";

    let said = decode_whole(transcript.as_bytes())
        .into_iter()
        .find_map(|event| match event {
            AiEvent::Error { message, .. } => Some(message),
            _ => None,
        })
        .expect("an unmappable reason is reported");
    assert!(
        said.contains("future_stop_reason"),
        "the operator must learn which reason it was: {said}"
    );
}

/// An in-band error, after a 200 has already been committed.
#[test]
fn an_error_event_ends_the_generation_with_what_the_provider_said() {
    let transcript = "event: error\n\
         data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";

    let events = decode_whole(transcript.as_bytes());
    assert!(
        events.iter().any(|event| matches!(
            event,
            AiEvent::Error { message, kind }
                if message.contains("Overloaded") && *kind == ErrorKind::Transient
        )),
        "an overloaded provider is retryable, and it said so in `error.type`: {events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}

/// Retryability is in `error.type`, not in the prose.
#[test]
fn an_error_types_retryability_is_read_from_the_type_the_provider_sent() {
    for (spelled, kind) in [
        ("overloaded_error", ErrorKind::Transient),
        ("rate_limit_error", ErrorKind::Transient),
        ("api_error", ErrorKind::Transient),
        ("timeout_error", ErrorKind::Transient),
        ("invalid_request_error", ErrorKind::Invalid),
        ("request_too_large", ErrorKind::Invalid),
        ("not_found_error", ErrorKind::Invalid),
        ("authentication_error", ErrorKind::Refused),
        ("permission_error", ErrorKind::Refused),
        ("billing_error", ErrorKind::Refused),
        ("a_type_the_docs_say_will_be_added", ErrorKind::Unclassified),
    ] {
        let transcript = format!(
            "event: error\n\
             data: {{\"type\":\"error\",\"error\":{{\"type\":\"{spelled}\",\"message\":\"m\"}}}}\n\n"
        );
        let kinds: Vec<ErrorKind> = decode_whole(transcript.as_bytes())
            .into_iter()
            .filter_map(|event| match event {
                AiEvent::Error { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec![kind], "{spelled} was classified wrongly");
    }
}

/// There is no `[DONE]` sentinel, so a stream that simply stops is a stream
/// that was cut — and that is an error, not a clean end.
#[test]
fn a_stream_that_ends_before_message_stop_is_an_error() {
    let cut = TEXT_TRANSCRIPT
        .split_once("event: message_delta")
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

    let body = include_str!("goldens/profile/anthropic/error/404.json");
    let response = Ok(HttpResponse {
        status: 404,
        headers: vec![
            HttpHeader::new("content-type", b"application/json".to_vec()).expect("header"),
        ],
        body: Box::pin(futures::stream::once(async move {
            Ok(bytes::Bytes::from(body.as_bytes()))
        })),
    });

    let events: Vec<AiEvent> = ego_ai::wire::anthropic_messages::decode(response, None)
        .collect()
        .await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            AiEvent::Error { message, kind }
                if message.contains("claude-opus-5-turbo") && *kind == ErrorKind::Invalid
        )),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}
