//! RED contract for regression case: a sanitized export is a producer-only bundle.

use std::collections::{BTreeMap, HashMap};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::{ConfigProvenance, EvidenceRecord, RunContextEvidence, RunInvocationId};
use ego_core::export::{ExportError, Exporter};
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
    let invocation = RunInvocationId::new();
    let run_id = format!("{session}.{}", invocation.uuid())
        .parse::<RunId>()
        .expect("typed run identity");
    SessionEntry::Evidence(EvidenceRecord::RunContext(RunContextEvidence::new(
        session,
        invocation,
        run_id,
        ConfigProvenance::resolved(Some("ci".to_owned())),
    )))
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
    assert!(encoded.starts_with("{\"v\":1,\"target\":"), "{encoded}");
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("bundle JSON");
    let keys: Vec<&str> = value
        .as_object()
        .expect("bundle object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["v", "target", "records", "redactions"]);
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
    assert!(
        ledger
            .iter()
            .all(|entry| entry["count"].as_u64() == Some(1))
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
    let before = source_bytes(&fixture);
    fixture.entries.insert(
        source,
        Err(ReplaySourceError::UnknownEntry {
            session: source,
            position: Seq::ZERO,
        }),
    );
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
    let before = source_bytes(&fixture);
    fixture.entries.insert(
        source,
        Err(ReplaySourceError::CorruptSession {
            session: source,
            reason: "checksum mismatch".to_owned(),
        }),
    );
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
    let before = source_bytes(&fixture);
    fixture.entries.insert(target, Ok(vec![context(other)]));
    let error = Exporter::new(&fixture)
        .sanitized(target)
        .expect_err("mismatched evidence");
    assert!(matches!(error, ExportError::Evidence { .. }));
    assert_eq!(before, source_bytes(&fixture));
}
