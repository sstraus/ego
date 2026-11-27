//! The one place a `DomainEvent` is assembled, persisted and reduced.
//!
//! See `docs/internals/decisions/2026-08-21-the-turn-loop.md` §5. Before this existed,
//! eight test files built their own `Envelope`, and they disagreed with each
//! other — `tests/public_golden.rs:167` gives a permission its own `ItemId`
//! while `tests/reducer.rs:143` reuses the tool's. That is what a contract
//! with no implementation looks like.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::event::domain::{DomainEvent, Envelope, EventKind};
use crate::event::hook::{
    HookActivation, HookId, HookInvocationId, HookLifecycle, HookOccurrenceKey,
    HookOccurrenceOwner, HookPlan, HookResult, HookSubject,
};
use crate::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use crate::event::time::Clock;
use crate::hook::{
    HookJournalPort, HookOccurrence, HookReachError, HookReachOutcome, HookReachedEvidence,
    HookRunError, HookSettledEvidence, HookSpec, HookStartError, HookStartedEvidence,
};
use crate::public::{PublicEvent, project_public};
use crate::session_lifecycle::{LeasedLifecycleAppend, LifecycleAppendError, LifecyclePublication};
use crate::sink::EventSink;
use crate::state::{ItemKind, SessionState};
use crate::store::session_store::{SessionStore, StoreError};

/// Assembles envelopes, appends them to the durable log, folds them into the
/// in-memory state, and publishes the projection — in that order, and nowhere
/// else.
///
/// The ordering is the point, and it now has two halves. The fold happens only
/// after the append returns; if the store may have written before reporting an
/// error, the exact event freezes this journal until its durable disposition
/// is resolved. The reverse order would produce an in-memory session that
/// remembers an event no reader will ever find. The sink comes last for the
/// same reason one layer out: a host told about an event that was never written
/// would be reporting a turn nobody can resume into.
///
/// That ordering is also why the sink is handed [`SessionState`]: at the
/// moment of publication this journal holds the reduction *including* the
/// event it is publishing, which is precisely what a consumer projecting
/// cumulative figures needs and cannot obtain for itself in the right order.
/// See `docs/internals/decisions/2026-08-21-what-a-sink-is-handed.md`.
pub struct Journal<'a> {
    session: SessionId,
    store: &'a dyn SessionStore,
    clock: &'a dyn Clock,
    /// Where the public projection goes the moment it is durable. Held rather
    /// than accumulated: `CLI.md` §4.2 says *live*, and a `Vec` handed over
    /// at turn end is a different contract wearing the same flag — as well as
    /// a buffer that grows for the life of the session.
    sink: &'a dyn EventSink,
    producer: Producer,
    /// Continues from the resumed state's `last_seq`, so reloading a session
    /// and carrying on does not restart its ordering.
    seq: Seq,
    state: SessionState,
    /// The exact event whose append returned an error. Until its durable
    /// disposition is resolved, no different event may consume this sequence.
    uncertain_append: Option<UncertainAppend>,
    /// Only attempts inherited when opening this journal, never live sibling work.
    interrupted_attempts: Vec<(EventId, crate::generator::attempt::AttemptRef)>,
    /// Process-owned task futures, never reconstructed from recorded open calls.
    worklist_executions: std::collections::BTreeMap<ItemId, String>,
    interrupted_tasks: Vec<ItemId>,
}

/// Turns the user opened by invoking a skill or command directly. Such a turn
/// commits a skill instruction instead of a `UserPrompt` item, yet it is a
/// user turn for the protocol filter.
fn direct_skill_turns(state: &SessionState) -> std::collections::HashSet<TurnId> {
    state
        .skill_events()
        .iter()
        .filter_map(|event| match event {
            crate::state::SkillEvent::Instruction(admitted) if admitted.parent.is_none() => {
                Some(admitted.turn)
            }
            _ => None,
        })
        .collect()
}

struct UncertainAppend {
    event: DomainEvent,
    cause: String,
}

/// A cloneable append authority for one borrow-scoped session journal.
///
/// The mutex serialises only synchronous journal operations. Store append,
/// reduction, and publication still happen in [`Journal::append_with_producer`]
/// and in their existing order; this handle adds no second write path.
#[derive(Clone)]
pub struct SharedJournal<'s> {
    inner: Arc<Mutex<Journal<'s>>>,
}

/// Why a shared journal operation could not complete.
#[derive(Debug)]
pub enum SharedJournalError {
    /// Another append-capable handle still owns the journal at finalization.
    InUse,
    /// A prior panic poisoned the journal lock, so no further write is safe.
    Synchronization,
    /// The sole underlying journal append failed.
    Store(StoreError),
    /// Finalization refused because a failed append has unknown durability.
    UncertainAppend {
        session: SessionId,
        seq: Seq,
        operation: String,
        cause: String,
    },
}

impl fmt::Display for SharedJournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InUse => f.write_str("the shared journal still has live append handles"),
            Self::Synchronization => f.write_str("the shared journal lock is poisoned"),
            Self::Store(error) => error.fmt(f),
            Self::UncertainAppend {
                session,
                seq,
                operation,
                cause,
            } => write!(
                f,
                "cannot finalize after failure to append {operation} at event sequence {seq}: \
                 {cause}. The record's durability is unknown. Retrying in this process cannot \
                 help. After resolving the store error, run `ego resume {session}` to reopen \
                 the saved journal before repeating any action."
            ),
        }
    }
}

impl std::error::Error for SharedJournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::InUse | Self::Synchronization | Self::UncertainAppend { .. } => None,
        }
    }
}

fn uncertain_append_error(message: &'static str) -> StoreError {
    StoreError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

impl From<StoreError> for SharedJournalError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<SharedJournalError> for StoreError {
    fn from(error: SharedJournalError) -> Self {
        match error {
            SharedJournalError::Store(error) => error,
            error => StoreError::Io(std::io::Error::other(error.to_string())),
        }
    }
}

impl<'s> SharedJournal<'s> {
    /// Wrap the session's sole journal in cloneable append authority.
    pub fn new(journal: Journal<'s>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(journal)),
        }
    }

    pub(crate) fn clock(&self) -> &'s dyn Clock {
        self.lock().expect("journal clock").clock
    }

    pub fn session_id(&self) -> Result<SessionId, SharedJournalError> {
        Ok(self.lock()?.session)
    }

    pub fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn publish(&self, event: &PublicEvent) -> Result<(), SharedJournalError> {
        self.lock()?.publish(event);
        Ok(())
    }

    pub(crate) fn wants_exec_progress(&self) -> bool {
        self.lock()
            .is_ok_and(|journal| journal.sink.wants_exec_progress())
    }

    pub(crate) fn exec_progress(&self, item: ItemId, elapsed: std::time::Duration, output: &[u8]) {
        if let Ok(journal) = self.lock() {
            journal.sink.exec_progress(item, elapsed, output);
        }
    }

    pub fn cache_observed(
        &self,
        prefix: &crate::generator::attempt::CachePrefix,
        usage: Option<crate::state::Usage>,
    ) -> Result<(), SharedJournalError> {
        self.lock()?.sink.cache_observed(prefix, usage);
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Journal<'s>>, SharedJournalError> {
        self.inner
            .lock()
            .map_err(|_| SharedJournalError::Synchronization)
    }

    /// Read-only egress view of this session's committed filter namespace.
    /// Session-start hooks may run before the first user prompt exists.
    pub fn egress_filter_snapshot(
        &self,
        namespace: &str,
    ) -> Result<
        (
            crate::generator::filter_state::FilterStateSnapshot,
            Option<TurnId>,
        ),
        &'static str,
    > {
        let journal = self
            .lock()
            .map_err(|_| "protocol_filter journal unavailable")?;
        if journal.uncertain_append.is_some() {
            return Err("protocol_filter journal state unavailable");
        }
        let key = format!("{}:{namespace}", journal.session);
        let mut turns: std::collections::HashSet<_> = journal
            .state
            .items
            .values()
            .filter(|item| item.kind == crate::state::ItemKind::UserPrompt)
            .map(|item| item.turn)
            .collect();
        turns.extend(direct_skill_turns(&journal.state));
        Ok((
            crate::generator::filter_state::FilterStateSnapshot {
                session_id: journal.session,
                turn_index: (turns.len() as u64).saturating_sub(1),
                state: journal
                    .state
                    .protocol_filter_variables
                    .get(&key)
                    .cloned()
                    .unwrap_or_default(),
            },
            journal
                .state
                .items
                .values()
                .max_by_key(|item| item.seq)
                .map(|item| item.turn),
        ))
    }

    /// Append through the existing journal sequence.
    pub fn filter_state(
        &self,
        turn: TurnId,
        role: crate::event::domain::GenerationRole,
        exchange: crate::generator::filter_state::FilterStateExchange,
    ) -> Result<(), StoreError> {
        use crate::generator::filter_state::{FilterStateOperation, FilterStateSnapshot};
        let Some((namespace, operation, reply)) = exchange.take() else {
            return Ok(());
        };
        let mut journal = match self.lock() {
            Ok(journal) => journal,
            Err(error) => {
                let _ = reply.send(Err("protocol_filter journal unavailable"));
                return Err(error.into());
            }
        };
        if journal.uncertain_append.is_some()
            || namespace.len() != 64
            || !namespace.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            let _ = reply.send(Err("protocol_filter journal state unavailable"));
            return Ok(());
        }
        // Lineage replay may include a parent's events. Those variables do
        // not belong to this new session instance.
        let key = format!("{}:{namespace}", journal.session);
        let current = journal
            .state
            .protocol_filter_variables
            .get(&key)
            .cloned()
            .unwrap_or_default();
        // Fork and compaction journals contain local events only. A lineage
        // sequence belongs to the source journal, not this journal's ordering.
        let skill_turns = direct_skill_turns(&journal.state);
        let has_source = journal
            .state
            .items
            .values()
            .any(|item| item.kind == crate::state::ItemKind::UserPrompt)
            || !skill_turns.is_empty()
            || (matches!(
                journal.state.lineage,
                Some(crate::session::SessionLineage::Compaction { .. })
            ) && journal
                .state
                .items
                .values()
                .any(|item| item.kind == crate::state::ItemKind::ContextCheckpoint));
        let first_prompt = journal
            .state
            .items
            .values()
            .filter(|item| item.turn == turn && item.kind == crate::state::ItemKind::UserPrompt)
            .map(|item| item.seq)
            .min();
        let boundary = match first_prompt {
            Some(first_prompt) => first_prompt,
            None if skill_turns.contains(&turn) => {
                // A directly invoked skill or command is this turn's user input.
                // Its instruction is the latest durable event of the turn.
                let mut boundary = journal.state.last_seq;
                boundary.next()
            }
            None if (role == crate::event::domain::GenerationRole::Compaction
                || (matches!(
                    role,
                    crate::event::domain::GenerationRole::Main
                        | crate::event::domain::GenerationRole::Judge
                        | crate::event::domain::GenerationRole::GoalCheck
                ) && (journal
                    .state
                    .turn(turn)
                    .is_some_and(|state| state.checkpoint_continued)
                    || journal.state.items.values().any(|item| {
                        item.turn == turn
                            && matches!(
                                item.kind,
                                crate::state::ItemKind::GoalContinuation
                                    | crate::state::ItemKind::DetachedExecNotification
                                    | crate::state::ItemKind::HostContext
                            )
                    }))))
                && has_source =>
            {
                // Durable source provenance permits host continuation without
                // inventing a user prompt. Count only this session's user turns;
                // variables remain isolated by the current journal session ID.
                let mut boundary = journal.state.last_seq;
                boundary.next()
            }
            None => {
                let _ = reply.send(Err("protocol_filter requires a committed user turn"));
                return Ok(());
            }
        };
        let mut preceding_turns = journal
            .state
            .items
            .values()
            .filter(|item| {
                item.kind == crate::state::ItemKind::UserPrompt
                    && item.seq < boundary
                    && item.turn != turn
            })
            .map(|item| item.turn)
            .collect::<std::collections::HashSet<_>>();
        preceding_turns.extend(skill_turns.iter().filter(|id| **id != turn));
        if matches!(
            journal.state.lineage,
            Some(crate::session::SessionLineage::Compaction { .. })
        ) {
            // A continued checkpoint owns a real filter turn even without a
            // user prompt. Input delivered on that turn must count only once.
            preceding_turns.extend(journal.state.turns.iter().filter_map(|(id, state)| {
                (state.checkpoint_continued && *id != turn).then_some(*id)
            }));
        }
        let turn_index = preceding_turns.len() as u64;
        if let FilterStateOperation::Save {
            expected_revision,
            writes,
        } = operation
        {
            if expected_revision != current.revision {
                let _ = reply.send(Err("protocol_filter state revision conflict"));
                return Ok(());
            }
            let next = match current.proposed(&writes) {
                Ok(next) => next,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return Ok(());
                }
            };
            if next != current {
                let session_id = journal.session;
                if let Err(error) = journal.append(
                    turn,
                    None,
                    None,
                    EventKind::ProtocolFilterVariablesSaved {
                        session_id,
                        namespace: namespace.clone(),
                        expected_revision,
                        state: next,
                    },
                ) {
                    let _ = reply.send(Err("protocol_filter session state persistence failed"));
                    return Err(error);
                }
            }
        }
        let _ = reply.send(Ok(FilterStateSnapshot {
            session_id: journal.session,
            turn_index,
            state: journal
                .state
                .protocol_filter_variables
                .get(&key)
                .cloned()
                .unwrap_or_default(),
        }));
        Ok(())
    }

    /// Append through the existing journal sequence.
    pub fn append(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        kind: EventKind,
    ) -> Result<EventId, SharedJournalError> {
        self.lock()?
            .append(turn, item, parent, kind)
            .map_err(Into::into)
    }

    /// Admit and publish one model worklist request under the sole state lock.
    /// Only actually running task futures can provide concurrency exemptions.
    pub fn worklist(
        &self,
        turn: TurnId,
        update: crate::plan::PlanUpdate,
    ) -> Result<Result<crate::plan::Plan, String>, SharedJournalError> {
        let mut journal = self.lock()?;
        let live = journal.worklist_executions.values().cloned().collect();
        let next = match journal.state.plan.admitted(&update, &live) {
            Ok(next) => next,
            Err(error) => return Ok(Err(error)),
        };
        for (item, id) in &journal.worklist_executions {
            let old = journal.state.plan.steps.iter().find(|step| &step.id == id);
            let new = next.steps.iter().find(|step| &step.id == id);
            if old != new {
                return Ok(Err(format!(
                    "worklist action {id} is owned by running task {item}; wait for its settlement"
                )));
            }
        }
        journal.append(turn, None, None, EventKind::PlanUpdated { update })?;
        Ok(Ok(journal.state.plan.clone()))
    }

    /// Bind only when the task future is polled and owns execution, not at preparation.
    pub(crate) fn start_worklist_task(
        &self,
        turn: TurnId,
        item: ItemId,
        parent: EventId,
        id: String,
    ) -> Result<Result<WorklistTask<'s>, String>, SharedJournalError> {
        let mut journal = self.lock()?;
        let valid_item = journal.state.items.get(&item).is_some_and(|call| {
            call.tool_name.as_deref() == Some("task")
                && call.ok.is_none()
                && call.tool_call_started == Some(parent)
                && serde_json::from_str::<serde_json::Value>(&call.args)
                    .ok()
                    .is_some_and(|args| {
                        args.get("worklist_step")
                            .and_then(serde_json::Value::as_str)
                            == Some(id.as_str())
                            && args.get("background").and_then(serde_json::Value::as_bool)
                                != Some(true)
                    })
        });
        if !valid_item
            || journal.worklist_executions.contains_key(&item)
            || journal
                .worklist_executions
                .values()
                .any(|bound| bound == &id)
        {
            return Ok(Err(format!(
                "worklist action {id} needs a unique live foreground task execution"
            )));
        }
        let mut live: std::collections::BTreeSet<_> =
            journal.worklist_executions.values().cloned().collect();
        live.insert(id.clone());
        let update = crate::plan::PlanUpdate::Step {
            id: id.clone(),
            status: crate::plan::StepStatus::InProgress,
            reason: None,
        };
        if let Err(error) = journal.state.plan.admitted(&update, &live) {
            return Ok(Err(error));
        }
        journal.append(turn, None, Some(parent), EventKind::PlanUpdated { update })?;
        journal.worklist_executions.insert(item, id);
        Ok(Ok(WorklistTask {
            journal: self.clone(),
            turn,
            item,
            parent,
            settled: false,
        }))
    }

    fn settle_worklist_task(
        &self,
        turn: TurnId,
        item: ItemId,
        parent: EventId,
        success: bool,
        reason: &str,
    ) -> Result<(), SharedJournalError> {
        let mut journal = self.lock()?;
        let Some(id) = journal.worklist_executions.get(&item).cloned() else {
            return Ok(());
        };
        // Remove live ownership even if persistence fails: an ended future is not live.
        journal.worklist_executions.remove(&item);
        if journal
            .state
            .plan
            .steps
            .iter()
            .any(|step| step.id == id && step.status == crate::plan::StepStatus::InProgress)
        {
            let update = crate::plan::PlanUpdate::Step {
                id,
                status: if success {
                    crate::plan::StepStatus::Completed
                } else {
                    crate::plan::StepStatus::Blocked
                },
                reason: (!success).then(|| reason.to_owned()),
            };
            let live = journal.worklist_executions.values().cloned().collect();
            journal
                .state
                .plan
                .admitted(&update, &live)
                .map_err(|error| StoreError::Io(std::io::Error::other(error)))?;
            journal.append(turn, None, Some(parent), EventKind::PlanUpdated { update })?;
        }
        Ok(())
    }

    /// Explicit turn cancellation is host-owned and does not depend on a stale clone.
    pub(crate) fn cancel_worklist(&self, turn: TurnId) -> Result<(), SharedJournalError> {
        let mut journal = self.lock()?;
        let mut steps = journal.state.plan.steps.clone();
        let mut changed = false;
        for step in &mut steps {
            if matches!(
                step.status,
                crate::plan::StepStatus::Pending | crate::plan::StepStatus::InProgress
            ) {
                step.status = crate::plan::StepStatus::Cancelled;
                step.reason = None;
                changed = true;
            }
        }
        if changed {
            let update = crate::plan::PlanUpdate::Replace { steps };
            let live = journal.worklist_executions.values().cloned().collect();
            journal
                .state
                .plan
                .admitted(&update, &live)
                .map_err(|error| StoreError::Io(std::io::Error::other(error)))?;
            journal.append(turn, None, None, EventKind::PlanUpdated { update })?;
        }
        Ok(())
    }

    /// Model goal operations share the journal lock with native lifecycle changes.
    pub(crate) fn model_goal(
        &self,
        turn: TurnId,
        request: crate::goal::GoalRequest,
        ceilings: &crate::ceiling::Ceilings,
    ) -> Result<Result<crate::goal::GoalRecord, crate::goal::GoalRefusal>, SharedJournalError> {
        use crate::goal::{GoalOwner, GoalRefusal, GoalRequest, GoalState};
        if let GoalRequest::Cancel { goal_id } = request {
            return self.cancel_goal(turn, goal_id, GoalOwner::Automated);
        }
        let mut journal = self.lock()?;
        let kind = match request {
            GoalRequest::Create { objective } => {
                if journal
                    .state
                    .goal
                    .as_ref()
                    .is_some_and(|goal| goal.state == GoalState::Active)
                {
                    return Ok(Err(GoalRefusal::ActiveGoalExists));
                }
                if journal.state.automatic_work_paused() {
                    return Ok(Err(GoalRefusal::AutomaticWorkPaused));
                }
                EventKind::GoalSet {
                    id: crate::event::id::GoalId::new(),
                    objective,
                    owner: GoalOwner::Automated,
                    ceilings: ceilings.clone(),
                    derived_from: None,
                }
            }
            GoalRequest::Cancel { .. } => unreachable!("cancel delegates before taking the lock"),
        };
        journal.append(turn, None, None, kind)?;
        Ok(Ok(journal
            .state
            .goal
            .clone()
            .expect("committed goal operation")))
    }

    /// Cancel the exact current goal under the session's sole state lock.
    /// A duplicate cancellation is harmless; stale ids and widened ownership refuse.
    pub fn cancel_goal(
        &self,
        turn: TurnId,
        goal_id: crate::event::id::GoalId,
        cancelled_by: crate::goal::GoalOwner,
    ) -> Result<Result<crate::goal::GoalRecord, crate::goal::GoalRefusal>, SharedJournalError> {
        use crate::goal::{GoalRefusal, GoalState, GoalTransition};
        let mut journal = self.lock()?;
        let Some(goal) = journal.state.goal.as_ref() else {
            return Ok(Err(GoalRefusal::NoActiveGoal));
        };
        if goal.id != goal_id {
            return Ok(Err(GoalRefusal::StaleGoal));
        }
        if let Err(error) = goal.admits_cancellation_by(cancelled_by) {
            return Ok(Err(error));
        }
        if goal.state == GoalState::Cancelled {
            return Ok(Ok(goal.clone()));
        }
        let transition = GoalTransition::Cancelled { cancelled_by };
        if let Err(error) = goal.admits(&transition) {
            return Ok(Err(error));
        }
        journal.append(
            turn,
            None,
            None,
            EventKind::GoalTransitioned {
                id: goal_id,
                transition,
            },
        )?;
        Ok(Ok(journal
            .state
            .goal
            .clone()
            .expect("committed goal cancellation")))
    }

    /// Linearize a goal-check result or continuation against native cancellation.
    /// False means cancellation/replacement won; no stale event is published.
    pub(crate) fn append_goal_event(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        goal_id: crate::event::id::GoalId,
        kind: EventKind,
    ) -> Result<bool, SharedJournalError> {
        let mut journal = self.lock()?;
        if !journal.state.goal.as_ref().is_some_and(|goal| {
            goal.id == goal_id
                && (goal.state == crate::goal::GoalState::Active
                    || (goal.state == crate::goal::GoalState::Blocked
                        && matches!(kind, EventKind::GoalCheckStopped { .. })))
        }) {
            return Ok(false);
        }
        journal.append(turn, item, None, kind)?;
        Ok(true)
    }

    /// Validate and durably update pins under the session's sole state lock.
    pub fn pins(
        &self,
        command: crate::compact::PinCommand,
    ) -> Result<String, crate::compact::PinError> {
        use crate::compact::{PinCommand, PinError};
        let mut journal = self.lock().map_err(PinError::Journal)?;
        if command == PinCommand::List {
            return Ok(journal.state.constraints.listing());
        }
        if !journal.state.live_attempts.is_empty()
            || !journal.state.unacknowledged_prompts().is_empty()
            || journal.state.pending_question().is_some()
        {
            return Err(PinError::Invalid(
                "cannot change pins while work is pending".into(),
            ));
        }
        let constraints = journal
            .state
            .constraints
            .changed(&command)
            .map_err(PinError::Invalid)?;
        journal
            .append(
                TurnId::new(),
                None,
                None,
                EventKind::PinsChanged { constraints },
            )
            .map_err(|error| PinError::Journal(error.into()))?;
        Ok(journal.state.constraints.listing())
    }

    /// Persist and publish the anomaly once, atomically with its replayed key.
    pub fn cost_anomaly(
        &self,
        turn: TurnId,
        model: &str,
        estimated_upper_bound: Option<crate::cost::Money>,
        cause: Option<crate::cost::PricingUnavailable>,
    ) -> Result<(), SharedJournalError> {
        let mut journal = self.lock()?;
        if !journal.state.cost_anomaly_models.contains(model) {
            journal.append(
                turn,
                None,
                None,
                EventKind::CostAnomaly {
                    model: model.to_owned(),
                    estimated_upper_bound,
                    cause,
                },
            )?;
        }
        Ok(())
    }

    /// Compare, append, and fold one lease-bound lifecycle event while the
    /// session's sole journal authority is locked.
    pub(crate) fn compare_and_append_lifecycle(
        &self,
        append: &dyn LeasedLifecycleAppend,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        self.lock()
            .map_err(|error| LifecycleAppendError::Store(error.into()))?
            .compare_and_append_lifecycle(append, expected, event)
    }

    /// Fold and project one exact event that the store reveals after an
    /// append returned an uncertain error. This never writes; it only brings
    /// the sole in-memory journal owner back to the durable sequence.
    pub(crate) fn reconcile_durable(&self, event: &DomainEvent) -> Result<(), SharedJournalError> {
        self.lock()?.reconcile_durable(event).map_err(Into::into)
    }

    /// Append a subagent settlement, or retry the exact same event after its
    /// caller has confirmed that a prior uncertain append is absent.
    pub(crate) fn append_or_retry_settlement(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        kind: EventKind,
    ) -> Result<EventId, SharedJournalError> {
        self.lock()?
            .append_or_retry_settlement(turn, item, parent, kind)
            .map_err(Into::into)
    }

    /// Durably freeze a lifecycle plan through the existing journal.
    pub fn hook_reached(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        occurrence: HookOccurrenceKey,
        subject: HookSubject,
        plan: HookPlan,
    ) -> Result<HookReachedEvidence, SharedJournalError> {
        self.lock()?
            .hook_reached(turn, item, parent, occurrence, subject, plan)
            .map_err(Into::into)
    }

    /// Durably record one hook delivery boundary through the existing journal.
    #[allow(clippy::too_many_arguments)]
    pub fn hook_started(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        reached: &HookReachedEvidence,
        hook: HookId,
        invocation: HookInvocationId,
        activation: HookActivation,
    ) -> Result<HookStartedEvidence, SharedJournalError> {
        self.lock()?
            .hook_started(turn, item, reached, hook, invocation, activation)
            .map_err(Into::into)
    }

    /// Durably settle one hook start through the existing journal.
    pub fn hook_settled(
        &self,
        turn: TurnId,
        item: Option<ItemId>,
        started: HookStartedEvidence,
        result: HookResult,
    ) -> Result<EventId, SharedJournalError> {
        self.lock()?
            .hook_settled(turn, item, started, result)
            .map_err(Into::into)
    }

    /// Settle attempts inherited from a previous process through the sole journal.
    pub(crate) fn recover_interrupted_attempts(
        &self,
        turn: TurnId,
    ) -> Result<(), SharedJournalError> {
        self.lock()?
            .recover_interrupted_attempts(turn)
            .map_err(Into::into)
    }

    /// Inspect current state without allowing a lock guard or reference to escape.
    pub fn inspect<T>(
        &self,
        inspect: impl FnOnce(&SessionState) -> T,
    ) -> Result<T, SharedJournalError> {
        let journal = self.lock()?;
        Ok(inspect(journal.state()))
    }

    /// Recover the exact final state once every other append handle is gone.
    pub fn try_into_state(self) -> Result<SessionState, SharedJournalError> {
        let journal = Arc::try_unwrap(self.inner).map_err(|_| SharedJournalError::InUse)?;
        let journal = journal
            .into_inner()
            .map_err(|_| SharedJournalError::Synchronization)?;
        if let Some(pending) = journal.uncertain_append {
            let event = pending.event;
            let operation = match event.kind {
                EventKind::HookLifecycleReached { occurrence, .. } => {
                    format!("{} hook reach", occurrence.lifecycle().as_str())
                }
                EventKind::HookInvocationStarted { lifecycle, .. } => {
                    format!("{} hook start", lifecycle.as_str())
                }
                EventKind::HookInvocationSettled { .. } => "hook settlement".to_owned(),
                _ => "session event".to_owned(),
            };
            return Err(SharedJournalError::UncertainAppend {
                session: journal.session,
                seq: event.env.seq,
                operation,
                cause: pending.cause,
            });
        }
        Ok(journal.into_state())
    }
}

/// A task's concurrency exception cannot outlive its owning future.
pub(crate) struct WorklistTask<'s> {
    journal: SharedJournal<'s>,
    turn: TurnId,
    item: ItemId,
    parent: EventId,
    settled: bool,
}
impl WorklistTask<'_> {
    pub(crate) fn finish(mut self, success: bool) -> Result<(), SharedJournalError> {
        self.settled = true;
        self.journal.settle_worklist_task(
            self.turn,
            self.item,
            self.parent,
            success,
            "child task returned a failure or was cancelled",
        )
    }
}
impl Drop for WorklistTask<'_> {
    fn drop(&mut self) {
        if !self.settled
            && let Err(error) = self.journal.settle_worklist_task(
                self.turn,
                self.item,
                self.parent,
                false,
                "child task execution was interrupted",
            )
        {
            tracing::warn!(%error, "interrupted worklist task settlement could not be persisted");
        }
    }
}

impl HookJournalPort for SharedJournal<'_> {
    fn reached(
        &self,
        producer: &Producer,
        occurrence: &HookOccurrence,
        plan: &HookPlan,
    ) -> Result<HookReachOutcome, HookReachError> {
        let mut journal = self
            .lock()
            .map_err(|error| HookReachError::Absent(HookRunError::new(error.to_string())))?;
        if let HookOccurrenceOwner::Continuation { id } = occurrence.key().owner() {
            let Some(item) = occurrence.causality().item_id() else {
                return Err(HookReachError::Absent(HookRunError::new(
                    "hook continuation has no causal tool item",
                )));
            };
            let valid_parent = match occurrence.key().lifecycle() {
                HookLifecycle::Elicitation => {
                    journal.state().items.get(&item).is_some_and(|tool| {
                        tool.kind == ItemKind::ToolCall
                            && tool.tool_call_started == occurrence.causality().parent()
                    })
                }
                HookLifecycle::ElicitationResult => {
                    occurrence.causality().parent().is_some_and(|parent| {
                        journal.state().owns_foreign_continuation_settlement(
                            id,
                            occurrence.causality().turn(),
                            item,
                            parent,
                        )
                    })
                }
                _ => false,
            };
            if !valid_parent {
                return Err(HookReachError::Absent(HookRunError::new(
                    "hook continuation does not name its exact lifecycle parent",
                )));
            }
        }
        if let Some(reached) = journal.state().hook_occurrence(occurrence.key()) {
            if reached.subject() != occurrence.event().subject()
                || reached.producer() != producer
                || reached.turn() != occurrence.causality().turn()
                || reached.item() != occurrence.causality().item_id()
            {
                return Err(HookReachError::Absent(HookRunError::new(
                    "durable hook reach evidence does not match its causal occurrence",
                )));
            }
            let evidence = HookReachedEvidence::new(
                reached.event(),
                reached.key().clone(),
                reached.subject(),
                reached.producer().clone(),
                reached.plan().clone(),
            );
            let starts = reached
                .starts()
                .iter()
                .map(|started| {
                    HookStartedEvidence::new(
                        started.event(),
                        reached.event(),
                        started.hook(),
                        started.invocation(),
                        started.activation(),
                    )
                })
                .collect();
            let settlements = journal
                .state()
                .hook_occurrence_settlements(reached.event())
                .into_iter()
                .map(|settled| {
                    HookSettledEvidence::new(
                        settled.event(),
                        settled.occurrence(),
                        settled.hook(),
                        settled.invocation(),
                        settled.result().clone(),
                    )
                })
                .collect();
            return Ok(HookReachOutcome::Recovered {
                evidence,
                starts,
                settlements,
            });
        }

        journal
            .hook_reached(
                occurrence.causality().turn(),
                occurrence.causality().item_id(),
                occurrence.causality().parent(),
                occurrence.key().clone(),
                occurrence.event().subject(),
                plan.clone(),
            )
            .map(HookReachOutcome::Fresh)
            .map_err(|error| HookReachError::Absent(HookRunError::new(error.to_string())))
    }

    fn started(
        &self,
        spec: &HookSpec,
        occurrence: &HookOccurrence,
        reached: &HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> Result<HookStartedEvidence, HookStartError> {
        self.lock()
            .map_err(|error| HookStartError::Absent(HookRunError::new(error.to_string())))?
            .hook_started(
                occurrence.causality().turn(),
                occurrence.causality().item_id(),
                reached,
                spec.id(),
                invocation,
                activation,
            )
            .map_err(|error| HookStartError::Absent(HookRunError::new(error.to_string())))
    }

    fn settled(
        &self,
        occurrence: &HookOccurrence,
        started: HookStartedEvidence,
        result: &HookResult,
    ) -> Result<EventId, HookRunError> {
        let mut journal = self
            .lock()
            .map_err(|error| HookRunError::new(error.to_string()))?;
        let turn = occurrence.causality().turn();
        let hook = started.hook();
        let settled = journal
            .hook_settled(
                turn,
                occurrence.causality().item_id(),
                started,
                result.clone(),
            )
            .map_err(|error| HookRunError::new(error.to_string()))?;
        if let HookResult::Failure { failure } = result {
            let kind = serde_json::to_value(failure.kind())
                .ok()
                .and_then(|kind| kind.as_str().map(str::to_owned))
                .unwrap_or_default();
            let cause = failure.feedback().map_or_else(
                || {
                    use crate::event::hook::HookFailureKind;
                    let reason = match failure.kind() {
                        HookFailureKind::EffectDenied => "the hook effect was denied",
                        HookFailureKind::TimedOut => "the hook timed out",
                        HookFailureKind::Cancelled => "the hook was cancelled",
                        HookFailureKind::MalformedResult => "the hook result was malformed",
                        HookFailureKind::BackendUnavailable => "the hook backend was unavailable",
                        HookFailureKind::HandlerFailed => "the hook handler failed",
                        HookFailureKind::EvidenceFailed => "hook evidence could not be recorded",
                        HookFailureKind::DeliveryUncertain => {
                            "hook delivery could not be confirmed"
                        }
                    };
                    format!(": {reason}")
                },
                |feedback| format!(": {}", feedback.head()),
            );
            let message = format!(
                "{} hook {hook} failed to run ({kind}){cause}",
                occurrence.key().lifecycle().as_str()
            );
            // The settlement is durable; a lost notice must not turn it into uncertainty.
            if let Err(error) = journal.append_with_producer(
                turn,
                None,
                Some(settled),
                Producer::Hook(hook),
                EventKind::Error {
                    message,
                    kind: crate::generator::types::ErrorKind::Unclassified,
                },
            ) {
                tracing::warn!(%error, "hook failure notice was not recorded");
            }
        }
        Ok(settled)
    }
}

impl<'a> Journal<'a> {
    /// Open a journal over `state`, continuing its sequence.
    pub fn new(
        session: SessionId,
        store: &'a dyn SessionStore,
        clock: &'a dyn Clock,
        sink: &'a dyn EventSink,
        producer: Producer,
        state: SessionState,
    ) -> Self {
        let mut interrupted_attempts: Vec<_> = state
            .generations
            .iter()
            .filter(|(_, attempt)| state.live_attempts.contains_key(&attempt.attempt()))
            .map(|(event, attempt)| (*event, *attempt))
            .collect();
        interrupted_attempts.sort_by_key(|(_, attempt)| *attempt);
        Self {
            session,
            store,
            clock,
            sink,
            producer,
            interrupted_attempts,
            worklist_executions: Default::default(),
            interrupted_tasks: crate::model_catalogue::active_foreground_tasks(&state)
                .into_iter()
                .filter_map(|task| task.id.parse().ok())
                .collect(),
            seq: state.last_seq,
            state,
            uncertain_append: None,
        }
    }

    fn recover_interrupted_attempts(&mut self, turn: TurnId) -> Result<(), StoreError> {
        use crate::cost::{CostDelta, UnknownCost};
        use crate::generator::attempt::{DeliveryCertainty, Outcome, UsageReport};
        use crate::generator::types::{ErrorKind, StopReason};
        use crate::state::{ChargeAttribution, Usage};

        while let Some(&item) = self.interrupted_tasks.last() {
            if let Some(call) = self.state.items.get(&item).filter(|call| call.ok.is_none()) {
                let parent = call.tool_call_started;
                let id = serde_json::from_str::<serde_json::Value>(&call.args)
                    .ok()
                    .and_then(|args| {
                        args.get("worklist_step")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    });
                if let Some(id) = id.filter(|id| {
                    self.state.plan.steps.iter().any(|step| {
                        &step.id == id && step.status == crate::plan::StepStatus::InProgress
                    })
                }) {
                    self.append(
                        turn,
                        None,
                        parent,
                        EventKind::PlanUpdated {
                            update: crate::plan::PlanUpdate::Step {
                                id,
                                status: crate::plan::StepStatus::Blocked,
                                reason: Some(
                                    "task execution interrupted by the previous process ending"
                                        .into(),
                                ),
                            },
                        },
                    )?;
                }
                self.append(
                    turn,
                    Some(item),
                    parent,
                    EventKind::ToolCallCompleted {
                        ok: false,
                        result: crate::permit::ToolResult::builtin(
                            "task execution interrupted by the previous process ending",
                        ),
                    },
                )?;
            }
            self.interrupted_tasks.pop();
        }
        while let Some(&(parent, attempt)) = self.interrupted_attempts.last() {
            if !self.state.live_attempts.contains_key(&attempt.attempt()) {
                self.interrupted_attempts.pop();
                continue;
            }
            let mut usage = None;
            for charge in &self.state.charges {
                if matches!(charge.attribution, ChargeAttribution::Generation { attempt: owner, .. } if owner == attempt)
                {
                    usage.get_or_insert(Usage::IDENTITY).add(&charge.usage);
                }
            }
            self.append(turn, None, Some(parent), EventKind::Error {
                message: format!("Attempt {} was interrupted by the previous process ending. Its request may have reached the provider; it will not be replayed automatically. Only your next explicit request will run.", attempt.attempt().uuid()),
                kind: ErrorKind::Unclassified,
            })?;
            if usage.is_none() {
                self.append(
                    turn,
                    None,
                    Some(parent),
                    EventKind::UsageUpdated {
                        usage: Usage::IDENTITY,
                        cost_delta: CostDelta::Unknown(UnknownCost::ProviderReportedNoUsage),
                        attempt: Some(attempt),
                    },
                )?;
            }
            self.append(
                turn,
                None,
                Some(parent),
                EventKind::AttemptSettled {
                    attempt,
                    outcome: Outcome::new(
                        StopReason::Aborted,
                        DeliveryCertainty::PossiblySent,
                        usage.map_or(UsageReport::Missing, UsageReport::Reported),
                    ),
                },
            )?;
            self.interrupted_attempts.pop();
        }
        Ok(())
    }

    /// Append one event to the log and reduce it.
    ///
    /// Returns the new event's own [`EventId`], which is what a later event
    /// names as its causal parent.
    pub fn append(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        kind: EventKind,
    ) -> Result<EventId, StoreError> {
        self.append_with_producer(turn, item, parent, self.producer.clone(), kind)
    }

    fn append_with_producer(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        producer: Producer,
        kind: EventKind,
    ) -> Result<EventId, StoreError> {
        if self.uncertain_append.is_some() {
            return Err(uncertain_append_error(
                "the journal has an unresolved uncertain append",
            ));
        }
        let id = EventId::new();
        let mut next = self.seq;
        let event = DomainEvent {
            env: Envelope {
                seq: next.next(),
                id,
                turn,
                item,
                parent,
                ts: self.clock.now(),
                producer,
            },
            kind,
        };
        if let Err(error) = self.store.append(&self.session, &event) {
            self.uncertain_append = Some(UncertainAppend {
                event,
                cause: error.to_string(),
            });
            return Err(error);
        }
        self.commit(event);
        Ok(id)
    }

    fn compare_and_append_lifecycle(
        &mut self,
        append: &dyn LeasedLifecycleAppend,
        expected: Seq,
        event: DomainEvent,
    ) -> Result<LifecyclePublication, LifecycleAppendError> {
        if self.uncertain_append.is_some() {
            return Err(LifecycleAppendError::Store(uncertain_append_error(
                "the journal has an unresolved uncertain append",
            )));
        }
        let mut next = expected;
        if expected != self.seq || self.state.last_seq != self.seq || event.env.seq != next.next() {
            return Err(LifecycleAppendError::Conflict);
        }

        let publication = append.compare_and_append_lifecycle(expected, event.clone())?;
        if publication.is_visible() {
            self.commit(event);
        }
        Ok(publication)
    }

    fn append_or_retry_settlement(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        kind: EventKind,
    ) -> Result<EventId, StoreError> {
        if !matches!(&kind, EventKind::SubagentSettled { .. }) {
            return Err(uncertain_append_error(
                "only a subagent settlement can retry an uncertain append",
            ));
        }
        let Some(event) = self
            .uncertain_append
            .as_ref()
            .map(|pending| pending.event.clone())
        else {
            return self.append(turn, item, parent, kind);
        };
        if event.env.turn != turn
            || event.env.item != item
            || event.env.parent != parent
            || event.env.producer != self.producer
            || event.kind != kind
        {
            return Err(uncertain_append_error(
                "a different event cannot replace the journal's uncertain append",
            ));
        }
        self.store.append(&self.session, &event)?;
        let id = event.env.id;
        self.uncertain_append = None;
        self.commit(event);
        Ok(id)
    }

    fn commit(&mut self, event: DomainEvent) {
        self.seq = event.env.seq;
        self.state.apply(&event);
        if let Some(public) = project_public(&event) {
            self.sink.publish(&public, &self.state);
        }
        if let Some(child) = crate::public::project_child_lifecycle(&event) {
            self.sink.publish(&child, &self.state);
        }
    }

    fn reconcile_durable(&mut self, event: &DomainEvent) -> Result<(), StoreError> {
        if !matches!(&event.kind, EventKind::SubagentSettled { .. }) {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "only a durable subagent settlement can reconcile an uncertain append",
            )));
        }
        if self.uncertain_append.as_ref().map(|pending| &pending.event) != Some(event) {
            return Err(uncertain_append_error(
                "durable settlement is not the journal's exact uncertain event",
            ));
        }
        let mut expected = self.state.last_seq;
        if self.seq != self.state.last_seq || event.env.seq != expected.next() {
            return Err(uncertain_append_error(
                "durable settlement is not the journal's exact uncertain append",
            ));
        }
        self.uncertain_append = None;
        self.commit(event.clone());
        Ok(())
    }

    /// Durably freeze the exact ordered plan at a real owner's lifecycle point.
    pub fn hook_reached(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        occurrence: HookOccurrenceKey,
        subject: HookSubject,
        plan: HookPlan,
    ) -> Result<HookReachedEvidence, StoreError> {
        let producer = self.producer.clone();
        let event = self.append_with_producer(
            turn,
            item,
            parent,
            producer.clone(),
            EventKind::HookLifecycleReached {
                occurrence: occurrence.clone(),
                subject,
                plan: plan.clone(),
            },
        )?;
        Ok(HookReachedEvidence::new(
            event, occurrence, subject, producer, plan,
        ))
    }

    /// Durably record the external-delivery boundary for one planned hook.
    #[allow(clippy::too_many_arguments)]
    pub fn hook_started(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        reached: &HookReachedEvidence,
        hook: HookId,
        invocation: HookInvocationId,
        activation: HookActivation,
    ) -> Result<HookStartedEvidence, StoreError> {
        let event = self.append_with_producer(
            turn,
            item,
            Some(reached.event()),
            reached.producer().clone(),
            EventKind::HookInvocationStarted {
                occurrence: reached.event(),
                hook,
                invocation,
                lifecycle: reached.occurrence().lifecycle(),
                subject: reached.subject(),
                activation,
            },
        )?;
        Ok(HookStartedEvidence::new(
            event,
            reached.event(),
            hook,
            invocation,
            activation,
        ))
    }

    /// Durably settle one start under the hook producer that performed it.
    pub fn hook_settled(
        &mut self,
        turn: TurnId,
        item: Option<ItemId>,
        started: HookStartedEvidence,
        result: HookResult,
    ) -> Result<EventId, StoreError> {
        self.append_with_producer(
            turn,
            item,
            Some(started.event()),
            Producer::Hook(started.hook()),
            EventKind::HookInvocationSettled {
                occurrence: started.occurrence(),
                hook: started.hook(),
                invocation: started.invocation(),
                result,
            },
        )
    }

    /// Publish an event the reducer cannot produce.
    ///
    /// `SessionStarted` and `TurnStarted` have no `EventKind` counterpart —
    /// they are facts the harness knows before any domain event exists (see
    /// `public/mod.rs`) — so they reach the sink through here rather than
    /// through [`append`](Self::append). Nothing is written to the log,
    /// because there is nothing to write: an unwritten event cannot be
    /// published out of order with one that was.
    pub fn publish(&self, event: &PublicEvent) {
        self.sink.publish(event, &self.state);
    }

    /// The reduction of everything appended so far.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Take the state back out, for a caller that wants to continue the
    /// session with another turn.
    pub fn into_state(self) -> SessionState {
        self.state
    }
}
