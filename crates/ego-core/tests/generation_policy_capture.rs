//! RED contract for the confidential, immutable generation-policy value.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use ego_core::config::{Mode, OriginSet, Ruleset, Source, SourcedRule};
use ego_core::credentials::SecretMaterial;
use ego_core::permit::fake::FakeFs;
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    Authority, CanonPath, Change, Classification, DuplexHandle, DuplexSpawner, Escalation,
    HttpRequest, HumanApprover, OneHop, OneHopResponse, Op, PermissionRequest, PolicyEngine,
    PreparedCall, ProcessError, ProcessExecutor, ResolvedConnect, Resolver, SpawnSpec, ToolResult,
    TransportError,
};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::turn::{
    GenerationPolicyCapture, GenerationPolicyCaptureError, GenerationPolicyCaptureStore,
    GenerationPolicyCaptureStoreError, GenerationPolicyCaptureStoreErrorKind,
    GenerationPolicyCaptureStoreOperation, GenerationPolicyDigest, GenerationPolicySnapshot,
};
use ego_core::turn::{ToolEffectPorts, Tools};
use futures::future::BoxFuture;

mod common;

struct NoProcess;

impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(futures::future::ready(Err(ProcessError::Invariant(
            "process is not used by policy capture tests".into(),
        ))))
    }
}

struct NoEvidence;

impl EvidenceStore for NoEvidence {
    fn blob_store(&self) -> &dyn ego_core::store::blob::BlobStore {
        &common::EMPTY_BLOB_STORE
    }

    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Err(EvidenceError::Limit { limit: 0 })
    }

    fn read_page<'a>(
        &'a self,
        _request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(futures::future::ready(Err(EvidenceError::Limit {
            limit: 0,
        })))
    }
}

struct NoResolver;

impl Resolver for NoResolver {
    fn resolve(
        &self,
        _target: ego_core::permit::NetworkTarget,
        _deadline: Option<Instant>,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, TransportError>> {
        panic!("resolver is not used by policy capture tests")
    }
}

struct NoHop;

impl OneHop for NoHop {
    fn execute(
        &self,
        _connect: ResolvedConnect,
        _request: HttpRequest,
        _deadline: Option<Instant>,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        panic!("network hop is not used by policy capture tests")
    }
}

struct NoDuplex;

impl DuplexSpawner for NoDuplex {
    fn spawn(&self, _spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        panic!("duplex is not used by policy capture tests")
    }
}

const WORKSPACE: &str = "/sentinel/generation-policy-workspace";
const RULE_A: &str = "read(/sentinel/generation-policy-workspace/src/**)";
const RULE_B: &str = "write(/sentinel/generation-policy-workspace/out/**)";
const RULE_C: &str = "exec(sentinel-tool *)";
const RULE_D: &str = "read(/sentinel/generation-policy-workspace/secrets/**)";
const CONTINUATION_ID: &str = "mcp-sentinel-generation-policy-continuation";
const RAW_BYTES: &str = "sentinel-raw-generation-policy-bytes";
const CODEC_LIMIT: usize = 1024 * 1024;

#[derive(Default)]
struct TypedCaptureStore {
    stored: Mutex<Option<(String, Arc<GenerationPolicySnapshot>)>>,
}

impl GenerationPolicyCaptureStore for TypedCaptureStore {
    fn store(
        &self,
        id: &str,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), GenerationPolicyCaptureStoreError> {
        *self.stored.lock().map_err(|_| {
            GenerationPolicyCaptureStoreError::new(
                GenerationPolicyCaptureStoreOperation::Store,
                GenerationPolicyCaptureStoreErrorKind::Unavailable,
            )
        })? = Some((id.to_owned(), snapshot));
        Ok(())
    }

    fn load(
        &self,
        id: &str,
        expected: &GenerationPolicyDigest,
    ) -> Result<Arc<GenerationPolicySnapshot>, GenerationPolicyCaptureStoreError> {
        let stored = self.stored.lock().map_err(|_| {
            GenerationPolicyCaptureStoreError::new(
                GenerationPolicyCaptureStoreOperation::Load,
                GenerationPolicyCaptureStoreErrorKind::Unavailable,
            )
        })?;
        let Some((_, snapshot)) = stored.as_ref().filter(|(stored_id, _)| stored_id == id) else {
            return Err(GenerationPolicyCaptureStoreError::new(
                GenerationPolicyCaptureStoreOperation::Load,
                GenerationPolicyCaptureStoreErrorKind::Missing,
            ));
        };
        if snapshot.digest() != expected.clone() {
            return Err(GenerationPolicyCaptureStoreError::new(
                GenerationPolicyCaptureStoreOperation::Load,
                GenerationPolicyCaptureStoreErrorKind::DigestMismatch,
            ));
        }
        Ok(Arc::clone(snapshot))
    }

    fn delete(&self, id: &str) -> Result<(), GenerationPolicyCaptureStoreError> {
        let mut stored = self.stored.lock().map_err(|_| {
            GenerationPolicyCaptureStoreError::new(
                GenerationPolicyCaptureStoreOperation::Delete,
                GenerationPolicyCaptureStoreErrorKind::Unavailable,
            )
        })?;
        if stored
            .as_ref()
            .is_some_and(|(stored_id, _)| stored_id == id)
        {
            *stored = None;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct AlwaysAllow;

impl HumanApprover for AlwaysAllow {
    fn decide<'a>(
        &'a self,
        _request: &'a PermissionRequest,
    ) -> BoxFuture<'a, ego_core::permit::PermissionDecision> {
        Box::pin(std::future::ready(
            ego_core::permit::PermissionDecision::Allow,
        ))
    }
}

fn rules() -> Ruleset {
    let mut allow = SourcedRule::new(Rule::load(RULE_A).expect("canonical allow"), Source::System);
    allow.also_carried_by(Source::User);
    let mut deny = SourcedRule::new(Rule::load(RULE_B).expect("canonical deny"), Source::User);
    deny.also_carried_by(Source::Project);
    let mut second_deny =
        SourcedRule::new(Rule::load(RULE_D).expect("canonical deny"), Source::Local);
    second_deny.also_carried_by(Source::System);
    Ruleset {
        allow: vec![
            allow,
            SourcedRule::new(Rule::load(RULE_C).expect("canonical allow"), Source::Flags),
        ],
        deny: vec![deny, second_deny],
    }
}

fn capture(mode: Mode, ruleset: Ruleset) -> GenerationPolicyCapture {
    GenerationPolicyCapture::new(CanonPath::new(WORKSPACE), mode, ruleset, None)
        .expect("valid policy capture")
}

fn assert_redacted(debug: impl std::fmt::Debug) {
    let rendered = format!("{debug:?}");
    assert!(
        !rendered.contains(WORKSPACE),
        "debug leaked workspace: {rendered}"
    );
    assert!(!rendered.contains(RULE_A), "debug leaked rule: {rendered}");
    assert!(!rendered.contains(RULE_B), "debug leaked rule: {rendered}");
    assert!(!rendered.contains(RULE_C), "debug leaked rule: {rendered}");
    assert!(!rendered.contains(RULE_D), "debug leaked rule: {rendered}");
}

fn canonical_wire() -> Vec<u8> {
    format!(
        r#"{{"version":1,"workspace":"{WORKSPACE}","mode":"auto","allow":[{{"rule":"{RULE_A}","origins":["system","user"]}},{{"rule":"{RULE_C}","origins":["flags"]}}],"deny":[{{"rule":"{RULE_B}","origins":["user","project"]}},{{"rule":"{RULE_D}","origins":["system","local"]}}]}}"#
    )
    .into_bytes()
}

fn rule_wire(rule: &str, origins: &str) -> String {
    format!(r#"{{"rule":"{rule}","origins":[{origins}]}}"#)
}

fn policy_wire(version: u32, allow: &str, deny: &str) -> Vec<u8> {
    format!(
        r#"{{"version":{version},"workspace":"{WORKSPACE}","mode":"auto","allow":[{allow}],"deny":[{deny}]}}"#
    )
    .into_bytes()
}

fn assert_rule_authority(authority: &Authority, from: Source, also: OriginSet) {
    let Authority::Rules { rules } = authority else {
        panic!("expected rule authority, got {authority:?}");
    };
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].from, from);
    assert_eq!(rules[0].also, also);
}

#[test]
fn confidential_roundtrip_preserves_order_origins_digest_and_encoding() {
    let original = capture(Mode::Auto, rules());
    let snapshot = original.snapshot();
    let encoded = snapshot.encode_confidential();
    let decoded = GenerationPolicySnapshot::decode_confidential(&encoded)
        .expect("canonical snapshot decodes");

    assert_eq!(encoded.expose_secret_bytes(), canonical_wire());
    assert_eq!(decoded.digest(), snapshot.digest());
    assert_eq!(
        decoded.encode_confidential().expose_secret_bytes(),
        canonical_wire()
    );

    let restored = GenerationPolicyCapture::restore(Arc::new(decoded), None)
        .expect("canonical policy restores");
    let policy = restored.policy();

    let Classification::Settled(Ok(allowed)) = policy.classify(PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/sentinel/generation-policy-workspace/src/main.rs"),
        },
    )) else {
        panic!("first ordered allow rule was not restored");
    };
    assert_rule_authority(allowed.by(), Source::System, OriginSet::just(Source::User));

    let Classification::Settled(Err(first_deny)) = policy.classify(PreparedCall::new(
        "edit",
        Op::Write {
            path: CanonPath::new("/sentinel/generation-policy-workspace/out/result.txt"),
            change: Change::Whole(Vec::new()),
        },
    )) else {
        panic!("first ordered deny rule was not restored");
    };
    assert_rule_authority(
        &first_deny.authority(),
        Source::User,
        OriginSet::just(Source::Project),
    );

    let Classification::Settled(Err(second_deny)) = policy.classify(PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/sentinel/generation-policy-workspace/secrets/token"),
        },
    )) else {
        panic!("second ordered deny rule was not restored");
    };
    assert_rule_authority(
        &second_deny.authority(),
        Source::System,
        OriginSet::just(Source::Local),
    );

    let Classification::Ask(pending) = policy.classify(PreparedCall::new(
        "edit",
        Op::Write {
            path: CanonPath::new("/sentinel/generation-policy-workspace/tmp/value"),
            change: Change::Whole(Vec::new()),
        },
    )) else {
        panic!("uncovered write should reach the captured mode");
    };
    let ego_core::permit::Resolution::Ask(pending) =
        futures::executor::block_on(policy.settle(pending))
    else {
        panic!("auto mode should preserve the unresolved mutating call");
    };
    assert!(matches!(
        pending.request().why(),
        Escalation::ModeAsks {
            mode: Mode::Auto,
            ..
        }
    ));

    let Classification::Settled(Err(outside)) = policy.classify(PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/outside/captured-workspace"),
        },
    )) else {
        panic!("restored workspace boundary must remain closed");
    };
    assert_eq!(outside.authority(), Authority::Boundary);
}

#[test]
fn digest_is_domain_separated_stable_and_changes_for_every_semantic_dimension() {
    let base = capture(Mode::Auto, rules()).snapshot().digest();
    assert_eq!(base, capture(Mode::Auto, rules()).snapshot().digest());
    assert_ne!(base, capture(Mode::Default, rules()).snapshot().digest());
    assert_ne!(
        base,
        GenerationPolicyCapture::new(CanonPath::new("/other"), Mode::Auto, rules(), None)
            .unwrap()
            .snapshot()
            .digest()
    );

    let mut reordered = rules();
    reordered.allow.swap(0, 1);
    assert_ne!(base, capture(Mode::Auto, reordered).snapshot().digest());
    let mut changed_text = rules();
    changed_text.allow[0] = SourcedRule::new(
        Rule::load("read(/sentinel/generation-policy-workspace/src/main.rs)").unwrap(),
        Source::System,
    );
    assert_ne!(base, capture(Mode::Auto, changed_text).snapshot().digest());
    let mut changed_polarity = rules();
    changed_polarity.allow.clear();
    changed_polarity.deny.push(SourcedRule::new(
        Rule::load(RULE_A).unwrap(),
        Source::System,
    ));
    assert_ne!(
        base,
        capture(Mode::Auto, changed_polarity).snapshot().digest()
    );
    let mut changed_origin = rules();
    changed_origin.allow[0] = SourcedRule::new(Rule::load(RULE_A).unwrap(), Source::Flags);
    assert_ne!(
        base,
        capture(Mode::Auto, changed_origin).snapshot().digest()
    );
}

fn assert_error_redacted(error: &GenerationPolicyCaptureError) {
    for rendered in [format!("{error:?}"), error.to_string()] {
        assert!(!rendered.contains(WORKSPACE));
        assert!(!rendered.contains(RULE_A));
        assert!(!rendered.contains(RULE_B));
        assert!(!rendered.contains(RULE_C));
        assert!(!rendered.contains(RULE_D));
        assert!(!rendered.contains("sentinel-raw-bytes"));
    }
}

fn assert_decode_rejected(bytes: &[u8], expected: GenerationPolicyCaptureError) {
    let err = GenerationPolicySnapshot::decode_confidential(&SecretMaterial::new(bytes.to_vec()))
        .expect_err("invalid confidential bytes must be rejected");
    assert_eq!(err, expected);
    assert_error_redacted(&err);
}

#[test]
fn decode_rejects_each_invalid_v1_semantic_before_canonical_acceptance() {
    let allow = rule_wire(RULE_A, r#""system","user""#);
    let deny = rule_wire(RULE_B, r#""user","project""#);

    assert_decode_rejected(
        &policy_wire(999, &allow, &deny),
        GenerationPolicyCaptureError::UnsupportedVersion,
    );
    assert_decode_rejected(
        format!(
            r#"{{"version":1,"workspace":"{WORKSPACE}","mode":"auto","allow":[{allow}],"deny":[{deny}],"sentinel-raw-bytes":true}}"#
        )
        .as_bytes(),
        GenerationPolicyCaptureError::InvalidSnapshot,
    );
    assert_decode_rejected(
        &policy_wire(
            1,
            &format!(r#"{{"rule":"{RULE_A}","origins":["system"],"sentinel-raw-bytes":true}}"#),
            &deny,
        ),
        GenerationPolicyCaptureError::InvalidSnapshot,
    );
    assert_decode_rejected(
        &policy_wire(1, &rule_wire(RULE_A, ""), &deny),
        GenerationPolicyCaptureError::InvalidSnapshot,
    );
    assert_decode_rejected(
        &policy_wire(1, &rule_wire(RULE_A, r#""system","system""#), &deny),
        GenerationPolicyCaptureError::InvalidSnapshot,
    );
    assert_decode_rejected(
        &policy_wire(1, &rule_wire(RULE_A, r#""user","system""#), &deny),
        GenerationPolicyCaptureError::InvalidSnapshot,
    );
    assert_decode_rejected(
        &policy_wire(
            1,
            &format!(
                "{},{}",
                rule_wire(RULE_A, r#""system""#),
                rule_wire(RULE_A, r#""user""#)
            ),
            &deny,
        ),
        GenerationPolicyCaptureError::InvalidSnapshot,
    );
    assert_decode_rejected(
        &policy_wire(1, &rule_wire("not-a-rule", r#""system""#), &deny),
        GenerationPolicyCaptureError::InvalidSnapshot,
    );
    assert_decode_rejected(
        &policy_wire(1, &rule_wire("read( src/** )", r#""system""#), &deny),
        GenerationPolicyCaptureError::InvalidSnapshot,
    );

    let mut whitespace = vec![b' '];
    whitespace.extend_from_slice(&canonical_wire());
    whitespace.push(b' ');
    assert_decode_rejected(
        &whitespace,
        GenerationPolicyCaptureError::NonCanonicalSnapshot,
    );
}

fn workspace_for_canonical_size(target: usize) -> String {
    let baseline =
        GenerationPolicyCapture::new(CanonPath::new("/"), Mode::Auto, Ruleset::default(), None)
            .expect("small baseline capture");
    let baseline_len = baseline
        .snapshot()
        .encode_confidential()
        .expose_secret_bytes()
        .len();
    assert!(target >= baseline_len);
    format!("/{}", "a".repeat(target - baseline_len))
}

#[test]
fn codec_accepts_exactly_one_mib_and_rejects_one_more_byte_before_parse() {
    let exact = GenerationPolicyCapture::new(
        CanonPath::new(workspace_for_canonical_size(CODEC_LIMIT)),
        Mode::Auto,
        Ruleset::default(),
        None,
    )
    .expect("the exact codec boundary is accepted");
    let encoded = exact.snapshot().encode_confidential();
    assert_eq!(encoded.expose_secret_bytes().len(), CODEC_LIMIT);
    let decoded = GenerationPolicySnapshot::decode_confidential(&encoded)
        .expect("the exact restored codec boundary is accepted");
    assert_eq!(
        decoded.encode_confidential().expose_secret_bytes(),
        encoded.expose_secret_bytes()
    );

    let mut oversized_restore = encoded.expose_secret_bytes().to_vec();
    oversized_restore.push(b' ');
    assert_eq!(oversized_restore.len(), CODEC_LIMIT + 1);
    assert_decode_rejected(
        &oversized_restore,
        GenerationPolicyCaptureError::InvalidSnapshot,
    );

    let fresh_error = GenerationPolicyCapture::new(
        CanonPath::new(workspace_for_canonical_size(CODEC_LIMIT + 1)),
        Mode::Auto,
        Ruleset::default(),
        None,
    )
    .expect_err("fresh canonical semantics above one MiB must be rejected");
    assert_eq!(fresh_error, GenerationPolicyCaptureError::InvalidSnapshot);
    assert_error_redacted(&fresh_error);
}

#[test]
fn debug_for_snapshot_capture_and_policy_engine_is_confidentially_redacted() {
    let capture = capture(Mode::Default, rules());
    assert_redacted(capture.snapshot());
    assert_redacted(capture);
    assert_redacted(PolicyEngine::new(
        CanonPath::new(WORKSPACE),
        Mode::Default,
        rules(),
    ));
}

#[test]
fn new_and_restore_couple_policy_semantics_and_live_human_port() {
    let with_human = Arc::new(AlwaysAllow);
    let created = GenerationPolicyCapture::new(
        CanonPath::new(WORKSPACE),
        Mode::Default,
        Ruleset::default(),
        Some(with_human.clone()),
    )
    .expect("capture with human approver");
    let snapshot = created.snapshot();
    let restored = GenerationPolicyCapture::restore(snapshot.clone(), Some(with_human))
        .expect("restore with live human approver");
    let call = PreparedCall::new(
        "edit",
        Op::Write {
            path: CanonPath::new("/sentinel/generation-policy-workspace/src/main.rs"),
            change: Change::Whole(b"updated".to_vec()),
        },
    );
    let Classification::Ask(pending) = restored.policy().classify(call) else {
        panic!("ordinary default write should reach the human seat");
    };
    let pending = match futures::executor::block_on(restored.policy().settle(pending)) {
        ego_core::permit::Resolution::Ask(pending) => pending,
        other => panic!("mode settlement must remain open before the human boundary: {other:?}"),
    };
    let fs = FakeFs::default();
    let process = NoProcess;
    let evidence = NoEvidence;
    let resolver = Arc::new(NoResolver);
    let hop = Arc::new(NoHop);
    let duplex = Arc::new(NoDuplex);
    let tools = Tools::new(
        &[],
        ToolEffectPorts::new(&fs, &process, &evidence, resolver, hop, duplex),
        CanonPath::new(WORKSPACE),
    );
    assert!(matches!(
        futures::executor::block_on(tools.decide(restored.policy().as_ref(), pending)),
        ego_core::permit::Resolution::HumanSettled { .. }
    ));

    let no_human = GenerationPolicyCapture::restore(snapshot, None).expect("restore headless");
    let call = PreparedCall::new(
        "edit",
        Op::Write {
            path: CanonPath::new("/sentinel/generation-policy-workspace/src/main.rs"),
            change: Change::Whole(b"updated".to_vec()),
        },
    );
    let Classification::Ask(pending) = no_human.policy().classify(call) else {
        panic!("headless restore must not silently escalate");
    };
    let pending = match futures::executor::block_on(no_human.policy().settle(pending)) {
        ego_core::permit::Resolution::Ask(pending) => pending,
        other => panic!("headless mode settlement must remain open: {other:?}"),
    };
    assert!(matches!(
        futures::executor::block_on(tools.decide(no_human.policy().as_ref(), pending)),
        ego_core::permit::Resolution::Ask(_)
    ));
}

#[test]
fn repeated_access_preserves_arc_ownership_and_restore_reuses_snapshot_arc() {
    let created = capture(Mode::Plan, rules());
    let policy_a = created.policy();
    let policy_b = created.policy();
    let snapshot_a = created.snapshot();
    let snapshot_b = created.snapshot();
    assert!(Arc::ptr_eq(&policy_a, &policy_b));
    assert!(Arc::ptr_eq(&snapshot_a, &snapshot_b));

    let restored = GenerationPolicyCapture::restore(snapshot_a.clone(), None).expect("restore");
    assert!(Arc::ptr_eq(&snapshot_a, &restored.snapshot()));
    assert!(!Arc::ptr_eq(&policy_a, &restored.policy()));
    assert_eq!(snapshot_a.digest(), restored.snapshot().digest());
}

fn assert_send_sync<T: Send + Sync + ?Sized>() {}

#[test]
fn typed_capture_store_contract_carries_only_verified_snapshot_values() {
    assert_send_sync::<dyn GenerationPolicyCaptureStore>();

    let store: Arc<dyn GenerationPolicyCaptureStore> = Arc::new(TypedCaptureStore::default());
    let snapshot = capture(Mode::Auto, rules()).snapshot();
    let expected = snapshot.digest();
    store
        .store(CONTINUATION_ID, Arc::clone(&snapshot))
        .expect("typed snapshot stores");
    let loaded = store
        .load(CONTINUATION_ID, &expected)
        .expect("expected digest loads the typed snapshot");
    assert!(Arc::ptr_eq(&loaded, &snapshot));
    store
        .delete(CONTINUATION_ID)
        .expect("typed snapshot deletes");
}

fn digest_text(digest: &GenerationPolicyDigest) -> String {
    serde_json::to_value(digest)
        .expect("digest serializes")
        .as_str()
        .expect("digest wire is a string")
        .to_owned()
}

#[test]
fn capture_store_errors_are_closed_stable_and_confidentially_redacted() {
    let expected_digest = capture(Mode::Auto, rules()).snapshot().digest();
    let actual_digest = capture(Mode::Plan, rules()).snapshot().digest();
    let expected_digest = digest_text(&expected_digest);
    let actual_digest = digest_text(&actual_digest);
    assert_ne!(expected_digest, actual_digest);
    let sentinels = [
        CONTINUATION_ID,
        WORKSPACE,
        RULE_A,
        RULE_B,
        RULE_C,
        RULE_D,
        expected_digest.as_str(),
        actual_digest.as_str(),
        RAW_BYTES,
    ];
    let operations = [
        GenerationPolicyCaptureStoreOperation::Store,
        GenerationPolicyCaptureStoreOperation::Load,
        GenerationPolicyCaptureStoreOperation::Delete,
    ];
    let kinds = [
        (
            GenerationPolicyCaptureStoreErrorKind::Unavailable,
            "generation policy capture store is unavailable",
        ),
        (
            GenerationPolicyCaptureStoreErrorKind::Missing,
            "generation policy capture is missing",
        ),
        (
            GenerationPolicyCaptureStoreErrorKind::CapacityExceeded,
            "generation policy capture exceeds storage capacity",
        ),
        (
            GenerationPolicyCaptureStoreErrorKind::Corrupt,
            "stored generation policy capture is corrupt",
        ),
        (
            GenerationPolicyCaptureStoreErrorKind::DigestMismatch,
            "stored generation policy capture digest does not match",
        ),
    ];

    for operation in operations {
        for (kind, diagnostic) in kinds {
            let error = GenerationPolicyCaptureStoreError::new(operation, kind);
            assert_eq!(error.operation(), operation);
            assert_eq!(error.kind(), kind);
            assert_eq!(error.diagnostic(), diagnostic);
            for rendered in [format!("{error:?}"), error.to_string()] {
                for sentinel in sentinels {
                    assert!(
                        !rendered.contains(sentinel),
                        "capture-store error leaked confidential input: {rendered}"
                    );
                }
            }
        }
    }
}

// Keep the error type in the contract's import surface even before the
// implementation exists; each rejection above must return this redacted type.
#[allow(dead_code)]
fn _error_is_named(_: GenerationPolicyCaptureError) {}
