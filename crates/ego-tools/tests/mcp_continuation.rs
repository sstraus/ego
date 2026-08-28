//! Coarse RED contract for durable MCP continuation authority.

use ego_core::config::{ConfigLayer, Mode, Ruleset, Source};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject};
use ego_core::permit::{
    CanonPath, ForeignToolId, ForeignTransportTarget, InvocationDigest, PolicyEngine, PreparedCall,
};
use ego_core::suspension::{ContinuationState, ResumeKeySource, ResumeSealer};
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
            br#"{"method":"tools/call"}"#.to_vec(),
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
        McpAdmissionFingerprint::new("capture-7", "weather", "tool-fingerprint")
            .expect("admission"),
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
    let claimed = journal.claim(&id).expect("claim");
    assert!(journal.claim(&id).is_err());
    journal
        .settle(claimed, ContinuationState::Completed)
        .expect("settled");
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
