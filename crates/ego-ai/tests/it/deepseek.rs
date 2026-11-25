//! The DeepSeek Chat mapping against its schema-derived fixtures.
//!
//! Fixtures and the bug each one catches: `goldens/profile/deepseek/README.md`.

use ego_ai::http::{HttpError, HttpResponse};
use ego_ai::provider::{Credentials, DEEPSEEK_KEY_VAR, MissingCredential, ProviderRow, Secret};
use ego_ai::wire::deepseek::{DEEPSEEK_REASONING_DIALECT, DeepSeek, request_body};
use ego_ai::wire::openai_completions::OPENROUTER_REASONING_DIALECT;
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, HostedSearch, Message, ReasoningState, Role,
    StopReason, ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use futures::StreamExt;
use url::Url;

const REASONING_TOOL_CALL: &str = include_str!("../goldens/profile/deepseek/stream/tool_calls.sse");
const INSUFFICIENT_RESOURCE: &str =
    include_str!("../goldens/profile/deepseek/stream/insufficient_resource.sse");
const ABORTED: &str = include_str!("../goldens/profile/deepseek/stream/aborted.sse");
const TEXT: &str = include_str!("../goldens/profile/deepseek/stream/text.sse");

fn golden(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/tests/goldens/profile/deepseek/request/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).expect("the golden exists"))
        .expect("the golden is JSON")
}

fn body(req: &GenRequest) -> serde_json::Value {
    serde_json::from_slice(&request_body(req).expect("DeepSeek expresses this request"))
        .expect("the body is JSON")
}

fn tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "get_date".to_string(),
            description: "Get the current date".to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDescriptor {
            name: "get_weather".to_string(),
            description: "Get weather of a location, the user should supply the location and date."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string", "description": "The city name" },
                    "date": { "type": "string", "description": "The date in format YYYY-mm-dd" }
                },
                "required": ["location", "date"]
            }),
        },
    ]
}

fn request(messages: Vec<Message>) -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "deepseek-flash".to_string(),
        messages: Conversation::new(messages).expect("the test histories are valid"),
        tools: tools(),
        thinking: ThinkingLevel::High,
        max_output_tokens: Some(4096),
        // Asked for, and still not sent: the schema has no such field.
        parallel_tool_calls: true,
    }
}

fn reasoning(blob: &str, dialect: &str) -> ContentBlock {
    ContentBlock::Reasoning {
        state: ReasoningState {
            connection: None,
            blob: blob.to_string(),
            dialect: dialect.to_string(),
        },
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_string(),
    }
}

fn call(call_id: &str, name: &str, args: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        call_id: call_id.to_string(),
        name: name.to_string(),
        args: args.to_string(),
    }
}

fn result(call_id: &str, output: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            call_id: call_id.to_string(),
            output: output.to_string(),
            is_error: false,
        }],
    }
}

fn assistant(content: Vec<ContentBlock>) -> Message {
    Message {
        role: Role::Assistant,
        content,
    }
}

fn responding(body: &str) -> Result<HttpResponse, HttpError> {
    answering(200, "text/event-stream", body)
}

fn answering(status: u16, content_type: &str, body: &str) -> Result<HttpResponse, HttpError> {
    let chunks: Vec<Result<bytes::Bytes, HttpError>> = body
        .as_bytes()
        .chunks(7)
        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
        .collect();
    Ok(HttpResponse {
        status,
        headers: vec![
            HttpHeader::new("content-type", content_type.as_bytes().to_vec()).expect("header"),
        ],
        body: Box::pin(futures::stream::iter(chunks)),
    })
}

fn events(transcript: &str) -> Vec<AiEvent> {
    futures::executor::block_on(DeepSeek.decode(responding(transcript), None).collect())
}

fn error_kinds(events: &[AiEvent]) -> Vec<ErrorKind> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::Error { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn usages(events: &[AiEvent]) -> Vec<ego_core::state::Usage> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .collect()
}

/// Catches: the ceiling sent as `max_completion_tokens`, which DeepSeek does
/// not list, so the output cap is lost; `parallel_tool_calls` sent outside the
/// schema; the thinking switch missing.
#[test]
fn a_reasoning_tool_request_carries_deepseeks_fields_and_none_outside_its_schema() {
    let req = request(vec![Message::text(
        Role::User,
        "How's the weather in Hangzhou Tomorrow",
    )]);
    assert_eq!(body(&req), golden("reasoning_tools.json"));
}

/// Catches: off sending no switch, so DeepSeek's default (thinking enabled,
/// effort `high`) stays active and bills reasoning tokens the caller refused.
#[test]
fn thinking_off_sends_the_disabled_switch_and_no_effort() {
    let req = GenRequest {
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(64),
        ..request(vec![Message::text(
            Role::User,
            "9.11 and 9.8, which is greater?",
        )])
    };
    assert_eq!(body(&req), golden("thinking_off.json"));
}

/// Catches: a level DeepSeek does not serve sent anyway, so the server maps
/// it to another level without saying so.
#[test]
fn efforts_deepseek_would_replace_are_refused_before_any_request() {
    let base = Url::parse("https://api.deepseek.com/").expect("the test base parses");
    let headers = Vec::new();
    let target = RequestTarget::new(&base, &headers);
    for level in [
        ThinkingLevel::Minimal,
        ThinkingLevel::Medium,
        ThinkingLevel::XHigh,
    ] {
        let req = GenRequest {
            thinking: level,
            ..request(vec![Message::text(Role::User, "Hi")])
        };
        let refused = DeepSeek
            .build_request(&target, &req)
            .expect_err("an effort DeepSeek replaces is refused");
        assert_eq!(
            refused.to_string(),
            format!(
                "DeepSeek has no `{}` reasoning effort; it serves off, low, high and max",
                level.as_str()
            )
        );
    }
}

/// Catches: a tool continuation sent without the earlier `reasoning_content`,
/// which DeepSeek answers with `400` — including the turn that made no tool
/// call and the turns before a later user message.
#[test]
fn earlier_reasoning_goes_back_on_every_assistant_turn_of_a_tool_continuation() {
    let req = request(vec![
        Message::text(Role::User, "How's the weather in Hangzhou Tomorrow"),
        assistant(vec![
            reasoning(
                "The user is asking about the weather in Hangzhou tomorrow. I need to get \
                 tomorrow's date first, then call the weather function.",
                DEEPSEEK_REASONING_DIALECT,
            ),
            text(
                "Let me check tomorrow's weather in Hangzhou for you. First, let me get \
                 tomorrow's date.",
            ),
            call("call_00_kw66qNnNto11bSfJVIdlV5Oo", "get_date", "{}"),
        ]),
        result("call_00_kw66qNnNto11bSfJVIdlV5Oo", "2026-04-19"),
        assistant(vec![
            reasoning(
                "Today is 2026-04-19, so tomorrow is 2026-04-20. Now I'll call the weather \
                 function for Hangzhou.",
                DEEPSEEK_REASONING_DIALECT,
            ),
            call(
                "call_00_H2SCW6136vWJGq9SQlBuhVt4",
                "get_weather",
                r#"{"location": "Hangzhou", "date": "2026-04-20"}"#,
            ),
        ]),
        result("call_00_H2SCW6136vWJGq9SQlBuhVt4", "Cloudy 7~13°C"),
        assistant(vec![
            reasoning(
                "The weather result is in. Let me share this with the user.",
                DEEPSEEK_REASONING_DIALECT,
            ),
            text(
                "Here's the weather forecast for Hangzhou tomorrow (April 20, 2026): cloudy, \
                 7°C ~ 13°C.",
            ),
        ]),
        Message::text(Role::User, "How's the weather in Guangzhou Tomorrow"),
    ]);
    assert_eq!(body(&req), golden("tool_result_continuation.json"));
}

/// Catches: the mapping replaying the OpenRouter dialect, so reasoning minted
/// through one provider leaks to another as `reasoning_content` instead of
/// being refused before any request.
#[test]
fn reasoning_of_another_dialect_is_refused_before_any_request() {
    let req = request(vec![
        Message::text(Role::User, "How's the weather in Hangzhou Tomorrow"),
        assistant(vec![
            reasoning("routed reasoning", OPENROUTER_REASONING_DIALECT),
            text("Let me check."),
        ]),
        Message::text(Role::User, "And in Guangzhou?"),
    ]);
    assert!(request_body(&req).is_err());
}

/// Catches: argument fragments joined wrongly, or the reasoning of the turn
/// not kept as DeepSeek replay state ahead of the call it led to.
#[test]
fn a_streamed_reasoning_tool_turn_mints_deepseek_state_before_its_whole_call() {
    let events = events(REASONING_TOOL_CALL);
    let state_at = events
        .iter()
        .position(|event| {
            *event
                == AiEvent::ReasoningState(ReasoningState {
                    connection: None,
                    blob: "Today is 2026-04-19, so tomorrow is 2026-04-20. Now I'll call the \
                           weather function for Hangzhou."
                        .to_string(),
                    dialect: DEEPSEEK_REASONING_DIALECT.to_string(),
                })
        })
        .unwrap_or_else(|| panic!("one DeepSeek reasoning state: {events:?}"));
    let call_at = events
        .iter()
        .position(|event| {
            *event
                == AiEvent::ToolCall {
                    call_id: "call_00_H2SCW6136vWJGq9SQlBuhVt4".to_string(),
                    name: "get_weather".to_string(),
                    args: r#"{"location": "Hangzhou", "date": "2026-04-20"}"#.to_string(),
                }
        })
        .unwrap_or_else(|| panic!("one whole call: {events:?}"));
    assert!(state_at < call_at, "{events:?}");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AiEvent::ReasoningState(_)))
            .count(),
        1
    );
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));
}

/// Catches: an opportunistic display extension contaminating DeepSeek's exact
/// replay bytes, or narrowing DeepSeek replay changing the routed contract.
/// This is deliberate schema-fixture fault injection, not an external capture.
#[test]
fn only_the_admitted_response_field_becomes_deepseek_replay_state() {
    let transcript = REASONING_TOOL_CALL.replacen(
        r#""reasoning_content":"""#,
        r#""reasoning":"display-only extension","reasoning_content":"""#,
        1,
    );
    let expected = golden("tool_result_continuation.json")["messages"][3]["reasoning_content"]
        .as_str()
        .unwrap()
        .to_string();
    let decoded = events(&transcript);
    assert!(decoded.contains(&AiEvent::ReasoningDelta {
        text: "display-only extension".into(),
    }));
    let states: Vec<_> = decoded
        .iter()
        .filter_map(|event| match event {
            AiEvent::ReasoningState(state) => Some(state.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![ReasoningState {
            connection: None,
            blob: expected.clone(),
            dialect: DEEPSEEK_REASONING_DIALECT.into(),
        }]
    );
    let routed: Vec<AiEvent> = futures::executor::block_on(
        ego_ai::wire::openai_completions::OpenRouterCompletions
            .decode(responding(&transcript), None)
            .collect(),
    );
    let routed_states: Vec<_> = routed
        .iter()
        .filter_map(|event| match event {
            AiEvent::ReasoningState(state) => Some(state.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        routed_states,
        vec![ReasoningState {
            connection: None,
            blob: format!("display-only extension{expected}"),
            dialect: OPENROUTER_REASONING_DIALECT.into(),
        }]
    );

    // A display-only spelling is not evidence of any replay payload, even
    // when the tool turn itself finishes normally.
    let display_only = REASONING_TOOL_CALL.replace("reasoning_content", "reasoning");
    let decoded = events(&display_only);
    assert!(
        decoded
            .iter()
            .any(|event| matches!(event, AiEvent::ReasoningDelta { .. }))
    );
    assert!(
        !decoded
            .iter()
            .any(|event| matches!(event, AiEvent::ReasoningState(_)))
    );
    assert_eq!(decoded.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));
}

/// Catches: a session store record, reducer or history rebuild dropping the
/// DeepSeek replay state, substituting displayed reasoning, or separating it
/// from the call it led to. State-only serde coverage cannot catch this path.
#[test]
fn reasoning_tool_continuation_survives_session_record_replay() {
    use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
    use ego_core::event::id::{EventId, ItemId, Producer, Seq, TurnId};
    use ego_core::event::time::Timestamp;
    use ego_core::generator::history::conversation_from;
    use ego_core::permit::ToolResult;
    use ego_core::state::SessionState;
    use ego_core::store::codec::{StoreLine, decode_line, encode_record};
    use ego_core::store::session_store::SessionEntry;

    let expected = golden("tool_result_continuation.json");
    let mut journal = Vec::new();
    let mut seq = Seq::ZERO;
    let turn = TurnId::new();
    let mut append = |item, kind| {
        let event = DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn,
                item: Some(item),
                parent: None,
                ts: Timestamp::from_millis(0),
                producer: Producer::Main,
            },
            kind,
        };
        journal.push(
            encode_record(event.env.seq, &SessionEntry::Event(event))
                .expect("the durable event encodes"),
        );
    };
    append(
        ItemId::new(),
        EventKind::UserPrompt {
            text: expected["messages"][0]["content"].as_str().unwrap().into(),
            images: Vec::new(),
            audio: Vec::new(),
            queued_prompt: None,
        },
    );
    let reasoning_item = ItemId::new();
    // Display is deliberately distinct from the opaque bytes minted below:
    // a resumed request must use replay state, never rendered reasoning.
    append(
        reasoning_item,
        EventKind::ReasoningDelta {
            text: "display-only reasoning preview".into(),
        },
    );
    // Deliberately perturb the schema fixture, not a purported external
    // recording: displayed extension text must not enter the journalled blob.
    let transcript = REASONING_TOOL_CALL.replacen(
        r#""reasoning_content":"""#,
        r#""reasoning":"display-only extension","reasoning_content":"""#,
        1,
    );
    for event in events(&transcript) {
        match event {
            AiEvent::ReasoningState(state) => {
                assert_eq!(state.dialect, DEEPSEEK_REASONING_DIALECT);
                assert_eq!(
                    state.blob,
                    expected["messages"][3]["reasoning_content"]
                        .as_str()
                        .unwrap()
                );
                append(reasoning_item, EventKind::ReasoningState { state });
            }
            AiEvent::ToolCall {
                call_id,
                name,
                args,
            } => {
                let item = ItemId::new();
                append(
                    item,
                    EventKind::ToolCallStarted {
                        name,
                        args,
                        call_id: Some(call_id),
                    },
                );
                append(
                    item,
                    EventKind::ToolCallCompleted {
                        ok: true,
                        result: ToolResult::builtin("Cloudy 7~13°C"),
                    },
                );
            }
            _ => {}
        }
    }

    let mut reloaded = SessionState::default();
    for bytes in journal {
        let StoreLine::Record(record) = decode_line(&bytes).expect("record checksum is valid")
        else {
            panic!("the journal contains only event records");
        };
        let SessionEntry::Event(event) =
            serde_json::from_str(record.payload.get()).expect("the durable entry decodes")
        else {
            panic!("the journal contains only conversation events");
        };
        reloaded.apply(&event);
    }
    let (messages, repairs) = conversation_from(&reloaded).expect("the session history rebuilds");
    assert!(
        repairs.is_empty(),
        "the completed tool call needs no repair"
    );
    let mut req = request(vec![Message::text(Role::User, "unused")]);
    req.messages = messages;
    let mut continuation = expected.clone();
    continuation["messages"] = serde_json::json!([
        expected["messages"][0],
        expected["messages"][3],
        expected["messages"][4]
    ]);
    assert_eq!(body(&req), continuation);
    assert!(ego_ai::wire::openai_completions::request_body(&req).is_err());
    assert!(ego_ai::wire::openai_completions::openrouter_request_body(&req).is_err());
}

/// Catches: a server interruption read as a clean stop, or as an error no
/// retry may follow; half a reasoning stored as replay state.
#[test]
fn a_server_interruption_is_a_retryable_error_and_never_a_clean_stop() {
    for (transcript, reason) in [
        (INSUFFICIENT_RESOURCE, "insufficient_system_resource"),
        (ABORTED, "aborted"),
    ] {
        let events = events(transcript);
        let expected = format!(
            "the provider interrupted the generation on its side \
             (finish_reason {reason}); a retry may succeed"
        );
        let errors: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AiEvent::Error { message, kind } => Some((message.as_str(), *kind)),
                _ => None,
            })
            .collect();
        assert_eq!(
            errors,
            vec![(expected.as_str(), ErrorKind::Transient)],
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::ReasoningState(_))),
            "{events:?}"
        );
        assert!(
            !events.contains(&AiEvent::Stop(StopReason::Stop)),
            "{events:?}"
        );
        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
    }
}

/// Catches: `402` (an empty balance) read as retryable, so `--retry` turns it
/// into a loop of refused requests; `429` or `503` losing their retry; `401`
/// read as anything but a refused credential. Statuses and their meaning:
/// <https://api-docs.deepseek.com/quick_start/error_codes>, read 2026-11-14.
#[test]
fn each_documented_error_status_keeps_its_retry_class() {
    for (status, expected) in [
        (401, ErrorKind::Refused),
        (402, ErrorKind::Refused),
        (422, ErrorKind::Invalid),
        (429, ErrorKind::Transient),
        (503, ErrorKind::Transient),
    ] {
        let path = format!(
            "{}/tests/goldens/profile/deepseek/error/{status}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let body = std::fs::read_to_string(&path).expect("the error golden exists");
        let events: Vec<AiEvent> = futures::executor::block_on(
            DeepSeek
                .decode(answering(status, "application/json", &body), None)
                .collect(),
        );
        assert_eq!(error_kinds(&events), vec![expected], "{status}: {events:?}");
        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
    }
}

struct NoCredentials;

impl Credentials for NoCredentials {
    fn secret(&self, var: &str) -> Result<Secret, MissingCredential> {
        Err(MissingCredential { var: var.into() })
    }
}

/// Catches: the row reading another provider's variable, or building a
/// request with no credential instead of refusing. The refusal comes from the
/// credential strategy alone, which takes no transport, so it is made before
/// any connection exists.
#[test]
fn a_missing_key_is_refused_by_name_before_any_request() {
    let refused = ProviderRow::deepseek_default()
        .credential
        .resolve(&NoCredentials)
        .expect_err("no key, no credential");
    assert_eq!(
        refused,
        MissingCredential {
            var: DEEPSEEK_KEY_VAR.into()
        }
    );
    assert_eq!(
        refused.to_string(),
        "no API key in DEEPSEEK_API_KEY; set this environment variable to the provider's API key"
    );
}

/// Catches: the cache hit dropped (every token billed at the miss rate) or
/// counted from both `cached_tokens` and the top-level
/// `prompt_cache_hit_tokens` beside it.
#[test]
fn a_cache_hit_is_read_once_from_cached_tokens() {
    let reported = usages(&events(REASONING_TOOL_CALL));
    let [usage] = reported.as_slice() else {
        panic!("one usage report: {reported:?}");
    };
    assert_eq!(
        (usage.cache_read_tokens, usage.uncached_input_tokens),
        (256, 56)
    );
    assert_eq!(usage.reasoning_tokens, Some(25));
}

#[test]
fn provider_cache_counters_do_not_require_the_optional_cached_tokens_alias() {
    let transcript = REASONING_TOOL_CALL.replace(r#""cached_tokens":256"#, "");
    assert_ne!(transcript, REASONING_TOOL_CALL);
    let reported = usages(&events(&transcript));
    let [usage] = reported.as_slice() else {
        panic!("expected one usage report: {reported:?}");
    };
    assert_eq!(
        (usage.cache_read_tokens, usage.uncached_input_tokens),
        (256, 56)
    );
}

/// The usage tail of the tool-call golden, at the example placement.
const CACHE_COUNTERS: &str = r#""prompt_tokens_details":{"cached_tokens":256},"prompt_cache_hit_tokens":256,"prompt_cache_miss_tokens":56"#;

fn with_cache_counters(replacement: &str) -> String {
    let transcript = REASONING_TOOL_CALL.replace(CACHE_COUNTERS, replacement);
    assert_ne!(
        transcript, REASONING_TOOL_CALL,
        "the golden carries the counters"
    );
    transcript
}

/// Catches: the counters read only at the example placement, so the schema
/// placement under `prompt_tokens_details` bills every hit as a miss.
#[test]
fn cache_counters_nested_under_prompt_tokens_details_are_read() {
    let transcript = with_cache_counters(
        r#""prompt_tokens_details":{"prompt_cache_hit_tokens":256,"prompt_cache_miss_tokens":56}"#,
    );
    let reported = usages(&events(&transcript));
    let [usage] = reported.as_slice() else {
        panic!("expected one usage report: {reported:?}");
    };
    assert_eq!(
        (usage.cache_read_tokens, usage.uncached_input_tokens),
        (256, 56)
    );
}

/// Catches: a report without the required counters read as a measured zero
/// hit, which bills every cached token at the miss rate, instead of an
/// unknown usage that settles at the bound.
#[test]
fn a_report_without_cache_counters_is_unknown_rather_than_a_zero_hit() {
    let events = events(&with_cache_counters(r#""prompt_tokens_details":{}"#));
    assert!(usages(&events).is_empty(), "{events:?}");
    assert!(
        error_kinds(&events).contains(&ErrorKind::Unclassified),
        "{events:?}"
    );
}

/// Catches: a split that contradicts itself repaired into a plausible
/// figure: the larger counter trusted, a lone counter completed from the
/// total, or the alias preferred over the required hit count.
#[test]
fn a_cache_split_that_contradicts_itself_is_discarded() {
    for (case, counters) in [
        (
            "hit plus miss is not the prompt",
            r#""prompt_tokens_details":{},"prompt_cache_hit_tokens":256,"prompt_cache_miss_tokens":100"#,
        ),
        (
            "only the hit counter",
            r#""prompt_tokens_details":{},"prompt_cache_hit_tokens":256"#,
        ),
        (
            "the alias disagrees with the hit counter",
            r#""prompt_tokens_details":{"cached_tokens":0},"prompt_cache_hit_tokens":256,"prompt_cache_miss_tokens":56"#,
        ),
    ] {
        let events = events(&with_cache_counters(counters));
        assert!(usages(&events).is_empty(), "{case}: {events:?}");
        assert!(
            error_kinds(&events).contains(&ErrorKind::Unclassified),
            "{case}: {events:?}"
        );
    }
}

fn searching(tools: Vec<ToolDescriptor>, thinking: ThinkingLevel) -> GenRequest {
    let mut req = request(vec![Message::text(
        Role::User,
        "What is new in DeepSeek V4.1?",
    )]);
    req.tools = tools;
    req.thinking = thinking;
    req.web_search = Some(HostedSearch {
        max_requests: std::num::NonZeroU32::new(5).expect("five is not zero"),
        allow_live_access: true,
    });
    req
}

fn search_declaration() -> ToolDescriptor {
    ToolDescriptor {
        name: "web_search".to_string(),
        description: "Search the web using the active provider hosted tool.".to_string(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
    }
}

/// Catches: the search declaration sent as a function the client would have
/// to run, the server tool missing so the model answers without searching,
/// or a field the guide answers with `400` (`tool_choice` naming the tool,
/// `web_search_options`). Shape: the Chat Completions request of the web
/// tools guide.
#[test]
fn a_search_request_declares_only_the_documented_server_tool() {
    for (level, switch) in [
        (ThinkingLevel::Off, "disabled"),
        (ThinkingLevel::High, "enabled"),
    ] {
        let body = body(&searching(vec![search_declaration()], level));
        assert_eq!(
            body["tools"],
            serde_json::json!([{ "type": "deepseek_web_search" }]),
            "{level:?}"
        );
        assert_eq!(body["thinking"]["type"], switch, "{level:?}");
        for absent in ["tool_choice", "web_search_options", "parallel_tool_calls"] {
            assert!(body.get(absent).is_none(), "{level:?}: {absent} in {body}");
        }
        assert_eq!(body["max_tokens"], 4096, "{level:?}");
    }
}

/// Catches: function tools stripped from a search request, so the model
/// loses tools it was promised, or sent beside the server tool in a shape
/// the guide does not document.
#[test]
fn a_search_request_with_function_tools_is_refused_naming_them() {
    let mut tools = tools();
    tools.push(search_declaration());
    let refused = request_body(&searching(tools, ThinkingLevel::High))
        .expect_err("search beside function tools is not documented");
    assert_eq!(
        refused.to_string(),
        "DeepSeek documents hosted search only in a request without function tools, \
         and this request also declares get_date, get_weather; select `--tools web_search` \
         alone to search, or deselect `web_search` to keep the other tools"
    );
}

/// Catches: default search switched on for every DeepSeek session although
/// no published capability field says which model serves it.
#[test]
fn search_is_offered_on_explicit_selection_only() {
    let row = ProviderRow::deepseek_default();
    let credential = row
        .credential
        .resolve(&crate::support::FakeCredentials::holding("test-key"))
        .expect("the fake credential resolves");
    assert!(DeepSeek.supports_web_search());
    assert!(!DeepSeek.supports_default_web_search(
        "deepseek-flash",
        &Url::parse(ego_ai::provider::DEEPSEEK_BASE).expect("the base parses"),
        &credential,
        &row.credential,
    ));
}

/// Catches: a stream that carried no usage settled as a free generation
/// (zero tokens) instead of an unknown one.
#[test]
fn a_stream_without_usage_reports_no_usage() {
    let start = TEXT
        .find(r#""usage":{"#)
        .expect("the text golden carries usage");
    let end = start + TEXT[start..].find("}}").expect("the usage object closes") + 2;
    let without = format!("{}\"usage\":null{}", &TEXT[..start], &TEXT[end..]);
    let events = events(&without);
    assert!(usages(&events).is_empty(), "{events:?}");
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
}
