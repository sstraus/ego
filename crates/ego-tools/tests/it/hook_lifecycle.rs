//! contract for the shared hook invocation wire at the tool adapters.

use std::time::Duration;

use ego_core::event::hook::{
    HookActivation, HookContentDigest, HookId, HookInvocationId, HookLifecycle,
    HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{EventId, SessionId, TurnId};
use ego_core::hook::{
    EffectScope, HookCausality, HookEvent, HookHandlerConfig, HookHandlerRequestV1,
    HookInputContext, HookInvocationRequest, HookStartedEvidence,
};

#[test]
fn hook_adapter_wire_retains_the_complete_invocation_identity() {
    let hook = HookId::derive(
        HookSourceAuthority::Project,
        &HookSourceLocation::parse("hooks/tool.toml").expect("hook source"),
        &HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("hook digest"),
        0,
    );
    let invocation = HookInvocationId::new();
    let request = HookInvocationRequest::new(
        HookStartedEvidence::new(
            EventId::new(),
            EventId::new(),
            hook,
            invocation,
            HookActivation::Persistent,
        ),
        HookCausality::session(SessionId::new(), TurnId::new()),
        HookHandlerConfig::Command {
            command: "/usr/bin/check".to_owned(),
            args: None,
            shell: None,
            timeout_seconds: Some(30),
        },
        HookEvent::session(HookLifecycle::PreToolUse),
        EffectScope::new(Duration::from_secs(30)),
    );
    let wire = serde_json::to_value(
        HookHandlerRequestV1::from_runtime(&request).expect("strict hook request"),
    )
    .expect("wire request");
    assert!(
        wire.get("hook_id").is_some(),
        "command and HTTP adapters must receive the admitted hook identity"
    );
}

// Catches: a tool output above 64 KiB makes the PostToolUse hook fail with
// "hook request exceeds 64 KiB" instead of running on a shortened payload.
#[test]
fn an_oversized_tool_output_is_shortened_so_the_hook_still_runs() {
    let hook = HookId::derive(
        HookSourceAuthority::Project,
        &HookSourceLocation::parse("hooks/tool.toml").expect("hook source"),
        &HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("hook digest"),
        0,
    );
    let output = "é".repeat(100 * 1024);
    let request = HookInvocationRequest::new(
        HookStartedEvidence::new(
            EventId::new(),
            EventId::new(),
            hook,
            HookInvocationId::new(),
            HookActivation::Persistent,
        ),
        HookCausality::session(SessionId::new(), TurnId::new()),
        HookHandlerConfig::Command {
            command: "/usr/bin/check".to_owned(),
            args: None,
            shell: None,
            timeout_seconds: Some(30),
        },
        HookEvent::session(HookLifecycle::PostToolUse).with_context(HookInputContext {
            tool_name: Some("exec".to_owned()),
            tool_input: Some(serde_json::json!({"command": "x".repeat(40 * 1024)})),
            tool_response: Some(serde_json::Value::String(output.clone())),
            ..HookInputContext::default()
        }),
        EffectScope::new(Duration::from_secs(30)),
    );
    let wire = serde_json::to_vec(
        &HookHandlerRequestV1::from_runtime(&request)
            .expect("an oversized output still runs the hook"),
    )
    .expect("wire request");
    assert!(wire.len() <= 64 * 1024, "{} bytes", wire.len());
    let wire: serde_json::Value = serde_json::from_slice(&wire).unwrap();
    let response = wire["tool_response"]
        .as_str()
        .expect("response kept as text");
    assert!(output.starts_with(response.split('\n').next().unwrap()));
    assert!(response.contains("bytes removed to fit the 64 KiB hook request"));
}
