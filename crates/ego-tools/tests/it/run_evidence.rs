//! contracts for lease-held authoritative evidence.

use ego_core::config::{ActiveProfileProvenance, ModelRef, OriginSet, Source};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, GenerationRole};
use ego_core::event::id::{EventId, GenerationId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{
    ConfigProvenance, EVIDENCE_SCHEMA_VERSION, EvidenceAdmissionService, EvidenceError,
    EvidenceKey, EvidenceRecord, EvidenceStore, GenerationAdmissionEvidence, GenerationTarget,
    ManifestEvidence, PricingEvidence, RunContextEvidence, RunInvocationId,
};
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::run::RunId;
use ego_core::store::codec::{
    CODEC_VERSION, Header, StoreLine, decode_line, encode_header, encode_record,
};
use ego_core::store::session_store::{SessionEntry, SessionStore, StoreError};
use ego_tools::store::{JsonlStore, LeaseInputs, OsIo};
use futures::executor::block_on;
use futures::stream::StreamExt;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;

const ALIVE: &dyn Fn(u32) -> bool = &|_| true;

fn seq(value: u64) -> Seq {
    serde_json::from_value(json!(value)).unwrap()
}

fn lease() -> LeaseInputs<'static> {
    LeaseInputs {
        pid: 66066,
        now: Timestamp::from_millis(6_606_600),
        stale_after: Duration::from_secs(30),
        is_alive: ALIVE,
    }
}

fn generation_started(attempt: AttemptRef) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: seq(1),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(6_606_601),
            producer: Producer::Main,
        },
        kind: EventKind::GenerationStarted {
            attempt,
            role: GenerationRole::Main,
            cache: None,
            snapshot: None,
        },
    }
}

fn model() -> ModelRef {
    ModelRef("provider/model".to_owned())
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(8192),
        max_output_tokens: Some(1024),
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

fn context(session: SessionId, id: RunInvocationId) -> RunContextEvidence {
    RunContextEvidence::new(
        session,
        id,
        RunId::new(session, id.uuid()),
        ConfigProvenance::default(),
    )
}

fn generation(
    session: SessionId,
    id: RunInvocationId,
    attempt: AttemptRef,
) -> GenerationAdmissionEvidence {
    generation_with(
        session,
        id,
        attempt,
        GenerationTarget::new("provider", model(), "wire-model"),
        capabilities(),
        PricingEvidence::Unpriced {
            source: "fixture".to_owned(),
            reason: "subscription".to_owned(),
        },
    )
}

fn generation_with(
    session: SessionId,
    id: RunInvocationId,
    attempt: AttemptRef,
    target: GenerationTarget,
    capabilities: ModelCapabilities,
    pricing: PricingEvidence,
) -> GenerationAdmissionEvidence {
    GenerationAdmissionEvidence::new(
        session,
        id,
        attempt,
        GenerationRole::Main,
        target,
        capabilities,
        pricing,
    )
}

fn entries(store: &JsonlStore, session: &SessionId) -> Vec<SessionEntry> {
    block_on(store.read(session).unwrap().collect::<Vec<_>>())
        .into_iter()
        .map(|entry| entry.unwrap())
        .collect()
}

fn log_path(root: &Path, session: &SessionId) -> PathBuf {
    root.join(serde_json::to_string(session).unwrap().trim_matches('"'))
        .join("events.jsonl")
}

fn manifest_path(root: &Path, session: &SessionId) -> PathBuf {
    root.join(serde_json::to_string(session).unwrap().trim_matches('"'))
        .join("manifest.json")
}

fn write_evidence_log(root: &Path, session: SessionId, entries: &[serde_json::Value]) -> PathBuf {
    let log = log_path(root, &session);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut image = encode_header(&Header {
        profiles: Default::default(),
        v: CODEC_VERSION,
        session,
        created: Timestamp::from_millis(1),
        workspace_root: "/workspace".into(),
    });
    image.push(b'\n');
    for (offset, entry) in entries.iter().enumerate() {
        image.extend_from_slice(&encode_record(seq((offset + 1) as u64), entry).unwrap());
        image.push(b'\n');
    }
    std::fs::write(&log, image).unwrap();
    log
}

fn legacy_run_transaction(session: &SessionId) -> serde_json::Value {
    serde_json::json!({
        "revision": 1,
        "run_id": {
            "session_id": serde_json::to_value(session).unwrap(),
            "nonce": "00000000-0000-7000-8000-000000000054"
        },
        "origin": {"kind": "top_level"},
        "root": "/workspace",
        "selections": {
            "model": "model/test",
            "thinking": null,
            "mode": null,
            "budget": null,
            "profile": null
        },
        "command": {
            "kind": "command_admitted",
            "request_id": "00000000-0000-7000-8000-000000000055",
            "digest": "migration",
            "expected_revision": null,
            "lease": "00000000-0000-7000-8000-000000000056",
            "executor_id": "00000000-0000-7000-8000-000000000057",
            "prompt": "migration",
            "model": "model/test",
            "start": true
        },
        "status": "running",
        "observed_at": 0,
        "pruned_receipts": []
    })
}

#[test]
fn lease_held_writer_is_the_only_evidence_store_and_physical_order_is_authoritative() {
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let attempt = AttemptRef::first(GenerationId::new());
    let opened = store.open_writer(&session, lease()).unwrap();
    let service = EvidenceAdmissionService::new();

    service
        .admit_run_context(&opened.writer, context(session, invocation))
        .unwrap();
    service
        .admit_generation(&opened.writer, generation(session, invocation, attempt))
        .unwrap();
    opened
        .writer
        .append(&session, &generation_started(attempt))
        .unwrap();

    let records = entries(&store, &session);
    assert!(matches!(
        records[0],
        SessionEntry::Evidence(EvidenceRecord::RunContext(_))
    ));
    assert!(matches!(
        records[1],
        SessionEntry::Evidence(EvidenceRecord::GenerationAdmission(_))
    ));
    assert!(matches!(
        records[2],
        SessionEntry::Event(DomainEvent {
            env: Envelope { seq: position, .. },
            kind: EventKind::GenerationStarted {
                attempt: actual,
                role: GenerationRole::Main, snapshot: None, cache: None, },
        }) if actual == attempt && position == seq(1)
    ));
}

#[test]
fn rooted_command_and_generation_evidence_precede_one_matching_start() {
    let root = tempdir().unwrap();
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let run_id = RunId::new(session, invocation.uuid());
    let log = log_path(root.path(), &session);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut initial = encode_header(&Header {
        profiles: Default::default(),
        v: CODEC_VERSION,
        session,
        created: Timestamp::from_millis(1),
        workspace_root: "/workspace".into(),
    });
    initial.push(b'\n');
    let mut admitted = legacy_run_transaction(&session);
    admitted["run_id"] = serde_json::to_value(run_id).unwrap();
    admitted["command"]["executor_id"] = serde_json::to_value(run_id.nonce()).unwrap();
    let admitted_entry = serde_json::json!({
        "kind": "run_transaction",
        "run_transaction": admitted,
    });
    initial.extend_from_slice(&encode_record(seq(1), &admitted_entry).unwrap());
    initial.push(b'\n');
    std::fs::write(&log, initial).unwrap();

    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let opened = store.open_writer(&session, lease()).unwrap();
    let service = EvidenceAdmissionService::new();
    let attempt = AttemptRef::first(GenerationId::new());
    service
        .admit_run_context(&opened.writer, context(session, invocation))
        .unwrap();
    service
        .admit_generation(&opened.writer, generation(session, invocation, attempt))
        .unwrap();
    opened
        .writer
        .append(&session, &generation_started(attempt))
        .unwrap();

    let records = entries(&store, &session);
    assert!(matches!(
        &records[0],
        SessionEntry::RunTransaction(transaction)
            if transaction.run_id() == run_id
                && matches!(transaction.command(), ego_core::run::RunCommand::CommandAdmitted { .. })
    ));
    assert!(matches!(
        &records[1],
        SessionEntry::Evidence(EvidenceRecord::RunContext(evidence))
            if evidence.session() == session
                && evidence.invocation() == invocation
                && evidence.run_id() == run_id
    ));
    assert!(matches!(
        &records[2],
        SessionEntry::Evidence(EvidenceRecord::GenerationAdmission(evidence))
            if evidence.session() == session
                && evidence.invocation() == invocation
                && evidence.attempt() == attempt
    ));
    assert_eq!(
        records
            .iter()
            .filter(|entry| matches!(
                entry,
                SessionEntry::Event(DomainEvent {
                    kind: EventKind::GenerationStarted { attempt: actual, .. },
                    ..
                }) if *actual == attempt
            ))
            .count(),
        1
    );
    assert!(matches!(
        &records[3],
        SessionEntry::Event(DomainEvent {
            kind: EventKind::GenerationStarted { attempt: actual, .. },
            ..
        }) if *actual == attempt
    ));
}

#[test]
fn run_and_generation_retries_are_idempotent_but_changed_inputs_conflict_without_mutation() {
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let attempt = AttemptRef::first(GenerationId::new());
    let opened = store.open_writer(&session, lease()).unwrap();
    let service = EvidenceAdmissionService::new();
    let run = context(session, invocation);
    let generation = generation(session, invocation, attempt);
    service
        .admit_run_context(&opened.writer, run.clone())
        .unwrap();
    service
        .admit_generation(&opened.writer, generation.clone())
        .unwrap();
    let before = entries(&store, &session);

    assert!(
        service
            .admit_run_context(&opened.writer, run)
            .unwrap()
            .is_already_recorded()
    );
    assert!(
        service
            .admit_generation(&opened.writer, generation)
            .unwrap()
            .is_already_recorded()
    );
    let changed = RunContextEvidence::new(
        session,
        invocation,
        RunId::new(session, invocation.uuid()),
        ConfigProvenance::resolved(Some(ActiveProfileProvenance {
            name: "different".to_owned(),
            selected_by: Source::Flags,
            definitions: OriginSet::just(Source::Flags),
        })),
    );
    assert!(matches!(
        service.admit_run_context(&opened.writer, changed),
        Err(EvidenceError::IdempotencyConflict { .. })
    ));
    assert_eq!(entries(&store, &session), before);
}

#[test]
fn schema_v1_run_context_is_checksum_valid_but_rejected_without_rewriting_source() {
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();

    // Open the real lease-held writer before installing the adversarial source
    // image so admission exercises the production writer boundary below.
    let opened = store.open_writer(&session, lease()).unwrap();
    let mut payload = serde_json::to_value(SessionEntry::Evidence(EvidenceRecord::RunContext(
        context(session, invocation),
    )))
    .unwrap();
    payload["evidence"]["version"] = json!(1);
    let mut legacy_payload = payload["evidence"].clone();
    legacy_payload.as_object_mut().unwrap().remove("kind");
    let legacy: RunContextEvidence = serde_json::from_value(legacy_payload).unwrap();

    let log = log_path(root.path(), &session);
    let mut image = encode_header(&Header {
        profiles: Default::default(),
        v: CODEC_VERSION,
        session,
        created: Timestamp::from_millis(1),
        workspace_root: "/workspace".into(),
    });
    image.push(b'\n');
    let record_line = encode_record(seq(1), &payload).unwrap();
    assert!(matches!(
        decode_line(&record_line).unwrap(),
        StoreLine::Record(record) if record.seq == seq(1)
    ));
    image.extend_from_slice(&record_line);
    image.push(b'\n');
    std::fs::write(&log, image).unwrap();
    let before = std::fs::read(&log).unwrap();

    let service = EvidenceAdmissionService::new();
    assert!(matches!(
        service.admit_run_context(&opened.writer, legacy.clone()),
        Err(EvidenceError::UnsupportedVersion {
            found: 1,
            required: 3,
            ..
        })
    ));
    assert_eq!(std::fs::read(&log).unwrap(), before);
    drop(opened);

    assert!(matches!(
        ManifestEvidence::from_records(&[EvidenceRecord::RunContext(legacy.clone())]),
        Err(
            ego_core::evidence::ManifestEvidenceError::UnsupportedVersion {
                found: 1,
                required: 3,
            }
        )
    ));
    let persisted_rebuild = ManifestEvidence::rebuild(&store, &session);
    assert!(
        matches!(
            &persisted_rebuild,
            Err(ego_core::evidence::ManifestEvidenceError::Store(message))
                if message.contains("unknown payload in the valid session prefix")
        ),
        "persisted reader must reject the stale payload before projection: {persisted_rebuild:?}"
    );
    assert_eq!(std::fs::read(&log).unwrap(), before);

    assert!(store.open_writer(&session, lease()).is_err());
    assert_eq!(std::fs::read(&log).unwrap(), before);
}

#[test]
fn generation_evidence_requires_matching_run_and_new_attempt_for_changed_target() {
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let attempt = AttemptRef::first(GenerationId::new());
    let opened = store.open_writer(&session, lease()).unwrap();
    let service = EvidenceAdmissionService::new();
    assert!(matches!(
        service.admit_generation(&opened.writer, generation(session, invocation, attempt)),
        Err(EvidenceError::RunContextRequired { .. })
    ));
    service
        .admit_run_context(&opened.writer, context(session, invocation))
        .unwrap();
    service
        .admit_generation(&opened.writer, generation(session, invocation, attempt))
        .unwrap();
    assert!(matches!(
        service.admit_generation(
            &opened.writer,
            generation_with(
                session,
                invocation,
                attempt,
                GenerationTarget::new(
                    "other-provider",
                    ModelRef("other/model".to_owned()),
                    "other-wire",
                ),
                ModelCapabilities {
                    max_output_tokens: Some(2048),
                    ..capabilities()
                },
                PricingEvidence::Unpriced {
                    source: "other-fixture".to_owned(),
                    reason: "metered".to_owned(),
                },
            )
        ),
        Err(EvidenceError::IdempotencyConflict { .. })
    ));
    let changed_attempt = attempt.retry();
    assert!(
        service
            .admit_generation(
                &opened.writer,
                generation(session, invocation, changed_attempt)
            )
            .is_ok()
    );
}

#[test]
fn same_attempt_ref_cannot_be_admitted_for_two_invocations_without_log_mutation() {
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let first = RunInvocationId::new();
    let second = RunInvocationId::new();
    let attempt = AttemptRef::first(GenerationId::new());
    let opened = store.open_writer(&session, lease()).unwrap();
    let service = EvidenceAdmissionService::new();
    service
        .admit_run_context(&opened.writer, context(session, first))
        .unwrap();
    service
        .admit_generation(&opened.writer, generation(session, first, attempt))
        .unwrap();
    service
        .admit_run_context(&opened.writer, context(session, second))
        .unwrap();
    let before = entries(&store, &session);
    assert!(matches!(
        service.admit_generation(&opened.writer, generation(session, second, attempt)),
        Err(EvidenceError::AttemptOwnershipConflict {
            session: actual_session,
            attempt: actual_attempt,
            existing_invocation,
            requested_invocation,
        }) if actual_session == session
            && actual_attempt == attempt
            && existing_invocation == first
            && requested_invocation == second
    ));
    assert_eq!(entries(&store, &session), before);
}

#[test]
fn manifest_evidence_rebuilds_from_authoritative_prefix_and_preserves_pricing_state() {
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let opened = store.open_writer(&session, lease()).unwrap();
    let service = EvidenceAdmissionService::new();
    service
        .admit_run_context(&opened.writer, context(session, invocation))
        .unwrap();
    service
        .admit_generation(
            &opened.writer,
            generation(session, invocation, AttemptRef::first(GenerationId::new())),
        )
        .unwrap();
    drop(opened);

    let manifest = ManifestEvidence::rebuild(&store, &session).unwrap();
    assert_eq!(manifest.run_contexts().len(), 1);
    assert_eq!(manifest.generations().len(), 1);
    assert!(matches!(
        manifest.generations()[0].pricing(),
        PricingEvidence::Unpriced { .. }
    ));
}

#[test]
fn manifest_cache_is_rebuilt_and_unknown_evidence_prefix_fails_closed() {
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let opened = store.open_writer(&session, lease()).unwrap();
    let service = EvidenceAdmissionService::new();
    service
        .admit_run_context(&opened.writer, context(session, invocation))
        .unwrap();
    service
        .admit_generation(
            &opened.writer,
            generation(session, invocation, AttemptRef::first(GenerationId::new())),
        )
        .unwrap();
    drop(opened);

    let expected = ManifestEvidence::rebuild(&store, &session).unwrap();
    let manifest = manifest_path(root.path(), &session);
    std::fs::remove_file(&manifest).unwrap();
    assert_eq!(
        ManifestEvidence::rebuild(&store, &session).unwrap(),
        expected
    );
    std::fs::write(&manifest, b"not a manifest").unwrap();
    assert_eq!(
        ManifestEvidence::rebuild(&store, &session).unwrap(),
        expected
    );

    let log = log_path(root.path(), &session);
    let before_log = std::fs::read(&log).unwrap();
    let unknown = serde_json::json!({
        "kind": "evidence",
        "evidence": {"kind": "future_evidence", "version": CODEC_VERSION + 1}
    });
    let mut damaged = before_log.clone();
    damaged.extend_from_slice(&encode_record(seq(3), &unknown).unwrap());
    damaged.push(b'\n');
    std::fs::write(&log, &damaged).unwrap();
    let before_manifest = std::fs::read(&manifest).unwrap();
    assert!(ManifestEvidence::rebuild(&store, &session).is_err());
    assert_eq!(std::fs::read(&log).unwrap(), damaged);
    assert_eq!(std::fs::read(&manifest).unwrap(), before_manifest);
}

#[test]
fn retired_v10_log_is_refused_before_evidence_rebuild_and_unchanged() {
    let root = tempdir().unwrap();
    let session = SessionId::new();
    let log = log_path(root.path(), &session);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut legacy = encode_header(&Header {
        profiles: Default::default(),
        v: 10,
        session,
        created: Timestamp::from_millis(1),
        workspace_root: "/workspace".into(),
    });
    legacy.push(b'\n');
    legacy.extend_from_slice(
        &encode_record(
            seq(19),
            &generation_started(AttemptRef::first(GenerationId::new())),
        )
        .unwrap(),
    );
    legacy.push(b'\n');
    legacy.extend_from_slice(&encode_record(seq(23), &legacy_run_transaction(&session)).unwrap());
    legacy.push(b'\n');
    std::fs::write(&log, &legacy).unwrap();

    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let before = std::fs::read(&log).unwrap();
    assert!(matches!(
        store.recover(&session),
        Err(StoreError::UnsupportedLog { found: 10 })
    ));
    assert_eq!(std::fs::read(&log).unwrap(), before);
    assert!(matches!(
        ManifestEvidence::rebuild(&store, &session),
        Err(ego_core::evidence::ManifestEvidenceError::Store(message))
            if message.contains("codec version 10")
    ));
    assert_eq!(std::fs::read(&log).unwrap(), before);
}

#[test]
fn final_codec_evidence_is_tagged_and_does_not_infer_from_lifecycle_events() {
    let record = EvidenceRecord::RunContext(context(SessionId::new(), RunInvocationId::new()));
    let entry = SessionEntry::Evidence(record);
    let wire = serde_json::to_value(entry).unwrap();
    assert_eq!(wire["kind"], "evidence");
    assert_eq!(wire["evidence"]["kind"], "run_context");
    let lifecycle = serde_json::to_value(SessionEntry::Event(generation_started(
        AttemptRef::first(GenerationId::new()),
    )))
    .unwrap();
    assert!(lifecycle["evidence"].is_null());
}

#[test]
fn old_capabilities_resume_names_unsupported_schema_instead_of_unknown_entry() {
    // Catches discarded strict-decoder diagnostics on both resume reader paths.
    use ego_core::session::ReplaySource;
    use ego_core::session_lifecycle::SessionLifecycleStore;
    let root = tempdir().unwrap();
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let attempt = AttemptRef::first(GenerationId::new());
    let mut wire = serde_json::to_value(SessionEntry::Evidence(
        EvidenceRecord::GenerationAdmission(generation(session, invocation, attempt)),
    ))
    .unwrap();
    wire["evidence"]["capabilities"]["schema_version"] = json!(5);
    wire["evidence"]["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("audio_input");
    let log = write_evidence_log(root.path(), session, &[wire]);
    let original = std::fs::read(&log).unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let reasons = [
        (
            ReplaySource::entries(&store, &session)
                .unwrap_err()
                .to_string(),
            "Seq(1)",
        ),
        (
            store.lifecycle_meta(session).unwrap_err().to_string(),
            "record 1",
        ),
    ];
    for (reason, position) in reasons {
        assert!(
            reason.contains("old, unsupported capabilities schema 5"),
            "{reason}"
        );
        assert!(reason.contains("requires schema 6"), "{reason}");
        assert!(reason.contains(&session.to_string()), "{reason}");
        assert!(
            reason.contains(position),
            "missing physical record: {reason}"
        );
    }
    assert_eq!(std::fs::read(&log).unwrap(), original);
}

#[test]
fn capabilities_diagnostics_do_not_misclassify_future_or_malformed_records() {
    // Catches old-schema wording for future data and false schema classification.
    use ego_core::session::ReplaySource;
    let root = tempdir().unwrap();
    for (schema, malformed) in [(6, false), (7, false), (5, true)] {
        let session = SessionId::new();
        let mut wire = serde_json::to_value(SessionEntry::Evidence(
            EvidenceRecord::GenerationAdmission(generation(
                session,
                RunInvocationId::new(),
                AttemptRef::first(GenerationId::new()),
            )),
        ))
        .unwrap();
        wire["evidence"]["capabilities"]["schema_version"] = json!(schema);
        if malformed {
            wire["evidence"]["target"] = json!(null);
        }
        write_evidence_log(root.path(), session, &[wire]);
        let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
        let result = ReplaySource::entries(&store, &session);
        if schema == 6 {
            assert_eq!(result.unwrap().unwrap().len(), 1);
        } else {
            let reason = result.unwrap_err().to_string();
            assert!(
                !reason.contains("old, unsupported capabilities"),
                "{reason}"
            );
            if !malformed {
                assert!(
                    reason.contains("newer, unsupported capabilities schema 7"),
                    "{reason}"
                );
                assert!(reason.contains("requires schema 6"), "{reason}");
            }
        }
    }
}

#[test]
fn generation_evidence_rejects_old_outer_and_inner_versions_at_every_authority() {
    assert_eq!(EVIDENCE_SCHEMA_VERSION, 3);
    assert_eq!(ModelCapabilities::SCHEMA_VERSION, 6);

    let root = tempdir().unwrap();
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let attempt = AttemptRef::first(GenerationId::new());
    let current_context = context(session, invocation);
    let current = EvidenceRecord::GenerationAdmission(generation(session, invocation, attempt));
    let current_wire = serde_json::to_value(SessionEntry::Evidence(current)).unwrap();
    let current_generation: GenerationAdmissionEvidence =
        serde_json::from_value(current_wire["evidence"].clone()).unwrap();
    assert_eq!(current_generation.version(), EVIDENCE_SCHEMA_VERSION);
    assert_eq!(
        current_generation.capabilities().schema_version,
        ModelCapabilities::SCHEMA_VERSION
    );

    let current_session = SessionId::new();
    let current_invocation = RunInvocationId::new();
    let current_attempt = AttemptRef::first(GenerationId::new());
    let current_context_wire = serde_json::to_value(SessionEntry::Evidence(
        EvidenceRecord::RunContext(context(current_session, current_invocation)),
    ))
    .unwrap();
    let current_generation_wire =
        serde_json::to_value(SessionEntry::Evidence(EvidenceRecord::GenerationAdmission(
            generation(current_session, current_invocation, current_attempt),
        )))
        .unwrap();
    let current_log = write_evidence_log(
        root.path(),
        current_session,
        &[current_context_wire, current_generation_wire],
    );
    let current_store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let current_entries = entries(&current_store, &current_session);
    assert!(matches!(
        &current_entries[0],
        SessionEntry::Evidence(EvidenceRecord::RunContext(value))
            if value.session() == current_session && value.invocation() == current_invocation
    ));
    assert!(matches!(
        &current_entries[1],
        SessionEntry::Evidence(EvidenceRecord::GenerationAdmission(value))
            if value.session() == current_session
                && value.invocation() == current_invocation
                && value.attempt() == current_attempt
                && value.version() == EVIDENCE_SCHEMA_VERSION
                && value.capabilities().schema_version == ModelCapabilities::SCHEMA_VERSION
    ));
    let current_manifest = ManifestEvidence::rebuild(&current_store, &current_session).unwrap();
    assert_eq!(current_manifest.run_contexts().len(), 1);
    assert_eq!(current_manifest.generations().len(), 1);
    assert!(std::fs::metadata(current_log).is_ok());

    let service = EvidenceAdmissionService::new();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let opened = store.open_writer(&session, lease()).unwrap();
    opened
        .writer
        .write(
            EvidenceKey::RunContext {
                session,
                invocation,
            },
            EvidenceRecord::RunContext(current_context),
        )
        .unwrap();
    opened
        .writer
        .write(
            EvidenceKey::GenerationAdmission {
                session,
                invocation,
                attempt,
            },
            EvidenceRecord::GenerationAdmission(current_generation.clone()),
        )
        .unwrap();

    for (outer, inner) in [(3, 4), (2, 5)] {
        let mut wire = current_wire.clone();
        wire["evidence"]["version"] = json!(outer);
        wire["evidence"]["capabilities"]["schema_version"] = json!(inner);
        let stale: GenerationAdmissionEvidence =
            serde_json::from_value(wire["evidence"].clone()).unwrap();
        let expected_found = if outer == 2 { outer } else { inner };
        let expected_required = if outer == 2 {
            EVIDENCE_SCHEMA_VERSION
        } else {
            ModelCapabilities::SCHEMA_VERSION
        };
        assert!(matches!(
            service.admit_generation(&opened.writer, stale.clone()),
            Err(EvidenceError::UnsupportedVersion { found, required, .. })
                if found == expected_found && required == expected_required
        ));

        let stale_session = SessionId::new();
        let stale_invocation = RunInvocationId::new();
        let stale_attempt = AttemptRef::first(GenerationId::new());
        let stale_record =
            serde_json::to_value(SessionEntry::Evidence(EvidenceRecord::GenerationAdmission(
                generation(stale_session, stale_invocation, stale_attempt),
            )))
            .unwrap();
        let mut typed_wire = stale_record.clone();
        typed_wire["evidence"]["version"] = json!(outer);
        typed_wire["evidence"]["capabilities"]["schema_version"] = json!(inner);
        let typed_stale: GenerationAdmissionEvidence =
            serde_json::from_value(typed_wire["evidence"].clone()).unwrap();
        let stale_context = context(stale_session, stale_invocation);
        let stale_store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
        let stale_opened = stale_store.open_writer(&stale_session, lease()).unwrap();
        stale_opened
            .writer
            .write(
                EvidenceKey::RunContext {
                    session: stale_session,
                    invocation: stale_invocation,
                },
                EvidenceRecord::RunContext(stale_context.clone()),
            )
            .unwrap();
        assert!(matches!(
            stale_opened.writer.write(
                EvidenceKey::GenerationAdmission {
                    session: stale_session,
                    invocation: stale_invocation,
                    attempt: stale_attempt,
                },
                EvidenceRecord::GenerationAdmission(typed_stale.clone()),
            ),
            Err(EvidenceError::UnsupportedVersion { found, required, .. })
                if found == expected_found && required == expected_required
        ));

        drop(stale_opened);

        let stale_context_wire = serde_json::to_value(SessionEntry::Evidence(
            EvidenceRecord::RunContext(context(stale_session, stale_invocation)),
        ))
        .unwrap();
        typed_wire["evidence"]["session"] = serde_json::to_value(stale_session).unwrap();
        let stale_log = write_evidence_log(
            root.path(),
            stale_session,
            &[stale_context_wire, typed_wire],
        );
        let persisted = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
        let read = persisted.read(&stale_session);
        match read {
            Err(_) => {}
            Ok(stream) => assert!(
                block_on(stream.collect::<Vec<_>>())
                    .into_iter()
                    .any(|entry| entry.is_err()),
                "persisted read accepted evidence outer {outer} / capabilities {inner}"
            ),
        }
        assert!(
            ManifestEvidence::rebuild(&persisted, &stale_session).is_err(),
            "persisted manifest must refuse evidence outer {outer} / capabilities {inner}"
        );
        let manifest_error = ManifestEvidence::from_records(&[
            EvidenceRecord::RunContext(stale_context),
            EvidenceRecord::GenerationAdmission(typed_stale.clone()),
        ])
        .unwrap_err();
        let expected_found = if outer == 2 { outer } else { inner };
        let expected_required = if outer == 2 {
            EVIDENCE_SCHEMA_VERSION
        } else {
            ModelCapabilities::SCHEMA_VERSION
        };
        assert!(matches!(
            manifest_error,
            ego_core::evidence::ManifestEvidenceError::UnsupportedVersion { found, required }
                if found == expected_found && required == expected_required
        ));
        assert!(std::fs::metadata(stale_log).is_ok());
    }
}

// Catches: unchanged catalogues grow the journal on every generation, or an
// empty catalogue is confused with absent historical evidence after reopen.
#[test]
fn tool_catalogue_is_deduplicated_and_empty_catalogue_survives_reopen() {
    use ego_core::evidence::{EvidenceLogSource, ToolCatalogueEvidence};
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let invocation = RunInvocationId::new();
    let service = EvidenceAdmissionService::new();
    let tools = Vec::<ego_core::generator::types::ToolDescriptor>::new();
    let digest = service.canonical_digest(&tools).unwrap();
    let record = EvidenceRecord::ToolCatalogue(ToolCatalogueEvidence {
        version: EVIDENCE_SCHEMA_VERSION,
        session,
        digest: digest.clone(),
        tools,
    });
    let opened = store.open_writer(&session, lease()).unwrap();
    service
        .admit_run_context(&opened.writer, context(session, invocation))
        .unwrap();
    opened.writer.write(record.key(), record.clone()).unwrap();
    let before = std::fs::metadata(log_path(root.path(), &session))
        .unwrap()
        .len();
    assert!(
        opened
            .writer
            .write(record.key(), record.clone())
            .unwrap()
            .is_already_recorded()
    );
    assert_eq!(
        before,
        std::fs::metadata(log_path(root.path(), &session))
            .unwrap()
            .len()
    );
    service
        .admit_generation(
            &opened.writer,
            generation(session, invocation, AttemptRef::first(GenerationId::new()))
                .with_tool_catalogue(digest.clone()),
        )
        .unwrap();
    drop(opened);
    let reopened = store.open_writer(&session, lease()).unwrap();
    assert!(
        reopened
            .writer
            .write(record.key(), record)
            .unwrap()
            .is_already_recorded()
    );
    let records = store.evidence_records(&session).unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, EvidenceRecord::ToolCatalogue(_)))
            .count(),
        1
    );
    assert!(records.iter().any(|r| matches!(r, EvidenceRecord::GenerationAdmission(g) if g.tool_catalogue() == Some(&digest))));
}

// Catches: corrupted descriptor bytes are accepted under another catalogue's digest.
#[test]
fn tool_catalogue_digest_mismatch_is_refused() {
    use ego_core::evidence::ToolCatalogueEvidence;
    let root = tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/workspace", Box::new(OsIo));
    let session = SessionId::new();
    let opened = store.open_writer(&session, lease()).unwrap();
    let record = EvidenceRecord::ToolCatalogue(ToolCatalogueEvidence {
        version: EVIDENCE_SCHEMA_VERSION,
        session,
        digest: ego_core::evidence::EvidenceDigest::of_bytes(b"not the catalogue"),
        tools: Vec::new(),
    });
    assert!(opened.writer.write(record.key(), record).is_err());
}

// Catches: one strict-decoder failure aborts history search or leaves stale hits
// instead of returning readable sessions and naming the unreadable session.
#[test]
fn history_search_skips_and_reports_unsupported_capabilities_session() {
    let root = tempdir().unwrap();
    let unreadable = SessionId::new();
    let readable = SessionId::new();
    let mut wire = serde_json::to_value(SessionEntry::Evidence(
        EvidenceRecord::GenerationAdmission(generation(
            unreadable,
            RunInvocationId::new(),
            AttemptRef::first(GenerationId::new()),
        )),
    ))
    .unwrap();
    wire["evidence"]["capabilities"]["schema_version"] = json!(5);
    let log = write_evidence_log(root.path(), unreadable, &[wire]);
    let original = std::fs::read(&log).unwrap();
    let store = std::sync::Arc::new(JsonlStore::new(root.path(), "/workspace", Box::new(OsIo)));
    let mut prompt = generation_started(AttemptRef::first(GenerationId::new()));
    prompt.kind = EventKind::UserPrompt {
        text: "history-readable-KDM".to_owned(),
        images: Vec::new(),
        audio: Vec::new(),
        queued_prompt: None,
    };
    store.append(&readable, &prompt).unwrap();
    for _ in 0..2 {
        let output = store.history().search("history-readable-KDM").unwrap();
        assert!(output.len() <= 1024);
        let result: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(result["hits"][0]["id"], readable.to_string());
        assert_eq!(result["unreadable"][0]["id"], unreadable.to_string());
        assert!(
            result["unreadable"][0]["error"]
                .as_str()
                .unwrap()
                .contains("unsupported capabilities schema 5")
        );
        assert_eq!(result["omitted_unreadable"], 0);
    }
    assert_eq!(std::fs::read(&log).unwrap(), original);
}
