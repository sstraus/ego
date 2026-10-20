//! Durable isolated children over the ordinary rooted run controller.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::lock::Mutex as AsyncMutex;

use crate::budget::SharedBudgetGate;
use crate::digest::sha256_hex;
use crate::event::domain::{DomainEvent, EventKind, SubagentRefusal, SubagentTerminal};
use crate::event::hook::{
    HookInvocationId, HookLifecycle, HookOccurrenceKey, HookOccurrenceOwner, HookSubject,
};
use crate::event::id::{EventId, GateId, ItemId, SessionId, SubagentInvocationId, TurnId};
use crate::event::time::Clock;
use crate::hook::{HookCausality, HookEvent, HookOccurrence, SessionHookRuntime};
use crate::run::{
    CancelKey, IdSource, RequestDigest, RequestId, RunCommandOutcome, RunController, RunError,
    RunExecution, RunExecutionBindings, RunExecutor, RunId, RunOrigin, RunRecoveryOutcome, RunRoot,
    RunSelections, RunSettlement, RunSnapshot, RunSubscription, StartRequest, TurnLease,
};
use crate::store::blob::BlobRef;
use crate::store::session_store::SessionStore;

/// Durable admission ceiling for an explicitly backgrounded task.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackgroundTaskAuthority {
    pub parent_run: RunId,
    pub tools: BTreeSet<String>,
    pub mode: crate::config::Mode,
    pub model: Option<crate::config::ModelRef>,
    pub policy: crate::turn::GenerationPolicyDigest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildWait {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecoveryCut {
    BeforeRequest,
    AfterRequest,
    AfterChildAdmission,
    AfterSpawn,
    AfterChildSettlement,
    BeforeParentSettlement,
}

/// Exact admitted parent identity for one child request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildRequestOrigin {
    parent_run: RunId,
    parent_turn: TurnId,
    parent_item: ItemId,
    parent_event: EventId,
}

impl ChildRequestOrigin {
    pub const fn new(
        parent_run: RunId,
        parent_turn: TurnId,
        parent_item: ItemId,
        parent_event: EventId,
    ) -> Self {
        Self {
            parent_run,
            parent_turn,
            parent_item,
            parent_event,
        }
    }

    fn run_origin(self, invocation: SubagentInvocationId) -> RunOrigin {
        RunOrigin::Subagent {
            invocation,
            parent_run: self.parent_run,
            parent_turn: self.parent_turn,
            parent_item: self.parent_item,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRequest {
    invocation: SubagentInvocationId,
    origin: ChildRequestOrigin,
    task: BlobRef,
    selections: RunSelections,
    wait: ChildWait,
    requested_tools: Option<BTreeSet<String>>,
    requested_permission: Option<String>,
    requested_sandbox: Option<String>,
    requested_offline: Option<bool>,
    requested_mcp: Option<BTreeSet<String>>,
    requested_profile: Option<String>,
    parent_transcript: Option<String>,
}

impl ChildRequest {
    pub fn new(
        invocation: SubagentInvocationId,
        origin: ChildRequestOrigin,
        task: BlobRef,
        selections: RunSelections,
        wait: ChildWait,
    ) -> Self {
        Self {
            invocation,
            origin,
            task,
            selections,
            wait,
            requested_tools: None,
            requested_permission: None,
            requested_sandbox: None,
            requested_offline: None,
            requested_mcp: None,
            requested_profile: None,
            parent_transcript: None,
        }
    }
    pub const fn invocation(&self) -> SubagentInvocationId {
        self.invocation
    }
    pub const fn wait(&self) -> ChildWait {
        self.wait
    }
    pub fn selections(&self) -> &RunSelections {
        &self.selections
    }
    pub fn requested_tools(&self) -> Option<&BTreeSet<String>> {
        self.requested_tools.as_ref()
    }
    pub fn requested_permission(&self) -> Option<&str> {
        self.requested_permission.as_deref()
    }
    pub fn requested_sandbox(&self) -> Option<&str> {
        self.requested_sandbox.as_deref()
    }
    pub const fn requested_offline(&self) -> Option<bool> {
        self.requested_offline
    }
    pub fn requested_mcp(&self) -> Option<&BTreeSet<String>> {
        self.requested_mcp.as_ref()
    }
    pub fn with_requested_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.requested_tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }
    pub fn with_requested_permission(mut self, permission: impl Into<String>) -> Self {
        self.requested_permission = Some(permission.into());
        self
    }
    pub fn with_requested_sandbox(mut self, sandbox: impl Into<String>) -> Self {
        self.requested_sandbox = Some(sandbox.into());
        self
    }
    pub fn with_requested_offline(mut self, offline: bool) -> Self {
        self.requested_offline = Some(offline);
        self
    }
    pub fn with_requested_mcp<I, S>(mut self, servers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.requested_mcp = Some(servers.into_iter().map(Into::into).collect());
        self
    }
    pub fn with_requested_profile(mut self, profile: impl Into<String>) -> Self {
        self.requested_profile = Some(profile.into());
        self
    }
    pub fn with_parent_transcript(mut self, transcript: impl Into<String>) -> Self {
        self.parent_transcript = Some(transcript.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildJournalError(String);
impl ChildJournalError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
impl std::fmt::Display for ChildJournalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for ChildJournalError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildResultReaderError {
    Missing { run: RunId },
    Read { diagnostic: String },
}
impl std::fmt::Display for ChildResultReaderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { run } => write!(
                formatter,
                "completed child {run} has no final assistant blob"
            ),
            Self::Read { diagnostic } => formatter.write_str(diagnostic),
        }
    }
}
impl std::error::Error for ChildResultReaderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildResultEvidence {
    item: BlobRef,
    bytes: Vec<u8>,
}
impl ChildResultEvidence {
    pub fn new(item: BlobRef, bytes: Vec<u8>) -> Self {
        Self { item, bytes }
    }
    pub fn item(&self) -> &BlobRef {
        &self.item
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Everything a child run needs from the process that owns its parent.
///
/// One port, not six. This began as `ChildJournal`, `ChildTaskReader`,
/// `ChildResultReader`, `ChildResultStore`, `ChildProfileResolver` and
/// `ChildSelectionAdmission` — six traits of one method each, every one with a
/// single production implementation, all six living in the same CLI module and
/// wired through a seven-argument constructor that needed
/// `#[allow(clippy::too_many_arguments)]` to compile.
///
/// `AGENTS.md`'s hexagonal rule admits a port at a *proven external boundary*.
/// None of the six was one: they did not vary independently, no second adapter
/// ever implemented one without the others, and every fake implemented all of
/// them together. Six names bought no substitutability and cost four extra
/// `Arc<dyn>` indirections plus an argument list long enough to trip a lint.
/// The boundary that is real is this one — the host process holding the
/// parent's journal, blobs, artifacts and configuration — and it is named once.
///
/// The rule of thumb this enacts, recorded in
/// `ARCHITECTURE.md`: one
/// real implementation and one fake is not a boundary, it is a function.
pub trait ChildHost: Send + Sync {
    /// The parent's durable record of one subagent invocation.
    fn read_journal(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<Vec<DomainEvent>, ChildJournalError>;

    /// The rendered task a child was handed, read back from parent storage.
    fn read_rendered_task(&self, reference: &BlobRef) -> Result<Vec<u8>, ChildResultReaderError>;

    /// What a completed child produced.
    fn read_final_assistant(
        &self,
        run: RunId,
    ) -> Result<ChildResultEvidence, ChildResultReaderError>;

    /// Copy a child's result into the parent's own storage, returning the
    /// parent-side reference.
    fn copy_to_parent(
        &self,
        child: RunId,
        evidence: &ChildResultEvidence,
    ) -> Result<BlobRef, ChildResultReaderError>;

    /// Whether a named profile exists and does not widen the child's authority.
    fn resolve_profile(&self, name: &str) -> Result<(), String>;

    /// The selections and bindings a child request is admitted under.
    fn admit_selections(
        &self,
        request: &ChildRequest,
        bindings: &RunExecutionBindings,
    ) -> Result<(RunSelections, RunExecutionBindings), String>;
}

pub struct ChildServicePorts {
    parent_run: RunId,
    host: Arc<dyn ChildHost>,
}
impl ChildServicePorts {
    pub fn new(parent_run: RunId, host: Arc<dyn ChildHost>) -> Self {
        Self { parent_run, host }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildServiceError {
    ProfileUnavailable { profile: String },
    AuthorityWouldWiden { dimension: String },
    BudgetDenied { diagnostic: String },
    Journal { diagnostic: String },
    Result { diagnostic: String },
    Controller { diagnostic: String },
    UnknownInvocation { invocation: SubagentInvocationId },
    InvalidDurableState { diagnostic: String },
}
impl std::fmt::Display for ChildServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProfileUnavailable { profile } => {
                write!(formatter, "child profile `{profile}` is unavailable")
            }
            Self::AuthorityWouldWiden { dimension } => {
                write!(formatter, "child authority would widen {dimension}")
            }
            Self::BudgetDenied { diagnostic }
            | Self::Journal { diagnostic }
            | Self::Result { diagnostic }
            | Self::Controller { diagnostic }
            | Self::InvalidDurableState { diagnostic } => formatter.write_str(diagnostic),
            Self::UnknownInvocation { invocation } => {
                write!(formatter, "child invocation {invocation} is unknown")
            }
        }
    }
}
impl std::error::Error for ChildServiceError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildWaitResult {
    Returned {
        final_message: BlobRef,
    },
    Refused {
        failure: SubagentRefusal,
    },
    Failed {
        settlement: RunSettlement,
    },
    ReturnBlocked {
        final_message: BlobRef,
        hook_invocation: HookInvocationId,
    },
}

pub struct ChildAdmission {
    inner: Arc<Inner>,
    invocation: SubagentInvocationId,
    parent_run: RunId,
    child_run: RunId,
    child_session: SessionId,
    origin: RunOrigin,
    turn_lease: TurnLease,
    selections: RunSelections,
    wait: ChildWait,
    subscription: RunSubscription,
}
impl std::fmt::Debug for ChildAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChildAdmission")
            .field("invocation", &self.invocation)
            .field("child_run", &self.child_run)
            .finish_non_exhaustive()
    }
}
impl ChildAdmission {
    pub const fn invocation(&self) -> SubagentInvocationId {
        self.invocation
    }
    pub const fn parent_run(&self) -> RunId {
        self.parent_run
    }
    pub const fn child_run(&self) -> RunId {
        self.child_run
    }
    pub const fn child_session(&self) -> SessionId {
        self.child_session
    }
    pub const fn session_id(&self) -> SessionId {
        self.child_session
    }
    pub fn origin(&self) -> &RunOrigin {
        &self.origin
    }
    pub const fn turn_lease(&self) -> TurnLease {
        self.turn_lease
    }
    pub fn selections(&self) -> &RunSelections {
        &self.selections
    }
    pub fn budget_gate(&self) -> GateId {
        self.inner.gate.id()
    }
    pub fn subscription(&self) -> RunSubscription {
        self.subscription.clone()
    }
    pub async fn cancel(&self) -> Result<(), ChildServiceError> {
        if self.wait != ChildWait::Foreground {
            return Err(ChildServiceError::AuthorityWouldWiden {
                dimension: "background cancellation ownership".into(),
            });
        }
        let key = self
            .inner
            .active
            .lock()
            .expect("active children")
            .get(&self.invocation)
            .and_then(|active| active.cancel_key.clone())
            .ok_or(ChildServiceError::UnknownInvocation {
                invocation: self.invocation,
            })?;
        self.inner
            .controller
            .cancel_admitted(&key)
            .await
            .map_err(controller_error)?;
        Ok(())
    }
    pub async fn wait(
        &self,
        parent: &SessionHookRuntime<'_>,
    ) -> Result<ChildWaitResult, ChildServiceError> {
        finish(&self.inner, self.invocation, parent).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredChild {
    invocation: SubagentInvocationId,
    child: Option<RunId>,
    origin: RunOrigin,
}
impl RecoveredChild {
    pub const fn invocation(&self) -> SubagentInvocationId {
        self.invocation
    }
    pub const fn child_run(&self) -> Option<RunId> {
        self.child
    }
    pub fn origin(&self) -> &RunOrigin {
        &self.origin
    }
    pub const fn admission_count(&self) -> usize {
        if self.child.is_some() { 1 } else { 0 }
    }
    pub const fn delivery_was_retried(&self) -> bool {
        false
    }
}

pub struct ChildRunService {
    inner: Arc<Inner>,
}
impl Clone for ChildRunService {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}
impl ChildRunService {
    pub fn new<S, E, C, I>(
        controller: Arc<RunController<S, E, C, I>>,
        gate: SharedBudgetGate,
        root: RunRoot,
        bindings: RunExecutionBindings,
        ports: ChildServicePorts,
    ) -> Self
    where
        S: SessionStore + 'static,
        E: RunExecutor + 'static,
        C: Clock + 'static,
        I: IdSource + 'static,
    {
        Self {
            inner: Arc::new(Inner {
                controller: Arc::new(ControllerAdapter(controller)),
                gate,
                root,
                bindings,
                ports,
                active: Mutex::new(BTreeMap::new()),
            }),
        }
    }
    /// Reopen a task under its original run lineage after the CLI process restarts.
    pub fn for_background_task(&self, parent_run: RunId) -> Self {
        Self {
            inner: Arc::new(Inner {
                controller: Arc::clone(&self.inner.controller),
                gate: self.inner.gate.clone(),
                root: self.inner.root.clone(),
                bindings: self
                    .inner
                    .bindings
                    .clone()
                    .without_delegation()
                    .with_cancel(crate::turn::CancelToken::new()),
                ports: ChildServicePorts::new(parent_run, Arc::clone(&self.inner.ports.host)),
                active: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// The already admitted parent identity required by typed child requests.
    pub fn parent_run(&self) -> RunId {
        self.inner.ports.parent_run
    }
    pub async fn start(
        &self,
        request: ChildRequest,
        parent: &SessionHookRuntime<'_>,
    ) -> Result<ChildAdmission, ChildServiceError> {
        start(&self.inner, request, parent).await
    }
    pub async fn collect_background(
        &self,
        invocation: SubagentInvocationId,
        parent: &SessionHookRuntime<'_>,
    ) -> Result<ChildWaitResult, ChildServiceError> {
        finish(&self.inner, invocation, parent).await
    }
    /// Transfer the admitted execution to the product runtime exactly once.
    ///
    /// Admission deliberately does not execute a child: the host that owns
    /// the async product invocation must launch this value immediately.
    pub fn take_execution(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<Option<RunExecution>, ChildServiceError> {
        self.inner
            .active
            .lock()
            .expect("active children")
            .get_mut(&invocation)
            .map(|child| child.execution.take())
            .ok_or(ChildServiceError::UnknownInvocation { invocation })
    }
    /// Execute a value transferred by [`Self::take_execution`].
    pub async fn execute(&self, execution: RunExecution) -> Result<RunSnapshot, ChildServiceError> {
        self.inner
            .controller
            .execute(execution)
            .await
            .map_err(controller_error)
    }
    /// A nonblocking observation; collection remains serialized by `finish`.
    pub fn is_settled(&self, invocation: SubagentInvocationId) -> Result<bool, ChildServiceError> {
        let active = self.inner.active.lock().expect("active children");
        let child = active
            .get(&invocation)
            .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
        Ok(child
            .settlement
            .try_lock()
            .is_some_and(|state| state.settled.is_some())
            || child.observed.is_some()
            || child
                .subscription
                .as_ref()
                .is_some_and(|subscription| subscription.snapshot().settlement().is_some()))
    }

    /// Cancel a child on behalf of the retained product owner.
    pub async fn cancel_owned(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<(), ChildServiceError> {
        let key = self
            .inner
            .active
            .lock()
            .expect("active children")
            .get(&invocation)
            .ok_or(ChildServiceError::UnknownInvocation { invocation })?
            .cancel_key
            .clone();
        if let Some(key) = key {
            self.inner
                .controller
                .cancel_admitted(&key)
                .await
                .map_err(controller_error)?;
        }
        Ok(())
    }
    pub async fn recover_from_durable(
        &self,
        invocation: SubagentInvocationId,
        _cut: RecoveryCut,
        parent: &SessionHookRuntime<'_>,
        wait: ChildWait,
        requested_tools: BTreeSet<String>,
    ) -> Result<Option<RecoveredChild>, ChildServiceError> {
        let events = self
            .inner
            .ports
            .host
            .read_journal(invocation)
            .map_err(journal_error)?;
        let mut requested_events = events.iter().filter(|event| {
            matches!(&event.kind, EventKind::SubagentRequested { invocation: id, .. } if *id == invocation)
        });
        let Some(requested) = requested_events.next() else {
            return Ok(None);
        };
        if requested_events.next().is_some() {
            return Err(ChildServiceError::InvalidDurableState {
                diagnostic: format!("child {invocation} has more than one durable request"),
            });
        }
        let (task, selections) = match &requested.kind {
            EventKind::SubagentRequested {
                task, selections, ..
            } => (task.clone(), selections.clone()),
            _ => unreachable!("matched requested event"),
        };
        let parent_item =
            requested
                .env
                .item
                .ok_or_else(|| ChildServiceError::InvalidDurableState {
                    diagnostic: "durable child request has no parent item".into(),
                })?;
        let parent_event =
            requested
                .env
                .parent
                .ok_or_else(|| ChildServiceError::InvalidDurableState {
                    diagnostic: "durable child request has no admitted parent event".into(),
                })?;
        let mut recovery_request = ChildRequest::new(
            invocation,
            ChildRequestOrigin::new(
                self.inner.ports.parent_run,
                requested.env.turn,
                parent_item,
                parent_event,
            ),
            task.clone(),
            selections,
            wait,
        )
        .with_requested_tools(requested_tools);
        if let Some(profile) = recovery_request.selections.profile().map(str::to_owned) {
            recovery_request = recovery_request.with_requested_profile(profile);
        }
        let (selections, bindings) = admit_child_authority(&self.inner, &mut recovery_request)?;
        let origin = recovery_request.origin.run_origin(invocation);
        validate_gate(&self.inner, &bindings)?;
        let spawned = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::SubagentSpawned {
                    invocation: found,
                    child,
                } if *found == invocation => Some((event, *child)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if spawned.len() > 1 {
            return Err(ChildServiceError::InvalidDurableState {
                diagnostic: format!("child {invocation} has more than one durable spawn"),
            });
        }
        if let Some((spawned_event, _)) = spawned.first()
            && (spawned_event.env.turn != requested.env.turn
                || spawned_event.env.item != requested.env.item
                || spawned_event.env.parent != Some(requested.env.id))
        {
            return Err(ChildServiceError::InvalidDurableState {
                diagnostic: format!("child {invocation} has an invalid durable spawn origin"),
            });
        }
        let settled = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::SubagentSettled {
                    invocation: found,
                    child,
                    terminal,
                } if *found == invocation => Some((event, *child, terminal.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        if settled.len() > 1 {
            return Err(ChildServiceError::InvalidDurableState {
                diagnostic: format!("child {invocation} has more than one durable settlement"),
            });
        }
        if let Some((settled_event, child, terminal)) = settled.first() {
            let child = (*child).ok_or_else(|| ChildServiceError::InvalidDurableState {
                diagnostic: "settled admitted child has no run identity".into(),
            })?;
            if spawned.first().map(|(_, run)| *run) != Some(child)
                || settled_event.env.turn != requested.env.turn
                || settled_event.env.item != requested.env.item
            {
                return Err(ChildServiceError::InvalidDurableState {
                    diagnostic: format!(
                        "child {invocation} has an invalid durable settlement origin"
                    ),
                });
            }
            self.inner.active.lock().expect("active children").insert(
                invocation,
                ActiveChild {
                    run: child,
                    parent_turn: requested.env.turn,
                    parent_item,
                    parent_event: settled_event.env.id,
                    execution: None,
                    cancel_key: None,
                    subscription: None,
                    observed: None,
                    settlement: Arc::new(AsyncMutex::new(SettlementState {
                        materialized: None,
                        pending: None,
                        settled: Some(result_of_terminal(terminal)),
                    })),
                },
            );
            return Ok(Some(RecoveredChild {
                invocation,
                child: Some(child),
                origin,
            }));
        }
        if let Some((spawned_event, child)) = spawned.first() {
            recover_active(
                &self.inner,
                invocation,
                *child,
                ChildRequestOrigin::new(
                    self.inner.ports.parent_run,
                    requested.env.turn,
                    parent_item,
                    spawned_event.env.id,
                ),
                bindings.clone(),
            )
            .await?;
            return Ok(Some(RecoveredChild {
                invocation,
                child: Some(*child),
                origin,
            }));
        }
        match self
            .inner
            .controller
            .status_by_request(request_id(invocation))
            .await
        {
            Ok(snapshot) => {
                validate_expected_origin(&snapshot, &origin)?;
                recover_active(
                    &self.inner,
                    invocation,
                    snapshot.run_id(),
                    recovery_request.origin,
                    bindings.clone(),
                )
                .await?;
                let parent_event = append_lifecycle(
                    parent,
                    requested.env.turn,
                    Some(parent_item),
                    Some(requested.env.id),
                    EventKind::SubagentSpawned {
                        invocation,
                        child: snapshot.run_id(),
                    },
                )?;
                if let Some(child) = self
                    .inner
                    .active
                    .lock()
                    .expect("active children")
                    .get_mut(&invocation)
                {
                    child.parent_event = parent_event;
                }
                Ok(Some(RecoveredChild {
                    invocation,
                    child: Some(snapshot.run_id()),
                    origin,
                }))
            }
            Err(RunError::Store(crate::store::session_store::StoreError::NotFound)) => {
                let prompt = read_task(&self.inner, &task)?;
                let (child, execution, cancel_key, subscription, _lease) = admit(
                    &self.inner,
                    invocation,
                    origin.clone(),
                    selections,
                    prompt,
                    bindings,
                )
                .await?;
                let parent_event = append_lifecycle(
                    parent,
                    requested.env.turn,
                    Some(parent_item),
                    Some(requested.env.id),
                    EventKind::SubagentSpawned { invocation, child },
                )?;
                self.inner.active.lock().expect("active children").insert(
                    invocation,
                    ActiveChild {
                        run: child,
                        parent_turn: requested.env.turn,
                        parent_item,
                        parent_event,
                        execution: Some(execution),
                        cancel_key: Some(cancel_key),
                        subscription: Some(subscription),
                        observed: None,
                        settlement: Arc::new(AsyncMutex::new(SettlementState::default())),
                    },
                );
                Ok(Some(RecoveredChild {
                    invocation,
                    child: Some(child),
                    origin,
                }))
            }
            Err(error) => Err(controller_error(error)),
        }
    }
}

struct Inner {
    controller: Arc<dyn ControllerPort>,
    gate: SharedBudgetGate,
    root: RunRoot,
    bindings: RunExecutionBindings,
    ports: ChildServicePorts,
    active: Mutex<BTreeMap<SubagentInvocationId, ActiveChild>>,
}
struct ActiveChild {
    run: RunId,
    parent_turn: TurnId,
    parent_item: ItemId,
    parent_event: EventId,
    execution: Option<RunExecution>,
    cancel_key: Option<CancelKey>,
    subscription: Option<RunSubscription>,
    observed: Option<RunSettlement>,
    settlement: Arc<AsyncMutex<SettlementState>>,
}

#[derive(Default)]
struct SettlementState {
    materialized: Option<ChildWaitResult>,
    pending: Option<ChildWaitResult>,
    settled: Option<ChildWaitResult>,
}

async fn recover_active(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
    child: RunId,
    parent: ChildRequestOrigin,
    bindings: RunExecutionBindings,
) -> Result<(), ChildServiceError> {
    let expected_origin = parent.run_origin(invocation);
    let outcome = inner
        .controller
        .recover(child, bindings)
        .await
        .map_err(controller_error)?;
    let active = match outcome {
        RunRecoveryOutcome::Reattached(reattachment) => {
            validate_expected_origin(reattachment.snapshot(), &expected_origin)?;
            let subscription = reattachment.subscription();
            let cancel_key = reattachment.cancel_key().clone();
            let execution = reattachment.into_execution();
            ActiveChild {
                run: child,
                parent_turn: parent.parent_turn,
                parent_item: parent.parent_item,
                parent_event: parent.parent_event,
                execution: Some(execution),
                cancel_key: Some(cancel_key),
                subscription: Some(subscription),
                observed: None,
                settlement: Arc::new(AsyncMutex::new(SettlementState::default())),
            }
        }
        RunRecoveryOutcome::Observed(observation) => {
            validate_expected_origin(observation.snapshot(), &expected_origin)?;
            ActiveChild {
                run: child,
                parent_turn: parent.parent_turn,
                parent_item: parent.parent_item,
                parent_event: parent.parent_event,
                execution: None,
                cancel_key: None,
                subscription: Some(observation.subscription()),
                observed: observation.snapshot().settlement().cloned(),
                settlement: Arc::new(AsyncMutex::new(SettlementState::default())),
            }
        }
    };
    inner
        .active
        .lock()
        .expect("active children")
        .insert(invocation, active);
    Ok(())
}

async fn start(
    inner: &Arc<Inner>,
    mut request: ChildRequest,
    parent: &SessionHookRuntime<'_>,
) -> Result<ChildAdmission, ChildServiceError> {
    if request.origin.parent_run != inner.ports.parent_run {
        return Err(ChildServiceError::InvalidDurableState {
            diagnostic: "child request belongs to a different parent run".into(),
        });
    }
    if request.parent_transcript.is_some() {
        return Err(ChildServiceError::AuthorityWouldWiden {
            dimension: "parent transcript".into(),
        });
    }
    validate_parent_origin(parent, &request)?;
    let (selections, bindings) = admit_child_authority(inner, &mut request)?;
    let prompt = read_task(inner, &request.task)?;
    let existing = inner
        .ports
        .host
        .read_journal(request.invocation)
        .map_err(journal_error)?;
    let requested_event = if let Some(event) = existing.iter().find(|event| {
        matches!(&event.kind, EventKind::SubagentRequested { invocation, .. } if *invocation == request.invocation)
    }) {
        event.env.id
    } else {
        append_lifecycle(
            parent,
            request.origin.parent_turn,
            Some(request.origin.parent_item),
            Some(request.origin.parent_event),
            EventKind::SubagentRequested {
                invocation: request.invocation,
                task: request.task.clone(),
                selections: selections.clone(),
            },
        )?
    };
    let origin = request.origin.run_origin(request.invocation);
    let (child_run, execution, cancel_key, subscription, turn_lease) = admit(
        inner,
        request.invocation,
        origin.clone(),
        selections.clone(),
        prompt,
        bindings,
    )
    .await?;
    let spawned = append_lifecycle(
        parent,
        request.origin.parent_turn,
        Some(request.origin.parent_item),
        Some(requested_event),
        EventKind::SubagentSpawned {
            invocation: request.invocation,
            child: child_run,
        },
    )?;
    let child_session = child_run.session_id();
    inner.active.lock().expect("active children").insert(
        request.invocation,
        ActiveChild {
            run: child_run,
            parent_turn: request.origin.parent_turn,
            parent_item: request.origin.parent_item,
            parent_event: spawned,
            execution: Some(execution),
            cancel_key: Some(cancel_key),
            subscription: Some(subscription.clone()),
            observed: None,
            settlement: Arc::new(AsyncMutex::new(SettlementState::default())),
        },
    );
    let started = dispatch_subagent(
        inner,
        request.invocation,
        HookLifecycle::SubagentStart,
        spawned,
        parent,
    )
    .await?;
    if let Some(event) = started.closure_event()
        && let Some(child) = inner
            .active
            .lock()
            .expect("active children")
            .get_mut(&request.invocation)
    {
        child.parent_event = event;
    }
    Ok(ChildAdmission {
        inner: Arc::clone(inner),
        invocation: request.invocation,
        parent_run: request.origin.parent_run,
        child_run,
        child_session,
        origin,
        turn_lease,
        selections,
        wait: request.wait,
        subscription,
    })
}

fn admit_child_authority(
    inner: &Inner,
    request: &mut ChildRequest,
) -> Result<(RunSelections, RunExecutionBindings), ChildServiceError> {
    if let Some(profile) = request.requested_profile.as_deref() {
        if profile.is_empty() || inner.ports.host.resolve_profile(profile).is_err() {
            return Err(ChildServiceError::ProfileUnavailable {
                profile: profile.to_owned(),
            });
        }
        request.selections = request.selections.with_profile_name(profile.to_owned());
    }
    let admitted = inner
        .ports
        .host
        .admit_selections(request, &inner.bindings)
        .map_err(|diagnostic| ChildServiceError::AuthorityWouldWiden {
            dimension: authority_dimension(&diagnostic),
        })?;
    validate_gate(inner, &admitted.1)?;
    Ok(admitted)
}

async fn admit(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
    origin: RunOrigin,
    selections: RunSelections,
    prompt: String,
    bindings: RunExecutionBindings,
) -> Result<(RunId, RunExecution, CancelKey, RunSubscription, TurnLease), ChildServiceError> {
    let outcome = inner
        .controller
        .start(
            StartRequest::new(
                RequestId::session_id(request_id(invocation)),
                inner.root.clone(),
                request_id(invocation),
                RequestDigest::parse(&format!(
                    "child:{invocation}:{}",
                    sha256_hex(prompt.as_bytes())
                ))
                .map_err(controller_error)?,
                selections,
                prompt,
            )
            .with_origin(origin),
            bindings,
        )
        .await
        .map_err(map_run_error)?;
    let admission = match outcome {
        RunCommandOutcome::Admitted(admission) => admission,
        RunCommandOutcome::Replayed(observation) => {
            return Err(ChildServiceError::InvalidDurableState {
                diagnostic: format!(
                    "child {invocation} already admitted as {}",
                    observation.snapshot().run_id()
                ),
            });
        }
    };
    let child = admission.run_id();
    let lease = admission.turn_lease();
    let subscription = admission.subscription();
    let cancel_key = admission.cancel_key().clone();
    let execution = admission.into_execution();
    Ok((child, execution, cancel_key, subscription, lease))
}

async fn finish(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
    parent: &SessionHookRuntime<'_>,
) -> Result<ChildWaitResult, ChildServiceError> {
    let settlement = inner
        .active
        .lock()
        .expect("active children")
        .get(&invocation)
        .map(|child| Arc::clone(&child.settlement))
        .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
    // This invocation-local lock is intentionally held across materializing
    // the result, dispatching SubagentStop, appending the durable settlement,
    // and caching it. Other invocations have independent locks.
    let mut flight = settlement.lock().await;
    if let Some(result) = &flight.settled {
        return Ok(result.clone());
    }
    if let Some(pending) = flight.pending.clone() {
        return persist_pending(inner, invocation, pending, parent, &mut flight);
    }

    let materialized = match flight.materialized.clone() {
        Some(materialized) => materialized,
        None => {
            let materialized = prepare_result(inner, invocation).await?;
            flight.materialized = Some(materialized.clone());
            materialized
        }
    };
    let prepared = match materialized {
        ChildWaitResult::Returned { final_message } => {
            let parent_event = active_parent_event(inner, invocation)?;
            let outcome = dispatch_subagent(
                inner,
                invocation,
                HookLifecycle::SubagentStop,
                parent_event,
                parent,
            )
            .await?;
            if let Some(event) = outcome.closure_event()
                && let Some(child) = inner
                    .active
                    .lock()
                    .expect("active children")
                    .get_mut(&invocation)
            {
                child.parent_event = event;
            }
            if outcome.is_blocked() {
                let hook_invocation = outcome.blocking_invocation().ok_or_else(|| {
                    ChildServiceError::InvalidDurableState {
                        diagnostic:
                            "blocked SubagentStop has no durable hook invocation settlement".into(),
                    }
                })?;
                ChildWaitResult::ReturnBlocked {
                    final_message,
                    hook_invocation,
                }
            } else {
                ChildWaitResult::Returned { final_message }
            }
        }
        other => other,
    };
    flight.materialized = None;
    flight.pending = Some(prepared.clone());
    persist_pending(inner, invocation, prepared, parent, &mut flight)
}

async fn dispatch_subagent(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
    lifecycle: HookLifecycle,
    parent_event: EventId,
    parent: &SessionHookRuntime<'_>,
) -> Result<crate::hook::HookRunResult, ChildServiceError> {
    let item = inner
        .active
        .lock()
        .expect("active children")
        .get(&invocation)
        .map(|child| child.parent_item)
        .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
    let subject = HookSubject::Item { item };
    let turn = inner
        .active
        .lock()
        .expect("active children")
        .get(&invocation)
        .map(|child| child.parent_turn)
        .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
    let causality = HookCausality::subagent(invocation, turn, item, parent_event);
    let occurrence = HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Subagent { invocation }),
        HookEvent::turn(lifecycle, subject),
        causality,
    )
    .map_err(controller_error)?;
    parent
        .dispatch(crate::event::id::Producer::Main, occurrence)
        .await
        .map_err(controller_error)
}

async fn prepare_result(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
) -> Result<ChildWaitResult, ChildServiceError> {
    enum ResultSource {
        Observed(RunSettlement),
        Wait(RunSubscription),
    }
    let (run, source) = {
        let mut active = inner.active.lock().expect("active children");
        let child = active
            .get_mut(&invocation)
            .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
        if child.execution.is_some() {
            return Err(ChildServiceError::InvalidDurableState {
                diagnostic: "admitted child execution was not launched by the product owner".into(),
            });
        }
        let source = if let Some(settlement) = child.observed.clone() {
            ResultSource::Observed(settlement)
        } else {
            ResultSource::Wait(child.subscription.clone().ok_or_else(|| {
                ChildServiceError::InvalidDurableState {
                    diagnostic: "active child has neither an observation nor a subscription".into(),
                }
            })?)
        };
        (child.run, source)
    };
    let settlement = match source {
        ResultSource::Observed(settlement) => settlement,
        ResultSource::Wait(subscription) => {
            subscription
                .await
                .settlement()
                .cloned()
                .unwrap_or(RunSettlement::Uncertain {
                    diagnostic: Some(
                        "the child run ended without recording how it settled".to_owned(),
                    ),
                })
        }
    };
    match settlement {
        RunSettlement::Completed => {
            let evidence = inner
                .ports
                .host
                .read_final_assistant(run)
                .map_err(result_error)?;
            validate_blob(evidence.item(), evidence.bytes())?;
            let final_message = inner
                .ports
                .host
                .copy_to_parent(run, &evidence)
                .map_err(result_error)?;
            if final_message != *evidence.item() {
                return Err(ChildServiceError::Result {
                    diagnostic: "parent result copy changed the content-addressed reference".into(),
                });
            }
            Ok(ChildWaitResult::Returned { final_message })
        }
        settlement => Ok(ChildWaitResult::Failed { settlement }),
    }
}

fn persist_pending(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
    result: ChildWaitResult,
    parent: &SessionHookRuntime<'_>,
    flight: &mut SettlementState,
) -> Result<ChildWaitResult, ChildServiceError> {
    let (run, turn, item) = {
        let active = inner.active.lock().expect("active children");
        let child = active
            .get(&invocation)
            .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
        (child.run, child.parent_turn, child.parent_item)
    };
    if let Some((event, durable)) = durable_settlement(inner, invocation, run)? {
        if durable != result {
            return Err(ChildServiceError::InvalidDurableState {
                diagnostic: format!(
                    "durable settlement for child {invocation} conflicts with the prepared outcome"
                ),
            });
        }
        parent
            .hook_state()
            .journal()
            .reconcile_durable(&event)
            .map_err(|error| ChildServiceError::Journal {
                diagnostic: error.to_string(),
            })?;
        flight.pending = None;
        flight.settled = Some(durable.clone());
        return Ok(durable);
    }
    parent
        .hook_state()
        .journal()
        .append_or_retry_settlement(
            turn,
            Some(item),
            Some(active_parent_event(inner, invocation)?),
            EventKind::SubagentSettled {
                invocation,
                child: Some(run),
                terminal: terminal_of(&result),
            },
        )
        .map_err(|error| ChildServiceError::Journal {
            diagnostic: error.to_string(),
        })?;
    flight.pending = None;
    flight.settled = Some(result.clone());
    Ok(result)
}

fn durable_settlement(
    inner: &Inner,
    invocation: SubagentInvocationId,
    run: RunId,
) -> Result<Option<(DomainEvent, ChildWaitResult)>, ChildServiceError> {
    let events = inner
        .ports
        .host
        .read_journal(invocation)
        .map_err(journal_error)?;
    let mut settlements = events.into_iter().filter_map(|event| match &event.kind {
        EventKind::SubagentSettled {
            invocation: found,
            child,
            terminal,
        } if *found == invocation => Some((event.clone(), *child, terminal.clone())),
        _ => None,
    });
    let Some((event, child, terminal)) = settlements.next() else {
        return Ok(None);
    };
    if settlements.next().is_some() {
        return Err(ChildServiceError::InvalidDurableState {
            diagnostic: format!("child {invocation} has more than one durable settlement"),
        });
    }
    if child != Some(run) {
        return Err(ChildServiceError::InvalidDurableState {
            diagnostic: format!("child {invocation} settled a different run identity"),
        });
    }
    Ok(Some((event, result_of_terminal(&terminal))))
}

fn terminal_of(result: &ChildWaitResult) -> SubagentTerminal {
    match result {
        ChildWaitResult::Returned { final_message } => SubagentTerminal::Returned {
            final_message: final_message.clone(),
        },
        ChildWaitResult::Refused { failure } => SubagentTerminal::Refused {
            failure: failure.clone(),
        },
        ChildWaitResult::Failed { settlement } => SubagentTerminal::Failed {
            settlement: settlement.clone(),
        },
        ChildWaitResult::ReturnBlocked {
            final_message,
            hook_invocation,
        } => SubagentTerminal::ReturnBlocked {
            final_message: final_message.clone(),
            hook_invocation: *hook_invocation,
        },
    }
}

fn result_of_terminal(terminal: &SubagentTerminal) -> ChildWaitResult {
    match terminal {
        SubagentTerminal::Returned { final_message } => ChildWaitResult::Returned {
            final_message: final_message.clone(),
        },
        SubagentTerminal::Refused { failure } => ChildWaitResult::Refused {
            failure: failure.clone(),
        },
        SubagentTerminal::Failed { settlement } => ChildWaitResult::Failed {
            settlement: settlement.clone(),
        },
        SubagentTerminal::ReturnBlocked {
            final_message,
            hook_invocation,
        } => ChildWaitResult::ReturnBlocked {
            final_message: final_message.clone(),
            hook_invocation: *hook_invocation,
        },
    }
}

fn read_task(inner: &Inner, reference: &BlobRef) -> Result<String, ChildServiceError> {
    let bytes = inner
        .ports
        .host
        .read_rendered_task(reference)
        .map_err(result_error)?;
    validate_blob(reference, &bytes)?;
    String::from_utf8(bytes).map_err(|error| ChildServiceError::Result {
        diagnostic: format!("rendered child task is not UTF-8: {error}"),
    })
}

fn validate_blob(reference: &BlobRef, bytes: &[u8]) -> Result<(), ChildServiceError> {
    if bytes.len() as u64 != reference.bytes || sha256_hex(bytes) != reference.id.as_str() {
        return Err(ChildServiceError::Result {
            diagnostic: format!("blob {reference} does not match its exact bytes"),
        });
    }
    Ok(())
}

fn validate_gate(inner: &Inner, bindings: &RunExecutionBindings) -> Result<(), ChildServiceError> {
    let bound = bindings
        .budget_gate()
        .ok_or_else(|| ChildServiceError::BudgetDenied {
            diagnostic: "child execution has no shared budget gate".into(),
        })?;
    if !bound.same_owner(&inner.gate) {
        return Err(ChildServiceError::BudgetDenied {
            diagnostic: "child bindings carry a different budget gate".into(),
        });
    }
    Ok(())
}

fn append_lifecycle(
    parent: &SessionHookRuntime<'_>,
    turn: TurnId,
    item: Option<ItemId>,
    parent_event: Option<EventId>,
    kind: EventKind,
) -> Result<EventId, ChildServiceError> {
    parent
        .hook_state()
        .journal()
        .append(turn, item, parent_event, kind)
        .map_err(|error| ChildServiceError::Journal {
            diagnostic: error.to_string(),
        })
}

fn validate_parent_origin(
    parent: &SessionHookRuntime<'_>,
    request: &ChildRequest,
) -> Result<(), ChildServiceError> {
    let valid = parent
        .hook_state()
        .inspect(|state| {
            state
                .items
                .get(&request.origin.parent_item)
                .filter(|item| item.turn == request.origin.parent_turn)
                .and_then(|item| item.tool_call_started)
                == Some(request.origin.parent_event)
                || state.skill_events().iter().any(|event| {
                    matches!(
                        event,
                        crate::state::SkillEvent::Instruction(admitted)
                            if admitted.event == request.origin.parent_event
                                && admitted.turn == request.origin.parent_turn
                                && admitted.item == Some(request.origin.parent_item)
                    )
                })
        })
        .map_err(|error| ChildServiceError::Journal {
            diagnostic: error.to_string(),
        })?;
    if valid {
        Ok(())
    } else {
        Err(ChildServiceError::InvalidDurableState {
            diagnostic: "child request does not name an admitted parent event".into(),
        })
    }
}

fn active_parent_event(
    inner: &Inner,
    invocation: SubagentInvocationId,
) -> Result<EventId, ChildServiceError> {
    inner
        .active
        .lock()
        .expect("active children")
        .get(&invocation)
        .map(|child| child.parent_event)
        .ok_or(ChildServiceError::UnknownInvocation { invocation })
}

fn authority_dimension(diagnostic: &str) -> String {
    for dimension in ["tools", "permissions", "sandbox", "offline", "MCP"] {
        if diagnostic
            .to_ascii_lowercase()
            .contains(&dimension.to_ascii_lowercase())
        {
            return dimension.to_owned();
        }
    }
    diagnostic.to_owned()
}
fn request_id(invocation: SubagentInvocationId) -> RequestId {
    RequestId::from_uuid(invocation.uuid())
}
fn validate_expected_origin(
    snapshot: &RunSnapshot,
    expected: &RunOrigin,
) -> Result<(), ChildServiceError> {
    if snapshot.origin() == expected {
        Ok(())
    } else {
        Err(ChildServiceError::InvalidDurableState {
            diagnostic: "child session has a different origin".into(),
        })
    }
}
fn journal_error(error: ChildJournalError) -> ChildServiceError {
    ChildServiceError::Journal {
        diagnostic: error.to_string(),
    }
}
fn result_error(error: ChildResultReaderError) -> ChildServiceError {
    ChildServiceError::Result {
        diagnostic: error.to_string(),
    }
}
fn controller_error(error: impl std::fmt::Display) -> ChildServiceError {
    ChildServiceError::Controller {
        diagnostic: error.to_string(),
    }
}
fn map_run_error(error: RunError) -> ChildServiceError {
    match error {
        RunError::Executor(ref source) if source.to_string().contains("budget") => {
            ChildServiceError::BudgetDenied {
                diagnostic: source.to_string(),
            }
        }
        other => controller_error(other),
    }
}

trait ControllerPort: Send + Sync {
    fn start<'a>(
        &'a self,
        request: StartRequest,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunCommandOutcome, RunError>>;
    fn execute<'a>(
        &'a self,
        execution: RunExecution,
    ) -> BoxFuture<'a, Result<RunSnapshot, RunError>>;
    fn recover<'a>(
        &'a self,
        child: RunId,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunRecoveryOutcome, RunError>>;
    fn cancel_admitted<'a>(
        &'a self,
        key: &'a CancelKey,
    ) -> BoxFuture<'a, Result<RunSnapshot, RunError>>;
    fn status_by_request<'a>(
        &'a self,
        request: RequestId,
    ) -> BoxFuture<'a, Result<RunSnapshot, RunError>>;
}
struct ControllerAdapter<S, E, C, I>(Arc<RunController<S, E, C, I>>);
impl<S, E, C, I> ControllerPort for ControllerAdapter<S, E, C, I>
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    fn start<'a>(
        &'a self,
        request: StartRequest,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunCommandOutcome, RunError>> {
        Box::pin(self.0.start(request, bindings))
    }
    fn execute<'a>(
        &'a self,
        execution: RunExecution,
    ) -> BoxFuture<'a, Result<RunSnapshot, RunError>> {
        Box::pin(self.0.execute(execution))
    }
    fn recover<'a>(
        &'a self,
        child: RunId,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunRecoveryOutcome, RunError>> {
        Box::pin(self.0.recover(child, bindings))
    }
    fn cancel_admitted<'a>(
        &'a self,
        key: &'a CancelKey,
    ) -> BoxFuture<'a, Result<RunSnapshot, RunError>> {
        Box::pin(self.0.cancel_admitted(key))
    }
    fn status_by_request<'a>(
        &'a self,
        request: RequestId,
    ) -> BoxFuture<'a, Result<RunSnapshot, RunError>> {
        Box::pin(self.0.status_by_request(request))
    }
}
