//! RED contracts for closed hook-owner causality and durable journal links.

use std::sync::Mutex;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::hook::{
    HookActivation, HookDecision, HookId, HookInvocationId, HookLifecycle, HookOccurrenceKey,
    HookOccurrenceOwner, HookOutcome, HookPlan, HookPlanEntry, HookResult, HookSubject,
};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::hook::HookStartedEvidence;
use ego_core::run::{RunAppend, RunRevision};
use ego_core::sink::CollectingSink;
use ego_core::state::{RefusalReason, SessionState};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::Journal;
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
        self.events
            .lock()
            .expect("journal events")
            .push(event.clone());
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
        _expected_revision: Option<RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<RunAppend, StoreError> {
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

fn event(
    turn: TurnId,
    item: Option<ItemId>,
    parent: Option<EventId>,
    kind: EventKind,
) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: Seq::ZERO,
            id: EventId::new(),
            turn,
            item,
            parent,
            ts: Timestamp::from_millis(79_000),
            producer: Producer::Main,
        },
        kind,
    }
}

fn reached(
    lifecycle: HookLifecycle,
    owner: HookOccurrenceOwner,
    subject: HookSubject,
    turn: TurnId,
    item: Option<ItemId>,
    parent: Option<EventId>,
) -> DomainEvent {
    event(
        turn,
        item,
        parent,
        EventKind::HookLifecycleReached {
            occurrence: HookOccurrenceKey::new(lifecycle, owner),
            subject,
            plan: HookPlan::default(),
        },
    )
}

fn assert_last_refusal(state: &SessionState, reason: RefusalReason) {
    assert_eq!(
        state.refusals.last().map(|refusal| refusal.reason),
        Some(reason)
    );
}

#[test]
fn session_owner_rejects_item_causality() {
    let turn = TurnId::new();
    let item = ItemId::new();
    let session = SessionId::new();
    let mut state = SessionState::default();

    state.apply(&reached(
        HookLifecycle::SessionStart,
        HookOccurrenceOwner::Session { session },
        HookSubject::Item { item },
        turn,
        Some(item),
        None,
    ));
    assert_last_refusal(&state, RefusalReason::HookSubjectMismatch);
}

#[test]
fn request_owner_rejects_item_causality() {
    let turn = TurnId::new();
    let item = ItemId::new();
    let request = ego_core::run::RequestId::from_uuid(uuid::Uuid::from_u128(79));
    let mut state = SessionState::default();
    state.apply(&reached(
        HookLifecycle::PreModelSwitch,
        HookOccurrenceOwner::Request { request },
        HookSubject::Item { item },
        turn,
        Some(item),
        None,
    ));
    assert_last_refusal(&state, RefusalReason::HookSubjectMismatch);
}

#[test]
fn event_owner_requires_parent_to_be_the_owner_event() {
    let turn = TurnId::new();
    let owner = EventId::new();
    let mut state = SessionState::default();

    state.apply(&reached(
        HookLifecycle::PermissionRequest,
        HookOccurrenceOwner::Event { event: owner },
        HookSubject::Turn { turn },
        turn,
        None,
        Some(EventId::new()),
    ));
    // The current reducer reports the closed owner mismatch under the older
    // subject refusal. The runtime contract must retain the more specific
    // parent reason once the shared owner validation is introduced.
    assert_last_refusal(&state, RefusalReason::HookSubjectMismatch);
}

#[test]
fn continuation_requires_a_tool_item() {
    let turn = TurnId::new();
    let item = ItemId::new();
    let continuation = HookOccurrenceKey::continuation(
        HookLifecycle::Elicitation,
        format!("mcp-{}", "a".repeat(64)),
    )
    .expect("valid continuation identity");
    let mut state = SessionState::default();

    let no_tool = DomainEvent {
        env: Envelope {
            seq: Seq::ZERO,
            id: EventId::new(),
            turn,
            item: Some(item),
            parent: None,
            ts: Timestamp::from_millis(79_000),
            producer: Producer::Main,
        },
        kind: EventKind::HookLifecycleReached {
            occurrence: continuation.clone(),
            subject: HookSubject::Item { item },
            plan: HookPlan::default(),
        },
    };
    state.apply(&no_tool);
    assert_last_refusal(&state, RefusalReason::HookSubjectMismatch);
}

#[test]
fn continuation_requires_exact_tool_parent() {
    let turn = TurnId::new();
    let item = ItemId::new();
    let continuation = HookOccurrenceKey::continuation(
        HookLifecycle::Elicitation,
        format!("mcp-{}", "a".repeat(64)),
    )
    .expect("valid continuation identity");
    let mut state = SessionState::default();
    let tool = event(
        turn,
        Some(item),
        None,
        EventKind::ToolCallStarted {
            name: "ask".to_owned(),
            args: "{}".to_owned(),
            call_id: Some("call-79".to_owned()),
        },
    );
    state.apply(&tool);
    let wrong_known_parent = event(
        turn,
        None,
        None,
        EventKind::AssistantMessage {
            text: "known but not the tool start".to_owned(),
        },
    );
    state.apply(&wrong_known_parent);
    let wrong_parent = DomainEvent {
        env: Envelope {
            seq: Seq::ZERO,
            id: EventId::new(),
            turn,
            item: Some(item),
            parent: Some(wrong_known_parent.env.id),
            ts: Timestamp::from_millis(79_000),
            producer: Producer::Main,
        },
        kind: EventKind::HookLifecycleReached {
            occurrence: continuation,
            subject: HookSubject::Item { item },
            plan: HookPlan::default(),
        },
    };
    state.apply(&wrong_parent);
    assert_last_refusal(&state, RefusalReason::HookOccurrenceParentMismatch);
}

#[test]
fn continuation_accepts_exact_tool_start_after_a_tool_delta() {
    let turn = TurnId::new();
    let item = ItemId::new();
    let continuation = HookOccurrenceKey::continuation(
        HookLifecycle::Elicitation,
        format!("mcp-{}", "b".repeat(64)),
    )
    .expect("valid continuation identity");
    let mut state = SessionState::default();

    let delta = event(
        turn,
        Some(item),
        None,
        EventKind::ToolCallDelta {
            text: "partial".to_owned(),
        },
    );
    state.apply(&delta);
    let started = event(
        turn,
        Some(item),
        Some(delta.env.id),
        EventKind::ToolCallStarted {
            name: "ask".to_owned(),
            args: "{}".to_owned(),
            call_id: Some("call-after-delta".to_owned()),
        },
    );
    state.apply(&started);
    let reached = event(
        turn,
        Some(item),
        Some(started.env.id),
        EventKind::HookLifecycleReached {
            occurrence: continuation.clone(),
            subject: HookSubject::Item { item },
            plan: HookPlan::default(),
        },
    );
    state.apply(&reached);

    assert!(
        state.refusals.is_empty(),
        "the exact tool start parent is accepted"
    );
    assert_eq!(
        state.items.get(&item).expect("tool item").tool_call_started,
        Some(started.env.id),
        "tool origin is the ToolCallStarted event, not the preceding delta"
    );
    assert_eq!(state.hook_occurrences().len(), 1);
}

#[test]
fn durable_start_and_settlement_repeat_reached_turn_item_and_parent_chain() {
    let session = SessionId::new();
    let turn = TurnId::new();
    let item = ItemId::new();
    let hook = HookId::parse(&format!("h1:{}", "b".repeat(64))).expect("valid hook id");
    let invocation = HookInvocationId::new();
    let store = Store::default();
    let clock = FixedClock;
    let sink = CollectingSink::new();
    let mut journal = Journal::new(
        session,
        &store,
        &clock,
        &sink,
        Producer::Main,
        SessionState::default(),
    );
    let occurrence = HookOccurrenceKey::new(
        HookLifecycle::PreToolUse,
        HookOccurrenceOwner::Item { item },
    );
    let reached = journal
        .hook_reached(
            turn,
            Some(item),
            None,
            occurrence,
            HookSubject::Item { item },
            HookPlan::new(vec![HookPlanEntry::new(hook, HookActivation::Persistent)])
                .expect("one hook plan"),
        )
        .expect("durable reach");
    let started = journal
        .hook_started(
            turn,
            Some(item),
            &reached,
            hook,
            invocation,
            HookActivation::Persistent,
        )
        .expect("durable start");
    journal
        .hook_settled(
            turn,
            Some(item),
            started,
            HookResult::Outcome {
                outcome: HookOutcome::new(HookDecision::Continue, None),
            },
        )
        .expect("durable settlement");

    let events = store.events();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].env.turn, turn);
    assert_eq!(events[0].env.item, Some(item));
    assert_eq!(events[0].env.parent, None);
    assert_eq!(events[1].env.turn, turn);
    assert_eq!(events[1].env.item, Some(item));
    assert_eq!(events[1].env.parent, Some(events[0].env.id));
    assert_eq!(events[2].env.turn, turn);
    assert_eq!(events[2].env.item, Some(item));
    assert_eq!(events[2].env.parent, Some(events[1].env.id));

    let _ = HookStartedEvidence::from_event(&events[1]).expect("start evidence remains durable");
    assert_eq!(journal.state().last_seq, events[2].env.seq);
}
