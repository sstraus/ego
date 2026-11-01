//! contract for one locked, comment-preserving user-config transaction.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use ego_core::config::{
    ConfigLayer, Source, UserConfigMutation, UserConfigPublication, UserConfigStore,
};
use ego_core::generator::types::ThinkingLevel;
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::{AtomicReplacePublication, OsIo, SNAPSHOT_VERSION, StoreIo};
use tempfile::TempDir;

struct FaultIo {
    inner: OsIo,
    fail_before_publish: Arc<std::sync::atomic::AtomicBool>,
    uncertain_after_publish: Arc<std::sync::atomic::AtomicBool>,
}

impl FaultIo {
    fn new() -> Self {
        Self {
            inner: OsIo,
            fail_before_publish: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            uncertain_after_publish: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
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
        if self
            .fail_before_publish
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(io::Error::other("injected pre-publish failure"));
        }
        self.inner.replace_atomic(path, bytes)?;
        if self
            .uncertain_after_publish
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
                diagnostic: "injected post-publish durability uncertainty".into(),
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

fn config_file(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "# keep this comment\nmodel = 'openai/old'\n\nthinking = 'medium'\nbare = true\n",
    )
    .unwrap();
    path
}

#[test]
fn transaction_locks_read_edit_strict_reparse_replace_and_unlocks_after_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file(&dir);
    let io = FaultIo::new();
    let store = OsUserConfigStore::new(&io, &path);

    assert_eq!(
        store
            .mutate(UserConfigMutation::ModelAndThinking {
                model: "openai/new".into(),
                thinking: ThinkingLevel::High,
            })
            .unwrap()
            .publication,
        UserConfigPublication::PublishedDurably
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep this comment"));
    assert!(text.contains("model = 'openai/new'"));
    assert!(text.contains("thinking = 'high'"));
    assert!(text.contains("bare = true"));
    ConfigLayer::parse(Source::User, &text).expect("published bytes are strict config");
    assert!(!path.with_extension("toml.tmp").exists());
}

#[test]
fn statusline_mutation_preserves_template_and_prompt_only_disables_optional_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file(&dir);
    let io = FaultIo::new();
    let store = OsUserConfigStore::new(&io, &path);

    store
        .mutate(UserConfigMutation::Statusline {
            template: "{{ model }}\n{{ prompt }}".into(),
        })
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("template ="));
    store
        .mutate(UserConfigMutation::Statusline {
            template: "{{ prompt }}".into(),
        })
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("{{ prompt }}"));
}

#[test]
fn template_mutation_preserves_jinja_source_without_interpreting_it_as_toml() {
    // Catches quoting or scalar type corruption at the config publication boundary.
    let dir = tempfile::tempdir().unwrap();
    let path = config_file(&dir);
    let io = FaultIo::new();
    let store = OsUserConfigStore::new(&io, &path);
    let template = "{% if trust == 'untrusted' %}{{ trust }}{% endif %}\n{{ prompt }}";
    store
        .mutate(UserConfigMutation::Statusline {
            template: template.into(),
        })
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let parsed = ConfigLayer::parse(Source::User, &text).unwrap();
    assert_eq!(
        parsed
            .value
            .ui
            .unwrap()
            .statusline
            .unwrap()
            .template
            .as_deref(),
        Some(template)
    );
}

#[test]
fn replace_failure_preserves_disk_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file(&dir);
    let io = FaultIo::new();
    let store = OsUserConfigStore::new(&io, &path);
    let before = std::fs::read_to_string(&path).unwrap();
    io.fail_before_publish
        .store(true, std::sync::atomic::Ordering::SeqCst);

    assert!(
        store
            .mutate(UserConfigMutation::ModelAndThinking {
                model: "openai/new".into(),
                thinking: ThinkingLevel::High,
            })
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn post_publish_barrier_failure_reports_uncertainty_and_keeps_new_disk_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file(&dir);
    let io = FaultIo::new();
    let store = OsUserConfigStore::new(&io, &path);
    io.uncertain_after_publish
        .store(true, std::sync::atomic::Ordering::SeqCst);

    assert_eq!(
        store
            .mutate(UserConfigMutation::ModelAndThinking {
                model: "openai/new".into(),
                thinking: ThinkingLevel::High,
            })
            .unwrap()
            .publication,
        UserConfigPublication::PublishedDurabilityUncertain {
            diagnostic: "injected post-publish durability uncertainty".into(),
        }
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("model = 'openai/new'"));
    assert!(text.contains("thinking = 'high'"));
}

#[test]
fn two_writers_serialize_and_each_reads_after_acquiring_the_lock_without_lost_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file(&dir);
    let ready = Arc::new(Barrier::new(2));
    let path_a = path.clone();
    let ready_a = Arc::clone(&ready);
    let first = thread::spawn(move || {
        let io = OsIo;
        let store = OsUserConfigStore::new(&io, &path_a);
        ready_a.wait();
        store
            .mutate(UserConfigMutation::Statusline {
                template: "{{ model }}\n{{ prompt }}".into(),
            })
            .unwrap();
    });
    let path_b = path.clone();
    let ready_b = Arc::clone(&ready);
    let second = thread::spawn(move || {
        let io = OsIo;
        let store = OsUserConfigStore::new(&io, &path_b);
        ready_b.wait();
        store
            .mutate(UserConfigMutation::ModelAndThinking {
                model: "openai/new".into(),
                thinking: ThinkingLevel::High,
            })
            .unwrap();
    });
    first.join().unwrap();
    second.join().unwrap();

    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("model = 'openai/new'"));
    assert!(text.contains("thinking = 'high'"));
    assert!(text.contains("{{ model }}"));
}

#[test]
fn current_versions_keep_event_codec_additive_but_advance_public_and_snapshot_shapes() {
    assert_eq!(ego_core::store::codec::CODEC_VERSION, 12);
    assert_eq!(ego_core::public::PUBLIC_EVENT_VERSION, 19);
    assert_eq!(SNAPSHOT_VERSION, 15);
}

// Catches: setup preview writes artifacts or publishes independent mutations.
#[test]
fn setup_batch_preview_has_no_effect_and_publishes_exact_candidate() {
    use ego_core::config::{ManagedConfigKey, ManagedConfigValue, UserConfigTarget};
    use ego_core::permit::RootAccess;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("absent/config.toml");
    let store = OsUserConfigStore::new(Arc::new(OsIo), &path);
    let changes = vec![
        UserConfigMutation::Root {
            target: UserConfigTarget::Base,
            path: root.path().join("read"),
            access: Some(RootAccess::Read),
        },
        UserConfigMutation::ManagedSet {
            target: UserConfigTarget::Base,
            key: ManagedConfigKey::ExecPaths,
            value: ManagedConfigValue::ExecPaths(vec![root.path().join("bin")]),
        },
    ];
    let preview = store.prepare_batch(changes.clone()).unwrap();
    let candidate = preview.candidate().to_owned();
    assert!(!path.parent().unwrap().exists());
    drop(preview); // A declined confirmation leaves no lock, directory or file.
    assert!(!path.parent().unwrap().exists());
    let preview = store.prepare_batch(changes).unwrap();
    let result = store.publish_batch(preview).unwrap();
    assert!(result.changed);
    assert_eq!(std::fs::read_to_string(path).unwrap(), candidate);
}

// Catches: a stale preview overwrites a concurrent user policy change.
#[test]
fn setup_batch_conflict_preserves_concurrent_user_bytes() {
    use ego_core::config::UserConfigTarget;
    use ego_core::permit::RootAccess;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(&path, "# old\n").unwrap();
    let store = OsUserConfigStore::new(Arc::new(OsIo), &path);
    let preview = store
        .prepare_batch(vec![UserConfigMutation::Root {
            target: UserConfigTarget::Base,
            path: root.path().join("read"),
            access: Some(RootAccess::Read),
        }])
        .unwrap();
    std::fs::write(&path, "# concurrent\n").unwrap();
    assert!(store.publish_batch(preview).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "# concurrent\n");
}

// Catches: post-publication uncertainty is treated as rollback or retried.
#[test]
fn setup_batch_durability_uncertainty_retains_exact_new_authority() {
    use ego_core::config::UserConfigTarget;
    use ego_core::permit::RootAccess;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let io = FaultIo::new();
    let store = OsUserConfigStore::new(&io, &path);
    let preview = store
        .prepare_batch(vec![UserConfigMutation::Root {
            target: UserConfigTarget::Base,
            path: root.path().join("read"),
            access: Some(RootAccess::Read),
        }])
        .unwrap();
    let candidate = preview.candidate().to_owned();
    io.uncertain_after_publish
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let result = store.publish_batch(preview).unwrap();
    assert!(matches!(
        result.publication,
        UserConfigPublication::PublishedDurabilityUncertain { .. }
    ));
    assert_eq!(std::fs::read_to_string(path).unwrap(), candidate);
}

// Catches: setup publication ignores a changed policy/path identity after confirmation.
#[test]
fn setup_batch_identity_recheck_refuses_publication_and_runs_under_lock() {
    use ego_core::config::{UserConfigError, UserConfigTarget};
    use ego_core::permit::RootAccess;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(&path, "# authority\n").unwrap();
    let store = OsUserConfigStore::new(Arc::new(OsIo), &path);
    let preview = store
        .prepare_batch(vec![UserConfigMutation::Root {
            target: UserConfigTarget::Base,
            path: root.path().join("read"),
            access: Some(RootAccess::Read),
        }])
        .unwrap();
    let checked = std::cell::Cell::new(false);
    let result = store.publish_batch_checked(preview, &|| {
        checked.set(true);
        Err(UserConfigError::new("fixture path changed"))
    });
    assert!(checked.get());
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# authority\n");
}
