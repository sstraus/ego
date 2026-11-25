//! MCP Tasks for long-running server operations (stories 886-78a2, 887-3ab1,
//! 888-dd64).
//!
//! A server may answer `tools/call` with a task. The model gets an immediate
//! answer; a session-owned [`McpTaskLedger`] polls the live tasks at each
//! generation boundary and hands the settled ones to the model as host context.
//! Tasks do not survive the process (coordinator, 2026-11-05), so nothing here is
//! journaled except the host-context result that delivers a settlement.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;

use crate::mcp::{McpOperationKind, McpProjectionError, McpSubject};
use crate::permit::{Authorized, GuardedIo, InheritedGrant, PreparedCall, ToolResult};
use crate::question::{
    ElicitationAnswers, ElicitationKind, ElicitationMember, ElicitationRound, Elicitor,
    QuestionAnswer, QuestionOutcome,
};
use crate::turn::ToolEffectPorts;

/// Longest task id ego accepts from a server.
const MAX_TASK_ID_BYTES: usize = 256;

/// Poll no faster than this, whatever the server suggests.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Poll no slower than this, whatever the server suggests.
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Poll interval when the server suggests none.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn valid_task_id(task_id: &str) -> bool {
    !task_id.trim().is_empty()
        && task_id.len() <= MAX_TASK_ID_BYTES
        && !task_id.chars().any(char::is_control)
}

/// The authority to poll or cancel one MCP task (story 886-78a2).
///
/// Only a [`McpTaskSeed`], taken from the grant that let a `tools/call` run,
/// makes one. A poll or a cancel is then that same prepared call with the same
/// invocation identities, transport and admission, minted under the same grant:
/// it is **not** classified again, so an original that a human approved is not
/// asked about at every poll. No other code path builds a `GetTask` or
/// `CancelTask` call, so a task id that did not come from this session's own
/// call has no authority.
#[derive(Debug, Clone)]
pub struct McpTaskTicket {
    original: PreparedCall,
    grant: InheritedGrant,
    task_id: String,
}

impl McpTaskTicket {
    /// Record `task_id` against the prepared `tools/call` that produced it and
    /// the permit and authority that let it run.
    pub fn record(
        original: &PreparedCall,
        permit: &crate::permit::Permit,
        by: &crate::permit::Authority,
        task_id: impl Into<String>,
    ) -> Result<Self, McpProjectionError> {
        Self::from_grant(original, InheritedGrant::of(permit, by), task_id.into())
    }

    fn from_grant(
        original: &PreparedCall,
        grant: InheritedGrant,
        task_id: String,
    ) -> Result<Self, McpProjectionError> {
        if !valid_task_id(&task_id) {
            return Err(McpProjectionError::InvalidIdentity);
        }
        let envelope = original
            .mcp_envelope()
            .ok_or(McpProjectionError::InvalidIdentity)?;
        if envelope.operation() != McpOperationKind::CallTool
            || !matches!(envelope.subject(), McpSubject::Tool { .. })
        {
            return Err(McpProjectionError::InvalidIdentity);
        }
        Ok(Self {
            original: original.clone(),
            grant,
            task_id,
        })
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// The `tasks/get` call for this task, under the original's grant.
    pub fn poll(&self) -> Authorized {
        self.derive(McpOperationKind::GetTask, None)
    }

    /// The `tasks/cancel` call for this task, under the original's grant.
    pub fn cancel(&self) -> Authorized {
        self.derive(McpOperationKind::CancelTask, None)
    }

    fn derive(
        &self,
        operation: McpOperationKind,
        responses: Option<serde_json::Value>,
    ) -> Authorized {
        let call = self
            .original
            .mcp_task_call(operation, &self.task_id, responses)
            .expect("a ticket is recorded from a validated tools/call");
        self.grant.mint(call)
    }
}

/// What a task ticket needs from a `tools/call` before it runs: the call and the
/// grant that let it run. The executor consumes the authorization, so the seed is
/// taken first; it becomes a ticket only when the server answers with a task.
#[derive(Debug, Clone)]
pub struct McpTaskSeed {
    original: PreparedCall,
    grant: InheritedGrant,
}

impl McpTaskSeed {
    /// `None` unless `authorized` answers an MCP `tools/call`.
    pub fn of(authorized: &Authorized) -> Option<Self> {
        let envelope = authorized.call().mcp_envelope()?;
        (envelope.operation() == McpOperationKind::CallTool
            && matches!(envelope.subject(), McpSubject::Tool { .. }))
        .then(|| Self {
            original: authorized.call().clone(),
            grant: InheritedGrant::of(authorized.permit(), authorized.by()),
        })
    }

    pub fn into_ticket(self, task_id: &str) -> Result<McpTaskTicket, McpProjectionError> {
        McpTaskTicket::from_grant(&self.original, self.grant, task_id.to_owned())
    }
}

/// What one `tasks/get` poll reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTaskPoll {
    Working,
    /// The server requested unsupported input.
    InputRequired,
    Forms(BTreeMap<String, ElicitationMember>),
    /// The final result of the original `tools/call`.
    Completed(ToolResult),
    /// The error that ended the task, as bounded text.
    Failed(String),
    Cancelled,
}

/// Sends the poll and the cancel for tasks of one server. The adapter that made
/// the task owns it, because only that adapter knows the transport.
pub trait McpTaskPoller: Send + Sync {
    fn poll<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<McpTaskPoll, String>>
    where
        'io: 'a;

    fn update<'a, 'io>(
        &'a self,
        _call: PreparedCall,
        _io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<(), String>>
    where
        'io: 'a,
    {
        Box::pin(futures::future::ready(Err(
            "task updates are unsupported".to_owned()
        )))
    }

    fn cancel<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<(), String>>
    where
        'io: 'a;
}

/// A shared [`McpTaskPoller`], comparable by identity so a [`McpTaskStart`] can
/// sit in a `ToolExecution`.
#[derive(Clone)]
pub struct McpTaskPollerHandle(pub Arc<dyn McpTaskPoller>);

impl std::fmt::Debug for McpTaskPollerHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("McpTaskPollerHandle")
    }
}

impl PartialEq for McpTaskPollerHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for McpTaskPollerHandle {}

/// The server made a task of a `tools/call`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTaskStart {
    pub task_id: String,
    pub server: String,
    pub tool: String,
    /// The server's suggested poll interval.
    pub poll_interval_ms: Option<u64>,
    /// The server's time to live for the task.
    pub ttl_ms: Option<u64>,
    /// The model's immediate answer: task id, server, tool, "result follows".
    pub result: ToolResult,
    pub poller: McpTaskPollerHandle,
}

/// Delivery of settled MCP tasks to the model (stories 887-3ab1, 888-dd64).
///
/// The host owns the live tasks in memory. The turn loop registers a task when
/// its call answers with one, and asks at every generation boundary for the text
/// of tasks that settled, never waiting for a live task. Each text is journaled
/// once as host context. A task that settled is dropped by the port when it is
/// returned, which is what makes the delivery exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTaskSettlement {
    pub text: String,
    pub result: Option<ToolResult>,
}

impl From<String> for McpTaskSettlement {
    fn from(text: String) -> Self {
        Self { text, result: None }
    }
}

pub trait McpTaskPort: Send + Sync {
    fn started(&self, ticket: McpTaskTicket, start: &McpTaskStart);

    fn refresh<'a>(
        &'a self,
        effects: ToolEffectPorts<'a>,
    ) -> BoxFuture<'a, Result<Vec<McpTaskSettlement>, String>>;

    /// When the next live task is due for a poll, or now when a settled
    /// result waits for delivery; `None` with no live task. An idle
    /// interactive host wakes at this time (story 890-7df7).
    fn next_due(&self) -> Option<Instant> {
        None
    }

    /// Work that would be discarded if the owner ended this session now.
    fn has_pending_work(&self) -> bool {
        self.next_due().is_some()
    }

    /// Poll the due tasks outside a generation and keep the settled ones for
    /// the next [`Self::refresh`]. True when a settled result waits, so an
    /// idle host starts one generation for it and none for a task that is
    /// still working.
    fn poll_due<'a>(
        &'a self,
        _effects: ToolEffectPorts<'a>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(futures::future::ready(Ok(false)))
    }

    /// Present a queued task form using the trusted human-input port.
    fn answer_pending<'a>(
        &'a self,
        _effects: ToolEffectPorts<'a>,
        _elicitor: &'a dyn Elicitor,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(futures::future::ready(Ok(false)))
    }

    /// Revoke automatic continuation for tasks admitted since `turn_started`,
    /// the start of the turn the user stopped; tasks of earlier turns keep
    /// their wakes. Their eventual results remain available to `refresh`.
    fn stop_automatic_wakes(&self, _turn_started: Instant) {}

    /// Ask each server to cancel every live task of the session, which is
    /// ending (exit, `/new`, resume, fork, compaction). Best effort: a cancel
    /// that fails is not retried and the task is dropped either way.
    fn cancel_all<'a>(&'a self, effects: ToolEffectPorts<'a>) -> BoxFuture<'a, ()>;
}

struct LiveTask {
    automatic_wake: bool,
    admitted: Instant,
    ticket: McpTaskTicket,
    poller: McpTaskPollerHandle,
    server: String,
    tool: String,
    interval: Duration,
    next_poll: Instant,
    expires: Option<Instant>,
    forms: Option<ElicitationRound>,
    answered: BTreeSet<String>,
}

/// The session's live tasks and their polling (story 888-dd64).
#[derive(Default)]
pub struct McpTaskLedger {
    live: Mutex<Vec<LiveTask>>,
    /// Settled results that an idle poll found, delivered by the next refresh.
    ready: Mutex<Vec<McpTaskSettlement>>,
    min_interval: Option<Duration>,
}

impl McpTaskLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the 500 ms floor of the poll interval (tests).
    #[must_use]
    pub fn with_min_interval(mut self, floor: Duration) -> Self {
        self.min_interval = Some(floor);
        self
    }

    /// Live tasks not yet settled.
    pub fn live(&self) -> usize {
        self.live.lock().map(|live| live.len()).unwrap_or(0)
    }

    fn notice(task: &LiveTask, outcome: &str) -> (McpTaskSettlement, bool) {
        (
            format!(
                "MCP task {} (server {}, tool {}) {outcome}",
                task.ticket.task_id(),
                task.server,
                task.tool
            )
            .into(),
            task.automatic_wake,
        )
    }
}

impl McpTaskPort for McpTaskLedger {
    fn stop_automatic_wakes(&self, turn_started: Instant) {
        if let Ok(mut live) = self.live.lock() {
            for task in live.iter_mut().filter(|task| task.admitted >= turn_started) {
                task.automatic_wake = false;
                if task.forms.take().is_some() {
                    // Recheck the server's state without opening a stopped
                    // turn's form. Outstanding forms take the cancel path.
                    task.next_poll = Instant::now();
                }
            }
        }
    }

    fn cancel_all<'a>(&'a self, effects: ToolEffectPorts<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // A result buffered for this session must not reach the next one.
            if let Ok(mut ready) = self.ready.lock() {
                ready.clear();
            }
            let live = match self.live.lock() {
                Ok(mut live) => std::mem::take(&mut *live),
                Err(_) => return,
            };
            // Every cancel runs at once, so an unreachable server costs one
            // deadline and not one per task.
            futures::future::join_all(live.iter().map(|task| {
                let (call, io) = effects.clone().bind(task.ticket.cancel());
                async move {
                    let _ = task.poller.0.cancel(call, &io).await;
                }
            }))
            .await;
        })
    }

    fn started(&self, ticket: McpTaskTicket, start: &McpTaskStart) {
        let now = Instant::now();
        let floor = self.min_interval.unwrap_or(MIN_POLL_INTERVAL);
        let interval = start
            .poll_interval_ms
            .map_or(DEFAULT_POLL_INTERVAL, Duration::from_millis)
            .max(floor)
            .min(MAX_POLL_INTERVAL.max(floor));
        let task = LiveTask {
            automatic_wake: true,
            admitted: now,
            ticket,
            poller: start.poller.clone(),
            server: start.server.clone(),
            tool: start.tool.clone(),
            interval,
            next_poll: now + interval,
            expires: start
                .ttl_ms
                .and_then(|ttl| now.checked_add(Duration::from_millis(ttl))),
            forms: None,
            answered: BTreeSet::new(),
        };
        if let Ok(mut live) = self.live.lock() {
            live.push(task);
        }
    }

    fn refresh<'a>(
        &'a self,
        effects: ToolEffectPorts<'a>,
    ) -> BoxFuture<'a, Result<Vec<McpTaskSettlement>, String>> {
        Box::pin(async move {
            let mut settled = std::mem::take(
                &mut *self
                    .ready
                    .lock()
                    .map_err(|_| "task ledger lock".to_owned())?,
            );
            settled.extend(
                self.poll_now(effects)
                    .await?
                    .into_iter()
                    .map(|(text, _)| text),
            );
            Ok(settled)
        })
    }

    fn next_due(&self) -> Option<Instant> {
        // A buffered result is not a reason to wake again: the generation it
        // started delivers it, or the next one does when that turn stopped
        // before it generated (a budget or a pause).
        let live = self.live.lock().ok()?;
        // An expiry is noticed at the task's next poll, as in a generation.
        live.iter()
            .filter_map(|task| match &task.forms {
                Some(_) if task.automatic_wake => Some(Instant::now()),
                Some(_) => None,
                None => Some(task.next_poll),
            })
            .min()
    }

    fn has_pending_work(&self) -> bool {
        self.live.lock().map_or(true, |live| !live.is_empty())
            || self.ready.lock().map_or(true, |ready| !ready.is_empty())
    }

    fn poll_due<'a>(&'a self, effects: ToolEffectPorts<'a>) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let settled = self.poll_now(effects).await?;
            let mut ready = self
                .ready
                .lock()
                .map_err(|_| "task ledger lock".to_owned())?;
            // Only a result this poll found starts a generation; a buffered
            // one waits for the next generation without starting more.
            let found = settled.iter().any(|(_, automatic_wake)| *automatic_wake);
            ready.extend(settled.into_iter().map(|(text, _)| text));
            Ok(found)
        })
    }

    fn answer_pending<'a>(
        &'a self,
        effects: ToolEffectPorts<'a>,
        elicitor: &'a dyn Elicitor,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let task = {
                let mut live = self
                    .live
                    .lock()
                    .map_err(|_| "task ledger lock".to_owned())?;
                live.iter()
                    .position(|task| task.forms.is_some() && task.automatic_wake)
                    .map(|index| live.remove(index))
            };
            let Some(mut task) = task else {
                return Ok(false);
            };
            let round = task.forms.take().expect("a pending round was selected");
            let responses = task_form_responses(&task, &round, elicitor).await;
            let message = match responses {
                Some(responses) if task.expires.is_none_or(|expires| Instant::now() < expires) => {
                    let authorization = task
                        .ticket
                        .derive(McpOperationKind::UpdateTask, Some(responses));
                    let (call, io) = effects.clone().bind(authorization);
                    if task.poller.0.update(call, &io).await.is_ok() {
                        task.answered.extend(round.requests.into_keys());
                        task.next_poll = Instant::now() + task.interval;
                        self.live
                            .lock()
                            .map_err(|_| "task ledger lock".to_owned())?
                            .push(task);
                        return Ok(false);
                    }
                    "form update was not acknowledged and may have reached the server; ego will not resend it and asked the server to cancel the task"
                }
                _ => {
                    "form was declined, unavailable or expired; ego asked the server to cancel the task"
                }
            };
            let (call, io) = effects.bind(task.ticket.cancel());
            let _ = task.poller.0.cancel(call, &io).await;
            self.ready
                .lock()
                .map_err(|_| "task ledger lock".to_owned())?
                .push(Self::notice(&task, message).0);
            Ok(true)
        })
    }
}

async fn task_form_responses(
    task: &LiveTask,
    round: &ElicitationRound,
    elicitor: &dyn Elicitor,
) -> Option<serde_json::Value> {
    let mut responses = BTreeMap::new();
    for (key, member) in &round.requests {
        if task
            .expires
            .is_some_and(|expires| Instant::now() >= expires)
        {
            return None;
        }
        let ElicitationKind::Form { schema } = &member.kind else {
            return None;
        };
        let mut request = member.form_request(schema);
        let message: String = request
            .prompt
            .chars()
            .filter(|c| !c.is_control() && !matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
            .take(512)
            .collect();
        request.prompt = format!(
            "MCP task from {} / {}\nServer request (do not enter credentials): {message}",
            task.server.escape_default(),
            task.tool.escape_default()
        );
        let QuestionOutcome::Answered(QuestionAnswer::Form(answer)) = elicitor
            .elicit_until(crate::event::id::ItemId::new(), request, task.expires)
            .await
            .ok()?
        else {
            return None;
        };
        responses.insert(key.clone(), serde_json::Value::Object(answer));
    }
    round
        .input_responses(&ElicitationAnswers {
            confirmed: true,
            responses,
        })
        .ok()
}

impl McpTaskLedger {
    /// Poll every due task once; return the notices of those that settled.
    async fn poll_now(
        &self,
        effects: ToolEffectPorts<'_>,
    ) -> Result<Vec<(McpTaskSettlement, bool)>, String> {
        {
            let now = Instant::now();
            // Take the due tasks out so no lock is held across an await; a task
            // that is still working goes back.
            let due = {
                let mut live = self
                    .live
                    .lock()
                    .map_err(|_| "task ledger lock".to_owned())?;
                let (due, waiting): (Vec<_>, Vec<_>) = live
                    .drain(..)
                    .partition(|task| task.forms.is_none() && task.next_poll <= now);
                *live = waiting;
                due
            };
            let mut settled = Vec::new();
            let mut still_working = Vec::new();
            for mut task in due {
                if task.expires.is_some_and(|expires| expires <= now) {
                    settled.push(Self::notice(&task, "expired before it settled"));
                    continue;
                }
                let (call, io) = effects.clone().bind(task.ticket.poll());
                match task.poller.0.poll(call, &io).await {
                    Ok(McpTaskPoll::Working) => {
                        task.next_poll = Instant::now() + task.interval;
                        still_working.push(task);
                    }
                    Ok(McpTaskPoll::Completed(result)) => {
                        let outcome = if result.foreign_tool_error() {
                            "completed with an error result"
                        } else {
                            "completed"
                        };
                        let (mut notice, wake) = Self::notice(&task, outcome);
                        notice.result = Some(result);
                        settled.push((notice, wake));
                    }
                    Ok(McpTaskPoll::Failed(error)) => {
                        settled.push(Self::notice(&task, &format!("failed: {error}")))
                    }
                    Ok(McpTaskPoll::Cancelled) => {
                        settled.push(Self::notice(&task, "was cancelled by the server"))
                    }
                    Ok(McpTaskPoll::Forms(mut forms)) => {
                        forms.retain(|key, _| !task.answered.contains(key));
                        let round = ElicitationRound {
                            expires_at_ms: None,
                            requests: forms,
                            completed: Default::default(),
                        };
                        if round.requests.is_empty()
                            || (task.automatic_wake
                                && round.validate().is_ok()
                                && task.answered.len() + round.requests.len() <= 64)
                        {
                            task.next_poll = Instant::now() + task.interval;
                            task.forms = (!round.requests.is_empty()).then_some(round);
                            still_working.push(task);
                        } else {
                            let (call, io) = effects.clone().bind(task.ticket.cancel());
                            let _ = task.poller.0.cancel(call, &io).await;
                            let reason = if task.automatic_wake {
                                "invalid or excessive form requests; ego asked the server to cancel it"
                            } else {
                                "needs input after the operator stopped the turn; ego asked the server to cancel it"
                            };
                            settled.push(Self::notice(&task, reason));
                        }
                    }
                    Ok(McpTaskPoll::InputRequired) => {
                        let (call, io) = effects.clone().bind(task.ticket.cancel());
                        let _ = task.poller.0.cancel(call, &io).await;
                        settled.push(Self::notice(
                            &task,
                            "needs input from the client; ego does not answer it, so the task was cancelled",
                        ));
                    }
                    Err(error) => {
                        // The server may still be running it: ask it to stop,
                        // best effort, before the ledger forgets the task.
                        let (call, io) = effects.clone().bind(task.ticket.cancel());
                        let _ = task.poller.0.cancel(call, &io).await;
                        settled.push(Self::notice(
                            &task,
                            &format!(
                                "could not be polled, so ego asked the server to cancel it: {error}"
                            ),
                        ));
                    }
                }
            }
            if !still_working.is_empty() {
                self.live
                    .lock()
                    .map_err(|_| "task ledger lock".to_owned())?
                    .extend(still_working);
            }
            Ok(settled)
        }
    }
}
