//! RED WAL/manifest crash-boundary contract for hook evidence.
//!
//! These tests drive the real `JsonlStore` through `StoreIo` spies and inspect
//! the raw on-disk manifest. An array of operation names is not evidence.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::hook::{
    HookActivation, HookCatalogueEntryEvidence, HookCatalogueEvidence, HookDecision,
    HookDescriptorEvidence, HookInvocationId, HookLifecycle, HookOccurrenceKey,
    HookOccurrenceOwner, HookOutcome, HookPlan, HookPlanEntry, HookResult, HookSourceAuthority,
    HookSourceLocation,
};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, OsIo, StoreIo};
use tempfile::tempdir;

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn descriptor(ordinal: u32) -> HookDescriptorEvidence {
    HookDescriptorEvidence::new(
        HookSourceAuthority::Project,
        HookSourceLocation::parse("hooks/project.json").unwrap(),
        DIGEST.parse().unwrap(),
        ordinal,
    )
}

fn session_name(id: &SessionId) -> String {
    serde_json::to_value(id)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}
fn manifest(root: &Path, id: &SessionId) -> PathBuf {
    root.join(session_name(id)).join("manifest.json")
}
fn log(root: &Path, id: &SessionId) -> PathBuf {
    root.join(session_name(id)).join("events.jsonl")
}

fn event(
    seq: &mut Seq,
    turn: TurnId,
    parent: Option<EventId>,
    producer: Producer,
    kind: EventKind,
) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn,
            item: None,
            parent,
            ts: Timestamp::from_millis(1_000),
            producer,
        },
        kind,
    }
}

#[derive(Clone)]
struct SpyIo {
    inner: OsIo,
    calls: Arc<Mutex<Vec<String>>>,
    fail_append: Arc<Mutex<bool>>,
}

impl SpyIo {
    fn new(calls: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            inner: OsIo,
            calls,
            fail_append: Arc::new(Mutex::new(false)),
        }
    }
    fn with_fail(calls: Arc<Mutex<Vec<String>>>, fail_append: Arc<Mutex<bool>>) -> Self {
        Self {
            inner: OsIo,
            calls,
            fail_append,
        }
    }
    fn mark(&self, name: &str) {
        self.calls.lock().unwrap().push(name.to_owned());
    }
}

impl StoreIo for SpyIo {
    fn create_new(&self, path: &Path) -> io::Result<()> {
        self.mark("create_new");
        self.inner.create_new(path)
    }
    fn append_bytes(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.mark("append_bytes");
        self.inner.append_bytes(path, bytes)
    }
    fn visit_file(
        &self,
        path: &Path,
        visitor: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<bool> {
        self.inner.visit_file(path, visitor)
    }
    fn publish_new(&self, staging: &Path, target: &Path) -> io::Result<bool> {
        self.inner.publish_new(staging, target)
    }
    fn list_files(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list_files(path)
    }
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.mark("append_line");
        if *self.fail_append.lock().unwrap() {
            return Err(io::Error::other("injected append failure"));
        }
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.mark("fsync");
        self.inner.fsync(path)
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.exists(path)
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.mark("replace_atomic");
        self.inner.replace_atomic(path, bytes)
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

fn hook_events() -> (DomainEvent, DomainEvent, DomainEvent, DomainEvent) {
    let d = descriptor(0);
    let turn = TurnId::new();
    let invocation = HookInvocationId::new();
    let mut seq = Seq::default();
    let admission = event(
        &mut seq,
        turn,
        None,
        Producer::Main,
        EventKind::HookCatalogueAdmitted {
            descriptors: HookCatalogueEvidence::new(vec![HookCatalogueEntryEvidence::new(
                d.clone(),
                HookActivation::Persistent,
            )])
            .unwrap(),
        },
    );
    let reached = event(
        &mut seq,
        turn,
        Some(admission.env.id),
        Producer::Main,
        EventKind::HookLifecycleReached {
            occurrence: HookOccurrenceKey::new(
                HookLifecycle::PreToolUse,
                HookOccurrenceOwner::Session {
                    session: SessionId::new(),
                },
            ),
            subject: ego_core::event::hook::HookSubject::Turn { turn },
            plan: HookPlan::new(vec![HookPlanEntry::new(d.id(), HookActivation::Persistent)])
                .unwrap(),
        },
    );
    let start = event(
        &mut seq,
        turn,
        Some(reached.env.id),
        Producer::Main,
        EventKind::HookInvocationStarted {
            occurrence: reached.env.id,
            hook: d.id(),
            invocation,
            lifecycle: HookLifecycle::PreToolUse,
            subject: ego_core::event::hook::HookSubject::Turn { turn },
            activation: ego_core::event::hook::HookActivation::Persistent,
        },
    );
    let settle = event(
        &mut seq,
        turn,
        Some(start.env.id),
        Producer::Hook(d.id()),
        EventKind::HookInvocationSettled {
            occurrence: reached.env.id,
            hook: d.id(),
            invocation,
            result: HookResult::Outcome {
                outcome: HookOutcome::new(HookDecision::Continue, None),
            },
        },
    );
    (admission, reached, start, settle)
}

#[test]
fn admission_fsyncs_log_before_atomic_manifest_replace_and_manifest_contains_typed_descriptor() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let io = SpyIo::new(Arc::clone(&calls));
    let store = JsonlStore::new(dir.path(), "/ws", Box::new(io));
    let id = SessionId::new();
    let (admission, _, _, _) = hook_events();
    store.append(&id, &admission).unwrap();
    let calls = calls.lock().unwrap();
    let fsync = calls.iter().position(|call| call == "fsync").unwrap();
    let replace = calls
        .iter()
        .position(|call| call == "replace_atomic")
        .unwrap();
    assert!(
        fsync < replace,
        "log durability must precede manifest publication: {calls:?}"
    );
    let raw = std::fs::read_to_string(manifest(dir.path(), &id)).unwrap();
    assert!(raw.contains(&descriptor(0).id().to_string()));
    assert!(raw.contains(DIGEST));
    assert!(!raw.contains("handler"));
}

#[test]
fn start_and_settlement_each_force_log_fsync_and_use_hook_producer_on_settlement() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let io = SpyIo::new(Arc::clone(&calls));
    let store = JsonlStore::new(dir.path(), "/ws", Box::new(io));
    let id = SessionId::new();
    let (admission, reached, start, settle) = hook_events();
    store.append(&id, &admission).unwrap();
    store.append(&id, &reached).unwrap();
    let baseline = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| *c == "fsync")
        .count();
    store.append(&id, &start).unwrap();
    store.append(&id, &settle).unwrap();
    let total = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| *c == "fsync")
        .count();
    assert_eq!(
        total,
        baseline + 2,
        "start and settlement are individually durable"
    );
    assert!(matches!(settle.env.producer, Producer::Hook(_)));
}

#[test]
fn missing_or_corrupt_manifest_rebuilds_never_fired_descriptors_from_authoritative_log() {
    let dir = tempdir().unwrap();
    let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
    let id = SessionId::new();
    let (admission, _, _, _) = hook_events();
    store.append(&id, &admission).unwrap();
    let path = manifest(dir.path(), &id);
    std::fs::remove_file(&path).unwrap();
    store.recover(&id).unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains(&descriptor(0).id().to_string())
    );
    std::fs::write(&path, b"not json").unwrap();
    store.recover(&id).unwrap();
    assert!(std::fs::read_to_string(&path).unwrap().contains(DIGEST));
}

#[test]
fn empty_admission_clears_manifest_catalogue_and_keeps_raw_manifest_valid() {
    let dir = tempdir().unwrap();
    let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
    let id = SessionId::new();
    let (admission, _, _, _) = hook_events();
    store.append(&id, &admission).unwrap();
    let mut seq = admission.env.seq;
    let clear = event(
        &mut seq,
        admission.env.turn,
        Some(admission.env.id),
        Producer::Main,
        EventKind::HookCatalogueAdmitted {
            descriptors: HookCatalogueEvidence::new(Vec::new()).unwrap(),
        },
    );
    store.append(&id, &clear).unwrap();
    let raw = std::fs::read_to_string(manifest(dir.path(), &id)).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        json.get("hooks")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn failed_append_and_torn_tail_cannot_advance_manifest_beyond_valid_log_prefix() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fail_append = Arc::new(Mutex::new(false));
    let io = SpyIo::with_fail(Arc::clone(&calls), Arc::clone(&fail_append));
    let store = JsonlStore::new(dir.path(), "/ws", Box::new(io));
    let id = SessionId::new();
    let turn = TurnId::new();
    let mut harmless_seq = Seq::default();
    let harmless = event(
        &mut harmless_seq,
        turn,
        None,
        Producer::Main,
        EventKind::TurnCompleted {
            ok: true,
            exit_reason: ego_core::event::domain::ExitReason::Completed,
        },
    );
    store.append(&id, &harmless).unwrap();
    let before = std::fs::read(manifest(dir.path(), &id)).unwrap();
    *fail_append.lock().unwrap() = true;
    let (admission, _, _, _) = hook_events();
    assert!(store.append(&id, &admission).is_err());
    assert_eq!(
        std::fs::read(manifest(dir.path(), &id)).unwrap(),
        before,
        "failed append cannot advance manifest"
    );
    let raw_log = log(dir.path(), &id);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&raw_log)
        .unwrap()
        .write_all(b"{torn")
        .unwrap();
    *fail_append.lock().unwrap() = false;
    let _ = store.recover(&id);
    assert_eq!(
        std::fs::read(manifest(dir.path(), &id)).unwrap(),
        before,
        "torn tail cannot advance manifest"
    );
}

#[test]
fn later_turn_boundary_preserves_admitted_hooks_and_does_not_replace_them_with_empty_state() {
    let dir = tempdir().unwrap();
    let store = JsonlStore::new(dir.path(), "/ws", Box::new(OsIo));
    let id = SessionId::new();
    let (admission, _, _, _) = hook_events();
    store.append(&id, &admission).unwrap();
    let mut seq = admission.env.seq;
    let boundary = event(
        &mut seq,
        admission.env.turn,
        Some(admission.env.id),
        Producer::Main,
        EventKind::TurnCompleted {
            ok: true,
            exit_reason: ego_core::event::domain::ExitReason::Completed,
        },
    );
    store.append(&id, &boundary).unwrap();
    let raw = std::fs::read_to_string(manifest(dir.path(), &id)).unwrap();
    assert!(raw.contains(&descriptor(0).id().to_string()));
}
