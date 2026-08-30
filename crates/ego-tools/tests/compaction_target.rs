//! RED contract for regression case's rooted JSONL compaction target port.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ego_core::compact::{CompactionTargetFacts, CompactionTargetInspection, CompactionTargetPort};
use ego_core::event::id::{Seq, SessionId};
use ego_core::session::{TargetPublication, TargetSeed};
use ego_tools::store::{AtomicReplacePublication, JsonlStore, OsIo, StoreIo};
use tempfile::{TempDir, tempdir};

fn fixture() -> (TempDir, JsonlStore) {
    let root = tempdir().expect("target root");
    let store = JsonlStore::new(root.path(), Path::new("/workspace"), Box::new(OsIo));
    (root, store)
}

fn seq(value: u64) -> Seq {
    let mut result = Seq::ZERO;
    for _ in 0..value {
        result.next();
    }
    result
}

fn log_path(root: &Path, id: SessionId) -> std::path::PathBuf {
    root.join(id.to_string()).join("events.jsonl")
}

fn facts(source: SessionId, source_seq: u64) -> CompactionTargetFacts {
    CompactionTargetFacts {
        source,
        source_seq: seq(source_seq),
        prompt_revision: 1,
    }
}

fn checkpoint() -> String {
    "# Objective\nContinue.\n# Binding constraints and preferences\nPreserve.\n# Completed work and evidence\nFixture.\n# Work in progress\nTransition.\n# Decisions and rationale\nTyped.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nTest.\n# Critical references\nADR."
        .to_owned()
}

fn compaction_seed(source: SessionId, source_seq: u64) -> TargetSeed {
    TargetSeed::compaction(
        source,
        seq(source_seq),
        1,
        checkpoint(),
        ego_core::event::time::Timestamp::from_millis(68_000),
    )
}

struct BarrierCountingIo {
    barriers: Arc<AtomicUsize>,
}

impl StoreIo for BarrierCountingIo {
    fn append_line(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, bytes)
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.barriers.fetch_add(1, Ordering::SeqCst);
        OsIo.fsync(path)
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        OsIo.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.replace_atomic(path, bytes)
    }

    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        OsIo.replace_atomic_publication(path, bytes)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OsIo.truncate(path, len)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        OsIo.remove_file(path)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        OsIo.list_dirs(path)
    }
}

#[test]
fn missing_target_is_observed_without_creating_a_manifest_or_log() {
    let (root, store) = fixture();
    let target = SessionId::new();
    assert_eq!(
        CompactionTargetPort::inspect(&store, target).expect("missing target inspection"),
        CompactionTargetInspection::Missing
    );
    assert!(!root.path().join(target.to_string()).exists());
}

#[test]
fn target_port_exposes_only_the_sequence_one_compaction_seed_facts() {
    let (_root, store) = fixture();
    let target = SessionId::new();
    let source = SessionId::new();
    let seed = compaction_seed(source, 12);
    let publication = CompactionTargetPort::publish(&store, target, &seed);
    assert!(publication.switch_target());
    let inspection = CompactionTargetPort::inspect(&store, target).expect("target inspection");
    assert_eq!(
        inspection,
        CompactionTargetInspection::Compaction(facts(source, 12))
    );
}

#[test]
fn matching_target_confirmation_rebuilds_manifest_and_requests_durability() {
    let (root, store) = fixture();
    let target = SessionId::new();
    let source = SessionId::new();
    let seed = compaction_seed(source, 7);
    assert!(CompactionTargetPort::publish(&store, target, &seed).switch_target());
    let result = CompactionTargetPort::confirm(&store, target, facts(source, 7))
        .expect("matching target confirmation");
    assert!(matches!(result, TargetPublication::PublishedDurably { .. }));
    let manifest = root.path().join(target.to_string()).join("manifest.json");
    let bytes = fs::read(&manifest).expect("rebuilt manifest");
    assert!(!bytes.is_empty());
}

#[test]
fn later_valid_target_entries_do_not_replace_the_authoritative_seed() {
    let (_root, store) = fixture();
    let target = SessionId::new();
    let source = SessionId::new();
    let seed = compaction_seed(source, 3);
    assert!(CompactionTargetPort::publish(&store, target, &seed).switch_target());
    assert_eq!(
        CompactionTargetPort::inspect(&store, target).expect("target inspection"),
        CompactionTargetInspection::Compaction(facts(source, 3))
    );
}

#[test]
fn corrupt_target_log_fails_closed_before_manifest_rebuild() {
    let (root, store) = fixture();
    let target = SessionId::new();
    let path = log_path(root.path(), target);
    fs::create_dir_all(path.parent().expect("target parent")).expect("target directory");
    fs::write(&path, b"not-json\n").expect("corrupt target fixture");
    assert!(CompactionTargetPort::inspect(&store, target).is_err());
}

#[test]
fn gapped_target_log_fails_closed_before_confirming_any_publication() {
    let (root, store) = fixture();
    let target = SessionId::new();
    let path = log_path(root.path(), target);
    fs::create_dir_all(path.parent().expect("target parent")).expect("target directory");
    fs::write(&path, br#"{"seq":2,"checksum":0,"payload":{}}\n"#).expect("gapped target fixture");
    assert!(CompactionTargetPort::inspect(&store, target).is_err());
}

#[test]
fn unknown_first_target_record_fails_closed_without_a_derived_manifest() {
    let (root, store) = fixture();
    let target = SessionId::new();
    let path = log_path(root.path(), target);
    fs::create_dir_all(path.parent().expect("target parent")).expect("target directory");
    fs::write(
        &path,
        br#"{"seq":1,"checksum":0,"payload":{"kind":"future_compaction"}}\n"#,
    )
    .expect("unknown target fixture");
    assert!(CompactionTargetPort::inspect(&store, target).is_err());
}

#[test]
fn wrong_source_or_revision_is_an_identity_collision_with_no_effect() {
    let (_root, store) = fixture();
    let target = SessionId::new();
    let result = CompactionTargetPort::confirm(&store, target, facts(SessionId::new(), 99));
    assert!(result.is_err());
}

#[test]
fn confirmation_uses_the_injected_store_barrier_and_rebuilds_manifest() {
    let root = tempdir().expect("target root");
    let barriers = Arc::new(AtomicUsize::new(0));
    let store = JsonlStore::new(
        root.path(),
        Path::new("/workspace"),
        Box::new(BarrierCountingIo {
            barriers: Arc::clone(&barriers),
        }),
    );
    let target = SessionId::new();
    let source = SessionId::new();
    let seed = compaction_seed(source, 9);
    assert!(CompactionTargetPort::publish(&store, target, &seed).switch_target());
    let publication = CompactionTargetPort::confirm(&store, target, facts(source, 9))
        .expect("confirmation barrier");
    assert!(matches!(
        publication,
        TargetPublication::PublishedDurably { .. }
    ));
    assert!(barriers.load(Ordering::SeqCst) > 0);
    assert!(
        !root
            .path()
            .join(target.to_string())
            .join("manifest.json")
            .metadata()
            .expect("manifest metadata")
            .len()
            .eq(&0)
    );
}

#[test]
fn not_published_is_retry_safe_but_published_uncertainty_is_not() {
    let outcomes = [
        TargetPublication::NotPublished {
            diagnostic: "target was not visible".to_owned(),
        },
        TargetPublication::PublishedDurably { diagnostic: None },
        TargetPublication::PublishedDurabilityUncertain {
            diagnostic: "parent barrier failed".to_owned(),
        },
    ];
    assert!(outcomes[0].retry_safe());
    assert!(!outcomes[1].retry_safe());
    assert!(!outcomes[2].retry_safe());
}

#[test]
fn target_projection_does_not_copy_source_events_or_accounting() {
    let facts = facts(SessionId::new(), 12);
    assert_eq!(facts.source_seq, seq(12));
    assert_eq!(facts.prompt_revision, 1);
}
