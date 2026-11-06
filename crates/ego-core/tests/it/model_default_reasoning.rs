//! contract for model-default reasoning resolution (model defaults).

use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::types::ThinkingLevel;

fn capabilities(
    default_reasoning_effort: Option<ThinkingLevel>,
    reasoning_efforts: ReasoningEffortSupport,
) -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(8_192),
        max_output_tokens: Some(1_024),
        max_tools: Some(32),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::None,
        transport: Transport::Sse,
        image_input: Default::default(),
        default_reasoning_effort,
    }
}

#[test]
fn absent_uses_valid_model_default_but_explicit_selection_is_preserved() {
    let caps = capabilities(
        Some(ThinkingLevel::Medium),
        ReasoningEffortSupport::Only([ThinkingLevel::Medium, ThinkingLevel::High].into()),
    );
    assert_eq!(
        caps.resolve_reasoning_effort(None).unwrap(),
        ThinkingLevel::Medium
    );
    assert_eq!(
        caps.resolve_reasoning_effort(Some(ThinkingLevel::High))
            .unwrap(),
        ThinkingLevel::High
    );
}

#[test]
fn invalid_explicit_selection_refuses_without_substitution() {
    let caps = capabilities(
        Some(ThinkingLevel::Medium),
        ReasoningEffortSupport::Only([ThinkingLevel::Medium].into()),
    );
    assert!(
        caps.resolve_reasoning_effort(Some(ThinkingLevel::Off))
            .is_err()
    );
}

#[test]
fn unknown_absent_default_is_actionable_and_invalid_default_refuses() {
    let unknown = capabilities(
        None,
        ReasoningEffortSupport::Only([ThinkingLevel::Medium].into()),
    );
    let unknown_error = unknown.resolve_reasoning_effort(None).unwrap_err();
    assert!(unknown_error.to_string().contains("--thinking"));

    let invalid = capabilities(
        Some(ThinkingLevel::Off),
        ReasoningEffortSupport::Only([ThinkingLevel::Medium].into()),
    );
    assert!(invalid.resolve_reasoning_effort(None).is_err());
}

#[test]
fn stale_capabilities_are_refused_by_resolution_but_current_capabilities_work() {
    let caps = capabilities(
        Some(ThinkingLevel::Medium),
        ReasoningEffortSupport::Only([ThinkingLevel::Medium].into()),
    );
    assert_eq!(
        caps.resolve_reasoning_effort(None).unwrap(),
        ThinkingLevel::Medium
    );
    let mut wire = serde_json::to_value(&caps).unwrap();
    wire["schema_version"] = serde_json::json!(4);
    let stale = serde_json::from_value::<ModelCapabilities>(wire).unwrap();
    assert!(stale.resolve_reasoning_effort(None).is_err());
}
