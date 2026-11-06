//! Contract: one malformed provider answer, every adapter, one outcome.
//!
//! Each case rewrites the same tool-call golden of every adapter into the same
//! malformed answer, then runs it through the production pipeline: the
//! adapter's `decode` and the request-local tool-name stage. The adapters
//! only translate, so the outcome must not depend on which one decoded it.

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    Conversation, GenRequest, Message, Role, StopReason, ThinkingLevel, ToolDescriptor,
};
use futures::StreamExt;
use serde_json::Value;

use crate::http::HttpResponse;
use crate::tool_names::ToolNames;
use crate::wire::{
    WireFormat, anthropic_messages::AnthropicMessages, gemini_interactions::GeminiInteractions,
    github_copilot_completions::GithubCopilotCompletions, nous_portal::NousPortal, ollama::Ollama,
    openai_codex_responses::OpenAiCodexResponses, openai_completions::OpenAiCompletions,
    openai_responses::OpenAiResponses, xai_responses::XaiResponses,
};

struct Adapter {
    name: &'static str,
    wire: &'static dyn WireFormat,
    fixture: &'static str,
    /// Whether this wire reports an input total that cache parts can exceed.
    input_total: bool,
}

fn adapters() -> [Adapter; 9] {
    fn adapter(
        name: &'static str,
        wire: &'static dyn WireFormat,
        fixture: &'static str,
    ) -> Adapter {
        Adapter {
            name,
            wire,
            fixture,
            input_total: true,
        }
    }
    [
        adapter(
            "completions",
            &OpenAiCompletions,
            include_str!("../../tests/goldens/chat_completions/stream/tool_calls.sse"),
        ),
        adapter(
            "ollama",
            &Ollama,
            include_str!("../../tests/goldens/ollama_tool_call.sse"),
        ),
        adapter(
            "nous",
            &NousPortal,
            include_str!("../../tests/goldens/profile/nous/stream/tool_calls.sse"),
        ),
        adapter(
            "copilot",
            &GithubCopilotCompletions,
            include_str!("../../tests/goldens/profile/github-copilot/stream/tool_calls.sse"),
        ),
        adapter(
            "responses",
            &OpenAiResponses,
            include_str!("../../tests/goldens/profile/openai/stream/tool_calls.sse"),
        ),
        adapter(
            "subscription",
            &OpenAiCodexResponses,
            include_str!("../../tests/goldens/profile/openai-codex/stream/tool_calls.sse"),
        ),
        adapter(
            "xai",
            &XaiResponses,
            include_str!("../../tests/goldens/xai_responses_tool.sse"),
        ),
        Adapter {
            name: "anthropic",
            wire: &AnthropicMessages,
            fixture: include_str!("../../tests/goldens/profile/anthropic/stream/tool_calls.sse"),
            // Anthropic reports disjoint input figures: nothing to exceed.
            input_total: false,
        },
        adapter(
            "gemini",
            &GeminiInteractions,
            include_str!("../../tests/goldens/profile/gemini/stream/tool_calls.sse"),
        ),
    ]
}

fn request() -> GenRequest {
    let tool = |name: &str| ToolDescriptor {
        name: name.into(),
        description: String::new(),
        parameters: serde_json::json!({"type":"object"}),
    };
    GenRequest {
        web_search: None,
        cache: None,
        model: "fixture".into(),
        messages: Conversation::new(vec![Message::text(Role::User, "do the work")]).unwrap(),
        tools: vec![tool("read_file"), tool("glob"), tool("read")],
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(1024),
        parallel_tool_calls: true,
    }
}

/// Rewrite every JSON `data:` frame of `fixture`, then append `tail`.
fn transcript(fixture: &str, rewrite: impl Fn(&mut Value), tail: &str) -> String {
    let mut lines: Vec<String> = fixture
        .lines()
        .map(|line| {
            let Some(data) = line.strip_prefix("data: ") else {
                return line.to_owned();
            };
            let Ok(mut data) = serde_json::from_str::<Value>(data) else {
                return line.to_owned();
            };
            walk(&mut data, &rewrite);
            format!("data: {data}")
        })
        .collect();
    lines.push(String::new());
    lines.push(tail.to_owned());
    lines.join("\n") + "\n\n"
}

fn walk(value: &mut Value, rewrite: &impl Fn(&mut Value)) {
    rewrite(value);
    match value {
        Value::Object(fields) => fields.values_mut().for_each(|value| walk(value, rewrite)),
        Value::Array(values) => values.iter_mut().for_each(|value| walk(value, rewrite)),
        _ => {}
    }
}

async fn run(adapter: &Adapter, transcript: String) -> Vec<AiEvent> {
    let response = HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
            transcript,
        ))])),
    };
    let request = request();
    let names = ToolNames::new(&request);
    adapter
        .wire
        .decode(Ok(response), None)
        .flat_map(|event| futures::stream::iter(names.restore(event)))
        .collect()
        .await
}

/// The terminal that `AttemptStream` believes: the first `Stop`.
fn first_stop(events: &[AiEvent]) -> Option<StopReason> {
    events.iter().find_map(|event| match event {
        AiEvent::Stop(stop) => Some(*stop),
        _ => None,
    })
}

fn errors(events: &[AiEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

// Catches: one adapter drops a call with no id and lets the response finish
// as a successful tool use, while another ends the generation.
#[tokio::test]
async fn a_call_with_no_id_ends_the_generation_through_every_adapter() {
    let mut messages = Vec::new();
    for adapter in adapters() {
        let events = run(
            &adapter,
            transcript(
                adapter.fixture,
                |value| {
                    let Value::Object(fields) = value else { return };
                    let call = matches!(
                        fields.get("type").and_then(Value::as_str),
                        Some("function" | "function_call" | "tool_use")
                    );
                    if call {
                        let key = if fields.contains_key("call_id") {
                            "call_id"
                        } else {
                            "id"
                        };
                        fields.insert(key.into(), "".into());
                    }
                },
                "",
            ),
        )
        .await;
        let name = adapter.name;
        assert_eq!(
            first_stop(&events),
            Some(StopReason::Error),
            "{name}: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::ToolCall { .. })),
            "{name}: an unattributable call was dispatched: {events:?}"
        );
        let found = errors(&events);
        assert!(!found.is_empty(), "{name}: no error: {events:?}");
        messages.push((name, found[0].clone()));
    }
    let first = &messages[0].1;
    assert!(
        messages.iter().all(|(_, message)| message == first),
        "adapters explain a missing id differently: {messages:#?}"
    );
}

// Catches: one adapter clamps cache parts larger than the input total into a
// usage figure that reads as true, or explains the discard in its own words.
#[tokio::test]
async fn incoherent_usage_is_discarded_with_one_notice_through_every_adapter() {
    let mut messages = Vec::new();
    for adapter in adapters().into_iter().filter(|adapter| adapter.input_total) {
        let events = run(
            &adapter,
            transcript(
                adapter.fixture,
                |value| {
                    let Value::Object(fields) = value else { return };
                    let total = |key: &str| fields.get(key).and_then(Value::as_u64);
                    let prompt = total("prompt_tokens");
                    let interaction = total("total_input_tokens");
                    let input = total("input_tokens").zip(total("output_tokens"));
                    if let Some(prompt) = prompt {
                        fields.insert(
                            "prompt_tokens_details".into(),
                            serde_json::json!({"cached_tokens": prompt + 1}),
                        );
                    } else if let Some(input) = interaction {
                        fields.insert("total_cached_tokens".into(), (input + 1).into());
                    } else if let Some((input, _)) = input {
                        fields.insert(
                            "input_tokens_details".into(),
                            serde_json::json!({"cached_tokens": input + 1}),
                        );
                    }
                },
                "",
            ),
        )
        .await;
        let name = adapter.name;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::Usage(_))),
            "{name}: an incoherent usage figure was reported: {events:?}"
        );
        assert_eq!(
            first_stop(&events),
            Some(StopReason::ToolUse),
            "{name}: {events:?}"
        );
        let found = errors(&events);
        assert_eq!(found.len(), 1, "{name}: expected one notice: {events:?}");
        messages.push((name, found[0].clone()));
    }
    let first = &messages[0].1;
    assert!(
        messages.iter().all(|(_, message)| message == first),
        "adapters explain incoherent usage differently: {messages:#?}"
    );
}
