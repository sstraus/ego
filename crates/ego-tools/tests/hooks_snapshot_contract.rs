//! RED snapshot contract for hook state, versions, and replay fallback.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::hook::{
    HookCatalogueEvidence, HookDescriptorEvidence, HookFailure, HookFailureKind, HookInvocationId,
    HookLifecycle, HookOccurrenceKey, HookOccurrenceOwner, HookPlan, HookResult,
    HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::state::{RefusalReason, SessionState};
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, OsIo, SNAPSHOT_VERSION, Snapshot, SnapshotRejection, validate};
use tempfile::{TempDir, tempdir};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Fixture {
    _dir: TempDir,
    root: std::path::PathBuf,
    store: JsonlStore,
    id: SessionId,
    events: Vec<DomainEvent>,
}

fn descriptor(ordinal: u32) -> HookDescriptorEvidence {
    HookDescriptorEvidence::new(
        HookSourceAuthority::Project,
        HookSourceLocation::parse("hooks/project.json").unwrap(),
        DIGEST.parse().unwrap(),
        ordinal,
    )
}

fn event(
    seq: Seq,
    id: EventId,
    turn: TurnId,
    parent: Option<EventId>,
    producer: Producer,
    kind: EventKind,
) -> DomainEvent {
    event_with_item(seq, id, turn, None, parent, producer, kind)
}

fn event_with_item(
    seq: Seq,
    id: EventId,
    turn: TurnId,
    item: Option<ItemId>,
    parent: Option<EventId>,
    producer: Producer,
    kind: EventKind,
) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq,
            id,
            turn,
            item,
            parent,
            ts: Timestamp::from_millis(1_000),
            producer,
        },
        kind,
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();
        let turn = TurnId::new();
        let d = descriptor(0);
        let invocation = HookInvocationId::new();
        let admission_id = EventId::new();
        let reached_id = EventId::new();
        let start_id = EventId::new();
        let settle_id = EventId::new();
        let mut seq = Seq::default();
        let events = vec![
            event(
                seq.next(),
                admission_id,
                turn,
                None,
                Producer::Main,
                EventKind::HookCatalogueAdmitted {
                    descriptors: HookCatalogueEvidence::new(vec![d.clone()]).unwrap(),
                },
            ),
            event(
                seq.next(),
                reached_id,
                turn,
                Some(admission_id),
                Producer::Main,
                EventKind::HookLifecycleReached {
                    occurrence: HookOccurrenceKey::new(
                        HookLifecycle::PreToolUse,
                        HookOccurrenceOwner::Session { session: id },
                    ),
                    subject: ego_core::event::hook::HookSubject::Turn { turn },
                    plan: HookPlan::new(vec![d.id()]).unwrap(),
                },
            ),
            event(
                seq.next(),
                start_id,
                turn,
                Some(reached_id),
                Producer::Main,
                EventKind::HookInvocationStarted {
                    occurrence: reached_id,
                    hook: d.id(),
                    invocation,
                    lifecycle: HookLifecycle::PreToolUse,
                    subject: ego_core::event::hook::HookSubject::Turn { turn },
                    activation: ego_core::event::hook::HookActivation::Persistent,
                },
            ),
            event(
                seq.next(),
                settle_id,
                turn,
                Some(start_id),
                Producer::Hook(d.id()),
                EventKind::HookInvocationSettled {
                    occurrence: reached_id,
                    hook: d.id(),
                    invocation,
                    result: HookResult::Failure {
                        failure: HookFailure::new(HookFailureKind::TimedOut, None),
                    },
                },
            ),
        ];
        for e in &events {
            store.append(&id, e).unwrap();
        }
        Self {
            _dir: dir,
            root,
            store,
            id,
            events,
        }
    }

    fn snapshot_path(&self) -> std::path::PathBuf {
        let name = serde_json::to_value(self.id)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        self.root.join(name).join("snapshot.json")
    }

    fn full_replay(&self) -> SessionState {
        let mut state = SessionState::default();
        for e in &self.events {
            state.apply(e);
        }
        state
    }
}

struct ToolOriginFixture {
    _dir: TempDir,
    root: std::path::PathBuf,
    store: JsonlStore,
    id: SessionId,
    item: ItemId,
    tool_started: EventId,
    events: Vec<DomainEvent>,
}

impl ToolOriginFixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
        let id = SessionId::new();
        let turn = TurnId::new();
        let item = ItemId::new();
        let mut delta = event_with_item(
            Seq::default().next(),
            EventId::new(),
            turn,
            Some(item),
            None,
            Producer::Main,
            EventKind::ToolCallDelta {
                text: "before-start".to_owned(),
            },
        );
        let mut tool_started = event_with_item(
            delta.env.seq.next(),
            EventId::new(),
            turn,
            Some(item),
            Some(delta.env.id),
            Producer::Main,
            EventKind::ToolCallStarted {
                name: "ask".to_owned(),
                args: "{}".to_owned(),
                call_id: Some("call-origin".to_owned()),
            },
        );
        let mut accepted = event_with_item(
            tool_started.env.seq.next(),
            EventId::new(),
            turn,
            Some(item),
            Some(tool_started.env.id),
            Producer::Main,
            EventKind::HookLifecycleReached {
                occurrence: HookOccurrenceKey::continuation(
                    HookLifecycle::Elicitation,
                    format!("mcp-{}", "a".repeat(64)),
                )
                .unwrap(),
                subject: ego_core::event::hook::HookSubject::Item { item },
                plan: HookPlan::default(),
            },
        );
        let rejected = event_with_item(
            accepted.env.seq.next(),
            EventId::new(),
            turn,
            Some(item),
            Some(delta.env.id),
            Producer::Main,
            EventKind::HookLifecycleReached {
                occurrence: HookOccurrenceKey::continuation(
                    HookLifecycle::Elicitation,
                    format!("mcp-{}", "b".repeat(64)),
                )
                .unwrap(),
                subject: ego_core::event::hook::HookSubject::Item { item },
                plan: HookPlan::default(),
            },
        );
        let events = vec![delta, tool_started.clone(), accepted, rejected];
        for event in &events {
            store.append(&id, event).unwrap();
        }
        Self {
            _dir: dir,
            root,
            store,
            id,
            item,
            tool_started: tool_started.env.id,
            events,
        }
    }

    fn snapshot_path(&self) -> std::path::PathBuf {
        self.root.join(self.id.to_string()).join("snapshot.json")
    }

    fn full_replay(&self) -> SessionState {
        let mut state = SessionState::default();
        for event in &self.events {
            state.apply(event);
        }
        state
    }

    fn base_state(&self) -> SessionState {
        let mut state = SessionState::default();
        for event in self.events.iter().take(2) {
            state.apply(event);
        }
        state
    }
}

fn assert_tool_origin_and_continuation_result(state: &SessionState, item: ItemId, start: EventId) {
    assert_eq!(
        state.items.get(&item).expect("tool item").tool_call_started,
        Some(start)
    );
    assert_eq!(state.hook_occurrences().len(), 1);
    assert_eq!(
        state.refusals.last().map(|refusal| refusal.reason),
        Some(RefusalReason::HookOccurrenceParentMismatch)
    );
}

#[test]
fn snapshot_version_is_twelve_and_validation_exposes_exact_rejection_set() {
    assert_eq!(SNAPSHOT_VERSION, 12);
    let fixture = Fixture::new();
    let state = fixture.full_replay();
    assert_eq!(state.hook_settlements().len(), 1);
    let snapshot = Snapshot::new(fixture.id, fixture.events[3].env.seq, state.clone());
    assert_eq!(snapshot.v, 12);
    assert_eq!(
        validate(
            &snapshot,
            &fixture.id,
            fixture.events[0].env.seq,
            fixture.events[3].env.seq
        ),
        Ok(())
    );
    assert!(matches!(
        validate(
            &snapshot,
            &SessionId::new(),
            fixture.events[0].env.seq,
            fixture.events[3].env.seq
        ),
        Err(SnapshotRejection::WrongSession)
    ));
}

#[test]
fn full_log_replay_reconstructs_exact_tool_call_origin_and_parent_result() {
    let fixture = ToolOriginFixture::new();
    let state = fixture.full_replay();

    assert_tool_origin_and_continuation_result(&state, fixture.item, fixture.tool_started);
}

#[test]
fn snapshot_v12_roundtrip_and_tail_replay_preserve_tool_origin_and_result() {
    let fixture = ToolOriginFixture::new();
    let expected = fixture.full_replay();
    fixture
        .store
        .write_snapshot(
            &fixture.id,
            fixture.events[1].env.seq,
            &fixture.base_state(),
        )
        .unwrap();

    let (resumed, rejection) = fixture.store.resume(&fixture.id).unwrap();
    assert_eq!(rejection, None);
    assert_eq!(resumed, expected);
    assert_tool_origin_and_continuation_result(&resumed, fixture.item, fixture.tool_started);
}

#[test]
fn real_v11_snapshot_without_tool_origin_is_version_rejected_then_full_replay_succeeds() {
    let fixture = ToolOriginFixture::new();
    let expected = fixture.full_replay();
    let snapshot = Snapshot::new(fixture.id, fixture.events[1].env.seq, fixture.base_state());
    let mut legacy = serde_json::to_value(snapshot).unwrap();
    legacy["v"] = serde_json::json!(11);
    let items = legacy["state"]["items"]
        .as_object_mut()
        .expect("snapshot items object");
    for item in items.values_mut() {
        item.as_object_mut()
            .expect("serialized item object")
            .remove("tool_call_started");
    }
    std::fs::write(
        fixture.snapshot_path(),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();

    let (resumed, rejection) = fixture.store.resume(&fixture.id).unwrap();
    assert_eq!(rejection, Some(SnapshotRejection::Version { found: 11 }));
    assert_eq!(resumed, expected);
    assert_tool_origin_and_continuation_result(&resumed, fixture.item, fixture.tool_started);
}

#[test]
fn actual_v4_snapshot_is_rejected_and_jsonl_replay_reconstructs_hooks() {
    let fixture = Fixture::new();
    let state = fixture.full_replay();
    let current = Snapshot::new(fixture.id, fixture.events[3].env.seq, state.clone());
    let mut version_only = serde_json::to_value(&current).unwrap();
    version_only["v"] = serde_json::json!(4);
    std::fs::write(
        fixture.snapshot_path(),
        serde_json::to_vec(&version_only).unwrap(),
    )
    .unwrap();
    let (resumed, rejection) = fixture.store.resume(&fixture.id).unwrap();
    assert_eq!(rejection, Some(SnapshotRejection::Version { found: 4 }));
    assert_eq!(resumed, state);

    let mut old = version_only;
    old["v"] = serde_json::json!(SNAPSHOT_VERSION);
    let state_object = old["state"].as_object_mut().expect("snapshot state object");
    for field in [
        "hook_catalogue",
        "live_hooks",
        "hook_settlements",
        "settled_hooks",
    ] {
        assert!(
            state_object.remove(field).is_some(),
            "v5 snapshot must carry {field}"
        );
    }
    std::fs::write(fixture.snapshot_path(), serde_json::to_vec(&old).unwrap()).unwrap();
    let (resumed, rejection) = fixture.store.resume(&fixture.id).unwrap();
    assert!(matches!(rejection, Some(SnapshotRejection::Unreadable(_))));
    assert_eq!(resumed, state);
}

#[test]
fn admitted_live_and_settled_hook_state_roundtrips_through_snapshot_and_replay() {
    let fixture = Fixture::new();
    let expected = fixture.full_replay();
    assert_eq!(expected.hook_catalogue().len(), 1);
    assert!(expected.live_hooks().is_empty());
    assert_eq!(expected.hook_settlements().len(), 1);
    fixture
        .store
        .write_snapshot(&fixture.id, fixture.events[3].env.seq, &expected)
        .unwrap();
    let written: Snapshot =
        serde_json::from_slice(&std::fs::read(fixture.snapshot_path()).unwrap()).unwrap();
    assert_eq!(written.v, SNAPSHOT_VERSION);
    assert_eq!(written.state, expected);
    let (resumed, rejection) = fixture.store.resume(&fixture.id).unwrap();
    assert_eq!(rejection, None);
    assert_eq!(resumed, expected);
}

#[test]
fn unresolved_live_pre_effect_and_observer_states_survive_snapshot_without_retry() {
    let dir = tempdir().unwrap();
    let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
    let id = SessionId::new();
    let turn = TurnId::new();
    let d = descriptor(0);
    let mut seq = Seq::default();
    let admission = event(
        seq.next(),
        EventId::new(),
        turn,
        None,
        Producer::Main,
        EventKind::HookCatalogueAdmitted {
            descriptors: HookCatalogueEvidence::new(vec![d.clone()]).unwrap(),
        },
    );
    let pre_reached = event(
        seq.next(),
        EventId::new(),
        turn,
        Some(admission.env.id),
        Producer::Main,
        EventKind::HookLifecycleReached {
            occurrence: HookOccurrenceKey::new(
                HookLifecycle::PreToolUse,
                HookOccurrenceOwner::Session { session: id },
            ),
            subject: ego_core::event::hook::HookSubject::Turn { turn },
            plan: HookPlan::new(vec![d.id()]).unwrap(),
        },
    );
    let pre = event(
        seq.next(),
        EventId::new(),
        turn,
        Some(pre_reached.env.id),
        Producer::Main,
        EventKind::HookInvocationStarted {
            occurrence: pre_reached.env.id,
            hook: d.id(),
            invocation: HookInvocationId::new(),
            lifecycle: HookLifecycle::PreToolUse,
            subject: ego_core::event::hook::HookSubject::Turn { turn },
            activation: ego_core::event::hook::HookActivation::Persistent,
        },
    );
    let observer_reached = event(
        seq.next(),
        EventId::new(),
        turn,
        Some(pre.env.id),
        Producer::Main,
        EventKind::HookLifecycleReached {
            occurrence: HookOccurrenceKey::new(
                HookLifecycle::PostToolUse,
                HookOccurrenceOwner::Session { session: id },
            ),
            subject: ego_core::event::hook::HookSubject::Turn { turn },
            plan: HookPlan::new(vec![d.id()]).unwrap(),
        },
    );
    let observer = event(
        seq.next(),
        EventId::new(),
        turn,
        Some(observer_reached.env.id),
        Producer::Main,
        EventKind::HookInvocationStarted {
            occurrence: observer_reached.env.id,
            hook: d.id(),
            invocation: HookInvocationId::new(),
            lifecycle: HookLifecycle::PostToolUse,
            subject: ego_core::event::hook::HookSubject::Turn { turn },
            activation: ego_core::event::hook::HookActivation::Persistent,
        },
    );
    for e in [&admission, &pre_reached, &pre, &observer_reached, &observer] {
        store.append(&id, e).unwrap();
    }
    let mut state = SessionState::default();
    for e in [&admission, &pre_reached, &pre, &observer_reached, &observer] {
        state.apply(e);
    }
    assert_eq!(state.live_hooks().len(), 2);
    store.write_snapshot(&id, observer.env.seq, &state).unwrap();
    let (resumed, rejection) = store.resume(&id).unwrap();
    assert_eq!(rejection, None);
    assert_eq!(resumed.live_hooks(), state.live_hooks());
    assert!(
        resumed
            .live_hooks()
            .values()
            .all(|hook| !hook.delivery().retry_is_safe())
    );
    assert!(
        resumed
            .live_hooks()
            .values()
            .all(|hook| hook.delivery() == DeliveryCertainty::PossiblySent)
    );
}
