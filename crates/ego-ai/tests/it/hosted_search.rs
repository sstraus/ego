//! Published provider examples, not live recordings; see goldens/hosted-search/README.md.
use ego_ai::http::HttpResponse;
use ego_ai::wire::{WireFormat, anthropic_messages, openai_responses};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, HostedSearch, Message, Role, StopReason, ThinkingLevel,
    ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use futures::StreamExt;
use serde_json::{Value, json};

fn request() -> GenRequest {
    GenRequest {
        cache: None,
        web_search: Some(HostedSearch {
            max_requests: std::num::NonZeroU32::new(5).unwrap(),
        }),
        model: "fixture-model".into(),
        messages: Conversation::new(vec![Message::text(Role::User, "Search the web")]).unwrap(),
        tools: vec![ToolDescriptor {
            name: "web_search".into(),
            description: "Hosted search".into(),
            parameters: json!({"type":"object"}),
        }],
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(1024),
        parallel_tool_calls: false,
    }
}

fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}

async fn decode(wire: &dyn WireFormat, transcript: &str, chunk: usize) -> Vec<AiEvent> {
    wire.decode(
        Ok(HttpResponse {
            status: 200,
            headers: vec![HttpHeader::new("content-type", b"text/event-stream".to_vec()).unwrap()],
            body: Box::pin(futures::stream::iter(
                transcript
                    .as_bytes()
                    .chunks(chunk)
                    .map(|bytes| Ok(bytes::Bytes::copy_from_slice(bytes)))
                    .collect::<Vec<_>>(),
            )),
        }),
        None,
    )
    .collect()
    .await
}

#[test]
fn hosted_declarations_catch_accidental_function_tools_and_unbounded_search() {
    let req = request();
    let openai: Value =
        serde_json::from_slice(&openai_responses::request_body(&req).unwrap()).unwrap();
    assert_eq!(openai["tools"], json!([{"type":"web_search"}]));
    assert_eq!(openai["max_tool_calls"], 5);
    let anthropic: Value =
        serde_json::from_slice(&anthropic_messages::request_body(&req).unwrap()).unwrap();
    assert_eq!(
        anthropic["tools"],
        json!([{"type":"web_search_20250305","name":"web_search","max_uses":5}])
    );
    let mut disabled = req;
    disabled.web_search = None;
    disabled.tools.clear();
    let disabled: Value =
        serde_json::from_slice(&openai_responses::request_body(&disabled).unwrap()).unwrap();
    assert!(disabled.get("tools").is_none());
    assert!(disabled.get("max_tool_calls").is_none());
}

#[tokio::test]
async fn openai_published_output_catches_lost_sources_double_dispatch_and_double_billing() {
    let output: Vec<Value> =
        serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json")).unwrap();
    let mut sse = String::new();
    for item in &output {
        if item["type"] == "message" {
            sse.push_str(&frame(
                json!({"type":"response.output_text.delta","delta":item["content"][0]["text"]}),
            ));
        }
        let done = frame(json!({"type":"response.output_item.done","item":item}));
        sse.push_str(&done);
        if item["type"] == "web_search_call" {
            sse.push_str(&done);
        }
    }
    // Terminal token fields are streaming-reference framing, not claimed to be part of the published search example.
    sse.push_str(&frame(json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":20}}})));
    for chunk in [1, 17, sse.len()] {
        let events = decode(&openai_responses::OpenAiResponses, &sse, chunk).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::ToolCall { .. }))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AiEvent::WebSearch { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(event, AiEvent::Citation { url, title } if url == "https://..." && title == "Title...")));
        assert!(
            events.iter().any(
                |event| matches!(event, AiEvent::Usage(usage) if usage.web_search_requests == 1)
            )
        );
        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
        let states: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event {
                AiEvent::ProviderContent { state, .. } => {
                    Some(ContentBlock::ProviderContent { state })
                }
                _ => None,
            })
            .collect();
        let mut replay = request();
        replay.messages = Conversation::new(vec![Message {
            role: Role::Assistant,
            content: states,
        }])
        .unwrap();
        let body: Value =
            serde_json::from_slice(&openai_responses::request_body(&replay).unwrap()).unwrap();
        assert_eq!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "web_search_call")
                .count(),
            1
        );
        assert!(body["input"].as_array().unwrap().contains(&output[0]));
        assert!(body["input"].as_array().unwrap().contains(&output[1]));
    }
}

#[tokio::test]
async fn anthropic_published_response_catches_lost_encrypted_content_citations_and_pause_turn() {
    let response: Value = serde_json::from_str(include_str!(
        "../goldens/hosted-search/anthropic-response.json"
    ))
    .unwrap();
    let mut sse = frame(json!({"type":"message_start","message":{"usage":response["usage"]}}));
    for (index, block) in response["content"].as_array().unwrap().iter().enumerate() {
        let mut start = block.clone();
        if block["type"] == "text" {
            start["text"] = json!("");
        }
        sse.push_str(&frame(
            json!({"type":"content_block_start","index":index,"content_block":start}),
        ));
        if block["type"] == "text" {
            sse.push_str(&frame(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}})));
        }
        sse.push_str(&frame(json!({"type":"content_block_stop","index":index})));
    }
    sse.push_str(&frame(json!({"type":"message_delta","delta":{"stop_reason":"pause_turn"},"usage":{"output_tokens":931,"server_tool_use":{"web_search_requests":1}}})));
    sse.push_str(&frame(json!({"type":"message_stop"})));
    for chunk in [1, 17, sse.len()] {
        let events = decode(&anthropic_messages::AnthropicMessages, &sse, chunk).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::ToolCall { .. }))
        );
        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Continue)));
        assert!(events.iter().any(|event| matches!(event, AiEvent::Usage(usage) if usage.web_search_requests == 1 && usage.uncached_input_tokens == 6039 && usage.output_tokens == 931)));
        assert!(events.iter().any(|event| matches!(event, AiEvent::Citation { url, .. } if url == "https://en.wikipedia.org/wiki/Claude_Shannon")));
        let states: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event {
                AiEvent::ProviderContent { state, .. } => {
                    Some(ContentBlock::ProviderContent { state })
                }
                _ => None,
            })
            .collect();
        let mut replay = request();
        replay.messages = Conversation::new(vec![Message {
            role: Role::Assistant,
            content: states,
        }])
        .unwrap();
        let body: Value =
            serde_json::from_slice(&anthropic_messages::request_body(&replay).unwrap()).unwrap();
        // The encrypted result and citation blocks are equal to the provider example, not reconstructed excerpts.
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert!(content.contains(&response["content"][2]));
        assert!(content.contains(&response["content"][4]));
    }
}

#[test]
fn opaque_replay_catches_json_rewriting_and_cross_provider_state_loss() {
    let mut req = request();
    let blob = "{  \"type\": \"web_search_call\", \"id\":\"ws_exact\", \"status\":\"completed\", \"action\":{\"type\":\"search\",\"query\":\"q\"}  }";
    req.messages = Conversation::new(vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ProviderContent {
            state: ego_core::generator::types::ReasoningState {
                dialect: "openai-responses".into(),
                blob: blob.into(),
            },
        }],
    }])
    .unwrap();
    let bytes = openai_responses::request_body(&req).unwrap();
    assert!(
        std::str::from_utf8(&bytes).unwrap().contains(blob),
        "opaque captured JSON must not be reserialized"
    );
    assert!(
        anthropic_messages::request_body(&req).is_err(),
        "changing providers cannot drop or reinterpret private search history"
    );
}
