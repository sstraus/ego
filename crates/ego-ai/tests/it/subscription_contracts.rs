//! Recorded subscription request contracts; not live provider acceptance.
//!
//! These tests pin client-derived contracts, not live provider acceptance.

use ego_ai::provider::{KIMI_CODING_BASE, ProviderRow};
use ego_ai::wire::anthropic_messages::AnthropicMessages;
use ego_ai::wire::github_copilot_completions::GithubCopilotCompletions;
use ego_ai::wire::kimi_anthropic::KimiAnthropic;
use ego_ai::wire::nous_portal::NousPortal;
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Message, Role, ThinkingLevel, ToolDescriptor,
};

fn request(messages: Vec<Message>) -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "gpt-4.1".to_string(),
        messages: Conversation::new(messages).expect("a valid conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(16),
        parallel_tool_calls: false,
    }
}

fn built_body(
    row: &ProviderRow,
    wire: &'static dyn WireFormat,
    request: &GenRequest,
) -> serde_json::Value {
    let headers =
        vec![ego_core::permit::HttpHeader::new("authorization", b"Bearer test".to_vec()).unwrap()];
    let target = RequestTarget::new(row.base(), &headers);
    let built = wire.build_request(&target, request).unwrap();
    serde_json::from_slice(built.body.as_deref().expect("a JSON body")).unwrap()
}

#[test]
fn kimi_coding_uses_the_current_coding_v1_anthropic_contract() {
    assert_eq!(KIMI_CODING_BASE, "https://api.kimi.com/coding/v1/");
    let row = ProviderRow::kimi_coding_default();
    assert_eq!(format!("{:?}", KimiAnthropic), "KimiAnthropic");
    let headers =
        vec![ego_core::permit::HttpHeader::new("authorization", b"Bearer test".to_vec()).unwrap()];
    let target = RequestTarget::new(row.base(), &headers);
    let built = KimiAnthropic
        .build_request(&target, &request(vec![Message::text(Role::User, "say ok")]))
        .unwrap();
    assert_eq!(
        built.url.as_str(),
        "https://api.kimi.com/coding/v1/messages?beta=true"
    );
    assert!(
        built
            .headers
            .iter()
            .any(|header| header.name().eq_ignore_ascii_case("anthropic-version"))
    );
}

#[test]
fn copilot_sets_current_dynamic_headers_without_changing_shared_chat_bytes() {
    let row = ProviderRow::github_copilot_default();
    assert_eq!(
        format!("{:?}", GithubCopilotCompletions),
        "GithubCopilotCompletions"
    );
    let headers =
        vec![ego_core::permit::HttpHeader::new("authorization", b"Bearer test".to_vec()).unwrap()];

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
        assert!(built.headers.iter().any(|header| {
            header.name().eq_ignore_ascii_case("x-initiator")
                && header.value() == initiator.as_bytes()
        }));
        assert!(built.headers.iter().any(|header| {
            header.name().eq_ignore_ascii_case("openai-intent")
                && header.value() == b"conversation-edits"
        }));
    }
}

#[test]
fn minimax_oauth_and_nous_select_only_the_captured_current_shapes() {
    let minimax = ProviderRow::minimax_oauth_default();
    assert_eq!(format!("{:?}", AnthropicMessages), "AnthropicMessages");
    assert_eq!(
        minimax.base().as_str(),
        "https://api.minimax.io/anthropic/v1/"
    );

    let nous = ProviderRow::nous_default();
    assert_eq!(format!("{:?}", NousPortal), "NousPortal");
    let body = built_body(
        &nous,
        &NousPortal,
        &request(vec![Message::text(Role::User, "say ok")]),
    );
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
        ProviderRow::nous_default(),
    ] {
        let wire: &'static dyn WireFormat = if row.id() == "github-copilot" {
            &GithubCopilotCompletions
        } else {
            &NousPortal
        };
        let body = built_body(&row, wire, &req);
        assert!(body.get("tools").is_some(), "{} dropped tools", row.id());
        assert!(
            body.get("parallel_tool_calls").is_none(),
            "{} invented a field its pinned client does not send: {body}",
            row.id()
        );
    }
}
