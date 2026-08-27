//! RED contract for regression case user-config publication and OS credentials.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ego_core::config::{
    ConfigLayer, McpLayerEntry, McpServerConfig, McpServerName, Source, UserConfigMutation,
    UserConfigPublication, UserConfigStore, UserMcpSlot,
};
use ego_core::credentials::{
    CredentialKey, CredentialRemoval, CredentialStore, CredentialTarget, SecretMaterial,
};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::{AtomicReplacePublication, OsIo, StoreIo};

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
        slot: UserMcpSlot::Base,
        name: name.clone(),
        expected_user_entry: None,
        server: stdio("first"),
    };
    assert_eq!(
        store.mutate(set).expect("set"),
        UserConfigPublication::PublishedDurably
    );
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(text.starts_with("# keep\nmodel = 'old'\n"));
    assert!(text.contains("[mcp.servers.unrelated]"));
    assert!(text.contains("command='keep'"));
    let delete = UserConfigMutation::McpDeleteUserEntry {
        slot: UserMcpSlot::Base,
        name: name.clone(),
        expected_entry: McpLayerEntry::Server(stdio("first")),
    };
    assert_eq!(
        store.mutate(delete).expect("delete"),
        UserConfigPublication::PublishedDurably
    );
    let insert = UserConfigMutation::McpInsertUserTombstone {
        name,
        expected_user_entry: None,
    };
    assert_eq!(
        store.mutate(insert).expect("tombstone"),
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
        slot: UserMcpSlot::Base,
        name: "managed".parse().expect("name"),
        expected_user_entry: Some(McpLayerEntry::Tombstone),
        server: stdio("revealed"),
    };
    assert_eq!(
        store.mutate(reveal).expect("replace tombstone"),
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
        slot: UserMcpSlot::Base,
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
        slot: UserMcpSlot::Base,
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
        slot: UserMcpSlot::Profile("dev".into()),
        name,
        expected_user_entry: None,
        server: stdio("published"),
    };
    assert!(
        matches!(uncertain_store.mutate(set), Ok(UserConfigPublication::PublishedDurabilityUncertain { diagnostic }) if diagnostic == "uncertain")
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

#[derive(Default, Clone)]
struct RecordingBackend(Arc<Mutex<BackendState>>);

#[derive(Default)]
struct BackendState {
    entries: HashMap<(String, String), Vec<u8>>,
    collision_payload: Option<Vec<u8>>,
    next_error: Option<ego_tools::credentials::CredentialBackendError>,
}

impl ego_tools::credentials::CredentialBackend for RecordingBackend {
    fn get(
        &self,
        service: &str,
        user: &str,
    ) -> Result<Option<Vec<u8>>, ego_tools::credentials::CredentialBackendError> {
        let mut state = self.0.lock().expect("backend lock");
        if let Some(error) = state.next_error.take() {
            return Err(error);
        }
        if !state
            .entries
            .contains_key(&(service.to_owned(), user.to_owned()))
        {
            if let Some(payload) = state.collision_payload.take() {
                state
                    .entries
                    .insert((service.to_owned(), user.to_owned()), payload);
            }
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
}
