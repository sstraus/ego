//! RED contract for the connection-neutral durable session lifecycle.
//!
//! Fault injection and run-state manipulation are retained by explicit test
//! handles. The lifecycle service has no test-only control surface and opens
//! no filesystem paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::run::{RunAppend, RunRevision, RunTransaction};
use ego_core::session::{
    ReplaySource, ReplaySourceError, SessionBlobSource, SessionLineage, TargetPublication,
    TargetSeed,
};
use ego_core::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, InactiveSessionLease, LifecycleAppendError,
    LifecycleError, LifecyclePublication, SessionActivity, SessionCursor, SessionLifecycle,
    SessionLifecycleStore, SessionRunLifecycle,
};
use ego_core::store::blob::{BlobError, BlobRef};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use futures::stream::{self, BoxStream};

#[derive(Default)]
struct DurableState {
    metadata: Vec<SessionMeta>,
    entries: Vec<(SessionId, SessionEntry)>,
}

#[derive(Clone, Default)]
struct FakeStore(Arc<Mutex<DurableState>>);

/// Explicit control retained by each test; it is never reachable from the
/// lifecycle service.
struct FakeStoreControl(FakeStore);

impl FakeStoreControl {
    fn append_event(&self, session: SessionId, event: DomainEvent) {
        self.0
            .0
            .lock()
            .unwrap()
            .entries
            .push((session, SessionEntry::Event(event)));
    }

    fn set_metadata(&self, metadata: Vec<SessionMeta>) {
        self.0.0.lock().unwrap().metadata = metadata;
    }

    fn push_metadata(&self, metadata: SessionMeta) {
        self.0.0.lock().unwrap().metadata.push(metadata);
    }

    fn remove_metadata(&self, id: SessionId) {
        self.0
            .0
            .lock()
            .unwrap()
            .metadata
            .retain(|meta| meta.id != id);
    }
}

impl SessionStore for FakeStore {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.0
            .lock()
            .unwrap()
            .entries
            .push((*id, SessionEntry::Event(event.clone())));
        Ok(())
    }
    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let entries = self
            .0
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|(id, _)| id == _id)
            .map(|(_, entry)| Ok(entry.clone()))
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(entries)))
    }
    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected: Option<RunRevision>,
        _transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        Err(StoreError::NotFound)
    }
    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(self.0.lock().unwrap().metadata.clone())
    }
}

impl SessionBlobSource for FakeStore {
    fn read(&self, _session: &SessionId, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Err(BlobError::NotFound {
            reference: reference.id.clone(),
        })
    }
}

struct FakeLease;
impl InactiveSessionLease for FakeLease {}

impl SessionLifecycleStore for FakeStore {
    fn publish_empty_session(
        &self,
        id: SessionId,
        root: &std::path::Path,
        created: Timestamp,
    ) -> Result<LifecyclePublication, StoreError> {
        self.0.lock().unwrap().metadata.push(SessionMeta {
            id,
            workspace_root: root.to_path_buf(),
            model: None,
            created,
            updated: created,
            title: None,
            lineage: None,
        });
        Ok(LifecyclePublication::Published)
    }
    fn compare_and_append_lifecycle(
        &self,
        id: SessionId,
        _expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        if let EventKind::SessionTitleChanged { title } = &event.kind
            && let Some(meta) = self
                .0
                .lock()
                .unwrap()
                .metadata
                .iter_mut()
                .find(|meta| meta.id == id)
        {
            meta.title = title.clone();
            meta.updated = event.env.ts;
        }
        self.append(&id, &event)
            .map_err(LifecycleAppendError::Store)?;
        Ok(LifecyclePublication::Published)
    }
    fn lifecycle_meta(&self, id: SessionId) -> Result<Option<SessionMeta>, StoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .metadata
            .iter()
            .find(|meta| meta.id == id)
            .cloned())
    }
    fn acquire_inactive_lease(
        &self,
        _id: SessionId,
    ) -> Result<Box<dyn InactiveSessionLease>, LifecycleAppendError> {
        Ok(Box::new(FakeLease))
    }
    fn publish_fork(&self, id: SessionId, seed: &TargetSeed) -> TargetPublication {
        let mut state = self.0.lock().unwrap();
        let source = match seed.lineage() {
            SessionLineage::Fork { source, .. } | SessionLineage::Compaction { source, .. } => {
                source
            }
        };
        if let Some(meta) = state
            .metadata
            .iter()
            .find(|meta| meta.id == source)
            .cloned()
        {
            state.metadata.push(SessionMeta {
                id,
                workspace_root: meta.workspace_root,
                model: meta.model,
                created: meta.created,
                updated: meta.updated,
                title: meta.title,
                lineage: Some(seed.lineage()),
            });
        }
        state.entries.extend(
            seed.events()
                .map(|event| (id, SessionEntry::Event(event.clone()))),
        );
        TargetPublication::PublishedDurably { diagnostic: None }
    }
}

impl ReplaySource for FakeStore {
    fn lineage(&self, _session: &SessionId) -> Result<Option<SessionLineage>, ReplaySourceError> {
        Ok(None)
    }
    fn entries(&self, session: &SessionId) -> Result<Option<Vec<SessionEntry>>, ReplaySourceError> {
        Ok(Some(
            self.0
                .lock()
                .unwrap()
                .entries
                .iter()
                .filter(|(id, _)| id == session)
                .map(|(_, e)| e.clone())
                .collect(),
        ))
    }
}

struct FakeRunControl {
    leased: Mutex<Vec<SessionId>>,
    active: Mutex<Vec<SessionId>>,
    uncertain: Mutex<Vec<SessionId>>,
}

impl FakeRunControl {
    fn hold_lease(&self, id: SessionId) {
        self.leased.lock().unwrap().push(id);
    }
    fn mark_active(&self, id: SessionId) {
        self.active.lock().unwrap().push(id);
    }
    fn mark_uncertain(&self, id: SessionId) {
        self.uncertain.lock().unwrap().push(id);
    }
}

impl SessionRunLifecycle for FakeRunControl {
    fn activity(&self, session: SessionId) -> ActivityFuture<'_> {
        let activity = if self.uncertain.lock().unwrap().contains(&session) {
            SessionActivity::Uncertain
        } else if self.active.lock().unwrap().contains(&session) {
            SessionActivity::Active
        } else if self.leased.lock().unwrap().contains(&session) {
            SessionActivity::LeaseHeld
        } else {
            SessionActivity::Inactive
        };
        Box::pin(std::future::ready(Ok(activity)))
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
        Timestamp::from_millis(83)
    }
}

fn service() -> (
    SessionLifecycle<FakeStore>,
    FakeStoreControl,
    Arc<FakeRunControl>,
) {
    let store = FakeStore::default();
    let control = FakeStoreControl(store.clone());
    let runs = Arc::new(FakeRunControl {
        leased: Mutex::new(Vec::new()),
        active: Mutex::new(Vec::new()),
        uncertain: Mutex::new(Vec::new()),
    });
    let run_port: Arc<dyn SessionRunLifecycle> = runs.clone();
    (
        SessionLifecycle::new(Arc::new(store), run_port, Arc::new(FixedClock)),
        control,
        runs,
    )
}

fn event(text: &str, seq: u64) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: serde_json::from_value(serde_json::json!(seq)).unwrap(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: Some(ItemId::new()),
            parent: None,
            ts: Timestamp::from_millis(seq),
            producer: Producer::Main,
        },
        kind: EventKind::UserPrompt {
            text: text.into(),
            images: Vec::new(),
            queued_prompt: None,
        },
    }
}

fn metadata(id: SessionId, root: &Path, created: u64) -> SessionMeta {
    SessionMeta {
        id,
        workspace_root: root.to_path_buf(),
        model: None,
        created: Timestamp::from_millis(created),
        updated: Timestamp::from_millis(created),
        title: None,
        lineage: None,
    }
}

#[test]
fn create_is_header_only_and_list_is_paginated_and_filter_bound() {
    let (service, _store, _runs) = service();
    let created = service.create(PathBuf::from("/workspace")).unwrap();
    service.create(PathBuf::from("/workspace")).unwrap();
    let workspace = PathBuf::from("/workspace");
    let page = service.list(None, 1, Some(&workspace)).unwrap();
    assert_eq!(page.items[0].id, created.id);
    assert_eq!(page.items[0].title, None);
    let cursor = page.next_cursor.expect("second page cursor");
    assert!(
        service
            .list(Some(&cursor), 1, Some(PathBuf::from("/other").as_path()))
            .is_err()
    );
    assert!(matches!(
        service.list(None, 0, Some(&workspace)),
        Err(LifecycleError::InvalidLimit)
    ));
}

#[test]
fn load_replays_events_while_resume_emits_no_history() {
    let (service, store, _runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    store.append_event(id, event("first", 1));
    store.append_event(id, event("second", 2));
    let loaded =
        futures::executor::block_on(service.load(id, PathBuf::from("/workspace").as_path()))
            .unwrap();
    assert_eq!(loaded.replay.len(), 2);
    let resumed =
        futures::executor::block_on(service.resume(id, PathBuf::from("/workspace").as_path()))
            .unwrap();
    assert!(resumed.replay.is_empty());
}

#[test]
fn corrupt_semantic_prefix_and_wrong_root_refuse_before_attachment() {
    let (service, store, _runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    store.append_event(id, event("first", 2));
    assert!(matches!(
        futures::executor::block_on(service.load(id, PathBuf::from("/workspace").as_path())),
        Err(LifecycleError::Replay(_))
    ));
    assert!(matches!(
        futures::executor::block_on(service.resume(id, PathBuf::from("/other").as_path())),
        Err(LifecycleError::RootMismatch { .. })
    ));
}

#[test]
fn fork_preserves_lineage_and_title_but_not_session_identity() {
    let (service, _store, _runs) = service();
    let source = service.create(PathBuf::from("/workspace")).unwrap();
    service.set_title(source.id, Some("named".into())).unwrap();
    let child = futures::executor::block_on(service.fork(source.id)).unwrap();
    assert_ne!(source.id, child.id);
    assert_eq!(
        service.meta(child.id).unwrap().title.as_deref(),
        Some("named")
    );
}

#[test]
fn delete_only_hides_target_and_keeps_ancestor_replay_and_descendants() {
    let (service, _store, _runs) = service();
    let root = service.create(PathBuf::from("/workspace")).unwrap().id;
    let child = futures::executor::block_on(service.fork(root)).unwrap().id;
    futures::executor::block_on(service.delete(root)).unwrap();
    futures::executor::block_on(service.delete(root)).unwrap();
    assert!(matches!(
        futures::executor::block_on(service.load(root, PathBuf::from("/workspace").as_path())),
        Err(LifecycleError::Deleted)
    ));
    assert!(
        futures::executor::block_on(service.resume(child, PathBuf::from("/workspace").as_path()))
            .is_ok()
    );
}

#[test]
fn lease_active_and_uncertain_controls_refuse_their_operations() {
    let (service, _store, runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    runs.hold_lease(id);
    assert!(matches!(
        futures::executor::block_on(service.delete(id)),
        Err(LifecycleError::LeaseHeld)
    ));
    runs.mark_active(id);
    assert!(matches!(
        futures::executor::block_on(service.fork(id)),
        Err(LifecycleError::Active)
    ));
    runs.mark_uncertain(id);
    assert!(matches!(
        futures::executor::block_on(service.close(id)),
        Err(LifecycleError::EffectUncertain)
    ));
}

#[test]
fn close_cancels_and_settles_before_the_lease_can_be_reused() {
    let (service, _store, _runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    let closed = futures::executor::block_on(service.close(id)).unwrap();
    assert!(closed.cancelled);
    assert!(
        futures::executor::block_on(service.attach(id, PathBuf::from("/workspace").as_path()))
            .is_ok()
    );
}

#[test]
fn restart_rebuilds_from_shared_durable_state() {
    let (service, store, _runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    drop(service);
    let run_port: Arc<dyn SessionRunLifecycle> = _runs.clone();
    let restarted =
        SessionLifecycle::new(Arc::new(store.0.clone()), run_port, Arc::new(FixedClock));
    assert_eq!(restarted.meta(id).unwrap().id, id);
}

#[test]
fn publication_uncertainty_is_visible_and_never_retryable() {
    let (service, _store, _runs) = service();
    let created = service.create(PathBuf::from("/workspace")).unwrap();
    assert!(created.publication.is_visible());
    assert!(!created.publication.retry_safe());
}

#[test]
fn keyset_cursor_orders_ties_and_survives_catalogue_changes() {
    let (service, control, _runs) = service();
    let root = PathBuf::from("/workspace");
    let ids = [
        SessionId::new(),
        SessionId::new(),
        SessionId::new(),
        SessionId::new(),
    ];
    control.set_metadata(
        ids.iter()
            .copied()
            .map(|id| metadata(id, &root, 100))
            .collect(),
    );
    let mut ordered = ids;
    ordered.sort();

    let first = service.list(None, 2, Some(&root)).unwrap();
    assert_eq!(
        first.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        ordered[..2]
    );
    let cursor = first.next_cursor.clone().expect("full page cursor");
    assert!(matches!(
        SessionCursor::parse("not-a-cursor"),
        Err(LifecycleError::InvalidCursor)
    ));
    assert!(matches!(
        service.list(Some(&cursor), 2, Some(&PathBuf::from("/other"))),
        Err(LifecycleError::InvalidCursor)
    ));

    control.push_metadata(metadata(SessionId::new(), &root, 101));
    let second = service.list(Some(&cursor), 2, Some(&root)).unwrap();
    assert_eq!(
        second.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        ordered[2..]
    );

    control.remove_metadata(first.items[1].id);
    let second_after_delete = service.list(Some(&cursor), 2, Some(&root)).unwrap();
    assert_eq!(
        second_after_delete
            .items
            .iter()
            .map(|meta| meta.id)
            .collect::<Vec<_>>(),
        ordered[2..]
    );
}

#[test]
fn list_fails_closed_when_a_catalogued_session_has_corrupt_replay() {
    let (service, control, _runs) = service();
    let root = PathBuf::from("/workspace");
    let id = SessionId::new();
    control.set_metadata(vec![metadata(id, &root, 100)]);
    control.append_event(id, event("corrupt", 2));

    assert!(matches!(
        service.list(None, 1, Some(&root)),
        Err(LifecycleError::Replay(_))
    ));
}
