//! Story 888-dd64: the session ledger that polls MCP tasks and delivers the
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
pub(crate) struct Scripted {
    polls: Mutex<Vec<Result<McpTaskPoll, String>>>,
    seen: Mutex<Vec<(McpOperationKind, Value)>>,
}

impl Scripted {
    pub(crate) fn completing(result: ToolResult) -> Arc<Self> {
        Arc::new(Self {
            polls: Mutex::new(vec![Ok(McpTaskPoll::Completed(result))]),
            seen: Mutex::default(),
        })
    }

    pub(crate) fn calls(&self) -> Vec<(McpOperationKind, Value)> {
        self.seen.lock().unwrap().clone()
    }

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

    fn update<'a, 'io>(
        &'a self,
        call: PreparedCall,
        _: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<(), String>>
    where
        'io: 'a,
    {
        self.record(&call);
        Box::pin(futures::future::ready(Err(
            "connection lost after send".into()
        )))
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
    block_on(ledger.refresh(effects)).map(|settlements| {
        settlements
            .into_iter()
            .map(|settlement| match settlement.result {
                Some(result) => format!("{}:\n{}", settlement.text, result.model_text()),
                None => settlement.text,
            })
            .collect()
    })
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

// Catches: a stopped task regains wake authority after a later explicit refresh,
// or stop incorrectly disables new tasks and discards retained evidence.
#[test]
fn stopped_mcp_tasks_retain_results_without_rearming_on_a_new_turn() {
    let (ledger, _) = ledger_with(
        vec![
            Ok(McpTaskPoll::Working),
            Ok(McpTaskPoll::Completed(ToolResult::builtin("old-result"))),
        ],
        None,
    );
    ledger.stop_automatic_wakes(std::time::Instant::now() - Duration::from_secs(1));
    assert!(refresh(&ledger).expect("new explicit turn").is_empty());
    assert!(!poll_due(&ledger).expect("late old result"));
    let retained = refresh(&ledger).expect("inspect retained result");
    assert_eq!(retained.len(), 1);
    assert!(retained[0].contains("old-result"), "{retained:?}");
    let poller = Arc::new(Scripted {
        polls: Mutex::new(vec![Ok(McpTaskPoll::Completed(ToolResult::builtin(
            "new-result",
        )))]),
        seen: Mutex::default(),
    });
    ledger.started(ticket("task-2"), &start(&poller, None));
    assert!(poll_due(&ledger).expect("new admitted task can wake"));
    assert!(refresh(&ledger).expect("new result")[0].contains("new-result"));
}

// Catches: a pending answer blocks unrelated work, or an uncertain update
// is retried instead of producing one notice and ending the original task.
#[test]
fn an_independent_mcp_task_settles_during_a_form_and_an_uncertain_update_is_not_retried() {
    use ego_core::event::id::ItemId;
    use ego_core::question::{
        ElicitationKind, ElicitationMember, Elicitor, ElicitorError, QuestionAnswer,
        QuestionOutcome, QuestionRequest,
    };
    use futures::FutureExt;
    use serde_json::json;

    struct FormOwner(Mutex<Option<futures::channel::oneshot::Receiver<QuestionOutcome>>>);
    impl Elicitor for FormOwner {
        fn elicit<'a>(
            &'a self,
            _: ItemId,
            _: QuestionRequest,
        ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>> {
            let answer = self
                .0
                .lock()
                .unwrap()
                .take()
                .expect("form is presented once");
            async move { Ok(answer.await.expect("operator response")) }.boxed()
        }
    }

    let forms = McpTaskPoll::Forms([("original-request".into(), ElicitationMember {
        prompt: "Report name?".into(),
        kind: ElicitationKind::Form { schema: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}) },
    })].into());
    let (ledger, original) = ledger_with(vec![Ok(forms)], None);
    assert!(refresh(&ledger).unwrap().is_empty());
    let independent = Scripted::completing(ToolResult::builtin("independent result"));
    ledger.started(ticket("task-2"), &start(&independent, None));
    let (send, receive) = futures::channel::oneshot::channel();
    let owner = FormOwner(Mutex::new(Some(receive)));
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
    let mut pending = ledger.answer_pending(effects, &owner);
    assert!(
        pending.as_mut().now_or_never().is_none(),
        "still waiting for the operator"
    );
    let completed = refresh(&ledger).unwrap();
    assert_eq!(completed.len(), 1, "{completed:?}");
    assert!(completed[0].contains("independent result"), "{completed:?}");
    assert_eq!(
        independent.calls(),
        [(McpOperationKind::GetTask, json!({"taskId":"task-2"}))]
    );
    send.send(QuestionOutcome::Answered(QuestionAnswer::Form(
        json!({"name":"Luca"}).as_object().unwrap().clone(),
    )))
    .unwrap();
    assert!(block_on(pending).unwrap());
    let notices = refresh(&ledger).unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("may have reached the server"),
        "{notices:?}"
    );
    assert!(refresh(&ledger).unwrap().is_empty());
    assert_eq!(
        original.calls(),
        [
            (McpOperationKind::GetTask, json!({"taskId":"task-1"})),
            (
                McpOperationKind::UpdateTask,
                json!({"taskId":"task-1","inputResponses":{"original-request":{"action":"accept","content":{"name":"Luca"}}}})
            ),
            (McpOperationKind::CancelTask, json!({"taskId":"task-1"})),
        ]
    );
}

// Catches: a queued form from a stopped turn never opens or settles.
#[test]
fn stopping_a_turn_cancels_its_queued_mcp_task_form() {
    use ego_core::question::{ElicitationKind, ElicitationMember};
    let forms = McpTaskPoll::Forms([(
        "name".to_owned(),
        ElicitationMember {
            prompt: "Report name?".to_owned(),
            kind: ElicitationKind::Form {
                schema: serde_json::json!({"type":"object","properties":{"name":{"type":"string"}}}),
            },
        },
    )].into());
    let (ledger, poller) = ledger_with(vec![Ok(forms.clone()), Ok(forms)], None);
    assert!(refresh(&ledger).expect("queue form").is_empty());
    ledger.stop_automatic_wakes(std::time::Instant::now() - Duration::from_secs(1));
    let notices = refresh(&ledger).expect("settle stopped task");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("operator stopped the turn"),
        "{notices:?}"
    );
    assert_eq!(
        poller.kinds(),
        [
            McpOperationKind::GetTask,
            McpOperationKind::GetTask,
            McpOperationKind::CancelTask
        ]
    );
    assert_eq!(ledger.live(), 0);
}

// Catches (1070-56ed): a user stop revokes the wake of a task that an earlier,
// already completed turn admitted, so that turn's work never resumes the model.
#[test]
fn user_stop_keeps_the_wake_of_a_task_from_an_earlier_turn() {
    let (ledger, _) = ledger_with(
        vec![Ok(McpTaskPoll::Completed(ToolResult::builtin(
            "earlier-result",
        )))],
        None,
    );
    std::thread::sleep(Duration::from_millis(2));
    ledger.stop_automatic_wakes(std::time::Instant::now());
    assert!(
        poll_due(&ledger).expect("earlier task settles"),
        "the stop of a later turn revoked the wake of an earlier turn's task"
    );
}

// Catches (890-7df7): a result that an idle poll buffered survives the end of its
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

// Catches (890-7df7): a buffered result keeps the idle timer due "now", so when
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

// Catches completed result references being flattened into the ledger's text notice.
#[test]
fn mcp_images_ledger_refresh_keeps_the_completed_result_typed() {
    let result = ToolResult::builtin("typed receipt");
    let (ledger, _) = ledger_with(vec![Ok(McpTaskPoll::Completed(result.clone()))], None);
    let fs = ego_core::permit::fake::FakeFs::default();
    let (process, evidence) = (NoProcess, NoEvidence);
    let effects = ToolEffectPorts::new(
        &fs,
        &process,
        &evidence,
        Arc::new(NoResolver),
        Arc::new(NoHop),
        Arc::new(NoDuplex),
    );
    let settlements = block_on(ledger.refresh(effects)).unwrap();
    assert_eq!(settlements.len(), 1);
    assert!(settlements[0].text.contains("task-1"));
    assert_eq!(settlements[0].result.as_ref(), Some(&result));
}
