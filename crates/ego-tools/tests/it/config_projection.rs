//! contract for reading stored user authority through the OS adapter.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ego_core::config::{
    ConfigLayer, ManagedConfigKey, ManagedConfigValue, Mode, ModelRef, ProfileName, Source,
    UserConfigSnapshot, UserConfigStore, UserConfigTarget,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::{AtomicReplacePublication, OsIo, StoreIo};

#[derive(Default)]
struct ReadIo {
    inner: OsIo,
    reads: AtomicUsize,
    writes: AtomicUsize,
    fail_read: AtomicBool,
}

impl StoreIo for ReadIo {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_read.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected read failure"));
        }
        self.inner.read(path)
    }
    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.replace_atomic_publication(path, bytes)
    }
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.inner.fsync(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.exists(path)
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace_atomic(path, bytes)
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

fn adapter(path: &Path, io: &Arc<ReadIo>) -> OsUserConfigStore<Arc<ReadIo>> {
    OsUserConfigStore::new(Arc::clone(io), path)
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text = "model = 'provider/raw'\nthinking = 'low'\nmode = 'edits'\nsandbox = 'workspace'\nbudget = '5$'\nbare = false\n[permissions]\njudge = 'judge/raw'\n[ui.statusline]\ntemplate = '{{ prompt }}'\n[profile.dev]\nmodel = 'dev/raw'\n[profile.ci]\nsession_dir = '/sessions'\nartifact_dir = '/artifacts'\n";
    std::fs::write(&path, text).unwrap();
    ConfigLayer::parse(Source::User, text).expect("fixture is strict");
    dir
}

#[test]
fn real_adapter_reads_exact_stored_values_in_canonical_order_without_writes() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    let io = Arc::new(ReadIo::default());
    let store = adapter(&path, &io);
    let snapshot: UserConfigSnapshot = store.snapshot(UserConfigTarget::Base).unwrap();
    assert_eq!(snapshot.target, UserConfigTarget::Base);
    assert_eq!(
        snapshot.values,
        vec![
            (
                ManagedConfigKey::Model,
                Some(ManagedConfigValue::Model(ModelRef("provider/raw".into())))
            ),
            (
                ManagedConfigKey::Thinking,
                Some(ManagedConfigValue::Thinking(ThinkingLevel::Low))
            ),
            (
                ManagedConfigKey::Mode,
                Some(ManagedConfigValue::Mode(Mode::Edits))
            ),
            (
                ManagedConfigKey::Sandbox,
                Some(ManagedConfigValue::Sandbox(Confinement::Workspace))
            ),
            (
                ManagedConfigKey::Budget,
                Some(ManagedConfigValue::Budget("5$".parse().unwrap()))
            ),
            (
                ManagedConfigKey::Bare,
                Some(ManagedConfigValue::Bare(false))
            ),
            (
                ManagedConfigKey::PermissionsJudge,
                Some(ManagedConfigValue::PermissionsJudge(ModelRef(
                    "judge/raw".into()
                )))
            ),
            (
                ManagedConfigKey::StatuslineTemplate,
                Some(ManagedConfigValue::StatuslineTemplate(
                    "{{ prompt }}".into()
                ))
            ),
            (ManagedConfigKey::ExecEnv, None),
            (ManagedConfigKey::ExecEnvAllowlist, None),
            (ManagedConfigKey::Network, None),
            (ManagedConfigKey::WebPrivateNetwork, None),
            (ManagedConfigKey::Roots, None),
            (ManagedConfigKey::ExecPaths, None),
            (ManagedConfigKey::Keybindings, None),
            (ManagedConfigKey::Vim, None),
            (ManagedConfigKey::CompactionIdle, None),
            (ManagedConfigKey::CompactionIdleMarginSeconds, None),
            (ManagedConfigKey::CompactionIdleThreshold, None),
            (ManagedConfigKey::CompactionHeadless, None),
            (ManagedConfigKey::DoctorNetwork, None),
            (ManagedConfigKey::RecapEnabled, None),
            (ManagedConfigKey::RecapAfterMinutes, None),
            (ManagedConfigKey::McpTasks, None),
            (ManagedConfigKey::Memory, None),
            (ManagedConfigKey::Checkpoints, None),
            (ManagedConfigKey::Tools, None),
            (ManagedConfigKey::SessionsCompress, None),
            (ManagedConfigKey::Cache, None),
            (ManagedConfigKey::InstructionsMaxFiles, None),
            (ManagedConfigKey::Lsp, None),
        ]
    );
    assert_eq!(io.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn profile_and_ci_are_exact_authorities_and_missing_profile_is_empty() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    let io = Arc::new(ReadIo::default());
    let store = adapter(&path, &io);
    let dev = store
        .snapshot(UserConfigTarget::Profile(
            ProfileName::parse("dev").unwrap(),
        ))
        .unwrap();
    assert_eq!(
        dev.values,
        vec![
            (
                ManagedConfigKey::Model,
                Some(ManagedConfigValue::Model(ModelRef("dev/raw".into())))
            ),
            (ManagedConfigKey::Thinking, None),
            (ManagedConfigKey::Mode, None),
            (ManagedConfigKey::Sandbox, None),
            (ManagedConfigKey::Budget, None),
            (ManagedConfigKey::PermissionsJudge, None),
            (ManagedConfigKey::ExecEnv, None),
            (ManagedConfigKey::ExecEnvAllowlist, None),
            (ManagedConfigKey::Network, None),
            (ManagedConfigKey::Roots, None),
            (ManagedConfigKey::ExecPaths, None),
        ]
    );
    let ci = store
        .snapshot(UserConfigTarget::Profile(ProfileName::parse("ci").unwrap()))
        .unwrap();
    assert_eq!(
        ci.values,
        vec![
            (ManagedConfigKey::Model, None),
            (ManagedConfigKey::Thinking, None),
            (ManagedConfigKey::Mode, None),
            (ManagedConfigKey::Sandbox, None),
            (ManagedConfigKey::Budget, None),
            (ManagedConfigKey::PermissionsJudge, None),
            (
                ManagedConfigKey::SessionDir,
                Some(ManagedConfigValue::Path(PathBuf::from("/sessions")))
            ),
            (
                ManagedConfigKey::ArtifactDir,
                Some(ManagedConfigValue::Path(PathBuf::from("/artifacts")))
            ),
            (ManagedConfigKey::ExecEnv, None),
            (ManagedConfigKey::ExecEnvAllowlist, None),
            (ManagedConfigKey::Network, None),
            (ManagedConfigKey::Roots, None),
            (ManagedConfigKey::ExecPaths, None),
        ]
    );
    let missing = store
        .snapshot(UserConfigTarget::Profile(
            ProfileName::parse("missing").unwrap(),
        ))
        .unwrap();
    assert_eq!(
        missing.target,
        UserConfigTarget::Profile(ProfileName::parse("missing").unwrap())
    );
    assert!(missing.values.iter().all(|(_, value)| value.is_none()));
}

#[test]
fn malformed_or_unreadable_authority_fails_closed_and_never_publishes() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "model = [not valid toml").unwrap();
    let io = Arc::new(ReadIo::default());
    let store = adapter(&path, &io);
    assert!(store.snapshot(UserConfigTarget::Base).is_err());
    assert_eq!(io.writes.load(Ordering::SeqCst), 0);
    io.fail_read.store(true, Ordering::SeqCst);
    assert!(store.snapshot(UserConfigTarget::Base).is_err());
    assert_eq!(io.writes.load(Ordering::SeqCst), 0);
}
