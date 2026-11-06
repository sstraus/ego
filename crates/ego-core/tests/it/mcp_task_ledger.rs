//! The session ledger that polls MCP tasks and delivers the
//! settled ones. The poller is a scripted fake; the ledger, the derived
//! GetTask/CancelTask calls and the effect binding are real.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_core::evidence::{
    EvidenceError as AdmissionError, EvidenceKey, EvidenceRecord, EvidenceStore as AdmissionStore,
    EvidenceWrite,
};
use ego_core::mcp::{
    McpOperationKind, McpTaskLedger, McpTaskPoll, McpTaskPoller, McpTaskPollerHandle, McpTaskPort,
    McpTaskStart,
};
use ego_core::permit::{
    DuplexSpawner, GuardedIo, OneHop, OneHopResponse, PreparedCall, ProcessError, ProcessExecutor,
    Resolver, ToolResult,
};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::turn::ToolEffectPorts;
use futures::executor::block_on;
use futures::future::BoxFuture;
use serde_json::Value;

use crate::common;
use crate::mcp_task_ticket::ticket;

struct NoEvidence;

impl AdmissionStore for NoEvidence {
    fn write(
        &self,
        _key: EvidenceKey,
        _record: EvidenceRecord,
    ) -> Result<EvidenceWrite, AdmissionError> {
        Ok(EvidenceWrite::Published)
    }
}

impl EvidenceStore for NoEvidence {
    fn blob_store(&self) -> &dyn ego_core::store::blob::BlobStore {
        &common::EMPTY_BLOB_STORE
    }

    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Err(EvidenceError::Limit { limit: 0 })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(futures::future::ready(Err(EvidenceError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }
}

struct NoProcess;

impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        panic!("a task poll runs no process")
    }
}

struct NoResolver;

impl Resolver for NoResolver {
    fn resolve(
        &self,
        _: ego_core::permit::NetworkTarget,
        _: std::time::Instant,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, ego_core::permit::TransportError>>
    {
        panic!("the scripted poller resolves nothing")
    }
}

struct NoHop;

impl OneHop for NoHop {
    fn begin_call(&self) -> std::time::Instant {
        std::time::Instant::now() + Duration::from_secs(60)
    }
    fn execute(
        &self,
        _: ego_core::permit::ResolvedConnect,
        _: ego_core::permit::HttpRequest,
        _: std::time::Instant,
    ) -> BoxFuture<'static, Result<OneHopResponse, ego_core::permit::TransportError>> {
        panic!("the scripted poller sends nothing")
    }
}

struct NoDuplex;

impl DuplexSpawner for NoDuplex {
    fn spawn(
        &self,
        _: ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'static, Result<ego_core::permit::DuplexHandle, ego_core::permit::TransportError>>
    {
        panic!("the scripted poller spawns nothing")
    }
}

/// Answers each poll from a script and records every call it was given.
#[derive(Default)]
struct Scripted {
    polls: Mutex<Vec<Result<McpTaskPoll, String>>>,
    seen: Mutex<Vec<(McpOperationKind, Value)>>,
}

impl Scripted {
    fn record(&self, call: &PreparedCall) {
        let envelope = call.mcp_envelope().expect("an MCP envelope");
        let parameters = serde_json::from_slice(envelope.parameters()).expect("json");
        self.seen
            .lock()
            .unwrap()
            .push((envelope.operation(), parameters));
    }

    fn kinds(&self) -> Vec<McpOperationKind> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, _)| *kind)
            .collect()
    }
}

impl McpTaskPoller for Scripted {
    fn poll<'a, 'io>(
        &'a self,
        call: PreparedCall,
        _: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<McpTaskPoll, String>>
    where
        'io: 'a,
    {
        self.record(&call);
        let next = self.polls.lock().unwrap().remove(0);
        Box::pin(futures::future::ready(next))
    }

    fn cancel<'a, 'io>(
        &'a self,
        call: PreparedCall,
        _: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<(), String>>
    where
        'io: 'a,
    {
        self.record(&call);
        Box::pin(futures::future::ready(Ok(())))
    }
}

fn start(poller: &Arc<Scripted>, ttl_ms: Option<u64>) -> McpTaskStart {
    McpTaskStart {
        task_id: "task-1".into(),
        server: "alpha".into(),
        tool: "weather".into(),
        poll_interval_ms: Some(0),
        ttl_ms,
        result: ToolResult::builtin("task started"),
        poller: McpTaskPollerHandle(Arc::clone(poller) as Arc<dyn McpTaskPoller>),
    }
}

fn ledger_with(
    polls: Vec<Result<McpTaskPoll, String>>,
    ttl_ms: Option<u64>,
) -> (McpTaskLedger, Arc<Scripted>) {
    let poller = Arc::new(Scripted {
        polls: Mutex::new(polls),
        seen: Mutex::default(),
    });
    let ledger = McpTaskLedger::new().with_min_interval(Duration::ZERO);
    ledger.started(ticket("task-1"), &start(&poller, ttl_ms));
    (ledger, poller)
}

fn refresh(ledger: &McpTaskLedger) -> Result<Vec<String>, String> {
    let (fs, process, evidence) = (
        ego_core::permit::fake::FakeFs::default(),
        NoProcess,
        NoEvidence,
    );
    let effects = ToolEffectPorts::new(
        &fs,
        &process,
        &evidence,
        Arc::new(NoResolver),
        Arc::new(NoHop),
        Arc::new(NoDuplex),
    );
    block_on(ledger.refresh(effects))
}

// Catches: a settled task is delivered twice, or stays live after delivery.
#[test]
fn a_completed_task_is_delivered_once_with_its_result_and_dropped() {
    let done = McpTaskPoll::Completed(ToolResult::builtin("sunny"));
    let (ledger, poller) = ledger_with(vec![Ok(done)], None);
    let first = refresh(&ledger).expect("refresh");
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(
        first[0].contains("task-1") && first[0].contains("alpha") && first[0].contains("sunny")
    );
    assert_eq!(ledger.live(), 0);
    assert!(refresh(&ledger).expect("again").is_empty());
    assert_eq!(poller.kinds(), [McpOperationKind::GetTask]);
}

// Catches: a working task is dropped, or polled by a call other than tasks/get for its id.
#[test]
fn a_working_task_stays_live_and_is_polled_with_its_id() {
    let (ledger, poller) = ledger_with(
        vec![Ok(McpTaskPoll::Working), Ok(McpTaskPoll::Cancelled)],
        None,
    );
    assert!(refresh(&ledger).expect("first").is_empty());
    assert_eq!(ledger.live(), 1);
    let second = refresh(&ledger).expect("second");
    assert!(second[0].contains("cancelled by the server"), "{second:?}");
    for (kind, parameters) in poller.seen.lock().unwrap().iter() {
        assert_eq!(*kind, McpOperationKind::GetTask);
        assert_eq!(parameters, &serde_json::json!({"taskId":"task-1"}));
    }
}

// Catches: a task is polled at every generation whatever interval the server asked for.
#[test]
fn a_task_that_is_not_due_is_not_polled() {
    let poller = Arc::new(Scripted::default());
    let ledger = McpTaskLedger::new().with_min_interval(Duration::from_secs(3600));
    let mut task = start(&poller, None);
    task.poll_interval_ms = Some(3_600_000);
    ledger.started(ticket("task-1"), &task);
    assert!(refresh(&ledger).expect("refresh").is_empty());
    assert!(poller.kinds().is_empty());
    assert_eq!(ledger.live(), 1);
}

// Catches: ego answers a server's input request unasked, or leaves the server task running.
#[test]
fn an_input_request_cancels_the_task_and_says_so() {
    let (ledger, poller) = ledger_with(vec![Ok(McpTaskPoll::InputRequired)], None);
    let notices = refresh(&ledger).expect("refresh");
    assert!(notices[0].contains("cancelled"), "{notices:?}");
    assert_eq!(
        poller.kinds(),
        [McpOperationKind::GetTask, McpOperationKind::CancelTask]
    );
    assert_eq!(ledger.live(), 0);
}

// Catches: a failed poll or a failed task is lost instead of reaching the model.
#[test]
fn a_failed_task_and_a_failed_poll_reach_the_model() {
    for (script, expected) in [
        (Ok(McpTaskPoll::Failed("boom".into())), "failed: boom"),
        (
            Err("transport down".into()),
            "asked the server to cancel it: transport down",
        ),
    ] {
        let (ledger, _) = ledger_with(vec![script], None);
        let notices = refresh(&ledger).expect("refresh");
        assert!(notices[0].contains(expected), "{notices:?}");
        assert_eq!(ledger.live(), 0);
    }
}

// Catches: a task past its time to live is polled forever.
#[test]
fn an_expired_task_is_reported_without_a_poll() {
    let (ledger, poller) = ledger_with(Vec::new(), Some(0));
    let notices = refresh(&ledger).expect("refresh");
    assert!(notices[0].contains("expired"), "{notices:?}");
    assert!(poller.kinds().is_empty());
}

// Catches: a session that ends leaves its server tasks running, or cancels a task twice.
#[test]
fn ending_the_session_cancels_every_live_task_once_and_empties_the_ledger() {
    let (ledger, poller) = ledger_with(Vec::new(), None);
    let (fs, process, evidence) = (
        ego_core::permit::fake::FakeFs::default(),
        NoProcess,
        NoEvidence,
    );
    let effects = ToolEffectPorts::new(
        &fs,
        &process,
        &evidence,
        Arc::new(NoResolver),
        Arc::new(NoHop),
        Arc::new(NoDuplex),
    );
    block_on(ledger.cancel_all(effects.clone()));
    assert_eq!(poller.kinds(), [McpOperationKind::CancelTask]);
    assert_eq!(ledger.live(), 0);
    block_on(ledger.cancel_all(effects));
    assert_eq!(
        poller.kinds().len(),
        1,
        "a cancelled task is not cancelled again"
    );
}

// Catches: a poll that fails (network blip, turn cancelled mid-poll) drops the
// task from the ledger without a tasks/cancel, so the server keeps running it
// after the session ends.
#[test]
fn a_task_whose_poll_failed_is_cancelled_on_the_server() {
    let (ledger, poller) = ledger_with(vec![Err("connection reset".into())], None);
    refresh(&ledger).expect("refresh");
    let (fs, process, evidence) = (
        ego_core::permit::fake::FakeFs::default(),
        NoProcess,
        NoEvidence,
    );
    let effects = ToolEffectPorts::new(
        &fs,
        &process,
        &evidence,
        Arc::new(NoResolver),
        Arc::new(NoHop),
        Arc::new(NoDuplex),
    );
    block_on(ledger.cancel_all(effects));
    assert_eq!(
        poller.kinds(),
        [McpOperationKind::GetTask, McpOperationKind::CancelTask],
        "the failed task must be cancelled once, at the failure or at session end"
    );
}

fn poll_due(ledger: &McpTaskLedger) -> Result<bool, String> {
    let (fs, process, evidence) = (
        ego_core::permit::fake::FakeFs::default(),
        NoProcess,
        NoEvidence,
    );
    let effects = ToolEffectPorts::new(
        &fs,
        &process,
        &evidence,
        Arc::new(NoResolver),
        Arc::new(NoHop),
        Arc::new(NoDuplex),
    );
    block_on(ledger.poll_due(effects))
}

// Catches: a result that an idle poll buffered survives the end of its
// session, so the next session (after /new, resume or fork) gives the model a
// task result from a conversation it never had.
#[test]
fn ending_the_session_drops_a_result_an_idle_poll_buffered() {
    let done = McpTaskPoll::Completed(ToolResult::builtin("sunny"));
    let (ledger, _) = ledger_with(vec![Ok(done)], None);
    assert!(poll_due(&ledger).expect("idle poll"), "the result waits");
    let (fs, process, evidence) = (
        ego_core::permit::fake::FakeFs::default(),
        NoProcess,
        NoEvidence,
    );
    let effects = ToolEffectPorts::new(
        &fs,
        &process,
        &evidence,
        Arc::new(NoResolver),
        Arc::new(NoHop),
        Arc::new(NoDuplex),
    );
    block_on(ledger.cancel_all(effects));
    let next_session = refresh(&ledger).expect("refresh");
    assert!(next_session.is_empty(), "{next_session:?}");
}

// Catches: a buffered result keeps the idle timer due "now", so when
// the checkpoint turn ends before a generation (token budget reached, a durable
// pause) and never drains it, the seat is woken and a turn started every 500 ms.
#[test]
fn a_buffered_result_with_no_live_task_does_not_keep_the_idle_timer_due() {
    let done = McpTaskPoll::Completed(ToolResult::builtin("sunny"));
    let (ledger, _) = ledger_with(vec![Ok(done)], None);
    assert!(poll_due(&ledger).expect("idle poll"), "the result waits");
    assert_eq!(ledger.live(), 0);
    assert_eq!(ledger.next_due(), None);
}
