//! RED contract for prompt hooks through the ordinary tool-less generation owner.

use futures::executor::block_on;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_cli::hook::{HookPromptAdapter, ToollessGenerationService};
use ego_core::event::domain::GenerationRole;
use ego_core::event::hook::{
    HookActivation, HookDecision, HookInvocationId, HookLifecycle, HookSubject,
};
use ego_core::hook::{EffectScope, HookHandlerConfig, HookInvocationRequest};

#[derive(Clone, Default)]
struct GenerationCapture {
    requests: Arc<Mutex<Vec<ego_cli::hook::ToollessGenerationRequest>>>,
    raw: Arc<Mutex<Vec<u8>>>,
}

impl GenerationCapture {
    fn with_raw(raw: impl Into<Vec<u8>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            raw: Arc::new(Mutex::new(raw.into())),
        }
    }
}

impl ToollessGenerationService for GenerationCapture {
    fn generate(
        &self,
        request: ego_cli::hook::ToollessGenerationRequest,
    ) -> ego_cli::hook::GenerationFuture<'_> {
        self.requests.lock().unwrap().push(request);
        let raw = self.raw.lock().unwrap().clone();
        let evidence_bytes = raw.len() as u64;
        Box::pin(std::future::ready(Ok(ego_cli::hook::GeneratedHookResult {
            raw,
            evidence: ego_core::store::evidence::EvidenceRef::new(
                ego_core::store::evidence::EvidenceId::parse(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .expect("generation evidence id"),
                evidence_bytes,
            ),
            delivery: ego_core::generator::attempt::DeliveryCertainty::DefinitelySent,
        })))
    }
}

fn request() -> HookInvocationRequest {
    HookInvocationRequest::new(
        HookHandlerConfig::Prompt {
            prompt: "Return one bounded diagnostic.".to_owned(),
            model: Some("ordinary-model".to_owned()),
            timeout_seconds: Some(30),
        },
        HookLifecycle::UserPromptSubmit,
        None,
        HookSubject::Session,
        HookActivation::Persistent,
        HookInvocationId::new(),
        EffectScope::new(Duration::from_secs(30)),
    )
}

#[test]
fn runtime_invocation_is_not_wire_and_handler_request_has_its_own_strict_codec() {
    let runtime = request();
    let invocation = runtime.invocation();
    assert_eq!(runtime.invocation(), invocation);
    assert_eq!(runtime.effect_scope().timeout(), Duration::from_secs(30));

    let wire = ego_core::hook::HookHandlerRequestV1::from_runtime(&runtime)
        .expect("strict handler request projection");
    let bytes = serde_json::to_vec(&wire).expect("handler request wire");
    let decoded: ego_core::hook::HookHandlerRequestV1 =
        serde_json::from_slice(&bytes).expect("handler request round trip");
    assert_eq!(decoded.invocation_id(), invocation);
    assert_eq!(decoded.activation(), runtime.activation());
}

#[test]
fn prompt_is_toolless_hook_generation_with_shared_attempt_accounting_and_effect_scope() {
    let capture = GenerationCapture::with_raw(br#"{"version":1,"decision":"continue"}"#);
    let result = block_on(HookPromptAdapter::new(capture.clone()).invoke(request()));
    let settled = result.expect("prompt hook settlement");
    let generated = &capture.requests.lock().unwrap()[0];
    assert_eq!(generated.role(), GenerationRole::Hook);
    assert_eq!(generated.model_slug(), Some("ordinary-model"));
    assert!(generated.tools().is_empty());
    assert_eq!(generated.effect_scope().timeout(), Duration::from_secs(30));
    assert_eq!(settled.raw(), br#"{"version":1,"decision":"continue"}"#);
}

#[test]
fn prompt_requires_strict_raw_result_and_normalizes_valid_decisions() {
    for (raw, expected) in [
        (
            json!({"version": 1, "decision": "block"}),
            Some(HookDecision::Block),
        ),
        (
            json!({"version": 1, "decision": "request_escalation"}),
            Some(HookDecision::RequestEscalation),
        ),
        (json!({"decision": "continue"}), None),
        (json!({"version": 1, "decision": "unknown"}), None),
    ] {
        let capture = GenerationCapture::with_raw(serde_json::to_vec(&raw).unwrap());
        let result = block_on(HookPromptAdapter::new(capture).invoke(request()));
        let settled = result.expect("typed prompt settlement");
        if let Some(expected) = expected {
            assert_eq!(settled.decision(), expected);
            assert!(settled.failure().is_none());
        } else {
            assert!(settled.failure().is_some());
        }
    }
}
