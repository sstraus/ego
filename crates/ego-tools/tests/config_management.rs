//! regression case contract for typed, atomic managed configuration mutations.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use ego_core::config::{
    ConfigLayer, ManagedConfigKey, ManagedConfigValue, ProfileName, Source, UserConfigMutation,
    UserConfigMutationFacts, UserConfigMutationResult, UserConfigPublication, UserConfigStore,
    UserConfigTarget,
};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::{AtomicReplacePublication, OsIo, StoreIo};

#[derive(Default)]
struct FaultIo {
    inner: OsIo,
    replace_calls: std::sync::atomic::AtomicUsize,
    fail_before_publish: std::sync::atomic::AtomicBool,
    uncertain_after_publish: std::sync::atomic::AtomicBool,
}

impl FaultIo {
    fn calls(&self) -> usize {
        self.replace_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl StoreIo for FaultIo {
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
        self.inner.replace_atomic(path, bytes)
    }
    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        self.replace_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .fail_before_publish
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(io::Error::other("injected pre-publication failure"));
        }
        self.inner.replace_atomic(path, bytes)?;
        if self
            .uncertain_after_publish
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
                diagnostic: "injected post-publication uncertainty".into(),
            });
        }
        Ok(AtomicReplacePublication::PublishedDurably)
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

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "# keep\nmodel = 'old/model'\nthinking = 'low'\n[profile.dev]\nmodel = 'dev/model'\n[profile.other]\n# retain this valid field\nmode = 'edits'\n",
    )
    .unwrap();
    ConfigLayer::parse(Source::User, &std::fs::read_to_string(path).unwrap())
        .expect("initial fixture is strict configuration");
    dir
}

fn adapter(path: &Path, io: &Arc<FaultIo>) -> OsUserConfigStore<Arc<FaultIo>> {
    OsUserConfigStore::new(Arc::clone(io), path)
}

fn set(target: UserConfigTarget, key: ManagedConfigKey, raw: &str) -> UserConfigMutation {
    UserConfigMutation::ManagedSet {
        target,
        key,
        value: ManagedConfigValue::parse(key, raw).unwrap(),
    }
}

#[test]
fn base_profile_and_ci_managed_set_unset_are_typed_and_strictly_reparseable() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    let io = Arc::new(FaultIo::default());
    let store = adapter(&path, &io);
    let dev = UserConfigTarget::Profile(ProfileName::parse("dev").unwrap());
    let ci = UserConfigTarget::Profile(ProfileName::parse("ci").unwrap());
    let result = store.mutate(set(
        UserConfigTarget::Base,
        ManagedConfigKey::Model,
        "'new/model'",
    ));
    let result = result.unwrap();
    assert_eq!(result.target, UserConfigTarget::Base);
    assert_eq!(
        result.facts,
        UserConfigMutationFacts::Set {
            key: ManagedConfigKey::Model,
            value: ManagedConfigValue::parse(ManagedConfigKey::Model, "'new/model'").unwrap(),
        }
    );
    assert!(result.changed);
    assert_eq!(result.publication, UserConfigPublication::PublishedDurably);
    let result = store
        .mutate(set(dev.clone(), ManagedConfigKey::Thinking, "'high'"))
        .unwrap();
    assert_eq!(result.target, dev.clone());
    assert_eq!(
        result.facts,
        UserConfigMutationFacts::Set {
            key: ManagedConfigKey::Thinking,
            value: ManagedConfigValue::parse(ManagedConfigKey::Thinking, "'high'").unwrap(),
        }
    );
    assert!(result.changed);
    assert_eq!(result.publication, UserConfigPublication::PublishedDurably);
    let result = store
        .mutate(set(ci, ManagedConfigKey::SessionDir, "'/tmp/session'"))
        .unwrap();
    assert_eq!(
        result.facts,
        UserConfigMutationFacts::Set {
            key: ManagedConfigKey::SessionDir,
            value: ManagedConfigValue::parse(ManagedConfigKey::SessionDir, "'/tmp/session'")
                .unwrap(),
        }
    );
    assert!(result.changed);
    assert_eq!(result.publication, UserConfigPublication::PublishedDurably);
    let result = store
        .mutate(UserConfigMutation::ManagedUnset {
            target: dev.clone(),
            key: ManagedConfigKey::Thinking,
        })
        .unwrap();
    assert_eq!(result.target, dev);
    assert_eq!(
        result.facts,
        UserConfigMutationFacts::Unset {
            key: ManagedConfigKey::Thinking
        }
    );
    assert!(result.changed);
    assert_eq!(result.publication, UserConfigPublication::PublishedDurably);
    let text = std::fs::read_to_string(&path).unwrap();
    let parsed = ConfigLayer::parse(Source::User, &text).unwrap().value;
    assert_eq!(parsed.model.unwrap().0, "new/model");
    assert_eq!(
        parsed.thinking,
        Some(ego_core::generator::types::ThinkingLevel::Low)
    );
    assert_eq!(parsed.profile.get("dev").unwrap().thinking, None);
    assert_eq!(
        parsed.profile.get("ci").unwrap().session_dir,
        Some(std::path::PathBuf::from("/tmp/session"))
    );
}

#[test]
fn nested_insertion_pruning_preserves_unrelated_profile_content_and_comments() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    let io = Arc::new(FaultIo::default());
    let store = adapter(&path, &io);
    let profile = UserConfigTarget::Profile(ProfileName::parse("other").unwrap());
    store
        .mutate(set(
            profile.clone(),
            ManagedConfigKey::PermissionsJudge,
            "'judge/model'",
        ))
        .unwrap();
    let after_set = std::fs::read_to_string(&path).unwrap();
    let parsed = ConfigLayer::parse(Source::User, &after_set).unwrap().value;
    assert_eq!(
        parsed
            .profile
            .get("other")
            .unwrap()
            .permissions
            .as_ref()
            .unwrap()
            .judge
            .as_ref()
            .unwrap()
            .0,
        "judge/model"
    );
    assert_eq!(
        parsed
            .profile
            .get("other")
            .unwrap()
            .mode
            .map(|mode| mode.as_str()),
        Some("edits")
    );
    assert!(after_set.contains("# retain this valid field"));
    store
        .mutate(UserConfigMutation::ManagedUnset {
            target: profile,
            key: ManagedConfigKey::PermissionsJudge,
        })
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let parsed = ConfigLayer::parse(Source::User, &text).unwrap().value;
    assert!(parsed.profile.get("other").unwrap().permissions.is_none());
    assert_eq!(
        parsed
            .profile
            .get("other")
            .unwrap()
            .mode
            .map(|mode| mode.as_str()),
        Some("edits")
    );
    assert!(text.contains("# retain this valid field"));
}

#[test]
fn invalid_target_key_and_value_are_refused_before_replace() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    let io = Arc::new(FaultIo::default());
    let store = adapter(&path, &io);
    assert!(ManagedConfigValue::parse(ManagedConfigKey::Budget, "'not-a-budget'").is_err());
    assert!(
        ManagedConfigValue::parse(ManagedConfigKey::StatuslineSegments, "['model', 'model']")
            .is_err()
    );
    let valid =
        ManagedConfigValue::parse(ManagedConfigKey::StatuslineSegments, "['model']").unwrap();
    assert!(
        store
            .mutate(UserConfigMutation::ManagedSet {
                target: UserConfigTarget::Profile(ProfileName::parse("dev").unwrap()),
                key: ManagedConfigKey::StatuslineSegments,
                value: valid,
            })
            .is_err()
    );
    assert_eq!(io.calls(), 0);
}

#[test]
fn no_op_unset_publishes_nothing_and_calls_replace_zero_times() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    let io = Arc::new(FaultIo::default());
    let store = adapter(&path, &io);
    let result = store.mutate(UserConfigMutation::ManagedUnset {
        target: UserConfigTarget::Profile(ProfileName::parse("missing").unwrap()),
        key: ManagedConfigKey::Model,
    });
    assert!(matches!(
        result,
        Ok(UserConfigMutationResult {
            changed: false,
            publication: UserConfigPublication::NotPublished,
            ..
        })
    ));
    assert_eq!(io.calls(), 0);
}

#[test]
fn publication_cuts_preserve_old_authority_or_report_new_authority_without_retry() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    let io = Arc::new(FaultIo::default());
    let store = adapter(&path, &io);
    let before = std::fs::read(&path).unwrap();
    io.fail_before_publish
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        store
            .mutate(set(
                UserConfigTarget::Base,
                ManagedConfigKey::Model,
                "'failed/model'"
            ))
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    io.fail_before_publish
        .store(false, std::sync::atomic::Ordering::SeqCst);
    io.uncertain_after_publish
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let result = store
        .mutate(set(
            UserConfigTarget::Base,
            ManagedConfigKey::Model,
            "'uncertain/model'",
        ))
        .unwrap();
    assert!(matches!(
        result,
        UserConfigMutationResult {
            changed: true,
            publication: UserConfigPublication::PublishedDurabilityUncertain { .. },
            ..
        }
    ));
    assert_eq!(io.calls(), 2);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("uncertain/model")
    );
}

#[test]
fn concurrent_distinct_managed_writes_serialize_without_lost_updates() {
    let dir = fixture();
    let path = dir.path().join("config.toml");
    let io = Arc::new(FaultIo::default());
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for (key, value) in [
        (ManagedConfigKey::Mode, "'edits'"),
        (ManagedConfigKey::Bare, "true"),
    ] {
        let path = path.clone();
        let io = Arc::clone(&io);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            adapter(&path, &io)
                .mutate(set(UserConfigTarget::Base, key, value))
                .unwrap();
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let parsed = ConfigLayer::parse(Source::User, &text).unwrap().value;
    assert_eq!(parsed.mode.map(|mode| mode.as_str()), Some("edits"));
    assert_eq!(parsed.bare, Some(true));
}

// Catches: concurrent roots add operations lose one edit through snapshot-then-set.
#[test]
fn concurrent_roots_add_preserves_both_atomic_edits() {
    use ego_core::permit::RootAccess;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let io = Arc::new(FaultIo::default());
    let barrier = Arc::new(Barrier::new(2));
    let workers = ["first", "second"].map(|name| {
        let path = path.clone();
        let io = io.clone();
        let barrier = barrier.clone();
        let root = directory.path().join(name);
        thread::spawn(move || {
            barrier.wait();
            adapter(&path, &io)
                .mutate(UserConfigMutation::Root {
                    target: UserConfigTarget::Base,
                    path: root,
                    access: Some(RootAccess::Read),
                })
                .unwrap();
        })
    });
    for worker in workers {
        worker.join().unwrap();
    }
    let snapshot = adapter(&path, &io)
        .snapshot(UserConfigTarget::Base)
        .unwrap();
    let roots = snapshot
        .values
        .into_iter()
        .find_map(|(key, value)| (key == ManagedConfigKey::Roots).then_some(value).flatten())
        .unwrap();
    let ManagedConfigValue::Roots(roots) = roots else {
        panic!("roots projection missing");
    };
    assert_eq!(roots.len(), 2);
    assert!(roots.iter().any(|root| root.path.ends_with("first")));
    assert!(roots.iter().any(|root| root.path.ends_with("second")));
}
