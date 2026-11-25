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
            allow_live_access: true,
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
                    Some(ContentBlock::ProviderContent { state, text: None })
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
                    Some(ContentBlock::ProviderContent { state, text: None })
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

/// The published example, re-cut the way a live stream carries it: the search input as two
/// `input_json_delta` frames and every text citation as its own `citations_delta` frame.
fn streamed_transcript(response: &Value, null_citations_at_start: bool) -> String {
    let mut sse = frame(json!({"type":"message_start","message":{"usage":response["usage"]}}));
    for (index, block) in response["content"].as_array().unwrap().iter().enumerate() {
        let mut start = block.clone();
        let mut deltas = Vec::new();
        match block["type"].as_str().unwrap() {
            "text" => {
                start["text"] = json!("");
                start.as_object_mut().unwrap().remove("citations");
                if null_citations_at_start && block.get("citations").is_some() {
                    start["citations"] = Value::Null;
                }
                deltas.push(json!({"type":"text_delta","text":block["text"]}));
                for citation in block["citations"].as_array().into_iter().flatten() {
                    deltas.push(json!({"type":"citations_delta","citation":citation}));
                }
            }
            "server_tool_use" => {
                start["input"] = json!({});
                let input = block["input"].to_string();
                let (head, tail) = input.split_at(input.len() / 2);
                for part in [head, tail] {
                    deltas.push(json!({"type":"input_json_delta","partial_json":part}));
                }
            }
            _ => {}
        }
        sse.push_str(&frame(
            json!({"type":"content_block_start","index":index,"content_block":start}),
        ));
        for delta in deltas {
            sse.push_str(&frame(
                json!({"type":"content_block_delta","index":index,"delta":delta}),
            ));
        }
        sse.push_str(&frame(json!({"type":"content_block_stop","index":index})));
    }
    sse.push_str(&frame(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":931,"server_tool_use":{"web_search_requests":1}}})));
    sse.push_str(&frame(json!({"type":"message_stop"})));
    sse
}

#[tokio::test]
async fn anthropic_streamed_deltas_catch_split_search_input_and_late_citations_being_lost() {
    let response: Value = serde_json::from_str(include_str!(
        "../goldens/hosted-search/anthropic-response.json"
    ))
    .unwrap();
    let blocks = response["content"].as_array().unwrap();
    let expected_text: String = blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .map(|block| block["text"].as_str().unwrap())
        .collect();
    for null_start in [false, true] {
        let sse = streamed_transcript(&response, null_start);
        for chunk in [1, 17, sse.len()] {
            let events = decode(&anthropic_messages::AnthropicMessages, &sse, chunk).await;
            let context = format!("null_start={null_start} chunk={chunk}");
            assert_eq!(
                events.last(),
                Some(&AiEvent::Stop(StopReason::Stop)),
                "{context}"
            );
            // Query: only reachable by joining the two partial JSON frames.
            let searches: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AiEvent::WebSearch { call_id, query } => {
                        Some((call_id.as_str(), query.as_str()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                searches,
                vec![(
                    "srvtoolu_01WYG3ziw53XMcoyKL4XcZmE",
                    "claude shannon birth date"
                )],
                "{context}"
            );
            // Citations: only reachable from `citations_delta`; the start block carried none.
            let citations: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AiEvent::Citation { url, title } => Some((url.as_str(), title.as_str())),
                    _ => None,
                })
                .collect();
            assert_eq!(
                citations,
                vec![(
                    "https://en.wikipedia.org/wiki/Claude_Shannon",
                    "Claude Shannon - Wikipedia"
                )],
                "{context}"
            );
            let visible: String = events
                .iter()
                .filter_map(|event| match event {
                    AiEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(visible, expected_text, "{context}");
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AiEvent::ToolCall { .. })),
                "{context}"
            );
            // Opaque replay: the reassembled blocks equal the published ones as JSON values.
            let states: Vec<_> = events
                .into_iter()
                .filter_map(|event| match event {
                    AiEvent::ProviderContent { state, .. } => {
                        Some(ContentBlock::ProviderContent { state, text: None })
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
                serde_json::from_slice(&anthropic_messages::request_body(&replay).unwrap())
                    .unwrap();
            let replayed = body["messages"][0]["content"].as_array().unwrap();
            assert_eq!(replayed.len(), 4, "{context}");
            assert_eq!(
                replayed[0], blocks[1],
                "search input reassembled: {context}"
            );
            assert_eq!(replayed[1], blocks[2], "encrypted result: {context}");
            assert_eq!(replayed[2], blocks[3], "uncited text: {context}");
            assert_eq!(replayed[3], blocks[4], "citations reattached: {context}");
        }
    }
}

// NS-F3 regressions: observed search calls remain accountable without terminal usage.
mod cut_search_usage {
    use super::*;

    fn response() -> Value {
        let mut response: Value = serde_json::from_str(include_str!(
            "../goldens/hosted-search/anthropic-response.json"
        ))
        .unwrap();
        // A streaming start reports input figures before any searches execute.
        response["usage"]["output_tokens"] = json!(0);
        response["usage"]["server_tool_use"]["web_search_requests"] = json!(0);
        response
    }

    fn cut_after_search(response: &Value) -> String {
        let sse = streamed_transcript(response, false);
        let search_index = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .position(|block| block["type"] == "server_tool_use")
            .unwrap();
        let stop = frame(json!({"type":"content_block_stop","index":search_index}));
        let end = sse.find(&stop).unwrap() + stop.len();
        sse[..end].to_owned()
    }

    fn before_message_delta(sse: &str) -> &str {
        let terminal = frame(
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":931,"server_tool_use":{"web_search_requests":1}}}),
        );
        sse.split_once(&terminal).unwrap().0
    }

    fn search_counts(events: &[AiEvent]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|event| match event {
                AiEvent::Usage(usage) => Some(usage.web_search_requests),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn anthropic_f3_cut_catches_completed_search_count_lost_before_message_delta() {
        let response = response();
        let cut = cut_after_search(&response);
        // No result block is needed to count a completed search attempt.
        for chunk in [1, 17, cut.len()] {
            let events = decode(&anthropic_messages::AnthropicMessages, &cut, chunk).await;
            assert_eq!(search_counts(&events), vec![1], "chunk={chunk}");
            assert!(events.iter().any(|event| matches!(
                event,
                AiEvent::WebSearch { call_id, query }
                    if call_id == "srvtoolu_01WYG3ziw53XMcoyKL4XcZmE"
                        && query == "claude shannon birth date"
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                AiEvent::Usage(usage) if usage.uncached_input_tokens == 6039
                    && usage.output_tokens == 0
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                AiEvent::Error { message, .. } if message.contains("ended between events")
            )));
            assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
        }
    }

    #[tokio::test]
    async fn anthropic_f3_counts_catch_provider_sum_underreport_and_duplicate_call_billing() {
        let mut response = response();
        let search = response["content"][1].clone();
        let mut second_search = search.clone();
        second_search["id"] = json!("srvtoolu_second");
        // Same ID repeated, then a distinct ID with the same query: two requests, not three.
        response["content"] = json!([search.clone(), search, second_search]);
        let sse = streamed_transcript(&response, false);
        let prefix = before_message_delta(&sse);
        for (provider, expected) in [(0, 2), (1, 2), (2, 2), (3, 3)] {
            let mut complete = prefix.to_owned();
            complete.push_str(&frame(json!({"type":"message_delta",
                "delta":{"stop_reason":"end_turn"},
                "usage":{"output_tokens":931,"server_tool_use":{"web_search_requests":provider}}
            })));
            complete.push_str(&frame(json!({"type":"message_stop"})));
            let events = decode(&anthropic_messages::AnthropicMessages, &complete, 17).await;
            assert_eq!(
                search_counts(&events),
                vec![expected],
                "provider={provider}"
            );
            assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
        }
    }

    #[tokio::test]
    async fn anthropic_f3_absent_input_catches_fabricated_usage_and_search_free_overcount() {
        let mut without_input = response();
        without_input.as_object_mut().unwrap().remove("usage");
        let cut = cut_after_search(&without_input);
        let events = decode(&anthropic_messages::AnthropicMessages, &cut, 17).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::WebSearch { .. }))
        );
        assert!(search_counts(&events).is_empty());

        let mut no_search = response();
        no_search["content"] = json!([no_search["content"][0].clone()]);
        let sse = streamed_transcript(&no_search, false);
        let cut = before_message_delta(&sse);
        let events = decode(&anthropic_messages::AnthropicMessages, cut, 17).await;
        assert_eq!(search_counts(&events), vec![0]);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::WebSearch { .. }))
        );
    }
}

// Source-derived contract inputs, NOT subscription/xAI live recordings. Shared item
// vocabulary and route differences are pinned in the native-search route report.
mod subscription_and_xai {
    use super::*;
    use ego_ai::wire::{openai_codex_responses, xai_responses};

    fn body(req: &GenRequest, subscription: bool) -> Value {
        let bytes = if subscription {
            openai_codex_responses::request_body(req).unwrap()
        } else {
            xai_responses::request_body(req).unwrap()
        };
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn native_search_bound_is_sent_on_every_route() {
        let mut req = request();
        req.thinking = ThinkingLevel::Low;
        req.web_search.as_mut().unwrap().max_requests = std::num::NonZeroU32::MIN;

        let openai: Value =
            serde_json::from_slice(&openai_responses::request_body(&req).unwrap()).unwrap();
        assert_eq!(openai["max_tool_calls"], 1);
        let anthropic: Value =
            serde_json::from_slice(&anthropic_messages::request_body(&req).unwrap()).unwrap();
        assert_eq!(anthropic["tools"][0]["max_uses"], 1);
        // xAI documents a count of agentic turns, not of individual searches.
        assert_eq!(body(&req, false)["max_turns"], 1, "xAI Responses");
    }

    #[test]
    fn native_search_routes_catch_local_dispatch_retention_and_invented_count_bounds() {
        let mut req = request();
        req.thinking = ThinkingLevel::Low;
        for subscription in [true, false] {
            let encoded = body(&req, subscription);
            let expected = if subscription {
                json!([{"type":"web_search","external_web_access":true}])
            } else {
                json!([{"type":"web_search"}])
            };
            assert_eq!(encoded["tools"], expected);
            assert_eq!(encoded["store"], false);
            assert_eq!(encoded["include"], json!(["reasoning.encrypted_content"]));
            assert!(encoded.get("max_tool_calls").is_none());
            if subscription {
                assert!(encoded.get("max_turns").is_none());
            } else {
                assert_eq!(encoded["max_turns"], 5);
            }
            let mut disabled = req.clone();
            disabled.web_search = None;
            disabled.tools.clear();
            let disabled = body(&disabled, subscription);
            assert!(disabled.get("tools").is_none());
            assert!(disabled.get("max_turns").is_none());
        }
        assert!(openai_codex_responses::OpenAiCodexResponses.supports_web_search());
        assert!(xai_responses::XaiResponses.supports_web_search());
    }

    // Catches duplicate counting, parsing past the cutoff in one chunk, or
    // continuing to poll the response body after the configured limit.
    #[tokio::test]
    async fn subscription_search_cutoff_drops_body_at_distinct_search_limit() {
        let output: Vec<Value> =
            serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json"))
                .unwrap();
        let search = |id: &str| {
            let mut item = output[0].clone();
            item["id"] = json!(id);
            frame(json!({"type":"response.output_item.done","item":item}))
        };
        let start = |id: &str| {
            frame(json!({"type":"response.output_item.added","item":{
                "type":"web_search_call","id":id,"status":"in_progress"
            }}))
        };
        let first_start = start("ws_first");
        let first = search("ws_first");
        let second = search("ws_second");
        let third = frame(json!({"type":"response.output_item.added","item":{
            "type":"web_search_call","id":"ws_third","status":"in_progress",
            "action":{"type":"search"}
        }}));
        let after_start =
            frame(json!({"type":"response.output_text.delta","delta":"after cutoff"}));
        let mut req = request();
        req.web_search.as_mut().unwrap().max_requests = std::num::NonZeroU32::new(2).unwrap();
        let (drop_tx, mut drop_rx) = futures::channel::oneshot::channel::<()>();
        let mut chunks = [
            format!("{first_start}{first_start}{first}{first}"),
            format!("{second}{third}{after_start}{}", search("ws_third")),
        ]
        .into_iter();
        let stream = futures::stream::poll_fn(move |_| {
            let _keep_alive = &drop_tx;
            let chunk = chunks.next().expect("body polled after the search limit");
            std::task::Poll::Ready(Some(Ok(bytes::Bytes::from(chunk))))
        });
        let wire = openai_codex_responses::OpenAiCodexResponses;
        let mut events = wire.decode_generation(
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: Box::pin(stream),
            }),
            None,
            req.web_search,
        );
        let mut calls = Vec::new();
        let mut refusals = 0;
        let mut stopped = false;
        while let Some(event) = events.next().await {
            match event {
                AiEvent::WebSearch { call_id, .. } => calls.push(call_id),
                AiEvent::Error { message, kind } => {
                    assert_eq!(kind, ego_core::generator::types::ErrorKind::LocalLimit);
                    assert!(message.contains("exceeded the local limit of 2 searches"));
                    refusals += 1;
                }
                AiEvent::Stop(reason) => {
                    assert_eq!(reason, StopReason::Error);
                    assert!(drop_rx.try_recv().is_err(), "body still held at stop");
                    stopped = true;
                    assert!(events.next().await.is_none());
                    break;
                }
                AiEvent::TextDelta { text } => panic!("read past search start: {text}"),
                _ => {}
            }
        }
        assert_eq!(calls, ["ws_first", "ws_second"]);
        assert_eq!(refusals, 1);
        assert!(stopped, "cutoff must emit a terminal stop");
    }

    // Catches an off-by-one cutoff: a budget of N allows N searches, so a turn
    // that uses exactly N must keep its answer, citations, usage and Stop.
    #[tokio::test]
    async fn subscription_search_cutoff_keeps_answer_when_searches_equal_limit() {
        let output: Vec<Value> =
            serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json"))
                .unwrap();
        let transcript = format!(
            "{}{}{}",
            frame(json!({"type":"response.output_item.done","item":output[0]})),
            frame(json!({"type":"response.output_item.done","item":output[1]})),
            frame(
                json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":20}}})
            )
        );
        let mut req = request();
        req.web_search.as_mut().unwrap().max_requests = std::num::NonZeroU32::MIN;
        let body = futures::stream::iter([Ok(bytes::Bytes::from(transcript))]);
        let events: Vec<AiEvent> = openai_codex_responses::OpenAiCodexResponses
            .decode_generation(
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: Box::pin(body),
                }),
                None,
                req.web_search,
            )
            .collect()
            .await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::Error { .. })),
            "one search within a budget of one was refused: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::Citation { .. }))
        );
        assert!(
            events.iter().any(
                |event| matches!(event, AiEvent::Usage(usage) if usage.web_search_requests == 1)
            )
        );
        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
    }

    // Catches counting an action-less start as a search: open_page and
    // find_in_page calls start without an action, so a turn with exactly N
    // searches plus one page open would lose its answer, citations and usage.
    #[tokio::test]
    async fn subscription_search_cutoff_ignores_page_actions_that_start_without_action() {
        let output: Vec<Value> =
            serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json"))
                .unwrap();
        let start = |id: &str| {
            frame(json!({"type":"response.output_item.added","item":{
                "type":"web_search_call","id":id,"status":"in_progress"
            }}))
        };
        let mut search = output[0].clone();
        search["id"] = json!("ws_search");
        let mut open = output[0].clone();
        open["id"] = json!("ws_open");
        open["action"] = json!({"type":"open_page","url":"https://example.com"});
        let transcript = format!(
            "{}{}{}{}{}{}",
            start("ws_search"),
            frame(json!({"type":"response.output_item.done","item":search})),
            start("ws_open"),
            frame(json!({"type":"response.output_item.done","item":open})),
            frame(json!({"type":"response.output_item.done","item":output[1]})),
            frame(
                json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":20}}})
            )
        );
        let mut req = request();
        req.web_search.as_mut().unwrap().max_requests = std::num::NonZeroU32::MIN;
        let body = futures::stream::iter([Ok(bytes::Bytes::from(transcript))]);
        let events: Vec<AiEvent> = openai_codex_responses::OpenAiCodexResponses
            .decode_generation(
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: Box::pin(body),
                }),
                None,
                req.web_search,
            )
            .collect()
            .await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::Error { .. })),
            "a page open was counted as a search: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::Citation { .. }))
        );
        assert!(
            events.iter().any(
                |event| matches!(event, AiEvent::Usage(usage) if usage.web_search_requests == 1)
            )
        );
        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
    }

    #[tokio::test]
    async fn native_search_routes_catch_lost_query_variants_citations_and_foreign_replay() {
        let output: Vec<Value> =
            serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json"))
                .unwrap();
        for subscription in [true, false] {
            let (wire, dialect): (&dyn WireFormat, &str) = if subscription {
                (
                    &openai_codex_responses::OpenAiCodexResponses,
                    "openai-codex-responses",
                )
            } else {
                (&xai_responses::XaiResponses, "xai-responses")
            };
            for action in [
                json!({"type":"search","query":"one query"}),
                json!({"type":"search","queries":["one", "two"]}),
                json!({"type":"search"}),
                json!({"type":"open_page","url":"https://example.com"}),
            ] {
                let mut search = output[0].clone();
                search["action"] = action.clone();
                // Duplicate terminal item tests idempotent observation/replay, not extra calls.
                let search_frame = frame(json!({"type":"response.output_item.done","item":search}));
                let transcript = format!(
                    "{search_frame}{search_frame}{}{}",
                    frame(json!({"type":"response.output_item.done","item":output[1]})),
                    frame(
                        json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":20}}})
                    )
                );
                for chunk in [1, 17, transcript.len()] {
                    let events = decode(wire, &transcript, chunk).await;
                    let expected_count = u64::from(action["type"] == "search");
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| matches!(event, AiEvent::WebSearch { .. }))
                            .count(),
                        expected_count as usize
                    );
                    let expected_query = match action.get("query").and_then(Value::as_str) {
                        Some(query) => query,
                        None if action.get("queries").is_some() => "one; two",
                        None => "web search",
                    };
                    if expected_count == 1 {
                        assert!(events.iter().any(|event| matches!(event, AiEvent::WebSearch { query, .. } if query == expected_query)));
                    }
                    assert!(events.iter().any(|event| matches!(event, AiEvent::Citation { url, .. } if url == "https://...")));
                    assert!(
                        !events
                            .iter()
                            .any(|event| matches!(event, AiEvent::ToolCall { .. }))
                    );
                    assert!(events.iter().any(|event| matches!(event, AiEvent::Usage(usage) if usage.web_search_requests == expected_count)));
                    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Stop)));
                    let states: Vec<_> = events
                        .into_iter()
                        .filter_map(|event| match event {
                            AiEvent::ProviderContent { state, .. } => Some(state),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(states.len(), 2);
                    assert!(states.iter().all(|state| state.dialect == dialect));
                    let mut replay = request();
                    replay.thinking = ThinkingLevel::Low;
                    replay.messages = Conversation::new(vec![Message {
                        role: Role::Assistant,
                        content: states
                            .iter()
                            .cloned()
                            .map(|state| ContentBlock::ProviderContent { state, text: None })
                            .collect(),
                    }])
                    .unwrap();
                    let replayed = body(&replay, subscription);
                    assert_eq!(replayed["input"], json!([search, output[1]]));
                    // Same JSON is not sufficient to transfer private state between dialects.
                    if subscription {
                        assert!(xai_responses::request_body(&replay).is_err());
                    } else {
                        assert!(openai_codex_responses::request_body(&replay).is_err());
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn native_search_idless_items_catch_private_json_leaking_into_public_call_labels() {
        let output: Vec<Value> =
            serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json"))
                .unwrap();
        let mut item = output[0].clone();
        item.as_object_mut().unwrap().remove("id");
        item["encrypted_content"] = json!("private-test-ciphertext");
        let done = frame(json!({"type":"response.output_item.done","item":item}));
        let transcript = format!("{done}{done}");
        let wires: [&dyn WireFormat; 2] = [
            &openai_codex_responses::OpenAiCodexResponses,
            &xai_responses::XaiResponses,
        ];
        for wire in wires {
            let events = decode(wire, &transcript, 1).await;
            let labels: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AiEvent::WebSearch { call_id, .. } => Some(call_id.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(labels, vec!["search-1"]);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AiEvent::ProviderContent { .. }))
                    .count(),
                1
            );
            assert!(events.iter().any(|event| matches!(event,
                AiEvent::ProviderContent { state, .. } if serde_json::from_str::<Value>(&state.blob).unwrap() == item)));
        }
    }

    #[tokio::test]
    async fn xai_search_usage_catches_attempts_overriding_successful_zero_and_lost_encryption() {
        let output: Vec<Value> =
            serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json"))
                .unwrap();
        // Schema-derived encrypted reasoning input, not a provider capture or valid ciphertext.
        let reasoning = json!({"type":"reasoning","id":"rs_contract","encrypted_content":"test-ciphertext","summary":[]});
        for supplied in [None, Some(0), Some(3)] {
            let mut usage = json!({"input_tokens":10,"output_tokens":20});
            if let Some(count) = supplied {
                usage["server_side_tool_usage_details"] =
                    json!({"web_search_calls":count,"other_tool_calls":99});
            }
            let transcript = format!(
                "{}{}{}",
                frame(json!({"type":"response.output_item.done","item":output[0]})),
                frame(json!({"type":"response.output_item.done","item":reasoning})),
                frame(
                    json!({"type":"response.completed","response":{"status":"completed","usage":usage}})
                )
            );
            let events = decode(&xai_responses::XaiResponses, &transcript, 1).await;
            assert!(events.iter().any(|event| matches!(event, AiEvent::Usage(usage) if usage.web_search_requests == supplied.unwrap_or(1))));
            let state = events
                .into_iter()
                .find_map(|event| match event {
                    AiEvent::ReasoningState(state) => Some(state),
                    _ => None,
                })
                .expect("encrypted reasoning preserved");
            let mut replay = request();
            replay.thinking = ThinkingLevel::Low;
            replay.messages = Conversation::new(vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning { state }],
            }])
            .unwrap();
            assert_eq!(body(&replay, false)["input"], json!([reasoning]));
        }
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
                connection: None,
                dialect: "openai-responses".into(),
                blob: blob.into(),
            },
            text: None,
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

// Offline serialization only: provider acceptance of search history without a
// current search declaration is a separate, unverified conformance contract.
mod ancillary_replay {
    use super::*;
    use ego_ai::wire::{openai_codex_responses, xai_responses};

    fn context(events: Vec<AiEvent>) -> GenRequest {
        let states = events
            .into_iter()
            .filter_map(|event| match event {
                AiEvent::ProviderContent { state, .. } => {
                    Some(ContentBlock::ProviderContent { state, text: None })
                }
                _ => None,
            })
            .collect();
        let mut req = request();
        req.web_search = None;
        req.tools.clear();
        req.thinking = ThinkingLevel::Low;
        req.messages = Conversation::new(vec![
            Message::text(Role::User, "Original search question"),
            Message {
                role: Role::Assistant,
                content: states,
            },
            Message::text(
                Role::User,
                "Summarize the existing evidence without searching",
            ),
        ])
        .unwrap();
        req
    }

    // Catches: disabling new search drops existing opaque output/citations or
    // accidentally reintroduces a native/function declaration at serialization.
    // The xAI/subscription inputs project a published public Responses example;
    // they are not recordings from those routes and do not prove acceptance.
    #[tokio::test]
    async fn responses_ancillary_replay_preserves_published_items_without_search_declarations() {
        let output: Vec<Value> =
            serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json"))
                .unwrap();
        let mut transcript = String::new();
        for item in &output {
            transcript.push_str(&frame(
                json!({"type":"response.output_item.done","item":item}),
            ));
        }
        transcript.push_str(&frame(json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":20}}})));
        let wires: [(&str, &dyn WireFormat); 3] = [
            ("public", &openai_responses::OpenAiResponses),
            (
                "subscription",
                &openai_codex_responses::OpenAiCodexResponses,
            ),
            ("xai", &xai_responses::XaiResponses),
        ];
        for (route, wire) in wires {
            let req = context(decode(wire, &transcript, 17).await);
            let bytes = match route {
                "public" => openai_responses::request_body(&req)
                    .expect("public offline request is expressible"),
                "subscription" => openai_codex_responses::request_body(&req)
                    .expect("subscription offline request is expressible"),
                _ => xai_responses::request_body(&req).expect("xai offline request is expressible"),
            };
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(body.get("tools").is_none(), "{route}");
            assert!(body.get("max_tool_calls").is_none(), "{route}");
            let input = body["input"].as_array().unwrap();
            assert_eq!(
                &input[1..3],
                output.as_slice(),
                "{route}: exact provider items"
            );
            assert!(
                input
                    .last()
                    .unwrap()
                    .to_string()
                    .contains("Summarize the existing evidence"),
                "{route}"
            );
            assert_eq!(body["store"], false, "{route}");
        }
    }

    // Catches: removal of current search activation corrupts encrypted prior
    // search results or citation indices required by the published replay rule.
    #[tokio::test]
    async fn messages_ancillary_replay_preserves_published_encrypted_blocks_without_search_declarations()
     {
        let response: Value = serde_json::from_str(include_str!(
            "../goldens/hosted-search/anthropic-response.json"
        ))
        .unwrap();
        let transcript = streamed_transcript(&response, false);
        let req = context(decode(&anthropic_messages::AnthropicMessages, &transcript, 17).await);
        let body: Value = serde_json::from_slice(
            &anthropic_messages::request_body(&req).expect("offline request is expressible"),
        )
        .unwrap();
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"][1]["role"], "assistant");
        // Initial uncited text uses TextDelta rather than ProviderContent. The
        // remaining four captured blocks include encrypted results and citations.
        assert_eq!(
            body["messages"][1]["content"],
            json!([
                response["content"][1],
                response["content"][2],
                response["content"][3],
                response["content"][4],
            ])
        );
        assert!(
            body["messages"][2]
                .to_string()
                .contains("Summarize the existing evidence")
        );
    }
}
