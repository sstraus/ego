//! RED wire contract for ACP's adaptation of the core RunController.
//!
//! This deliberately reuses the connected JSON-RPC line harness pattern from
//! `prompt.rs`. ACP owns only wire correlation. The real `Arc<RunController>`
//! owns store transactions, leases, status and interruption.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use agent_client_protocol::{ConnectTo, Lines};
use agent_client_protocol_schema::ProtocolVersion;
use futures::channel::mpsc;
use futures::future::{self, BoxFuture};
use futures::{SinkExt, StreamExt};

use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::run::{
    IdSource, RunAppend, RunController, RunExecutor, RunExecutorError,
    RunHandle, RunId, RunRetention, RunRevision, RunSettlement, RunStart, RunTransaction,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};

#[derive(Default)]
struct AcpStore {
    entries: Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>,
    sequence: Arc<Mutex<Vec<&'static str>>>,
}

impl AcpStore {
    fn with_sequence(sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            sequence,
        }
    }

    fn transactions(&self, id: SessionId) -> Vec<RunTransaction> {
        self.entries
            .lock()
            .expect("ACP session entries")
            .get(&id)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| match entry {
                        SessionEntry::RunTransaction(transaction) => Some(transaction.clone()),
                        SessionEntry::Event(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl SessionStore for AcpStore {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("ACP session entries")
            .entry(*id)
            .or_default()
            .push(SessionEntry::Event(event.clone()));
        Ok(())
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<futures::stream::BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let entries = self
            .entries
            .lock()
            .expect("ACP session entries")
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound)?;
        Ok(Box::pin(futures::stream::iter(
            entries.into_iter().map(Ok::<_, StoreError>),
        )))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(self
            .entries
            .lock()
            .expect("ACP session entries")
            .keys()
            .map(|id| SessionMeta {
                id: *id,
                workspace_root: PathBuf::from("/workspace"),
                model: None,
                created: Timestamp::EPOCH,
                updated: Timestamp::EPOCH,
                title: None,
                lineage: None,
            })
            .collect())
    }

    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        let mut entries = self.entries.lock().expect("ACP session entries");
        let actual = entries
            .get(session)
            .and_then(|entries| {
                entries.iter().rev().find_map(|entry| match entry {
                    SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                    SessionEntry::Event(_) => None,
                })
            });
        if expected_revision != actual {
            return match actual {
                Some(actual) => Ok(RunAppend::Conflict { actual }),
                None => Err(StoreError::NotFound),
            };
        }
        entries
            .entry(*session)
            .or_default()
            .push(SessionEntry::RunTransaction(transaction));
        self.sequence.lock().expect("ACP transaction sequence").push("commit");
        Ok(RunAppend::Committed {
            revision: actual.map_or_else(|| RunRevision::new(1), RunRevision::next),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecutorCall {
    Start(RunId),
    Recover(RunId),
    Submit(RunId),
    Interrupt(RunId),
}

struct AcpExecutor {
    calls: Mutex<Vec<ExecutorCall>>,
    settlements: Mutex<VecDeque<Result<ego_core::run::RunSettlement, RunExecutorError>>>,
    gate: Mutex<SubmitGate>,
    sequence: Arc<Mutex<Vec<&'static str>>>,
}

struct SubmitGate {
    released: bool,
    interrupted: bool,
    started: bool,
    waker: Option<Waker>,
    started_waker: Option<Waker>,
}

impl Default for SubmitGate {
    fn default() -> Self {
        Self {
            released: false,
            interrupted: false,
            started: false,
            waker: None,
            started_waker: None,
        }
    }
}

impl Default for AcpExecutor {
    fn default() -> Self {
        Self::with_sequence(Arc::new(Mutex::new(Vec::new())))
    }
}

impl AcpExecutor {
    fn with_sequence(sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            settlements: Mutex::new(VecDeque::new()),
            gate: Mutex::new(SubmitGate::default()),
            sequence,
        }
    }

    fn calls(&self) -> Vec<ExecutorCall> {
        self.calls.lock().expect("ACP executor calls").clone()
    }

    fn allow_submit(&self) {
        let waker = {
            let mut gate = self.gate.lock().expect("ACP submit gate");
            gate.released = true;
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    async fn wait_for_submit_start(&self) {
        futures::future::poll_fn(|cx| {
            let mut gate = self.gate.lock().expect("ACP submit gate");
            if gate.started {
                Poll::Ready(())
            } else {
                gate.started_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }

    fn settle_next(&self, settlement: RunSettlement) {
        self.settlements
            .lock()
            .expect("ACP executor settlements")
            .push_back(Ok(settlement));
        self.allow_submit();
    }

    fn any_submit_started(&self) -> bool {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .iter()
            .any(|call| matches!(call, ExecutorCall::Submit(_)))
    }
}

impl RunExecutor for AcpExecutor {
    fn start<'a>(
        &'a self,
        request: &'a RunStart,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Start(request.run_id));
        self.sequence.lock().expect("ACP executor sequence").push("start");
        Box::pin(future::ready(Ok(RunHandle::new(request.run_id))))
    }

    fn recover<'a>(
        &'a self,
        run: &'a RunId,
    ) -> BoxFuture<'a, Result<ego_core::run::RecoveryEvidence, RunExecutorError>> {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Recover(*run));
        Box::pin(future::ready(Ok(ego_core::run::RecoveryEvidence::Running)))
    }

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        _prompt: &'a str,
    ) -> BoxFuture<'a, Result<ego_core::run::RunSettlement, RunExecutorError>> {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Submit(handle.run_id()));
        self.sequence.lock().expect("ACP executor sequence").push("submit");
        let gate = &self.gate;
        let mut result = self
            .settlements
            .lock()
            .expect("ACP executor settlements")
            .pop_front();
        Box::pin(futures::future::poll_fn(move |cx| {
            let (started_waker, outcome) = {
                let mut gate = gate.lock().expect("ACP submit gate");
                gate.started = true;
                let started_waker = gate.started_waker.take();
                let outcome = if gate.interrupted {
                    Some(Ok(ego_core::run::RunSettlement::cancelled(
                        ego_core::run::CancelReason::User,
                    )))
                } else if gate.released {
                    Some(
                        result
                            .take()
                            .unwrap_or_else(|| Ok(ego_core::run::RunSettlement::completed())),
                    )
                } else {
                    gate.waker = Some(cx.waker().clone());
                    None
                };
                (started_waker, outcome)
            };
            if let Some(waker) = started_waker {
                waker.wake();
            }
            if let Some(outcome) = outcome {
                return Poll::Ready(outcome);
            }
            Poll::Pending
        }))
    }

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        self.calls
            .lock()
            .expect("ACP executor calls")
            .push(ExecutorCall::Interrupt(handle.run_id()));
        self.sequence.lock().expect("ACP executor sequence").push("interrupt");
        let waker = {
            let mut gate = self.gate.lock().expect("ACP submit gate");
            gate.interrupted = true;
            gate.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Box::pin(future::ready(Ok(())))
    }
}

struct AcpClock {
    now: Timestamp,
}

impl Default for AcpClock {
    fn default() -> Self {
        Self {
            now: Timestamp::EPOCH,
        }
    }
}

impl Clock for AcpClock {
    fn now(&self) -> Timestamp {
        self.now.clone()
    }
}

#[derive(Default)]
struct AcpIds {
    next: Mutex<u128>,
}

impl IdSource for AcpIds {
    fn next_uuid(&self) -> uuid::Uuid {
        let mut next = self.next.lock().expect("ACP ids");
        *next += 1;
        uuid::Uuid::from_u128(*next)
    }
}

type Controller = RunController<AcpStore, AcpExecutor, AcpClock, AcpIds>;

fn controller(store: Arc<AcpStore>, executor: Arc<AcpExecutor>) -> Arc<Controller> {
    Arc::new(RunController::new(
        store,
        executor,
        Arc::new(AcpClock::default()),
        Arc::new(AcpIds::default()),
        RunRetention::new(NonZeroUsize::new(4).expect("retention")).expect("retention"),
    ))
}

struct Connected {
    to_agent: mpsc::UnboundedSender<String>,
    from_agent: mpsc::UnboundedReceiver<String>,
    agent: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    next_id: u64,
}

impl Connected {
    fn with(controller: Arc<Controller>) -> Self {
        let (to_agent, to_agent_rx) = mpsc::unbounded::<String>();
        let (from_agent, from_agent_rx) = mpsc::unbounded::<String>();
        let transport = Lines::new(
            from_agent.sink_map_err(std::io::Error::other),
            to_agent_rx.map(Ok::<_, std::io::Error>),
        );
        Self {
            to_agent,
            from_agent: from_agent_rx,
            agent: tokio::spawn(ego_acp::serve::agent(controller).connect_to(transport)),
            next_id: 1,
        }
    }

    async fn send(&mut self, method: &str, params: serde_json::Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.to_agent
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                })
                .to_string(),
            )
            .await
            .expect("ACP agent listening");
        id
    }

    async fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.to_agent
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params,
                })
                .to_string(),
            )
            .await
            .expect("ACP agent listening");
    }

    async fn response_to(&mut self, id: u64) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut notifications = Vec::new();
        loop {
            let line = self.from_agent.next().await.expect("ACP response");
            let message: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC JSON");
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return (message, notifications);
            }
            notifications.push(message);
        }
    }

    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let id = self.send(method, params).await;
        self.response_to(id).await
    }

    async fn open(&mut self, cwd: &str) -> String {
        let (initialize, _) = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": ProtocolVersion::V1.as_u16(),
                    "clientCapabilities": {},
                }),
            )
            .await;
        assert!(initialize.get("result").is_some(), "initialize: {initialize}");
        let (opened, _) = self
            .request(
                "session/new",
                serde_json::json!({ "cwd": cwd, "mcpServers": [] }),
            )
            .await;
        opened["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string()
    }

    async fn finish(self) {
        self.to_agent.close_channel();
        let _ = self.agent.await;
    }
}

#[tokio::test]
async fn session_new_prompt_and_baseline_cancel_notification_drive_the_shared_controller() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let mut connection = Connected::with(Arc::clone(&controller));
    let session = connection.open("/workspace/project").await;
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "run" }],
            }),
        )
        .await;
    connection
        .notify("session/cancel", serde_json::json!({ "sessionId": session }))
        .await;
    let (response, _) = connection.response_to(prompt_id).await;
    assert_eq!(response["result"]["stopReason"], "cancelled");
    let id = session.parse::<SessionId>().expect("session identity");
    let run_id = store
        .transactions(id)
        .first()
        .expect("start transaction")
        .run_id();
    let run = controller.status(run_id).expect("run status");
    assert_eq!(run.status(), ego_core::run::RunStatus::Cancelled);
    assert_eq!(store.transactions(id).len(), 2);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        1
    );
    let sequence = sequence.lock().expect("ACP order").clone();
    assert_eq!(sequence.iter().filter(|step| **step == "commit").count(), 2);
    assert!(sequence.windows(2).any(|pair| pair == ["commit", "interrupt"]));
    connection.finish().await;
}

#[tokio::test]
async fn duplicate_cancel_replays_one_receipt_and_one_interrupt() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(AcpStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(AcpExecutor::with_sequence(Arc::clone(&sequence)));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let mut connection = Connected::with(Arc::clone(&controller));
    let session = connection.open("/workspace/project").await;
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({"sessionId": session, "prompt": [{"type":"text","text":"blocked"}]}),
        )
        .await;
    let cancel = serde_json::json!({ "sessionId": session });
    connection.notify("session/cancel", cancel.clone()).await;
    connection.notify("session/cancel", cancel).await;
    let (response, _) = connection.response_to(prompt_id).await;
    assert_eq!(response["result"]["stopReason"], "cancelled");
    let id = session.parse::<SessionId>().expect("session identity");
    assert_eq!(store.transactions(id).len(), 2);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, ExecutorCall::Interrupt(_)))
            .count(),
        1
    );
    let sequence = sequence.lock().expect("ACP order").clone();
    assert_eq!(sequence.iter().filter(|step| **step == "commit").count(), 2);
    assert!(sequence.windows(2).any(|pair| pair == ["commit", "interrupt"]));
    connection.finish().await;
}

#[tokio::test]
async fn noninteractive_permission_failure_returns_a_wire_error_without_waiting_for_a_human() {
    let store = Arc::new(AcpStore::default());
    let executor = Arc::new(AcpExecutor::default());
    executor.settle_next(RunSettlement::permission_required(
        "foreign network access",
        ego_core::run::DeniedBy::Nobody,
    ));
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let mut connection = Connected::with(controller);
    let session = connection.open("/workspace/project").await;
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "need access" }],
            }),
        )
        .await;
    let (response, _) = connection.response_to(prompt_id).await;
    let message = response["error"]["message"]
        .as_str()
        .expect("permission error message");
    assert_eq!(
        message,
        "the turn stopped because foreign network access needed a permission nobody could grant"
    );
    let id = session.parse::<SessionId>().expect("session identity");
    assert_eq!(store.transactions(id).len(), 2);
    assert!(executor.any_submit_started());
    assert!(!executor.calls().iter().any(|call| matches!(call, ExecutorCall::Interrupt(_))));
    connection.finish().await;
}

#[tokio::test]
async fn immediate_cancel_is_admitted_before_effect_and_does_not_retarget_a_later_turn() {
    let store = Arc::new(AcpStore::default());
    let executor = Arc::new(AcpExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let mut connection = Connected::with(Arc::clone(&controller));
    let session = connection.open("/workspace/project").await;
    connection
        .notify("session/cancel", serde_json::json!({ "sessionId": session }))
        .await;
    let id = session.parse::<SessionId>().expect("session identity");
    assert!(store.transactions(id).is_empty());
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "run" }],
            }),
        )
        .await;
    executor.wait_for_submit_start().await;
    assert!(executor.calls().iter().any(|call| matches!(call, ExecutorCall::Submit(_))));
    assert!(!executor.calls().iter().any(|call| matches!(call, ExecutorCall::Interrupt(_))));
    executor.allow_submit();
    let (response, _) = connection.response_to(prompt_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert_eq!(store.transactions(id).len(), 2);
    assert!(!executor.calls().iter().any(|call| matches!(call, ExecutorCall::Interrupt(_))));
    connection.finish().await;
}

#[tokio::test]
async fn unknown_and_between_turn_cancel_notifications_have_no_store_or_executor_effect() {
    let store = Arc::new(AcpStore::default());
    let executor = Arc::new(AcpExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let mut connection = Connected::with(controller);
    let session = connection.open("/workspace/project").await;
    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": "00000000-0000-0000-0000-000000000099" }),
        )
        .await;
    connection
        .notify("session/cancel", serde_json::json!({ "sessionId": session }))
        .await;
    let id = session.parse::<SessionId>().expect("session identity");
    assert!(store.transactions(id).is_empty());
    assert!(executor.calls().is_empty());
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "later" }],
            }),
        )
        .await;
    executor.wait_for_submit_start().await;
    assert!(!executor.calls().iter().any(|call| matches!(call, ExecutorCall::Interrupt(_))));
    executor.allow_submit();
    let (response, _) = connection.response_to(prompt_id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert_eq!(store.transactions(id).len(), 2);
    assert!(!executor.calls().iter().any(|call| matches!(call, ExecutorCall::Interrupt(_))));
    connection.finish().await;
}

#[tokio::test]
async fn disconnect_detaches_wire_subscription_without_cancelling_the_durable_run() {
    let store = Arc::new(AcpStore::default());
    let executor = Arc::new(AcpExecutor::default());
    let controller = controller(Arc::clone(&store), Arc::clone(&executor));
    let mut connection = Connected::with(Arc::clone(&controller));
    let session = connection.open("/workspace/project").await;
    let prompt_id = connection
        .send(
            "session/prompt",
            serde_json::json!({"sessionId": session, "prompt": [{"type":"text","text":"run"}]}),
        )
        .await;
    assert!(prompt_id > 0);
    executor.wait_for_submit_start().await;
    assert!(executor.any_submit_started());
    connection.finish().await;
    let id = session.parse::<SessionId>().expect("session identity");
    let run_id = store
        .transactions(id)
        .first()
        .expect("run transaction after disconnect")
        .run_id();
    let run = controller.status(run_id).expect("run after disconnect");
    assert_eq!(run.status(), ego_core::run::RunStatus::Running);
    assert!(!executor.calls().iter().any(|call| matches!(call, ExecutorCall::Interrupt(_))));
}
