//! router contract for regression case's durable ACP v1 session surface.
//!
//! Every request crosses the pinned SDK's real JSON-RPC router. The fixture is
//! deliberately restartable: separate routers share only the durable store,
//! never a connection-local session map.

use std::future::Future;
use std::time::Duration;

use agent_client_protocol::{ConnectTo, ErrorCode, Lines};
use agent_client_protocol_schema::ProtocolVersion;
use ego_acp::project::{AcpNotice, NoticeAction};
use ego_acp::serve::AcpNotices;
use ego_core::run::RunStatus;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};

mod support;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/regression case");
const OTHER_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/regression case");
const WRONG_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/regression case");
#[cfg(not(windows))]
const MCP_COMMAND: &str = "/bin/true";
#[cfg(windows)]
const MCP_COMMAND: &str = r"C:\Windows\System32\cmd.exe";
const regression_case_PHASE_TIMEOUT: Duration = Duration::from_secs(1);

async fn regression_case_phase<T>(name: &str, operation: impl Future<Output = T>) -> T {
    tokio::time::timeout(regression_case_PHASE_TIMEOUT, operation)
        .await
        .unwrap_or_else(|_| panic!("regression case phase timed out: {name}"))
}

struct Connected {
    to_agent: mpsc::UnboundedSender<String>,
    from_agent: mpsc::UnboundedReceiver<String>,
    agent: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    next_id: u64,
}

impl Connected {
    fn new(router: agent_client_protocol::AgentProtocolRouter) -> Self {
        let (to_agent, to_agent_rx) = mpsc::unbounded::<String>();
        let (from_agent, from_agent_rx) = mpsc::unbounded::<String>();
        let transport = Lines::new(
            from_agent.sink_map_err(std::io::Error::other),
            to_agent_rx.map(Ok::<_, std::io::Error>),
        );
        Self {
            to_agent,
            from_agent: from_agent_rx,
            agent: tokio::spawn(router.connect_to(transport)),
            next_id: 1,
        }
    }

    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let id = self.send_request(method, params).await;
        self.response(id).await
    }

    async fn send_request(&mut self, method: &str, params: serde_json::Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send_request_with_id(id, method, params).await;
        id
    }

    async fn send_request_with_id(&mut self, id: u64, method: &str, params: serde_json::Value) {
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
            .expect("the durable ACP agent is listening");
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
            .expect("the durable ACP agent is listening");
    }

    async fn response(&mut self, id: u64) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut notifications = Vec::new();
        loop {
            let line = self
                .from_agent
                .next()
                .await
                .expect("every lifecycle request is answered");
            let message: serde_json::Value =
                serde_json::from_str(&line).expect("the agent writes JSON-RPC");
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return (message, notifications);
            }
            notifications.push(message);
        }
    }

    /// The notifications the agent wrote *after* answering the last request.
    ///
    /// Every session notification now follows the response that names its
    /// session, so a test cannot collect them by reading up to that response.
    /// Reading "until the stream goes quiet" would make the assertions timing
    /// dependent; instead this sends a second request and reads up to *its*
    /// answer. The agent writes a handler's notifications before it takes the
    /// next request off the wire, so everything ahead of the fence belongs to
    /// the operation under test, deterministically.
    ///
    /// `session/list` is the fence because it is read-only and emits nothing of
    /// its own.
    async fn announced(&mut self) -> Vec<serde_json::Value> {
        let fence = self
            .send_request("session/list", serde_json::json!({ "cwd": ROOT }))
            .await;
        self.response(fence).await.1
    }

    async fn initialize(&mut self) -> serde_json::Value {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": ProtocolVersion::V1.as_u16(),
                "clientCapabilities": {},
            }),
        )
        .await
        .0
    }

    async fn new_session(&mut self, root: &str) -> (String, Vec<serde_json::Value>) {
        let (response, before) = self
            .request(
                "session/new",
                serde_json::json!({
                    "cwd": root,
                    "mcpServers": [],
                }),
            )
            .await;
        assert!(
            before.is_empty(),
            "session/new spoke about a session before the response that names it: {before:?}"
        );
        let id = response["result"]["sessionId"]
            .as_str()
            .expect("session/new returns a session id")
            .to_owned();
        let announced = self.announced().await;
        (id, announced)
    }

    async fn finish(self) {
        self.to_agent.close_channel();
        self.agent
            .await
            .expect("the router task joins")
            .expect("the in-memory connection closes cleanly");
    }

    async fn abort(self) {
        self.to_agent.close_channel();
        self.agent.abort();
        let _ = self.agent.await;
    }
}

#[tokio::test]
async fn notice_published_between_prompts_reaches_the_session_as_an_acp_update() {
    let notices = AcpNotices::default();
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router_with_notices(notices.clone()));
    assert!(connection.initialize().await["result"].is_object());
    let (session, _) = connection.new_session(ROOT).await;
    let session_id = session.clone().into();
    let card = AcpNotice {
        id: "result-42".to_owned(),
        text: "Worker finished: RESULT".to_owned(),
        action: NoticeAction::OpenResult {
            path: "/workspace/results/worker.md".into(),
        },
    };
    let unknown = notices
        .publish(&"unknown-session".into(), &card)
        .expect_err("a notice must not be sent to a different session");
    assert_eq!(unknown.code, ErrorCode::ResourceNotFound);

    notices
        .publish(&session_id, &card)
        .expect("the active ACP host can receive a notice without a prompt");

    let updates = connection.announced().await;
    assert_eq!(updates.len(), 1, "exactly one notice: {updates:?}");
    let notice = &updates[0];
    assert_eq!(notice["method"], "session/update");
    assert_eq!(notice["params"]["sessionId"], session);
    assert_eq!(
        notice["params"]["update"]["sessionUpdate"],
        "agent_message_chunk"
    );
    assert_eq!(
        notice["params"]["update"]["content"]["text"],
        "Worker finished: RESULT"
    );
    assert_eq!(
        notice["params"]["update"]["_meta"]["ego"]["salience"],
        "card"
    );
    connection.finish().await;
    let closed = notices
        .publish(&session_id, &card)
        .expect_err("a closed host must not be told a later notice was delivered");
    assert_eq!(closed.code, ErrorCode::ResourceNotFound);
    // Catches: accepted cards have no durable timestamps, or failed sends are recorded as delivery.
    use ego_core::store::session_store::{SessionEntry, SessionStore};
    use futures::StreamExt;
    let entries = fixture
        .store
        .read(&session.parse().unwrap())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let deliveries = entries
        .into_iter()
        .filter_map(|entry| match entry.unwrap() {
            SessionEntry::NoticeDelivered(notice) => Some(notice),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].card_id, "result-42");
    assert_eq!(
        deliveries[0].source_path.as_deref(),
        Some(std::path::Path::new("/workspace/results/worker.md"))
    );
    assert!(deliveries[0].delivered_at.as_millis() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_notices_each_reach_the_idle_session_once() {
    let notices = AcpNotices::default();
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router_with_notices(notices.clone()));
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let mut sends = Vec::new();
    for id in ["notice-a", "notice-b"] {
        let notices = notices.clone();
        let session = session.clone().into();
        let barrier = barrier.clone();
        sends.push(tokio::spawn(async move {
            barrier.wait().await;
            notices.publish(
                &session,
                &AcpNotice {
                    id: id.to_owned(),
                    text: id.to_owned(),
                    action: NoticeAction::Answer {
                        question_id: id.to_owned(),
                    },
                },
            )
        }));
    }
    barrier.wait().await;
    for send in sends {
        send.await.unwrap().expect("both notices can be queued");
    }
    let updates = connection.announced().await;
    let mut ids = updates
        .iter()
        .map(|update| update["params"]["update"]["messageId"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, ["notice-a", "notice-b"]);
    connection.finish().await;
}

#[tokio::test]
async fn old_host_closure_does_not_remove_a_resumed_hosts_notice_route() {
    let notices = AcpNotices::default();
    let fixture = support::DurableAcpFixture::new();
    let mut first = Connected::new(fixture.router_with_notices(notices.clone()));
    first.initialize().await;
    let (session, _) = first.new_session(ROOT).await;

    let mut resumed = Connected::new(fixture.router_with_notices(notices.clone()));
    resumed.initialize().await;
    let (answer, _) = resumed
        .request(
            "session/resume",
            serde_json::json!({
                "sessionId": session,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(answer.get("result").is_some(), "resume failed: {answer}");
    resumed.announced().await;
    first.finish().await;

    notices
        .publish(
            &session.clone().into(),
            &AcpNotice {
                id: "resumed-result".to_owned(),
                text: "Result after reconnect".to_owned(),
                action: NoticeAction::OpenResult {
                    path: "/workspace/results/resumed.md".into(),
                },
            },
        )
        .expect("the resumed host still owns the notice route");
    let updates = resumed.announced().await;
    assert_eq!(updates.len(), 1, "exactly one resumed notice: {updates:?}");
    assert_eq!(
        updates[0]["params"]["update"]["messageId"],
        "resumed-result"
    );
    resumed.finish().await;
}

fn updates(messages: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    messages
        .iter()
        .filter(|message| message["method"] == "session/update")
        .map(|message| &message["params"])
        .collect()
}

fn assert_info_update(messages: &[serde_json::Value], session: &str, title: Option<&str>) {
    let updates = updates(messages);
    let info = updates
        .iter()
        .find(|params| params["update"]["sessionUpdate"] == "session_info_update")
        .unwrap_or_else(|| panic!("no current SessionInfoUpdate was emitted: {messages:?}"));
    assert_eq!(info["sessionId"], session);
    assert_eq!(info["update"]["title"], serde_json::json!(title));
}

#[tokio::test]
async fn initialize_advertises_exactly_the_composed_v1_lifecycle_surface() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router());
    let response = connection.initialize().await;
    let capabilities = &response["result"]["agentCapabilities"];

    assert_eq!(capabilities["loadSession"], true);
    assert_eq!(
        capabilities["sessionCapabilities"]["delete"]["_meta"]["ego"]["force"],
        true
    );
    for method in ["list", "delete", "fork", "resume", "close"] {
        assert!(
            capabilities["sessionCapabilities"][method].is_object(),
            "{method} is served but not advertised: {response}"
        );
    }
    assert!(
        capabilities["sessionCapabilities"]
            .get("additionalDirectories")
            .is_none(),
        "unsupported multi-root authority was advertised: {response}"
    );
    assert_eq!(
        capabilities["promptCapabilities"]["image"], true,
        "image prompts are served but not advertised: {response}"
    );
    for absent in ["audio", "embeddedContext"] {
        assert_ne!(
            capabilities["promptCapabilities"][absent], true,
            "unsupported prompt capability {absent} was advertised"
        );
    }

    connection.finish().await;
}

/// Renamed from `..._before_answering`, which asserted the inversion this story
/// fixes. The durability half is unchanged; only the ordering claim moved, and
/// `new_session` now fails if anything at all precedes the response.
#[tokio::test]
async fn new_is_durable_and_announces_the_session_after_naming_it() {
    let fixture = support::DurableAcpFixture::new();
    let mut first = Connected::new(fixture.router());
    first.initialize().await;
    let (session, notifications) = first.new_session(ROOT).await;
    assert_info_update(&notifications, &session, None);
    first.finish().await;

    let mut reconnected = Connected::new(fixture.router());
    reconnected.initialize().await;
    let (listed, _) = reconnected
        .request("session/list", serde_json::json!({ "cwd": ROOT }))
        .await;
    assert!(
        listed["result"]["sessions"]
            .as_array()
            .expect("session/list returns rows")
            .iter()
            .any(|row| row["sessionId"] == session),
        "the newly created session disappeared with its connection: {listed}"
    );
    reconnected.finish().await;
}

/// A listed session states when it was last active, in the spelling the field
/// is defined to carry.
///
/// `session/list` promises no order, so `updated_at` is the only thing a host
/// can rank a session picker by. The fixture's clock is pinned to the epoch,
/// which is what makes this assertion exact: the value is read from the
/// session's own last observation time and not from the wall clock of whoever
/// happened to ask.
#[tokio::test]
async fn a_listed_session_states_its_last_activity_as_rfc_3339() {
    const EPOCH: &str = "1970-01-01T00:00:00.000Z";
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (session, notifications) = connection.new_session(ROOT).await;

    let updates = updates(&notifications);
    let info = updates
        .iter()
        .find(|params| params["update"]["sessionUpdate"] == "session_info_update")
        .unwrap_or_else(|| panic!("no SessionInfoUpdate was emitted: {notifications:?}"));
    assert_eq!(
        info["update"]["updatedAt"],
        serde_json::json!(EPOCH),
        "the notification dated the session from nowhere: {info}"
    );

    let (listed, _) = connection
        .request("session/list", serde_json::json!({ "cwd": ROOT }))
        .await;
    let row = listed["result"]["sessions"]
        .as_array()
        .expect("session/list returns rows")
        .iter()
        .find(|row| row["sessionId"] == session.as_str())
        .unwrap_or_else(|| panic!("the session it just minted was not listed: {listed}"));
    assert_eq!(
        row["updatedAt"],
        serde_json::json!(EPOCH),
        "a listed session carried no last-activity time, so no host can order the list: {row}"
    );

    connection.finish().await;
}

#[tokio::test]
async fn reconnecting_a_header_only_session_starts_its_first_run_under_the_same_id() {
    let fixture = support::DurableAcpFixture::new();
    let mut first = Connected::new(fixture.router());
    first.initialize().await;
    let (session, _) = first.new_session(ROOT).await;
    first.finish().await;

    let mut reconnected = Connected::new(fixture.router());
    reconnected.initialize().await;
    let (resumed, _) = reconnected
        .request(
            "session/resume",
            serde_json::json!({
                "sessionId": session.clone(),
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(resumed.get("result").is_some(), "resume failed: {resumed}");
    let (prompted, _) = reconnected
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "first durable turn" }],
            }),
        )
        .await;
    assert_eq!(prompted["result"]["stopReason"], "end_turn");
    assert!(
        fixture.has_run(&session),
        "the first run was not appended under the durable session id"
    );
    reconnected.finish().await;
}

#[tokio::test]
async fn durable_new_prompt_and_cancel_retain_the_active_cancel_authority() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    fixture.block_next_turn();
    let prompt = connection
        .send_request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "stop this durable turn" }],
            }),
        )
        .await;
    fixture.turn_began().await;
    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session.clone() }),
        )
        .await;
    let _ = connection
        .request("session/list", serde_json::json!({ "cwd": ROOT }))
        .await;
    fixture.release_turn();
    let (answered, _) = connection.response(prompt).await;
    assert_eq!(answered["result"]["stopReason"], "cancelled");
    connection.finish().await;
}

#[tokio::test]
async fn load_and_resume_refuse_to_replace_a_same_connection_active_turn() {
    for method in ["session/load", "session/resume"] {
        let fixture = support::DurableAcpFixture::new();
        let mut connection = Connected::new(fixture.router());
        connection.initialize().await;
        let (session, _) = connection.new_session(ROOT).await;
        fixture.block_next_turn();
        let prompt = connection
            .send_request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session.clone(),
                    "prompt": [{ "type": "text", "text": "keep authority" }],
                }),
            )
            .await;
        fixture.turn_began().await;
        let effects_before_refusal = fixture.effect_counts();

        let (reattached, _) = connection
            .request(
                method,
                serde_json::json!({
                    "sessionId": session.clone(),
                    "cwd": ROOT,
                    "mcpServers": [],
                }),
            )
            .await;
        assert!(
            reattached.get("error").is_some(),
            "{method} replaced the active registration: {reattached}"
        );
        assert_eq!(
            fixture.effect_counts(),
            effects_before_refusal,
            "{method} resolved roots, admitted MCP, or mutated lifecycle before refusing"
        );
        connection
            .notify(
                "session/cancel",
                serde_json::json!({ "sessionId": session.clone() }),
            )
            .await;
        let _ = connection
            .request("session/list", serde_json::json!({ "cwd": ROOT }))
            .await;
        fixture.release_turn();
        let (answered, _) = connection.response(prompt).await;
        assert_eq!(answered["result"]["stopReason"], "cancelled");
        connection.finish().await;
    }
}

#[tokio::test]
async fn fresh_controller_load_recovers_a_durable_running_outcome() {
    let fixture = support::DurableAcpFixture::new();
    let session = fixture.seed_running_session(ROOT);
    assert_eq!(fixture.run_status(&session), RunStatus::Running);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    let (loaded, _) = connection
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": session.clone(),
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(
        loaded.get("result").is_some(),
        "recovery load failed: {loaded}"
    );
    assert_eq!(
        fixture.run_status(&session),
        RunStatus::Uncertain,
        "the fresh controller left a stale Running run instead of recording its ambiguous recovery outcome"
    );
    connection.finish().await;
}

async fn recover_cancel_and_continue(
    fixture: &support::DurableAcpFixture,
    connection: &mut Connected,
    session: &str,
    method: &str,
) {
    let (reattached, _) = connection
        .request(
            method,
            serde_json::json!({
                "sessionId": session,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(
        reattached.get("result").is_some(),
        "{method} did not reattach the durable run: {reattached}"
    );
    fixture.recovery_observe_began().await;

    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session }),
        )
        .await;
    let (listed, _) = connection
        .request("session/list", serde_json::json!({ "cwd": ROOT }))
        .await;
    assert!(
        listed.get("result").is_some(),
        "session/list failed: {listed}"
    );
    assert_eq!(
        fixture.run_status(session),
        RunStatus::Cancelling,
        "cancel did not retain the recovered run's durable cancellation state"
    );

    fixture.release_recovery_observe();
    fixture
        .wait_for_run_status(session, RunStatus::Cancelled)
        .await;

    let (prompted, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "continue after recovery" }],
            }),
        )
        .await;
    assert_eq!(
        prompted["result"]["stopReason"], "end_turn",
        "settled recovery did not release the session registry: {prompted}"
    );
    assert_eq!(fixture.run_status(session), RunStatus::Settled);
}

#[tokio::test]
async fn fresh_controller_load_reattaches_running_and_releases_registry_after_cancel() {
    let fixture = support::DurableAcpFixture::new();
    let session = fixture.seed_running_session(ROOT);
    fixture.enable_running_recovery();
    fixture.block_recovery_observe();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    recover_cancel_and_continue(&fixture, &mut connection, &session, "session/load").await;
    connection.finish().await;
}

#[tokio::test]
async fn fresh_controller_resume_reattaches_cancelling_and_releases_registry_after_cancel() {
    let fixture = support::DurableAcpFixture::new();
    let session = fixture.seed_cancelling_session(ROOT);
    fixture.enable_running_recovery();
    fixture.block_recovery_observe();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    recover_cancel_and_continue(&fixture, &mut connection, &session, "session/resume").await;
    connection.finish().await;
}

#[tokio::test]
async fn regression_case_delayed_recovery_watcher_cannot_finish_a_newer_prompt() {
    let fixture = support::DurableAcpFixture::new();
    let session = fixture.seed_running_session(ROOT);
    fixture.enable_running_recovery();
    fixture.park_recovery_watcher();
    let mut connection = Connected::new(fixture.router());
    regression_case_phase("initialize", connection.initialize()).await;

    let (loaded, _) = regression_case_phase(
        "load running W1",
        connection.request(
            "session/load",
            serde_json::json!({
                "sessionId": session.clone(),
                "cwd": ROOT,
                "mcpServers": [],
            }),
        ),
    )
    .await;
    assert!(loaded.get("result").is_some(), "W1 load failed: {loaded}");
    regression_case_phase(
        "park recovery W1 watcher",
        fixture.recovery_watcher_parked(),
    )
    .await;
    regression_case_phase(
        "settle recovered W1 execution",
        fixture.wait_for_run_status(&session, RunStatus::Settled),
    )
    .await;

    let (closed, _) = regression_case_phase(
        "close and remove W1",
        connection.request(
            "session/close",
            serde_json::json!({ "sessionId": session.clone() }),
        ),
    )
    .await;
    assert!(closed.get("result").is_some(), "close failed: {closed}");

    let (reattached, _) = regression_case_phase(
        "reattach terminal W1",
        connection.request(
            "session/load",
            serde_json::json!({
                "sessionId": session.clone(),
                "cwd": ROOT,
                "mcpServers": [],
            }),
        ),
    )
    .await;
    assert!(
        reattached.get("result").is_some(),
        "terminal W1 reattach failed: {reattached}"
    );
    assert_eq!(fixture.run_status(&session), RunStatus::Settled);

    // `session/prompt` installs its registration and CancelKey before this
    // third execution spawn (serve.rs:1381-1407). Since regression case the
    // continuation reattaches and settles on its own, so W2 is held inside
    // submit: the delayed W1 watcher must land while W2 is genuinely running.
    fixture.block_next_turn();
    let _prompt = regression_case_phase(
        "send newer W2 prompt",
        connection.send_request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "new W2" }],
            }),
        ),
    )
    .await;
    regression_case_phase("spawn newer W2 execution", fixture.w2_execution_spawned()).await;
    regression_case_phase("hold newer W2 inside submit", fixture.turn_began()).await;

    fixture.release_recovery_watcher();
    regression_case_phase(
        "finish delayed W1 watcher",
        fixture.recovery_watcher_finished(),
    )
    .await;

    let (second, _) = regression_case_phase(
        "refuse second prompt while W2 is busy",
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "must wait for W2" }],
            }),
        ),
    )
    .await;
    let second_error = second
        .get("error")
        .unwrap_or_else(|| panic!("a delayed W1 watcher admitted a second prompt: {second}"));

    regression_case_phase(
        "cancel newer W2 prompt",
        connection.notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session.clone() }),
        ),
    )
    .await;
    let _ = regression_case_phase(
        "observe cancellation notification barrier",
        connection.request("session/list", serde_json::json!({ "cwd": ROOT })),
    )
    .await;
    let durable_status = fixture.run_status(&session);
    regression_case_phase("abort ACP connection cleanup", connection.abort()).await;

    assert_eq!(
        second_error["code"], -32600,
        "stale W1 completion cleared W2 busy/CancelKey: {second}"
    );
    assert!(
        second_error["data"]
            .as_str()
            .is_some_and(|data| data.contains("already has a turn running")),
        "second prompt was not rejected as busy: {second}"
    );
    assert_eq!(
        durable_status,
        RunStatus::Cancelling,
        "stale W1 completion removed W2 cancellation authority"
    );
}

#[tokio::test]
async fn list_uses_restart_safe_cursor_pagination_and_root_filtering() {
    let fixture = support::DurableAcpFixture::new();
    fixture.seed_sessions(ROOT, support::SESSION_PAGE_SIZE + 1);
    fixture.seed_sessions(OTHER_ROOT, 1);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    let (first, _) = connection
        .request("session/list", serde_json::json!({ "cwd": ROOT }))
        .await;
    let first_rows = first["result"]["sessions"]
        .as_array()
        .expect("first list page");
    assert_eq!(first_rows.len(), support::SESSION_PAGE_SIZE);
    assert!(first_rows.iter().all(|row| row["cwd"] == ROOT));
    let cursor = first["result"]["nextCursor"]
        .as_str()
        .expect("a full page has a cursor")
        .to_owned();

    connection.finish().await;
    let mut reconnected = Connected::new(fixture.router());
    reconnected.initialize().await;
    let (second, _) = reconnected
        .request(
            "session/list",
            serde_json::json!({ "cwd": ROOT, "cursor": cursor.clone() }),
        )
        .await;
    assert_eq!(
        second["result"]["sessions"]
            .as_array()
            .expect("second list page")
            .len(),
        1
    );
    assert!(second["result"].get("nextCursor").is_none());

    let (wrong_filter, _) = reconnected
        .request(
            "session/list",
            serde_json::json!({
                "cwd": OTHER_ROOT,
                "cursor": cursor,
            }),
        )
        .await;
    assert!(
        wrong_filter.get("error").is_some(),
        "a cursor was accepted under another authority filter: {wrong_filter}"
    );
    reconnected.finish().await;
}

#[tokio::test]
async fn load_replays_projectable_history_then_one_final_usage_and_current_info() {
    let fixture = support::DurableAcpFixture::new();
    let seeded = fixture.seed_replay_session(ROOT, "Durable title", 200_000);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    let (loaded, before) = connection
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": seeded.session_id,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(loaded.get("result").is_some(), "load failed: {loaded}");
    assert!(
        before.is_empty(),
        "load replayed history before answering the request that names the session: {before:?}"
    );
    let notifications = connection.announced().await;
    let actual = updates(&notifications);
    assert_eq!(
        actual
            .iter()
            .filter(|params| params["update"]["sessionUpdate"] == "usage_update")
            .count(),
        1,
        "load must reduce historical usage to one final state: {notifications:?}"
    );
    assert_eq!(
        actual
            .iter()
            .filter(|params| params["update"]["sessionUpdate"] != "session_info_update")
            .map(|params| params["update"].clone())
            .collect::<Vec<_>>(),
        seeded.expected_replay_updates,
        "load did not preserve the durable public projection order"
    );
    assert_info_update(&notifications, &seeded.session_id, Some("Durable title"));
    connection.finish().await;
}

#[tokio::test]
async fn load_replays_tool_presentation_and_failure_over_real_acp() {
    let fixture = support::DurableAcpFixture::new();
    let session_id = fixture.seed_tool_replay_session(ROOT);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (loaded, before) = connection
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": session_id,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(loaded.get("result").is_some(), "load failed: {loaded}");
    assert!(before.is_empty(), "history preceded the load response");
    let notifications = connection.announced().await;
    let calls = updates(&notifications)
        .iter()
        .map(|params| &params["update"])
        .filter(|update| {
            matches!(
                update["sessionUpdate"].as_str(),
                Some("tool_call" | "tool_call_update")
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        3,
        "expected start, progress and failure: {calls:?}"
    );
    assert_eq!(calls[0]["title"], "read src/main.rs");
    assert_eq!(calls[0]["kind"], "read");
    assert_eq!(
        calls[0]["locations"][0]["path"],
        format!("{ROOT}/src/main.rs")
    );
    assert_eq!(calls[1]["status"], "in_progress");
    assert_eq!(calls[2]["status"], "failed");
    assert_eq!(calls[0]["toolCallId"], calls[1]["toolCallId"]);
    assert_eq!(calls[1]["toolCallId"], calls[2]["toolCallId"]);
    connection.finish().await;
}

#[tokio::test]
async fn resume_emits_current_info_but_no_history_or_usage() {
    let fixture = support::DurableAcpFixture::new();
    let seeded = fixture.seed_replay_session(ROOT, "Resume me", 200_000);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    let (resumed, before) = connection
        .request(
            "session/resume",
            serde_json::json!({
                "sessionId": seeded.session_id,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(resumed.get("result").is_some(), "resume failed: {resumed}");
    assert!(
        before.is_empty(),
        "resume announced the session before answering: {before:?}"
    );
    let notifications = connection.announced().await;
    assert_eq!(
        updates(&notifications).len(),
        1,
        "resume replayed durable history: {notifications:?}"
    );
    assert_info_update(&notifications, &seeded.session_id, Some("Resume me"));
    connection.finish().await;
}

#[tokio::test]
async fn fork_uses_durable_lineage_and_attaches_only_the_child() {
    let fixture = support::DurableAcpFixture::new();
    let source = fixture.seed_replay_session(ROOT, "Source title", 200_000);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    let (forked, before) = connection
        .request(
            "session/fork",
            serde_json::json!({
                "sessionId": source.session_id,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    let child = forked["result"]["sessionId"]
        .as_str()
        .expect("fork returns the child id")
        .to_owned();
    assert_ne!(child, source.session_id);
    // The child's id exists nowhere but this response, so an announcement
    // ahead of it names a session the client has never heard of.
    assert!(
        before.is_empty(),
        "fork announced the child before naming it: {before:?}"
    );
    let notifications = connection.announced().await;
    assert_info_update(&notifications, &child, Some("Source title"));
    assert_eq!(
        fixture.lineage_source(&child).as_deref(),
        Some(source.session_id.as_str())
    );
    connection.finish().await;
}

// Catches: ignored fork metadata, lost inherited IDs, stale/deleted parent lookup,
// and inherited replay ordered after the child's own messages.
#[tokio::test]
async fn message_fork_replays_the_cutoff_before_local_history_even_after_parent_deletion() {
    let fixture = support::DurableAcpFixture::new();
    let (source, items) = fixture.seed_settled_messages(ROOT);
    let mut connection = Connected::new(fixture.router());
    let initialized = connection.initialize().await;
    assert_eq!(
        initialized["result"]["agentCapabilities"]["sessionCapabilities"]["fork"]["_meta"]["ego"]["atMessage"],
        true
    );
    let (forked, before) = connection
        .request(
            "session/fork",
            serde_json::json!({
                "sessionId": source, "cwd": ROOT, "mcpServers": [],
                "_meta": {"ego": {"atMessageId": items[0]}}
            }),
        )
        .await;
    assert!(before.is_empty());
    let child = forked["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("fork failed: {forked}"))
        .to_owned();
    let announced = connection.announced().await;
    let lineage = &updates(&announced)[0]["update"]["_meta"]["ego"]["lineage"];
    assert_eq!(lineage["kind"], "fork");
    assert_eq!(lineage["sourceSessionId"], source);
    assert_eq!(lineage["rootSessionId"], source);
    assert_eq!(lineage["atMessageId"], serde_json::json!(items[0]));
    let own = fixture.append_settled_message(&child, "child turn");
    let (deleted, _) = connection
        .request("session/delete", serde_json::json!({"sessionId": source}))
        .await;
    assert!(deleted.get("result").is_some(), "delete failed: {deleted}");
    let (loaded, _) = connection
        .request(
            "session/load",
            serde_json::json!({"sessionId": child, "cwd": ROOT, "mcpServers": []}),
        )
        .await;
    assert!(loaded.get("result").is_some(), "load failed: {loaded}");
    let history = connection.announced().await;
    let frames = updates(&history);
    let messages = frames
        .iter()
        .filter(|frame| frame["update"]["sessionUpdate"] == "user_message_chunk")
        .collect::<Vec<_>>();
    assert_eq!(
        messages.len(),
        2,
        "cutoff omitted or tip inherited: {history:?}"
    );
    assert_eq!(
        messages[0]["update"]["messageId"],
        serde_json::json!(items[0])
    );
    assert_eq!(messages[0]["update"]["content"]["text"], "turn 1");
    assert_eq!(messages[0]["update"]["_meta"]["ego"]["inherited"], true);
    assert_eq!(messages[1]["update"]["messageId"], serde_json::json!(own));
    assert_eq!(messages[1]["update"]["content"]["text"], "child turn");
    assert!(
        messages[1]["update"]["_meta"]["ego"]
            .get("inherited")
            .is_none()
    );
    assert_eq!(
        frames.last().unwrap()["update"]["_meta"]["ego"]["lineage"]["sourceDeleted"],
        true
    );
    let (listed, _) = connection
        .request("session/list", serde_json::json!({"cwd": ROOT}))
        .await;
    let listed_child = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["sessionId"] == child)
        .unwrap();
    assert_eq!(
        listed_child["_meta"]["ego"]["lineage"]["sourceDeleted"],
        true
    );
    let (forked_again, _) = connection
        .request(
            "session/fork",
            serde_json::json!({"sessionId": child, "cwd": ROOT, "mcpServers": []}),
        )
        .await;
    assert!(
        forked_again.get("result").is_some(),
        "deleted ancestor blocked child fork: {forked_again}"
    );
    connection.announced().await;
    connection.finish().await;
}

// Catches: unknown or foreign IDs silently turning into a successful tip fork.
#[tokio::test]
async fn invalid_message_forks_publish_no_child() {
    let fixture = support::DurableAcpFixture::new();
    let (source, _) = fixture.seed_settled_messages(ROOT);
    let (_, foreign) = fixture.seed_settled_messages(ROOT);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    for message in [
        serde_json::json!(ego_core::event::id::ItemId::new()),
        serde_json::json!(foreign[0]),
        serde_json::Value::Null,
        serde_json::json!("bad-id"),
    ] {
        let before = fixture.effect_counts().2;
        let (response, notifications) = connection.request("session/fork", serde_json::json!({
            "sessionId": source, "cwd": ROOT, "mcpServers": [], "_meta": {"ego": {"atMessageId": message}}
        })).await;
        assert!(
            response.get("error").is_some(),
            "invalid message accepted: {response}"
        );
        assert!(notifications.is_empty());
        assert_eq!(
            fixture.effect_counts().2,
            before,
            "invalid fork published a child"
        );
    }
    connection.finish().await;
}

// Catches: compaction edges disappearing or incorrectly terminating the tree root.
#[tokio::test]
async fn compaction_children_project_lineage_in_list_and_load_info() {
    let fixture = support::DurableAcpFixture::new();
    let (source, _) = fixture.seed_settled_messages(ROOT);
    let child = fixture.seed_compaction_child(&source);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (listed, _) = connection
        .request("session/list", serde_json::json!({"cwd": ROOT}))
        .await;
    let item = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["sessionId"] == child)
        .unwrap();
    let expected = serde_json::json!({"kind": "compaction", "sourceSessionId": source, "rootSessionId": source, "sourceDeleted": false});
    assert_eq!(item["_meta"]["ego"]["lineage"], expected);
    let (loaded, _) = connection
        .request(
            "session/load",
            serde_json::json!({"sessionId": child, "cwd": ROOT, "mcpServers": []}),
        )
        .await;
    assert!(loaded.get("result").is_some(), "load failed: {loaded}");
    let announced = connection.announced().await;
    assert_eq!(
        updates(&announced).last().unwrap()["update"]["_meta"]["ego"]["lineage"],
        expected
    );
    connection.finish().await;
}

// Catches: a durable goal set outside the current connection never reaching
// the host's plan card on attachment.
#[tokio::test]
async fn loading_a_goal_emits_one_current_plan_entry() {
    let fixture = support::DurableAcpFixture::new();
    let (session, _) = fixture.seed_settled_messages(ROOT);
    fixture.set_current_goal(&session, "Finish the ACP surface");
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (loaded, _) = connection
        .request(
            "session/load",
            serde_json::json!({"sessionId": session, "cwd": ROOT, "mcpServers": []}),
        )
        .await;
    assert!(loaded.get("result").is_some(), "load failed: {loaded}");
    let announced = connection.announced().await;
    let plans = updates(&announced)
        .into_iter()
        .filter(|params| params["update"]["sessionUpdate"] == "plan")
        .collect::<Vec<_>>();
    assert_eq!(plans.len(), 1, "no complete current plan: {announced:?}");
    let entries = plans[0]["update"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["content"], "Finish the ACP surface");
    assert_eq!(entries[0]["status"], "in_progress");
    assert_eq!(entries[0]["_meta"]["ego"]["goalState"], "active");
    connection.finish().await;
}

#[tokio::test]
async fn failed_mcp_admission_leaves_no_fork_child_listed() {
    let fixture = support::DurableAcpFixture::new();
    let source = fixture.seed_replay_session(ROOT, "Source title", 200_000);
    let before = fixture.effect_counts();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    let (response, _) = connection
        .request(
            "session/fork",
            serde_json::json!({
                "sessionId": source.session_id,
                "cwd": ROOT,
                "mcpServers": [{
                    "name": "refused-server",
                    "command": MCP_COMMAND,
                    "args": [],
                    "env": [],
                }],
            }),
        )
        .await;
    assert!(
        response.get("error").is_some(),
        "fork unexpectedly survived failed MCP admission: {response}"
    );
    let after = fixture.effect_counts();
    assert_eq!(
        after.0,
        before.0 + 1,
        "fork did not resolve its source root"
    );
    assert_eq!(after.1, before.1 + 1, "fork did not attempt MCP admission");
    // Whether the child is hidden while admission runs is pinned by the
    // window tests; this one pins what a refused admission leaves behind.
    let (listed, _) = connection
        .request("session/list", serde_json::json!({ "cwd": ROOT }))
        .await;
    let ids: Vec<&str> = listed["result"]["sessions"]
        .as_array()
        .expect("session/list rows")
        .iter()
        .map(|row| row["sessionId"].as_str().expect("row id"))
        .collect();
    assert_eq!(
        ids,
        vec![source.session_id.as_str()],
        "failed MCP admission left a listed fork child"
    );
    connection.finish().await;
}

#[tokio::test]
async fn delete_is_idempotent_hides_the_target_and_makes_reactivation_explicitly_fail() {
    let fixture = support::DurableAcpFixture::new();
    let target = fixture.seed_replay_session(ROOT, "Delete me", 200_000);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    for _ in 0..2 {
        let (deleted, _) = connection
            .request(
                "session/delete",
                serde_json::json!({ "sessionId": target.session_id.clone() }),
            )
            .await;
        assert!(deleted.get("result").is_some(), "delete failed: {deleted}");
    }
    let (listed, _) = connection
        .request("session/list", serde_json::json!({ "cwd": ROOT }))
        .await;
    assert!(
        listed["result"]["sessions"]
            .as_array()
            .expect("list rows")
            .iter()
            .all(|row| row["sessionId"] != target.session_id)
    );
    let (loaded, _) = connection
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": target.session_id.clone(),
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(
        loaded.get("error").is_some(),
        "a deleted session loaded: {loaded}"
    );
    connection.finish().await;
}

#[tokio::test]
async fn close_detaches_connection_state_without_deleting_durable_history() {
    let fixture = support::DurableAcpFixture::new();
    let target = fixture.seed_replay_session(ROOT, "Close me", 200_000);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    connection
        .request(
            "session/resume",
            serde_json::json!({
                "sessionId": target.session_id.clone(),
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;

    let (closed, _) = connection
        .request(
            "session/close",
            serde_json::json!({ "sessionId": target.session_id.clone() }),
        )
        .await;
    assert!(closed.get("result").is_some(), "close failed: {closed}");
    let (prompt, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": target.session_id.clone(),
                "prompt": [{ "type": "text", "text": "must be detached" }],
            }),
        )
        .await;
    assert!(
        prompt.get("error").is_some(),
        "closed connection handle still accepted a prompt: {prompt}"
    );
    connection.finish().await;

    let mut reconnected = Connected::new(fixture.router());
    reconnected.initialize().await;
    let (resumed, _) = reconnected
        .request(
            "session/resume",
            serde_json::json!({
                "sessionId": target.session_id,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(
        resumed.get("result").is_some(),
        "close deleted durable history: {resumed}"
    );
    reconnected.finish().await;
}

#[tokio::test]
async fn close_then_fresh_router_load_or_resume_continues_the_terminal_session() {
    for method in ["session/load", "session/resume"] {
        let fixture = support::DurableAcpFixture::new();
        let mut first = Connected::new(fixture.router());
        regression_case_phase("initialize first router", first.initialize()).await;
        let (session, _) = regression_case_phase("create terminal session", first.new_session(ROOT)).await;
        let (first_prompt, _) = regression_case_phase(
            "complete first terminal prompt",
            first.request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session.clone(),
                    "prompt": [{ "type": "text", "text": "first terminal turn" }],
                }),
            ),
        )
        .await;
        assert_eq!(first_prompt["result"]["stopReason"], "end_turn");
        let (closed, _) = regression_case_phase(
            "close terminal session",
            first.request(
                "session/close",
                serde_json::json!({ "sessionId": session.clone() }),
            ),
        )
        .await;
        assert!(closed.get("result").is_some(), "close failed: {closed}");
        regression_case_phase("finish first router", first.finish()).await;

        let mut fresh = Connected::new(fixture.router());
        regression_case_phase("initialize fresh router", fresh.initialize()).await;
        let (reattached, _) = regression_case_phase(
            "load or resume terminal session",
            fresh.request(
                method,
                serde_json::json!({
                    "sessionId": session.clone(),
                    "cwd": ROOT,
                    "mcpServers": [],
                }),
            ),
        )
        .await;
        assert!(
            reattached.get("result").is_some(),
            "{method} failed after close: {reattached}"
        );
        let (continued, _) = regression_case_phase(
            "complete continuation prompt",
            fresh.request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session.clone(),
                    "prompt": [{ "type": "text", "text": "continue after close" }],
                }),
            ),
        )
        .await;
        assert_eq!(continued["result"]["stopReason"], "end_turn", "{continued}");
        assert_eq!(fixture.run_status(&session), RunStatus::Settled);
        regression_case_phase("finish fresh router", fresh.finish()).await;
    }
}

#[tokio::test]
async fn immediate_cancel_after_admitted_continuation_preserves_durable_cancellation() {
    let fixture = support::DurableAcpFixture::new();
    let mut first = Connected::new(fixture.router());
    first.initialize().await;
    let (session, _) = first.new_session(ROOT).await;
    let (initial, _) = first
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "initial" }],
            }),
        )
        .await;
    assert_eq!(initial["result"]["stopReason"], "end_turn");
    let (closed, _) = first
        .request(
            "session/close",
            serde_json::json!({ "sessionId": session.clone() }),
        )
        .await;
    assert!(closed.get("result").is_some(), "close failed: {closed}");
    first.finish().await;

    let mut fresh = Connected::new(fixture.router());
    fresh.initialize().await;
    let (loaded, _) = fresh
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": session.clone(),
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(loaded.get("result").is_some(), "load failed: {loaded}");
    fixture.block_next_turn();
    let prompt = fresh
        .send_request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "cancel continuation" }],
            }),
        )
        .await;
    regression_case_phase("continuation reaches submit", fixture.turn_began()).await;
    fresh
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session.clone() }),
        )
        .await;
    regression_case_phase(
        "cancel notification barrier",
        fresh.request("session/list", serde_json::json!({ "cwd": ROOT })),
    )
    .await;
    fixture.release_turn();
    let (cancelled, _) =
        regression_case_phase("answer cancelled continuation", fresh.response(prompt)).await;
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );
    regression_case_phase(
        "durable cancellation settlement",
        fixture.wait_for_run_status(&session, RunStatus::Cancelled),
    )
    .await;
    fresh.finish().await;
}

#[tokio::test]
async fn exact_acp_continuation_replay_does_not_repeat_attach_or_submit() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    let params = serde_json::json!({
        "sessionId": session.clone(),
        "prompt": [{ "type": "text", "text": "replay me" }],
    });
    connection
        .send_request_with_id(41, "session/prompt", params.clone())
        .await;
    let (first, _) = regression_case_phase("complete first ACP prompt", connection.response(41)).await;
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    let effects_after_first = fixture.executor_effect_counts();
    connection
        .send_request_with_id(41, "session/prompt", params)
        .await;
    let (replay, _) = regression_case_phase("complete exact ACP replay", connection.response(41)).await;
    assert!(replay.get("result").is_some(), "replay failed: {replay}");
    assert_eq!(fixture.executor_effect_counts(), effects_after_first);
    connection.finish().await;
}

#[tokio::test]
async fn a_prompt_whose_settlement_durability_is_uncertain_is_answered_not_orphaned() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    fixture.fail_next_settlement_publication("settlement durability uncertain");
    let (answer, _) = regression_case_phase(
        "answer an uncertain settlement",
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "uncertain settlement" }],
            }),
        ),
    )
    .await;
    let data = answer["error"]["data"].as_str().unwrap_or_default();
    assert!(
        answer.get("error").is_some() && data.contains("settlement durability uncertain"),
        "an unpublishable settlement did not answer its request: {answer}"
    );
}

#[tokio::test]
async fn a_reused_request_id_carrying_a_new_prompt_is_new_work_not_a_replay() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    connection
        .send_request_with_id(
            41,
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "first" }],
            }),
        )
        .await;
    let (first, _) = regression_case_phase("answer the first prompt", connection.response(41)).await;
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    let (attached, submitted, observed) = fixture.executor_effect_counts();

    // JSON-RPC lets a client reuse an identity it has already been answered
    // on, so the same id carrying a different prompt is a second turn.
    connection
        .send_request_with_id(
            41,
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "second" }],
            }),
        )
        .await;
    let (second, _) = regression_case_phase("answer the reused id", connection.response(41)).await;
    assert_eq!(second["result"]["stopReason"], "end_turn", "{second}");
    assert_eq!(
        fixture.executor_effect_counts(),
        (attached, submitted + 1, observed),
        "a reused request id was mistaken for a replay"
    );
    connection.finish().await;
}

#[tokio::test]
async fn attach_and_submit_failures_return_typed_responses_without_hanging() {
    let fixture = support::DurableAcpFixture::new();
    let mut first = Connected::new(fixture.router());
    first.initialize().await;
    let (session, _) = first.new_session(ROOT).await;
    let (initial, _) = first
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "initial" }],
            }),
        )
        .await;
    assert_eq!(initial["result"]["stopReason"], "end_turn");
    first
        .request(
            "session/close",
            serde_json::json!({ "sessionId": session.clone() }),
        )
        .await;
    first.finish().await;

    let mut fresh = Connected::new(fixture.router());
    fresh.initialize().await;
    fresh
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": session.clone(),
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    fixture.fail_next_attach("attach provider failed");
    let (attach_failure, _) = regression_case_phase(
        "typed attach failure",
        fresh.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session.clone(),
                "prompt": [{ "type": "text", "text": "attach failure" }],
            }),
        ),
    )
    .await;
    let attach_data = attach_failure["error"]["data"].as_str().unwrap_or_default();
    assert!(
        attach_failure.get("error").is_some() && attach_data.contains("attach provider failed"),
        "attach failure was not typed or diagnostic-preserving: {attach_failure}"
    );
    fresh.abort().await;

    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    fixture.fail_next_submit("submit provider failed");
    let (submit_failure, _) = regression_case_phase(
        "typed submit failure",
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "submit failure" }],
            }),
        ),
    )
    .await;
    let submit_data = submit_failure["error"]["data"].as_str().unwrap_or_default();
    assert!(
        submit_failure.get("error").is_some() && submit_data.contains("submit provider failed"),
        "submit failure was not typed or diagnostic-preserving: {submit_failure}"
    );
    connection.abort().await;
}

#[tokio::test]
async fn every_nonempty_additional_directories_field_refuses_before_other_effects() {
    for method in [
        "session/new",
        "session/load",
        "session/resume",
        "session/fork",
    ] {
        let fixture = support::DurableAcpFixture::new();
        let target = fixture.seed_replay_session(ROOT, "Target", 200_000);
        let before = fixture.effect_counts();
        let mut connection = Connected::new(fixture.router());
        connection.initialize().await;
        let mut params = serde_json::json!({
            "cwd": ROOT,
            "mcpServers": [],
            "additionalDirectories": ["/tmp/extra-root"],
        });
        if method != "session/new" {
            params["sessionId"] = serde_json::json!(target.session_id);
        }
        let (response, _) = connection.request(method, params).await;
        assert!(
            response.get("error").is_some(),
            "{method} accepted unsupported additionalDirectories: {response}"
        );
        assert_eq!(
            fixture.effect_counts(),
            before,
            "{method} resolved roots, admitted MCP, or mutated lifecycle before refusing"
        );
        connection.finish().await;
    }
}

#[tokio::test]
async fn malformed_unknown_and_wrong_root_requests_return_explicit_errors() {
    let fixture = support::DurableAcpFixture::new();
    let target = fixture.seed_replay_session(ROOT, "Target", 200_000);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;

    for (method, params) in [
        (
            "session/load",
            serde_json::json!({
                "sessionId": "not-a-session-id",
                "cwd": ROOT,
                "mcpServers": [],
            }),
        ),
        (
            "session/resume",
            serde_json::json!({
                "sessionId": "00000000-0000-0000-0000-000000000001",
                "cwd": ROOT,
                "mcpServers": [],
            }),
        ),
        (
            "session/load",
            serde_json::json!({
                "sessionId": target.session_id,
                "cwd": WRONG_ROOT,
                "mcpServers": [],
            }),
        ),
    ] {
        let (response, _) = connection.request(method, params).await;
        let data = response["error"]["data"].as_str().unwrap_or_default();
        assert!(
            !data.is_empty(),
            "{method} did not explain its refusal: {response}"
        );
    }
    connection.finish().await;
}

// Catches: nested forks replaying an ancestor's discarded tail, losing local
// frames, or treating a tombstoned ancestor as unavailable.
#[tokio::test]
async fn critic_nested_fork_keeps_each_cutoff_and_local_ids_after_ancestor_deletion() {
    let fixture = support::DurableAcpFixture::new();
    let (root, root_items) = fixture.seed_settled_messages(ROOT);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (fork, _) = connection
        .request(
            "session/fork",
            serde_json::json!({
                "sessionId": root, "cwd": ROOT, "mcpServers": [],
                "_meta": {"ego": {"atMessageId": root_items[0]}}
            }),
        )
        .await;
    let child = fork["result"]["sessionId"].as_str().unwrap().to_owned();
    connection.announced().await;
    let child_first = fixture.append_settled_message(&child, "child first");
    let child_tail = fixture.append_settled_message(&child, "discarded child tail");
    let (fork, _) = connection
        .request(
            "session/fork",
            serde_json::json!({
                "sessionId": child, "cwd": ROOT, "mcpServers": [],
                "_meta": {"ego": {"atMessageId": child_first}}
            }),
        )
        .await;
    let grandchild = fork["result"]["sessionId"].as_str().unwrap().to_owned();
    connection.announced().await;
    let local = fixture.append_settled_message(&grandchild, "grandchild local");
    for parent in [&root, &child] {
        let (deleted, _) = connection
            .request(
                "session/delete",
                serde_json::json!({
                    "sessionId": parent
                }),
            )
            .await;
        assert!(deleted.get("error").is_none(), "{deleted}");
    }
    connection.finish().await;
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (loaded, _) = connection
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": grandchild, "cwd": ROOT, "mcpServers": []
            }),
        )
        .await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let announced = connection.announced().await;
    let messages = updates(&announced)
        .into_iter()
        .filter(|params| params["update"]["sessionUpdate"] == "user_message_chunk")
        .map(|params| &params["update"])
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 3, "discarded tails replayed: {messages:?}");
    for (message, expected) in messages.iter().zip([root_items[0], child_first, local]) {
        assert_eq!(message["messageId"], expected.to_string());
    }
    assert_eq!(messages[0]["_meta"]["ego"]["inherited"], true);
    assert_eq!(messages[1]["_meta"]["ego"]["inherited"], true);
    assert!(messages[2]["_meta"]["ego"].get("inherited").is_none());
    assert_eq!(
        loaded["result"]["_meta"]["ego"]["lineage"]["rootSessionId"],
        root
    );
    assert_eq!(
        loaded["result"]["_meta"]["ego"]["lineage"]["sourceDeleted"],
        true
    );
    let (before, _) = connection
        .request("session/list", serde_json::json!({"cwd": ROOT}))
        .await;
    for hidden in [root_items[1], child_tail] {
        let (refused, _) = connection
            .request(
                "session/fork",
                serde_json::json!({
                    "sessionId": grandchild, "cwd": ROOT, "mcpServers": [],
                    "_meta": {"ego": {"atMessageId": hidden}}
                }),
            )
            .await;
        assert!(
            refused.get("error").is_some(),
            "hidden tail became forkable: {refused}"
        );
    }
    let (after, _) = connection
        .request("session/list", serde_json::json!({"cwd": ROOT}))
        .await;
    assert_eq!(
        before["result"]["sessions"], after["result"]["sessions"],
        "refused forks published sessions"
    );
    connection.finish().await;
}

#[tokio::test]
async fn journal_failure_does_not_report_success_after_the_notice_was_queued() {
    let notices = AcpNotices::default();
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router_with_notices(notices.clone()));
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    notices.set_delivery_recorder(std::sync::Arc::new(|_, _| Err("disk full".to_owned())));
    let error = notices
        .publish(
            &session.into(),
            &AcpNotice {
                id: "recording-failure".to_owned(),
                text: "RESULT".to_owned(),
                action: NoticeAction::OpenResult {
                    path: "/results/worker.md".into(),
                },
            },
        )
        .expect_err("cursor must not acknowledge an unrecorded delivery");
    assert_eq!(error.code, ErrorCode::InternalError);
    assert!(
        error
            .data
            .unwrap()
            .to_string()
            .contains("delivery journal failed")
    );
    assert_eq!(connection.announced().await.len(), 1);
    connection.finish().await;
}

// Catches: an idle result resume bypasses ACP admission, updates another seat, or silently retries failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_resume_reuses_the_idle_acp_turn_and_reports_admission_failure() {
    let notices = AcpNotices::default();
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router_with_notices(notices.clone()));
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    assert!(
        notices
            .resume(&"unknown".into(), "report".into(), "Verify".into())
            .is_err()
    );
    let resumed = notices
        .resume(
            &session.clone().into(),
            "report".into(),
            "Verify actual evidence".into(),
        )
        .unwrap();
    assert_eq!(
        regression_case_phase("resume completion", resumed).await.unwrap(),
        Ok(())
    );
    assert_eq!(fixture.executor_effect_counts().1, 1);
    fixture.fail_next_submit("evidence verification failed");
    let failed = notices
        .resume(&session.into(), "report-two".into(), "Verify".into())
        .unwrap();
    assert!(
        regression_case_phase("resume failure", failed)
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(fixture.executor_effect_counts().1, 2);
    connection.finish().await;
}

// Catches: reports received during a turn run concurrently or create several pending turns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_resume_busy_turn_queues_one_consolidated_pending_turn() {
    let notices = AcpNotices::default();
    let fixture = support::DurableAcpFixture::new();
    let mut connection = Connected::new(fixture.router_with_notices(notices.clone()));
    connection.initialize().await;
    let (session, _) = connection.new_session(ROOT).await;
    fixture.block_next_turn();
    let prompt = connection
        .send_request(
            "session/prompt",
            serde_json::json!({"sessionId":session, "prompt":[{"type":"text","text":"start"}]}),
        )
        .await;
    regression_case_phase("original turn began", fixture.turn_began()).await;
    let first = notices
        .resume(
            &session.clone().into(),
            "first".into(),
            "RESULT first".into(),
        )
        .unwrap();
    let second = notices
        .resume(&session.into(), "second".into(), "BLOCKED second".into())
        .unwrap();
    assert_eq!(
        fixture.executor_effect_counts().1,
        1,
        "resume cannot overlap the original turn"
    );
    fixture.release_turn();
    assert!(connection.response(prompt).await.0["result"].is_object());
    assert_eq!(regression_case_phase("first report", first).await.unwrap(), Ok(()));
    assert_eq!(
        regression_case_phase("second report", second).await.unwrap(),
        Ok(())
    );
    assert_eq!(
        fixture.executor_effect_counts().1,
        2,
        "one pending consolidated turn"
    );
    connection.finish().await;
}

// Catches: advertised force is not boolean-validated, or delete bypasses durable holds.
#[tokio::test]
async fn delete_validates_force_and_refuses_held_sessions_even_with_force() {
    let fixture = support::DurableAcpFixture::new();
    let target = fixture.seed_replay_session(ROOT, "Held", 200_000);
    let mut connection = Connected::new(fixture.router());
    connection.initialize().await;
    let (invalid, _) = connection
        .request(
            "session/delete",
            serde_json::json!({
                "sessionId": target.session_id, "_meta": {"ego": {"force": "yes"}}
            }),
        )
        .await;
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    let (paused, _) = connection.request("_ego/pause", serde_json::json!({
        "v": 1, "sessionId": target.session_id, "requestId": uuid::Uuid::now_v7().to_string()
    })).await;
    assert!(paused.get("result").is_some(), "{paused}");
    let (deleted, _) = connection
        .request(
            "session/delete",
            serde_json::json!({
                "sessionId": target.session_id, "_meta": {"ego": {"force": true}}
            }),
        )
        .await;
    assert_eq!(deleted["error"]["code"], -32011, "{deleted}");
    let (listed, _) = connection
        .request("session/list", serde_json::json!({"cwd": ROOT}))
        .await;
    assert!(
        listed["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["sessionId"] == target.session_id)
    );
    connection.finish().await;
}
