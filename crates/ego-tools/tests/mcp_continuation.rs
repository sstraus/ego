//! Coarse RED contract for durable MCP continuation authority.

use ego_core::config::{ConfigLayer, Mode, Ruleset, Source};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject};
use ego_core::permit::{
    CanonPath, ForeignToolId, ForeignTransportTarget, InvocationDigest, PolicyEngine, PreparedCall,
};
use ego_core::suspension::{ContinuationJournal, ContinuationState, ResumeKeySource, ResumeSealer};
use ego_core::turn::GenerationPolicyDigest;
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use serde_json::json;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const CAPTURE: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const GENERATION_POLICY_DIGEST: &str =
    "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn generation_policy_digest() -> GenerationPolicyDigest {
    GenerationPolicyDigest::try_from(GENERATION_POLICY_DIGEST.to_owned())
        .expect("valid deterministic generation policy digest")
}

#[derive(Clone)]
struct FakeResumeKey {
    key: Arc<[u8]>,
}
impl ResumeKeySource for FakeResumeKey {
    fn resume_key(&self) -> Result<Arc<[u8]>, ego_core::suspension::ContinuationError> {
        Ok(Arc::clone(&self.key))
    }
}

struct FixedClock(Timestamp);
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

fn record(
    authorized: &ego_core::permit::Authorized,
    key: Arc<[u8]>,
    now: Timestamp,
    lifetime_ms: u64,
) -> ego_core::suspension::ContinuationRecord {
    let sealer = ResumeSealer::new(
        Arc::new(FakeResumeKey { key }),
        Arc::new(FixedClock(now)),
        lifetime_ms,
    )
    .expect("sealer");
    let floor = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        Ruleset::from_layer(
            ConfigLayer::parse(Source::Project, "[permissions]\nallow=[]")
                .expect("rules")
                .value
                .permissions
                .expect("permissions"),
            Source::Project,
        ),
    )
    .resume_floor();
    sealer
        .prepare(authorized, floor)
        .expect("prepared resume")
        .into_record(
            "continuation-1",
            generation_policy_digest(),
            br#"{"requestState":"opaque"}"#.to_vec(),
            "request-1",
            br#"{"type":"object"}"#.to_vec(),
            "profile-7",
        )
        .expect("record")
}

fn now() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    Timestamp::from_millis(millis)
}

fn persisted_wires(root: &std::path::Path) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(root.join("continuations.jsonl")).expect("journal bytes");
    std::str::from_utf8(&bytes)
        .expect("journal UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("journal wire JSON"))
        .collect()
}

fn persisted_wire(root: &std::path::Path) -> serde_json::Value {
    persisted_wires(root)
        .into_iter()
        .next()
        .expect("journal line")
}

fn authorized() -> ego_core::permit::Authorized {
    let permissions = ConfigLayer::parse(
        Source::Project,
        "[permissions]\nallow=[\"tool(alpha/weather)\",\"net(mcp.example:443)\"]",
    )
    .expect("rules")
    .value
    .permissions
    .expect("permissions");
    let rules = Ruleset::from_layer(permissions, Source::Project);
    let call = PreparedCall::mcp(
        McpSubject::Tool {
            server: "alpha".into(),
            name: "weather".into(),
        },
        McpOperationKind::CallTool,
        vec![(
            ForeignToolId::new("alpha", "weather").expect("tool"),
            InvocationDigest::parse(&format!("sha256:{:064x}", 1)).expect("digest"),
        )],
        McpAdmissionFingerprint::new(CAPTURE, "weather", "tool-fingerprint").expect("admission"),
        "profile-7",
        ForeignTransportTarget::http(
            ego_core::permit::HttpRoute::new(
                ego_core::permit::HttpScheme::Https,
                ego_core::permit::NetworkTarget::new("mcp.example", 443).expect("target"),
            )
            .expect("route"),
        ),
        &serde_json::json!({"name":"weather","arguments":{}}),
    )
    .expect("MCP call");
    match PolicyEngine::new(CanonPath::new("/workspace"), Mode::Default, rules).classify(call) {
        ego_core::permit::Classification::Settled(Ok(value)) => value,
        other => panic!("authorized fixture must settle: {other:?}"),
    }
}

#[test]
fn accepted_answer_is_durable_before_atomic_single_use_claim() {
    let root = tempfile::tempdir().expect("directory");
    let key = FakeResumeKey {
        key: Arc::from(b"deterministic-key-0123456789abcdef".as_slice()),
    };
    let journal = JsonlContinuationJournal::open(root.path()).expect("journal");
    let authorized = authorized();
    let id = journal
        .accept(
            record(&authorized, Arc::clone(&key.key), now(), 300_000),
            br#"{"inputResponses":[{"answer":"yes"}]}"#,
        )
        .expect("accepted");
    let wire = persisted_wire(root.path());
    assert_eq!(wire["version"], json!(2));
    assert_eq!(
        wire["record"]["generation_policy_digest"],
        json!(GENERATION_POLICY_DIGEST)
    );
    assert_eq!(wire["record"]["subject"]["capture"], json!(CAPTURE));
    assert_eq!(wire["record"]["fingerprints"]["capture"], json!(CAPTURE));
    let serialized = serde_json::to_string(&wire).expect("wire JSON");
    assert!(!serialized.contains("admission_generation"));
    assert!(!serialized.contains("generation-"));
    let claimed = journal.claim(&id).expect("claim");
    assert!(journal.claim(&id).is_err());
    journal
        .settle(claimed, ContinuationState::Completed)
        .expect("settled");
    let committed = persisted_wires(root.path());
    assert_eq!(committed.len(), 3);
    assert_eq!(
        committed
            .iter()
            .map(|snapshot| snapshot["record"]["state"].as_str())
            .collect::<Vec<_>>(),
        vec![Some("accepted"), Some("claimed"), Some("completed")]
    );
    for snapshot in committed {
        assert_eq!(snapshot["version"], json!(2));
        assert_eq!(
            snapshot["record"]["generation_policy_digest"],
            json!(GENERATION_POLICY_DIGEST)
        );
    }
}

#[test]
fn stable_capture_identity_survives_reopen_and_resume_validation() {
    let root = tempfile::tempdir().expect("directory");
    let key = FakeResumeKey {
        key: Arc::from(b"deterministic-key-0123456789abcdef".as_slice()),
    };
    let writer = JsonlContinuationJournal::open(root.path()).expect("writer");
    let authorized = authorized();
    let call_digest = authorized.call().authority_fingerprint();
    let prepared_fingerprint = format!(
        "sha256:{}",
        call_digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let id = writer
        .accept(
            record(&authorized, Arc::clone(&key.key), now(), 300_000),
            br#"{"inputResponses":[{"answer":"yes"}]}"#,
        )
        .expect("accepted");

    let reopened = JsonlContinuationJournal::open(root.path()).expect("reopen");
    let resumed = reopened
        .resume_with_fingerprint(&id, CAPTURE, "tool-fingerprint", &prepared_fingerprint)
        .expect("stable capture resumes after reopen");
    let loaded_policy_digest: &GenerationPolicyDigest = resumed.generation_policy_digest();
    assert_eq!(loaded_policy_digest, &generation_policy_digest());
    assert_eq!(resumed.capture(), CAPTURE);
    assert_eq!(resumed.fingerprints().capture(), CAPTURE);
}

#[test]
fn journal_refuses_v1_and_v2_without_policy_digest_before_record_use() {
    let key = FakeResumeKey {
        key: Arc::from(b"deterministic-key-0123456789abcdef".as_slice()),
    };
    let authorized = authorized();
    let record = record(&authorized, key.key, now(), 300_000);
    let id = record.id().to_owned();
    let record_wire = serde_json::to_value(record).expect("record JSON");

    let v1_root = tempfile::tempdir().expect("v1 directory");
    let v1_wire = json!({
        "version": 1,
        "record": record_wire.clone(),
    });
    std::fs::write(
        v1_root.path().join("continuations.jsonl"),
        format!(
            "{}\n",
            serde_json::to_string(&v1_wire).expect("v1 wire JSON")
        ),
    )
    .expect("v1 snapshot");
    let v1 = JsonlContinuationJournal::open(v1_root.path()).expect("v1 journal");
    assert!(
        matches!(
            ContinuationJournal::load(&v1, &id),
            Err(ego_core::suspension::ContinuationError::Corrupt(_))
        ),
        "an otherwise valid v1 record must fail during journal read"
    );

    let v2_root = tempfile::tempdir().expect("v2 directory");
    let mut missing_digest = record_wire.clone();
    let removed_digest = missing_digest
        .as_object_mut()
        .expect("record wire is an object")
        .remove("generation_policy_digest")
        .expect("fixture carries a required generation policy digest");
    assert_eq!(removed_digest, json!(GENERATION_POLICY_DIGEST));
    let v2_wire = json!({
        "version": 2,
        "record": missing_digest,
    });
    std::fs::write(
        v2_root.path().join("continuations.jsonl"),
        format!(
            "{}\n",
            serde_json::to_string(&v2_wire).expect("v2 wire JSON")
        ),
    )
    .expect("v2 snapshot without digest");
    let v2 = JsonlContinuationJournal::open(v2_root.path()).expect("v2 journal");
    assert!(
        matches!(
            ContinuationJournal::load(&v2, &id),
            Err(ego_core::suspension::ContinuationError::Corrupt(_))
        ),
        "a v2 record without generation policy digest must fail during journal read"
    );

    let assert_corrupt_digest = |digest: serde_json::Value, label: &str| {
        let root = tempfile::tempdir().expect("invalid digest directory");
        let mut invalid_digest = record_wire.clone();
        invalid_digest["generation_policy_digest"] = digest;
        let wire = json!({
            "version": 2,
            "record": invalid_digest,
        });
        std::fs::write(
            root.path().join("continuations.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&wire).expect("invalid digest wire JSON")
            ),
        )
        .expect("invalid digest snapshot");
        let journal = JsonlContinuationJournal::open(root.path()).expect("invalid digest journal");
        assert!(
            matches!(
                ContinuationJournal::load(&journal, &id),
                Err(ego_core::suspension::ContinuationError::Corrupt(_))
            ),
            "{label} must fail during journal read"
        );
    };
    assert_corrupt_digest(json!(7), "a non-string generation policy digest");
    assert_corrupt_digest(
        json!("not-a-generation-policy-digest"),
        "an invalid generation policy digest",
    );
}

#[test]
fn two_reopened_adapters_race_one_non_clone_claim_and_never_duplicate_data() {
    let root = tempfile::tempdir().expect("directory");
    let key = FakeResumeKey {
        key: Arc::from(b"deterministic-key-0123456789abcdef".as_slice()),
    };
    let writer = JsonlContinuationJournal::open(root.path()).expect("writer");
    let authorized = authorized();
    let id = writer
        .accept(
            record(&authorized, Arc::clone(&key.key), now(), 300_000),
            br#"{"inputResponses":[{"answer":"yes"}]}"#,
        )
        .expect("accepted");
    let left = JsonlContinuationJournal::open(root.path()).expect("left");
    let right = JsonlContinuationJournal::open(root.path()).expect("right");
    let a = left.claim(&id).expect("first race");
    let b = right.claim(&id);
    assert!(b.is_err());
    left.settle(a, ContinuationState::Uncertain)
        .expect("uncertain");
    assert!(
        JsonlContinuationJournal::open(root.path())
            .expect("restart")
            .replay(&id)
            .expect("replay")
            .is_none()
    );
}

#[test]
fn crash_after_claim_settles_uncertain_and_key_or_fingerprint_tamper_fails_before_claim() {
    let root = tempfile::tempdir().expect("directory");
    let key = FakeResumeKey {
        key: Arc::from(b"deterministic-key-0123456789abcdef".as_slice()),
    };
    let journal = JsonlContinuationJournal::open(root.path()).expect("journal");
    let authorized = authorized();
    let id = journal
        .accept(
            record(&authorized, Arc::clone(&key.key), now(), 300_000),
            br#"{"inputResponses":[{"answer":"yes"}]}"#,
        )
        .expect("accepted");
    let claimed = journal.claim(&id).expect("claim");
    journal
        .settle(claimed, ContinuationState::Uncertain)
        .expect("uncertain");
    assert_eq!(
        journal.state(&id).expect("state"),
        ContinuationState::Uncertain
    );
    let wrong = FakeResumeKey {
        key: Arc::from(b"different-key-0123456789abcdef".as_slice()),
    };
    assert!(
        journal
            .resume_with_fingerprint(
                &id,
                "capture-other",
                "tool-fingerprint",
                "prepared-call-fingerprint"
            )
            .is_err()
    );
    assert!(wrong.resume_key().is_ok());
}
