//! Reopening a session that was killed mid-turn.
//!
//! `tests/faults.rs` checks individual recovery points. These tests check
//! that writer opening composes recovery before reading or appending.

use std::path::PathBuf;
use std::time::Duration;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::store::session_store::{SessionEntry, SessionStore, StoreError};
use ego_tools::store::{JsonlStore, LeaseInputs, OpenError, OsIo};
use futures::StreamExt;
use futures::executor::block_on;
use tempfile::{TempDir, tempdir};

const ALWAYS_ALIVE: &dyn Fn(u32) -> bool = &|_| true;

/// Lease policy for a test: a live holder, so nothing is ever taken over by
/// accident and a contended lease stays contended.
fn lease_inputs() -> LeaseInputs<'static> {
    LeaseInputs {
        pid: 4242,
        now: Timestamp::from_millis(9_000),
        stale_after: Duration::from_secs(30),
        is_alive: ALWAYS_ALIVE,
    }
}

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    store: JsonlStore,
    id: SessionId,
    turn: TurnId,
    seq: Seq,
}

impl Fixture {
    /// A healthy session: one complete turn, so the log is well-formed and
    /// the manifest has been written and fsynced before any damage.
    fn healthy() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = JsonlStore::new(&root, "/ws", Box::new(OsIo));
        let mut fx = Self {
            _dir: dir,
            root,
            store,
            id: SessionId::new(),
            turn: TurnId::new(),
            seq: Seq::default(),
        };
        for (kind, ts) in [
            (
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "list the files".to_string(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
                1_000,
            ),
            (
                EventKind::AssistantMessage {
                    text: "here they are".to_string(),
                },
                1_100,
            ),
            (
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ExitReason::Completed,
                },
                1_200,
            ),
        ] {
            fx.append(kind, ts);
        }
        fx
    }

    fn append_result(&mut self, kind: EventKind, ts_millis: u64) -> Result<(), StoreError> {
        let ev = DomainEvent {
            env: Envelope {
                id: EventId::new(),
                seq: self.seq.next(),
                ts: Timestamp::from_millis(ts_millis),
                turn: self.turn,
                item: Some(ItemId::new()),
                parent: None,
                producer: Producer::Main,
            },
            kind,
        };
        self.store.append(&self.id, &ev)
    }

    fn append(&mut self, kind: EventKind, ts_millis: u64) {
        self.append_result(kind, ts_millis).unwrap();
    }

    fn log_path(&self) -> PathBuf {
        let name = match serde_json::to_value(self.id).unwrap() {
            serde_json::Value::String(s) => s,
            other => unreachable!("{other}"),
        };
        self.root.join(name).join("events.jsonl")
    }

    fn log_bytes(&self) -> Vec<u8> {
        std::fs::read(self.log_path()).unwrap()
    }

    /// Cut `bytes` off the end of the log — exactly what a crash part-way
    /// through an `append` leaves behind.
    fn tear_tail(&self, bytes: usize) {
        let log = self.log_bytes();
        let keep = log.len() - bytes;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(self.log_path())
            .unwrap();
        file.set_len(keep as u64).unwrap();
    }

    /// Every event `read` yields, requiring all of them to decode.
    fn read_all(&self) -> Result<Vec<DomainEvent>, StoreError> {
        let stream = self.store.read(&self.id).unwrap();
        block_on(stream.collect::<Vec<_>>())
            .into_iter()
            .map(|result| match result? {
                SessionEntry::Event(event) => Ok(event),
                SessionEntry::RunTransaction(_) => Err(StoreError::Io(std::io::Error::other(
                    "event-only fixture contained a run transaction",
                ))),
                SessionEntry::Evidence(_)
                | SessionEntry::NoticeDelivered(_)
                | SessionEntry::ModelSelected(_) => Err(StoreError::Io(std::io::Error::other(
                    "event-only fixture contained evidence",
                ))),
            })
            .collect()
    }

    /// A second `JsonlStore` over the same root — what another process
    /// attaching to the same session directory holds.
    fn other_store(&self) -> JsonlStore {
        JsonlStore::new(&self.root, "/ws", Box::new(OsIo))
    }

    fn texts(&self) -> Vec<String> {
        self.read_all()
            .unwrap_or_default()
            .iter()
            .filter_map(|ev| match &ev.kind {
                EventKind::UserPrompt { text, .. } | EventKind::AssistantMessage { text } => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect()
    }
}

/// Appending to an unrepaired torn log refuses before mutation. The caller
/// must open the session for writing, which repairs the tail before allowing
/// any append; a direct append cannot splice a new record onto damaged bytes.
#[test]
fn appending_to_an_unrepaired_tear_refuses_before_mutation() {
    let mut fx = Fixture::healthy();
    let before = fx.texts();
    assert_eq!(before, ["list the files", "here they are"]);

    // A crash part-way through writing the next event.
    const TEAR: usize = 20;
    fx.tear_tail(TEAR);
    let torn = fx.log_bytes();

    let failure = fx.append_result(
        EventKind::UserPrompt {
            audio: Vec::new(),
            text: "and now the tests".to_string(),
            images: Vec::new(),
            queued_prompt: None,
        },
        2_000,
    );
    let error = failure.expect_err("an append must refuse an unterminated tail");
    assert!(
        matches!(error, StoreError::Io(ref io_error) if io_error.kind() == std::io::ErrorKind::InvalidData),
        "wrong refusal for a torn tail: {error:?}"
    );
    assert_eq!(
        fx.log_bytes(),
        torn,
        "a refused append must not mutate bytes"
    );

    let repair = fx.store.recover(&fx.id).unwrap();

    assert!(!repair.is_clean(), "the original tear has to be repaired");
    assert_eq!(
        fx.texts(),
        before,
        "recovery preserves the earlier valid prefix"
    );
    assert_eq!(repair.dropped_lines, 1, "{repair:?}");
    assert!(
        repair.truncated_bytes > TEAR as u64,
        "recovery removes the surviving bytes of the incomplete record too: {repair:?}"
    );
}

// -- Opening for writing ------------------------------------------------

/// The Step 12 contract: kill mid-stream, reopen, and the rebuilt state is
/// the state of everything that survived the kill — with the torn tail
/// repaired on the way in rather than left for the next append to splice onto.
#[test]
fn reopening_a_torn_session_repairs_it_and_rebuilds_the_surviving_state() {
    let fx = Fixture::healthy();
    let (before, _) = fx.store.resume(&fx.id).unwrap();

    // A crash part-way through writing a fourth event: the `TurnCompleted`
    // line is left unterminated.
    fx.tear_tail(20);

    let open = fx.store.open_writer(&fx.id, lease_inputs()).unwrap();

    assert!(
        !open.repair.is_clean(),
        "the tear has to be repaired on the way in, not reported clean"
    );
    assert_eq!(open.repair.dropped_lines, 1, "{:?}", open.repair);
    assert_ne!(
        open.state, before,
        "the torn event is genuinely gone — a state equal to the pre-crash \
         one would mean the tear was never real"
    );

    // The repair is not merely reported: a second open finds nothing left to
    // do, which is what "repaired on the way in" has to mean.
    drop(open);
    let reopened = fx.store.open_writer(&fx.id, lease_inputs()).unwrap();
    assert!(reopened.repair.is_clean(), "{:?}", reopened.repair);
    assert_eq!(
        reopened.state,
        fx.store.resume(&fx.id).unwrap().0,
        "the state the open rebuilt is the state the log now holds"
    );
}

/// The counterpart to `appending_to_an_unrepaired_tear_…`: appends made
/// through an opened writer survive, because the splice never happens.
#[test]
fn appends_through_an_opened_writer_survive_the_next_recovery() {
    let fx = Fixture::healthy();
    fx.tear_tail(20);

    {
        let open = fx.store.open_writer(&fx.id, lease_inputs()).unwrap();
        assert!(!open.repair.is_clean());

        let mut seq = open.state.last_seq;
        let turn = TurnId::new();
        for (kind, ts) in [
            (
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "and now the tests".to_string(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
                2_000,
            ),
            (
                EventKind::AssistantMessage {
                    text: "here are the tests".to_string(),
                },
                2_100,
            ),
        ] {
            let ev = DomainEvent {
                env: Envelope {
                    id: EventId::new(),
                    seq: seq.next(),
                    ts: Timestamp::from_millis(ts),
                    turn,
                    item: Some(ItemId::new()),
                    parent: None,
                    producer: Producer::Main,
                },
                kind,
            };
            open.writer.append(&fx.id, &ev).unwrap();
        }
    }

    // Not `is_clean()`: `append` only rewrites the manifest on a turn
    // boundary, and neither of these two events is one, so the log is
    // legitimately ahead of the manifest and recovery rebuilds it. What must
    // be untouched is the *log* — no tear, no dropped line.
    let after = fx.store.recover(&fx.id).unwrap();
    assert_eq!(after.truncated_bytes, 0, "{after:?}");
    assert_eq!(after.dropped_lines, 0, "{after:?}");
    assert_eq!(
        fx.texts(),
        [
            "list the files",
            "here they are",
            "and now the tests",
            "here are the tests"
        ],
        "both appends are still there — the repair happened before them"
    );
}

/// A clean session is opened without being touched: recovery is idempotent,
/// so a caller may run it unconditionally rather than having to work out
/// whether a crash happened, which it cannot.
#[test]
fn opening_a_clean_session_repairs_nothing_and_rebuilds_it_exactly() {
    let fx = Fixture::healthy();
    let (expected, _) = fx.store.resume(&fx.id).unwrap();
    let before = fx.log_bytes();

    let open = fx.store.open_writer(&fx.id, lease_inputs()).unwrap();

    assert!(open.repair.is_clean(), "{:?}", open.repair);
    assert_eq!(open.state, expected);
    assert_eq!(fx.log_bytes(), before, "a clean open rewrites no bytes");
}

/// One entry point serves a new session and a resumed one: `recover` is safe
/// on a session that was never written, and `resume` on an empty log yields
/// the default state. There is no second "create" path to keep in step.
#[test]
fn opening_a_session_that_was_never_written_yields_an_empty_state() {
    let fx = Fixture::healthy();
    let fresh = SessionId::new();

    let open = fx.store.open_writer(&fresh, lease_inputs()).unwrap();

    assert!(open.repair.is_clean(), "{:?}", open.repair);
    assert_eq!(open.state, Default::default());
    assert_eq!(open.writer.id(), fresh);
}

/// The lease is taken *first*, so a second writer is refused before it can
/// repair anything — an append landing between another writer's repair and
/// its first write would splice onto the log the repair just made whole.
#[test]
fn a_second_writer_cannot_open_a_session_the_first_still_holds() {
    let fx = Fixture::healthy();
    let other = fx.other_store();

    let _first = fx.store.open_writer(&fx.id, lease_inputs()).unwrap();

    match other.open_writer(&fx.id, lease_inputs()) {
        Err(OpenError::Lease(_)) => {}
        Err(other) => panic!("wrong failure: {other}"),
        Ok(_) => panic!("two writers held the same session at once"),
    }
}

/// Dropping the opened session releases the lease — which is the whole reason
/// the `Lease` lives inside the writer rather than beside it.
#[test]
fn dropping_the_open_session_releases_the_lease() {
    let fx = Fixture::healthy();
    let other = fx.other_store();

    let first = fx.store.open_writer(&fx.id, lease_inputs()).unwrap();
    drop(first);

    other
        .open_writer(&fx.id, lease_inputs())
        .expect("the lease must be free once the opened session is dropped");
}

/// A writer holds the lease for one session and refuses another rather than
/// delegating: an append to a second session would be an append the lease
/// does not cover.
#[test]
fn the_writer_refuses_a_session_it_does_not_hold() {
    let mut fx = Fixture::healthy();
    let open = fx.store.open_writer(&fx.id, lease_inputs()).unwrap();
    let stranger = SessionId::new();

    let ev = DomainEvent {
        env: Envelope {
            id: EventId::new(),
            seq: fx.seq.next(),
            ts: Timestamp::from_millis(3_000),
            turn: fx.turn,
            item: None,
            parent: None,
            producer: Producer::Main,
        },
        kind: EventKind::UserPrompt {
            audio: Vec::new(),
            text: "not your session".to_string(),
            images: Vec::new(),
            queued_prompt: None,
        },
    };

    match open.writer.append(&stranger, &ev) {
        Err(StoreError::WrongSession { expected, asked }) => {
            assert_eq!(expected, fx.id);
            assert_eq!(asked, stranger);
        }
        Err(other) => panic!("wrong failure: {other}"),
        Ok(()) => panic!("the writer appended to a session it holds no lease for"),
    }

    assert!(
        open.writer.read(&stranger).is_err(),
        "reading another session through this writer is the same confusion"
    );
}

/// `OpenError::Lease` says "nothing was touched", and this is what holds it
/// to that: a refused open must not have repaired the log on its way to
/// being refused. It is also the only test that can see the *order* — a
/// lease taken after recovery would still refuse a second writer, but would
/// have rewritten another process's log first.
#[test]
fn an_open_refused_by_the_lease_leaves_the_damage_alone() {
    let fx = Fixture::healthy();
    let other = fx.other_store();
    fx.tear_tail(20);
    let torn = fx.log_bytes();

    let _first = fx.store.open_writer(&fx.id, lease_inputs()).unwrap();
    // The first open repaired it; put the damage back so the second open has
    // something to wrongly repair.
    std::fs::write(fx.log_path(), &torn).unwrap();

    match other.open_writer(&fx.id, lease_inputs()) {
        Err(OpenError::Lease(_)) => {}
        Err(e) => panic!("wrong failure: {e}"),
        Ok(_) => panic!("two writers held the same session at once"),
    }

    assert_eq!(
        fx.log_bytes(),
        torn,
        "the refused open repaired a log it was never allowed to write to"
    );
}
