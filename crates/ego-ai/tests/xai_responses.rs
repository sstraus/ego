//! Offline contract for xAI's current Responses API mapping.
//!
//! Derived from xAI's Responses, streaming, function-calling, reasoning and
//! cost-tracking documentation, read 2026-08-24. This is vendor-schema
//! evidence, not a live xAI recording.

use std::sync::Arc;

use ego_ai::http::HttpResponse;
use ego_ai::provider::ProviderRow;
use ego_ai::wire::provider_cost::XaiUsdTicks;
use ego_ai::wire::xai_responses::{Inexpressible, PATH, XaiResponses, request_body};
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Message, ReasoningState, Role, StopReason,
    ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use futures::StreamExt;
use url::Url;

fn request() -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "grok-4.6".to_string(),
        messages: Conversation::new(vec![
            Message::text(Role::System, "be brief"),
            Message::text(Role::User, "list the files"),
        ])
        .expect("system then user is a conversation"),
        tools: vec![ToolDescriptor {
            name: "read".to_string(),
            description: "read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }],
        thinking: ThinkingLevel::Low,
        max_output_tokens: Some(1024),
        parallel_tool_calls: true,
    }
}

async fn decode(sse: &str) -> Vec<AiEvent> {
    XaiResponses
        .decode(
            Ok(HttpResponse {
                status: 200,
                headers: vec![
                    HttpHeader::new("content-type", b"text/event-stream".to_vec()).expect("header"),
                ],
                body: Box::pin(futures::stream::iter(
                    sse.as_bytes()
                        .chunks(11)
                        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
                        .collect::<Vec<_>>(),
                )),
            }),
            Some(Arc::new(XaiUsdTicks::new("2026-08-24"))),
        )
        .collect()
        .await
}

#[test]
fn request_is_current_xai_responses_not_chat_completions_or_codex() {
    assert_eq!(PATH, "responses");
    let body =
        serde_json::from_slice::<serde_json::Value>(&request_body(&request()).unwrap()).unwrap();
    assert_eq!(body["model"], "grok-4.6");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][0]["content"], "be brief");
    assert_eq!(body["input"][1]["role"], "user");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(body["max_output_tokens"], 1024);
    for forbidden in ["messages", "max_tokens", "instructions"] {
        assert!(
            body.get(forbidden).is_none(),
            "xAI sent {forbidden}: {body}"
        );
    }
}

#[test]
fn request_joins_v1_responses_without_codex_beta() {
    let base = Url::parse("https://api.x.ai/v1/").unwrap();
    let headers = vec![HttpHeader::new("authorization", b"Bearer test".to_vec()).unwrap()];
    let target = RequestTarget::new(&base, &headers);
    let built = XaiResponses.build_request(&target, &request()).unwrap();
    assert_eq!(built.url.as_str(), "https://api.x.ai/v1/responses");
    assert!(!built.url.as_str().contains("chat/completions"));
    assert!(!built.url.as_str().contains("codex"));
    assert!(
        !built
            .headers
            .iter()
            .any(|header| header.name().eq_ignore_ascii_case("openai-beta"))
    );
}

#[test]
fn unsupported_current_grok_reasoning_efforts_are_refused() {
    for level in [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Max,
    ] {
        let mut req = request();
        req.thinking = level;
        assert_eq!(
            request_body(&req),
            Err(Inexpressible::ReasoningEffortNotSupported(level))
        );
    }
}

#[test]
fn opaque_reasoning_state_is_refused_rather_than_dropped() {
    let mut req = request();
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "list the files"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning {
                state: ReasoningState {
                    blob: "opaque".to_string(),
                    dialect: "xai-responses".to_string(),
                },
            }],
        },
    ])
    .expect("user then assistant is a conversation");
    assert_eq!(
        request_body(&req),
        Err(Inexpressible::ReasoningStateNotCarriable)
    );
}

#[test]
fn a_tool_round_trip_uses_responses_input_items_without_chat_messages() {
    let mut req = request();
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "read it"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                call_id: "call_1".to_string(),
                name: "read".to_string(),
                args: r#"{"path":"src/main.rs"}"#.to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "call_1".to_string(),
                output: "fn main() {}".to_string(),
                is_error: false,
            }],
        },
    ])
    .expect("a complete tool round trip is a conversation");

    let body: serde_json::Value = serde_json::from_slice(&request_body(&req).unwrap()).unwrap();
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][1]["call_id"], "call_1");
    assert_eq!(body["input"][1]["arguments"], r#"{"path":"src/main.rs"}"#);
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["input"][2]["call_id"], "call_1");
    assert_eq!(body["input"][2]["output"], "fn main() {}");
    assert!(body.get("messages").is_none());
}

#[tokio::test]
async fn text_reasoning_usage_and_cost_ticks_are_all_preserved_and_stop_once() {
    let events = decode(concat!(
        "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"checking\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
        "\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":2},",
        "\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":4},",
        "\"num_sources_used\":0,\"num_server_side_tools_used\":0,",
        "\"cost_in_usd_ticks\":12345678}}}\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::ReasoningDelta { text } if text == "checking"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::TextDelta { text } if text == "Hi"))
    );
    let usage = events
        .iter()
        .find_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("usage");
    assert_eq!(usage.uncached_input_tokens, 10);
    assert_eq!(usage.cache_read_tokens, 2);
    assert_eq!(usage.output_tokens, 7);
    assert_eq!(usage.reasoning_tokens, Some(4));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AiEvent::ProviderCost { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AiEvent::Stop(_)))
            .count(),
        1
    );
    assert!(matches!(
        events.last(),
        Some(AiEvent::Stop(StopReason::Stop))
    ));
}

#[tokio::test]
async fn absent_reasoning_usage_is_not_invented_as_zero() {
    let events = decode(concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
        "\"usage\":{\"input_tokens\":2,\"output_tokens\":1,\"cost_in_usd_ticks\":0}}}\n\n",
    ))
    .await;
    let usage = events
        .iter()
        .find_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("usage");
    assert_eq!(usage.reasoning_tokens, None);
}

#[tokio::test]
async fn streamed_function_call_arguments_are_assembled_before_tool_use_stop() {
    let events = decode(concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",",
        "\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",",
        "\"delta\":\"{\\\"path\\\":\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",",
        "\"delta\":\"\\\"a\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
        "\"usage\":{\"input_tokens\":4,\"output_tokens\":2,",
        "\"output_tokens_details\":{\"reasoning_tokens\":0},\"cost_in_usd_ticks\":0}}}\n\n",
    ))
    .await;
    assert!(events.iter().any(|event| matches!(
        event,
        AiEvent::ToolCall { call_id, name, args }
            if call_id == "call_1" && name == "read" && args == "{\"path\":\"a\"}"
    )));
    assert!(matches!(
        events.last(),
        Some(AiEvent::Stop(StopReason::ToolUse))
    ));
}

#[tokio::test]
async fn response_failed_and_in_band_error_are_terminal_errors() {
    for sse in [
        "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"generation failed\"}}}\n\n",
        "data: {\"type\":\"error\",\"code\":\"permission_denied\",\"message\":\"stream refused\"}\n\n",
    ] {
        let events = decode(sse).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::Error { .. }))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AiEvent::Stop(StopReason::Error)))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(AiEvent::Stop(StopReason::Error))
        ));
    }
}

#[tokio::test]
async fn non_2xx_uses_the_shared_failure_contract() {
    let events: Vec<AiEvent> = XaiResponses
        .decode(
            Ok(HttpResponse {
                status: 401,
                headers: vec![],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                    r#"{"error":{"message":"no entitlement"}}"#,
                ))])),
            }),
            None,
        )
        .collect()
        .await;
    // The three terminal events plus the host-safe cause (regression).
    assert_eq!(events.len(), 4);
    assert!(matches!(events[2], AiEvent::ProviderFailure { .. }));
    assert!(matches!(events[3], AiEvent::Stop(StopReason::Error)));
}

#[test]
fn xai_oauth_row_selects_xai_responses_and_cost_ticks() {
    let row = ProviderRow::xai_oauth_default();
    assert_eq!(format!("{:?}", XaiResponses), "XaiResponses");
    assert_eq!(row.base().as_str(), "https://api.x.ai/v1/");
    assert_eq!(
        row.cost
            .as_ref()
            .expect("xAI Responses documents cost ticks")
            .revision()
            .field,
        "usage.cost_in_usd_ticks"
    );
}
