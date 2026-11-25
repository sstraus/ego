//! behavioral contract for the lease-bound shared lifecycle journal.
use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::public::PublicEvent;
use ego_core::session_lifecycle::{
    LeasedLifecycleAppend, LifecycleAppendError, LifecyclePublication,
};
use ego_core::sink::EventSink;
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use futures::stream::{self, BoxStream};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct SameFake {
    id: SessionId,
    events: Arc<Mutex<Vec<DomainEvent>>>,
    append_error: Arc<Mutex<bool>>,
    publication: Arc<Mutex<LifecyclePublication>>,
    delegations: Arc<Mutex<usize>>,
}
impl SameFake {
    fn new() -> Self {
        Self {
            id: SessionId::new(),
            events: Arc::new(Mutex::new(Vec::new())),
            append_error: Arc::new(Mutex::new(false)),
            publication: Arc::new(Mutex::new(LifecyclePublication::Published)),
            delegations: Arc::new(Mutex::new(0)),
        }
    }
    fn set_append_error(&self, v: bool) {
        *self.append_error.lock().unwrap() = v
    }
    fn set_publication(&self, v: LifecyclePublication) {
        *self.publication.lock().unwrap() = v
    }
    fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().unwrap().clone()
    }
    fn err() -> StoreError {
        StoreError::Io(std::io::Error::other("injected"))
    }
}
impl SessionStore for SameFake {
    fn append(&self, id: &SessionId, e: &DomainEvent) -> Result<(), StoreError> {
        assert_eq!(*id, self.id);
        if *self.append_error.lock().unwrap() {
            return Err(Self::err());
        }
        let mut l = self.events.lock().unwrap();
        let expected = l.last().map_or(q(1), |e| {
            let mut s = e.env.seq;
            s.next()
        });
        assert_eq!(e.env.seq, expected);
        l.push(e.clone());
        Ok(())
    }
    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        assert_eq!(*id, self.id);
        Ok(Box::pin(stream::iter(
            self.events()
                .into_iter()
                .map(|e| Ok(SessionEntry::Event(e))),
        )))
    }
    fn compare_and_append_run(
        &self,
        _: &SessionId,
        _: Option<ego_core::run::RunRevision>,
        _: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        Err(Self::err())
    }
    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}
impl LeasedLifecycleAppend for SameFake {
    fn session_id(&self) -> SessionId {
        self.id
    }
    fn compare_and_append_lifecycle(
        &self,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        let l = self.events.lock().unwrap();
        let actual = l.last().map_or(Seq::ZERO, |e| e.env.seq);
        if expected != actual {
            return Err(LifecycleAppendError::Conflict);
        }
        drop(l);
        *self.delegations.lock().unwrap() += 1;
        let p = self.publication.lock().unwrap().clone();
        if p.is_visible() {
            self.events.lock().unwrap().push(event)
        }
        Ok(p)
    }
}
struct RecordingSink {
    events: Mutex<Vec<PublicEvent>>,
}
impl RecordingSink {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
}
impl EventSink for RecordingSink {
    fn publish(&self, e: &PublicEvent, _: &SessionState) {
        self.events.lock().unwrap().push(e.clone())
    }
}
struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1)
    }
}
fn event(seq: Seq, kind: EventKind) -> DomainEvent {
    DomainEvent {
        env: ego_core::event::domain::Envelope {
            seq,
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(1),
            producer: Producer::Main,
        },
        kind,
    }
}
fn q(n: u64) -> Seq {
    let mut s = Seq::ZERO;
    for _ in 0..n {
        s.next();
    }
    s
}
fn title(seq: Seq, text: &str) -> DomainEvent {
    event(
        seq,
        EventKind::SessionTitleChanged {
            title: Some(text.into()),
        },
    )
}

#[test]
fn journal_orders_ordinary_lifecycle_ordinary_and_replays_exactly() {
    let f = SameFake::new();
    let s = RecordingSink::new();
    let j = ego_core::session_lifecycle::LeasedSharedJournal::new(
        &f,
        &FixedClock,
        &s,
        Producer::Main,
        SessionState::default(),
    );
    j.shared()
        .append(
            TurnId::new(),
            None,
            None,
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "one".into(),
                images: Vec::new(),
                queued_prompt: None,
            },
        )
        .unwrap();
    j.compare_and_append_lifecycle(q(1), title(q(2), "named"))
        .unwrap();
    j.shared()
        .append(
            TurnId::new(),
            None,
            None,
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "three".into(),
                images: Vec::new(),
                queued_prompt: None,
            },
        )
        .unwrap();
    assert_eq!(
        f.events().iter().map(|e| e.env.seq).collect::<Vec<_>>(),
        vec![q(1), q(2), q(3)]
    );
    let st = j.try_into_state().unwrap();
    assert_eq!(st.last_seq, q(3));
    assert_eq!(st.title.as_deref(), Some("named"));
}
#[test]
fn stale_expected_conflicts_before_delegation_and_mutation() {
    let f = SameFake::new();
    let s = RecordingSink::new();
    let j = ego_core::session_lifecycle::LeasedSharedJournal::new(
        &f,
        &FixedClock,
        &s,
        Producer::Main,
        SessionState::default(),
    );
    assert!(matches!(
        j.compare_and_append_lifecycle(q(9), title(q(1), "x")),
        Err(LifecycleAppendError::Conflict)
    ));
    assert!(f.events().is_empty());
    assert_eq!(*f.delegations.lock().unwrap(), 0)
}
#[test]
fn frozen_ordinary_append_prevents_lifecycle_delegation() {
    let f = SameFake::new();
    f.set_append_error(true);
    let s = RecordingSink::new();
    let j = ego_core::session_lifecycle::LeasedSharedJournal::new(
        &f,
        &FixedClock,
        &s,
        Producer::Main,
        SessionState::default(),
    );
    assert!(
        j.shared()
            .append(
                TurnId::new(),
                None,
                None,
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "x".into(),
                    images: Vec::new(),
                    queued_prompt: None
                }
            )
            .is_err()
    );
    assert!(matches!(
        j.compare_and_append_lifecycle(Seq::ZERO, title(q(1), "x")),
        Err(LifecycleAppendError::Store(_))
    ));
    assert_eq!(*f.delegations.lock().unwrap(), 0)
}
#[test]
fn unpublished_lifecycle_does_not_fold_or_consume_sequence() {
    let f = SameFake::new();
    f.set_publication(LifecyclePublication::NotPublished {
        diagnostic: "no".into(),
    });
    let s = RecordingSink::new();
    let j = ego_core::session_lifecycle::LeasedSharedJournal::new(
        &f,
        &FixedClock,
        &s,
        Producer::Main,
        SessionState::default(),
    );
    j.compare_and_append_lifecycle(Seq::ZERO, title(q(1), "x"))
        .unwrap();
    assert!(
        j.shared()
            .inspect(|state| state.title.clone())
            .unwrap()
            .is_none()
    );
    j.shared()
        .append(
            TurnId::new(),
            None,
            None,
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "x".into(),
                images: Vec::new(),
                queued_prompt: None,
            },
        )
        .unwrap();
    assert_eq!(f.events().len(), 1);
    assert_eq!(f.events()[0].env.seq, q(1))
}
#[test]
fn uncertain_publication_folds_once_and_next_append_follows() {
    let f = SameFake::new();
    f.set_publication(LifecyclePublication::PublishedDurabilityUncertain {
        diagnostic: "uncertain".into(),
    });
    let s = RecordingSink::new();
    let j = ego_core::session_lifecycle::LeasedSharedJournal::new(
        &f,
        &FixedClock,
        &s,
        Producer::Main,
        SessionState::default(),
    );
    let p = j
        .compare_and_append_lifecycle(Seq::ZERO, title(q(1), "x"))
        .unwrap();
    assert!(
        matches!(p, LifecyclePublication::PublishedDurabilityUncertain { diagnostic } if diagnostic == "uncertain")
    );
    j.shared()
        .append(
            TurnId::new(),
            None,
            None,
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "x".into(),
                images: Vec::new(),
                queued_prompt: None,
            },
        )
        .unwrap();
    let st = j.try_into_state().unwrap();
    assert_eq!(st.last_seq, q(2));
    assert_eq!(st.title.as_deref(), Some("x"));
}
