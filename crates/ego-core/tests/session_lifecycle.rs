//! contract for the connection-neutral durable session lifecycle.
//!
//! Fault injection and run-state manipulation are retained by explicit test
//! handles. The lifecycle service has no test-only control surface and opens
//! no filesystem paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ego_core::config::ModelRef;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, GenerationRole};
use ego_core::event::id::{EventId, GenerationId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{
    EvidenceLogSource, EvidenceRecord, GenerationAdmissionEvidence, GenerationTarget,
    ManifestEvidenceError, PricingEvidence, RunInvocationId,
};
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::goal::{GoalOwner, GoalRefusal, GoalState, GoalTransition};
use ego_core::run::{RequestId, RunAppend, RunRevision, RunTransaction};
use ego_core::session::{
    ReplaySource, ReplaySourceError, SessionBlobSource, SessionLineage, TargetPublication,
    TargetSeed,
};
use ego_core::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, HoldSignalFuture, HoldState, InactiveSessionLease,
    LeasedLifecycleAppend, LifecycleAppendError, LifecycleError, LifecyclePublication,
    SessionActivity, SessionCursor, SessionLifecycle, SessionLifecycleStore, SessionMetadata,
    SessionRunLifecycle,
};
use ego_core::store::blob::{BlobError, BlobRef};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use futures::stream::{self, BoxStream};

#[derive(Default)]
struct DurableState {
    metadata: Vec<SessionMeta>,
    entries: Vec<(SessionId, SessionEntry)>,
    fail_meta: bool,
    inject_meta_failure: bool,
    quarantined: Vec<SessionId>,
}

#[derive(Clone, Default)]
struct FakeStore(Arc<Mutex<DurableState>>);

/// Explicit control retained by each test; it is never reachable from the
/// lifecycle service.
struct FakeStoreControl(FakeStore);

impl FakeStoreControl {
    fn event_count(&self, session: SessionId) -> usize {
        self.0
            .0
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|(id, _)| *id == session)
            .count()
    }
    fn append_event(&self, session: SessionId, event: DomainEvent) {
        self.0
            .0
            .lock()
            .unwrap()
            .entries
            .push((session, SessionEntry::Event(event)));
    }

    fn append_evidence(&self, session: SessionId, evidence: EvidenceRecord) {
        self.0
            .0
            .lock()
            .unwrap()
            .entries
            .push((session, SessionEntry::Evidence(evidence)));
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
    fn fail_metadata_readback(&self) {
        self.0.0.lock().unwrap().inject_meta_failure = true;
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

impl EvidenceLogSource for FakeStore {
    fn evidence_records(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<EvidenceRecord>, ManifestEvidenceError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter_map(|(id, entry)| match entry {
                SessionEntry::Evidence(evidence) if id == _session => Some(evidence.clone()),
                SessionEntry::Event(_)
                | SessionEntry::RunTransaction(_)
                | SessionEntry::Evidence(_)
                | SessionEntry::NoticeDelivered(_)
                | SessionEntry::ModelSelected(_) => None,
            })
            .collect())
    }
}

impl SessionBlobSource for FakeStore {
    fn read(&self, _session: &SessionId, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Err(BlobError::NotFound {
            reference: reference.id.clone(),
        })
    }
}

struct FakeLease {
    session: SessionId,
    store: FakeStore,
}

impl LeasedLifecycleAppend for FakeLease {
    fn session_id(&self) -> SessionId {
        self.session
    }
    fn compare_and_append_lifecycle(
        &self,
        _expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        if let EventKind::SessionTitleChanged { title } = &event.kind
            && let Some(meta) = self
                .store
                .0
                .lock()
                .unwrap()
                .metadata
                .iter_mut()
                .find(|meta| meta.id == self.session)
        {
            meta.title = title.clone();
        }
        let result = self
            .store
            .append(&self.session, &event)
            .map(|_| LifecyclePublication::Published)
            .map_err(LifecycleAppendError::Store);
        let mut state = self.store.0.lock().unwrap();
        if state.inject_meta_failure {
            state.fail_meta = true;
        }
        result
    }
}
impl InactiveSessionLease for FakeLease {}

impl SessionLifecycleStore for FakeStore {
    fn publish_session(
        &self,
        id: SessionId,
        root: &std::path::Path,
        created: Timestamp,
        profiles: ego_core::store::session_store::SessionProfiles,
    ) -> Result<LifecyclePublication, StoreError> {
        self.0.lock().unwrap().metadata.push(SessionMeta {
            profiles,
            id,
            workspace_root: root.to_path_buf(),
            model: None,
            created,
            updated: created,
            title: None,
            summary: None,
            summary_failures: 0,
            lineage: None,
        });
        Ok(LifecyclePublication::Published)
    }
    fn lifecycle_meta(&self, id: SessionId) -> Result<Option<SessionMeta>, StoreError> {
        if self.0.lock().unwrap().fail_meta {
            return Err(StoreError::NotFound);
        }
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
        Ok(Box::new(FakeLease {
            session: _id,
            store: self.clone(),
        }))
    }
    fn quarantine(&self, id: SessionId) -> Result<(), StoreError> {
        self.0.lock().unwrap().quarantined.push(id);
        Ok(())
    }
    fn is_quarantined(&self, id: SessionId) -> Result<bool, StoreError> {
        Ok(self.0.lock().unwrap().quarantined.contains(&id))
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
                profiles: Default::default(),
                id,
                workspace_root: meta.workspace_root,
                model: meta.model,
                created: meta.created,
                updated: meta.updated,
                title: meta.title,
                summary: meta.summary,
                summary_failures: meta.summary_failures,
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
    fn lineage(&self, session: &SessionId) -> Result<Option<SessionLineage>, ReplaySourceError> {
        Ok(ego_core::state::SessionState::replay(
            self.entries(session)?
                .unwrap_or_default()
                .into_iter()
                .filter_map(|entry| match entry {
                    SessionEntry::Event(event) => Some(event),
                    _ => None,
                }),
        )
        .lineage)
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
    /// Sessions whose turn is live enough to receive a hold.
    live: Mutex<Vec<SessionId>>,
    /// Every hold this double was asked to deliver, in order. The point of
    /// recording them is that a request already answered must produce no
    /// entry at all.
    delivered: Mutex<Vec<(SessionId, RequestId)>>,
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
    fn mark_live(&self, id: SessionId) {
        self.live.lock().unwrap().push(id);
    }
    fn delivered(&self) -> Vec<(SessionId, RequestId)> {
        self.delivered.lock().unwrap().clone()
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

    /// `false` unless the session was marked live: with no live run a pause
    /// has no turn to reach and the caller writes the boundary itself.
    fn hold(&self, session: SessionId, request: RequestId) -> HoldSignalFuture<'_> {
        self.delivered.lock().unwrap().push((session, request));
        let live = self.live.lock().unwrap().contains(&session);
        Box::pin(std::future::ready(Ok(live)))
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
        live: Mutex::new(Vec::new()),
        delivered: Mutex::new(Vec::new()),
    });
    let run_port: Arc<dyn SessionRunLifecycle> = runs.clone();
    (
        SessionLifecycle::new(Arc::new(store), run_port, Arc::new(FixedClock)),
        control,
        runs,
    )
}

fn metadata_fixture(control: &FakeStoreControl) -> SessionMetadata<FakeStore> {
    SessionMetadata::new(Arc::new(control.0.clone()), Arc::new(FixedClock))
}

fn lease(control: &FakeStoreControl, session: SessionId) -> FakeLease {
    FakeLease {
        session,
        store: control.0.clone(),
    }
}

fn expected(control: &FakeStoreControl, session: SessionId) -> Seq {
    let mut seq = Seq::ZERO;
    for _ in 0..control.event_count(session) {
        seq.next();
    }
    seq
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
            audio: Vec::new(),
            text: text.into(),
            images: Vec::new(),
            queued_prompt: None,
        },
    }
}

fn generation_evidence(session: SessionId, context_window: u64) -> EvidenceRecord {
    let capabilities = ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(context_window),
        max_output_tokens: None,
        max_tools: None,
        supports_parallel_tool_calls: false,
        reasoning_carryover: ReasoningCarryover::Unsupported,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Unsupported,
        cache: CacheRetention::None,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    };
    let invocation = RunInvocationId::new();
    EvidenceRecord::GenerationAdmission(GenerationAdmissionEvidence::new(
        session,
        invocation,
        AttemptRef::first(GenerationId::new()),
        GenerationRole::Main,
        GenerationTarget::new(
            "provider",
            ModelRef("provider/model".to_owned()),
            "wire-model",
        ),
        capabilities,
        PricingEvidence::Unpriced {
            source: "fixture".to_owned(),
            reason: "not relevant".to_owned(),
        },
    ))
}

fn metadata(id: SessionId, root: &Path, created: u64) -> SessionMeta {
    SessionMeta {
        profiles: Default::default(),
        id,
        workspace_root: root.to_path_buf(),
        model: None,
        created: Timestamp::from_millis(created),
        updated: Timestamp::from_millis(created),
        title: None,
        summary: None,
        summary_failures: 0,
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
    assert_eq!(loaded.meta.id, id);
    assert_eq!(loaded.context_window, None);
    let resumed =
        futures::executor::block_on(service.resume(id, PathBuf::from("/workspace").as_path()))
            .unwrap();
    assert!(resumed.replay.is_empty());
}

#[test]
fn attachment_reports_the_latest_durable_generation_context_window() {
    let (service, store, _runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    store.append_evidence(id, generation_evidence(id, 8_192));
    store.append_evidence(id, generation_evidence(id, 200_000));

    let attachment =
        futures::executor::block_on(service.resume(id, Path::new("/workspace"))).unwrap();

    assert_eq!(attachment.context_window, Some(200_000));
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
    let (service, store, _runs) = service();
    let meta = metadata_fixture(&store);
    let source = service.create(PathBuf::from("/workspace")).unwrap();
    meta.set_title(
        &lease(&store, source.id),
        expected(&store, source.id),
        Some("named".into()),
    )
    .unwrap();
    let source_goal = meta
        .set_goal(
            &lease(&store, source.id),
            expected(&store, source.id),
            "active objective".into(),
            GoalOwner::Operator,
            ego_core::ceiling::Ceilings::default(),
        )
        .unwrap();
    let child = futures::executor::block_on(service.fork(source.id)).unwrap();
    assert_ne!(source.id, child.id);
    assert_eq!(meta.meta(child.id).unwrap().title.as_deref(), Some("named"));
    let child_goal = meta.goal(child.id).unwrap().unwrap();
    assert_ne!(child_goal.id, source_goal.value.id);
    assert_eq!(child_goal.derived_from, Some(source_goal.value.id));
    assert_eq!(child_goal.objective, source_goal.value.objective);
    assert_eq!(child_goal.owner, source_goal.value.owner);
    assert_eq!(child_goal.ceilings, source_goal.value.ceilings);
    assert_eq!(child_goal.state, GoalState::Active);
    assert_eq!(meta.goal(source.id).unwrap().unwrap(), source_goal.value);
}

#[test]
fn fork_does_not_copy_terminal_goal() {
    let (service, store, _runs) = service();
    let meta = metadata_fixture(&store);
    let source = service.create(PathBuf::from("/workspace")).unwrap();
    meta.set_goal(
        &lease(&store, source.id),
        expected(&store, source.id),
        "completed objective".into(),
        GoalOwner::Operator,
        ego_core::ceiling::Ceilings::default(),
    )
    .unwrap();
    meta.transition_goal(
        &lease(&store, source.id),
        expected(&store, source.id),
        GoalTransition::Achieved {
            attested_by: GoalOwner::Operator,
            evidence: None,
        },
    )
    .unwrap();
    let child = futures::executor::block_on(service.fork(source.id)).unwrap();
    assert_eq!(meta.goal(child.id).unwrap(), None);
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

/// Catches a quarantine that hides a session from `list` only, or only until
/// the next connection: every way to reach it must answer unknown, and its
/// neighbours must stay visible.
#[test]
fn a_quarantined_session_is_unknown_to_every_operation_and_its_neighbours_stay() {
    let (service, _store, _runs) = service();
    let root = PathBuf::from("/workspace");
    let refused = service.create(root.clone()).unwrap().id;
    let kept = service.create(root.clone()).unwrap().id;
    service.quarantine(refused).unwrap();
    service.quarantine(refused).unwrap();

    let listed = service.list(None, 10, Some(&root)).unwrap();
    assert_eq!(
        listed.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        vec![kept]
    );
    assert!(matches!(
        service.meta(refused),
        Err(LifecycleError::UnknownSession)
    ));
    assert!(matches!(
        futures::executor::block_on(service.load(refused, &root)),
        Err(LifecycleError::UnknownSession)
    ));
    assert!(matches!(
        futures::executor::block_on(service.resume(refused, &root)),
        Err(LifecycleError::UnknownSession)
    ));
    assert!(matches!(
        futures::executor::block_on(service.fork(refused)),
        Err(LifecycleError::UnknownSession)
    ));
    assert!(matches!(
        service.hold(refused),
        Err(LifecycleError::UnknownSession)
    ));
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
    // Uncertain evidence remains protected from deletion; close may abandon it.
    assert!(matches!(
        futures::executor::block_on(service.delete(id)),
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
    let _restarted =
        SessionLifecycle::new(Arc::new(store.0.clone()), run_port, Arc::new(FixedClock));
    assert_eq!(metadata_fixture(&store).meta(id).unwrap().id, id);
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

#[test]
fn lifecycle_goal_operations_replay_and_refuse_invalid_transitions_without_append() {
    let (service, control, _runs) = service();
    let meta = metadata_fixture(&control);
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    assert_eq!(meta.goal(id).unwrap(), None);
    let ceilings = ego_core::ceiling::Ceilings::default();
    meta.set_goal(
        &lease(&control, id),
        expected(&control, id),
        "  Ship  the   product  ".into(),
        GoalOwner::Operator,
        ceilings.clone(),
    )
    .unwrap();
    let goal = meta.goal(id).unwrap().unwrap();
    assert_eq!(goal.objective, "Ship  the   product");
    assert_eq!(goal.state, GoalState::Active);
    let before = control.event_count(id);
    assert!(matches!(
        meta.set_goal(
            &lease(&control, id),
            expected(&control, id),
            "second".into(),
            GoalOwner::Operator,
            ceilings.clone()
        ),
        Err(LifecycleError::Goal(_))
    ));
    assert_eq!(control.event_count(id), before);
    meta.transition_goal(
        &lease(&control, id),
        expected(&control, id),
        GoalTransition::Achieved {
            attested_by: GoalOwner::Operator,
            evidence: None,
        },
    )
    .unwrap();
    assert_eq!(meta.goal(id).unwrap().unwrap().state, GoalState::Achieved);
    let terminal = meta.goal(id).unwrap().unwrap().id;
    let before = control.event_count(id);
    assert!(matches!(
        meta.transition_goal(
            &lease(&control, id),
            expected(&control, id),
            GoalTransition::BlockerObserved {
                reason: "late".into()
            }
        ),
        Err(LifecycleError::Goal(GoalRefusal::AlreadyTerminal(
            GoalState::Achieved
        )))
    ));
    assert_eq!(control.event_count(id), before);
    let replacement = meta
        .set_goal(
            &lease(&control, id),
            expected(&control, id),
            "new objective".into(),
            GoalOwner::Operator,
            ceilings,
        )
        .unwrap();
    assert_ne!(replacement.value.id, terminal);
    assert_eq!(replacement.value.derived_from, None);
    let achieved = meta
        .transition_goal(
            &lease(&control, id),
            expected(&control, id),
            GoalTransition::Achieved {
                attested_by: GoalOwner::Operator,
                evidence: None,
            },
        )
        .unwrap();
    assert_eq!(achieved.value.state, GoalState::Achieved);
}

#[test]
fn lifecycle_goal_requires_active_goal_for_transition() {
    let (service, control, _runs) = service();
    let meta = metadata_fixture(&control);
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    let before = control.event_count(id);
    assert!(matches!(
        meta.transition_goal(
            &lease(&control, id),
            expected(&control, id),
            GoalTransition::BlockerObserved {
                reason: "blocked".into()
            }
        ),
        Err(LifecycleError::Goal(_))
    ));
    assert_eq!(control.event_count(id), before);
}

#[test]
fn operator_text_is_trimmed_bounded_and_shared_by_title_goal_and_blocker() {
    let (lifecycle, control, _runs) = service();
    let meta = metadata_fixture(&control);
    let id = lifecycle.create(PathBuf::from("/workspace")).unwrap().id;
    let accepted = "\u{2003}Release  Candidate\u{2003}";
    meta.set_title(
        &lease(&control, id),
        expected(&control, id),
        Some(accepted.into()),
    )
    .unwrap();
    assert_eq!(
        meta.meta(id).unwrap().title.as_deref(),
        Some("Release  Candidate")
    );
    let goal = meta
        .set_goal(
            &lease(&control, id),
            expected(&control, id),
            format!("\u{2003}{}\u{2003}", "x".repeat(1024)),
            GoalOwner::Operator,
            ego_core::ceiling::Ceilings::default(),
        )
        .unwrap();
    assert_eq!(goal.value.objective.len(), 1024);
    for invalid in ["", " \u{2003} ", "bad\ntext", &"x".repeat(1025)] {
        let (fresh, fresh_control, _runs) = service();
        let fresh_id = fresh.create(PathBuf::from("/workspace")).unwrap().id;
        let before = fresh_control.event_count(fresh_id);
        let fresh_meta = metadata_fixture(&fresh_control);
        assert!(matches!(
            fresh_meta.set_title(
                &lease(&fresh_control, fresh_id),
                expected(&fresh_control, fresh_id),
                Some((*invalid).into())
            ),
            Err(LifecycleError::InvalidText)
        ));
        assert_eq!(fresh_control.event_count(fresh_id), before);
        assert!(matches!(
            fresh_meta.set_goal(
                &lease(&fresh_control, fresh_id),
                expected(&fresh_control, fresh_id),
                (*invalid).into(),
                GoalOwner::Operator,
                ego_core::ceiling::Ceilings::default()
            ),
            Err(LifecycleError::InvalidText)
        ));
        assert_eq!(fresh_control.event_count(fresh_id), before);
    }
    let before = control.event_count(id);
    assert!(matches!(
        meta.transition_goal(
            &lease(&control, id),
            expected(&control, id),
            GoalTransition::BlockerObserved {
                reason: "bad\ntext".into()
            }
        ),
        Err(LifecycleError::InvalidText)
    ));
    assert_eq!(control.event_count(id), before);
    meta.transition_goal(
        &lease(&control, id),
        expected(&control, id),
        GoalTransition::BlockerObserved {
            reason: " API   unavailable ".into(),
        },
    )
    .unwrap();
    meta.transition_goal(
        &lease(&control, id),
        expected(&control, id),
        GoalTransition::BlockerObserved {
            reason: "api unavailable".into(),
        },
    )
    .unwrap();
    let goal = meta.goal(id).unwrap().unwrap();
    assert_eq!(goal.blocker_reason.as_deref(), Some("API   unavailable"));
    assert_eq!(goal.blocker_observations, 2);
}

#[test]
fn visible_metadata_mutation_returns_prepared_value_without_readback() {
    let (service, control, _runs) = service();
    let metadata = metadata_fixture(&control);
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    control.fail_metadata_readback();
    let mutation = metadata
        .set_title(
            &lease(&control, id),
            expected(&control, id),
            Some("named".into()),
        )
        .expect("visible append returns prepared value");
    assert_eq!(mutation.value.title.as_deref(), Some("named"));
    assert_eq!(control.event_count(id), 1);
}

/// A re-delivered `_ego/pause` is answered from its receipt and never reaches
/// the turn a second time.
///
/// The dangerous variant of a delayed retry. The idle path's own guard is
/// enough while the session stays idle, but by the time a duplicate arrives
/// the session may be running again — and then the request would be delivered
/// to a live turn and stop work nobody asked to stop, on the strength of one
/// request the host made once. The receipt is therefore read *before* the run
/// is signalled, not merely before the log is written.
#[test]
fn a_redelivered_pause_is_never_delivered_to_a_live_turn() {
    let (service, _store, runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    let held = RequestId::from_uuid(uuid::Uuid::from_u128(0x91));
    let released = RequestId::from_uuid(uuid::Uuid::from_u128(0x92));

    assert_eq!(
        futures::executor::block_on(service.pause(id, held)).unwrap(),
        HoldState::Paused
    );
    assert_eq!(
        futures::executor::block_on(service.release(id, released)).unwrap(),
        HoldState::Running
    );
    // The session is given work again, so a duplicate now has a turn it could
    // reach. This is what makes the receipt load-bearing.
    runs.mark_live(id);

    let replayed = futures::executor::block_on(service.pause(id, held)).unwrap();

    assert_eq!(
        replayed,
        HoldState::Paused,
        "the duplicate was acted on instead of replayed"
    );
    assert_eq!(
        runs.delivered(),
        vec![(id, held)],
        "the duplicate reached the live turn: {:?}",
        runs.delivered()
    );
}

/// A re-delivered `_ego/resume` is answered from its receipt even after the
/// session has been given more work.
///
/// The symmetric case, and the one the lease cannot serve. By the time a lost
/// reply is re-sent the session may be running again, and taking an inactive
/// lease to answer would refuse with "active work" — telling the host its
/// release failed when the release is precisely why the session is running.
#[test]
fn a_redelivered_resume_is_answered_without_taking_a_lease() {
    let (service, _store, runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    let held = RequestId::from_uuid(uuid::Uuid::from_u128(0x93));
    let released = RequestId::from_uuid(uuid::Uuid::from_u128(0x94));
    futures::executor::block_on(service.pause(id, held)).unwrap();
    futures::executor::block_on(service.release(id, released)).unwrap();
    runs.mark_active(id);

    let replayed = futures::executor::block_on(service.release(id, released));

    assert_eq!(
        replayed.unwrap(),
        HoldState::Running,
        "the retry was refused as active work instead of replayed"
    );
}

/// A hold whose boundary never arrived can still be released.
///
/// The two pause events are two appends, not one record, so a process that
/// dies between them leaves `SessionPauseRequested` standing alone. That state
/// refuses new work — which is right, the operator asked for a stop — but the
/// writer that would have finished the pair is gone, so the boundary will
/// never arrive. Refusing to release it would wedge the session permanently:
/// too paused to run, not paused enough to resume.
///
/// A live turn is a different matter and is refused earlier, by the inactive
/// lease this path takes before it looks at the hold at all.
#[test]
fn a_hold_whose_boundary_never_arrived_is_still_releasable() {
    let (service, store, _runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    let interrupted = RequestId::from_uuid(uuid::Uuid::from_u128(0x95));
    store.append_event(
        id,
        DomainEvent {
            env: Envelope {
                seq: serde_json::from_value(serde_json::json!(1)).unwrap(),
                id: EventId::new(),
                turn: TurnId::new(),
                item: None,
                parent: None,
                ts: Timestamp::from_millis(1),
                producer: Producer::Main,
            },
            kind: EventKind::SessionPauseRequested {
                request: interrupted,
            },
        },
    );
    assert_eq!(service.hold(id).unwrap(), HoldState::Pending);

    let released = futures::executor::block_on(
        service.release(id, RequestId::from_uuid(uuid::Uuid::from_u128(0x96))),
    );

    assert_eq!(
        released.unwrap(),
        HoldState::Running,
        "an interrupted pause left the session with no way out"
    );
    assert_eq!(service.hold(id).unwrap(), HoldState::Running);
}

// regression case: cut after the whole answered turn, never at item creation.
fn fork_turn(
    store: &FakeStoreControl,
    source: SessionId,
    text: &str,
    ok: Option<bool>,
) -> (ItemId, ItemId, Seq) {
    let mut prompt = event(text, store.event_count(source) as u64 + 1);
    let user = prompt.env.item.unwrap();
    let turn = prompt.env.turn;
    store.append_event(source, prompt.clone());
    prompt.env.seq.next();
    prompt.env.id = EventId::new();
    let assistant = ItemId::new();
    prompt.env.item = Some(assistant);
    prompt.kind = EventKind::AssistantMessage {
        text: format!("answer {text}"),
    };
    store.append_event(source, prompt.clone());
    if let Some(ok) = ok {
        prompt.env.seq.next();
        prompt.env.id = EventId::new();
        prompt.env.item = None;
        prompt.env.turn = turn;
        prompt.kind = EventKind::TurnCompleted {
            ok,
            exit_reason: ego_core::event::domain::ExitReason::Completed,
        };
        store.append_event(source, prompt.clone());
    }
    (user, assistant, prompt.env.seq)
}

#[test]
fn fork_at_user_and_assistant_of_turn_two_preserves_the_full_answer_not_later_turns() {
    let (service, store, _) = service();
    let source = service.create(PathBuf::from("/workspace")).unwrap().id;
    fork_turn(&store, source, "one", Some(true));
    let (user, assistant, cutoff) = fork_turn(&store, source, "two", Some(true));
    fork_turn(&store, source, "three", Some(true));
    fork_turn(&store, source, "four", Some(true));
    for item in [user, assistant] {
        let child = futures::executor::block_on(service.fork_at(source, item))
            .unwrap()
            .id;
        assert_eq!(
            fork_state(&store, child).lineage,
            Some(SessionLineage::Fork {
                source,
                at_seq: cutoff
            })
        );
        let replay = ego_core::session::ReplayContext::new(&store.0, &store.0)
            .replay(&child)
            .unwrap();
        let bytes = serde_json::to_string(replay.conversation()).unwrap();
        assert!(bytes.contains("answer two"));
        assert!(!bytes.contains("three"));
        assert!(!bytes.contains("four"));
    }
}

#[test]
fn fork_at_incomplete_failed_and_foreign_items_refuses_without_publishing_a_target() {
    use ego_core::session::ForkError;
    let (service, store, _) = service();
    let source = service.create(PathBuf::from("/workspace")).unwrap().id;
    let other = service.create(PathBuf::from("/workspace")).unwrap().id;
    let (_, foreign, _) = fork_turn(&store, other, "foreign", Some(true));
    let (_, failed, _) = fork_turn(&store, source, "failed", Some(false));
    let (_, incomplete, _) = fork_turn(&store, source, "incomplete", None);
    let before = store.0.0.lock().unwrap().metadata.len();
    for (item, expected) in [
        (failed, ForkError::UnsettledTurn),
        (incomplete, ForkError::UnsettledTurn),
        (foreign, ForkError::UnknownItem),
    ] {
        assert!(
            matches!(futures::executor::block_on(service.fork_at(source, item)), Err(LifecycleError::Fork(error)) if error == expected)
        );
    }
    assert_eq!(store.0.0.lock().unwrap().metadata.len(), before);
}

#[test]
fn fork_at_ancestor_uses_ancestor_identity_and_rejects_items_beyond_inherited_cutoff() {
    let (service, store, _) = service();
    let source = service.create(PathBuf::from("/workspace")).unwrap().id;
    let (_, item, cutoff) = fork_turn(&store, source, "one", Some(true));
    let child = futures::executor::block_on(service.fork_at(source, item))
        .unwrap()
        .id;
    let (_, hidden, _) = fork_turn(&store, source, "later", Some(true));
    let sibling = futures::executor::block_on(service.fork_at(child, item))
        .unwrap()
        .id;
    assert_eq!(
        fork_state(&store, sibling).lineage,
        Some(SessionLineage::Fork {
            source,
            at_seq: cutoff
        })
    );
    assert!(matches!(
        futures::executor::block_on(service.fork_at(child, hidden)),
        Err(LifecycleError::Fork(
            ego_core::session::ForkError::UnknownItem
        ))
    ));
}

#[test]
fn fork_at_uses_cutoff_title_and_goal_instead_of_current_metadata() {
    let (service, store, _) = service();
    let source = service.create(PathBuf::from("/workspace")).unwrap().id;
    let meta = metadata_fixture(&store);
    meta.set_title(
        &lease(&store, source),
        expected(&store, source),
        Some("old title".into()),
    )
    .unwrap();
    let goal = meta
        .set_goal(
            &lease(&store, source),
            expected(&store, source),
            "old objective".into(),
            GoalOwner::Operator,
            Default::default(),
        )
        .unwrap();
    let (_, item, _) = fork_turn(&store, source, "one", Some(true));
    meta.set_title(
        &lease(&store, source),
        expected(&store, source),
        Some("new title".into()),
    )
    .unwrap();
    meta.transition_goal(
        &lease(&store, source),
        expected(&store, source),
        GoalTransition::Achieved {
            attested_by: GoalOwner::Operator,
            evidence: None,
        },
    )
    .unwrap();
    meta.set_goal(
        &lease(&store, source),
        expected(&store, source),
        "new objective".into(),
        GoalOwner::Operator,
        Default::default(),
    )
    .unwrap();
    let child = futures::executor::block_on(service.fork_at(source, item))
        .unwrap()
        .id;
    let state = fork_state(&store, child);
    assert_eq!(state.title.as_deref(), Some("old title"));
    assert_eq!(state.goal.as_ref().unwrap().objective, "old objective");
    assert_eq!(state.goal.unwrap().derived_from, Some(goal.value.id));
}

fn fork_state(store: &FakeStoreControl, id: SessionId) -> ego_core::state::SessionState {
    ego_core::state::SessionState::replay(
        store
            .0
            .entries(&id)
            .unwrap()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Event(event) => Some(event),
                _ => None,
            }),
    )
}

#[test]
fn fork_at_rejects_reasoning_and_compaction_checkpoint_ids_instead_of_accepting_storage_items() {
    let (service, store, _) = service();
    let source = service.create(PathBuf::from("/workspace")).unwrap().id;
    let (_, hidden, _) = fork_turn(&store, source, "old", Some(true));
    let target = SessionId::new();
    let seed = TargetSeed::compaction(
        source,
        expected(&store, source),
        1,
        "checkpoint".into(),
        Timestamp::from_millis(1),
    );
    store.0.publish_fork(target, &seed);
    let checkpoint = seed.event().env.item.unwrap();
    assert!(matches!(
        futures::executor::block_on(service.fork_at(target, checkpoint)),
        Err(LifecycleError::Fork(
            ego_core::session::ForkError::CompactionBoundary
        ))
    ));
    assert!(matches!(
        futures::executor::block_on(service.fork_at(target, hidden)),
        Err(LifecycleError::Fork(
            ego_core::session::ForkError::UnknownItem
        ))
    ));
    let mut reasoning = event("", store.event_count(source) as u64 + 1);
    let item = reasoning.env.item.unwrap();
    reasoning.kind = EventKind::ReasoningDelta {
        text: "not a message".into(),
    };
    store.append_event(source, reasoning);
    assert!(matches!(
        futures::executor::block_on(service.fork_at(source, item)),
        Err(LifecycleError::Fork(
            ego_core::session::ForkError::NotMessage
        ))
    ));
}

fn receipt_only_fixture() -> Vec<SessionEntry> {
    serde_json::from_str(include_str!("fixtures/receipt-only-replay.json")).unwrap()
}

fn load_receipt_entries(
    entries: Vec<SessionEntry>,
) -> Vec<ego_core::session_lifecycle::ReplayFrame> {
    let (service, store, _runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    store
        .0
        .0
        .lock()
        .unwrap()
        .entries
        .extend(entries.into_iter().map(|entry| (id, entry)));
    futures::executor::block_on(service.load(id, Path::new("/workspace")))
        .unwrap()
        .replay
}

#[test]
fn receipt_only_prompts_and_interruptions_are_not_dropped_from_replay() {
    use ego_core::public::{ItemKind, PublicEvent};
    let frames = load_receipt_entries(receipt_only_fixture());
    assert_eq!(
        frames.len(),
        6,
        "three real prompts and three recorded failures, no assistant reply"
    );
    for pair in frames.as_chunks::<2>().0 {
        assert!(
            matches!(&pair[0].event, PublicEvent::ItemCompleted { kind: ItemKind::UserMessage, text: Some(text), .. } if text == "ciao")
        );
        assert!(
            matches!(&pair[1].event, PublicEvent::Error { message, .. } if message.starts_with("confinement changed before execution:") && message.ends_with("the session must be reinitialized"))
        );
    }
    let ids = frames
        .iter()
        .step_by(2)
        .map(|frame| match frame.event {
            PublicEvent::ItemCompleted { item, .. } => item,
            _ => unreachable!(),
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        ids.len(),
        3,
        "identical text represents three distinct admissions"
    );
    assert_eq!(
        frames,
        load_receipt_entries(receipt_only_fixture()),
        "replay identities are stable"
    );
}

#[test]
fn receipt_replay_does_not_duplicate_semantic_prompt() {
    let mut entries = receipt_only_fixture();
    entries.truncate(2);
    let mut settled = serde_json::to_value(&entries[1]).unwrap();
    settled["run_transaction"]["command"]["settlement"] =
        serde_json::json!({"outcome": "completed"});
    settled["run_transaction"]["status"] = serde_json::json!("settled");
    entries[1] = serde_json::from_value(settled).unwrap();
    entries.insert(1, SessionEntry::Event(event("ciao", 1)));
    let frames = load_receipt_entries(entries);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                frame.event,
                ego_core::public::PublicEvent::ItemCompleted {
                    kind: ego_core::public::ItemKind::UserMessage,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[test]
fn receipt_replay_keeps_unprompted_session_empty() {
    assert!(load_receipt_entries(Vec::new()).is_empty());
}

#[test]
fn receipt_replay_does_not_duplicate_recorded_error() {
    let mut entries = receipt_only_fixture();
    entries.truncate(2);
    let diagnostic = match &entries[1] {
        SessionEntry::RunTransaction(transaction) => match transaction.command() {
            ego_core::run::RunCommand::CommandSettled { settlement, .. } => {
                settlement.diagnostic().unwrap().to_owned()
            }
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    let mut error = event("unused", 1);
    error.kind = EventKind::Error {
        message: diagnostic,
        kind: ego_core::generator::types::ErrorKind::Unclassified,
    };
    entries.insert(1, SessionEntry::Event(error));
    let frames = load_receipt_entries(entries);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame.event, ego_core::public::PublicEvent::Error { .. }))
            .count(),
        1
    );
}

// Catches: deleting a held session silently discards a resumable conversation.
#[test]
fn delete_refuses_a_durably_held_session() {
    let (service, _store, _runs) = service();
    let id = service.create(PathBuf::from("/workspace")).unwrap().id;
    futures::executor::block_on(
        service.pause(id, RequestId::from_uuid(uuid::Uuid::from_u128(0x439))),
    )
    .unwrap();
    assert!(futures::executor::block_on(service.delete(id)).is_err());
    assert_eq!(service.list(None, 10, None).unwrap().items.len(), 1);
}

// Catches: force either cannot acknowledge uncertainty, or bypasses live/held safety.
#[test]
fn delete_force_only_overrides_uncertainty_not_active_leased_or_held_sessions() {
    let (service, store, runs) = service();
    let root = PathBuf::from("/workspace");
    let uncertain = service.create(root.clone()).unwrap().id;
    let kept = service.create(root.clone()).unwrap().id;
    let kept_count = store.event_count(kept);
    runs.mark_uncertain(uncertain);
    assert!(matches!(
        futures::executor::block_on(service.delete(uncertain)),
        Err(LifecycleError::EffectUncertain)
    ));
    futures::executor::block_on(service.delete_with_force(uncertain, true)).unwrap();
    assert_eq!(store.event_count(kept), kept_count);
    assert_eq!(service.list(None, 10, None).unwrap().items[0].id, kept);
    for activity in [SessionActivity::Active, SessionActivity::LeaseHeld] {
        let id = service.create(root.clone()).unwrap().id;
        match activity {
            SessionActivity::Active => runs.mark_active(id),
            _ => runs.hold_lease(id),
        }
        assert!(futures::executor::block_on(service.delete_with_force(id, true)).is_err());
    }
    let held = service.create(root).unwrap().id;
    futures::executor::block_on(
        service.pause(held, RequestId::from_uuid(uuid::Uuid::from_u128(0x440))),
    )
    .unwrap();
    assert!(matches!(
        futures::executor::block_on(service.delete_with_force(held, true)),
        Err(LifecycleError::Held)
    ));
}
