//! Generic command hook context wire regressions.
use ego_core::hook::HookHandlerResultV1;

#[test]
fn claude_context_output_is_not_rejected_as_an_unknown_result() {
    let result = HookHandlerResultV1::parse(br#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"real recall"}}"#);
    assert!(
        result.is_ok(),
        "Claude Code context output must be accepted: {result:?}"
    );
}

#[test]
fn unknown_result_fields_do_not_silently_change_hook_policy() {
    assert!(
        HookHandlerResultV1::parse(br#"{"version":1,"decision":"continue","extra":true}"#).is_err()
    );
}

#[test]
fn context_cap_preserves_utf8_and_marks_truncation() {
    let raw = serde_json::to_vec(&serde_json::json!({"hookSpecificOutput": {
        "hookEventName": "UserPromptSubmit", "additionalContext": "é".repeat(12_000)
    }}))
    .unwrap();
    let result = HookHandlerResultV1::parse(&raw).unwrap();
    let context = &result.context().unwrap().additional_context;
    assert!(context.len() <= 16 * 1024);
    assert!(context.ends_with("[hook context truncated]"));
}

#[test]
fn unsupported_or_mismatched_context_cannot_cross_lifecycle_boundaries() {
    let result = HookHandlerResultV1::parse(
        br#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"context"}}"#,
    )
    .unwrap();
    assert!(
        result
            .context()
            .unwrap()
            .text_for(ego_core::event::hook::HookLifecycle::UserPromptSubmit)
            .is_err()
    );
    assert!(HookHandlerResultV1::parse(br#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"context"}}"#).is_err());
    assert!(
        HookHandlerResultV1::parse(
            br#"{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"context"}}"#
        )
        .is_err()
    );
    assert!(HookHandlerResultV1::parse(br#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"context","permissionDecision":"allow"}}"#).is_err());
    assert!(
        HookHandlerResultV1::parse(br#"{}"#)
            .unwrap()
            .decision()
            .is_continue()
    );
}
