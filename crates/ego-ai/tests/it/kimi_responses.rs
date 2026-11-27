//! Offline contract for the Moonshot Kimi Responses mapping.
//!
//! Derived from the public OpenAPI schema and guide at
//! https://platform.kimi.ai/docs/api/responses and
//! https://platform.kimi.ai/docs/guide/context-caching, read 2026-11-21.
//! Vendor schema evidence, not a live service recording.

use std::num::NonZeroU32;

use ego_ai::http::HttpResponse;
use ego_ai::wire::WireFormat;
use ego_ai::wire::kimi_responses::{DIALECT, KimiResponses, request_body};
use ego_core::event::id::SessionId;
use ego_core::generator::capabilities::CacheRetention;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, HostedSearch, Message, PromptCache, ReasoningState,
    Role, StopReason, ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use futures::StreamExt;
use serde_json::{Value, json};

fn request(messages: Vec<Message>) -> GenRequest {
    GenRequest {
        web_search: Some(HostedSearch {
            max_requests: NonZeroU32::new(5).unwrap(),
            allow_live_access: true,
        }),
        cache: None,
        model: "kimi-k3.1-pro".into(),
        messages: Conversation::new(messages).expect("valid history"),
        tools: vec![ToolDescriptor {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }],
        thinking: ThinkingLevel::High,
        max_output_tokens: Some(4096),
        parallel_tool_calls: false,
    }
}

fn opening() -> Vec<Message> {
    vec![
        Message::text(Role::System, "be brief"),
        Message::text(Role::User, "find kimi news"),
    ]
}

fn body(req: &GenRequest) -> Value {
    serde_json::from_slice(&request_body(req).expect("expressible")).unwrap()
}

fn cached(retention: CacheRetention, root: SessionId) -> GenRequest {
    let mut req = request(opening());
    req.cache = Some(PromptCache {
        verifier: None,
        session_id: SessionId::new(),
        lineage_root: root,
        retention,
        stable_messages: 1,
    });
    req
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

fn decode(events: &[Value]) -> Vec<AiEvent> {
    let text = sse(events);
    futures::executor::block_on(
        KimiResponses
            .decode(
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![
                        HttpHeader::new("content-type", b"text/event-stream".to_vec()).unwrap(),
                    ],
                    body: Box::pin(futures::stream::iter(
                        text.as_bytes()
                            .chunks(13)
                            .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
                            .collect::<Vec<_>>(),
                    )),
                }),
                None,
            )
            .collect(),
    )
}

fn search_item() -> Value {
    json!({"type":"web_search_call","id":"ws_1","status":"completed","action":{
        "type":"search","query":"kimi news",
        "sources":[{"type":"url","url":"https://example.com/a","title":"A"}]}})
}

fn reasoning_item() -> Value {
    json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"look it up"}],
        "encrypted_content":null,"status":"completed"})
}

fn completed(usage: Value, ttl: &str) -> Value {
    json!({"type":"response.completed","sequence_number":9,"response":{
        "status":"completed","usage":usage,
        "prompt_cache_options":{"mode":"implicit","ttl":ttl}}})
}

fn tool_turn(usage: Value, ttl: &str) -> Vec<Value> {
    vec![
        json!({"type":"response.created","sequence_number":0,"response":{"status":"in_progress"}}),
        json!({"type":"response.output_item.done","sequence_number":1,"item":search_item()}),
        json!({"type":"response.output_item.done","sequence_number":2,"item":reasoning_item()}),
        json!({"type":"response.reasoning_summary_text.delta","sequence_number":3,"delta":"look it up"}),
        json!({"type":"response.output_text.delta","sequence_number":4,"delta":"Reading."}),
        json!({"type":"response.output_item.added","sequence_number":5,"item":{
            "type":"function_call","id":"fc_1","call_id":"read_0","name":"read",
            "arguments":"","status":"in_progress"}}),
        json!({"type":"response.function_call_arguments.delta","sequence_number":6,
            "item_id":"fc_1","delta":"{\"path\":\"a\"}"}),
        completed(usage, ttl),
    ]
}

fn usage_of(events: &[AiEvent]) -> Option<ego_core::state::Usage> {
    events.iter().find_map(|event| match event {
        AiEvent::Usage(usage) => Some(*usage),
        _ => None,
    })
}

#[test]
fn request_sends_only_documented_fields_and_hosted_search() {
    // Catches: copying another Responses contract's store, encrypted-reasoning
    // include, parallel_tool_calls or `system` role, which this schema lacks.
    assert_eq!(
        body(&request(opening())),
        json!({
            "model":"kimi-k3.1-pro","stream":true,"reasoning":{"effort":"high"},
            "max_output_tokens":4096,
            "input":[
                {"type":"message","role":"developer","content":"be brief"},
                {"type":"message","role":"user","content":"find kimi news"}
            ],
            "tools":[
                {"type":"function","name":"read","description":"Read a file",
                 "parameters":{"type":"object","properties":{"path":{"type":"string"}}}},
                {"type":"web_search"}
            ],
            "include":["web_search_call.action.sources"]
        })
    );
    let mut plain = request(opening());
    plain.web_search = None;
    let plain = body(&plain);
    assert!(plain.get("include").is_none());
    assert_eq!(plain["tools"].as_array().unwrap().len(), 1);
}

#[test]
fn inexpressible_requests_are_refused_not_rewritten() {
    // Catches: a silent effort substitution, an out-of-range cap, a dropped
    // parallel-call promise, or foreign reasoning state lost on replay.
    for level in [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Medium,
        ThinkingLevel::XHigh,
    ] {
        let mut req = request(opening());
        req.thinking = level;
        assert!(request_body(&req).is_err(), "{level:?}");
    }
    for cap in [0, 1_048_577] {
        let mut req = request(opening());
        req.max_output_tokens = Some(cap);
        assert!(request_body(&req).is_err(), "{cap}");
    }
    let mut req = request(opening());
    req.max_output_tokens = Some(1_048_576);
    assert!(request_body(&req).is_ok());
    req.parallel_tool_calls = true;
    assert!(request_body(&req).is_err());

    let mut history = opening();
    history.push(Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Reasoning {
                state: ReasoningState {
                    connection: None,
                    dialect: "xai-responses".into(),
                    blob: reasoning_item().to_string(),
                },
            },
            ContentBlock::Text { text: "ok".into() },
        ],
    });
    history.push(Message::text(Role::User, "again"));
    let error = request_body(&request(history)).unwrap_err();
    assert!(format!("{error:?}").contains("xai-responses"), "{error:?}");
}

#[test]
fn user_images_travel_as_data_url_parts_in_block_order() {
    // Catches: images refused or dropped although the schema accepts data-URL
    // input_image parts, or text reordered ahead of the image it describes.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .unwrap();
    let user: Message = serde_json::from_value(json!({"role":"user","content":[
        {"type":"image","image":{"bytes":bytes,"media_type":"png","width":1,"height":1}},
        {"type":"text","text":"describe"}
    ]}))
    .expect("valid image message");
    let input = body(&request(vec![user]))["input"].clone();
    assert_eq!(
        input,
        json!([{"type":"message","role":"user","content":[
            {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")},
            {"type":"input_text","text":"describe"}
        ]}])
    );
}

#[test]
fn stateless_replay_returns_output_items_unchanged_and_keeps_the_prefix() {
    // Catches: dropping preserved reasoning or the search item between turns,
    // or re-encoding them, which breaks continuation and the cached prefix.
    let usage = json!({"input_tokens":10,"output_tokens":5});
    let events = decode(&tool_turn(usage, "5m"));
    let mut states = events.iter().filter_map(|event| match event {
        AiEvent::ProviderContent { state, visible } => {
            assert!(!visible);
            Some(ContentBlock::ProviderContent {
                state: state.clone(),
                text: None,
            })
        }
        AiEvent::ReasoningState(state) => Some(ContentBlock::Reasoning {
            state: state.clone(),
        }),
        _ => None,
    });
    let search = states.next().expect("search item state");
    let reasoning = states.next().expect("reasoning state");
    assert!(states.next().is_none());
    assert!(events.contains(&AiEvent::WebSearch {
        call_id: "ws_1".into(),
        query: "kimi news".into()
    }));
    assert!(events.contains(&AiEvent::Citation {
        url: "https://example.com/a".into(),
        title: "A".into()
    }));
    assert!(events.contains(&AiEvent::ToolCall {
        call_id: "read_0".into(),
        name: "read".into(),
        args: "{\"path\":\"a\"}".into()
    }));
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));

    let first = body(&request(opening()));
    let mut history = opening();
    history.push(Message {
        role: Role::Assistant,
        content: vec![
            search,
            reasoning,
            ContentBlock::Text {
                text: "Reading.".into(),
            },
            ContentBlock::ToolUse {
                call_id: "read_0".into(),
                name: "read".into(),
                args: "{\"path\":\"a\"}".into(),
            },
        ],
    });
    history.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            call_id: "read_0".into(),
            output: "file a".into(),
            is_error: false,
        }],
    });
    let second = body(&request(history));
    let input = second["input"].as_array().unwrap();
    assert_eq!(input[..2], first["input"].as_array().unwrap()[..]);
    assert_eq!(input[2], search_item());
    assert_eq!(input[3], reasoning_item());
    assert_eq!(
        input[4..],
        [
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Reading."}]}),
            json!({"type":"function_call","call_id":"read_0","name":"read","arguments":"{\"path\":\"a\"}"}),
            json!({"type":"function_call_output","call_id":"read_0","output":"file a"}),
        ]
    );
    for forbidden in ["store", "previous_response_id", "conversation"] {
        assert!(second.get(forbidden).is_none(), "{forbidden}");
    }
}

#[test]
fn cache_affinity_follows_the_lineage_and_ttl_matches_the_server_default() {
    // Catches: a per-session key that resets affinity on every fork, a dropped
    // 1h tier, or a policy claiming no cache while the server writes 5m.
    let root = SessionId::new();
    for (retention, ttl, seconds) in [
        (CacheRetention::Short, Some("5m"), 300),
        (CacheRetention::Long, Some("1h"), 3_600),
        (CacheRetention::None, None, 300),
    ] {
        let parent = cached(retention, root);
        let fork = cached(retention, root);
        let (a, b) = (body(&parent), body(&fork));
        assert_eq!(
            a["prompt_cache_key"], b["prompt_cache_key"],
            "{retention:?}"
        );
        assert_eq!(a["prompt_cache_options"]["ttl"].as_str(), ttl);
        assert_eq!(
            KimiResponses
                .prompt_cache_policy(&parent)
                .ttl_seconds
                .map(|s| s.get()),
            Some(seconds)
        );
        match ttl {
            Some(_) => {
                let key = a["prompt_cache_key"].as_str().expect("affinity key");
                assert_ne!(
                    key,
                    root.to_string(),
                    "the lineage id must not leave in clear"
                );
                assert_eq!(a["prompt_cache_options"]["mode"], "implicit");
                let other = body(&cached(retention, SessionId::new()));
                assert_ne!(a["prompt_cache_key"], other["prompt_cache_key"]);
            }
            None => assert!(a.get("prompt_cache_key").is_none()),
        }
    }
}

#[test]
fn usage_partitions_input_and_bills_writes_at_the_applied_tier() {
    // Catches: double-counting cached input, 1h writes billed as 5m, or a
    // partition that exceeds input_tokens accepted as a measurement.
    let usage = json!({"input_tokens":1000,"output_tokens":50,
        "input_tokens_details":{"cached_tokens":600,"cache_write_tokens":300},
        "output_tokens_details":{"reasoning_tokens":20}});
    for (ttl, standard, extended) in [("1h", 0, 300), ("5m", 300, 0)] {
        let got = usage_of(&decode(&tool_turn(usage.clone(), ttl))).expect("usage");
        assert_eq!(
            (got.uncached_input_tokens, got.cache_read_tokens),
            (100, 600)
        );
        assert_eq!(
            (got.cache_write_tokens, got.cache_write_extended_tokens),
            (standard, extended)
        );
        assert_eq!((got.output_tokens, got.reasoning_tokens), (50, Some(20)));
        assert_eq!(got.web_search_requests, 1);
    }
    let incoherent = json!({"input_tokens":10,"output_tokens":1,
        "input_tokens_details":{"cached_tokens":8,"cache_write_tokens":8}});
    assert!(usage_of(&decode(&tool_turn(incoherent, "5m"))).is_none());
}

#[test]
fn incomplete_reasons_map_to_length_or_refusal() {
    // Catches: a content-filter stop read as truncation or a clean stop.
    let incomplete = |reason: &str| {
        decode(&[
            json!({"type":"response.incomplete","sequence_number":1,"response":{
            "status":"incomplete","incomplete_details":{"reason":reason}}}),
        ])
    };
    assert_eq!(
        incomplete("max_output_tokens").last(),
        Some(&AiEvent::Stop(StopReason::Length))
    );
    let filtered = incomplete("content_filter");
    assert_eq!(filtered.last(), Some(&AiEvent::Stop(StopReason::Error)));
    assert!(filtered.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ego_core::generator::types::ErrorKind::Refused,
            ..
        }
    )));
    let failed = decode(&[
        json!({"type":"response.failed","sequence_number":1,"response":{
        "status":"failed","error":{"code":"server_error","message":"boom"}}}),
    ]);
    assert!(failed.iter().any(|event| matches!(
        event,
        AiEvent::Error { message, .. } if message == "boom"
    )));
    assert_eq!(failed.last(), Some(&AiEvent::Stop(StopReason::Error)));
    assert_eq!(DIALECT, "kimi-responses");
}
