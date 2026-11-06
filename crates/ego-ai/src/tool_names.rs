//! Request-local provider aliases; catalogue and journal names stay unchanged.

use std::collections::{BTreeMap, BTreeSet};

use ego_core::digest::sha256_hex;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ContentBlock, Conversation, ErrorKind, GenRequest, StopReason};

const PREFIX: &str = "__ego_";
const MAX_BYTES: usize = 64;

pub(crate) struct ToolNames {
    encoded: BTreeMap<String, String>,
    declared: BTreeMap<String, String>,
}

impl ToolNames {
    pub(crate) fn new(request: &GenRequest) -> Self {
        let names: BTreeSet<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .chain(
                request
                    .messages
                    .messages()
                    .iter()
                    .flat_map(|message| &message.content)
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                        _ => None,
                    }),
            )
            .collect();
        // Reserve valid originals first. Reserved-prefix originals must also be
        // encoded, so none can impersonate the alias of a different tool.
        let mut encoded = BTreeMap::new();
        let mut originals = BTreeMap::new();
        for name in names.iter().copied().filter(|name| portable(name)) {
            encoded.insert(name.to_owned(), name.to_owned());
            originals.insert(name.to_owned(), name.to_owned());
        }
        for name in names.into_iter().filter(|name| !portable(name)) {
            let hash = sha256_hex(name.as_bytes());
            let mut number = 0_u64;
            let alias = loop {
                let suffix = if number == 0 {
                    String::new()
                } else {
                    format!("_{number}")
                };
                let alias = format!(
                    "{PREFIX}{}{suffix}",
                    &hash[..MAX_BYTES - PREFIX.len() - suffix.len()]
                );
                if !originals.contains_key(&alias) {
                    break alias;
                }
                number = number
                    .checked_add(1)
                    .expect("a request cannot contain 2^64 distinct tools");
            };
            encoded.insert(name.to_owned(), alias.clone());
            originals.insert(alias, name.to_owned());
        }
        let declared = request
            .tools
            .iter()
            .map(|tool| (encoded[&tool.name].clone(), tool.name.clone()))
            .collect();
        Self { encoded, declared }
    }

    pub(crate) fn encode(&self, request: &GenRequest) -> GenRequest {
        let mut encoded = request.clone();
        for tool in &mut encoded.tools {
            tool.name = self.encoded[&tool.name].clone();
        }
        let mut messages: Vec<_> = encoded.messages.into();
        for block in messages.iter_mut().flat_map(|message| &mut message.content) {
            if let ContentBlock::ToolUse { name, .. } = block {
                *name = self.encoded[name.as_str()].clone();
            }
        }
        encoded.messages = Conversation::new(messages)
            .expect("renaming tools preserves conversation call/result invariants");
        encoded
    }

    pub(crate) fn restore(&self, mut event: AiEvent) -> Vec<AiEvent> {
        if let AiEvent::ToolCall {
            call_id,
            name,
            args,
        } = &mut event
        {
            // A result requires a real correlation id. Duplicates and conflicting
            // identities remain protocol failures; core also checks session reuse.
            if call_id.is_empty() {
                return vec![AiEvent::Error {
                    message: "the provider returned a tool call with no id; its result cannot be attributed".into(),
                    kind: ErrorKind::Unclassified,
                }, AiEvent::Stop(StopReason::Error)];
            }
            let Some(original) = self.declared.get(name.as_str()) else {
                let problem = if name.is_empty() {
                    "provider returned a tool call with no function name".to_owned()
                } else {
                    format!("provider returned undeclared tool name '{name}'")
                };
                let declared = self.declared.keys().cloned().collect::<Vec<_>>().join(", ");
                return vec![AiEvent::ToolCallRejected {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    message: format!(
                        "{problem}. Declared tools: {declared}. Choose a declared tool and correct the call."
                    ),
                }];
            };
            name.clone_from(original);
        }
        vec![event]
    }
}

fn portable(name: &str) -> bool {
    !name.starts_with(PREFIX)
        && !name.is_empty()
        && name.len() <= MAX_BYTES
        && name
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
    use crate::wire::{
        WireFormat, anthropic_messages::AnthropicMessages, gemini_interactions::GeminiInteractions,
        github_copilot_completions::GithubCopilotCompletions, nous_portal::NousPortal,
        ollama::Ollama, openai_codex_responses::OpenAiCodexResponses,
        openai_completions::OpenAiCompletions, openai_responses::OpenAiResponses,
        xai_responses::XaiResponses,
    };
    use ego_core::generator::types::{Message, Role, ThinkingLevel, ToolDescriptor};
    use futures::StreamExt;

    // Catches: adapters decide to stop on a correctable name before shared feedback.
    #[tokio::test]
    async fn malformed_names_share_feedback_across_every_adapter() {
        let request = GenRequest {
            web_search: None,
            cache: None,
            model: "fixture".into(),
            messages: Conversation::new(vec![Message::text(Role::User, "do the work")]).unwrap(),
            tools: vec![ToolDescriptor {
                name: "declared_tool".into(),
                description: String::new(),
                parameters: serde_json::json!({"type":"object"}),
            }],
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(1024),
            parallel_tool_calls: false,
        };
        let adapters: [(&str, &dyn WireFormat, &str); 9] = [
            (
                "completions",
                &OpenAiCompletions,
                include_str!("../tests/goldens/chat_completions/stream/tool_calls.sse"),
            ),
            (
                "responses",
                &OpenAiResponses,
                include_str!("../tests/goldens/profile/openai/stream/tool_calls.sse"),
            ),
            (
                "subscription",
                &OpenAiCodexResponses,
                include_str!("../tests/goldens/profile/openai-codex/stream/tool_calls.sse"),
            ),
            (
                "xai",
                &XaiResponses,
                include_str!("../tests/goldens/xai_responses_tool.sse"),
            ),
            (
                "anthropic",
                &AnthropicMessages,
                include_str!("../tests/goldens/profile/anthropic/stream/tool_calls.sse"),
            ),
            (
                "gemini",
                &GeminiInteractions,
                include_str!("../tests/goldens/profile/gemini/stream/tool_calls.sse"),
            ),
            (
                "ollama",
                &Ollama,
                include_str!("../tests/goldens/ollama_tool_call.sse"),
            ),
            (
                "nous",
                &NousPortal,
                include_str!("../tests/goldens/profile/nous/stream/tool_calls.sse"),
            ),
            (
                "copilot",
                &GithubCopilotCompletions,
                include_str!("../tests/goldens/profile/github-copilot/stream/tool_calls.sse"),
            ),
        ];
        fn rewrite(value: &mut serde_json::Value, name: Option<&str>) {
            match value {
                serde_json::Value::Object(fields) => {
                    if fields.contains_key("name") {
                        match name {
                            Some(name) => {
                                fields.insert("name".into(), name.into());
                            }
                            None => {
                                fields.remove("name");
                            }
                        }
                    }
                    for value in fields.values_mut() {
                        rewrite(value, name);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        rewrite(value, name);
                    }
                }
                _ => {}
            }
        }
        for (adapter, wire, fixture) in adapters {
            for name in [None, Some(""), Some("ls")] {
                let transcript = fixture
                    .lines()
                    .map(|line| {
                        let Some(data) = line.strip_prefix("data: ") else {
                            return line.to_owned();
                        };
                        let Ok(mut data) = serde_json::from_str::<serde_json::Value>(data) else {
                            return line.to_owned();
                        };
                        rewrite(&mut data, name);
                        format!("data: {data}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n\n";
                let response = HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(
                        transcript,
                    ))])),
                };
                let names = ToolNames::new(&request);
                let events: Vec<_> = wire
                    .decode(Ok(response), None)
                    .flat_map(|event| futures::stream::iter(names.restore(event)))
                    .collect()
                    .await;
                assert!(
                    !events.iter().any(|event| matches!(
                        event,
                        AiEvent::Error { .. } | AiEvent::Stop(StopReason::Error)
                    )),
                    "{adapter} name={name:?}: {events:?}"
                );
                assert!(
                    events.iter().any(|event| matches!(event,
                    AiEvent::ToolCallRejected { call_id, message, .. }
                    if !call_id.is_empty() && message.contains("Declared tools: declared_tool"))),
                    "{adapter} name={name:?}: no actionable rejected call: {events:?}"
                );
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, AiEvent::ToolCall { .. })),
                    "a rejected name must never become dispatchable: {adapter} {name:?}"
                );
            }
        }
    }
}
