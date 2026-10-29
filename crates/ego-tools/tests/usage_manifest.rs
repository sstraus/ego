//! Manifest model projection follows main evidence.

use std::time::Duration;

use ego_core::config::ModelRef;
use ego_core::event::domain::GenerationRole;
use ego_core::event::id::{GenerationId, SessionId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{
    ConfigProvenance, EvidenceAdmissionService, GenerationAdmissionEvidence, GenerationTarget,
    PricingEvidence, RunInvocationId,
};
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::run::RunId;
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, LeaseInputs, OsIo};
use tempfile::tempdir;

const ALIVE: &dyn Fn(u32) -> bool = &|_| true;

fn lease() -> LeaseInputs<'static> {
    LeaseInputs {
        pid: 67_067,
        now: Timestamp::from_millis(67_067_000),
        stale_after: Duration::from_secs(30),
        is_alive: ALIVE,
    }
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(8_192),
        max_output_tokens: Some(1_024),
        max_tools: Some(32),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::None,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

fn admission(
    session: SessionId,
    invocation: RunInvocationId,
    role: GenerationRole,
    model: &str,
) -> GenerationAdmissionEvidence {
    GenerationAdmissionEvidence::new(
        session,
        invocation,
        AttemptRef::first(GenerationId::new()),
        role,
        GenerationTarget::new("provider", ModelRef(model.to_owned()), model),
        capabilities(),
        PricingEvidence::Unpriced {
            source: "usage-JZZ-fixture".to_owned(),
            reason: "subscription".to_owned(),
        },
    )
}

#[test]
fn main_generation_evidence_sets_model_after_judge_compaction_and_ambient_records() {
    let root = tempdir().expect("session root");
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let opened = store.open_writer(&session, lease()).expect("lease");
    let service = EvidenceAdmissionService::new();
    service
        .admit_run_context(
            &opened.writer,
            ego_core::evidence::RunContextEvidence::new(
                session,
                invocation,
                RunId::new(session, invocation.uuid()),
                ConfigProvenance::default(),
            ),
        )
        .expect("run context");
    for (role, model) in [
        (GenerationRole::Judge, "judge/model"),
        (GenerationRole::Compaction, "compaction/model"),
        (GenerationRole::Main, "main/model"),
    ] {
        service
            .admit_generation(&opened.writer, admission(session, invocation, role, model))
            .expect("generation evidence");
    }
    drop(opened);

    let listed = store.list().expect("manifest listing");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].model.as_deref(), Some("main/model"));
}

#[test]
fn manifest_model_stays_absent_without_main_generation_evidence() {
    let root = tempdir().expect("session root");
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let opened = store.open_writer(&session, lease()).expect("lease");
    let service = EvidenceAdmissionService::new();
    service
        .admit_run_context(
            &opened.writer,
            ego_core::evidence::RunContextEvidence::new(
                session,
                invocation,
                RunId::new(session, invocation.uuid()),
                ConfigProvenance::default(),
            ),
        )
        .expect("run context");
    for (role, model) in [
        (GenerationRole::Judge, "judge/model"),
        (GenerationRole::Compaction, "compaction/model"),
    ] {
        service
            .admit_generation(&opened.writer, admission(session, invocation, role, model))
            .expect("generation evidence");
    }
    drop(opened);

    let listed = store.list().expect("manifest listing");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].model, None);
}
