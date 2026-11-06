//! A sanitized export is a producer-only bundle.

use std::collections::{BTreeMap, HashMap};

use ego_core::config::ModelRef;
use ego_core::config::{ActiveProfileProvenance, OriginSet, Source};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason, GenerationRole};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{
    ConfigProvenance, EvidenceKey, EvidenceRecord, GenerationAdmissionEvidence, GenerationTarget,
    PricingEvidence, RunContextEvidence, RunInvocationId,
};
use ego_core::export::{ExportError, ExportEvidenceFailure, Exporter};
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::{ErrorKind, ImageMediaType, ImageRef};
use ego_core::permit::{CanonPath, Escalation, OpName, ToolResult};
use ego_core::run::RunId;
use ego_core::session::{
    ReplayError, ReplaySource, ReplaySourceError, SessionBlobSource, SessionLineage,
};
use ego_core::store::blob::{BlobError, BlobId, BlobRef};
use ego_core::store::session_store::SessionEntry;

#[derive(Clone)]
struct ExportFixture {
    source: SessionId,
    lineages: BTreeMap<SessionId, Option<SessionLineage>>,
    entries: BTreeMap<SessionId, Result<Vec<SessionEntry>, ReplaySourceError>>,
    blobs: HashMap<(SessionId, BlobRef), Result<Vec<u8>, BlobError>>,
}

impl ReplaySource for ExportFixture {
    fn lineage(&self, session: &SessionId) -> Result<Option<SessionLineage>, ReplaySourceError> {
        Ok(self.lineages.get(session).copied().flatten())
    }

    fn entries(&self, session: &SessionId) -> Result<Option<Vec<SessionEntry>>, ReplaySourceError> {
        match self.entries.get(session) {
            Some(Ok(entries)) => Ok(Some(entries.clone())),
            Some(Err(error)) => Err(error.clone()),
            None => Ok(None),
        }
    }
}

impl SessionBlobSource for ExportFixture {
    fn read(&self, session: &SessionId, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        self.blobs
            .get(&(*session, reference.clone()))
            .cloned()
            .unwrap_or_else(|| {
                Err(BlobError::NotFound {
                    reference: reference.id.clone(),
                })
            })
    }
}

fn event(seq: &mut Seq, kind: EventKind) -> SessionEntry {
    let position = *seq;
    seq.next();
    SessionEntry::Event(DomainEvent {
        env: Envelope {
            seq: position,
            id: EventId::new(),
            turn: TurnId::new(),
            item: Some(ItemId::new()),
            parent: None,
            ts: Timestamp::from_millis(1_001),
            producer: Producer::Main,
        },
        kind,
    })
}

fn context(session: SessionId) -> SessionEntry {
    context_for(session, RunInvocationId::new())
}

fn context_for(session: SessionId, invocation: RunInvocationId) -> SessionEntry {
    let run_id = format!("{session}.{}", invocation.uuid())
        .parse::<RunId>()
        .expect("typed run identity");
    SessionEntry::Evidence(EvidenceRecord::RunContext(RunContextEvidence::new(
        session,
        invocation,
        run_id,
        ConfigProvenance::resolved(Some(ActiveProfileProvenance {
            name: "ci".to_owned(),
            selected_by: Source::Flags,
            definitions: OriginSet::just(Source::Flags),
        })),
    )))
}

fn generation_for(
    session: SessionId,
    invocation: RunInvocationId,
    attempt: AttemptRef,
) -> SessionEntry {
    SessionEntry::Evidence(EvidenceRecord::GenerationAdmission(
        GenerationAdmissionEvidence::new(
            session,
            invocation,
            attempt,
            GenerationRole::Main,
            GenerationTarget::new(
                "provider",
                ModelRef("provider/model".to_owned()),
                "wire-model",
            ),
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
            },
            PricingEvidence::Unpriced {
                source: "fixture".to_owned(),
                reason: "subscription".to_owned(),
            },
        ),
    ))
}

fn generation_started(attempt: AttemptRef) -> SessionEntry {
    let mut sequence = Seq::ZERO;
    event(
        &mut sequence,
        EventKind::GenerationStarted {
            attempt,
            role: GenerationRole::Main,
            cache: None,
            snapshot: None,
        },
    )
}

fn admitted_command(session: SessionId, invocation: RunInvocationId) -> SessionEntry {
    let run_id = RunId::new(session, invocation.uuid());
    serde_json::from_value(serde_json::json!({
        "kind": "run_transaction",
        "run_transaction": {
            "revision": 1,
            "run_id": run_id,
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
                "request_id": invocation.uuid(),
                "digest": "export-order",
                "expected_revision": null,
                "lease": "00000000-0000-7000-8000-000000000056",
                "executor_id": run_id.nonce(),
                "prompt": "export-order",
                "model": "model/test",
                "start": true
            },
            "status": "running",
            "observed_at": 0,
            "pruned_receipts": []
        }
    }))
    .expect("admitted command fixture")
}

fn fixture() -> (ExportFixture, SessionId, Vec<u8>) {
    let source = SessionId::new();
    let compacted = SessionId::new();
    let target = SessionId::new();
    let blob_bytes = b"BLOB-PAYLOAD-SECRET".to_vec();
    let blob = BlobRef::new(
        BlobId::parse(ego_core::digest::sha256_hex(&blob_bytes)).expect("blob digest"),
        blob_bytes.len() as u64,
    );
    let image = ImageRef::new(blob.clone(), ImageMediaType::Png, 1, 1).expect("image metadata");

    let mut source_seq = Seq::ZERO;
    let source_entries = vec![event(
        &mut source_seq,
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "SECRET-PROMPT".to_owned(),
            images: vec![image],
            queued_prompt: None,
        },
    )];

    let mut compacted_seq = Seq::ZERO;
    let compacted_entries = vec![event(
        &mut compacted_seq,
        EventKind::AssistantMessage {
            text: "retained assistant fact".to_owned(),
        },
    )];

    let mut target_seq = Seq::ZERO;
    let target_entries = vec![
        context(target),
        event(
            &mut target_seq,
            EventKind::ReasoningDelta {
                text: "HIDDEN-REASONING".to_owned(),
            },
        ),
        event(
            &mut target_seq,
            EventKind::ToolCallStarted {
                name: "secret_tool".to_owned(),
                args: "{\"token\":\"SECRET-TOKEN\"}".to_owned(),
                call_id: Some("provider-call".to_owned()),
            },
        ),
        event(
            &mut target_seq,
            EventKind::PermissionRequested {
                tool: "secret_tool".to_owned(),
                escalation: Escalation::NoRuleCovers {
                    op: OpName::Read,
                    path: CanonPath::new("/private/SECRET-PATH"),
                },
            },
        ),
        event(
            &mut target_seq,
            EventKind::ToolCallCompleted {
                ok: false,
                result: ToolResult::builtin("SECRET-TOOL-RESULT"),
            },
        ),
        event(
            &mut target_seq,
            EventKind::Error {
                message: "RAW-PROVIDER-ERROR".to_owned(),
                kind: ErrorKind::Unclassified,
            },
        ),
        event(
            &mut target_seq,
            EventKind::TurnCompleted {
                ok: false,
                exit_reason: ExitReason::Provider,
            },
        ),
    ];

    let mut lineages = BTreeMap::new();
    lineages.insert(source, None);
    lineages.insert(
        compacted,
        Some(SessionLineage::Compaction {
            source,
            source_seq: Seq::ZERO,
            prompt_revision: 1,
        }),
    );
    lineages.insert(
        target,
        Some(SessionLineage::Fork {
            source: compacted,
            at_seq: Seq::ZERO,
        }),
    );

    let mut entries = BTreeMap::new();
    entries.insert(source, Ok(source_entries));
    entries.insert(compacted, Ok(compacted_entries));
    entries.insert(target, Ok(target_entries));

    let mut blobs = HashMap::new();
    blobs.insert((source, blob), Ok(blob_bytes.clone()));

    (
        ExportFixture {
            source,
            lineages,
            entries,
            blobs,
        },
        target,
        blob_bytes,
    )
}

fn source_bytes(fixture: &ExportFixture) -> Vec<Vec<u8>> {
    fixture
        .entries
        .values()
        .filter_map(|entries| entries.as_ref().ok())
        .flat_map(|entries| entries.iter())
        .map(|entry| serde_json::to_vec(entry).expect("entry bytes"))
        .collect()
}

#[test]
fn exporter_is_producer_only_and_emits_the_exact_versioned_shape() {
    let (fixture, target, _) = fixture();
    let bundle = Exporter::new(&fixture)
        .sanitized(target)
        .expect("valid evidence and lineage export");
    let encoded = serde_json::to_string(&bundle).expect("export bundle serializes");
    let field_markers = ["{\"v\":1,\"target\":", ",\"records\":", ",\"redactions\":"];
    let field_positions = field_markers.map(|marker| encoded.find(marker).expect("field marker"));
    assert!(
        field_positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{encoded}"
    );
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("bundle JSON");
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("bundle object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["records", "redactions", "target", "v"]);
    assert_eq!(value["v"], 1);
    assert!(value["records"].is_array(), "{value}");
    assert!(value["redactions"].is_array(), "{value}");
}

#[test]
fn exporter_projects_adversarial_typed_entries_without_opaque_material() {
    let (fixture, target, blob_bytes) = fixture();
    let before = source_bytes(&fixture);
    let bundle = Exporter::new(&fixture)
        .sanitized(target)
        .expect("adversarial entries are structurally sanitized");
    let encoded = serde_json::to_string(&bundle).expect("export bundle serializes");
    for secret in [
        "SECRET-PROMPT",
        "HIDDEN-REASONING",
        "SECRET-TOKEN",
        "SECRET-PATH",
        "SECRET-TOOL-RESULT",
        "RAW-PROVIDER-ERROR",
        std::str::from_utf8(&blob_bytes).expect("blob marker"),
    ] {
        assert!(!encoded.contains(secret), "{secret} leaked in {encoded}");
    }
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("bundle JSON");
    let records = value["records"].as_array().expect("records");
    assert_eq!(
        records.first().and_then(|record| record["kind"].as_str()),
        Some("session")
    );
    assert!(
        records
            .iter()
            .any(|record| record["kind"] == "blob_reference")
    );
    let ledger = value["redactions"].as_array().expect("redaction ledger");
    assert_eq!(
        ledger
            .iter()
            .map(|entry| entry["category"].as_str().expect("category"))
            .collect::<Vec<_>>(),
        [
            "opaque_prose",
            "hidden_reasoning",
            "path",
            "tool_material",
            "permission_material",
            "raw_error",
            "blob_payload",
        ]
    );
    assert_eq!(
        ledger
            .iter()
            .map(|entry| (
                entry["category"].as_str().expect("category"),
                entry["count"].as_u64().expect("count"),
            ))
            .collect::<Vec<_>>(),
        [
            ("opaque_prose", 2),
            ("hidden_reasoning", 1),
            ("path", 1),
            ("tool_material", 4),
            ("permission_material", 1),
            ("raw_error", 1),
            ("blob_payload", 1),
        ]
    );
    assert_eq!(
        before,
        source_bytes(&fixture),
        "export mutated source bytes"
    );
}

#[test]
fn exporter_emits_oldest_lineage_first_and_physical_positions() {
    let (fixture, target, _) = fixture();
    let bundle = Exporter::new(&fixture)
        .sanitized(target)
        .expect("fork and compaction lineage is exportable");
    let value = serde_json::to_value(bundle).expect("bundle value");
    let records = value["records"].as_array().expect("records");
    let sessions: Vec<_> = records
        .iter()
        .filter(|record| record["kind"] == "session")
        .map(|record| record["session"].clone())
        .collect();
    assert_eq!(sessions.len(), 3);
    for record in records.iter().filter(|record| record["kind"] == "event") {
        assert!(record["position"].is_u64(), "{record}");
        assert!(record["event_seq"].is_u64(), "{record}");
    }
}

#[test]
fn unknown_durable_entry_fails_closed_and_preserves_source_bytes() {
    let (mut fixture, target, _) = fixture();
    let source = fixture.source;
    fixture.entries.insert(
        source,
        Err(ReplaySourceError::UnknownEntry {
            session: source,
            position: Seq::ZERO,
            reason: "unknown fixture entry".to_owned(),
        }),
    );
    let before = source_bytes(&fixture);
    let error = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("unknown entry");
    assert!(matches!(
        error,
        ExportError::Replay(ReplayError::UnknownEntry { session, .. }) if session == source
    ));
    assert_eq!(before, source_bytes(&fixture));
}

#[test]
fn missing_lineage_source_fails_closed_and_preserves_source_bytes() {
    let (mut fixture, target, _) = fixture();
    let missing = SessionId::new();
    fixture.lineages.insert(
        target,
        Some(SessionLineage::Fork {
            source: missing,
            at_seq: Seq::ZERO,
        }),
    );
    let before = source_bytes(&fixture);
    let error = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("missing source");
    assert!(matches!(
        error,
        ExportError::Replay(ReplayError::MissingSession { session }) if session == missing
    ));
    assert_eq!(before, source_bytes(&fixture));
}

#[test]
fn corrupt_session_fails_closed_and_preserves_source_bytes() {
    let (mut fixture, target, _) = fixture();
    let source = fixture.source;
    fixture.entries.insert(
        source,
        Err(ReplaySourceError::CorruptSession {
            session: source,
            reason: "checksum mismatch".to_owned(),
        }),
    );
    let before = source_bytes(&fixture);
    let error = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("corrupt source");
    assert!(matches!(
        error,
        ExportError::Replay(ReplayError::CorruptSession { session, .. }) if session == source
    ));
    assert_eq!(before, source_bytes(&fixture));
}

#[test]
fn cycle_fails_closed_and_preserves_source_bytes() {
    let (mut fixture, target, _) = fixture();
    let source = fixture.source;
    fixture.lineages.insert(
        source,
        Some(SessionLineage::Fork {
            source: target,
            at_seq: Seq::ZERO,
        }),
    );
    let before = source_bytes(&fixture);
    let error = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("lineage cycle");
    assert!(matches!(
        error,
        ExportError::Replay(ReplayError::Cycle { .. })
    ));
    assert_eq!(before, source_bytes(&fixture));
}

#[test]
fn missing_and_corrupt_blob_references_fail_closed_without_source_mutation() {
    let (mut missing_fixture, target, _) = fixture();
    let before = source_bytes(&missing_fixture);
    missing_fixture.blobs.clear();
    let missing = Exporter::new(&missing_fixture)
        .sanitized(target)
        .expect_err("missing blob");
    assert!(matches!(
        missing,
        ExportError::Replay(ReplayError::MissingBlob { .. })
    ));
    assert_eq!(before, source_bytes(&missing_fixture));

    let (mut fixture, target, _) = fixture();
    let source = fixture.source;
    let reference = fixture
        .blobs
        .keys()
        .find(|(session, _)| *session == source)
        .map(|(_, reference)| reference.clone())
        .expect("blob reference");
    fixture.blobs.insert(
        (source, reference.clone()),
        Err(BlobError::WrongLength {
            expected: reference.bytes,
            found: reference.bytes + 1,
        }),
    );
    let before = source_bytes(&fixture);
    let corrupt = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("corrupt blob");
    assert!(matches!(
        corrupt,
        ExportError::Replay(ReplayError::CorruptBlob { .. })
    ));
    assert_eq!(before, source_bytes(&fixture));
}

#[test]
fn invalid_run_evidence_fails_closed_before_rendering_or_mutating_sources() {
    let (mut fixture, target, _) = fixture();
    let other = SessionId::new();
    fixture.entries.insert(target, Ok(vec![context(other)]));
    let before = source_bytes(&fixture);
    let error = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("mismatched evidence");
    assert!(matches!(error, ExportError::Evidence { .. }));
    assert_eq!(before, source_bytes(&fixture));
}

#[test]
fn rooted_command_must_precede_matching_run_context() {
    let (mut fixture, target, _) = fixture();
    let invocation = RunInvocationId::new();
    fixture.entries.insert(
        target,
        Ok(vec![
            SessionEntry::Evidence(EvidenceRecord::RunContext(RunContextEvidence::new(
                target,
                invocation,
                RunId::new(target, invocation.uuid()),
                ConfigProvenance::default(),
            ))),
            admitted_command(target, invocation),
        ]),
    );

    let error = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("run context before command admission");
    assert!(matches!(
        error,
        ExportError::Evidence {
            session,
            failure: ExportEvidenceFailure::OutOfOrder {
                key: EvidenceKey::RunContext {
                    session: recorded_session,
                    invocation: recorded_invocation,
                },
            },
            ..
        } if session == target
            && recorded_session == target
            && recorded_invocation == invocation
    ));
}

#[test]
fn same_attempt_ref_across_invocations_is_rejected_without_bundle_or_source_mutation() {
    let (mut fixture, target, _) = fixture();
    let first = RunInvocationId::new();
    let second = RunInvocationId::new();
    let attempt = AttemptRef::first(ego_core::event::id::GenerationId::new());
    fixture.entries.insert(
        target,
        Ok(vec![
            admitted_command(target, first),
            context_for(target, first),
            generation_for(target, first, attempt),
            admitted_command(target, second),
            context_for(target, second),
            generation_for(target, second, attempt),
            generation_started(attempt),
        ]),
    );
    let before = source_bytes(&fixture);
    let error = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("reusing an AttemptRef across invocations");
    assert!(matches!(
        error,
        ExportError::Evidence {
            failure: ExportEvidenceFailure::AttemptOwnershipConflict {
                attempt: actual,
                existing_invocation,
                requested_invocation,
            },
            ..
        } if actual == attempt
            && existing_invocation == first
            && requested_invocation == second
    ));
    assert_eq!(source_bytes(&fixture), before);
}

/// The redaction gate is *field*-exhaustive, not just variant-exhaustive.
///
/// Exhaustive field matching makes new durable fields require explicit
/// export classification. Rest patterns would compile silently when a
/// sensitive field is added, weakening that protection to variants alone.
///
/// The compiler cannot state this rule, so the source does. Reading the file
/// is the whole test — it costs no fixture and no bundle, and it fails the
/// moment someone reaches for `..` to make a new field compile.
#[test]
fn no_sanitizer_drops_a_field_without_naming_it() {
    const SOURCE: &str = include_str!("../../src/export.rs");
    // Only the two disclosure boundaries. Evidence validation reads fields to
    // build keys and discloses nothing, so a rest pattern there is ordinary
    // Rust rather than an unclassified redaction.
    for function in [
        "fn project_event(",
        "fn project_run_command(",
        "fn project_detached_exec(",
        "fn project_question_outcome(",
    ] {
        let start = SOURCE
            .find(function)
            .unwrap_or_else(|| panic!("{function} still exists"));
        let body = &SOURCE[start..];
        let end = body
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{function} has a terminated body"));
        let offending: Vec<_> = body[..end]
            .lines()
            .filter(|line| line.contains(".."))
            .collect();
        assert!(
            offending.is_empty(),
            "{function} drops a field without naming it: {offending:?}"
        );
    }
}

// Catches: export dropping an explicit model choice from an otherwise complete session stream.
#[test]
fn exporter_preserves_the_session_model_choice_at_its_physical_position() {
    let (mut fixture, target, _) = fixture();
    let entries = fixture.entries.get_mut(&target).unwrap().as_mut().unwrap();
    entries.push(SessionEntry::ModelSelected(ModelRef("ollama/qwen3".into())));
    let position = entries.len();
    let bundle = Exporter::new(&fixture).sanitized(target).unwrap();
    let value = serde_json::to_value(bundle).unwrap();
    let choice = value["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["kind"] == "model_selected")
        .unwrap();
    assert_eq!(choice["session"], target.to_string());
    assert_eq!(choice["position"], position);
    assert_eq!(choice["model"], "ollama/qwen3");
}

// Catches: private judge provider text leaking through public projection or sanitized export.
#[test]
fn raw_judge_response_stays_private_in_projection_and_export() {
    let (mut fixture, target, _) = fixture();
    let entries = fixture.entries.get_mut(&target).unwrap().as_mut().unwrap();
    let mut seq = entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(event.env.seq),
            _ => None,
        })
        .max()
        .unwrap();
    seq.next();
    let raw = "PRIVATE-JUDGE-TEXT";
    let response = event(
        &mut seq,
        EventKind::JudgeResponse {
            attempt: AttemptRef::first(ego_core::event::id::GenerationId::new()),
            text: raw.into(),
            truncated: false,
        },
    );
    let SessionEntry::Event(domain) = &response else {
        unreachable!()
    };
    assert!(ego_core::public::project_public(domain).is_none());
    entries.push(response);
    let exported = Exporter::new(&fixture).sanitized(target).unwrap();
    let encoded = serde_json::to_string(&exported).unwrap();
    assert!(!encoded.contains(raw));
    let value = serde_json::to_value(exported).unwrap();
    let response = value["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["event"]["kind"] == "judge_response")
        .expect("response metadata survives export");
    assert_eq!(response["event"]["bytes"], raw.len());
    assert_eq!(response["event"]["truncated"], false);
}

// Catches: a residual judge decision exports the refused path or the judge's
// reason, which name host files and private task text.
#[test]
fn residual_judge_decision_exports_its_outcome_without_path_or_reason() {
    let (mut fixture, target, _) = fixture();
    let entries = fixture.entries.get_mut(&target).unwrap().as_mut().unwrap();
    let mut seq = entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(event.env.seq),
            _ => None,
        })
        .max()
        .unwrap();
    seq.next();
    let (path, reason) = ("/PRIVATE-HOST/exports", "PRIVATE-JUDGE-REASON");
    let decided = event(
        &mut seq,
        EventKind::SandboxResidualDecided {
            path: CanonPath::new(path),
            outcome: ego_core::permit::residual::ResidualOutcome::Granted,
            recommended: Some(ego_core::permit::residual::ResidualScope::Session),
            reason: reason.into(),
            attempt: Some(AttemptRef::first(ego_core::event::id::GenerationId::new())),
        },
    );
    let SessionEntry::Event(domain) = &decided else {
        unreachable!()
    };
    assert!(ego_core::public::project_public(domain).is_none());
    entries.push(decided);
    let exported = Exporter::new(&fixture).sanitized(target).unwrap();
    let encoded = serde_json::to_string(&exported).unwrap();
    assert!(
        !encoded.contains(path) && !encoded.contains(reason),
        "{encoded}"
    );
    let value = serde_json::to_value(exported).unwrap();
    let record = value["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["event"]["kind"] == "sandbox_residual_decided")
        .expect("the decision survives export");
    assert_eq!(record["event"]["outcome"], "granted");
    assert_eq!(record["event"]["recommended"], "session");
}

// Catches: goal/fallback diagnostics or permission reasons leak through sanitized export.
#[test]
fn exporter_redacts_goal_fallback_and_permission_material_but_keeps_decisions() {
    use ego_core::event::domain::FallbackDecision;
    use ego_core::event::id::GoalId;
    use ego_core::goal::{GoalOwner, GoalTransition};
    use ego_core::run::RequestId;
    let (mut fixture, target, _) = fixture();
    fixture.lineages.insert(target, None);
    let id = GoalId::new();
    let request = RequestId::from_uuid(uuid::Uuid::new_v4());
    let secret = "PRIVATE-POLICY-MATERIAL";
    let mut seq = Seq::ZERO;
    let kinds = vec![
        EventKind::SessionTitleChanged {
            title: Some(secret.into()),
        },
        EventKind::SessionPauseRequested { request },
        EventKind::SessionPaused { request },
        EventKind::SessionPauseObserved { request },
        EventKind::SessionResumed { request },
        EventKind::GoalSet {
            id,
            objective: secret.into(),
            owner: GoalOwner::Operator,
            ceilings: Default::default(),
            derived_from: None,
        },
        EventKind::GoalAllowanceInherited {
            id,
            continuation_budget: Some(2),
            continuation_turns: 1,
        },
        EventKind::GoalCheckConfigured {
            id,
            continuation_budget: 2,
        },
        EventKind::GoalContinuation {
            id,
            text: secret.into(),
        },
        EventKind::GoalUpdated {
            id,
            objective: secret.into(),
        },
        EventKind::GoalCheckStopped {
            id,
            reason: secret.into(),
        },
        EventKind::GoalTransitioned {
            id,
            transition: GoalTransition::BlockerObserved {
                reason: secret.into(),
            },
        },
        EventKind::FallbackDecision {
            candidate: ModelRef("provider/model".into()),
            decision: FallbackDecision::ExpressibilitySkipped {
                message: secret.into(),
            },
        },
        EventKind::FallbackDecision {
            candidate: ModelRef("provider/model".into()),
            decision: FallbackDecision::ResolutionSkipped {
                message: secret.into(),
            },
        },
        EventKind::FallbackDecision {
            candidate: ModelRef("provider/model".into()),
            decision: FallbackDecision::Skipped { gaps: vec![] },
        },
        EventKind::FallbackDecision {
            candidate: ModelRef("provider/model".into()),
            decision: FallbackDecision::ReasoningDropped {
                dropped_blocks: 2,
                dialects: vec!["opaque".into()],
            },
        },
        EventKind::AssistantDelta {
            text: secret.into(),
        },
        EventKind::HostContextAppended {
            text: secret.into(),
        },
        EventKind::CompactionSeeded {
            source: fixture.source,
            source_seq: Seq::ZERO,
            prompt_revision: 1,
            checkpoint: secret.into(),
            latest_user_request: Some(secret.into()),
            constraints: Default::default(),
        },
        EventKind::PinsChanged {
            constraints: serde_json::from_value(
                serde_json::json!({"next_id": 1, "entries": {"1": secret}}),
            )
            .unwrap(),
        },
        EventKind::Compacted {
            summary: secret.into(),
        },
        EventKind::PermissionAmended {
            tool: secret.into(),
        },
        EventKind::PermissionDecided {
            allowed: Some(false),
            by: Some(ego_core::permit::Authority::Party),
            reason: Some(secret.into()),
        },
        EventKind::ToolCallDelta {
            text: secret.into(),
        },
    ];
    let count = kinds.len();
    fixture.entries.insert(
        target,
        Ok(std::iter::once(context(target))
            .chain(kinds.into_iter().map(|kind| event(&mut seq, kind)))
            .collect()),
    );
    let before = source_bytes(&fixture);
    let bundle = Exporter::new(&fixture).sanitized(target).unwrap();
    let encoded = serde_json::to_string(&bundle).unwrap();
    assert!(!encoded.contains(secret), "{encoded}");
    let value = serde_json::to_value(bundle).unwrap();
    let events: Vec<_> = value["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["kind"] == "event")
        .collect();
    assert_eq!(events.len(), count);
    let ledger: BTreeMap<_, _> = value["redactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["category"].as_str().unwrap(),
                entry["count"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(ledger["opaque_prose"], 14);
    assert_eq!(ledger["permission_material"], 4);
    assert_eq!(ledger["tool_material"], 1);
    assert_eq!(before, source_bytes(&fixture));
    assert!(encoded.contains("\"allowed\":false"));
}

#[test]
fn round_private_destinations_and_member_outcomes_never_enter_structural_exports() {
    // Catches: a nested round bypasses the export's prose and tool-material redaction.
    use ego_core::question::{
        ElicitationAnswers, ElicitationKind, ElicitationMember, ElicitationRound, QuestionAnswer,
        QuestionMode, QuestionOutcome, QuestionRequest, QuestionTarget,
    };
    let session = SessionId::new();
    let mut seq = Seq::ZERO;
    let responses = BTreeMap::from([(
        "PRIVATE-KEY".to_owned(),
        serde_json::json!({"completed":true}),
    )]);
    let request = QuestionRequest::new(
        "MCP input required",
        QuestionMode::Round(ElicitationRound {
            expires_at_ms: Some(10000),
            requests: BTreeMap::from([(
                "PRIVATE-KEY".to_owned(),
                ElicitationMember {
                    prompt: "PRIVATE-MEMBER-PROMPT".to_owned(),
                    kind: ElicitationKind::Url {
                        url: "https://example.test/PRIVATE-URL?PRIVATE-QUERY=secret".to_owned(),
                        elicitation_id: "PRIVATE-ELICITATION-ID".to_owned(),
                    },
                },
            )]),
            completed: responses.clone(),
        }),
        vec![],
        false,
    );
    let entries = vec![
        event(
            &mut seq,
            EventKind::QuestionRequested {
                request,
                target: QuestionTarget::ForeignContinuation {
                    tool_call: ItemId::new(),
                    continuation_id: "local-operation".to_owned(),
                },
            },
        ),
        event(
            &mut seq,
            EventKind::QuestionMemberSettled {
                key: "PRIVATE-KEY".to_owned(),
                answer: serde_json::json!({"completed":true}),
            },
        ),
        event(
            &mut seq,
            EventKind::QuestionSettled {
                outcome: QuestionOutcome::Answered(QuestionAnswer::Round(ElicitationAnswers {
                    confirmed: true,
                    responses,
                })),
            },
        ),
    ];
    let fixture = ExportFixture {
        source: session,
        lineages: BTreeMap::from([(session, None)]),
        entries: BTreeMap::from([(session, Ok(entries))]),
        blobs: HashMap::new(),
    };
    let exported = Exporter::new(&fixture).sanitized(session).unwrap();
    let encoded = serde_json::to_string(&exported).unwrap();
    assert!(!encoded.contains("PRIVATE-"));
    assert!(!encoded.contains("example.test"));
    assert!(encoded.contains("question_member_settled"));
}
