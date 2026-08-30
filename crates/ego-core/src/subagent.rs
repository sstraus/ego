//! Durable isolated children over the ordinary rooted run controller.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::budget::BudgetGate;
use crate::digest::sha256_hex;
use crate::event::domain::{DomainEvent, Envelope, EventKind, SubagentRefusal, SubagentTerminal};
use crate::event::hook::{
    HookInvocationId, HookLifecycle, HookOccurrenceKey, HookOccurrenceOwner, HookSubject,
};
use crate::event::id::{
    EventId, GateId, ItemId, Producer, Seq, SessionId, SubagentInvocationId, TurnId,
};
use crate::event::time::{Clock, Timestamp};
use crate::hook::{HookEvent, HookLifecycleDispatcher, HookOccurrence};
use crate::run::{
    CancelKey, IdSource, RequestDigest, RequestId, RunCommandOutcome, RunController, RunError,
    RunExecution, RunExecutionBindings, RunExecutor, RunId, RunOrigin, RunRecoveryOutcome, RunRoot,
    RunSelections, RunSettlement, RunSnapshot, RunSubscription, StartRequest, TurnLease,
};
use crate::store::blob::BlobRef;
use crate::store::session_store::SessionStore;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRequest {
    invocation: SubagentInvocationId,
    parent_run: RunId,
    parent_turn: TurnId,
    parent_item: ItemId,
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
        parent_run: RunId,
        parent_turn: TurnId,
        parent_item: ItemId,
        task: BlobRef,
        selections: RunSelections,
        wait: ChildWait,
    ) -> Self {
        Self {
            invocation,
            parent_run,
            parent_turn,
            parent_item,
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

pub trait ChildJournal: Send + Sync {
    fn append(&self, event: DomainEvent) -> Result<(), ChildJournalError>;
    fn read(&self, invocation: SubagentInvocationId)
    -> Result<Vec<DomainEvent>, ChildJournalError>;
}

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

pub trait ChildTaskReader: Send + Sync {
    fn read_rendered_task(&self, reference: &BlobRef) -> Result<Vec<u8>, ChildResultReaderError>;
}

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

pub trait ChildResultReader: Send + Sync {
    fn read_final_assistant(
        &self,
        run: RunId,
    ) -> Result<ChildResultEvidence, ChildResultReaderError>;
}

pub trait ChildResultStore: Send + Sync {
    fn copy_to_parent(
        &self,
        child: RunId,
        evidence: &ChildResultEvidence,
    ) -> Result<BlobRef, ChildResultReaderError>;
}

pub trait ChildProfileResolver: Send + Sync {
    fn resolve_profile(&self, name: &str) -> Result<(), String>;
}

pub trait ChildSelectionAdmission: Send + Sync {
    fn admit(
        &self,
        request: &ChildRequest,
        bindings: &RunExecutionBindings,
    ) -> Result<(RunSelections, RunExecutionBindings), String>;
}

pub struct ChildServicePorts {
    parent_run: RunId,
    journal: Arc<dyn ChildJournal>,
    tasks: Arc<dyn ChildTaskReader>,
    results: Arc<dyn ChildResultReader>,
    parent_results: Arc<dyn ChildResultStore>,
    profiles: Arc<dyn ChildProfileResolver>,
    selections: Arc<dyn ChildSelectionAdmission>,
    hooks: Arc<HookLifecycleDispatcher>,
    producer: Producer,
}
impl ChildServicePorts {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        parent_run: RunId,
        journal: Arc<dyn ChildJournal>,
        tasks: Arc<dyn ChildTaskReader>,
        results: Arc<dyn ChildResultReader>,
        parent_results: Arc<dyn ChildResultStore>,
        profiles: Arc<dyn ChildProfileResolver>,
        selections: Arc<dyn ChildSelectionAdmission>,
        hooks: Arc<HookLifecycleDispatcher>,
        producer: Producer,
    ) -> Self {
        Self {
            parent_run,
            journal,
            tasks,
            results,
            parent_results,
            profiles,
            selections,
            hooks,
            producer,
        }
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
        self.inner.gate.lock().expect("child budget gate").id()
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
    pub async fn wait(&self) -> Result<ChildWaitResult, ChildServiceError> {
        finish(&self.inner, self.invocation).await
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
impl ChildRunService {
    pub fn new<S, E, C, I>(
        controller: Arc<RunController<S, E, C, I>>,
        gate: Arc<Mutex<BudgetGate>>,
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
    /// The already admitted parent identity required by typed child requests.
    pub fn parent_run(&self) -> RunId {
        self.inner.ports.parent_run
    }
    pub async fn start(&self, request: ChildRequest) -> Result<ChildAdmission, ChildServiceError> {
        start(&self.inner, request).await
    }
    pub async fn collect_background(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<ChildWaitResult, ChildServiceError> {
        finish(&self.inner, invocation).await
    }
    pub async fn recover_from_durable(
        &self,
        invocation: SubagentInvocationId,
        _cut: RecoveryCut,
    ) -> Result<Option<RecoveredChild>, ChildServiceError> {
        let events = self
            .inner
            .ports
            .journal
            .read(invocation)
            .map_err(journal_error)?;
        let Some(requested) = events.iter().find(|event| {
            matches!(&event.kind, EventKind::SubagentRequested { invocation: id, .. } if *id == invocation)
        }) else {
            return Ok(None);
        };
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
        let origin = RunOrigin::Subagent {
            invocation,
            parent_run: self.inner.ports.parent_run,
            parent_turn: requested.env.turn,
            parent_item,
        };
        validate_gate(&self.inner, &self.inner.bindings)?;
        if let Some(child) = events.iter().find_map(|event| match &event.kind {
            EventKind::SubagentSettled { child, .. } => Some(*child),
            _ => None,
        }) {
            return Ok(Some(RecoveredChild {
                invocation,
                child,
                origin,
            }));
        }
        if let Some(child) = events.iter().find_map(|event| match &event.kind {
            EventKind::SubagentSpawned { child, .. } => Some(*child),
            _ => None,
        }) {
            recover_active(
                &self.inner,
                invocation,
                child,
                requested.env.turn,
                parent_item,
                &origin,
            )
            .await?;
            return Ok(Some(RecoveredChild {
                invocation,
                child: Some(child),
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
                    requested.env.turn,
                    parent_item,
                    &origin,
                )
                .await?;
                append_lifecycle(
                    &self.inner,
                    invocation,
                    requested.env.turn,
                    Some(parent_item),
                    EventKind::SubagentSpawned {
                        invocation,
                        child: snapshot.run_id(),
                    },
                )?;
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
                    self.inner.bindings.clone(),
                )
                .await?;
                append_lifecycle(
                    &self.inner,
                    invocation,
                    requested.env.turn,
                    Some(parent_item),
                    EventKind::SubagentSpawned { invocation, child },
                )?;
                self.inner.active.lock().expect("active children").insert(
                    invocation,
                    ActiveChild {
                        run: child,
                        parent_turn: requested.env.turn,
                        parent_item,
                        execution: Some(execution),
                        cancel_key: Some(cancel_key),
                        subscription,
                        observed: None,
                        pending: None,
                        result: None,
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
    gate: Arc<Mutex<BudgetGate>>,
    root: RunRoot,
    bindings: RunExecutionBindings,
    ports: ChildServicePorts,
    active: Mutex<BTreeMap<SubagentInvocationId, ActiveChild>>,
}
struct ActiveChild {
    run: RunId,
    parent_turn: TurnId,
    parent_item: ItemId,
    execution: Option<RunExecution>,
    cancel_key: Option<CancelKey>,
    subscription: RunSubscription,
    observed: Option<RunSettlement>,
    pending: Option<ChildWaitResult>,
    result: Option<ChildWaitResult>,
}

async fn recover_active(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
    child: RunId,
    parent_turn: TurnId,
    parent_item: ItemId,
    expected_origin: &RunOrigin,
) -> Result<(), ChildServiceError> {
    let outcome = inner
        .controller
        .recover(child, inner.bindings.clone())
        .await
        .map_err(controller_error)?;
    let active = match outcome {
        RunRecoveryOutcome::Reattached(reattachment) => {
            validate_expected_origin(reattachment.snapshot(), expected_origin)?;
            let subscription = reattachment.subscription();
            let cancel_key = reattachment.cancel_key().clone();
            let execution = reattachment.into_execution();
            ActiveChild {
                run: child,
                parent_turn,
                parent_item,
                execution: Some(execution),
                cancel_key: Some(cancel_key),
                subscription,
                observed: None,
                pending: None,
                result: None,
            }
        }
        RunRecoveryOutcome::Observed(observation) => {
            validate_expected_origin(observation.snapshot(), expected_origin)?;
            ActiveChild {
                run: child,
                parent_turn,
                parent_item,
                execution: None,
                cancel_key: None,
                subscription: observation.subscription(),
                observed: observation.snapshot().settlement().cloned(),
                pending: None,
                result: None,
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
) -> Result<ChildAdmission, ChildServiceError> {
    if request.parent_run != inner.ports.parent_run {
        return Err(ChildServiceError::InvalidDurableState {
            diagnostic: "child request belongs to a different parent run".into(),
        });
    }
    if request.parent_transcript.is_some() {
        return Err(ChildServiceError::AuthorityWouldWiden {
            dimension: "parent transcript".into(),
        });
    }
    if let Some(profile) = request.requested_profile.as_deref() {
        if profile.is_empty() || inner.ports.profiles.resolve_profile(profile).is_err() {
            return Err(ChildServiceError::ProfileUnavailable {
                profile: profile.to_owned(),
            });
        }
        request.selections = request.selections.with_profile_name(profile.to_owned());
    }
    let (selections, bindings) = inner
        .ports
        .selections
        .admit(&request, &inner.bindings)
        .map_err(|diagnostic| ChildServiceError::AuthorityWouldWiden {
            dimension: authority_dimension(&diagnostic),
        })?;
    validate_gate(inner, &bindings)?;
    let prompt = read_task(inner, &request.task)?;
    let existing = inner
        .ports
        .journal
        .read(request.invocation)
        .map_err(journal_error)?;
    if !existing.iter().any(|event| {
        matches!(&event.kind, EventKind::SubagentRequested { invocation, .. } if *invocation == request.invocation)
    }) {
        append_lifecycle(
            inner, request.invocation, request.parent_turn, Some(request.parent_item),
            EventKind::SubagentRequested {
                invocation: request.invocation,
                task: request.task.clone(),
                selections: selections.clone(),
            },
        )?;
    }
    let origin = RunOrigin::Subagent {
        invocation: request.invocation,
        parent_run: request.parent_run,
        parent_turn: request.parent_turn,
        parent_item: request.parent_item,
    };
    let (child_run, execution, cancel_key, subscription, turn_lease) = admit(
        inner,
        request.invocation,
        origin.clone(),
        selections.clone(),
        prompt,
        bindings,
    )
    .await?;
    append_lifecycle(
        inner,
        request.invocation,
        request.parent_turn,
        Some(request.parent_item),
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
            parent_turn: request.parent_turn,
            parent_item: request.parent_item,
            execution: Some(execution),
            cancel_key: Some(cancel_key),
            subscription: subscription.clone(),
            observed: None,
            pending: None,
            result: None,
        },
    );
    dispatch_subagent(inner, request.invocation, HookLifecycle::SubagentStart).await?;
    Ok(ChildAdmission {
        inner: Arc::clone(inner),
        invocation: request.invocation,
        parent_run: request.parent_run,
        child_run,
        child_session,
        origin,
        turn_lease,
        selections,
        wait: request.wait,
        subscription,
    })
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
) -> Result<ChildWaitResult, ChildServiceError> {
    {
        let active = inner.active.lock().expect("active children");
        let child = active
            .get(&invocation)
            .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
        if let Some(result) = &child.result {
            return Ok(result.clone());
        }
        if child.pending.is_some() {
            drop(active);
            return persist_pending(inner, invocation);
        }
    }
    let prepared = prepare_result(inner, invocation).await?;
    let prepared = match prepared {
        ChildWaitResult::Returned { final_message } => {
            let outcome = dispatch_subagent(inner, invocation, HookLifecycle::SubagentStop).await?;
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
    set_pending(inner, invocation, prepared)?;
    persist_pending(inner, invocation)
}

async fn dispatch_subagent(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
    lifecycle: HookLifecycle,
) -> Result<crate::hook::HookRunResult, ChildServiceError> {
    let item = inner
        .active
        .lock()
        .expect("active children")
        .get(&invocation)
        .map(|child| child.parent_item)
        .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
    let subject = HookSubject::Item { item };
    let occurrence = HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Subagent { invocation }),
        HookEvent::turn(lifecycle, subject),
    )
    .map_err(controller_error)?;
    inner
        .ports
        .hooks
        .dispatch(inner.ports.producer.clone(), occurrence)
        .await
        .map_err(controller_error)
}

async fn prepare_result(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
) -> Result<ChildWaitResult, ChildServiceError> {
    {
        let active = inner.active.lock().expect("active children");
        let child = active
            .get(&invocation)
            .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
        if let Some(result) = &child.result {
            return Ok(result.clone());
        }
        if let Some(pending) = &child.pending {
            return Ok(pending.clone());
        }
    }
    enum ResultSource {
        Execute(Box<RunExecution>),
        Observed(RunSettlement),
        Wait(RunSubscription),
    }
    let (run, source) = {
        let mut active = inner.active.lock().expect("active children");
        let child = active
            .get_mut(&invocation)
            .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
        let source = if let Some(execution) = child.execution.take() {
            ResultSource::Execute(Box::new(execution))
        } else if let Some(settlement) = child.observed.clone() {
            ResultSource::Observed(settlement)
        } else {
            ResultSource::Wait(child.subscription.clone())
        };
        (child.run, source)
    };
    let settlement = match source {
        ResultSource::Execute(execution) => inner
            .controller
            .execute(*execution)
            .await
            .map_err(controller_error)?
            .settlement()
            .cloned()
            .unwrap_or(RunSettlement::Uncertain),
        ResultSource::Observed(settlement) => settlement,
        ResultSource::Wait(subscription) => subscription
            .await
            .settlement()
            .cloned()
            .unwrap_or(RunSettlement::Uncertain),
    };
    match settlement {
        RunSettlement::Completed => {
            let evidence = inner
                .ports
                .results
                .read_final_assistant(run)
                .map_err(result_error)?;
            validate_blob(evidence.item(), evidence.bytes())?;
            let final_message = inner
                .ports
                .parent_results
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

fn set_pending(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
    result: ChildWaitResult,
) -> Result<(), ChildServiceError> {
    let mut active = inner.active.lock().expect("active children");
    let child = active
        .get_mut(&invocation)
        .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
    if child.result.is_none() {
        child.pending = Some(result);
    }
    Ok(())
}

fn persist_pending(
    inner: &Arc<Inner>,
    invocation: SubagentInvocationId,
) -> Result<ChildWaitResult, ChildServiceError> {
    let (run, turn, item, result) = {
        let active = inner.active.lock().expect("active children");
        let child = active
            .get(&invocation)
            .ok_or(ChildServiceError::UnknownInvocation { invocation })?;
        if let Some(result) = &child.result {
            return Ok(result.clone());
        }
        (
            child.run,
            child.parent_turn,
            child.parent_item,
            child
                .pending
                .clone()
                .ok_or(ChildServiceError::UnknownInvocation { invocation })?,
        )
    };
    append_lifecycle(
        inner,
        invocation,
        turn,
        Some(item),
        EventKind::SubagentSettled {
            invocation,
            child: Some(run),
            terminal: terminal_of(&result),
        },
    )?;
    let mut active = inner.active.lock().expect("active children");
    let child = active
        .get_mut(&invocation)
        .expect("persisted child remains active");
    child.pending = None;
    child.result = Some(result.clone());
    Ok(result)
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

fn read_task(inner: &Inner, reference: &BlobRef) -> Result<String, ChildServiceError> {
    let bytes = inner
        .ports
        .tasks
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
    if !Arc::ptr_eq(&bound, &inner.gate) {
        return Err(ChildServiceError::BudgetDenied {
            diagnostic: "child bindings carry a different budget gate".into(),
        });
    }
    Ok(())
}

fn append_lifecycle(
    inner: &Inner,
    invocation: SubagentInvocationId,
    turn: TurnId,
    item: Option<ItemId>,
    kind: EventKind,
) -> Result<(), ChildServiceError> {
    let prior = inner
        .ports
        .journal
        .read(invocation)
        .map_err(journal_error)?;
    let mut seq = prior.last().map_or(Seq::ZERO, |event| event.env.seq);
    inner
        .ports
        .journal
        .append(DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn,
                item,
                parent: prior.last().map(|event| event.env.id),
                ts: Timestamp::EPOCH,
                producer: Producer::Main,
            },
            kind,
        })
        .map_err(journal_error)
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
