//! One prompt, driven by a host, over a real connection.
//!
//! `tests/acp_golden.rs` pins what an event *becomes*, and `tests/initialize.rs`
//! pins what the router answers. Neither drives a turn, so until this file
//! existed nothing exercised the path `session/prompt` actually takes:
//! [`prompt_text`](ego_acp::serve) flattening the blocks, the session registry
//! handing back what `session/new` recorded, the spawn that keeps the dispatch
//! loop free, the sink turning a `PublicEvent` into a notification, and the
//! responder answering from inside the spawned work rather than when it was
//! accepted. Each of those is a place where a session's updates could go out
//! under the wrong name, in the wrong order, or not at all — and the projection
//! tests would all still pass.
//!
//! The claim being pinned is the milestone's: *the same* session updates the
//! CLI produces, through the same projection, not a second mapping. So the
//! expectations here are computed by calling `project_acp_v1` — the function
//! `--output jsonl`'s sibling sink calls — rather than written out as literal
//! JSON. A literal would pass while the two mappings diverged, which is the one
//! failure this file exists to catch.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::{ConnectTo, Lines};
use agent_client_protocol_schema::{ProtocolVersion, v1};
use ego_acp::project::project_acp_v1;
use ego_core::config::ModelRef;
use ego_core::event::domain::{CancelCause, DomainEvent};
use ego_core::event::id::{ItemId, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::public::{ItemKind, PublicEvent};
use ego_core::run::{
    IdSource, RunAppend, RunExecutionBindings, RunExecutor, RunExecutorError, RunHandle, RunId,
    RunPreparation, RunPrepared, RunRevision, RunRoot, RunSettlement, RunStart, RunTransaction,
};
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::CancelToken;
use futures::channel::mpsc;
use futures::future::{self, BoxFuture};
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tokio::sync::Notify;

mod support;

/// The model a turn in this file resolved to.
///
/// A stated context window, because the one thing an `UsageUpdate` needs that
/// no event carries is the window it is a fraction of — so leaving it `None`
/// would suppress exactly the update whose delivery this file is checking.
fn caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: 1,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_tools: Some(128),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
    }
}

const TURN: &str = "00000000-0000-0000-0000-000000000002";
const ITEM_REASONING: &str = "00000000-0000-0000-0000-000000000003";
const ITEM_MESSAGE: &str = "00000000-0000-0000-0000-000000000004";

/// Deterministic ids, so the run and the expectation below name one item.
///
/// `ItemId::new()` would not do: this fixture is built twice — once by the
/// turn and once by `expected_updates` — and two fresh UUIDs would differ in
/// exactly the `messageId` field the comparison is meant to check.
///
/// Monomorphised per type rather than generic for the reason `acp_golden.rs`
/// gives: `ego-acp` has no direct `serde` dependency to name
/// `serde::de::DeserializeOwned` with.
fn turn_id(uuid: &str) -> TurnId {
    serde_json::from_str(&format!("{uuid:?}")).expect("a valid uuid")
}

fn item_id(uuid: &str) -> ItemId {
    serde_json::from_str(&format!("{uuid:?}")).expect("a valid uuid")
}

/// What a turn publishes here: a thought, then a message in two pieces,
/// bracketed by the item lifecycle.
///
/// Five events and three updates, and the gap is the point twice over. Three
/// because one notification cannot show ordering, and ordering is half of what
/// a live stream promises. The gap because `ItemStarted` and `ItemCompleted`
/// have no stateless v1 counterpart (`acp_golden.rs` pins why), so the sink
/// must send nothing for them — a sink that sent an empty or defaulted update
/// per event would satisfy "three arrived, in order" and be wrong.
fn published() -> Vec<PublicEvent> {
    let turn = turn_id(TURN);
    let reasoning = item_id(ITEM_REASONING);
    let message = item_id(ITEM_MESSAGE);
    vec![
        PublicEvent::ItemStarted {
            turn,
            item: reasoning,
            kind: ItemKind::Reasoning,
            name: None,
            text: None,
        },
        PublicEvent::ItemDelta {
            turn,
            item: reasoning,
            kind: ItemKind::Reasoning,
            text: "weighing the options".to_string(),
        },
        PublicEvent::ItemDelta {
            turn,
            item: message,
            kind: ItemKind::AgentMessage,
            text: "Sure".to_string(),
        },
        PublicEvent::ItemDelta {
            turn,
            item: message,
            kind: ItemKind::AgentMessage,
            text: ", one sec".to_string(),
        },
        PublicEvent::ItemCompleted {
            turn,
            item: message,
            kind: ItemKind::AgentMessage,
            ok: true,
            text: Some("Sure, one sec".to_string()),
        },
    ]
}

#[derive(Clone, Default)]
struct BlockingState {
    started: Arc<Notify>,
    release: Arc<Notify>,
    ran: Arc<Mutex<Vec<(String, bool)>>>,
}

#[derive(Clone)]
enum PromptMode {
    Publish,
    Block(BlockingState),
}

#[derive(Clone)]
struct PromptExecutor {
    mode: PromptMode,
    turns: Arc<Mutex<Vec<(String, PathBuf)>>>,
    roots: Arc<Mutex<HashMap<RunId, RunRoot>>>,
}

struct PromptAuthority {
    cancel: CancelToken,
    mode: PromptMode,
}

struct PreparedToken;

impl PromptExecutor {
    fn publishing() -> Self {
        Self {
            mode: PromptMode::Publish,
            turns: Arc::new(Mutex::new(Vec::new())),
            roots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn blocking() -> Self {
        Self {
            mode: PromptMode::Block(BlockingState::default()),
            turns: Arc::new(Mutex::new(Vec::new())),
            roots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn turns(&self) -> Vec<(String, PathBuf)> {
        self.turns.lock().expect("prompt turns").clone()
    }

    async fn turn_began(&self) {
        match &self.mode {
            PromptMode::Block(blocking) => blocking.started.notified().await,
            PromptMode::Publish => panic!("publishing executor has no blocking turn"),
        }
    }

    fn let_a_turn_end(&self) {
        match &self.mode {
            PromptMode::Block(blocking) => blocking.release.notify_one(),
            PromptMode::Publish => panic!("publishing executor has no blocking turn"),
        }
    }

    fn ran(&self) -> Vec<(String, bool)> {
        match &self.mode {
            PromptMode::Block(blocking) => blocking.ran.lock().expect("prompt runs").clone(),
            PromptMode::Publish => panic!("publishing executor has no blocking turn"),
        }
    }
}

impl RunExecutor for PromptExecutor {
    type Prepared = PreparedToken;

    fn prepare<'a>(
        &'a self,
        _request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunPrepared::new(
            ModelRef("model/test".to_owned()),
            PreparedToken,
        ))))
    }

    fn start<'a>(
        &'a self,
        request: &'a RunStart,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        self.roots
            .lock()
            .expect("prompt roots")
            .insert(request.run_id, request.root.clone());
        let authority = PromptAuthority {
            cancel: CancelToken::new(),
            mode: self.mode.clone(),
        };
        Box::pin(future::ready(Ok(RunHandle::new(
            request.run_id,
            Arc::new(authority),
        ))))
    }

    fn recover<'a>(
        &'a self,
        _run: &'a RunId,
    ) -> BoxFuture<'a, Result<ego_core::run::RecoveryEvidence, RunExecutorError>> {
        Box::pin(future::ready(Ok(
            ego_core::run::RecoveryEvidence::Ambiguous,
        )))
    }

    fn observe<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunSettlement::completed())))
    }

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        prompt: &'a str,
        _prepared: Self::Prepared,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        let authority = handle
            .authority::<Arc<PromptAuthority>>()
            .expect("prompt authority");
        let mode = authority.mode.clone();
        let cancel = authority.cancel.clone();
        let root = self
            .roots
            .lock()
            .expect("prompt roots")
            .get(&handle.run_id())
            .cloned()
            .expect("started prompt root");
        let prompt = prompt.to_owned();
        let turns = Arc::clone(&self.turns);
        Box::pin(async move {
            match mode {
                PromptMode::Publish => {
                    turns
                        .lock()
                        .expect("prompt turns")
                        .push((prompt, root.as_path().to_path_buf()));
                    let sink = bindings.sink_factory.create(&caps());
                    let state = SessionState::default();
                    for event in published() {
                        sink.publish(&event, &state);
                    }
                    Ok(RunSettlement::completed())
                }
                PromptMode::Block(blocking) => {
                    blocking.started.notify_one();
                    blocking.release.notified().await;
                    let cancelled = cancel.is_cancelled();
                    blocking
                        .ran
                        .lock()
                        .expect("prompt runs")
                        .push((prompt, cancelled));
                    if cancelled {
                        Ok(RunSettlement::cancelled(ego_core::run::CancelReason::User))
                    } else {
                        Ok(RunSettlement::completed())
                    }
                }
            }
        })
    }

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        let authority = handle
            .authority::<Arc<PromptAuthority>>()
            .expect("prompt authority");
        authority.cancel.cancel(CancelCause::User);
        Box::pin(future::ready(Ok(())))
    }
}

#[derive(Default)]
struct PromptStore {
    entries: Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>,
}

impl SessionStore for PromptStore {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("prompt entries")
            .entry(*id)
            .or_default()
            .push(SessionEntry::Event(event.clone()));
        Ok(())
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let entries = self
            .entries
            .lock()
            .expect("prompt entries")
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
            .expect("prompt entries")
            .keys()
            .map(|id| SessionMeta {
                id: *id,
                workspace_root: PathBuf::from("/tmp"),
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
        let mut entries = self.entries.lock().expect("prompt entries");
        let actual = entries.get(session).and_then(|entries| {
            entries.iter().rev().find_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                SessionEntry::Event(_) => None,
            })
        });
        if expected_revision != actual {
            return Ok(RunAppend::Conflict {
                actual: actual.ok_or(StoreError::NotFound)?,
            });
        }
        let revision = transaction.revision();
        entries
            .entry(*session)
            .or_default()
            .push(SessionEntry::RunTransaction(transaction));
        Ok(RunAppend::Committed { revision })
    }
}

#[derive(Default)]
struct PromptClock;

impl Clock for PromptClock {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}

#[derive(Default)]
struct PromptIds {
    next: Mutex<u128>,
}

impl IdSource for PromptIds {
    fn next_uuid(&self) -> uuid::Uuid {
        let mut next = self.next.lock().expect("prompt ids");
        *next += 1;
        uuid::Uuid::from_u128(*next)
    }
}

/// One connection, driven request by request.
///
/// A struct rather than a function per request because these tests are about a
/// *sequence*: `session/new` mints the id `session/prompt` then names, and a
/// helper that opened a fresh connection each time could not express that.
struct Connected {
    to_agent: mpsc::UnboundedSender<String>,
    from_agent: mpsc::UnboundedReceiver<String>,
    agent: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    next_id: u64,
}

impl Connected {
    fn to(executor: PromptExecutor) -> Self {
        let (to_agent, to_agent_rx) = mpsc::unbounded::<String>();
        let (from_agent, from_agent_rx) = mpsc::unbounded::<String>();
        let transport = Lines::new(
            from_agent.sink_map_err(std::io::Error::other),
            to_agent_rx.map(Ok::<_, std::io::Error>),
        );
        let store = Arc::new(PromptStore::default());
        let executor = Arc::new(executor);
        let ids = Arc::new(PromptIds::default());
        let agent = support::agent_with(
            store,
            Arc::clone(&executor),
            Arc::new(PromptClock),
            Arc::clone(&ids),
        );
        Connected {
            to_agent,
            from_agent: from_agent_rx,
            agent: tokio::spawn(agent.connect_to(transport)),
            next_id: 1,
        }
    }

    /// Write one request and return its id **without waiting for the answer**.
    ///
    /// The half of [`request`](Self::request) that these tests need on its own:
    /// a second prompt can only arrive while the first turn is running if the
    /// first prompt's response has not been waited for.
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
            .expect("the agent is listening");
        id
    }

    /// Write one notification. No id, and so no response to wait for — which
    /// is what `session/cancel` is.
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
            .expect("the agent is listening");
    }

    /// Read until the answer to `id` arrives, collecting what came first.
    ///
    /// Matching on the id rather than taking the next line is the point: a
    /// prompt's response arrives after its updates, so a helper that read one
    /// line would read a notification and call it the answer.
    async fn response_to(&mut self, id: u64) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut notifications = Vec::new();
        loop {
            let line = self
                .from_agent
                .next()
                .await
                .expect("every request is answered");
            let message: serde_json::Value =
                serde_json::from_str(&line).expect("the agent writes JSON-RPC");
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return (message, notifications);
            }
            notifications.push(message);
        }
    }

    /// Send one request and return the response to *it*, collecting every
    /// notification that arrived first.
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let id = self.send(method, params).await;
        self.response_to(id).await
    }

    /// `initialize` then `session/new`, returning the session id minted.
    async fn open(&mut self, cwd: &str) -> String {
        let (initialized, _) = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": ProtocolVersion::V1.as_u16(),
                    "clientCapabilities": {},
                }),
            )
            .await;
        assert!(
            initialized.get("result").is_some(),
            "initialize was refused: {initialized}"
        );

        let (opened, _) = self
            .request(
                "session/new",
                serde_json::json!({ "cwd": cwd, "mcpServers": [] }),
            )
            .await;
        opened["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new answered without a session id: {opened}"))
            .to_string()
    }

    /// Close our end so the spawned agent finishes rather than outliving the
    /// test and holding a runtime worker.
    async fn finish(self) {
        self.to_agent.close_channel();
        let _ = self.agent.await;
    }
}

/// Await something that must happen, and say what its not happening meant.
///
/// Without this a request the agent never answers hangs the whole test binary,
/// and a hang is not a report: `cargo test` prints nothing and the reader is
/// left to guess which assertion never ran. The admission defect presents
/// exactly that way — a second prompt is accepted, parks behind the first, and
/// its response never comes.
///
/// Ten seconds because it is only ever reached on failure. Nothing here waits
/// for a model; every step is an in-memory channel, so a passing run consumes
/// none of it.
async fn awaited<T>(what: &str, work: impl Future<Output = T>) -> T {
    match tokio::time::timeout(std::time::Duration::from_secs(10), work).await {
        Ok(value) => value,
        Err(_) => panic!("{what}"),
    }
}

/// The notifications a turn should have produced, computed the same way the
/// `--output jsonl` sink computes its lines.
fn expected_updates(session: &str) -> Vec<serde_json::Value> {
    let state = SessionState::default();
    published()
        .iter()
        .filter_map(|event| project_acp_v1(event, &state, &caps()))
        .map(|update| {
            serde_json::to_value(v1::SessionNotification::new(
                v1::SessionId::new(session.to_string()),
                update,
            ))
            .expect("a session notification serialises")
        })
        .collect()
}

/// The whole claim, end to end: a host prompt produces this session's updates,
/// in order, and then the answer.
#[tokio::test]
async fn a_prompt_produces_the_same_updates_the_cli_would_and_then_its_stop_reason() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open("/tmp/some-workspace").await;

    let (response, notifications) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "list the files" }],
            }),
        )
        .await;

    assert_eq!(
        response["result"]["stopReason"], "end_turn",
        "the turn's stop reason did not reach the response: {response}"
    );
    assert_eq!(
        turns.turns(),
        vec![(
            "list the files".to_string(),
            PathBuf::from("/tmp/some-workspace")
        )],
        "the host was handed a different prompt, or a different workspace, \
         than the client sent"
    );

    let params: Vec<serde_json::Value> = notifications
        .iter()
        .filter(|message| message["method"] == "session/update")
        .map(|message| message["params"].clone())
        .collect();
    assert_eq!(
        params,
        expected_updates(&session),
        "the updates a host received are not `project_acp_v1` of what the run \
         published — the ACP path has grown a second mapping"
    );

    connection.finish().await;
}

/// Two prompts on one connection stay in their own sessions.
///
/// The registry hands a prompt the session `session/new` recorded, and nothing
/// but the id says which. A single-session test cannot fail on a sink built
/// with the wrong id, because there is only one id to build it with.
#[tokio::test]
async fn each_session_receives_only_its_own_updates() {
    let mut connection = Connected::to(PromptExecutor::publishing());
    let first = connection.open("/tmp/first").await;
    let second = connection.open("/tmp/second").await;
    assert_ne!(first, second, "session/new handed out the same id twice");

    let (_, notifications) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": second,
                "prompt": [{ "type": "text", "text": "go" }],
            }),
        )
        .await;

    let named: Vec<&serde_json::Value> = notifications
        .iter()
        .filter(|message| message["method"] == "session/update")
        .map(|message| &message["params"]["sessionId"])
        .collect();
    assert!(!named.is_empty(), "the turn published nothing at all");
    assert!(
        named.iter().all(|id| *id == &serde_json::json!(second)),
        "an update went out under another session's name: {named:?}"
    );

    connection.finish().await;
}

/// A resource link is carried into the prompt as its URI, and the two blocks
/// every agent must accept arrive joined in the order sent.
///
/// The alternative — dropping the link — would answer a question about a file
/// the client believed it had attached, which is `prompt_text`'s stated reason
/// for keeping it.
#[tokio::test]
async fn the_two_block_kinds_every_agent_must_accept_reach_the_turn() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open("/tmp/ws").await;

    connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [
                    { "type": "text", "text": "summarise" },
                    { "type": "resource_link", "uri": "file:///tmp/ws/notes.md", "name": "notes" },
                ],
            }),
        )
        .await;

    assert_eq!(
        turns.turns()[0].0,
        "summarise\nfile:///tmp/ws/notes.md",
        "a resource link did not reach the turn as its URI"
    );

    connection.finish().await;
}

/// A block kind this build cannot read is refused by name, and no turn runs.
///
/// Both halves matter. `initialize` advertises no image capability, so a
/// conforming client never sends one; a client that does must not have its
/// prompt silently answered without the picture it asked about.
#[tokio::test]
async fn an_image_is_refused_by_name_and_no_turn_is_started() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open("/tmp/ws").await;

    let (response, notifications) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "image", "data": "aGk=", "mimeType": "image/png" }],
            }),
        )
        .await;

    let data = response["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("an image prompt was not refused: {response}"));
    assert!(
        data.contains("an image"),
        "the refusal does not name what it refused: {data}"
    );
    assert!(
        turns.turns().is_empty(),
        "a turn ran on a prompt this build could not read"
    );
    assert!(
        notifications.is_empty(),
        "a refused prompt still published updates: {notifications:?}"
    );

    // And the session is still usable. The admission rule marks a session busy
    // before the blocks are read, so a build that refused here without
    // releasing it would cost the client its session over one bad block —
    // permanently, since nothing else ever clears the mark.
    let (recovered, _) = awaited(
        "the session was left marked busy by a prompt it refused",
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "then just tell me" }],
            }),
        ),
    )
    .await;
    assert_eq!(
        recovered["result"]["stopReason"], "end_turn",
        "the next prompt on that session was refused too: {recovered}"
    );

    connection.finish().await;
}

/// A prompt for a session nobody opened is refused, and says how to open one.
#[tokio::test]
async fn a_prompt_for_an_unopened_session_is_refused() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    connection.open("/tmp/ws").await;

    let (response, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": "00000000-0000-0000-0000-000000000000",
                "prompt": [{ "type": "text", "text": "go" }],
            }),
        )
        .await;

    let data = response["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("an unknown session was accepted: {response}"));
    assert!(
        data.contains("session/new"),
        "the refusal does not say how to open a session: {data}"
    );
    assert!(turns.turns().is_empty(), "a turn ran without a session");

    connection.finish().await;
}

/// A second prompt for a session whose turn is still running is refused, and
/// no second turn starts.
///
/// `docs/decisions/2026-08-21-one-turn-per-session-and-what-cancel-means.md`
/// §3. The refusal is not politeness: the registry holds one `CancelToken` per
/// session, so admitting a second turn would replace the running turn's token
/// with the queued turn's and leave the running one unreachable by
/// `session/cancel` — the defect §1 describes.
///
/// Both halves are asserted. A build that answered the second prompt with an
/// error *and* ran the turn anyway would satisfy the first assertion alone.
#[tokio::test]
async fn a_second_prompt_while_a_turn_is_running_is_refused_and_starts_nothing() {
    let host = PromptExecutor::blocking();
    let mut connection = Connected::to(host.clone());
    let session = connection.open("/tmp/ws").await;

    let first = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "the running turn" }],
            }),
        )
        .await;
    awaited("the first turn never started", host.turn_began()).await;

    let (refused, _) = awaited(
        "a second concurrent prompt was neither refused nor answered: it was \
         accepted and queued behind the running turn",
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "the second turn" }],
            }),
        ),
    )
    .await;
    let data = refused["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("a second concurrent prompt was accepted: {refused}"));
    assert!(
        data.contains("already has a turn running"),
        "the refusal does not say why it refused: {data}"
    );

    host.let_a_turn_end();
    let (answered, _) = awaited(
        "the running turn never answered once it was released",
        connection.response_to(first),
    )
    .await;
    assert_eq!(
        answered["result"]["stopReason"], "end_turn",
        "the running turn did not answer normally: {answered}"
    );
    assert_eq!(
        host.ran()
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>(),
        vec!["the running turn"],
        "a second turn ran for a session that already had one"
    );

    connection.finish().await;
}

/// `session/cancel` reaches the turn that is running.
///
/// The other half of §3, and the one the admission rule exists to keep true: a
/// host's cancel names a session, and the session's answer must be the turn
/// that is actually spending money — not a token left over from a prompt that
/// arrived later.
#[tokio::test]
async fn cancel_reaches_the_turn_that_is_running() {
    let host = PromptExecutor::blocking();
    let mut connection = Connected::to(host.clone());
    let session = connection.open("/tmp/ws").await;

    let running = connection
        .send(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "stop me" }],
            }),
        )
        .await;
    awaited("the turn never started", host.turn_began()).await;

    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session }),
        )
        .await;
    // The notification and the release race unless the release is what unblocks
    // the turn *after* the cancel has been dispatched. `session/cancel` stays
    // on the dispatch loop (`serve.rs`'s module doc), and the turn is parked on
    // `release` — so the loop is free and the flip has happened by the time
    // this next request is answered.
    let (_, _) = awaited(
        "the dispatch loop was blocked, so the cancel never reached the registry",
        connection.request(
            "session/new",
            serde_json::json!({ "cwd": "/tmp/other", "mcpServers": [] }),
        ),
    )
    .await;
    host.let_a_turn_end();

    let (answered, _) = awaited(
        "the cancelled turn never answered",
        connection.response_to(running),
    )
    .await;
    assert_eq!(
        answered["result"]["stopReason"], "cancelled",
        "the running turn was not cancelled: {answered}"
    );
    assert_eq!(
        host.ran(),
        vec![("stop me".to_string(), true)],
        "the turn did not see its own token flip"
    );

    connection.finish().await;
}

/// A session that finished a turn accepts the next one.
///
/// The admission rule is per *turn in flight*, not per session for the life of
/// the connection. A build that marked a session busy and never cleared it
/// would pass both tests above and make every session single-use.
#[tokio::test]
async fn a_session_accepts_another_prompt_once_its_turn_has_ended() {
    let host = PromptExecutor::blocking();
    let mut connection = Connected::to(host.clone());
    let session = connection.open("/tmp/ws").await;

    for text in ["first", "second"] {
        let sent = connection
            .send(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await;
        awaited(&format!("the {text} turn never started"), host.turn_began()).await;
        host.let_a_turn_end();
        let (answered, _) = awaited(
            &format!("the {text} turn never answered"),
            connection.response_to(sent),
        )
        .await;
        assert_eq!(
            answered["result"]["stopReason"], "end_turn",
            "the {text} turn was refused or failed: {answered}"
        );
    }

    assert_eq!(
        host.ran()
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"],
        "the session did not run both of its turns"
    );

    connection.finish().await;
}

/// `session/new` refuses MCP servers rather than accepting ones it will never
/// connect to.
///
/// A client that names a server and is told nothing would attribute every
/// missing tool to the agent being bad at using them.
#[tokio::test]
async fn session_new_refuses_mcp_servers_this_build_cannot_connect_to() {
    let mut connection = Connected::to(PromptExecutor::publishing());
    let (initialized, _) = connection
        .request(
            "initialize",
            serde_json::json!({
                "protocolVersion": ProtocolVersion::V1.as_u16(),
                "clientCapabilities": {},
            }),
        )
        .await;
    assert!(initialized.get("result").is_some());

    let (response, _) = connection
        .request(
            "session/new",
            serde_json::json!({
                "cwd": "/tmp/ws",
                "mcpServers": [{ "name": "files", "command": "/bin/true", "args": [], "env": [] }],
            }),
        )
        .await;

    assert!(
        response.get("error").is_some(),
        "a server was accepted that nothing will connect to: {response}"
    );

    connection.finish().await;
}
