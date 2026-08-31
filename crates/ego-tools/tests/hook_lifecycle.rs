//! RED contract for the shared hook invocation wire at the tool adapters.

use std::time::Duration;

use ego_core::event::hook::{HookActivation, HookInvocationId, HookLifecycle, HookSubject};
use ego_core::hook::{EffectScope, HookHandlerConfig, HookHandlerRequestV1, HookInvocationRequest};

#[test]
fn hook_adapter_wire_retains_the_complete_invocation_identity() {
    let request = HookInvocationRequest::new(
        HookHandlerConfig::Command {
            command: "/usr/bin/check".to_owned(),
            args: None,
            shell: None,
            timeout_seconds: Some(30),
        },
        HookLifecycle::PreToolUse,
        None,
        HookSubject::Session,
        HookActivation::Persistent,
        HookInvocationId::new(),
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
