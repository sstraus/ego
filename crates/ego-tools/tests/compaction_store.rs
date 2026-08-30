//! RED contract for regression case's target-session durability boundary.

use ego_core::cost::{CostDelta, UnknownCost};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::session::{TargetPublication, TargetSeed, TargetStore};
use ego_core::state::Usage;
use ego_core::store::session_store::{SessionEntry, SessionStore};
use ego_tools::store::{AtomicReplacePublication, JsonlStore, OsIo, StoreIo};
use futures::StreamExt;
use futures::executor::block_on;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;

const ALWAYS_ALIVE: &dyn Fn(u32) -> bool = &|_| true;

fn lease_inputs() -> ego_tools::store::LeaseInputs<'static> {
    ego_tools::store::LeaseInputs {
        pid: 47047,
        now: Timestamp::from_millis(9_000),
        stale_after: Duration::from_secs(30),
        is_alive: ALWAYS_ALIVE,
    }
}

fn read_all(store: &JsonlStore, id: &SessionId) -> Vec<DomainEvent> {
    block_on(
        store
            .read(id)
            .expect("target log exists")
            .collect::<Vec<_>>(),
    )
    .into_iter()
    .map(
        |result| match result.expect("target log is durable and decodable") {
            SessionEntry::Event(event) => event,
            SessionEntry::RunTransaction(_) => {
                panic!("compaction target contained a run transaction")
            }
            SessionEntry::Evidence(_) => panic!("compaction target contained evidence"),
        },
    )
    .collect()
}

fn seq(value: u64) -> Seq {
    let mut result = Seq::default();
    for _ in 0..value {
        result.next();
    }
    result
}

fn event(kind: EventKind) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: Seq::default(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(1_000),
            producer: Producer::Main,
        },
        kind,
    }
}

fn seed(source: SessionId, at_seq: Seq) -> TargetSeed {
    TargetSeed::fork(source, at_seq, Timestamp::from_millis(1_000))
}

fn assert_lineage_fields(lineage: &impl serde::Serialize, source: SessionId, at_seq: Seq) {
    let value = serde_json::to_value(lineage).expect("typed lineage serialises");
    assert_eq!(value["kind"], "fork");
    assert_eq!(value["source"], serde_json::to_value(source).unwrap());
    assert_eq!(value["at_seq"], serde_json::to_value(at_seq).unwrap());
}

struct FailingAppendIo;

impl StoreIo for FailingAppendIo {
    fn append_line(&self, _path: &Path, _line: &[u8]) -> io::Result<()> {
        Err(io::Error::other("injected target append failure"))
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
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

/// The rename has made the target visible, but the parent-directory barrier
/// is not known to have completed.  This is the post-publication branch of
/// the target contract, not an append failure that can be retried safely.
struct UncertainPublicationIo;

impl StoreIo for UncertainPublicationIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, line)
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
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
        OsIo.replace_atomic(path, bytes)?;
        Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
            diagnostic: "parent directory barrier failed".to_string(),
        })
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

/// The authoritative target log succeeds, but its derived catalogue cache
/// cannot be refreshed.  The publication must remain authoritative and carry
/// the cache failure as a diagnostic.
struct ManifestCacheFailureIo;

fn is_manifest(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("manifest.json")
}

impl StoreIo for ManifestCacheFailureIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, line)
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
        OsIo.fsync(path)
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        OsIo.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if is_manifest(path) {
            return Err(io::Error::other("injected manifest cache failure"));
        }
        OsIo.replace_atomic(path, bytes)
    }

    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        if is_manifest(path) {
            return Err(io::Error::other("injected manifest cache failure"));
        }
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
fn target_seed_is_durable_before_publication_and_carries_one_lineage_event() {
    let root = tempdir().expect("temporary session root");
    let store = JsonlStore::new(root.path(), "/ws", Box::new(OsIo));
    let target = SessionId::new();
    let source = SessionId::new();
    let open = store
        .open_writer(&target, lease_inputs())
        .expect("target writer lease");
    let publication = open.writer.publish_target(&seed(source, seq(7)));
    assert_eq!(
        publication,
        TargetPublication::PublishedDurably { diagnostic: None }
    );

    let events = read_all(&store, &target);
    assert_eq!(
        events.len(),
        1,
        "the seed is the target's first semantic event"
    );
    assert_eq!(events[0].env.seq, seq(1));
    assert!(events[0].env.item.is_none());
    assert_eq!(events[0].env.parent, None);
    assert_eq!(events[0].env.producer, Producer::Main);
    assert!(matches!(
        &events[0].kind,
        EventKind::Forked { source: actual, at_seq }
            if *actual == source && *at_seq == seq(7)
    ));
    drop(open);
}

#[test]
fn reopening_after_target_seed_recovers_lineage_without_source_history_or_totals() {
    let root = tempdir().expect("temporary session root");
    let store = JsonlStore::new(root.path(), "/ws", Box::new(OsIo));
    let target = SessionId::new();
    let source = SessionId::new();
    let open = store
        .open_writer(&target, lease_inputs())
        .expect("target writer lease");
    assert_eq!(
        open.writer.publish_target(&seed(source, seq(41))),
        TargetPublication::PublishedDurably { diagnostic: None }
    );
    drop(open);

    let resumed = store.resume(&target).expect("target resumes");
    assert!(
        resumed.0.items.is_empty(),
        "a fork seed carries no target item"
    );
    assert_eq!(
        resumed.0.usage.total_tokens(),
        0,
        "target starts fresh totals"
    );
    assert!(
        resumed.0.compactions.is_empty(),
        "in-place compaction is absent"
    );
    assert_eq!(resumed.0.forks.len(), 1, "a fork seed records one fork");
    assert_lineage_fields(
        resumed
            .0
            .lineage
            .as_ref()
            .expect("resume exposes typed lineage"),
        source,
        seq(41),
    );

    let listed = store.list().expect("target is listable");
    let target_meta = listed
        .iter()
        .find(|meta| meta.id == target)
        .expect("target appears in catalogue");
    assert_lineage_fields(
        target_meta
            .lineage
            .as_ref()
            .expect("catalogue exposes typed lineage"),
        source,
        seq(41),
    );
}

#[test]
fn existing_target_is_refused_without_publication() {
    let root = tempdir().expect("temporary session root");
    let store = JsonlStore::new(root.path(), "/ws", Box::new(OsIo));
    let target = SessionId::new();
    let first = store
        .open_writer(&target, lease_inputs())
        .expect("target writer lease");
    assert!(
        first
            .writer
            .publish_target(&seed(SessionId::new(), seq(3)))
            .switch_target()
    );
    drop(first);
    let existing = store
        .open_writer(&target, lease_inputs())
        .expect("existing target writer lease");
    let publication = existing
        .writer
        .publish_target(&seed(SessionId::new(), seq(4)));
    assert!(matches!(
        publication,
        TargetPublication::NotPublished { .. }
    ));
    assert!(!publication.switch_target());
    assert!(publication.retry_safe());
}

#[test]
fn publication_outcomes_are_exhaustive_and_retry_policy_is_explicit() {
    let outcomes = [
        TargetPublication::NotPublished {
            diagnostic: "target append refused".to_string(),
        },
        TargetPublication::PublishedDurably { diagnostic: None },
        TargetPublication::PublishedDurabilityUncertain {
            diagnostic: "parent directory barrier failed".to_string(),
        },
    ];
    for outcome in &outcomes {
        match outcome {
            TargetPublication::NotPublished { diagnostic } => {
                assert!(!diagnostic.is_empty());
                assert!(!outcome.switch_target());
                assert!(outcome.retry_safe());
            }
            TargetPublication::PublishedDurably { diagnostic } => {
                assert!(diagnostic.is_none());
                assert!(outcome.switch_target());
                assert!(!outcome.retry_safe());
            }
            TargetPublication::PublishedDurabilityUncertain { diagnostic } => {
                assert!(!diagnostic.is_empty());
                assert!(outcome.switch_target());
                assert!(!outcome.retry_safe());
            }
        }
    }
}

#[test]
fn prepublication_target_failure_is_not_published_and_is_retry_safe() {
    let root = tempdir().expect("temporary session root");
    let store = JsonlStore::new(root.path(), "/ws", Box::new(FailingAppendIo));
    let target = SessionId::new();
    let open = store
        .open_writer(&target, lease_inputs())
        .expect("target writer lease");

    let publication = open.writer.publish_target(&seed(SessionId::new(), seq(5)));
    assert!(matches!(
        publication,
        TargetPublication::NotPublished { .. }
    ));
    assert!(!publication.switch_target());
    assert!(publication.retry_safe());
}

#[test]
fn post_rename_durability_uncertainty_publishes_target_and_forbids_retry() {
    let root = tempdir().expect("temporary session root");
    let store = JsonlStore::new(root.path(), "/ws", Box::new(UncertainPublicationIo));
    let target = SessionId::new();
    let open = store
        .open_writer(&target, lease_inputs())
        .expect("target writer lease");
    let publication = open.writer.publish_target(&seed(SessionId::new(), seq(8)));
    assert!(matches!(
        publication,
        TargetPublication::PublishedDurabilityUncertain { ref diagnostic }
            if diagnostic.contains("parent directory barrier failed")
    ));
    assert!(publication.switch_target());
    assert!(!publication.retry_safe());
    drop(open);
    assert_eq!(
        read_all(&store, &target).len(),
        1,
        "visible target is authoritative"
    );
}

#[test]
fn authoritative_log_survives_manifest_cache_failure_with_diagnostic() {
    let root = tempdir().expect("temporary session root");
    let store = JsonlStore::new(root.path(), "/ws", Box::new(ManifestCacheFailureIo));
    let target = SessionId::new();
    let open = store
        .open_writer(&target, lease_inputs())
        .expect("target writer lease");
    let publication = open.writer.publish_target(&seed(SessionId::new(), seq(9)));
    assert!(matches!(
        publication,
        TargetPublication::PublishedDurably {
            diagnostic: Some(ref diagnostic)
        } if diagnostic.contains("manifest")
    ));
    assert!(publication.switch_target());
    assert!(!publication.retry_safe());
    drop(open);
    assert_eq!(
        read_all(&store, &target).len(),
        1,
        "log publication is authoritative"
    );
}

#[test]
fn crash_before_the_seed_line_is_recovered_as_an_unusable_target() {
    let root = tempdir().expect("temporary session root");
    let store = JsonlStore::new(root.path(), "/ws", Box::new(OsIo));
    let target = SessionId::new();
    let open = store
        .open_writer(&target, lease_inputs())
        .expect("target writer lease");
    assert_eq!(
        open.writer.publish_target(&seed(SessionId::new(), seq(2))),
        TargetPublication::PublishedDurably { diagnostic: None }
    );
    drop(open);
    let id = serde_json::to_value(target)
        .expect("target id serialises")
        .as_str()
        .expect("target id is a string")
        .to_string();
    let log = root.path().join(PathBuf::from(id)).join("events.jsonl");
    let bytes = std::fs::read(&log).expect("seed log exists");
    std::fs::write(&log, &bytes[..bytes.len() - 1]).expect("tear the final newline");
    let repair = store.recover(&target).expect("crash recovery runs");
    assert!(!repair.is_clean());
    let resumed = store
        .resume(&target)
        .expect("recovery leaves a readable target");
    assert!(resumed.0.items.is_empty(), "no seed means no usable target");
}

#[test]
fn source_totals_are_not_silently_carried_into_the_fresh_target() {
    let root = tempdir().expect("temporary session root");
    let store = JsonlStore::new(root.path(), "/ws", Box::new(OsIo));
    let source = SessionId::new();
    let target = SessionId::new();
    let source_usage = Usage {
        uncached_input_tokens: 31,
        output_tokens: 9,
        ..Usage::IDENTITY
    };
    store
        .append(
            &source,
            &event(EventKind::UsageUpdated {
                usage: source_usage,
                cost_delta: CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage),
                attempt: None,
            }),
        )
        .expect("source usage is durable");
    let open = store
        .open_writer(&target, lease_inputs())
        .expect("target writer lease");
    assert_eq!(
        open.writer.publish_target(&seed(source, seq(1))),
        TargetPublication::PublishedDurably { diagnostic: None }
    );
    drop(open);

    let source_state = store.resume(&source).expect("source resumes").0;
    let target_state = store.resume(&target).expect("target resumes").0;
    assert_eq!(source_state.usage, source_usage);
    assert_eq!(target_state.usage, Usage::IDENTITY);
    assert!(target_state.charges.is_empty());
}
