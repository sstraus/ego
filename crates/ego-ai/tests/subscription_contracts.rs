//! Request contracts captured from the current Pi and Hermes clients.
//!
//! These tests pin client-derived contracts, not live provider acceptance.
//! Pi source: `earendil-works/pi@4af9d21d3b4d664e4a29fcabfec85171077248e3`.
//! Hermes source: `NousResearch/hermes-agent@a0ca7c19204e514f9590ce3b812e029b315ab9e9`.
//! Both were read 2026-08-24.

use ego_ai::provider::{KIMI_CODING_BASE, ProviderRow};
use ego_ai::wire::github_copilot_completions::GithubCopilotCompletions;
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Message, Role, ThinkingLevel, ToolDescriptor,
};

fn request(messages: Vec<Message>) -> GenRequest {
    GenRequest {
        model: "gpt-4.1".to_string(),
        messages: Conversation::new(messages).expect("a valid conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(16),
        parallel_tool_calls: false,
    }
}

fn built_body(row: &ProviderRow, request: &GenRequest) -> serde_json::Value {
    let headers = vec![("authorization".to_string(), "Bearer test".to_string())];
    let target = RequestTarget::new(row.base(), &headers);
    let built = row.wire.build_request(&target, request).unwrap();
    serde_json::from_slice(built.body.as_deref().expect("a JSON body")).unwrap()
}

#[test]
fn kimi_coding_uses_the_current_coding_v1_anthropic_contract() {
    assert_eq!(KIMI_CODING_BASE, "https://api.kimi.com/coding/v1/");
    let row = ProviderRow::kimi_coding_default();
    assert_eq!(format!("{:?}", row.wire), "AnthropicMessages");
    let headers = vec![("authorization".to_string(), "Bearer test".to_string())];
    let target = RequestTarget::new(row.base(), &headers);
    let built = row
        .wire
        .build_request(&target, &request(vec![Message::text(Role::User, "say ok")]))
        .unwrap();
    assert_eq!(
        built.url.as_str(),
        "https://api.kimi.com/coding/v1/messages"
    );
    assert!(
        built
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("anthropic-version"))
    );
}

#[test]
fn copilot_sets_current_dynamic_headers_without_changing_shared_chat_bytes() {
    let row = ProviderRow::github_copilot_default();
    assert_eq!(format!("{:?}", row.wire), "GithubCopilotCompletions");
    let headers = vec![("authorization".to_string(), "Bearer test".to_string())];

    for (messages, initiator) in [
        (vec![Message::text(Role::User, "say ok")], "user"),
        (
            vec![
                Message::text(Role::User, "read it"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        call_id: "call_1".to_string(),
                        name: "read".to_string(),
                        args: "{}".to_string(),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        call_id: "call_1".to_string(),
                        output: "contents".to_string(),
                        is_error: false,
                    }],
                },
            ],
            "agent",
        ),
    ] {
        let target = RequestTarget::new(row.base(), &headers);
        let built = GithubCopilotCompletions
            .build_request(&target, &request(messages))
            .unwrap();
        assert_eq!(
            built.url.as_str(),
            "https://api.individual.githubcopilot.com/chat/completions"
        );
        assert!(built.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("x-initiator") && value == initiator
        }));
        assert!(built.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("openai-intent") && value == "conversation-edits"
        }));
    }
}

#[test]
fn qwen_portal_normalises_message_parts_and_marks_the_system_cache_breakpoint() {
    let row = ProviderRow::qwen_oauth_default();
    assert_eq!(format!("{:?}", row.wire), "QwenPortal");
    let body = built_body(
        &row,
        &request(vec![
            Message::text(Role::System, "be brief"),
            Message::text(Role::User, "say ok"),
        ]),
    );
    assert_eq!(body["vl_high_resolution_images"], true);
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(body["messages"][0]["content"][0]["text"], "be brief");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    assert_eq!(body["messages"][1]["content"][0]["type"], "text");
    assert_eq!(body["messages"][1]["content"][0]["text"], "say ok");
}

#[test]
fn minimax_oauth_and_nous_select_only_the_captured_current_shapes() {
    let minimax = ProviderRow::minimax_oauth_default();
    assert_eq!(format!("{:?}", minimax.wire), "AnthropicMessages");
    assert_eq!(
        minimax.base().as_str(),
        "https://api.minimax.io/anthropic/v1/"
    );

    let nous = ProviderRow::nous_default();
    assert_eq!(format!("{:?}", nous.wire), "NousPortal");
    let body = built_body(&nous, &request(vec![Message::text(Role::User, "say ok")]));
    assert_eq!(body["model"], "gpt-4.1");
    assert!(body.get("messages").is_some());
    assert!(body.get("input").is_none());
}

#[test]
fn captured_chat_clients_do_not_invent_a_parallel_tool_calls_field() {
    let mut req = request(vec![Message::text(Role::User, "read it")]);
    req.tools = vec![ToolDescriptor {
        name: "read_file".to_string(),
        description: "Read one file.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    }];

    for row in [
        ProviderRow::github_copilot_default(),
        ProviderRow::qwen_oauth_default(),
        ProviderRow::nous_default(),
    ] {
        let body = built_body(&row, &req);
        assert!(body.get("tools").is_some(), "{} dropped tools", row.id());
        assert!(
            body.get("parallel_tool_calls").is_none(),
            "{} invented a field its pinned client does not send: {body}",
            row.id()
        );
    }
}
