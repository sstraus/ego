//! Offline contract of the Codex Responses mapping.

use ego_ai::http::HttpResponse;
use ego_ai::provider::ProviderRow;
use ego_ai::wire::openai_codex_responses::{
    CODEX_INSTRUCTIONS, Inexpressible, OpenAiCodexResponses, PATH, request_body,
};
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
    assert_eq!(body["instructions"], CODEX_INSTRUCTIONS);
    assert_eq!(
        body["input"][0],
        serde_json::json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": "be brief"}],
        })
    );
    assert_eq!(body["input"][1]["role"], "user");
    assert_eq!(body["input"][1]["content"], "list the files");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("messages").is_none());
}

// Catches: a function tool without `strict` lets the Responses endpoint
// normalise it to strict mode and move the optional `question` of the host
// `progress` tool into `required`, so every `type=done` report is refused.
#[test]
fn function_tools_opt_out_of_strict_normalisation() {
    let parameters = serde_json::json!({
        "type": "object",
        "properties": {
            "type": {"type": "string", "enum": ["done", "blocked"]},
            "text": {"type": "string"},
            "question": {"type": "object", "properties": {"kind": {"type": "string"}}}
        },
        "required": ["type", "text"]
    });
    let mut req = request();
    req.tools[0].parameters = parameters.clone();
    let body = serde_json::from_slice::<serde_json::Value>(&request_body(&req).unwrap()).unwrap();
    assert_eq!(
        body["tools"][0]["strict"],
        serde_json::Value::Bool(false),
        "{body}"
    );
    assert_eq!(body["tools"][0]["parameters"], parameters);
}

// Catches: a replayed continuation is hoisted into either adapter's leading
// instructions, or same-role grouping loses its chronological position.
#[test]
fn goal_continuation_keeps_hoisting_adapter_prefixes_stable() {
    use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
    use ego_core::event::id::{EventId, GoalId, ItemId, Producer, Seq, TurnId};
    use ego_core::event::time::Timestamp;
    use ego_core::generator::history::conversation_from;
    use ego_core::state::SessionState;

    let mut state = SessionState::default();
    let mut seq = Seq::ZERO;
    let turn = TurnId::new();
    let mut append = |state: &mut SessionState, kind| {
        state.apply(&DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn,
                item: Some(ItemId::new()),
                parent: None,
                ts: Timestamp::from_millis(0),
                producer: Producer::Main,
            },
            kind,
        });
    };
    let goal_id = GoalId::new();
    append(
        &mut state,
        EventKind::GoalSet {
            id: goal_id,
            objective: "verify artifact".into(),
            owner: ego_core::goal::GoalOwner::Operator,
            ceilings: Default::default(),
            derived_from: None,
        },
    );
    append(
        &mut state,
        EventKind::UserPrompt {
            text: "original operator request".into(),
            images: Vec::new(),
            audio: Vec::new(),
            queued_prompt: None,
        },
    );
    let projected = |state: &SessionState| {
        let (history, repairs) = conversation_from(state).unwrap();
        assert!(repairs.is_empty());
        let mut messages = vec![Message::text(
            Role::System,
            "Active session goal: verify artifact",
        )];
        messages.extend(Vec::<Message>::from(history));
        let mut req = request();
        req.messages = Conversation::new(messages).unwrap();
        req
    };
    let before = projected(&state);
    append(
        &mut state,
        EventKind::GoalContinuation {
            id: goal_id,
            text: "Independent check found this unmet condition: missing receipt".into(),
        },
    );
    append(
        &mut state,
        EventKind::UserPrompt {
            text: "new operator evidence".into(),
            images: Vec::new(),
            audio: Vec::new(),
            queued_prompt: None,
        },
    );
    let after = projected(&state);
    for (wire, old_body, new_body, prefix, history_key) in [
        (
            "responses",
            request_body(&before).unwrap(),
            request_body(&after).unwrap(),
            "instructions",
            "input",
        ),
        (
            "messages",
            ego_ai::wire::anthropic_messages::request_body(&before).unwrap(),
            ego_ai::wire::anthropic_messages::request_body(&after).unwrap(),
            "system",
            "messages",
        ),
    ] {
        let old: serde_json::Value = serde_json::from_slice(&old_body).unwrap();
        let new: serde_json::Value = serde_json::from_slice(&new_body).unwrap();
        assert_eq!(old[prefix], new[prefix], "{wire} leading prefix changed");
        let history = new[history_key].to_string();
        let original = history.find("original operator request").unwrap();
        let continuation = history.find("Automatic goal continuation").unwrap();
        let followup = history.find("new operator evidence").unwrap();
        assert!(
            original < continuation && continuation < followup,
            "{wire}: {history}"
        );
        assert!(history.contains("not operator input"), "{wire}: {history}");
    }
}

#[test]
fn build_request_joins_the_codex_path_and_sets_the_wire_beta() {
    let base = Url::parse("https://chatgpt.com/backend-api/").unwrap();
    let headers = vec![HttpHeader::new("authorization", b"Bearer test".to_vec()).unwrap()];
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
    assert!(request.headers.iter().any(|header| {
        header.name().eq_ignore_ascii_case("openai-beta")
            && header.value() == b"responses=experimental"
    }));
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
async fn a_non_200_ends_in_the_shared_terminal_events() {
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
    // The three terminal events plus the host-safe cause (story 288-f994).
    assert_eq!(events.len(), 4);
    assert!(matches!(events[2], AiEvent::ProviderFailure { .. }));
    assert!(matches!(events[3], AiEvent::Stop(StopReason::Error)));
}

/// Malformed state must not be silently dropped or interpreted as plain text.
#[test]
fn reasoning_state_is_refused_rather_than_dropped() {
    let mut req = request();
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "list the files"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning {
                state: ReasoningState {
                    connection: None,
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
    assert_eq!(
        format!("{:?}", OpenAiCodexResponses),
        "OpenAiCodexResponses"
    );
    assert_eq!(row.base().as_str(), "https://chatgpt.com/backend-api/");
}

// Catches: Codex cache writes counted as ordinary input, or incoherent usage
// accepted after subtracting only cache reads.
#[tokio::test]
async fn codex_usage_partitions_cache_writes_and_rejects_overflow() {
    for (total, read, write, expected) in [(12, 2, 3, Some(7)), (12, 2, 11, None)] {
        let sse = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"usage\":{{\"input_tokens\":{total},\"output_tokens\":3,\"input_tokens_details\":{{\"cached_tokens\":{read},\"cache_write_tokens\":{write}}}}}}}}}\n\n"
        );
        let events: Vec<_> = OpenAiCodexResponses
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
        let usage = events.iter().find_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        });
        assert_eq!(usage.map(|usage| usage.uncached_input_tokens), expected);
        if let Some(usage) = usage {
            assert_eq!(usage.cache_write_tokens, write);
            assert_eq!(usage.cache_read_tokens, read);
        }
    }
}

#[test]
fn codex_requests_encrypted_reasoning_instead_of_losing_replay_state() {
    let body: serde_json::Value =
        serde_json::from_slice(&request_body(&request()).unwrap()).unwrap();
    assert_eq!(
        body["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
}

#[tokio::test]
async fn encrypted_reasoning_is_persistable_and_replayed_without_plaintext_substitution() {
    let item = serde_json::json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque-ciphertext","summary":[]});
    let sse = format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"type":"response.output_item.done","item":item}),
        serde_json::json!({"type":"response.completed","response":{"status":"completed"}})
    );
    let events: Vec<_> = OpenAiCodexResponses
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
    let state = events
        .into_iter()
        .find_map(|event| {
            if let AiEvent::ReasoningState(state) = event {
                Some(state)
            } else {
                None
            }
        })
        .expect("opaque state emitted");
    let durable = serde_json::to_vec(&state).unwrap();
    let mut req = request();
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "read"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning {
                state: serde_json::from_slice(&durable).unwrap(),
            }],
        },
    ])
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request_body(&req).unwrap()).unwrap();
    assert_eq!(body["input"][1], item);
}

// Story 1175-d721. Catches: the system text returning to the `instructions`
// field, which GPT-6.2 Sol never reads from the prompt cache, or a request
// shape that breaks append-only WebSocket continuation.
#[test]
fn system_text_leads_the_input_and_continuation_stays_append_only() {
    let first = request();
    let mut messages = first.messages.messages().to_vec();
    messages.push(Message::text(Role::Assistant, "listing"));
    messages.push(Message::text(Role::User, "now read one"));
    let second = GenRequest {
        messages: Conversation::new(messages).expect("conversation"),
        ..request()
    };
    let parse = |r: &GenRequest| -> serde_json::Value {
        serde_json::from_slice(&request_body(r).unwrap()).unwrap()
    };
    let (a, b) = (parse(&first), parse(&second));
    assert!(
        b["input"]
            .as_array()
            .unwrap()
            .starts_with(a["input"].as_array().unwrap()),
        "later input must extend the earlier input"
    );
    let strip = |v: &serde_json::Value| {
        let mut v = v.clone();
        v.as_object_mut().unwrap().remove("input");
        v
    };
    assert_eq!(strip(&a), strip(&b), "non-input settings must stay fixed");
}

// Story 1175-d721. Catches: a constant directive or an empty developer item
// sent when the conversation has no system text.
#[test]
fn no_system_text_sends_neither_instructions_nor_developer_item() {
    let mut req = request();
    req.messages = Conversation::new(vec![Message::text(Role::User, "list the files")]).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request_body(&req).unwrap()).unwrap();
    assert!(body.get("instructions").is_none());
    assert_eq!(
        body["input"][0],
        serde_json::json!({"role": "user", "content": "list the files"})
    );
}
