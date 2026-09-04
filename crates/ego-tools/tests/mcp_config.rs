//! RED contract for regression case user-config publication and OS credentials.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ego_core::config::{
    ConfigLayer, McpLayerEntry, McpServerConfig, McpServerName, Mode, ProfileName, Ruleset, Source,
    SourcedRule, UserConfigMutation, UserConfigMutationResult, UserConfigPublication,
    UserConfigStore, UserConfigTarget,
};
use ego_core::credentials::{
    CredentialKey, CredentialRemoval, CredentialStore, CredentialTarget, SecretMaterial,
};
use ego_core::permit::CanonPath;
use ego_core::permit::rule::Rule;
use ego_core::turn::{
    GenerationPolicyCapture, GenerationPolicyCaptureStore, GenerationPolicyCaptureStoreError,
    GenerationPolicyCaptureStoreErrorKind, GenerationPolicyCaptureStoreOperation,
    GenerationPolicyDigest, GenerationPolicySnapshot,
};
use ego_tools::config::OsUserConfigStore;
use ego_tools::credentials::OsGenerationPolicyCaptureStore;
use ego_tools::store::{AtomicReplacePublication, OsIo, StoreIo};
use sha2::{Digest, Sha256};

struct FakeIo {
    inner: OsIo,
    bytes: Mutex<Option<Vec<u8>>>,
    uncertain: bool,
}

impl FakeIo {
    fn new() -> Self {
        Self {
            inner: OsIo,
            bytes: Mutex::new(None),
            uncertain: false,
        }
    }
}

impl StoreIo for FakeIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.inner.fsync(path)
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.exists(path)
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        *self.bytes.lock().expect("fake lock") = Some(bytes.to_vec());
        self.inner.replace_atomic(path, bytes)
    }
    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        self.replace_atomic(path, bytes)?;
        if self.uncertain {
            Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
                diagnostic: "uncertain".into(),
            })
        } else {
            Ok(AtomicReplacePublication::PublishedDurably)
        }
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.inner.truncate(path, len)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list_dirs(path)
    }
}

fn fixture(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "# keep\nmodel = 'old'\n\n[mcp.servers.unrelated]\ntransport='stdio'\ncommand='keep'\n",
    )
    .expect("fixture");
    path
}

fn stdio(command: &str) -> McpServerConfig {
    McpServerConfig::Stdio(ego_core::config::McpStdioConfig {
        command: command.into(),
        args: vec![],
        cwd: None,
        inherit_env: vec![],
    })
}

#[test]
fn mcp_set_delete_and_tombstone_preserve_comments_order_and_unrelated_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = fixture(&dir);
    let io = FakeIo::new();
    let store = OsUserConfigStore::new(&io, &path);
    let name: McpServerName = "managed".parse().expect("name");
    let set = UserConfigMutation::McpSet {
        target: UserConfigTarget::Base,
        name: name.clone(),
        expected_user_entry: None,
        server: stdio("first"),
    };
    assert_eq!(
        store.mutate(set).expect("set").publication,
        UserConfigPublication::PublishedDurably
    );
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(text.starts_with("# keep\nmodel = 'old'\n"));
    assert!(text.contains("[mcp.servers.unrelated]"));
    assert!(text.contains("command='keep'"));
    let delete = UserConfigMutation::McpDeleteUserEntry {
        target: UserConfigTarget::Base,
        name: name.clone(),
        expected_entry: McpLayerEntry::Server(stdio("first")),
    };
    assert_eq!(
        store.mutate(delete).expect("delete").publication,
        UserConfigPublication::PublishedDurably
    );
    let insert = UserConfigMutation::McpInsertUserTombstone {
        name,
        expected_user_entry: None,
    };
    assert_eq!(
        store.mutate(insert).expect("tombstone").publication,
        UserConfigPublication::PublishedDurably
    );
    let published = std::fs::read_to_string(&path).expect("read");
    ConfigLayer::parse(Source::User, &published).expect("strict published config");
    let parsed = ConfigLayer::parse(Source::User, &published)
        .expect("strict published config")
        .value;
    assert!(matches!(
        parsed.mcp.servers.get(&"managed".parse().expect("name")),
        Some(McpLayerEntry::Tombstone)
    ));
    let reveal = UserConfigMutation::McpSet {
        target: UserConfigTarget::Base,
        name: "managed".parse().expect("name"),
        expected_user_entry: Some(McpLayerEntry::Tombstone),
        server: stdio("revealed"),
    };
    assert_eq!(
        store.mutate(reveal).expect("replace tombstone").publication,
        UserConfigPublication::PublishedDurably
    );
    let revealed = ConfigLayer::parse(Source::User, &std::fs::read_to_string(&path).expect("read"))
        .expect("strict revealed config")
        .value;
    assert!(matches!(
        revealed.mcp.servers.get(&"managed".parse().expect("name")),
        Some(McpLayerEntry::Server(McpServerConfig::Stdio(s))) if s.command == "revealed"
    ));
}

#[test]
fn stale_compare_and_set_is_unpublished_and_publication_uncertainty_is_durable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = fixture(&dir);
    let io = FakeIo::new();
    let store = OsUserConfigStore::new(&io, &path);
    let name: McpServerName = "managed".parse().expect("name");
    let stale = UserConfigMutation::McpDeleteUserEntry {
        target: UserConfigTarget::Base,
        name: name.clone(),
        expected_entry: McpLayerEntry::Server(stdio("never-present")),
    };
    assert!(store.mutate(stale).is_err());
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "# keep\nmodel = 'old'\n\n[mcp.servers.unrelated]\ntransport='stdio'\ncommand='keep'\n"
    );
    let before_stale_set = std::fs::read(&path).expect("read bytes");
    let stale_set = UserConfigMutation::McpSet {
        target: UserConfigTarget::Base,
        name: name.clone(),
        expected_user_entry: Some(McpLayerEntry::Tombstone),
        server: stdio("must-not-publish"),
    };
    assert!(store.mutate(stale_set).is_err());
    assert_eq!(std::fs::read(&path).expect("read bytes"), before_stale_set);
    let uncertain_io = FakeIo {
        uncertain: true,
        ..FakeIo::new()
    };
    let uncertain_store = OsUserConfigStore::new(&uncertain_io, &path);
    let set = UserConfigMutation::McpSet {
        target: UserConfigTarget::Profile(ProfileName::parse("dev").unwrap()),
        name,
        expected_user_entry: None,
        server: stdio("published"),
    };
    assert!(
        matches!(uncertain_store.mutate(set), Ok(UserConfigMutationResult { publication: UserConfigPublication::PublishedDurabilityUncertain { diagnostic }, .. }) if diagnostic == "uncertain")
    );
    let published = std::fs::read_to_string(&path).expect("read");
    assert!(published.contains("[profile.dev.mcp.servers.managed]"));
    assert!(!published.contains("[mcp.servers.managed]"));
}

#[test]
fn credentials_are_bound_to_canonical_key_and_delete_has_removed_absent() {
    let key = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::ProtectedResource("https://resource".into()),
        Some("acct".into()),
    )
    .expect("key");
    let other = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::Audience("https://resource".into()),
        Some("acct".into()),
    )
    .expect("key");
    let backend = RecordingBackend::default();
    let store = ego_tools::credentials::OsCredentialStore::new(backend);
    store
        .put(&key, SecretMaterial::new(b"opaque".to_vec()))
        .expect("put");
    assert_eq!(
        store
            .get(&key)
            .expect("get")
            .expect("present")
            .expose_secret_bytes(),
        b"opaque"
    );
    assert!(store.get(&other).expect("domain isolation").is_none());
    assert_eq!(
        store.delete(&key).expect("delete"),
        CredentialRemoval::Removed
    );
    assert_eq!(
        store.delete(&key).expect("absent"),
        CredentialRemoval::Absent
    );
}

#[test]
fn credential_envelope_corruption_fails_closed_without_secret_disclosure() {
    let key = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::ProtectedResource("https://resource".into()),
        None,
    )
    .expect("key");
    let backend = RecordingBackend::default();
    let store = ego_tools::credentials::OsCredentialStore::new(backend.clone());
    store
        .put(&key, SecretMaterial::new(b"opaque".to_vec()))
        .expect("put");
    backend.overwrite_existing_payloads(vec![0, 1, 2, 3, 4]);
    let error = store
        .get(&key)
        .expect_err("malformed envelope must fail closed");
    assert!(!format!("{error:?}").contains("opaque"));
}

#[test]
fn credential_full_key_mismatch_is_collision_and_redacted() {
    let first = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::ProtectedResource("https://resource-a".into()),
        None,
    )
    .expect("key");
    let second = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::ProtectedResource("https://resource-b".into()),
        None,
    )
    .expect("key");
    let backend = RecordingBackend::default();
    let store = ego_tools::credentials::OsCredentialStore::new(backend.clone());
    store
        .put(&first, SecretMaterial::new(b"collision-secret".to_vec()))
        .expect("put");
    backend.use_payload_for_next_key(backend.any_payload().expect("stored envelope"));
    let error = store
        .get(&second)
        .expect_err("full-key mismatch must fail closed as collision");
    let rendered = format!("{error:?} {error}");
    assert!(rendered.to_lowercase().contains("collision"));
    assert!(!rendered.contains("collision-secret"));
}

#[test]
fn credential_backend_failures_map_to_closed_categories_and_no_entry_is_operation_specific() {
    use ego_core::credentials::{CredentialStoreErrorKind, CredentialStoreOperation};
    use ego_tools::credentials::CredentialBackendError as B;

    let key = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::ProtectedResource("https://resource".into()),
        None,
    )
    .expect("key");
    let cases = [
        (B::Invalid, CredentialStoreErrorKind::Invalid),
        (B::TooLong, CredentialStoreErrorKind::Invalid),
        (B::Ambiguous, CredentialStoreErrorKind::Ambiguous),
        (B::BadEncoding, CredentialStoreErrorKind::Corrupt),
        (B::BadDataFormat, CredentialStoreErrorKind::Corrupt),
        (B::BadStoreFormat, CredentialStoreErrorKind::Corrupt),
        (
            B::NotSupportedByStore,
            CredentialStoreErrorKind::Unsupported,
        ),
        (B::PlatformFailure, CredentialStoreErrorKind::Unavailable),
        (B::NoStorageAccess, CredentialStoreErrorKind::Unavailable),
        (B::NoDefaultStore, CredentialStoreErrorKind::Unavailable),
    ];
    for (backend_error, expected_kind) in cases {
        let backend = RecordingBackend::default();
        backend.fail_next(backend_error);
        let error = ego_tools::credentials::OsCredentialStore::new(backend)
            .get(&key)
            .expect_err("backend category must fail closed");
        assert_eq!(error.operation(), CredentialStoreOperation::Get);
        assert_eq!(error.kind(), expected_kind);
        assert!(!format!("{error:?} {error}").contains("backend_error"));
    }

    let backend = RecordingBackend::default();
    backend.fail_next(B::NoEntry);
    assert!(
        ego_tools::credentials::OsCredentialStore::new(backend)
            .get(&key)
            .expect("get NoEntry is absence")
            .is_none()
    );

    let backend = RecordingBackend::default();
    backend.fail_next(B::NoEntry);
    assert_eq!(
        ego_tools::credentials::OsCredentialStore::new(backend)
            .delete(&key)
            .expect("delete NoEntry is absence"),
        CredentialRemoval::Absent
    );

    let backend = RecordingBackend::default();
    backend.fail_next(B::NoEntry);
    let error = ego_tools::credentials::OsCredentialStore::new(backend)
        .put(&key, SecretMaterial::new(b"opaque".to_vec()))
        .expect_err("put NoEntry is unavailable");
    assert_eq!(error.kind(), CredentialStoreErrorKind::Unavailable);
}

const CAPTURE_SERVICE: &str = "ego";
const CAPTURE_USER_PREFIX: &str = "mcp/generation-policy-capture/v1/";
const CAPTURE_KEY_DOMAIN: &[u8] = b"ego:generation-policy-capture-key:v1";
const CAPTURE_ENVELOPE_MAGIC: &[u8; 8] = b"EGO-GPC\0";
const CAPTURE_ENVELOPE_VERSION: u8 = 1;
const CAPTURE_CODEC_LIMIT: usize = 1024 * 1024;
const CAPTURE_ID: &str = "mcp-generation-policy-continuation-sentinel";
const OTHER_CAPTURE_ID: &str = "mcp-generation-policy-other-continuation";
const CAPTURE_WORKSPACE: &str = "/sentinel/generation-policy-capture-workspace";
const CAPTURE_RULE: &str = "read(/sentinel/generation-policy-capture-workspace/**)";
const CAPTURE_RAW_BYTES: &str = "sentinel-raw-generation-policy-bytes";

fn policy_snapshot(mode: Mode) -> Arc<GenerationPolicySnapshot> {
    GenerationPolicyCapture::new(
        CanonPath::new(CAPTURE_WORKSPACE),
        mode,
        Ruleset {
            allow: vec![SourcedRule::new(
                Rule::load(CAPTURE_RULE).expect("canonical capture rule"),
                Source::User,
            )],
            deny: Vec::new(),
        },
        None,
    )
    .expect("valid generation policy capture")
    .snapshot()
}

fn capture_key(id: &str) -> Vec<u8> {
    let mut key = Vec::new();
    push_capture_key_field(&mut key, 0, CAPTURE_KEY_DOMAIN);
    push_capture_key_field(&mut key, 1, id.as_bytes());
    key
}

fn push_capture_key_field(key: &mut Vec<u8>, tag: u8, value: &[u8]) {
    key.push(tag);
    key.extend_from_slice(&(value.len() as u64).to_be_bytes());
    key.extend_from_slice(value);
}

fn capture_user(id: &str) -> String {
    let digest = Sha256::digest(capture_key(id));
    let mut user = String::with_capacity(CAPTURE_USER_PREFIX.len() + digest.len() * 2);
    user.push_str(CAPTURE_USER_PREFIX);
    for byte in digest {
        use std::fmt::Write as _;
        write!(user, "{byte:02x}").expect("writing to String cannot fail");
    }
    user
}

fn capture_envelope(id: &str, policy: &[u8]) -> Vec<u8> {
    let key = capture_key(id);
    let key_len = u32::try_from(key.len()).expect("test key fits envelope");
    let policy_len = u32::try_from(policy.len()).expect("test policy fits envelope");
    let mut envelope = Vec::new();
    envelope.extend_from_slice(CAPTURE_ENVELOPE_MAGIC);
    envelope.push(CAPTURE_ENVELOPE_VERSION);
    envelope.extend_from_slice(&key_len.to_be_bytes());
    envelope.extend_from_slice(&key);
    envelope.extend_from_slice(&policy_len.to_be_bytes());
    envelope.extend_from_slice(policy);
    envelope
}

fn digest_text(digest: &GenerationPolicyDigest) -> String {
    serde_json::to_value(digest)
        .expect("digest serializes")
        .as_str()
        .expect("digest wire is a string")
        .to_owned()
}

fn assert_capture_store_error(
    error: &GenerationPolicyCaptureStoreError,
    operation: GenerationPolicyCaptureStoreOperation,
    kind: GenerationPolicyCaptureStoreErrorKind,
    extra_sentinels: &[&str],
) {
    assert_eq!(error.operation(), operation);
    assert_eq!(error.kind(), kind);
    for rendered in [
        format!("{error:?}"),
        error.to_string(),
        error.diagnostic().to_owned(),
    ] {
        for sentinel in [
            CAPTURE_ID,
            CAPTURE_WORKSPACE,
            CAPTURE_RULE,
            CAPTURE_RAW_BYTES,
        ]
        .into_iter()
        .chain(extra_sentinels.iter().copied())
        {
            assert!(
                !rendered.contains(sentinel),
                "capture-store error leaked confidential input: {rendered}"
            );
        }
        for backend_detail in ["TooLong", "BadEncoding", "BadDataFormat", "PlatformFailure"] {
            assert!(
                !rendered.contains(backend_detail),
                "capture-store error leaked backend detail: {rendered}"
            );
        }
    }
}

#[test]
fn generation_policy_capture_store_roundtrip_uses_one_domain_isolated_entry() {
    let backend = RecordingBackend::default();
    let store = OsGenerationPolicyCaptureStore::new(backend.clone());
    let snapshot = policy_snapshot(Mode::Auto);
    let digest = snapshot.digest();
    let canonical = snapshot.encode_confidential();
    let user = capture_user(CAPTURE_ID);

    store
        .store(CAPTURE_ID, Arc::clone(&snapshot))
        .expect("typed snapshot stores");
    let stored = backend
        .payload(CAPTURE_SERVICE, &user)
        .expect("one capture envelope is stored");
    assert_eq!(
        stored,
        capture_envelope(CAPTURE_ID, canonical.expose_secret_bytes())
    );
    assert!(stored.starts_with(CAPTURE_ENVELOPE_MAGIC));
    assert!(!stored.starts_with(b"EGO-MCP\0"));
    assert_eq!(user.len(), CAPTURE_USER_PREFIX.len() + 64);
    assert!(user.starts_with(CAPTURE_USER_PREFIX));
    assert!(
        user[CAPTURE_USER_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert!(!user.contains(CAPTURE_ID));

    let loaded: Arc<GenerationPolicySnapshot> = store
        .load(CAPTURE_ID, &digest)
        .expect("matching capture loads");
    assert_eq!(loaded.digest(), digest);
    assert_eq!(
        loaded.encode_confidential().expose_secret_bytes(),
        canonical.expose_secret_bytes()
    );

    store.delete(CAPTURE_ID).expect("present capture deletes");
    store
        .delete(CAPTURE_ID)
        .expect("absent capture deletion is idempotent");
    let missing = store
        .load(CAPTURE_ID, &digest)
        .expect_err("an absent capture is not a verified value");
    assert_capture_store_error(
        &missing,
        GenerationPolicyCaptureStoreOperation::Load,
        GenerationPolicyCaptureStoreErrorKind::Missing,
        &[],
    );
    assert_eq!(
        backend.calls(),
        vec![
            BackendCall::get(CAPTURE_SERVICE, &user),
            BackendCall::put(CAPTURE_SERVICE, &user),
            BackendCall::get(CAPTURE_SERVICE, &user),
            BackendCall::get(CAPTURE_SERVICE, &user),
            BackendCall::delete(CAPTURE_SERVICE, &user),
            BackendCall::get(CAPTURE_SERVICE, &user),
            BackendCall::get(CAPTURE_SERVICE, &user),
        ]
    );
}

#[test]
fn generation_policy_capture_store_refuses_wrong_digest_without_disclosure() {
    let backend = RecordingBackend::default();
    let store = OsGenerationPolicyCaptureStore::new(backend);
    let snapshot = policy_snapshot(Mode::Auto);
    let actual = snapshot.digest();
    let expected = policy_snapshot(Mode::Plan).digest();
    store
        .store(CAPTURE_ID, snapshot)
        .expect("typed snapshot stores");

    let error = store
        .load(CAPTURE_ID, &expected)
        .expect_err("a different expected digest must fail closed");
    let actual = digest_text(&actual);
    let expected = digest_text(&expected);
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Load,
        GenerationPolicyCaptureStoreErrorKind::DigestMismatch,
        &[actual.as_str(), expected.as_str()],
    );
}

#[test]
fn generation_policy_capture_store_refuses_every_corrupt_envelope_form() {
    let backend = RecordingBackend::default();
    let store = OsGenerationPolicyCaptureStore::new(backend.clone());
    let snapshot = policy_snapshot(Mode::Auto);
    let digest = snapshot.digest();
    store
        .store(CAPTURE_ID, snapshot)
        .expect("typed snapshot stores");
    let valid = backend.any_payload().expect("stored capture envelope");

    let malformed = vec![0, 1, 2, 3];
    let mut unknown_version = valid.clone();
    unknown_version[CAPTURE_ENVELOPE_MAGIC.len()] = 2;
    let truncated_length = valid[..CAPTURE_ENVELOPE_MAGIC.len() + 1 + 2].to_vec();
    let mut declared_key_overrun = valid.clone();
    let key_length = CAPTURE_ENVELOPE_MAGIC.len() + 1;
    declared_key_overrun[key_length..key_length + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut trailing = valid.clone();
    trailing.push(0);
    let invalid_policy = capture_envelope(CAPTURE_ID, CAPTURE_RAW_BYTES.as_bytes());
    let oversized_policy = capture_envelope(CAPTURE_ID, &vec![b'x'; CAPTURE_CODEC_LIMIT + 1]);
    for corrupt in [
        malformed,
        unknown_version,
        truncated_length,
        declared_key_overrun,
        trailing,
        invalid_policy,
        oversized_policy,
    ] {
        backend.overwrite_existing_payloads(corrupt);
        let error = store
            .load(CAPTURE_ID, &digest)
            .expect_err("corrupt capture bytes must fail closed");
        assert_capture_store_error(
            &error,
            GenerationPolicyCaptureStoreOperation::Load,
            GenerationPolicyCaptureStoreErrorKind::Corrupt,
            &[],
        );
    }

    backend.use_payload_for_next_key(valid);
    let error = store
        .load(OTHER_CAPTURE_ID, &digest)
        .expect_err("a colliding full capture key must fail closed");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Load,
        GenerationPolicyCaptureStoreErrorKind::Corrupt,
        &[OTHER_CAPTURE_ID],
    );
}

#[test]
fn generation_policy_capture_store_maps_backend_failures_to_closed_categories() {
    use ego_tools::credentials::CredentialBackendError as B;

    let snapshot = policy_snapshot(Mode::Auto);

    let backend = RecordingBackend::default();
    backend.fail_next_put(B::TooLong);
    let store = OsGenerationPolicyCaptureStore::new(backend.clone());
    let error = store
        .store(CAPTURE_ID, Arc::clone(&snapshot))
        .expect_err("native capacity failure must stay operation-specific");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Store,
        GenerationPolicyCaptureStoreErrorKind::CapacityExceeded,
        &[],
    );
    assert!(backend.any_payload().is_none());
    let user = capture_user(CAPTURE_ID);
    assert_eq!(
        backend.calls(),
        vec![
            BackendCall::get(CAPTURE_SERVICE, &user),
            BackendCall::put(CAPTURE_SERVICE, &user),
        ]
    );

    let backend = RecordingBackend::default();
    backend.fail_next(B::BadDataFormat);
    let error = OsGenerationPolicyCaptureStore::new(backend.clone())
        .store(CAPTURE_ID, Arc::clone(&snapshot))
        .expect_err("corrupt native preflight data must fail closed");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Store,
        GenerationPolicyCaptureStoreErrorKind::Corrupt,
        &[],
    );
    assert!(backend.any_payload().is_none());

    let backend = RecordingBackend::default();
    let store = OsGenerationPolicyCaptureStore::new(backend.clone());
    store
        .store(CAPTURE_ID, Arc::clone(&snapshot))
        .expect("typed snapshot stores");
    backend.fail_next(B::BadEncoding);
    let error = store
        .load(CAPTURE_ID, &snapshot.digest())
        .expect_err("corrupt native load must fail closed");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Load,
        GenerationPolicyCaptureStoreErrorKind::Corrupt,
        &[],
    );
    backend.fail_next_delete(B::PlatformFailure);
    let error = store
        .delete(CAPTURE_ID)
        .expect_err("unavailable native delete must fail closed");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Delete,
        GenerationPolicyCaptureStoreErrorKind::Unavailable,
        &[],
    );
    assert!(
        backend
            .payload(CAPTURE_SERVICE, &capture_user(CAPTURE_ID))
            .is_some()
    );
}

#[test]
fn generation_policy_capture_store_preflight_never_overwrites_corrupt_or_colliding_data() {
    let original = policy_snapshot(Mode::Auto);
    let replacement = policy_snapshot(Mode::Plan);
    let backend = RecordingBackend::default();
    let store = OsGenerationPolicyCaptureStore::new(backend.clone());
    store
        .store(CAPTURE_ID, Arc::clone(&original))
        .expect("original capture stores");
    let malformed = vec![0, 1, 2, 3, 4];
    backend.overwrite_existing_payloads(malformed.clone());
    backend.clear_calls();

    let error = store
        .store(CAPTURE_ID, Arc::clone(&replacement))
        .expect_err("corrupt preflight must prevent replacement");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Store,
        GenerationPolicyCaptureStoreErrorKind::Corrupt,
        &[],
    );
    let user = capture_user(CAPTURE_ID);
    assert_eq!(backend.payload(CAPTURE_SERVICE, &user), Some(malformed));
    assert_eq!(
        backend.calls(),
        vec![BackendCall::get(CAPTURE_SERVICE, &user)]
    );

    let first_envelope = capture_envelope(
        CAPTURE_ID,
        original.encode_confidential().expose_secret_bytes(),
    );
    backend.use_payload_for_next_key(first_envelope.clone());
    backend.clear_calls();
    let error = store
        .store(OTHER_CAPTURE_ID, replacement)
        .expect_err("full-key collision must prevent replacement");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Store,
        GenerationPolicyCaptureStoreErrorKind::Corrupt,
        &[OTHER_CAPTURE_ID],
    );
    let other_user = capture_user(OTHER_CAPTURE_ID);
    assert_eq!(
        backend.payload(CAPTURE_SERVICE, &other_user),
        Some(first_envelope)
    );
    assert_eq!(
        backend.calls(),
        vec![BackendCall::get(CAPTURE_SERVICE, &other_user)]
    );
}

#[test]
fn generation_policy_capture_store_delete_refuses_corrupt_and_colliding_payloads() {
    let snapshot = policy_snapshot(Mode::Auto);
    let backend = RecordingBackend::default();
    let store = OsGenerationPolicyCaptureStore::new(backend.clone());
    store
        .store(CAPTURE_ID, Arc::clone(&snapshot))
        .expect("original capture stores");
    let mut corrupt = backend.any_payload().expect("stored capture envelope");
    corrupt.push(0);
    backend.overwrite_existing_payloads(corrupt.clone());
    backend.clear_calls();

    let error = store
        .delete(CAPTURE_ID)
        .expect_err("corrupt capture must not be deleted");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Delete,
        GenerationPolicyCaptureStoreErrorKind::Corrupt,
        &[],
    );
    let user = capture_user(CAPTURE_ID);
    assert_eq!(backend.payload(CAPTURE_SERVICE, &user), Some(corrupt));
    assert_eq!(
        backend.calls(),
        vec![BackendCall::get(CAPTURE_SERVICE, &user)]
    );

    let backend = RecordingBackend::default();
    let store = OsGenerationPolicyCaptureStore::new(backend.clone());
    store
        .store(CAPTURE_ID, snapshot)
        .expect("original capture stores");
    let other_key_payload = backend.any_payload().expect("stored capture envelope");
    backend.use_payload_for_next_key(other_key_payload.clone());
    backend.clear_calls();

    let error = store
        .delete(OTHER_CAPTURE_ID)
        .expect_err("capture bound to another key must not be deleted");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Delete,
        GenerationPolicyCaptureStoreErrorKind::Corrupt,
        &[OTHER_CAPTURE_ID],
    );
    let other_user = capture_user(OTHER_CAPTURE_ID);
    assert_eq!(
        backend.payload(CAPTURE_SERVICE, &other_user),
        Some(other_key_payload)
    );
    assert_eq!(
        backend.calls(),
        vec![BackendCall::get(CAPTURE_SERVICE, &other_user)]
    );
}

#[test]
fn generation_policy_capture_store_maps_backend_no_entry_by_operation() {
    use ego_tools::credentials::CredentialBackendError as B;

    let backend = RecordingBackend::default();
    let store = OsGenerationPolicyCaptureStore::new(backend.clone());
    let digest = policy_snapshot(Mode::Auto).digest();
    backend.fail_next(B::NoEntry);
    let error = store
        .load(CAPTURE_ID, &digest)
        .expect_err("backend absence during load is missing");
    assert_capture_store_error(
        &error,
        GenerationPolicyCaptureStoreOperation::Load,
        GenerationPolicyCaptureStoreErrorKind::Missing,
        &[],
    );

    backend.fail_next(B::NoEntry);
    store
        .delete(CAPTURE_ID)
        .expect("backend absence during delete is idempotent");
    let user = capture_user(CAPTURE_ID);
    assert_eq!(
        backend.calls(),
        vec![
            BackendCall::get(CAPTURE_SERVICE, &user),
            BackendCall::get(CAPTURE_SERVICE, &user),
        ]
    );
}

#[derive(Default, Clone)]
struct RecordingBackend(Arc<Mutex<BackendState>>);

#[derive(Default)]
struct BackendState {
    entries: HashMap<(String, String), Vec<u8>>,
    collision_payload: Option<Vec<u8>>,
    next_error: Option<ego_tools::credentials::CredentialBackendError>,
    next_put_error: Option<ego_tools::credentials::CredentialBackendError>,
    next_delete_error: Option<ego_tools::credentials::CredentialBackendError>,
    calls: Vec<BackendCall>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BackendCall {
    Get { service: String, user: String },
    Put { service: String, user: String },
    Delete { service: String, user: String },
}

impl BackendCall {
    fn get(service: &str, user: &str) -> Self {
        Self::Get {
            service: service.to_owned(),
            user: user.to_owned(),
        }
    }

    fn put(service: &str, user: &str) -> Self {
        Self::Put {
            service: service.to_owned(),
            user: user.to_owned(),
        }
    }

    fn delete(service: &str, user: &str) -> Self {
        Self::Delete {
            service: service.to_owned(),
            user: user.to_owned(),
        }
    }
}

impl ego_tools::credentials::CredentialBackend for RecordingBackend {
    fn get(
        &self,
        service: &str,
        user: &str,
    ) -> Result<Option<Vec<u8>>, ego_tools::credentials::CredentialBackendError> {
        let mut state = self.0.lock().expect("backend lock");
        state.calls.push(BackendCall::get(service, user));
        if let Some(error) = state.next_error.take() {
            return Err(error);
        }
        if !state
            .entries
            .contains_key(&(service.to_owned(), user.to_owned()))
            && let Some(payload) = state.collision_payload.take()
        {
            state
                .entries
                .insert((service.to_owned(), user.to_owned()), payload);
        }
        Ok(state
            .entries
            .get(&(service.to_owned(), user.to_owned()))
            .cloned())
    }
    fn put(
        &self,
        service: &str,
        user: &str,
        value: Vec<u8>,
    ) -> Result<(), ego_tools::credentials::CredentialBackendError> {
        let mut state = self.0.lock().expect("backend lock");
        state.calls.push(BackendCall::put(service, user));
        if let Some(error) = state.next_put_error.take() {
            return Err(error);
        }
        if let Some(error) = state.next_error.take() {
            return Err(error);
        }
        state
            .entries
            .insert((service.to_owned(), user.to_owned()), value);
        Ok(())
    }
    fn delete(
        &self,
        service: &str,
        user: &str,
    ) -> Result<bool, ego_tools::credentials::CredentialBackendError> {
        let mut state = self.0.lock().expect("backend lock");
        state.calls.push(BackendCall::delete(service, user));
        if let Some(error) = state.next_delete_error.take() {
            return Err(error);
        }
        if let Some(error) = state.next_error.take() {
            return Err(error);
        }
        Ok(state
            .entries
            .remove(&(service.to_owned(), user.to_owned()))
            .is_some())
    }
}

impl RecordingBackend {
    fn fail_next(&self, error: ego_tools::credentials::CredentialBackendError) {
        self.0.lock().expect("backend lock").next_error = Some(error);
    }

    fn fail_next_put(&self, error: ego_tools::credentials::CredentialBackendError) {
        self.0.lock().expect("backend lock").next_put_error = Some(error);
    }

    fn fail_next_delete(&self, error: ego_tools::credentials::CredentialBackendError) {
        self.0.lock().expect("backend lock").next_delete_error = Some(error);
    }

    fn overwrite_existing_payloads(&self, payload: Vec<u8>) {
        for value in self.0.lock().expect("backend lock").entries.values_mut() {
            *value = payload.clone();
        }
    }

    fn use_payload_for_next_key(&self, payload: Vec<u8>) {
        self.0.lock().expect("backend lock").collision_payload = Some(payload);
    }

    fn any_payload(&self) -> Option<Vec<u8>> {
        self.0
            .lock()
            .expect("backend lock")
            .entries
            .values()
            .next()
            .cloned()
    }

    fn payload(&self, service: &str, user: &str) -> Option<Vec<u8>> {
        self.0
            .lock()
            .expect("backend lock")
            .entries
            .get(&(service.to_owned(), user.to_owned()))
            .cloned()
    }

    fn calls(&self) -> Vec<BackendCall> {
        self.0.lock().expect("backend lock").calls.clone()
    }

    fn clear_calls(&self) {
        self.0.lock().expect("backend lock").calls.clear();
    }
}
