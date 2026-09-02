//! RED contract for the shared hook invocation wire at the tool adapters.

use std::time::Duration;

use ego_core::event::hook::{
    HookActivation, HookContentDigest, HookId, HookInvocationId, HookLifecycle,
    HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{EventId, SessionId, TurnId};
use ego_core::hook::{
    EffectScope, HookCausality, HookEvent, HookHandlerConfig, HookHandlerRequestV1,
    HookInvocationRequest, HookStartedEvidence,
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
