//! A real builtin descriptor must keep its discriminator at the routed wire boundary.

use std::sync::Arc;

use ego_ai::wire::openai_completions::openrouter_request_body;
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
use ego_core::skill::SkillCatalogue;
use ego_core::turn::RegisteredTool;
use serde_json::{Value, json};

/// Catches a wire projection that drops the skill union's required discriminator.
/// This does not simulate how a gateway or a live model interprets that schema.
#[test]
fn routed_skill_schema_keeps_required_kind_in_both_union_branches() {
    let descriptor =
        RegisteredTool::skill(Arc::new(SkillCatalogue::new(Vec::new()).unwrap())).descriptor();
    for model in ["anthropic/claude-haiku-4.5", "openai/gpt-6-luna"] {
        let request = GenRequest {
            model: model.into(),
            messages: Conversation::new(vec![Message::text(Role::User, "Invoke a skill.")])
                .unwrap(),
            tools: vec![descriptor.clone()],
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(1024),
            parallel_tool_calls: false,
            cache: None,
            web_search: None,
        };
        let bytes = openrouter_request_body(&request).unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let function = &body["tools"][0]["function"];
        assert_eq!(function["name"], "skill");
        let branches = function["parameters"]["oneOf"].as_array().unwrap();
        assert_eq!(branches.len(), 2);
        for (index, (kind, required)) in [
            ("invoke", json!(["kind", "name", "arguments"])),
            ("read_resource", json!(["kind", "invocation", "path"])),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(branches[index]["type"], "object");
            assert_eq!(branches[index]["required"], required);
            assert_eq!(branches[index]["properties"]["kind"]["const"], kind);
            assert_eq!(branches[index]["additionalProperties"], false);
        }
    }
}
