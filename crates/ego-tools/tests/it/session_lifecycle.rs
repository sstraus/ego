//! adapter contract for Story 096.
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
use ego_core::run::RequestId;
use ego_core::session_lifecycle::LeasedLifecycleAppend;
use ego_core::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, HoldSignalFuture, HoldState, LifecycleAppendError,
    SessionActivity, SessionLifecycle, SessionLifecycleStore, SessionMetadata, SessionRunLifecycle,
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

    /// `false`: this double reports no live run, so a pause has no turn to
    /// reach and the caller writes the boundary itself.
    fn hold(&self, _session: SessionId, _request: RequestId) -> HoldSignalFuture<'_> {
        Box::pin(std::future::ready(Ok(false)))
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
    fail_manifest: Mutex<bool>,
}

impl CutControl {
    fn set(&self, cut: PublicationCut) {
        *self.cut.lock().unwrap() = cut;
    }

    fn publication_calls(&self) -> usize {
        *self.publication_calls.lock().unwrap()
    }
    fn fail_manifest(&self) {
        *self.fail_manifest.lock().unwrap() = true;
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
        if *self.control.fail_manifest.lock().unwrap() {
            return Err(io::Error::other("injected manifest rebuild failure"));
        }
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

fn append_lifecycle(
    store: &JsonlStore,
    id: SessionId,
    expected: Seq,
    event: DomainEvent,
) -> Result<ego_core::session_lifecycle::LifecyclePublication, LifecycleAppendError> {
    let opened = store
        .open_writer(&id, lease())
        .map_err(|_| LifecycleAppendError::LeaseHeld)?;
    opened.writer.compare_and_append_lifecycle(expected, event)
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

// Catches: a missing manifest drops the creation ceiling during catalogue recovery.
#[test]
fn profile_ceiling_creation_authority_survives_missing_manifest() {
    let dir = tempdir().unwrap();
    let first = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    let id = SessionId::new();
    let profiles = ego_core::store::session_store::SessionProfiles {
        profile: Some("repo".to_owned()),
        ceiling_profile: Some("machine".to_owned()),
        prompt_options: Some(ego_core::config::SessionPromptOptions {
            bare: Some(true),
            ..Default::default()
        }),
    };
    first
        .publish_session(
            id,
            Path::new("/workspace"),
            Timestamp::from_millis(1),
            profiles.clone(),
        )
        .unwrap();
    std::fs::remove_file(dir.path().join(id.to_string()).join("manifest.json")).unwrap();
    let restarted = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
    assert_eq!(
        restarted.lifecycle_meta(id).unwrap().unwrap().profiles,
        profiles
    );
    assert_eq!(restarted.list().unwrap()[0].profiles, profiles);
}

/// Catches a quarantine kept in memory or needing the writer lease: it must
/// survive a new adapter over the same directory and be set while another
/// writer holds the session.
#[test]
fn quarantine_is_durable_and_needs_no_writer_lease() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first = store(dir.path(), Arc::clone(&calls));
    let id = SessionId::new();
    let other = SessionId::new();
    for session in [id, other] {
        first
            .publish_empty_session(session, Path::new("/workspace"), Timestamp::from_millis(1))
            .unwrap();
    }
    let _writer = first.acquire_inactive_lease(id).ok().unwrap();
    first.quarantine(id).unwrap();
    first.quarantine(id).unwrap();
    let restarted = store(dir.path(), calls);
    assert!(restarted.is_quarantined(id).unwrap());
    assert!(!restarted.is_quarantined(other).unwrap());
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
    append_lifecycle(
        &store,
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
    append_lifecycle(
        &store,
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
    append_lifecycle(
        &jsonl,
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
    append_lifecycle(
        &jsonl,
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
    append_lifecycle(
        &store,
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
    let result = append_lifecycle(
        &store,
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
                fail_manifest: Mutex::new(false),
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
                fail_manifest: Mutex::new(false),
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

    let result = append_lifecycle(
        &store,
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

    let publication = append_lifecycle(
        &store,
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

    let result = append_lifecycle(
        &store,
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

    let publication = append_lifecycle(
        &store,
        id,
        Seq::ZERO,
        lifecycle_event(1, EventKind::SessionDeleted),
    )
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
    let mutation_store = JsonlStore::new(&sessions, &workspace, Box::new(OsIo));
    let metadata = SessionMetadata::new(Arc::new(mutation_store), Arc::new(FixedClock));
    let writer_store = JsonlStore::new(&sessions, &workspace, Box::new(OsIo));
    let writer = writer_store.open_writer(&source, lease()).unwrap();
    metadata
        .set_title(&writer.writer, Seq::ZERO, Some("source title".into()))
        .unwrap();
    drop(writer);
    let child = futures::executor::block_on(service.fork(source))
        .unwrap()
        .id;
    drop(service);

    let reopened = make_service();
    let reopened_meta = SessionMetadata::new(
        Arc::new(JsonlStore::new(&sessions, &workspace, Box::new(OsIo))),
        Arc::new(FixedClock),
    );
    let child_meta = reopened_meta.meta(child).unwrap();
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
fn catalogue_does_not_refuse_healthy_sessions_when_a_cached_session_log_is_missing() {
    // Catches: one manifest-only session turns ACP session/list into "session not found".
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
    let missing = service.create(workspace.clone()).unwrap().id;
    let healthy = service.create(workspace.clone()).unwrap().id;
    futures::executor::block_on(service.load(missing, &workspace)).unwrap();
    drop(service);

    // Fault injection affects only this test's files. Preserve the real manifest
    // produced by creation; it must not confer authority after its log is gone.
    let missing_dir = sessions.join(missing.to_string());
    std::fs::remove_file(missing_dir.join("events.jsonl")).unwrap();
    let restarted = make_service();
    let refused = futures::executor::block_on(restarted.load(missing, &workspace)).unwrap_err();
    // Catches: missing-session diagnostics name the workspace instead of the actual store.
    assert!(refused.to_string().contains(&missing.to_string()));
    assert!(
        refused
            .to_string()
            .contains(&sessions.display().to_string())
    );
    match refused {
        ego_core::session_lifecycle::LifecycleError::UnknownSession {
            session,
            store_root,
        } => {
            assert_eq!(session, missing);
            assert_eq!(store_root.as_deref(), Some(sessions.as_path()));
        }
        other => panic!("expected a missing-session refusal, got {other}"),
    }
    assert!(missing_dir.join("manifest.json").is_file());
    let page = restarted.list(None, 1, Some(&workspace)).unwrap();
    assert_eq!(
        page.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        vec![healthy]
    );
    assert!(page.next_cursor.is_none());
    futures::executor::block_on(restarted.load(healthy, &workspace)).unwrap();
}

#[test]
fn lifecycle_title_requires_the_session_writer_lease() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let lifecycle_store = JsonlStore::new(
        dir.path().join("sessions"),
        workspace.clone(),
        Box::new(OsIo),
    );
    let service = SessionLifecycle::new(
        Arc::new(lifecycle_store),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    let id = service.create(workspace.clone()).unwrap().id;
    let holder_store = JsonlStore::new(dir.path().join("sessions"), workspace, Box::new(OsIo));
    let _held = holder_store.open_writer(&id, lease()).unwrap();
    let metadata_store = JsonlStore::new(
        dir.path().join("sessions"),
        dir.path().join("workspace"),
        Box::new(OsIo),
    );
    let metadata = SessionMetadata::new(Arc::new(metadata_store), Arc::new(FixedClock));
    let blocked_writer_store = JsonlStore::new(
        dir.path().join("sessions"),
        dir.path().join("workspace"),
        Box::new(OsIo),
    );
    assert!(matches!(
        blocked_writer_store.open_writer(&id, lease()),
        Err(ego_tools::store::OpenError::Lease(_))
    ));
    assert_eq!(metadata.meta(id).unwrap().title, None);
    assert_eq!(session_entries(&holder_store, &id).len(), 0);
}

#[test]
fn reopened_fork_seed_accepts_active_goal_and_plan_with_and_without_title() {
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
    let mutation_store = JsonlStore::new(&sessions, &workspace, Box::new(OsIo));
    let metadata = SessionMetadata::new(Arc::new(mutation_store), Arc::new(FixedClock));
    let writer_store = JsonlStore::new(&sessions, &workspace, Box::new(OsIo));
    let titled_writer = writer_store.open_writer(&titled, lease()).unwrap();
    metadata
        .set_title(&titled_writer.writer, Seq::ZERO, Some("named".into()))
        .unwrap();
    let mut one = Seq::ZERO;
    one.next();
    let titled_goal = metadata
        .set_goal(
            &titled_writer.writer,
            one,
            "titled objective".into(),
            GoalOwner::Operator,
            Ceilings::default(),
        )
        .unwrap();
    let inherited_plan = ego_core::plan::PlanUpdate::Replace {
        steps: vec![ego_core::plan::PlanStep {
            id: "stable".into(),
            text: "Finish work".into(),
            status: ego_core::plan::StepStatus::Blocked,
            reason: None,
        }],
    };
    titled_writer
        .writer
        .append(
            &titled,
            &lifecycle_event(
                3,
                EventKind::PlanUpdated {
                    update: inherited_plan.clone(),
                },
            ),
        )
        .unwrap();
    drop(titled_writer);
    let titled_child = futures::executor::block_on(service.fork(titled))
        .unwrap()
        .id;

    let untitled = service.create(workspace.clone()).unwrap().id;
    let untitled_store = JsonlStore::new(&sessions, &workspace, Box::new(OsIo));
    let untitled_metadata = SessionMetadata::new(Arc::new(untitled_store), Arc::new(FixedClock));
    let untitled_writer_store = JsonlStore::new(&sessions, &workspace, Box::new(OsIo));
    let untitled_writer = untitled_writer_store
        .open_writer(&untitled, lease())
        .unwrap();
    let untitled_goal = untitled_metadata
        .set_goal(
            &untitled_writer.writer,
            Seq::ZERO,
            "untitled objective".into(),
            GoalOwner::Operator,
            Ceilings::default(),
        )
        .unwrap();
    untitled_writer
        .writer
        .append(
            &untitled,
            &lifecycle_event(
                2,
                EventKind::PlanUpdated {
                    update: inherited_plan,
                },
            ),
        )
        .unwrap();
    drop(untitled_writer);
    let untitled_child = futures::executor::block_on(service.fork(untitled))
        .unwrap()
        .id;
    drop(service);

    let _reopened = make_service();
    let reopened_meta = SessionMetadata::new(
        Arc::new(JsonlStore::new(&sessions, &workspace, Box::new(OsIo))),
        Arc::new(FixedClock),
    );
    for (child, source_goal) in [(titled_child, titled_goal), (untitled_child, untitled_goal)] {
        let inherited = reopened_meta.goal(child).unwrap().unwrap();
        let plan = _reopened.plan(child).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].id, "stable");
        assert_eq!(plan.steps[0].status, ego_core::plan::StepStatus::Blocked);
        assert_ne!(inherited.id, source_goal.value.id);
        assert_eq!(inherited.derived_from, Some(source_goal.value.id));
        assert_eq!(inherited.objective, source_goal.value.objective);
        assert_eq!(inherited.owner, source_goal.value.owner);
        assert_eq!(inherited.ceilings, source_goal.value.ceilings);
    }
}

/// The hold events must survive the real store, not just a permissive double.
///
/// `compare_and_append_lifecycle_leased` whitelists the event kinds it will
/// admit, and a kind missing from that list is refused as an invalid lifecycle
/// append. The ACP fixture accepts every kind, so the whole `_ego/pause`
/// surface can be green against the double while no pause can be written to a
/// session on disk. This test is the one that would have said so.
#[test]
fn a_hold_and_its_release_survive_the_real_session_log() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let sessions = dir.path().join("sessions");
    let service = || {
        SessionLifecycle::new(
            Arc::new(JsonlStore::new(
                sessions.clone(),
                workspace.clone(),
                Box::new(OsIo),
            )),
            Arc::new(InactiveRuns),
            Arc::new(FixedClock),
        )
    };
    let held = RequestId::from_uuid(uuid::Uuid::from_u128(0x96));
    let released = RequestId::from_uuid(uuid::Uuid::from_u128(0x97));

    let id = service().create(workspace.clone()).unwrap().id;
    assert_eq!(
        futures::executor::block_on(service().pause(id, held)).unwrap(),
        HoldState::Paused,
        "pausing an idle session on the real store"
    );

    // A fresh service over the same directory: the boundary has to be read
    // back from the log, not remembered in the process that wrote it.
    let reopened = service();
    let entries = session_entries(
        &JsonlStore::new(sessions.clone(), workspace.clone(), Box::new(OsIo)),
        &id,
    );
    let kinds: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(&event.kind),
            _ => None,
        })
        .collect();
    assert!(
        matches!(kinds.as_slice(), [
            EventKind::SessionPauseRequested { request: asked },
            EventKind::SessionPaused { request: reached },
        ] if *asked == held && *reached == held),
        "the log holds the request/boundary pair naming the request that asked: {kinds:?}"
    );

    assert_eq!(
        futures::executor::block_on(reopened.release(id, released)).unwrap(),
        HoldState::Running,
        "releasing a hold the log alone proves"
    );
}

#[test]
fn lifecycle_log_visible_when_manifest_projection_rebuild_fails() {
    let dir = tempdir().unwrap();
    let control = Arc::new(CutControl::default());
    let store = cut_store(dir.path(), Arc::clone(&control));
    let id = SessionId::new();
    store
        .publish_empty_session(id, Path::new("/workspace"), Timestamp::from_millis(1))
        .unwrap();
    control.fail_manifest();
    let metadata_store = cut_store(dir.path(), Arc::clone(&control));
    let metadata = SessionMetadata::new(Arc::new(metadata_store), Arc::new(FixedClock));
    let writer_store = cut_store(dir.path(), control);
    let writer = writer_store.open_writer(&id, lease()).unwrap();
    let mutation = metadata
        .set_title(&writer.writer, Seq::ZERO, Some("new title".into()))
        .unwrap();
    let publication = mutation.publication.expect("publication");
    let debug = format!("{publication:?}");
    assert!(debug.contains("manifest cache update failed"));
    assert!(publication.is_visible());
    assert!(!publication.retry_safe());
}

/// Counts the bytes handed back for every `events.jsonl` read.
struct LogReadCounter {
    inner: OsIo,
    log_bytes: Arc<Mutex<usize>>,
    log_reads: Arc<Mutex<std::collections::HashMap<PathBuf, usize>>>,
}

impl StoreIo for LogReadCounter {
    fn file_revision(&self, path: &Path) -> io::Result<Option<ego_tools::store::FileRevision>> {
        self.inner.file_revision(path)
    }
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.inner.fsync(path)
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        let bytes = self.inner.read(path)?;
        if path.file_name().is_some_and(|name| name == "events.jsonl") {
            *self.log_bytes.lock().unwrap() += bytes.as_ref().map_or(0, Vec::len);
            *self
                .log_reads
                .lock()
                .unwrap()
                .entry(path.to_path_buf())
                .or_default() += 1;
        }
        Ok(bytes)
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

// Catches: session/list cost proportional to total event-log size instead of
// the manifest count, because deleted state is replayed from every log.
#[test]
fn listing_sessions_reads_no_event_log_bytes() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let log_bytes = Arc::new(Mutex::new(0));
    let service = SessionLifecycle::new(
        Arc::new(JsonlStore::new(
            &sessions,
            &workspace,
            Box::new(LogReadCounter {
                inner: OsIo,
                log_bytes: Arc::clone(&log_bytes),
                log_reads: Default::default(),
            }),
        )),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    let ids = (0..3)
        .map(|_| service.create(workspace.clone()).unwrap().id)
        .collect::<Vec<_>>();
    let big = "x".repeat(200_000);
    for id in &ids {
        let store = JsonlStore::new(&sessions, &workspace, Box::new(OsIo));
        store
            .append(
                id,
                &lifecycle_event(
                    1,
                    EventKind::UserPrompt {
                        audio: Vec::new(),
                        text: big.clone(),
                        images: Vec::new(),
                        queued_prompt: None,
                    },
                ),
            )
            .unwrap();
    }
    *log_bytes.lock().unwrap() = 0;

    let mut cursor = None;
    let mut listed = Vec::new();
    let mut pages = 0;
    loop {
        let page = service.list(cursor.as_ref(), 2, Some(&workspace)).unwrap();
        pages += 1;
        listed.extend(page.items.into_iter().map(|meta| meta.id));
        assert_eq!(
            *log_bytes.lock().unwrap(),
            0,
            "page {pages} replayed event logs"
        );
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
        assert!(pages < 3, "pagination did not terminate");
    }
    listed.sort();
    let mut expected = ids;
    expected.sort();
    assert_eq!(
        listed, expected,
        "pagination omitted or duplicated a session"
    );
    assert_eq!(pages, 2);
}

fn append_big_prompt(sessions: &Path, workspace: &Path, id: &SessionId) {
    JsonlStore::new(sessions, workspace, Box::new(OsIo))
        .append(
            id,
            &lifecycle_event(
                1,
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "x".repeat(200_000),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            ),
        )
        .unwrap();
}

// Catches: a listed page that replays each row's log for its hold state or its
// lineage, so a page costs the size of the logs it returns.
#[test]
fn a_listed_page_with_hold_and_lineage_reads_no_event_log_bytes() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let log_bytes = Arc::new(Mutex::new(0));
    let service = SessionLifecycle::new(
        Arc::new(JsonlStore::new(
            &sessions,
            &workspace,
            Box::new(LogReadCounter {
                inner: OsIo,
                log_bytes: Arc::clone(&log_bytes),
                log_reads: Default::default(),
            }),
        )),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    let plain = service.create(workspace.clone()).unwrap().id;
    let paused = service.create(workspace.clone()).unwrap().id;
    let source = service.create(workspace.clone()).unwrap().id;
    for id in [&plain, &paused, &source] {
        append_big_prompt(&sessions, &workspace, id);
    }
    let request = RequestId::from_uuid(uuid::Uuid::from_u128(0x1078));
    futures::executor::block_on(service.pause(paused, request)).unwrap();
    let child = futures::executor::block_on(service.fork(source))
        .unwrap()
        .id;
    let page_rows = || {
        let mut cursor = None;
        let mut rows = std::collections::HashMap::new();
        loop {
            let page = service.list(cursor.as_ref(), 2, Some(&workspace)).unwrap();
            for meta in &page.items {
                assert!(rows.insert(meta.id, page_row(&service, meta)).is_none());
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
            assert!(rows.len() < 4, "pagination did not terminate");
        }
        rows
    };
    // A first listing may derive and persist what an older manifest lacks.
    let first = page_rows();
    *log_bytes.lock().unwrap() = 0;

    let second = page_rows();

    assert_eq!(second, first);
    assert_eq!(second.len(), 4);
    assert_eq!(second[&plain].0, HoldState::Running);
    assert_eq!(second[&paused].0, HoldState::Paused);
    let child_lineage = second[&child].1.as_ref().expect("a fork has lineage");
    assert_eq!(child_lineage.root, source);
    assert!(!child_lineage.source_deleted);
    assert_eq!(*log_bytes.lock().unwrap(), 0, "a page replayed event logs");
}

// Catches: cold legacy siblings replay their shared ancestor repeatedly while
// rendering different cursor pages, despite the catalogue's bounded-read contract.
#[test]
fn legacy_lineage_listing_reads_each_log_once_across_pages() {
    bounded_lineage_listing(ManifestDamage::Legacy);
}

// Catches: catalogue fallback rereads missing-manifest logs before their row
// reaches the current page, and then reads them again to render that row.
#[test]
fn missing_manifest_lineage_listing_reads_each_log_once_across_pages() {
    bounded_lineage_listing(ManifestDamage::Missing);
}

// Catches: malformed cache recovery repeats full ancestry scans on each page.
#[test]
fn corrupt_manifest_lineage_listing_reads_each_log_once_across_pages() {
    bounded_lineage_listing(ManifestDamage::Corrupt);
}

enum ManifestDamage {
    Legacy,
    Missing,
    Corrupt,
}

fn bounded_lineage_listing(damage: ManifestDamage) {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let log_reads = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let service = SessionLifecycle::new(
        Arc::new(JsonlStore::new(
            &sessions,
            &workspace,
            Box::new(LogReadCounter {
                inner: OsIo,
                log_bytes: Default::default(),
                log_reads: Arc::clone(&log_reads),
            }),
        )),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    let source = service.create(workspace.clone()).unwrap().id;
    append_big_prompt(&sessions, &workspace, &source);
    let mut ids = vec![source];
    for _ in 0..2 {
        ids.push(
            futures::executor::block_on(service.fork(source))
                .unwrap()
                .id,
        );
    }
    for id in &ids {
        let path = sessions.join(id.to_string()).join("manifest.json");
        match damage {
            ManifestDamage::Legacy => {
                let mut manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let fields = manifest.as_object_mut().unwrap();
                fields.remove("pause");
                fields.remove("lineage_anchor");
                std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            }
            ManifestDamage::Missing => std::fs::remove_file(path).unwrap(),
            ManifestDamage::Corrupt => std::fs::write(path, b"{").unwrap(),
        }
    }
    // A fresh adapter makes this a cold catalogue, not a setup-warmed cache.
    let service = SessionLifecycle::new(
        Arc::new(JsonlStore::new(
            &sessions,
            &workspace,
            Box::new(LogReadCounter {
                inner: OsIo,
                log_bytes: Default::default(),
                log_reads: Arc::clone(&log_reads),
            }),
        )),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    log_reads.lock().unwrap().clear();

    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = service.list(cursor.as_ref(), 1, Some(&workspace)).unwrap();
        for meta in &page.items {
            let (hold, lineage) = service.listed_row(meta).unwrap();
            assert_eq!(hold, HoldState::Running);
            if meta.id != source {
                assert_eq!(lineage.unwrap().root, source);
            }
            seen.push(meta.id);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
        assert!(seen.len() < 3, "pagination did not terminate");
    }
    seen.sort();
    ids.sort();
    assert_eq!(seen, ids);
    let repeated: Vec<_> = log_reads
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(path, count)| (path.clone(), *count))
        .collect();
    assert!(
        repeated.is_empty(),
        "logs read repeatedly across pages: {repeated:?}"
    );
    log_reads.lock().unwrap().clear();
    for meta in service.list(None, 10, Some(&workspace)).unwrap().items {
        service.listed_row(&meta).unwrap();
    }
    assert!(
        log_reads.lock().unwrap().is_empty(),
        "warm catalogue replayed logs"
    );

    // A source deletion must remain visible even after the anchors are cached.
    futures::executor::block_on(service.delete(source)).unwrap();
    for meta in service.list(None, 10, Some(&workspace)).unwrap().items {
        let (_, lineage) = service.listed_row(&meta).unwrap();
        assert!(lineage.unwrap().source_deleted);
    }
}

// Catches: compact message-marker caches survive an external source append,
// returning an old cutoff marker; also checks that earlier cutoffs stay stable.
#[test]
fn catalogue_message_markers_follow_source_revision() {
    for supports_revisions in [true, false] {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let io: Box<dyn StoreIo> = if supports_revisions {
            Box::new(OsIo)
        } else {
            Box::new(RecordingIo::new(Default::default()))
        };
        let store = Arc::new(JsonlStore::new(dir.path().join("sessions"), &workspace, io));
        let service = SessionLifecycle::new(
            Arc::clone(&store),
            Arc::new(InactiveRuns),
            Arc::new(FixedClock),
        );
        let id = service.create(workspace).unwrap().id;
        let mut items = Vec::new();
        for seq in 1..=2 {
            let item = ego_core::event::id::ItemId::new();
            items.push(item);
            let mut event = lifecycle_event(
                seq,
                EventKind::UserPrompt {
                    text: format!("prompt {seq}"),
                    audio: Vec::new(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            );
            event.env.item = Some(item);
            store.append(&id, &event).unwrap();
            let cutoff = serde_json::from_value(serde_json::json!(seq)).unwrap();
            assert_eq!(store.listed_message_at(id, cutoff).unwrap(), Some(item));
        }
        let first = serde_json::from_value(serde_json::json!(1)).unwrap();
        assert_eq!(store.listed_message_at(id, first).unwrap(), Some(items[0]));
    }
}

// Catches: a missing-manifest fork adopts the source's latest model instead
// of the main-generation evidence visible at its own durable cutoff.
#[test]
fn missing_manifest_catalogue_preserves_models_at_distinct_fork_cutoffs() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = Arc::new(JsonlStore::new(&sessions, &workspace, Box::new(OsIo)));
    let service = SessionLifecycle::new(
        Arc::clone(&store),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    let source = service.create(workspace.clone()).unwrap().id;
    let invocation = ego_core::evidence::RunInvocationId::new();
    let evidence = ego_core::evidence::EvidenceAdmissionService::new();
    let mut children = Vec::new();
    for (seq, model) in [(1, "main/first"), (2, "main/second")] {
        store
            .append(
                &source,
                &lifecycle_event(
                    seq,
                    EventKind::UserPrompt {
                        text: format!("prompt {seq}"),
                        audio: Vec::new(),
                        images: Vec::new(),
                        queued_prompt: None,
                    },
                ),
            )
            .unwrap();
        let opened = store.open_writer(&source, lease()).unwrap();
        if seq == 1 {
            evidence
                .admit_run_context(
                    &opened.writer,
                    ego_core::evidence::RunContextEvidence::new(
                        source,
                        invocation,
                        ego_core::run::RunId::new(source, invocation.uuid()),
                        ego_core::evidence::ConfigProvenance::default(),
                    ),
                )
                .unwrap();
        }
        evidence
            .admit_generation(
                &opened.writer,
                super::usage_manifest::admission(
                    source,
                    invocation,
                    ego_core::event::domain::GenerationRole::Main,
                    model,
                ),
            )
            .unwrap();
        drop(opened);
        children.push(
            futures::executor::block_on(service.fork(source))
                .unwrap()
                .id,
        );
    }
    for id in [source, children[0], children[1]] {
        std::fs::remove_file(sessions.join(id.to_string()).join("manifest.json")).unwrap();
    }
    let reads = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let fresh = SessionLifecycle::new(
        Arc::new(JsonlStore::new(
            &sessions,
            &workspace,
            Box::new(LogReadCounter {
                inner: OsIo,
                log_bytes: Default::default(),
                log_reads: Arc::clone(&reads),
            }),
        )),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    let page = fresh.list(None, 10, Some(&workspace)).unwrap();
    assert_eq!(page.items.len(), 3);
    for meta in &page.items {
        let expected = if meta.id == children[0] {
            "main/first"
        } else {
            "main/second"
        };
        assert_eq!(meta.model.as_deref(), Some(expected));
        fresh.listed_row(meta).unwrap();
    }
    assert!(reads.lock().unwrap().values().all(|count| *count <= 1));
}

// Catches: optional lineage display hides a readable child after its source
// disappears, both before and after the lineage anchor has been cached.
#[test]
fn missing_ancestor_keeps_readable_catalogue_row() {
    for compaction in [false, true] {
        for cached_anchor in [false, true] {
            let dir = tempdir().unwrap();
            let sessions = dir.path().join("sessions");
            let workspace = dir.path().join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            let store = Arc::new(JsonlStore::new(&sessions, &workspace, Box::new(OsIo)));
            let service = SessionLifecycle::new(
                Arc::clone(&store),
                Arc::new(InactiveRuns),
                Arc::new(FixedClock),
            );
            let source = service.create(workspace.clone()).unwrap().id;
            let child = if compaction {
                let child = service.create(workspace.clone()).unwrap().id;
                store
                    .append(
                        &child,
                        &lifecycle_event(
                            1,
                            EventKind::CompactionSeeded {
                                source,
                                source_seq: Seq::ZERO,
                                prompt_revision: 1,
                                checkpoint: "Preserved local checkpoint".into(),
                                latest_user_request: None,
                                constraints: Default::default(),
                            },
                        ),
                    )
                    .unwrap();
                child
            } else {
                futures::executor::block_on(service.fork(source))
                    .unwrap()
                    .id
            };
            futures::executor::block_on(
                service.pause(child, RequestId::from_uuid(uuid::Uuid::from_u128(0x1079))),
            )
            .unwrap();
            let meta = service
                .list(None, 10, Some(&workspace))
                .unwrap()
                .items
                .into_iter()
                .find(|meta| meta.id == child)
                .unwrap();
            if cached_anchor {
                assert!(service.listed_row(&meta).unwrap().1.is_some());
            }
            let child_log = sessions.join(child.to_string()).join("events.jsonl");
            let before = std::fs::read(&child_log).unwrap();
            std::fs::remove_dir_all(sessions.join(source.to_string())).unwrap();

            let rows = service.list(None, 10, Some(&workspace)).unwrap();
            assert_eq!(rows.items.len(), 1);
            assert_eq!(rows.items[0].id, child);
            let (hold, lineage) = service.listed_row(&rows.items[0]).unwrap();
            assert_eq!(hold, HoldState::Paused);
            assert_eq!(
                lineage, None,
                "do not invent an unavailable ancestor's state"
            );
            assert_eq!(std::fs::read(&child_log).unwrap(), before);
            assert!(matches!(
                service.lineage_info(child),
                Err(ego_core::session_lifecycle::LifecycleError::UnknownSession { session, .. })
                    if session == source
            ));
        }
    }
}

fn page_row(
    service: &SessionLifecycle<JsonlStore>,
    meta: &ego_core::store::session_store::SessionMeta,
) -> (
    HoldState,
    Option<ego_core::session_lifecycle::SessionLineageInfo>,
) {
    service.listed_row(meta).unwrap()
}

// Catches: a manifest written before `pause` existed reads as Running (a held
// session listed as live), or is re-derived from the log on every listing.
#[test]
fn a_manifest_without_pause_is_completed_once_from_the_log() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let log_bytes = Arc::new(Mutex::new(0));
    let service = SessionLifecycle::new(
        Arc::new(JsonlStore::new(
            &sessions,
            &workspace,
            Box::new(LogReadCounter {
                inner: OsIo,
                log_bytes: Arc::clone(&log_bytes),
                log_reads: Default::default(),
            }),
        )),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    let id = service.create(workspace.clone()).unwrap().id;
    futures::executor::block_on(service.pause(id, RequestId::from_uuid(uuid::Uuid::from_u128(1))))
        .unwrap();
    let manifest_path = sessions.join(id.to_string()).join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert!(manifest.as_object_mut().unwrap().remove("pause").is_some());
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let meta = service
        .list(None, 1, Some(&workspace))
        .unwrap()
        .items
        .remove(0);

    *log_bytes.lock().unwrap() = 0;
    assert_eq!(service.listed_row(&meta).unwrap().0, HoldState::Paused);
    assert!(
        *log_bytes.lock().unwrap() > 0,
        "an old manifest is derived from the log"
    );
    assert!(
        std::fs::read_to_string(&manifest_path)
            .unwrap()
            .contains("\"pause\""),
        "the derived hold state is persisted"
    );

    *log_bytes.lock().unwrap() = 0;
    assert_eq!(service.listed_row(&meta).unwrap().0, HoldState::Paused);
    assert_eq!(
        *log_bytes.lock().unwrap(),
        0,
        "the second listing replays again"
    );
}

/// Counts every `manifest.json` read.
struct ManifestReadCounter {
    inner: OsIo,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl StoreIo for ManifestReadCounter {
    fn file_revision(&self, path: &Path) -> io::Result<Option<ego_tools::store::FileRevision>> {
        self.inner.file_revision(path)
    }
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.inner.fsync(path)
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        if path.file_name().is_some_and(|name| name == "manifest.json") {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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

type ReadCount = Arc<std::sync::atomic::AtomicUsize>;

/// A lifecycle over `sessions` whose manifest reads are counted.
fn counted_lifecycle(
    sessions: &Path,
    workspace: &Path,
) -> (SessionLifecycle<JsonlStore>, ReadCount) {
    let reads = ReadCount::default();
    let lifecycle = SessionLifecycle::new(
        Arc::new(JsonlStore::new(
            sessions,
            workspace,
            Box::new(ManifestReadCounter {
                inner: OsIo,
                reads: Arc::clone(&reads),
            }),
        )),
        Arc::new(InactiveRuns),
        Arc::new(FixedClock),
    );
    (lifecycle, reads)
}

fn reads_since(reads: &ReadCount) -> usize {
    reads.swap(0, std::sync::atomic::Ordering::Relaxed)
}

/// Every visible id of `workspace`, page by page, in listing order.
fn listed_ids(lifecycle: &SessionLifecycle<JsonlStore>, workspace: &Path) -> Vec<SessionId> {
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = lifecycle.list(cursor.as_ref(), 2, Some(workspace)).unwrap();
        ids.extend(page.items.iter().map(|meta| meta.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            return ids;
        }
    }
}

// Catches: session/list still opening every manifest of the store to fill one
// page, although the session table in history.sqlite orders and filters them.
#[test]
fn one_listed_page_reads_only_the_manifests_of_that_page() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&workspace).unwrap();
    let (lifecycle, reads) = counted_lifecycle(&sessions, &workspace);
    let mut ids = (0..8)
        .map(|_| lifecycle.create(workspace.clone()).unwrap().id)
        .collect::<Vec<_>>();
    for _ in 0..4 {
        lifecycle.create(elsewhere.clone()).unwrap();
    }
    // FixedClock gives every session one creation time, so ids break the tie.
    ids.sort();

    reads_since(&reads);
    let page = lifecycle.list(None, 2, Some(&workspace)).unwrap();
    assert_eq!(
        page.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        ids[..2]
    );
    assert_eq!(
        reads_since(&reads),
        3,
        "a page of two reads its two rows and the one that proves a next page"
    );
    assert_eq!(listed_ids(&lifecycle, &workspace), ids);
}

// Catches: a missing or damaged history.sqlite breaking or emptying the resume
// list, or a listing that never rebuilds the table and scans forever.
#[test]
fn a_missing_or_corrupt_session_table_lists_from_manifests_and_is_rebuilt() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (lifecycle, _) = counted_lifecycle(&sessions, &workspace);
    let ids = (0..5)
        .map(|_| lifecycle.create(workspace.clone()).unwrap().id)
        .collect::<Vec<_>>();
    futures::executor::block_on(lifecycle.delete(ids[2])).unwrap();
    let mut expected = ids.clone();
    expected.remove(2);
    expected.sort();
    assert_eq!(listed_ids(&lifecycle, &workspace), expected);

    let table = sessions.join("history.sqlite");
    for damage in [None, Some(b"not a database, only bytes".repeat(400))] {
        std::fs::remove_file(&table).unwrap();
        if let Some(bytes) = &damage {
            std::fs::write(&table, bytes).unwrap();
        }
        let (fresh, reads) = counted_lifecycle(&sessions, &workspace);
        assert_eq!(
            listed_ids(&fresh, &workspace),
            expected,
            "damaged={} changed the listed rows",
            damage.is_some()
        );
        assert!(table.exists(), "the session table was not rebuilt");
        reads_since(&reads);
        fresh.list(None, 2, Some(&workspace)).unwrap();
        assert_eq!(
            reads_since(&reads),
            3,
            "the rebuilt table did not serve the next page"
        );
    }
}

const DAY: u64 = 86_400_000;
const RETAIN: Duration = Duration::from_secs(30 * 86_400);
const NOW: u64 = 100 * DAY;

struct At(u64);

impl Clock for At {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.0)
    }
}

fn lifecycle_at(store: &Arc<JsonlStore>, at: u64) -> SessionLifecycle<JsonlStore> {
    SessionLifecycle::new(Arc::clone(store), Arc::new(InactiveRuns), Arc::new(At(at)))
}

/// Append a user prompt written at `at`.
fn prompt_at(store: &JsonlStore, id: SessionId, seq: u64, at: u64) {
    let mut event = lifecycle_event(
        seq,
        EventKind::UserPrompt {
            text: "retention marker".into(),
            audio: Vec::new(),
            images: Vec::new(),
            queued_prompt: None,
        },
    );
    event.env.ts = Timestamp::from_millis(at);
    event.env.item = Some(ego_core::event::id::ItemId::new());
    store.append(&id, &event).unwrap();
}

fn history_rows(sessions: &Path, id: SessionId) -> i64 {
    let connection = rusqlite::Connection::open(sessions.join("history.sqlite")).unwrap();
    [
        "sessions WHERE id",
        "sources WHERE session",
        "records WHERE session",
    ]
    .iter()
    .map(|from| {
        connection
            .query_row(
                &format!("SELECT count(*) FROM {from}=?1"),
                [id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    })
    .sum()
}

// Catches: retention that deletes a session opened 29 days ago, or that removes
// the session directory but leaves its rows in history.sqlite.
#[test]
fn retention_deletes_only_sessions_not_opened_for_the_period_with_their_rows() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = Arc::new(JsonlStore::new(&sessions, &workspace, Box::new(OsIo)));
    let mut ids = Vec::new();
    for age in [31, 29] {
        let at = NOW - age * DAY;
        let id = lifecycle_at(&store, at)
            .create(workspace.clone())
            .unwrap()
            .id;
        prompt_at(&store, id, 1, at);
        ids.push(id);
    }
    let (old, recent) = (ids[0], ids[1]);
    assert_eq!(store.history().matches("marker").unwrap().len(), 2);
    assert!(history_rows(&sessions, old) > 0);

    let report = store
        .prune_expired_sessions(RETAIN, Timestamp::from_millis(NOW), false)
        .unwrap()
        .expect("a requested pass runs");

    assert_eq!(report.deleted, vec![old]);
    assert!(report.freed_bytes > 0);
    assert!(!sessions.join(old.to_string()).exists());
    assert!(sessions.join(recent.to_string()).exists());
    assert_eq!(
        history_rows(&sessions, old),
        0,
        "rows of the deleted session remain"
    );
    assert!(history_rows(&sessions, recent) > 0);
    let listed = lifecycle_at(&store, NOW)
        .list(None, 10, Some(&workspace))
        .unwrap();
    assert_eq!(
        listed.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        [recent]
    );
}

// Catches: retention deleting a session in use: open by a writer, held, resumed
// recently, or the source a recent fork replays.
#[test]
fn retention_keeps_open_held_resumed_and_source_sessions() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = Arc::new(JsonlStore::new(&sessions, &workspace, Box::new(OsIo)));
    let old_at = NOW - 40 * DAY;
    let old = lifecycle_at(&store, old_at);
    let recent = lifecycle_at(&store, NOW - DAY);
    let create = || {
        let id = old.create(workspace.clone()).unwrap().id;
        prompt_at(&store, id, 1, old_at);
        id
    };
    let (open, held, resumed, source, unused) = (create(), create(), create(), create(), create());

    let _writer = store.open_writer(&open, lease()).unwrap();
    futures::executor::block_on(
        old.pause(held, RequestId::from_uuid(uuid::Uuid::from_u128(0x1087))),
    )
    .unwrap();
    futures::executor::block_on(recent.resume(resumed, &workspace)).unwrap();
    let fork = futures::executor::block_on(recent.fork(source)).unwrap().id;

    let report = store
        .prune_expired_sessions(RETAIN, Timestamp::from_millis(NOW), false)
        .unwrap()
        .expect("a requested pass runs");

    assert_eq!(
        report.deleted,
        vec![unused],
        "only the unused session expires"
    );
    assert_eq!(report.kept_in_use, 2, "the open and the held session");
    for id in [open, held, resumed, source, fork] {
        assert!(sessions.join(id.to_string()).exists(), "{id} was deleted");
    }
}
