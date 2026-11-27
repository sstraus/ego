//! Runtime ownership regressions: an open call or prose is not a live task.
use super::journal::{Journal, SharedJournal};
use crate::event::domain::{DomainEvent, EventKind};
use crate::event::id::{ItemId, Producer, SessionId, TurnId};
use crate::event::time::{Clock, Timestamp};
use crate::plan::{PlanStep, PlanUpdate, StepStatus};
use crate::state::SessionState;
use crate::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use futures::stream::{self, BoxStream};
use std::sync::Mutex;

#[derive(Default)]
struct Store(Mutex<Vec<DomainEvent>>);
impl SessionStore for Store {
    fn append(&self, _: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }
    fn read(
        &self,
        _: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        Ok(Box::pin(stream::iter(
            self.0
                .lock()
                .unwrap()
                .clone()
                .into_iter()
                .map(|event| Ok(SessionEntry::Event(event))),
        )))
    }
    fn compare_and_append_run(
        &self,
        _: &SessionId,
        _: Option<crate::run::RunRevision>,
        _: crate::run::RunTransaction,
    ) -> Result<crate::run::RunAppend, StoreError> {
        Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "worklist fixtures cannot create durable runs",
        )))
    }
    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(vec![])
    }
}
struct Time;
impl Clock for Time {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1)
    }
}
fn journal(store: &Store, state: SessionState) -> SharedJournal<'_> {
    SharedJournal::new(Journal::new(
        SessionId::new(),
        store,
        &Time,
        &crate::sink::NullSink,
        Producer::Main,
        state,
    ))
}
fn step(id: &str, status: StepStatus) -> PlanStep {
    PlanStep {
        id: id.into(),
        text: id.into(),
        status,
        reason: None,
    }
}
fn update(id: &str, status: StepStatus) -> PlanUpdate {
    PlanUpdate::Step {
        id: id.into(),
        status,
        reason: None,
    }
}
fn open_task(
    journal: &SharedJournal<'_>,
    turn: TurnId,
    id: &str,
) -> (ItemId, crate::event::id::EventId) {
    let item = ItemId::new();
    let parent = journal
        .append(
            turn,
            Some(item),
            None,
            EventKind::ToolCallStarted {
                name: "task".into(),
                call_id: Some(item.to_string()),
                args: serde_json::json!({"name":"verify", "prompt":"verify", "worklist_step":id})
                    .to_string(),
            },
        )
        .unwrap();
    (item, parent)
}

// Catches: treating prepared/open calls as execution, model takeover of a bound
// row, and a dropped or settled task permanently retaining concurrency authority.
#[test]
fn worklist_concurrency_requires_a_running_future_and_expires_on_settlement_or_drop() {
    let store = Store::default();
    let journal = journal(&store, SessionState::default());
    let turn = TurnId::new();
    journal
        .worklist(
            turn,
            PlanUpdate::Replace {
                steps: vec![
                    step("direct", StepStatus::InProgress),
                    step("child", StepStatus::Pending),
                    step("third", StepStatus::Pending),
                ],
            },
        )
        .unwrap()
        .unwrap();
    let (item, parent) = open_task(&journal, turn, "child");
    let before = journal
        .inspect(|state| (state.last_seq, state.plan.clone()))
        .unwrap();
    assert!(
        journal
            .worklist(turn, update("child", StepStatus::InProgress))
            .unwrap()
            .is_err()
    );
    assert_eq!(
        journal
            .inspect(|state| (state.last_seq, state.plan.clone()))
            .unwrap(),
        before
    );
    let binding = journal
        .start_worklist_task(turn, item, parent, "child".into())
        .unwrap()
        .unwrap();
    assert_eq!(
        journal.inspect(|state| state.plan.steps[1].status).unwrap(),
        StepStatus::InProgress
    );
    assert!(
        journal
            .worklist(turn, update("child", StepStatus::Completed))
            .unwrap()
            .unwrap_err()
            .contains("owned by running task")
    );
    assert!(
        journal
            .worklist(turn, update("third", StepStatus::InProgress))
            .unwrap()
            .is_err()
    );
    drop(binding);
    assert_eq!(
        journal.inspect(|state| state.plan.steps[1].status).unwrap(),
        StepStatus::Blocked
    );
    assert!(
        journal
            .inspect(|state| state.plan.steps[1].reason.is_some())
            .unwrap()
    );
    assert!(
        journal
            .worklist(turn, update("child", StepStatus::InProgress))
            .unwrap()
            .is_err()
    );
    let (item, parent) = open_task(&journal, turn, "child");
    let binding = journal
        .start_worklist_task(turn, item, parent, "child".into())
        .unwrap()
        .unwrap();
    binding.finish(true).unwrap();
    assert_eq!(
        journal.inspect(|state| state.plan.steps[1].status).unwrap(),
        StepStatus::Completed
    );
    assert!(
        journal
            .worklist(turn, update("third", StepStatus::InProgress))
            .unwrap()
            .is_err()
    );
}

// Catches: resurrecting an interrupted process's open task as a live execution.
#[test]
fn worklist_reopen_retires_torn_task_and_preserves_started_identity() {
    let store = Store::default();
    let original = journal(&store, SessionState::default());
    let turn = TurnId::new();
    original
        .worklist(
            turn,
            PlanUpdate::Replace {
                steps: vec![
                    step("direct", StepStatus::InProgress),
                    step("child", StepStatus::Pending),
                ],
            },
        )
        .unwrap()
        .unwrap();
    let (item, parent) = open_task(&original, turn, "child");
    let binding = original
        .start_worklist_task(turn, item, parent, "child".into())
        .unwrap()
        .unwrap();
    let replay = SessionState::replay(store.0.lock().unwrap().clone());
    let reopened_store = Store::default();
    let reopened = journal(&reopened_store, replay);
    assert!(
        reopened
            .worklist(turn, update("child", StepStatus::Completed))
            .unwrap()
            .is_ok(),
        "without live ownership, the model may report actual terminal progress"
    );
    // A second reopen from the genuinely interrupted checkpoint recovers its task.
    let checkpoint = original.inspect(Clone::clone).unwrap();
    let recovered_store = Store::default();
    let recovered = journal(&recovered_store, checkpoint);
    recovered.recover_interrupted_attempts(turn).unwrap();
    assert_eq!(
        recovered
            .inspect(|state| state.plan.steps[1].status)
            .unwrap(),
        StepStatus::Blocked
    );
    assert_eq!(
        recovered.inspect(|state| state.items[&item].ok).unwrap(),
        Some(false)
    );
    assert!(
        recovered
            .worklist(turn, update("child", StepStatus::Pending))
            .unwrap()
            .is_err()
    );
    drop(binding);
}

// Catches: a recorded started-then-pending row (the reported 1171 defect) vetoing
// every unrelated worklist write and task binding, although unchanged recorded
// rows are meant not to block repair of an older worklist.
#[test]
fn recorded_reset_row_does_not_veto_unrelated_progress_or_binding() {
    let store = Store::default();
    let journal = journal(&store, SessionState::default());
    let turn = TurnId::new();
    for recorded in [
        PlanUpdate::Replace {
            steps: vec![
                step("legacy", StepStatus::InProgress),
                step("child", StepStatus::Pending),
                step("other", StepStatus::Pending),
            ],
        },
        update("legacy", StepStatus::Pending),
    ] {
        journal
            .append(
                turn,
                None,
                None,
                EventKind::PlanUpdated { update: recorded },
            )
            .unwrap();
    }
    assert_eq!(
        journal.inspect(|state| state.plan.steps[0].status).unwrap(),
        StepStatus::Pending,
        "recorded history must still replay"
    );
    let (item, parent) = open_task(&journal, turn, "child");
    journal
        .start_worklist_task(turn, item, parent, "child".into())
        .unwrap()
        .expect("an unrelated recorded row must not refuse a live binding")
        .finish(true)
        .unwrap();
    journal
        .worklist(turn, update("other", StepStatus::InProgress))
        .unwrap()
        .expect("an unchanged recorded row must not refuse an unrelated update");
}

// Catches: host settlement refused by the model-facing 16 KiB budget once the
// failure reason is added, which fails the task batch with a store error and
// leaves a dead execution's row in_progress.
#[test]
fn failed_task_settles_at_the_worklist_size_budget() {
    let store = Store::default();
    let journal = journal(&store, SessionState::default());
    let turn = TurnId::new();
    let mut full = step("child", StepStatus::Pending);
    full.text = "x".repeat(16 * 1024 - full.id.len());
    journal
        .worklist(turn, PlanUpdate::Replace { steps: vec![full] })
        .unwrap()
        .unwrap();
    let (item, parent) = open_task(&journal, turn, "child");
    let binding = journal
        .start_worklist_task(turn, item, parent, "child".into())
        .unwrap()
        .unwrap();
    binding
        .finish(false)
        .expect("host settlement of a failed task must not be refused");
    assert_eq!(
        journal.inspect(|state| state.plan.steps[0].status).unwrap(),
        StepStatus::Blocked
    );
}

// Catches: reopen recovery appending a suspension that replay silently cannot
// fold (`let _ = plan.apply`), so the durable log says blocked while the folded
// state keeps an interrupted task's row in_progress.
#[test]
fn reopen_recovery_blocks_torn_row_at_the_worklist_size_budget() {
    let store = Store::default();
    let original = journal(&store, SessionState::default());
    let turn = TurnId::new();
    let mut full = step("child", StepStatus::Pending);
    full.text = "x".repeat(16 * 1024 - full.id.len());
    original
        .worklist(turn, PlanUpdate::Replace { steps: vec![full] })
        .unwrap()
        .unwrap();
    let (item, parent) = open_task(&original, turn, "child");
    let binding = original
        .start_worklist_task(turn, item, parent, "child".into())
        .unwrap()
        .unwrap();
    let checkpoint = original.inspect(Clone::clone).unwrap();
    let recovered_store = Store::default();
    let recovered = journal(&recovered_store, checkpoint);
    recovered.recover_interrupted_attempts(turn).unwrap();
    assert_eq!(
        recovered.inspect(|state| state.items[&item].ok).unwrap(),
        Some(false)
    );
    assert_eq!(
        recovered
            .inspect(|state| state.plan.steps[0].status)
            .unwrap(),
        StepStatus::Blocked,
        "the recorded suspension must also be the folded state"
    );
    drop(binding);
}

// Catches: two independently prepared model updates both pass a cloned-state
// check and publish, leaving two direct active actions after the race.
#[test]
fn concurrent_worklist_admission_publishes_only_one_direct_start() {
    let store = Store::default();
    let journal = journal(&store, SessionState::default());
    let turn = TurnId::new();
    journal
        .worklist(
            turn,
            PlanUpdate::Replace {
                steps: vec![
                    step("one", StepStatus::Pending),
                    step("two", StepStatus::Pending),
                ],
            },
        )
        .unwrap()
        .unwrap();
    let results = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            journal
                .worklist(turn, update("one", StepStatus::InProgress))
                .unwrap()
        });
        let second = scope.spawn(|| {
            journal
                .worklist(turn, update("two", StepStatus::InProgress))
                .unwrap()
        });
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        journal
            .inspect(|state| state
                .plan
                .steps
                .iter()
                .filter(|row| row.status == StepStatus::InProgress)
                .count())
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event.kind, EventKind::PlanUpdated { .. }))
            .count(),
        2
    );
}

// Catches: a task prepared against a pending row still launches after another
// admitted update removes that row, or a fabricated call identity creates a lease.
#[test]
fn removed_pending_action_and_fabricated_call_cannot_start_bound_execution() {
    let store = Store::default();
    let journal = journal(&store, SessionState::default());
    let turn = TurnId::new();
    journal
        .worklist(
            turn,
            PlanUpdate::Replace {
                steps: vec![step("child", StepStatus::Pending)],
            },
        )
        .unwrap()
        .unwrap();
    let (item, parent) = open_task(&journal, turn, "child");
    let before = journal
        .inspect(|state| (state.last_seq, state.plan.clone()))
        .unwrap();
    assert!(
        journal
            .start_worklist_task(turn, ItemId::new(), parent, "child".into())
            .unwrap()
            .is_err()
    );
    assert_eq!(
        journal
            .inspect(|state| (state.last_seq, state.plan.clone()))
            .unwrap(),
        before
    );
    journal
        .worklist(turn, PlanUpdate::Replace { steps: vec![] })
        .unwrap()
        .unwrap();
    let before = journal
        .inspect(|state| (state.last_seq, state.plan.clone()))
        .unwrap();
    assert!(
        journal
            .start_worklist_task(turn, item, parent, "child".into())
            .unwrap()
            .is_err()
    );
    assert_eq!(
        journal
            .inspect(|state| (state.last_seq, state.plan.clone()))
            .unwrap(),
        before
    );
}
