//! RED snapshot contract for hook state, versions, and replay fallback.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::hook::{
    HookCatalogueEvidence, HookDescriptorEvidence, HookFailure, HookFailureKind, HookInvocationId,
    HookLifecycle, HookResult, HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::state::SessionState;
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
    DomainEvent {
        env: Envelope {
            seq,
            id,
            turn,
            item: None,
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
                start_id,
                turn,
                Some(admission_id),
                Producer::Main,
                EventKind::HookInvocationStarted {
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

#[test]
fn snapshot_version_is_nine_and_validation_exposes_exact_rejection_set() {
    assert_eq!(SNAPSHOT_VERSION, 9);
    let fixture = Fixture::new();
    let state = fixture.full_replay();
    assert_eq!(state.hook_settlements().len(), 1);
    let snapshot = Snapshot::new(fixture.id, fixture.events[2].env.seq, state.clone());
    assert_eq!(snapshot.v, 9);
    assert_eq!(
        validate(
            &snapshot,
            &fixture.id,
            fixture.events[0].env.seq,
            fixture.events[2].env.seq
        ),
        Ok(())
    );
    assert!(matches!(
        validate(
            &snapshot,
            &SessionId::new(),
            fixture.events[0].env.seq,
            fixture.events[2].env.seq
        ),
        Err(SnapshotRejection::WrongSession)
    ));
}

#[test]
fn actual_v4_snapshot_is_rejected_and_jsonl_replay_reconstructs_hooks() {
    let fixture = Fixture::new();
    let state = fixture.full_replay();
    let current = Snapshot::new(fixture.id, fixture.events[2].env.seq, state.clone());
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
        .write_snapshot(&fixture.id, fixture.events[2].env.seq, &expected)
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
    let pre = event(
        seq.next(),
        EventId::new(),
        turn,
        Some(admission.env.id),
        Producer::Main,
        EventKind::HookInvocationStarted {
            hook: d.id(),
            invocation: HookInvocationId::new(),
            lifecycle: HookLifecycle::PreToolUse,
            subject: ego_core::event::hook::HookSubject::Turn { turn },
            activation: ego_core::event::hook::HookActivation::Persistent,
        },
    );
    let observer = event(
        seq.next(),
        EventId::new(),
        turn,
        Some(pre.env.id),
        Producer::Main,
        EventKind::HookInvocationStarted {
            hook: d.id(),
            invocation: HookInvocationId::new(),
            lifecycle: HookLifecycle::PostToolUse,
            subject: ego_core::event::hook::HookSubject::Turn { turn },
            activation: ego_core::event::hook::HookActivation::Persistent,
        },
    );
    for e in [&admission, &pre, &observer] {
        store.append(&id, e).unwrap();
    }
    let mut state = SessionState::default();
    for e in [&admission, &pre, &observer] {
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
