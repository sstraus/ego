//! The `gemini-interactions` wire format, against transcripts it did not write.
//!
//! # Why this is a third mapping
//!
//! `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §4
//! allows a provider to differ in exactly three places. Gemini's Interactions
//! API differs in the request's *shape* and the stream's *event model*, like
//! Anthropic — but it differs from Anthropic too, which is the point of having
//! it: two mappings could be a special case, three is a seam.
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
//! Hand-assembled from `docs/14-six-wire-protocols.md` §3.1-§3.8, which reads
//! Google's documentation and the request types in the **released** v2.19.0 of
//! `googleapis/python-genai` — `[D]+[S]`, never `[R]`. No `GEMINI_API_KEY`
//! exists in this repository, so nothing here has been observed against a live
//! server. §3.7's curl is what would upgrade it.
//!
//! Fields this mapping ignores are present in the transcripts on purpose
//! (`service_tier`, `object`, `created`, `input_tokens_by_modality`,
//! `interaction.status_update`, `step_usage`), so that ignoring them is tested
//! rather than assumed.

use ego_ai::http::{HttpError, HttpResponse};
use ego_ai::wire::RequestTarget;
use ego_ai::wire::gemini_interactions::{
    DIALECT, Inexpressible, StreamDecoder, build_request, request_body,
};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, Message, ReasoningState, Role, StopReason,
    ThinkingLevel, ToolDescriptor,
};
use ego_core::state::Usage;
use url::Url;

const TEXT_TRANSCRIPT: &str = include_str!("goldens/profile/gemini/stream/text.sse");
const TOOL_TRANSCRIPT: &str = include_str!("goldens/profile/gemini/stream/tool_calls.sse");

fn conversation(messages: Vec<Message>) -> Conversation {
    Conversation::new(messages).expect("the test histories here are valid")
}

/// A request with every optional field exercised at once, so a field that
/// silently stops being sent has nowhere to hide.
fn wide_request() -> GenRequest {
    GenRequest {
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

/// The system prompt is a top-level **string**, and no step carries it.
///
/// A string and not an object: `CreateModelInteractionParam.system_instruction`
/// is `str` (docs/14 §3.8, [S]). Asserting only that it is set would pass a
/// mapping that also left a system turn in `input`.
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

/// A conversation is steps, not roles — and the assistant's turn is a
/// *request-side* `model_output` step.
///
/// This is the difference that no amount of reading the response shape would
/// have settled: `model_output` is in `CreateModelInteractionParam`'s own
/// input union, so replaying a prior answer is the documented ABI rather than
/// the response shape assumed to travel back (docs/14 §3.8).
#[test]
fn a_prior_answer_replays_as_a_model_output_step() {
    let req = GenRequest {
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

/// A tool call replays with its arguments as an **object**, and its result as
/// a `function_result` step carrying the flag that says it failed.
///
/// `arguments` is `Dict[str, Any]` here and a JSON *string* on
/// chat-completions ([S], docs/14 §3.8). "Arguments are a string" was the
/// family's fact, never a shared one.
#[test]
fn a_tool_call_and_its_result_are_two_steps_and_the_arguments_are_an_object() {
    let req = GenRequest {
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

/// Reasoning state replays as a `thought` step, byte for byte.
///
/// Google's wording is as hard as Anthropic's: "You **MUST** always resend all
/// `thought` blocks exactly as they were received". A re-serialised blob is
/// not the same bytes, so the blob travels verbatim.
#[test]
fn reasoning_state_is_replayed_exactly_as_the_provider_minted_it() {
    let blob = r#"{"type":"thought","signature":"EpoGCpcGAdHtim9","summary":[{"type":"text","text":"short"}]}"#;
    let req = GenRequest {
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

/// `Off` has no spelling here, so it is refused and the refusal names the four
/// that work.
///
/// The cost `docs/14` §3.6 accepted with its eyes open. Omitting the field
/// does not substitute: Gemini models think by default, so an omission would
/// bill a caller for reasoning they switched off.
#[test]
fn off_is_refused_because_this_api_cannot_switch_thinking_off() {
    let refusal = request_body(&GenRequest {
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

/// The URL asks for SSE framing, and the row's credential header survives.
#[test]
fn the_stream_is_asked_for_in_the_query_as_well_as_in_the_body() {
    let base = Url::parse("https://generativelanguage.googleapis.com/v1beta/")
        .expect("a base this test controls");
    let headers = vec![("x-goog-api-key".to_string(), "k".to_string())];
    let target = RequestTarget::new(&base, &headers);
    let built = build_request(&target, &wide_request()).expect("this request is expressible");

    assert_eq!(
        built.url.as_str(),
        "https://generativelanguage.googleapis.com/v1beta/interactions?alt=sse",
        "?alt=sse is how this API frames a stream as SSE"
    );
    let names: Vec<&str> = built.headers.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"x-goog-api-key"), "{names:?}");
    assert!(names.contains(&"content-type"), "{names:?}");
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
                blob: r#"{"type":"thought","signature":"EpoGCpcGAdHtim9"}"#.to_string(),
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

/// `step.stop` restates the running total, and restating is not spending.
///
/// docs/14 §3.3 names this trap outright: "Emitting the running total as an
/// `AiEvent::Usage` on each step multiplies the bill by the number of steps."
/// The golden carries a `usage` on a `step.stop` *and* on
/// `interaction.completed` so a decoder that reported both would fail here.
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

/// Thought tokens are a separate summand, and tool-use tokens are input.
///
/// docs/14 §3.3 checks the arithmetic against Google's own example:
/// `11 + 90 + 245 + 0 = 346 = total_tokens`. So `total_thought_tokens` is
/// **not** a subset of `total_output_tokens`, and an adapter that treated it
/// as one would under-report every reasoning turn.
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

/// An in-band `error` event ends the generation with what the provider said.
///
/// `code` is a string slug here, not a number ([D], docs/14 §3.5).
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

/// There is no `[DONE]` sentinel and no finish reason, so a stream that simply
/// stops was cut — and that is an error, not a short answer.
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

/// A transport failure mid-stream is reported with the certainty the transport
/// read, not with one this decoder invented.
#[test]
fn a_transport_failure_keeps_what_the_transport_observed() {
    let cut = TEXT_TRANSCRIPT
        .split_once("event: interaction.completed")
        .expect("the golden has one")
        .0;
    let mut decoder = StreamDecoder::default();
    let mut events = decoder.push(cut.as_bytes());
    events.extend(decoder.finish(Some(HttpError::Transport {
        message: "connection reset".to_string(),
        delivery: DeliveryCertainty::PossiblySent,
    })));

    assert!(
        events.iter().any(|event| matches!(
            event,
            AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent)
        )),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}

/// Once the interaction has reported its status the turn is complete, and a
/// transport that dies afterwards is not the turn failing.
#[test]
fn a_cut_after_the_status_is_a_complete_turn() {
    let mut decoder = StreamDecoder::default();
    let mut events = decoder.push(TEXT_TRANSCRIPT.as_bytes());
    events.extend(decoder.finish(Some(HttpError::Transport {
        message: "connection reset".to_string(),
        delivery: DeliveryCertainty::PossiblySent,
    })));

    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::Error { .. })),
        "the interaction completed; the transport failing afterwards is not the \
         turn failing: {events:?}"
    );
}

/// A non-200 is decoded from the body the provider sent with it.
#[tokio::test]
async fn a_refused_request_relays_the_sentence_the_provider_wrote() {
    use futures::StreamExt;

    let body = include_str!("goldens/profile/gemini/error/404.json");
    let response = Ok(HttpResponse {
        status: 404,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
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
