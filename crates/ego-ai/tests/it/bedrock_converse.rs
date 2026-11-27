//! Schema-derived fixtures, never captured provider responses.
//! Each test names the observable bug it protects; no network or credential is used.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use ego_ai::http::HttpResponse;
use ego_ai::wire::bedrock_converse::{BedrockConverse, request_body};
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::capabilities::CacheRetention;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, Message, PromptCache, ReasoningState, Role,
    StopReason, ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use url::Url;

// Frozen binary messages generated with Python stdlib struct/zlib from the
// published Smithy framing plus AWS JSON event schemas. CRC expectations are
// independent of the production codec and the mutation helper below.
const FRAMES: [&str; 5] = [
    "000000760000005296d5fade0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d7479706507000c6d65737361676553746172740d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b22726f6c65223a22617373697374616e74227d1cc6be18",
    "0000009700000057f30be03e0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d74797065070011636f6e74656e74426c6f636b44656c74610d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b22636f6e74656e74426c6f636b496e646578223a302c2264656c7461223a7b2274657874223a2248656c6c6f227d7dd3df8db4",
    "0000007d00000056e6680fd60d3a6d6573736167652d747970650700056576656e740b3a6576656e742d74797065070010636f6e74656e74426c6f636b53746f700d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b22636f6e74656e74426c6f636b496e646578223a307d587eaac2",
    "0000007a00000051ca2c46650d3a6d6573736167652d747970650700056576656e740b3a6576656e742d7479706507000b6d65737361676553746f700d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b2273746f70526561736f6e223a22656e645f7475726e227d3dc33bfa",
    "000000b50000004e2c61349a0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d747970650700086d657461646174610d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b227573616765223a7b22696e707574546f6b656e73223a372c226f7574707574546f6b656e73223a322c22746f74616c546f6b656e73223a397d2c226d657472696373223a7b226c6174656e63794d73223a31307d7d4f28af2b",
];

fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn transcript() -> Vec<u8> {
    FRAMES.iter().flat_map(|frame| unhex(frame)).collect()
}

fn request() -> GenRequest {
    GenRequest {
        web_search: None,
        // The production turn always carries cache bookkeeping, even disabled.
        cache: Some(PromptCache {
            verifier: None,
            session_id: ego_core::event::id::SessionId::new(),
            lineage_root: ego_core::event::id::SessionId::new(),
            retention: CacheRetention::None,
            stable_messages: 1,
        }),
        model: "us.meta.llama4-maverick-17b-instruct-v1:0".to_string(),
        messages: Conversation::new(vec![
            Message::text(Role::System, "Be concise"),
            Message::text(Role::User, "Hello"),
        ])
        .unwrap(),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(128),
        parallel_tool_calls: false,
    }
}

fn tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "weather".to_string(),
        description: "Current weather".to_string(),
        parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
    }
}

async fn decode(content_type: &str, chunks: Vec<Vec<u8>>) -> Vec<AiEvent> {
    BedrockConverse
        .decode(
            Ok(HttpResponse {
                status: 200,
                headers: vec![
                    HttpHeader::new("content-type", content_type.as_bytes().to_vec()).unwrap(),
                ],
                body: stream::iter(
                    chunks
                        .into_iter()
                        .map(|chunk| Ok(bytes::Bytes::from(chunk))),
                )
                .boxed(),
            }),
            None,
        )
        .collect()
        .await
}

async fn binary(bytes: Vec<u8>) -> Vec<AiEvent> {
    decode("application/vnd.amazon.eventstream", vec![bytes]).await
}

fn assert_failed(events: &[AiEvent]) {
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AiEvent::Stop(_)))
            .count(),
        1
    );
    assert!(
        matches!(events.last(), Some(AiEvent::Stop(StopReason::Error))),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::Error { .. })),
        "{events:?}"
    );
}

// Only for constructing adversarial frames with otherwise-correct checksums.
fn crc(bytes: &[u8]) -> u32 {
    let table: Vec<u32> = (0u32..256)
        .map(|mut value| {
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    (value >> 1) ^ 0xedb8_8320
                } else {
                    value >> 1
                };
            }
            value
        })
        .collect();
    !bytes.iter().fold(u32::MAX, |sum, byte| {
        table[((sum ^ u32::from(*byte)) & 255) as usize] ^ (sum >> 8)
    })
}

fn string_header(name: &str, value: &str) -> Vec<u8> {
    let mut header = vec![u8::try_from(name.len()).unwrap()];
    header.extend_from_slice(name.as_bytes());
    header.push(7);
    header.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
    header.extend_from_slice(value.as_bytes());
    header
}

fn frame(headers: Vec<u8>, payload: &[u8]) -> Vec<u8> {
    let mut frame = u32::try_from(16 + headers.len() + payload.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(&u32::try_from(headers.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(&crc(&frame).to_be_bytes());
    frame.extend(headers);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc(&frame).to_be_bytes());
    frame
}

fn event(kind: &str, payload: Value) -> Vec<u8> {
    frame(
        [
            string_header(":message-type", "event"),
            string_header(":event-type", kind),
            string_header(":content-type", "application/json"),
        ]
        .concat(),
        payload.to_string().as_bytes(),
    )
}

#[test]
fn native_route_and_body_do_not_masquerade_as_chat_or_sse() {
    let req = request();
    let base = Url::parse("https://runtime.example/prefix/").unwrap();
    let headers = vec![HttpHeader::new("authorization", b"opaque-test-value".to_vec()).unwrap()];
    let call = BedrockConverse
        .build_request(&RequestTarget::new(&base, &headers), &req)
        .unwrap();
    assert_eq!(
        call.url.as_str(),
        "https://runtime.example/prefix/model/us.meta.llama4-maverick-17b-instruct-v1%3A0/converse-stream"
    );
    assert!(call.headers.iter().any(|header| header.name() == "accept"
        && header.value() == b"application/vnd.amazon.eventstream"));
    assert!(
        call.headers.iter().any(
            |header| header.name() == "authorization" && header.value() == b"opaque-test-value"
        )
    );
    let body: Value = serde_json::from_slice(call.body.as_ref().unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"system":[{"text":"Be concise"}],"messages":[{"role":"user","content":[{"text":"Hello"}]}],"inferenceConfig":{"maxTokens":128}})
    );
    let mut resource = req;
    resource.model = "arn:aws:bedrock:us-east-1:123:inference-profile/a/b?#".to_string();
    let call = BedrockConverse
        .build_request(&RequestTarget::new(&base, &[]), &resource)
        .unwrap();
    assert!(
        call.url
            .path()
            .ends_with("inference-profile%2Fa%2Fb%3F%23/converse-stream")
    );
    assert!(call.url.query().is_none());
    assert!(call.url.fragment().is_none());
}

#[tokio::test]
async fn llama_tool_turn_and_result_stay_synchronous_with_original_identity() {
    let response = json!({"output":{"message":{"role":"assistant","content":[{"toolUse":{"toolUseId":"call:1","name":"weather","input":{"city":"Rome"}}}]}},"stopReason":"tool_use","usage":{"inputTokens":7,"outputTokens":2,"totalTokens":9},"metrics":{"latencyMs":10}});
    let events = decode(
        "application/json; charset=utf-8",
        vec![response.to_string().into_bytes()],
    )
    .await;
    assert!(events.iter().any(|event| matches!(event, AiEvent::ToolCall {call_id,name,args} if call_id=="call:1" && name=="weather" && serde_json::from_str::<Value>(args).unwrap()==json!({"city":"Rome"}))));
    assert!(matches!(
        events.last(),
        Some(AiEvent::Stop(StopReason::ToolUse))
    ));
    let mut req = request();
    req.tools.push(tool());
    let base = Url::parse("https://runtime.example/").unwrap();
    let first = BedrockConverse
        .build_request(&RequestTarget::new(&base, &[]), &req)
        .unwrap();
    assert!(first.url.path().ends_with("/converse"));
    let first_body: Value = serde_json::from_slice(first.body.as_ref().unwrap()).unwrap();
    assert_eq!(
        first_body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"],
        tool().parameters
    );
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "weather?"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                call_id: "call:1".to_string(),
                name: "weather".to_string(),
                args: "{\"city\":\"Rome\"}".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "call:1".to_string(),
                output: "sunny".to_string(),
                is_error: false,
            }],
        },
    ])
    .unwrap();
    // Even without fresh definitions the historical tool turn cannot stream.
    req.tools.clear();
    let next = BedrockConverse
        .build_request(&RequestTarget::new(&base, &[]), &req)
        .unwrap();
    assert!(next.url.path().ends_with("/converse"));
    let body: Value = serde_json::from_slice(next.body.as_ref().unwrap()).unwrap();
    assert_eq!(
        body["messages"][1]["content"][0]["toolUse"],
        json!({"toolUseId":"call:1","name":"weather","input":{"city":"Rome"}})
    );
    assert_eq!(
        body["messages"][2]["content"][0]["toolResult"],
        json!({"toolUseId":"call:1","content":[{"text":"sunny"}]})
    );
    let final_response = json!({"output":{"message":{"role":"assistant","content":[{"text":"It is sunny."}]}},"stopReason":"end_turn","usage":{"inputTokens":12,"outputTokens":4,"totalTokens":16},"metrics":{"latencyMs":10}}).to_string().into_bytes();
    let events = decode(
        "application/json",
        vec![final_response[..10].to_vec(), final_response[10..].to_vec()],
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::TextDelta { text } if text == "It is sunny."))
    );
    assert!(matches!(
        events.last(),
        Some(AiEvent::Stop(StopReason::Stop))
    ));
}

#[test]
fn unsupported_controls_and_state_are_refused_instead_of_silently_lost() {
    let mut req = request();
    req.thinking = ThinkingLevel::Low;
    assert!(request_body(&req).is_err());
    req = request();
    req.tools.push(tool());
    assert!(!req.parallel_tool_calls);
    let disabled_cache: Value = serde_json::from_slice(&request_body(&req).unwrap()).unwrap();
    assert!(disabled_cache["toolConfig"].get("tools").is_some());
    assert!(disabled_cache.get("parallel_tool_calls").is_none());
    assert!(
        disabled_cache["toolConfig"]
            .get("parallelToolCalls")
            .is_none()
    );
    req.cache = None;
    assert_eq!(
        serde_json::from_slice::<Value>(&request_body(&req).unwrap()).unwrap(),
        disabled_cache
    );
    for retention in [CacheRetention::Short, CacheRetention::Long] {
        req = request();
        req.cache.as_mut().unwrap().retention = retention;
        assert!(
            request_body(&req).is_err(),
            "positive cache controls must not be dropped"
        );
    }
    req = request();
    req.max_output_tokens = Some(0);
    assert!(request_body(&req).is_err());
    req = request();
    req.max_output_tokens = Some(u64::MAX);
    assert!(request_body(&req).is_err());
    req = request();
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "Hi"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning {
                state: ReasoningState {
                    connection: None,
                    blob: "private".to_string(),
                    dialect: "another-wire".to_string(),
                },
            }],
        },
    ])
    .unwrap();
    assert!(request_body(&req).is_err());
    req = request();
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "Hi"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                call_id: "call:1".to_string(),
                name: "weather".to_string(),
                args: "{}".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "call:1".to_string(),
                output: "failed".to_string(),
                is_error: true,
            }],
        },
    ])
    .unwrap();
    let failed: Value = serde_json::from_slice(&request_body(&req).unwrap()).unwrap();
    let result = &failed["messages"][2]["content"][0]["toolResult"];
    assert_eq!(result["toolUseId"], "call:1");
    assert_eq!(
        result["content"],
        json!([{"text":"The tool failed:\nfailed"}])
    );
    assert!(result.get("status").is_none());
    let mut history = req.messages.messages().to_vec();
    if let ContentBlock::ToolResult { is_error, .. } = &mut history[2].content[0] {
        *is_error = false;
    }
    req.messages = Conversation::new(history).unwrap();
    let successful: Value = serde_json::from_slice(&request_body(&req).unwrap()).unwrap();
    assert_eq!(
        successful["messages"][2]["content"][0]["toolResult"]["content"],
        json!([{"text":"failed"}])
    );
}

#[tokio::test]
async fn native_binary_splits_and_sparse_indices_preserve_text_usage_and_one_stop() {
    let bytes = transcript();
    let expected = binary(bytes.clone()).await;
    assert!(
        expected
            .iter()
            .any(|event| matches!(event, AiEvent::TextDelta {text} if text=="Hello"))
    );
    assert!(expected.iter().any(|event| matches!(event, AiEvent::Usage(usage) if usage.uncached_input_tokens==7 && usage.output_tokens==2 && usage.reasoning_tokens.is_none())));
    assert_eq!(
        expected
            .iter()
            .filter(|event| matches!(event, AiEvent::Usage(_)))
            .count(),
        1
    );
    assert!(matches!(
        expected.last(),
        Some(AiEvent::Stop(StopReason::Stop))
    ));
    for at in 0..=bytes.len() {
        assert_eq!(
            decode(
                "application/vnd.amazon.eventstream",
                vec![bytes[..at].to_vec(), bytes[at..].to_vec()]
            )
            .await,
            expected,
            "split {at}"
        );
    }
    // The schema requires correlation, not contiguous numbering from zero.
    let mut sparse = unhex(FRAMES[0]);
    for index in [7, 42] {
        sparse.extend(event(
            "contentBlockDelta",
            json!({"contentBlockIndex":index,"delta":{"text":"Hi"}}),
        ));
        sparse.extend(event(
            "contentBlockStop",
            json!({"contentBlockIndex":index}),
        ));
    }
    sparse.extend(unhex(FRAMES[3]));
    sparse.extend(unhex(FRAMES[4]));
    assert!(matches!(
        binary(sparse).await.last(),
        Some(AiEvent::Stop(StopReason::Stop))
    ));
}

#[tokio::test]
async fn every_truncated_prefix_including_after_message_stop_fails_closed() {
    let bytes = transcript();
    for at in 0..bytes.len() {
        assert_failed(&binary(bytes[..at].to_vec()).await);
    }
}

#[tokio::test]
async fn either_crc_corruption_cannot_emit_a_successful_stop() {
    for byte in [0, 4, 8, 12, 40, 100, 117] {
        let mut bytes = transcript();
        bytes[byte] ^= 1;
        assert_failed(&binary(bytes).await);
    }
    // Corruption after the terminal message and usage also overrides success.
    let mut bytes = transcript();
    bytes.extend(unhex(FRAMES[0]));
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    assert_failed(&binary(bytes).await);
}

#[tokio::test]
async fn valid_crc_does_not_make_invalid_lengths_or_headers_acceptable() {
    for (total, headers) in [(0u32, 0u32), (15, 0), (16, 1), (u32::MAX, 0)] {
        let mut prelude = total.to_be_bytes().to_vec();
        prelude.extend(headers.to_be_bytes());
        prelude.extend(crc(&prelude).to_be_bytes());
        assert_failed(&binary(prelude).await);
    }
    let normal = [
        string_header(":message-type", "event"),
        string_header(":event-type", "messageStart"),
        string_header(":content-type", "application/json"),
    ]
    .concat();
    for suffix in [
        vec![0],
        vec![1, b'x', 10],
        vec![1, b'x', 7, 0, 5, b'a'],
        string_header(":message-type", "event"),
        vec![1, 255, 0],
    ] {
        let mut headers = normal.clone();
        headers.extend(suffix);
        assert_failed(&binary(frame(headers, b"{\"role\":\"assistant\"}")).await);
    }
}

#[tokio::test]
async fn known_typed_extension_headers_do_not_turn_binary_frames_into_sse() {
    // Every fixed native type is legal under an unrelated extension name.
    for (kind, length) in [
        (0, 0),
        (1, 0),
        (2, 1),
        (3, 2),
        (4, 4),
        (5, 8),
        (8, 8),
        (9, 16),
    ] {
        let mut headers = [
            string_header(":message-type", "event"),
            string_header(":event-type", "messageStart"),
            string_header(":content-type", "application/json"),
        ]
        .concat();
        headers.extend([1, b'x', kind]);
        headers.extend(vec![0; length]);
        let mut bytes = frame(headers, b"{\"role\":\"assistant\"}");
        for rest in &FRAMES[1..] {
            bytes.extend(unhex(rest));
        }
        assert!(matches!(
            binary(bytes).await.last(),
            Some(AiEvent::Stop(StopReason::Stop))
        ));
    }
    assert_failed(&decode("text/event-stream", vec![b"data: {}\n\n".to_vec()]).await);
}

#[tokio::test]
async fn stream_tool_state_unknown_events_and_reordered_usage_are_not_synthetic_success() {
    for injected in [
        event(
            "contentBlockStart",
            json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"a","name":"weather"}}}),
        ),
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"secret"}}}),
        ),
        event(
            "metadata",
            json!({"usage":{"inputTokens":7,"outputTokens":2,"totalTokens":9},"metrics":{"latencyMs":10}}),
        ),
        event("futureEvent", json!({})),
    ] {
        let mut bytes = unhex(FRAMES[0]);
        bytes.extend(injected);
        assert_failed(&binary(bytes).await);
    }
    let mut duplicate = transcript();
    duplicate.extend(unhex(FRAMES[4]));
    assert_failed(&binary(duplicate).await);
}

#[tokio::test]
async fn native_exceptions_keep_failure_class_and_never_successfully_stop() {
    for (code, kind, capacity) in [
        ("throttlingException", ErrorKind::Transient, true),
        ("serviceUnavailableException", ErrorKind::Transient, true),
        ("internalServerException", ErrorKind::Transient, false),
        ("modelStreamErrorException", ErrorKind::Transient, false),
        ("validationException", ErrorKind::Invalid, false),
        ("unknownException", ErrorKind::Unclassified, false),
    ] {
        let bytes = frame(
            [
                string_header(":message-type", "exception"),
                string_header(":exception-type", code),
                string_header(":content-type", "application/json"),
            ]
            .concat(),
            b"{\"message\":\"safe typed cause\"}",
        );
        let events = binary(bytes).await;
        assert_failed(&events);
        assert!(
            events
                .iter()
                .any(|event| matches!(event,AiEvent::Error {kind:actual,..} if *actual==kind))
        );
        assert!(events.iter().any(|event|matches!(event,AiEvent::ProviderFailure {cause,capacity:actual,..} if cause=="safe typed cause" && *actual==capacity)));
    }
    let events = binary(frame(
        [
            string_header(":message-type", "error"),
            string_header(":error-code", "Unknown"),
            string_header(":error-message", "typed header cause"),
        ]
        .concat(),
        b"",
    ))
    .await;
    assert_failed(&events);
    assert!(events.iter().any(
        |event| matches!(event,AiEvent::ProviderFailure {cause,..} if cause=="typed header cause")
    ));
}

#[tokio::test]
async fn sync_json_truncation_unsupported_output_and_incoherent_usage_do_not_succeed() {
    let response = json!({"output":{"message":{"role":"assistant","content":[{"text":"Hello"}]}},"stopReason":"end_turn","usage":{"inputTokens":7,"outputTokens":2,"totalTokens":9},"metrics":{"latencyMs":10}});
    let bytes = response.to_string().into_bytes();
    assert_failed(&decode("application/json", vec![bytes[..bytes.len() - 1].to_vec()]).await);
    let mut overflow = response.clone();
    overflow["stopReason"] = json!("model_context_window_exceeded");
    let overflow = decode("application/json", vec![overflow.to_string().into_bytes()]).await;
    assert_failed(&overflow);
    assert!(overflow.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::ContextOverflow,
            ..
        }
    )));
    for (field, value) in [
        ("stopReason", json!("content_filtered")),
        ("stopReason", json!("unknown")),
        (
            "usage",
            json!({"inputTokens":7,"outputTokens":2,"totalTokens":8}),
        ),
        (
            "usage",
            json!({"inputTokens":7,"outputTokens":2,"totalTokens":9,"cacheReadInputTokens":1}),
        ),
        (
            "output",
            json!({"message":{"role":"assistant","content":[{"reasoningContent":{"reasoningText":{"text":"secret","signature":"private"}}}]}}),
        ),
    ] {
        let mut malformed = response.clone();
        malformed[field] = value;
        assert_failed(&decode("application/json", vec![malformed.to_string().into_bytes()]).await);
    }
}

// These are operator-declared fixture facts, not built-in model claims. In
// particular parallel support is false unless positively declared; false is
// not a server at-most-one-call guarantee or a tool-use prohibition.
fn native_instance() -> ego_core::config::ProviderInstance {
    use ego_core::config::*;
    use ego_core::generator::capabilities::*;
    ProviderInstance {
        profile: ProviderProfile::BedrockRuntime,
        base_url: "https://bedrock-runtime.us-east-1.amazonaws.com/declared/"
            .parse()
            .unwrap(),
        credential_ref: Some("native-account".parse().unwrap()),
        models: std::collections::BTreeMap::from([(
            "chat-looking-alias".parse().unwrap(),
            ProviderInstanceModel {
                upstream_id: "us.meta.llama4-maverick-17b-instruct-v1:0".into(),
                contract: ProviderModelContract::BedrockConverse,
                facts: ModelCapabilities {
                    schema_version: ModelCapabilities::SCHEMA_VERSION,
                    max_context_tokens: Some(1_048_576),
                    max_output_tokens: Some(8192),
                    max_tools: Some(8),
                    supports_parallel_tool_calls: false,
                    reasoning_carryover: ReasoningCarryover::Unsupported,
                    reasoning_efforts: ReasoningEffortSupport::Only([ThinkingLevel::Off].into()),
                    default_reasoning_effort: Some(ThinkingLevel::Off),
                    reasoning_state_dialect: None,
                    cancel: CancelSemantics::Cooperative,
                    cache: CacheRetention::None,
                    transport: Transport::AmazonEventStream,
                    image_input: ImageInputSupport::Unsupported,
                    audio_input: AudioInputSupport::Unsupported,
                },
                metering: ego_core::budget::MeteringIdentity::Currency {
                    settlement: ego_core::budget::SettlementSource::Quote,
                },
            },
        )]),
    }
}

fn native_providers(
    instance: ego_core::config::ProviderInstance,
) -> ego_core::config::ResolvedProviders {
    ego_core::config::ResolvedProviders {
        instances: std::collections::BTreeMap::from([(
            "office-native".parse().unwrap(),
            ego_core::config::Sourced {
                source: ego_core::config::Source::User,
                value: instance,
            },
        )]),
    }
}

fn native_connections() -> std::collections::BTreeMap<String, ego_core::config::ConnectionConfig> {
    std::collections::BTreeMap::from([(
        "native-account".into(),
        ego_core::config::ConnectionConfig {
            name: "Native account".into(),
            vendor: "bedrock-runtime".into(),
            access: ego_core::config::ConnectionAccess::Key,
            key_env: Some("NATIVE_BEDROCK_API_KEY".into()),
            azure_resource: None,
        },
    )])
}

struct ScopedSecrets {
    available: &'static str,
    reads: std::sync::Mutex<Vec<String>>,
}

impl ego_ai::provider::Credentials for ScopedSecrets {
    fn secret(
        &self,
        var: &str,
    ) -> Result<ego_ai::provider::Secret, ego_ai::provider::MissingCredential> {
        self.reads.lock().unwrap().push(var.into());
        if var == self.available {
            Ok("scoped-key".into())
        } else {
            Err(ego_ai::provider::MissingCredential { var: var.into() })
        }
    }
}

#[test]
fn native_profile_is_distinct_in_inventory_without_default_models_or_prices() {
    use ego_ai::provider::{CredentialStrategy, ProviderRow};
    let native = ProviderRow::bedrock_runtime_default();
    let messages = ProviderRow::bedrock_default();
    assert_eq!(native.id(), "bedrock-runtime");
    assert_eq!(
        native.base().as_str(),
        "https://bedrock-runtime.us-east-1.amazonaws.com/"
    );
    assert!(native.price_table_name().is_none());
    assert!(native.cost.is_none());
    assert!(
        matches!(native.credential, CredentialStrategy::BearerFromEnv {ref var} if var==ego_ai::provider::BEDROCK_KEY_VAR)
    );
    assert!(
        matches!(messages.credential, CredentialStrategy::HeaderFromEnv {ref header,..} if header=="x-api-key")
    );
    let rows = ego_ai::conformance::default_final_provider_rows().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.id() == "bedrock-runtime")
            .count(),
        1
    );
    assert_eq!(rows.iter().filter(|row| row.id() == "bedrock").count(), 1);
    let catalogue = ego_ai::catalogue::Catalogue::new(rows);
    let slug = ego_ai::catalogue::ModelSlug::parse("bedrock-runtime/unconfigured-model").unwrap();
    assert!(catalogue.static_request_profile(&slug).is_err());
    let (http, unasked) = crate::support::unasked();
    assert!(
        futures::executor::block_on(catalogue.resolve(&slug, &http, &crate::support::credential()))
            .is_err()
    );
    assert!(
        unasked.asked().is_empty(),
        "unconfigured models must not trigger /models or generation"
    );
}

#[test]
fn configured_native_static_and_runtime_contracts_preserve_alias_route_and_scoped_bearer() {
    use ego_ai::catalogue::{Catalogue, ModelSlug};
    use ego_ai::provider::ProviderRow;
    let catalogue = Catalogue::new([ProviderRow::bedrock_runtime_default()])
        .with_instances(&native_providers(native_instance()), &native_connections())
        .unwrap();
    let slug = ModelSlug::parse("office-native/chat-looking-alias").unwrap();
    let profile = catalogue.static_request_profile(&slug).unwrap().unwrap();
    let secrets = ScopedSecrets {
        available: "NATIVE_BEDROCK_API_KEY",
        reads: Default::default(),
    };
    let credential = catalogue
        .row("office-native")
        .unwrap()
        .credential
        .resolve(&secrets)
        .unwrap();
    let (http, unasked) = crate::support::unasked();
    let resolved =
        futures::executor::block_on(catalogue.resolve(&slug, &http, &credential)).unwrap();
    assert_eq!(
        profile.request_model(),
        "us.meta.llama4-maverick-17b-instruct-v1:0"
    );
    assert_eq!(resolved.model, profile.request_model());
    assert_eq!(resolved.row.id(), "office-native");
    assert_eq!(resolved.wire.name(), "BedrockConverse");
    assert_eq!(profile.capabilities(), &resolved.capabilities);
    let mut req = request();
    req.model = resolved.model.clone();
    req.parallel_tool_calls = resolved.capabilities.supports_parallel_tool_calls;
    assert!(!req.parallel_tool_calls);
    assert_eq!(req.cache.as_ref().unwrap().retention, CacheRetention::None);
    req.tools.push(tool());
    profile.ensure_expressible(&req).unwrap();
    let headers = credential.headers();
    let built = resolved
        .wire
        .build_request(&RequestTarget::new(resolved.row.base(), &headers), &req)
        .unwrap();
    assert_eq!(
        built.url.as_str(),
        "https://bedrock-runtime.us-east-1.amazonaws.com/declared/model/us.meta.llama4-maverick-17b-instruct-v1%3A0/converse"
    );
    assert!(
        built.headers.iter().any(
            |header| header.name() == "authorization" && header.value() == b"Bearer scoped-key"
        )
    );
    assert!(
        !built
            .headers
            .iter()
            .any(|header| header.name() == "x-api-key")
    );
    let body: Value = serde_json::from_slice(built.body.as_ref().unwrap()).unwrap();
    assert!(body.get("model").is_none());
    assert!(body.get("toolConfig").is_some());
    req.thinking = ThinkingLevel::High;
    assert!(profile.ensure_expressible(&req).is_err());
    assert!(
        resolved
            .wire
            .build_request(&RequestTarget::new(resolved.row.base(), &headers), &req)
            .is_err()
    );
    assert_eq!(
        *secrets.reads.lock().unwrap(),
        vec!["NATIVE_BEDROCK_API_KEY"]
    );
    assert!(
        unasked.asked().is_empty(),
        "static/runtime configured resolution must not discover models or dispatch"
    );
}

#[test]
fn native_connection_missing_wrong_vendor_oauth_or_missing_key_cannot_fall_back() {
    use ego_ai::catalogue::Catalogue;
    use ego_ai::provider::ProviderRow;
    use ego_core::config::ConnectionAccess;
    let providers = native_providers(native_instance());
    let mut absent_ref = native_instance();
    absent_ref.credential_ref = None;
    assert!(
        Catalogue::new([ProviderRow::bedrock_runtime_default()])
            .with_instances(&native_providers(absent_ref), &native_connections())
            .is_err()
    );
    assert!(
        Catalogue::new([ProviderRow::bedrock_runtime_default()])
            .with_instances(&providers, &Default::default())
            .is_err()
    );
    for (vendor, access, key_env) in [
        (
            "bedrock",
            ConnectionAccess::Key,
            Some("NATIVE_BEDROCK_API_KEY"),
        ),
        ("bedrock-runtime", ConnectionAccess::Oauth, None),
    ] {
        let mut connections = native_connections();
        let connection = connections.get_mut("native-account").unwrap();
        connection.vendor = vendor.into();
        connection.access = access;
        connection.key_env = key_env.map(str::to_string);
        assert!(
            Catalogue::new([ProviderRow::bedrock_runtime_default()])
                .with_instances(&providers, &connections)
                .is_err()
        );
    }
    let catalogue = Catalogue::new([ProviderRow::bedrock_runtime_default()])
        .with_instances(&providers, &native_connections())
        .unwrap();
    // A profile-wide ambient key exists in this double, but must not be read.
    let secrets = ScopedSecrets {
        available: ego_ai::provider::BEDROCK_KEY_VAR,
        reads: Default::default(),
    };
    let refused = catalogue
        .row("office-native")
        .unwrap()
        .credential
        .resolve(&secrets)
        .unwrap_err();
    assert_eq!(refused.var, "NATIVE_BEDROCK_API_KEY");
    assert_eq!(
        *secrets.reads.lock().unwrap(),
        vec!["NATIVE_BEDROCK_API_KEY"]
    );
}

#[test]
fn native_contract_allowlist_and_facts_do_not_admit_an_inexpressible_model() {
    use ego_core::config::{ProviderModelContract, ProviderProfile};
    use ego_core::generator::capabilities::{ImageInputSupport, Transport};
    assert_eq!(
        "bedrock-runtime".parse::<ProviderProfile>().unwrap(),
        ProviderProfile::BedrockRuntime
    );
    assert_eq!(
        serde_json::from_str::<ProviderModelContract>("\"bedrock-converse\"").unwrap(),
        ProviderModelContract::BedrockConverse
    );
    assert!(ProviderProfile::BedrockRuntime.allows(ProviderModelContract::BedrockConverse));
    assert!(!ProviderProfile::BedrockRuntime.allows(ProviderModelContract::Messages));
    assert!(!ProviderProfile::OpenaiCompatible.allows(ProviderModelContract::BedrockConverse));
    let valid = native_instance();
    assert!(valid.validate().is_ok());
    let mut declared_parallel = valid.clone();
    declared_parallel
        .models
        .values_mut()
        .next()
        .unwrap()
        .facts
        .supports_parallel_tool_calls = true;
    assert!(declared_parallel.validate().is_ok());
    let encoded = serde_json::to_value(&valid).unwrap();
    assert_eq!(
        encoded["models"]["chat-looking-alias"]["facts"]["transport"],
        "amazon_event_stream"
    );
    let decoded: ego_core::config::ProviderInstance = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, valid);
    assert!(decoded.validate().is_ok());
    assert_eq!(
        serde_json::from_str::<Transport>("\"sse\"").unwrap(),
        Transport::Sse
    );
    assert_eq!(
        serde_json::from_str::<Transport>("\"web_socket\"").unwrap(),
        Transport::WebSocket
    );
    assert!(serde_json::from_str::<Transport>("\"eventstream\"").is_err());
    for change in 0..5 {
        let mut invalid = valid.clone();
        let facts = &mut invalid.models.values_mut().next().unwrap().facts;
        match change {
            0 => facts.transport = Transport::Sse,
            1 => facts.cache = CacheRetention::Short,
            2 => facts.reasoning_state_dialect = Some("private-state".into()),
            3 => facts.max_tools = None,
            _ => facts.image_input = ImageInputSupport::Unknown,
        }
        assert!(invalid.validate().is_err());
    }
    let mut tool_free = valid;
    let facts = &mut tool_free.models.values_mut().next().unwrap().facts;
    facts.max_tools = Some(0);
    facts.supports_parallel_tool_calls = false;
    assert!(tool_free.validate().is_ok());
}

#[tokio::test]
async fn dropping_generation_releases_the_original_raw_body_without_draining_it() {
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(dropped.clone());
    let body = stream::unfold(flag, |_flag| async move {
        std::future::pending::<Option<(Result<bytes::Bytes, ego_ai::http::HttpError>, DropFlag)>>()
            .await
    })
    .boxed();
    let mut generation = BedrockConverse.decode(
        Ok(HttpResponse {
            status: 200,
            headers: vec![
                HttpHeader::new(
                    "content-type",
                    b"application/vnd.amazon.eventstream".to_vec(),
                )
                .unwrap(),
            ],
            body,
        }),
        None,
    );
    assert!(matches!(
        generation.next().await,
        Some(AiEvent::DeliveryObserved(_))
    ));
    drop(generation);
    assert!(dropped.load(Ordering::SeqCst));
}

// Catches resolving Mistral by the alias or posting Chat to the native root.
#[test]
fn configured_bedrock_mistral_uses_runtime_chat_without_discovery() {
    use ego_ai::catalogue::{Catalogue, ModelSlug};
    use ego_ai::provider::ProviderRow;
    let mut instance = native_instance();
    instance.base_url = "https://bedrock-runtime.us-east-1.amazonaws.com/"
        .parse()
        .unwrap();
    let mut model = instance.models.values().next().unwrap().clone();
    // AWS model card: model-card-mistral-ai-mistral-large-3.html.
    let upstream = "mistral.mistral-large-3-675b-instruct";
    model.upstream_id = upstream.into();
    model.contract = serde_json::from_value(serde_json::json!("bedrock-chat")).unwrap();
    model.facts.transport = ego_core::generator::capabilities::Transport::Sse;
    instance
        .models
        .insert("converse-looking-alias".parse().unwrap(), model);
    let catalogue = Catalogue::new([ProviderRow::bedrock_runtime_default()])
        .with_instances(&native_providers(instance), &native_connections())
        .unwrap();
    let slug = ModelSlug::parse("office-native/converse-looking-alias").unwrap();
    let static_profile = catalogue.static_request_profile(&slug).unwrap().unwrap();
    let credential = catalogue
        .row("office-native")
        .unwrap()
        .credential
        .resolve(&ScopedSecrets {
            available: "NATIVE_BEDROCK_API_KEY",
            reads: Default::default(),
        })
        .unwrap();
    let (http, unasked) = crate::support::unasked();
    let resolved =
        futures::executor::block_on(catalogue.resolve(&slug, &http, &credential)).unwrap();
    let mut req = request();
    req.model = resolved.model;
    req.max_output_tokens = Some(1024);
    assert_eq!(static_profile.request_model(), upstream);
    static_profile.ensure_expressible(&req).unwrap();
    let built = resolved
        .wire
        .build_request(
            &RequestTarget::new(resolved.row.base(), &credential.headers()),
            &req,
        )
        .unwrap();
    assert_eq!(
        built.url.as_str(),
        "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1/chat/completions"
    );
    let body: Value = serde_json::from_slice(built.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["model"], upstream);
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["stream"], true);
    for field in [
        "max_completion_tokens",
        "parallel_tool_calls",
        "reasoning_effort",
    ] {
        assert!(body.get(field).is_none(), "{field}");
    }
    assert!(unasked.asked().is_empty());
}

// Catches accepting bearer connections for signing or signing after guarded admission.
#[test]
fn bedrock_sigv4_sends_signed_native_request_from_named_credentials() {
    use ego_ai::catalogue::{Catalogue, ModelSlug};
    use ego_ai::generator::WireGenerator;
    use ego_ai::provider::ProviderRow;
    use ego_core::budget::{BudgetGate, Metering};
    use ego_core::cost::CurrencyCode;
    use ego_core::event::id::GenerationId;
    use ego_core::generator::attempt::GenerationAttempt;
    use ego_core::generator::port::Generator;

    let mut instance = native_instance();
    instance.profile = ego_core::config::ProviderProfile::BedrockRuntimeSigv4;
    instance.base_url = "https://bedrock-runtime.us-east-2.amazonaws.com/"
        .parse()
        .unwrap();
    let model = instance.models.values_mut().next().unwrap();
    model.metering = ego_core::budget::MeteringIdentity::Unmetered {
        revision: ego_core::cost::MeteringRevision("test".into()),
    };
    let providers = native_providers(instance.clone());
    let catalogue = || Catalogue::new([ProviderRow::bedrock_runtime_default()]);
    let mut connections = native_connections();
    assert!(
        catalogue()
            .with_instances(&providers, &connections)
            .is_err()
    );
    connections.get_mut("native-account").unwrap().vendor = "bedrock-runtime-sigv4".into();
    instance.base_url = "https://bedrock-runtime.us-east-2.amazonaws.com.example.com/"
        .parse()
        .unwrap();
    assert!(
        catalogue()
            .with_instances(&native_providers(instance), &connections)
            .is_err()
    );
    let catalogue = catalogue()
        .with_instances(&providers, &connections)
        .unwrap();

    // AWS published example credentials, never an actual account.
    let credentials = crate::support::FakeCredentials::holding(
        r#"{
        "Version":1,
        "AccessKeyId":"AKIDEXAMPLE",
        "SecretAccessKey":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"
    }"#,
    );
    let credential = catalogue
        .row("office-native")
        .unwrap()
        .credential
        .resolve(&credentials)
        .unwrap();
    let (http, seen) = crate::support::unasked();
    let slug = ModelSlug::parse("office-native/chat-looking-alias").unwrap();
    let resolved =
        futures::executor::block_on(catalogue.resolve(&slug, &http, &credential)).unwrap();

    let mut req = request();
    req.model = resolved.model;
    req.tools.push(tool());
    let metering: &Metering = &resolved.metering;
    let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").unwrap());
    let reservation = gate
        .admit(metering, None, &req, &resolved.capabilities)
        .unwrap();
    let attempt = GenerationAttempt::new(
        GenerationId::new(),
        reservation,
        metering,
        None,
        &req,
        &resolved.capabilities,
    )
    .unwrap();

    let generator = WireGenerator::new(resolved.row, resolved.wire, http, credential);
    let _ = futures::executor::block_on(generator.generate(req, attempt).collect::<Vec<_>>());
    let captured = seen.asked();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].path,
        "/model/us.meta.llama4-maverick-17b-instruct-v1%3A0/converse"
    );
    let auth = captured[0]
        .headers
        .iter()
        .find(|(name, _)| name == "authorization")
        .unwrap();
    assert!(
        auth.1
            .starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
    );
    assert!(auth.1.contains("/us-east-2/bedrock/aws4_request"));
    assert!(!auth.1.contains("EXAMPLEKEY"));
}

mod sigv4_attacks;
