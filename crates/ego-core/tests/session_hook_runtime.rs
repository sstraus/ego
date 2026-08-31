//! RED contracts for the borrow-scoped shared session hook runtime.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::task::{Context, Poll};

use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::hook::{
    HookDecision, HookInvocationId, HookLifecycle, HookOccurrenceKey,
    HookOccurrenceOwner, HookOutcome, HookPlan, HookResult, HookSubject,
};
use ego_core::event::id::{Producer, Seq, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::config::{ConfigLayer, Source, Trust};
use ego_core::hook::{
    HookCatalogue, HookEvent, HookFuture, HookInvocationRequest, HookLifecycleDispatcher, HookPort,
    HookRecordFuture, HookReachFuture, HookReachedEvidence, HookRunner, HookSpec,
    HookStartFuture, HookStartedEvidence, SessionHookRuntime,
};
use ego_core::sink::NullSink;
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::{CancelToken, Journal, SharedJournal};
use futures::stream::BoxStream;

#[derive(Default)]
struct Store {
    events: Mutex<Vec<DomainEvent>>,
}

impl Store {
    fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().expect("journal events").clone()
    }
}

impl SessionStore for Store {
    fn append(&self, _id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.events.lock().expect("journal events").push(event.clone());
        Ok(())
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let events = self
            .events
            .lock()
            .expect("journal events")
            .iter()
            .cloned()
            .map(|event| Ok(SessionEntry::Event(event)))
            .collect::<Vec<_>>();
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<ego_core::run::RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        panic!("run transactions are outside this hook fixture")
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        panic!("session listing is outside this hook fixture")
    }
}

#[derive(Clone, Copy, Default)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(79_000)
    }
}

#[derive(Default)]
struct RecordingPort {
    calls: Mutex<Vec<HookInvocationRequest>>,
    blocked: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
}

impl RecordingPort {
    fn calls(&self) -> usize {
        self.calls.lock().expect("hook calls").len()
    }

    fn blocked() -> (Self, futures::channel::oneshot::Sender<()>) {
        let (sender, receiver) = futures::channel::oneshot::channel();
        (
            Self {
                calls: Mutex::new(Vec::new()),
                blocked: Mutex::new(Some(receiver)),
            },
            sender,
        )
    }
}

impl HookPort for RecordingPort {
    fn reached<'a>(
        &'a self,
        _producer: &'a Producer,
        _occurrence: &'a ego_core::hook::HookOccurrence,
        _plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        Box::pin(std::future::ready(Err(ego_core::hook::HookReachError::Absent(
            ego_core::hook::HookRunError::new("journal is supplied by SessionHookRuntime"),
        ))))
    }

    fn started<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _occurrence: &'a ego_core::hook::HookOccurrence,
        _reached: &'a HookReachedEvidence,
        _activation: ego_core::event::hook::HookActivation,
        _invocation: HookInvocationId,
    ) -> HookStartFuture<'a> {
        Box::pin(std::future::ready(Err(ego_core::hook::HookStartError::Absent(
            ego_core::hook::HookRunError::new("journal is supplied by SessionHookRuntime"),
        ))))
    }

    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        self.calls.lock().expect("hook calls").push(request);
        let blocked = self.blocked.lock().expect("hook block").take();
        if let Some(blocked) = blocked {
            return Box::pin(async move {
                let _ = blocked.await;
                Ok(HookOutcome::new(HookDecision::Continue, None))
            });
        }
        Box::pin(std::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

fn empty_catalogue() -> HookCatalogue {
    HookCatalogue::admit_typed(&[], &[], ego_core::config::Trust::Trusted)
        .expect("empty catalogue")
}

fn active_catalogue() -> HookCatalogue {
    let layer = ConfigLayer::parse(
        Source::User,
        "[[hooks]]\nevent = \"SessionStart\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n",
    )
    .expect("hook layer");
    HookCatalogue::admit_typed(&[layer], &[], Trust::Trusted).expect("active catalogue")
}

fn runtime_fixture(blocked: bool) -> (
    SessionHookRuntime<'static>,
    Arc<RecordingPort>,
    &'static Store,
    TurnId,
    Option<futures::channel::oneshot::Sender<()>>,
) {
    let session = SessionId::new();
    let turn = TurnId::new();
    let store: &'static Store = Box::leak(Box::new(Store::default()));
    let clock: &'static FixedClock = Box::leak(Box::new(FixedClock));
    let sink: &'static NullSink = Box::leak(Box::new(NullSink));
    let journal = Journal::new(
        session,
        store,
        clock,
        sink,
        Producer::Main,
        SessionState::default(),
    );
    let shared = SharedJournal::new(journal);
    let (port, release) = if blocked {
        let (port, release) = RecordingPort::blocked();
        (port, Some(release))
    } else {
        (RecordingPort::default(), None)
    };
    let invocation = Arc::new(port);
    let dispatcher = HookLifecycleDispatcher::new(
        HookRunner::new(RecordingPort::default()),
        active_catalogue(),
    );
    let runtime = SessionHookRuntime::new(
        dispatcher,
        shared,
        Arc::clone(&invocation),
        CancelToken::new(),
        Instant::now() + Duration::from_secs(60),
        ego_core::hook::HookCausality::turn(turn),
    );
    (runtime, invocation, store, turn, release)
}

fn occurrence(lifecycle: HookLifecycle, turn: TurnId, n: u128) -> ego_core::hook::HookOccurrence {
    ego_core::hook::HookOccurrence::new(
        HookOccurrenceKey::new(
            lifecycle,
            HookOccurrenceOwner::Event {
                event: serde_json::from_str(&format!(
                    "\"00000000-0000-7000-8000-{n:012x}\""
                ))
                .expect("event id"),
            },
        ),
        HookEvent::turn(lifecycle, HookSubject::Turn { turn }),
    )
    .expect("matching occurrence")
}

#[test]
fn normal_append_and_two_hook_occurrences_share_one_sequence_and_state() {
    let (runtime, _invocation, store, turn, release) = runtime_fixture(false);
    let normal = runtime
        .journal()
        .append(
            turn,
            None,
            None,
            EventKind::AssistantMessage {
                text: "normal".into(),
            },
        )
        .expect("normal append");
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionStart, turn, 1),
    ))
    .expect("first hook");
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionEnd, turn, 2),
    ))
    .expect("second hook");
    if let Some(release) = release {
        let _ = release.send(());
    }

    let events = store.events();
    assert!(events.len() >= 3);
    assert_eq!(events[0].env.id, normal);
    let mut expected = Seq::ZERO;
    for event in &events {
        assert_eq!(event.env.seq, expected.next());
    }
    assert_eq!(runtime.state().last_seq, events[2].env.seq);
}

#[test]
fn recovered_completed_occurrence_does_not_invoke_and_state_keeps_all_facts() {
    let (runtime, invocation, store, turn, release) = runtime_fixture(false);
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionStart, turn, 3),
    ))
    .expect("first dispatch");
    let first_events = store.events().len();
    let first_calls = invocation.calls();
    assert_eq!(first_calls, 1, "first occurrence invokes once");
    assert!(release.is_none());
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionStart, turn, 3),
    ))
    .expect("recovered dispatch");
    assert_eq!(invocation.calls(), first_calls, "recovery never invokes again");
    assert!(store
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::HookLifecycleReached { .. })));
    assert_eq!(store.events().len(), first_events, "recovery reuses durable envelope");
}

#[test]
fn blocked_invocation_does_not_hold_journal_lock_across_await() {
    let (runtime, _invocation, _store, turn, release) = runtime_fixture(true);
    let mut blocked = Box::pin(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionStart, turn, 4),
    ));
    let waker = futures::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    assert!(matches!(blocked.as_mut().poll(&mut context), Poll::Pending));
    let append = runtime.journal().append(
        turn,
        None,
        None,
        EventKind::AssistantMessage {
            text: "independent".into(),
        },
    );
    assert!(append.is_ok(), "journal append must not await a hook-owned lock");
    let _ = release.expect("blocked invocation release").send(());
    drop(blocked);
}

#[test]
fn finalization_rejects_live_append_handles_then_succeeds_after_ordered_teardown() {
    let (runtime, _invocation, _store, _turn, _release) = runtime_fixture(false);
    let journal = runtime.journal().clone();
    assert!(journal.try_into_state().is_err(), "live runtime retains append authority");
    drop(runtime);
    assert!(journal.try_into_state().is_ok(), "ordered teardown releases authority");
}

#[test]
fn session_start_and_end_are_exact_once_and_reuse_durable_envelopes() {
    let (runtime, _invocation, store, turn, release) = runtime_fixture(false);
    let start = occurrence(HookLifecycle::SessionStart, turn, 5);
    let end = occurrence(HookLifecycle::SessionEnd, turn, 6);
    futures::executor::block_on(runtime.dispatch(Producer::Main, start.clone())).expect("start");
    futures::executor::block_on(runtime.dispatch(Producer::Main, start)).expect("replayed start");
    futures::executor::block_on(runtime.dispatch(Producer::Main, end.clone())).expect("end");
    futures::executor::block_on(runtime.dispatch(Producer::Main, end)).expect("replayed end");
    assert!(release.is_none());
    let events = store.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::HookLifecycleReached { .. }))
            .count(),
        2
    );
}
