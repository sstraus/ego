//! Offline contract of the Codex Responses mapping.

use ego_ai::http::HttpResponse;
use ego_ai::provider::ProviderRow;
use ego_ai::wire::openai_codex_responses::{
    Inexpressible, OpenAiCodexResponses, PATH, request_body,
};
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Message, ReasoningState, Role, StopReason,
    ThinkingLevel, ToolDescriptor,
};
use futures::StreamExt;
use url::Url;

fn request() -> GenRequest {
    GenRequest {
        model: "gpt-5.6-sol".to_string(),
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
        thinking: ThinkingLevel::Medium,
        max_output_tokens: Some(1024),
        parallel_tool_calls: true,
    }
}

#[test]
fn the_path_is_codex_responses_not_chat_completions() {
    assert_eq!(PATH, "codex/responses");
    let body =
        serde_json::from_slice::<serde_json::Value>(&request_body(&request()).unwrap()).unwrap();
    assert_eq!(body["model"], "gpt-5.6-sol");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["input"][0]["content"], "list the files");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["max_output_tokens"], 1024);
    assert!(body.get("messages").is_none());
}

#[test]
fn build_request_joins_the_codex_path_and_sets_the_wire_beta() {
    let base = Url::parse("https://chatgpt.com/backend-api/").unwrap();
    let headers = vec![("authorization".to_string(), "Bearer test".to_string())];
    let target = RequestTarget::new(&base, &headers);
    let request = OpenAiCodexResponses
        .build_request(&target, &request())
        .unwrap();
    assert!(
        request.url.as_str().ends_with("/codex/responses"),
        "{}",
        request.url
    );
    assert!(!request.url.as_str().contains("chat/completions"));
    assert!(
        request
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("openai-beta")
                && value == "responses=experimental")
    );
}

#[tokio::test]
async fn a_text_stream_stops_once_and_reports_usage() {
    let sse = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
        "\"usage\":{\"input_tokens\":12,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n",
        "data: [DONE]\n\n",
    );
    let events: Vec<AiEvent> = OpenAiCodexResponses
        .decode(
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(sse))])),
            }),
            None,
        )
        .collect()
        .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::TextDelta { text } if text == "Hi"))
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
    let usage = events.iter().find_map(|event| match event {
        AiEvent::Usage(usage) => Some(usage),
        _ => None,
    });
    let usage = usage.expect("usage");
    assert_eq!(usage.uncached_input_tokens, 10);
    assert_eq!(usage.cache_read_tokens, 2);
    assert_eq!(usage.output_tokens, 3);
}

#[tokio::test]
async fn a_function_call_stream_ends_in_tool_use() {
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",",
        "\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"path\\\":\\\"a\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
        "\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n",
    );
    let events: Vec<AiEvent> = OpenAiCodexResponses
        .decode(
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(sse))])),
            }),
            None,
        )
        .collect()
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
async fn a_non_200_is_three_terminal_events() {
    let events: Vec<AiEvent> = OpenAiCodexResponses
        .decode(
            Ok(HttpResponse {
                status: 401,
                headers: vec![],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                    r#"{"error":{"message":"nope"}}"#,
                ))])),
            }),
            None,
        )
        .collect()
        .await;
    assert_eq!(events.len(), 3);
    assert!(matches!(events[2], AiEvent::Stop(StopReason::Error)));
}

/// The Codex Responses body has no field for an opaque reasoning signature.
/// Sending the request without it would be a silent drop of state the next
/// turn cannot reconstruct.
#[test]
fn reasoning_state_is_refused_rather_than_dropped() {
    let mut req = request();
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "list the files"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning {
                state: ReasoningState {
                    blob: "opaque".to_string(),
                    dialect: "openai-codex-responses".to_string(),
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
fn the_row_selects_this_mapping_not_completions() {
    let row = ProviderRow::openai_codex_default();
    assert_eq!(format!("{:?}", row.wire), "OpenAiCodexResponses");
    assert_eq!(row.base.as_str(), "https://chatgpt.com/backend-api/");
}
