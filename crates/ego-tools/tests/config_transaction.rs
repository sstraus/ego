//! RED contract for one locked, comment-preserving user-config transaction.

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
fn statusline_mutation_stores_the_final_ordered_names_and_empty_disables_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file(&dir);
    let io = FaultIo::new();
    let store = OsUserConfigStore::new(&io, &path);

    store
        .mutate(UserConfigMutation::Statusline {
            segments: vec!["queue".into(), "model".into(), "interrupt".into()],
        })
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("segments = [\"queue\", \"model\", \"interrupt\"]"));
    store
        .mutate(UserConfigMutation::Statusline { segments: vec![] })
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("segments = []"));
}

#[test]
fn unknown_or_duplicate_segment_names_fail_before_any_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file(&dir);
    let before = std::fs::read_to_string(&path).unwrap();
    let io = FaultIo::new();
    let store = OsUserConfigStore::new(&io, &path);

    for segments in [
        vec!["not-a-segment".into()],
        vec!["model".into(), "model".into()],
    ] {
        assert!(
            store
                .mutate(UserConfigMutation::Statusline { segments })
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
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
                segments: vec!["model".into(), "queue".into()],
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
    assert!(text.contains("segments = [\"model\", \"queue\"]"));
}

#[test]
fn current_versions_keep_event_codec_additive_but_advance_public_and_snapshot_shapes() {
    assert_eq!(ego_core::store::codec::CODEC_VERSION, 12);
    assert_eq!(ego_core::public::PUBLIC_EVENT_VERSION, 9);
    assert_eq!(SNAPSHOT_VERSION, 14);
}
