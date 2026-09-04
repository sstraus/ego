//! RED adapter contract for regression case.
//!
//! These tests exercise the outer durable-session adapter. Creation is a
//! header-only operation; appending a conversation event is intentionally not
//! used as a proxy for session creation.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_core::ceiling::Ceilings;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Clock;
use ego_core::event::time::Timestamp;
use ego_core::goal::{GoalOwner, GoalTransition};
use ego_core::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, LifecycleAppendError, SessionActivity,
    SessionLifecycle, SessionLifecycleStore, SessionRunLifecycle,
};
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionStore};
use ego_tools::store::{AtomicReplacePublication, JsonlStore, LeaseInputs, OsIo, StoreIo};
use futures::StreamExt;
use tempfile::tempdir;

struct InactiveRuns;

impl SessionRunLifecycle for InactiveRuns {
    fn activity(&self, _session: SessionId) -> ActivityFuture<'_> {
        Box::pin(std::future::ready(Ok(SessionActivity::Inactive)))
    }

    fn cancel_and_settle(&self, session: SessionId) -> CloseFuture<'_> {
        Box::pin(std::future::ready(Ok(ClosedSession {
            id: session,
            cancelled: true,
        })))
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1_000)
    }
}

struct RecordingIo {
    inner: OsIo,
    calls: Arc<Mutex<Vec<IoCall>>>,
}

#[derive(Clone, Copy, Default)]
enum PublicationCut {
    #[default]
    None,
    BeforeRename,
    AfterRename,
}

#[derive(Default)]
struct CutControl {
    cut: Mutex<PublicationCut>,
    publication_calls: Mutex<usize>,
}

impl CutControl {
    fn set(&self, cut: PublicationCut) {
        *self.cut.lock().unwrap() = cut;
    }

    fn publication_calls(&self) -> usize {
        *self.publication_calls.lock().unwrap()
    }
}

struct CutIo {
    inner: OsIo,
    control: Arc<CutControl>,
}

impl StoreIo for CutIo {
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
        *self.control.publication_calls.lock().unwrap() += 1;
        match *self.control.cut.lock().unwrap() {
            PublicationCut::None => self.inner.replace_atomic_publication(path, bytes),
            PublicationCut::BeforeRename => {
                Err(io::Error::other("injected pre-publication failure"))
            }
            PublicationCut::AfterRename => {
                self.inner.replace_atomic(path, bytes)?;
                Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
                    diagnostic: "injected post-publication uncertainty".into(),
                })
            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IoCall {
    AppendLine,
    Fsync,
    ReplaceAtomic,
    ReplaceAtomicPublication,
}

impl RecordingIo {
    fn new(calls: Arc<Mutex<Vec<IoCall>>>) -> Self {
        Self { inner: OsIo, calls }
    }
    fn record(&self, operation: IoCall) {
        self.calls.lock().unwrap().push(operation);
    }
}

impl StoreIo for RecordingIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.record(IoCall::AppendLine);
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.record(IoCall::Fsync);
        self.inner.fsync(path)
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.exists(path)
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.record(IoCall::ReplaceAtomic);
        self.inner.replace_atomic(path, bytes)
    }
    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        self.record(IoCall::ReplaceAtomicPublication);
        self.inner.replace_atomic_publication(path, bytes)
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

const ALIVE: &dyn Fn(u32) -> bool = &|_| true;

fn lease() -> LeaseInputs<'static> {
    LeaseInputs {
        pid: 83_083,
        now: Timestamp::from_millis(83_083),
        stale_after: Duration::from_secs(30),
        is_alive: ALIVE,
    }
}

fn lifecycle_event(seq: u64, kind: EventKind) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: serde_json::from_value(serde_json::json!(seq)).unwrap(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(seq),
            producer: Producer::Main,
        },
        kind,
    }
}

fn store(root: &Path, calls: Arc<Mutex<Vec<IoCall>>>) -> JsonlStore {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    JsonlStore::new(
        root.join("sessions"),
        workspace,
        Box::new(RecordingIo::new(calls)),
    )
}

fn cut_store(root: &Path, control: Arc<CutControl>) -> JsonlStore {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    JsonlStore::new(
        root.join("sessions"),
        workspace,
        Box::new(CutIo {
            inner: OsIo,
            control,
        }),
    )
}

fn session_entries(store: &JsonlStore, id: &SessionId) -> Vec<SessionEntry> {
    futures::executor::block_on(store.read(id).unwrap().collect::<Vec<_>>())
        .into_iter()
        .map(|entry| entry.unwrap())
        .collect()
}

#[test]
fn create_header_only_publishes_manifest_and_empty_log() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let store = store(dir.path(), Arc::clone(&calls));
    let id = SessionId::new();
    store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    let listed = store.list().unwrap();
    assert_eq!(listed[0].id, id);
    assert!(
        calls
            .lock()
            .unwrap()
            .contains(&IoCall::ReplaceAtomicPublication)
    );
}

#[test]
fn fresh_adapter_rebuilds_catalogue_from_header_only_durable_state() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first = store(dir.path(), Arc::clone(&calls));
    let id = SessionId::new();
    first
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    let restarted = store(dir.path(), calls);
    assert_eq!(restarted.list().unwrap()[0].id, id);
}

#[test]
fn writer_lease_excludes_a_second_writer_for_the_same_session() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first = store(dir.path(), Arc::clone(&calls));
    let id = SessionId::new();
    first
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    let held = first.open_writer(&id, lease()).unwrap();
    let second = store(dir.path(), calls);
    assert!(second.open_writer(&id, lease()).is_err());
    drop(held);
}

#[test]
fn compare_and_append_title_and_delete_are_typed_single_session_mutations() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let store = store(dir.path(), calls);
    let id = SessionId::new();
    store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    store
        .compare_and_append_lifecycle(
            id,
            Seq::ZERO,
            lifecycle_event(
                1,
                EventKind::SessionTitleChanged {
                    title: Some("named".into()),
                },
            ),
        )
        .unwrap();
    store
        .compare_and_append_lifecycle(
            id,
            serde_json::from_value(serde_json::json!(1)).unwrap(),
            lifecycle_event(2, EventKind::SessionDeleted),
        )
        .unwrap();
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn jsonl_lifecycle_allows_goal_events_and_replays_them_after_restart() {
    let dir = tempdir().unwrap();
    let jsonl = store(dir.path(), Arc::new(Mutex::new(Vec::new())));
    let id = SessionId::new();
    jsonl
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    let goal = ego_core::event::id::GoalId::new();
    jsonl
        .compare_and_append_lifecycle(
            id,
            Seq::ZERO,
            lifecycle_event(
                1,
                EventKind::GoalSet {
                    id: goal,
                    objective: "ship".into(),
                    owner: GoalOwner::Operator,
                    ceilings: Ceilings::default(),
                    derived_from: None,
                },
            ),
        )
        .unwrap();
    jsonl
        .compare_and_append_lifecycle(
            id,
            serde_json::from_value(serde_json::json!(1)).unwrap(),
            lifecycle_event(
                2,
                EventKind::GoalTransitioned {
                    id: goal,
                    transition: GoalTransition::BlockerObserved {
                        reason: "blocked".into(),
                    },
                },
            ),
        )
        .unwrap();
    let restarted = store(dir.path(), Arc::new(Mutex::new(Vec::new())));
    let events = session_entries(&restarted, &id)
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(event),
            _ => None,
        });
    assert_eq!(SessionState::replay(events).goal.unwrap().id, goal);
}

#[test]
fn stale_compare_and_append_is_a_conflict_without_a_second_lifecycle_event() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let store = store(dir.path(), calls);
    let id = SessionId::new();
    store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    store
        .compare_and_append_lifecycle(
            id,
            Seq::ZERO,
            lifecycle_event(
                1,
                EventKind::SessionTitleChanged {
                    title: Some("named".into()),
                },
            ),
        )
        .unwrap();
    let result = store.compare_and_append_lifecycle(
        id,
        Seq::ZERO,
        lifecycle_event(2, EventKind::SessionDeleted),
    );
    assert!(matches!(
        result,
        Err(ego_core::session_lifecycle::LifecycleAppendError::Conflict)
    ));
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn publication_cut_reports_visibility_and_retry_policy_without_duplicate_manifest() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let store = store(dir.path(), calls);
    let id = SessionId::new();
    let publication = store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    assert!(publication.is_visible());
    assert!(!publication.retry_safe());
    assert_eq!(
        store
            .list()
            .unwrap()
            .iter()
            .filter(|meta| meta.id == id)
            .count(),
        1
    );
}

#[test]
fn lifecycle_create_prepublication_failure_is_hidden_and_retry_safe() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let root = dir.path().join("sessions");
    let store = JsonlStore::new(
        &root,
        &workspace,
        Box::new(CutIo {
            inner: OsIo,
            control: Arc::new(CutControl {
                cut: Mutex::new(PublicationCut::BeforeRename),
                publication_calls: Mutex::new(0),
            }),
        }),
    );
    let id = SessionId::new();
    assert!(
        store
            .publish_empty_session(id, &workspace, Timestamp::from_millis(1))
            .is_err()
    );
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn lifecycle_create_postpublication_uncertainty_remains_visible_and_nonretryable() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let root = dir.path().join("sessions");
    let store = JsonlStore::new(
        &root,
        &workspace,
        Box::new(CutIo {
            inner: OsIo,
            control: Arc::new(CutControl {
                cut: Mutex::new(PublicationCut::AfterRename),
                publication_calls: Mutex::new(0),
            }),
        }),
    );
    let id = SessionId::new();
    let publication = store
        .publish_empty_session(id, &workspace, Timestamp::from_millis(1))
        .unwrap();
    assert!(publication.is_visible());
    assert!(!publication.retry_safe());
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn lifecycle_title_prepublication_failure_preserves_authority_after_restart() {
    let dir = tempdir().unwrap();
    let control = Arc::new(CutControl::default());
    let store = cut_store(dir.path(), Arc::clone(&control));
    let id = SessionId::new();
    store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    control.set(PublicationCut::BeforeRename);

    let result = store.compare_and_append_lifecycle(
        id,
        Seq::ZERO,
        lifecycle_event(
            1,
            EventKind::SessionTitleChanged {
                title: Some("named".into()),
            },
        ),
    );
    assert!(matches!(result, Err(LifecycleAppendError::Store(_))));
    assert_eq!(store.lifecycle_meta(id).unwrap().unwrap().title, None);
    assert_eq!(control.publication_calls(), 2);

    let restarted = cut_store(dir.path(), control);
    assert_eq!(restarted.lifecycle_meta(id).unwrap().unwrap().title, None);
    assert_eq!(restarted.list().unwrap().len(), 1);
    assert_eq!(session_entries(&restarted, &id).len(), 0);
}

#[test]
fn lifecycle_title_postpublication_uncertainty_is_visible_once_after_restart() {
    let dir = tempdir().unwrap();
    let control = Arc::new(CutControl::default());
    let store = cut_store(dir.path(), Arc::clone(&control));
    let id = SessionId::new();
    store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    control.set(PublicationCut::AfterRename);

    let publication = store
        .compare_and_append_lifecycle(
            id,
            Seq::ZERO,
            lifecycle_event(
                1,
                EventKind::SessionTitleChanged {
                    title: Some("named".into()),
                },
            ),
        )
        .unwrap();
    assert!(publication.is_visible());
    assert!(!publication.retry_safe());
    assert_eq!(control.publication_calls(), 2);
    assert_eq!(session_entries(&store, &id).len(), 1);

    let restarted = cut_store(dir.path(), control);
    assert_eq!(
        restarted.lifecycle_meta(id).unwrap().unwrap().title,
        Some("named".into())
    );
    assert_eq!(restarted.list().unwrap().len(), 1);
    assert_eq!(session_entries(&restarted, &id).len(), 1);
}

#[test]
fn lifecycle_delete_prepublication_failure_preserves_authority_after_restart() {
    let dir = tempdir().unwrap();
    let control = Arc::new(CutControl::default());
    let store = cut_store(dir.path(), Arc::clone(&control));
    let id = SessionId::new();
    store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    control.set(PublicationCut::BeforeRename);

    let result = store.compare_and_append_lifecycle(
        id,
        Seq::ZERO,
        lifecycle_event(1, EventKind::SessionDeleted),
    );
    assert!(matches!(result, Err(LifecycleAppendError::Store(_))));
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(control.publication_calls(), 2);

    let restarted = cut_store(dir.path(), control);
    assert_eq!(restarted.list().unwrap().len(), 1);
    assert!(restarted.lifecycle_meta(id).unwrap().is_some());
    assert_eq!(session_entries(&restarted, &id).len(), 0);
}

#[test]
fn lifecycle_delete_postpublication_uncertainty_is_visible_once_after_restart() {
    let dir = tempdir().unwrap();
    let control = Arc::new(CutControl::default());
    let store = cut_store(dir.path(), Arc::clone(&control));
    let id = SessionId::new();
    store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    control.set(PublicationCut::AfterRename);

    let publication = store
        .compare_and_append_lifecycle(id, Seq::ZERO, lifecycle_event(1, EventKind::SessionDeleted))
        .unwrap();
    assert!(publication.is_visible());
    assert!(!publication.retry_safe());
    assert_eq!(control.publication_calls(), 2);
    assert!(store.list().unwrap().is_empty());
    assert_eq!(session_entries(&store, &id).len(), 1);

    let restarted = cut_store(dir.path(), control);
    assert!(restarted.list().unwrap().is_empty());
    assert!(restarted.lifecycle_meta(id).unwrap().is_some());
    assert_eq!(session_entries(&restarted, &id).len(), 1);
}

#[test]
fn reopened_lifecycle_preserves_title_lineage_and_keyset_catalogue_order() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let make_service = || {
        let store = JsonlStore::new(&sessions, &workspace, Box::new(OsIo));
        SessionLifecycle::new(
            Arc::new(store),
            Arc::new(InactiveRuns),
            Arc::new(FixedClock),
        )
    };

    let service = make_service();
    let source = SessionId::new();
    let peer = SessionId::new();
    let third = SessionId::new();
    service.create_with_id(source, workspace.clone()).unwrap();
    service.create_with_id(peer, workspace.clone()).unwrap();
    service.create_with_id(third, workspace.clone()).unwrap();
    service
        .set_title(source, Some("source title".into()))
        .unwrap();
    let child = futures::executor::block_on(service.fork(source))
        .unwrap()
        .id;
    drop(service);

    let reopened = make_service();
    let child_meta = reopened.meta(child).unwrap();
    assert_eq!(child_meta.title.as_deref(), Some("source title"));
    assert!(matches!(
        child_meta.lineage,
        Some(ego_core::session::SessionLineage::Fork { source: parent, .. }) if parent == source
    ));

    let mut expected = [source, peer, third, child];
    expected.sort();
    let page = reopened.list(None, 2, Some(&workspace)).unwrap();
    assert_eq!(
        page.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        expected[..2]
    );
    let cursor = page.next_cursor.expect("second page cursor");
    let next = reopened.list(Some(&cursor), 2, Some(&workspace)).unwrap();
    assert_eq!(
        next.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        expected[2..]
    );
}

#[test]
fn reopened_fork_seed_accepts_active_goal_with_and_without_title() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let make_service = || {
        SessionLifecycle::new(
            Arc::new(JsonlStore::new(&sessions, &workspace, Box::new(OsIo))),
            Arc::new(InactiveRuns),
            Arc::new(FixedClock),
        )
    };

    let service = make_service();
    let titled = service.create(workspace.clone()).unwrap().id;
    service.set_title(titled, Some("named".into())).unwrap();
    let titled_goal = service
        .set_goal(
            titled,
            "titled objective".into(),
            GoalOwner::Operator,
            Ceilings::default(),
        )
        .unwrap();
    let titled_child = futures::executor::block_on(service.fork(titled))
        .unwrap()
        .id;

    let untitled = service.create(workspace.clone()).unwrap().id;
    let untitled_goal = service
        .set_goal(
            untitled,
            "untitled objective".into(),
            GoalOwner::Operator,
            Ceilings::default(),
        )
        .unwrap();
    let untitled_child = futures::executor::block_on(service.fork(untitled))
        .unwrap()
        .id;
    drop(service);

    let reopened = make_service();
    for (child, source_goal) in [(titled_child, titled_goal), (untitled_child, untitled_goal)] {
        let inherited = reopened.goal(child).unwrap().unwrap();
        assert_ne!(inherited.id, source_goal.id);
        assert_eq!(inherited.derived_from, Some(source_goal.id));
        assert_eq!(inherited.objective, source_goal.objective);
        assert_eq!(inherited.owner, source_goal.owner);
        assert_eq!(inherited.ceilings, source_goal.ceilings);
    }
}
