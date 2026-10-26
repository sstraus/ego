//! A real builtin descriptor must keep its discriminator at the routed wire boundary.

use std::sync::Arc;

use ego_ai::wire::openai_completions::openrouter_request_body;
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
use ego_core::skill::SkillCatalogue;
use ego_core::turn::RegisteredTool;
use serde_json::{Value, json};

/// Catches a wire projection that drops the skill discriminator or restores a root union.
/// This does not simulate how a gateway or a live model interprets that schema.
#[test]
fn routed_skill_schema_keeps_required_kind_and_flat_object() {
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
        let schema = &function["parameters"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["kind"]));
        assert_eq!(schema["properties"]["kind"]["type"], "string");
        assert_eq!(
            schema["properties"]["kind"]["enum"],
            json!(["invoke", "read_resource"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema.get("oneOf").is_none());
    }
}
