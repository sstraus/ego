//! RED contracts for regression case: lease-held authoritative evidence.

use ego_core::config::{ActiveProfileProvenance, ModelRef, OriginSet, Source};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, GenerationRole};
use ego_core::event::id::{EventId, GenerationId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{
    ConfigProvenance, EvidenceAdmissionService, EvidenceError, EvidenceRecord,
    GenerationAdmissionEvidence, GenerationTarget, ManifestEvidence, PricingEvidence,
    RunContextEvidence, RunInvocationId,
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
use ego_core::store::session_store::{SessionEntry, SessionStore};
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
        },
    }
}

fn model() -> ModelRef {
    ModelRef("provider/model".to_owned())
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
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

fn legacy_run_transaction(session: &SessionId) -> serde_json::Value {
    serde_json::json!({
        "revision": 1,
        "run_id": {
            "session_id": serde_json::to_value(session).unwrap(),
            "nonce": "00000000-0000-7000-8000-000000000054"
        },
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
                role: GenerationRole::Main,
            },
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
        v: 8,
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
            required: 2,
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
                required: 2,
            }
        )
    ));
    assert!(matches!(
        ManifestEvidence::rebuild(&store, &session),
        Err(
            ego_core::evidence::ManifestEvidenceError::UnsupportedVersion {
                found: 1,
                required: 2,
            }
        )
    ));
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
fn recognized_v8_legacy_entries_migrate_to_tagged_v9_and_unknown_stays_unchanged() {
    let root = tempdir().unwrap();
    let session = SessionId::new();
    let log = log_path(root.path(), &session);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut legacy = encode_header(&Header {
        v: 8,
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
    let repair = store.recover(&session).unwrap();
    assert!(repair.migrated);
    let migrated = std::fs::read(&log).unwrap();
    let mut lines = migrated
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty());
    let header = decode_line(lines.next().unwrap()).unwrap();
    assert!(matches!(
        header,
        StoreLine::Header(Header {
            v: CODEC_VERSION,
            ..
        })
    ));
    let mut decoded_legacy = Vec::new();
    for line in lines {
        let StoreLine::Record(record) = decode_line(line).unwrap() else {
            continue;
        };
        let entry: SessionEntry = serde_json::from_str(record.payload.get()).unwrap();
        assert!(serde_json::to_value(&entry).unwrap().get("kind").is_some());
        let is_legacy_event = !matches!(&entry, SessionEntry::Evidence(_));
        decoded_legacy.push(entry);
        assert!(is_legacy_event);
        assert!(!record.payload.get().contains("codec9"));
    }
    assert!(
        decoded_legacy
            .iter()
            .any(|entry| matches!(entry, SessionEntry::Event(_)))
    );
    assert!(
        decoded_legacy
            .iter()
            .any(|entry| matches!(entry, SessionEntry::RunTransaction(_)))
    );

    let opened = store.open_writer(&session, lease()).unwrap();
    let service = EvidenceAdmissionService::new();
    service
        .admit_run_context(&opened.writer, context(session, RunInvocationId::new()))
        .unwrap();
    drop(opened);
    let with_evidence = std::fs::read(&log).unwrap();
    let evidence_line = with_evidence
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .rev()
        .find_map(|line| match decode_line(line).unwrap() {
            StoreLine::Record(record) => Some(record),
            StoreLine::Header(_) | StoreLine::Unknown(_) => None,
        })
        .unwrap();
    let evidence: SessionEntry = serde_json::from_str(evidence_line.payload.get()).unwrap();
    assert!(matches!(evidence, SessionEntry::Evidence(_)));
    assert!(!evidence_line.payload.get().contains("codec9"));

    let mut unknown = legacy;
    unknown.extend_from_slice(
        &encode_record(
            seq(29),
            &serde_json::json!({"kind":"legacy_future_payload"}),
        )
        .unwrap(),
    );
    unknown.push(b'\n');
    std::fs::write(&log, &unknown).unwrap();
    let before_unknown = std::fs::read(&log).unwrap();
    assert!(store.recover(&session).is_err());
    assert_eq!(std::fs::read(&log).unwrap(), before_unknown);
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
