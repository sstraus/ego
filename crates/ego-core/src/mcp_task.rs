//! MCP Tasks for long-running server operations.
//!
//! A server may answer `tools/call` with a task. The model gets an immediate
//! answer; a session-owned [`McpTaskLedger`] polls the live tasks at each
//! generation boundary and hands the settled ones to the model as host context.
//! Tasks do not survive the process (coordinator, 2026-11-05), so nothing here is
//! journaled except the host-context text that delivers a settlement.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;

use crate::mcp::{McpOperationKind, McpProjectionError, McpSubject};
use crate::permit::{Authorized, GuardedIo, InheritedGrant, PreparedCall, ToolResult};
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

/// The authority to poll or cancel one MCP task.
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
        self.derive(McpOperationKind::GetTask)
    }

    /// The `tasks/cancel` call for this task, under the original's grant.
    pub fn cancel(&self) -> Authorized {
        self.derive(McpOperationKind::CancelTask)
    }

    fn derive(&self, operation: McpOperationKind) -> Authorized {
        let call = self
            .original
            .mcp_task_call(operation, &self.task_id)
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
    /// The server needs input. Ego does not answer it in v1: the task is cancelled.
    InputRequired,
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

/// Delivery of settled MCP tasks to the model.
///
/// The host owns the live tasks in memory. The turn loop registers a task when
/// its call answers with one, and asks at every generation boundary for the text
/// of tasks that settled, never waiting for a live task. Each text is journaled
/// once as host context. A task that settled is dropped by the port when it is
/// returned, which is what makes the delivery exactly once.
pub trait McpTaskPort: Send + Sync {
    fn started(&self, ticket: McpTaskTicket, start: &McpTaskStart);

    fn refresh<'a>(
        &'a self,
        effects: ToolEffectPorts<'a>,
    ) -> BoxFuture<'a, Result<Vec<String>, String>>;

    /// When the next live task is due for a poll, or now when a settled
    /// result waits for delivery; `None` with no live task. An idle
    /// interactive host wakes at this time.
    fn next_due(&self) -> Option<Instant> {
        None
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

    /// Ask each server to cancel every live task of the session, which is
    /// ending (exit, `/new`, resume, fork, compaction). Best effort: a cancel
    /// that fails is not retried and the task is dropped either way.
    fn cancel_all<'a>(&'a self, effects: ToolEffectPorts<'a>) -> BoxFuture<'a, ()>;
}

struct LiveTask {
    ticket: McpTaskTicket,
    poller: McpTaskPollerHandle,
    server: String,
    tool: String,
    interval: Duration,
    next_poll: Instant,
    expires: Option<Instant>,
}

/// The session's live tasks and their polling.
#[derive(Default)]
pub struct McpTaskLedger {
    live: Mutex<Vec<LiveTask>>,
    /// Settled results that an idle poll found, delivered by the next refresh.
    ready: Mutex<Vec<String>>,
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

    fn notice(task: &LiveTask, outcome: &str) -> String {
        format!(
            "MCP task {} (server {}, tool {}) {outcome}",
            task.ticket.task_id(),
            task.server,
            task.tool
        )
    }
}

impl McpTaskPort for McpTaskLedger {
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
            ticket,
            poller: start.poller.clone(),
            server: start.server.clone(),
            tool: start.tool.clone(),
            interval,
            next_poll: now + interval,
            expires: start.ttl_ms.map(|ttl| now + Duration::from_millis(ttl)),
        };
        if let Ok(mut live) = self.live.lock() {
            live.push(task);
        }
    }

    fn refresh<'a>(
        &'a self,
        effects: ToolEffectPorts<'a>,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async move {
            let mut settled = std::mem::take(
                &mut *self
                    .ready
                    .lock()
                    .map_err(|_| "task ledger lock".to_owned())?,
            );
            settled.extend(self.poll_now(effects).await?);
            Ok(settled)
        })
    }

    fn next_due(&self) -> Option<Instant> {
        // A buffered result is not a reason to wake again: the generation it
        // started delivers it, or the next one does when that turn stopped
        // before it generated (a budget or a pause).
        let live = self.live.lock().ok()?;
        // An expiry is noticed at the task's next poll, as in a generation.
        live.iter().map(|task| task.next_poll).min()
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
            let found = !settled.is_empty();
            ready.extend(settled);
            Ok(found)
        })
    }
}

impl McpTaskLedger {
    /// Poll every due task once; return the notices of those that settled.
    async fn poll_now(&self, effects: ToolEffectPorts<'_>) -> Result<Vec<String>, String> {
        {
            let now = Instant::now();
            // Take the due tasks out so no lock is held across an await; a task
            // that is still working goes back.
            let due = {
                let mut live = self
                    .live
                    .lock()
                    .map_err(|_| "task ledger lock".to_owned())?;
                let (due, waiting): (Vec<_>, Vec<_>) =
                    live.drain(..).partition(|task| task.next_poll <= now);
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
                    Ok(McpTaskPoll::Completed(result)) => settled.push(Self::notice(
                        &task,
                        &format!("completed:\n{}", result.model_text()),
                    )),
                    Ok(McpTaskPoll::Failed(error)) => {
                        settled.push(Self::notice(&task, &format!("failed: {error}")))
                    }
                    Ok(McpTaskPoll::Cancelled) => {
                        settled.push(Self::notice(&task, "was cancelled by the server"))
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
