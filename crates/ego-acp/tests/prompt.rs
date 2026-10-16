//! One prompt, driven by a host, over a real connection.
//!
//! `tests/acp_golden.rs` pins what an event *becomes*, and `tests/initialize.rs`
//! pins what the router answers. Neither drives a turn, so until this file
//! existed nothing exercised the path `session/prompt` actually takes:
//! [`prompt_content`](ego_acp::serve) reading the blocks, the session registry
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::{ConnectTo, Lines};
use agent_client_protocol_schema::{ProtocolVersion, v1};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ego_acp::project::project_acp_v1;
use ego_acp::serve::{McpSessionAdmissionError, McpSessionAdmitter, SessionMcpAdmission};
use ego_core::config::ModelRef;
use ego_core::event::domain::{CancelCause, DomainEvent};
use ego_core::event::hook::{HookFailure, HookFailureKind};
use ego_core::event::id::{ItemId, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{EvidenceError, EvidenceWrite};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::ImageMediaType;
use ego_core::hook::{
    HookCausality, HookFuture, HookInvocationPort, HookInvocationRequest, SessionHookRuntime,
};
use ego_core::mcp::{ForeignQuestionPort, McpRuntimeBinding, ToolExecution};
use ego_core::public::{ItemKind, PublicEvent};
use ego_core::run::{
    IdSource, RequestId, RunAppend, RunAttachment, RunCommand, RunExecutionBindings, RunExecutor,
    RunExecutorError, RunHandle, RunId, RunPreparation, RunPrepared, RunPrompt, RunRevision,
    RunRoot, RunSettlement, RunTransaction,
};
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::suspension::ResumeFloor;
use ego_core::turn::{
    AdmittedToolSource, CapturedToolCatalogue, ToolCatalogueError, ToolEffectPorts,
};
use ego_core::turn::{CancelToken, PauseToken};
use futures::channel::mpsc;
use futures::future::{self, BoxFuture};
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tokio::sync::Notify;

mod support;

use support::session_writer;

const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/prompt-workspace");
const SOME_WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/prompt-some-workspace");
const FIRST_WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/prompt-first");
const SECOND_WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/prompt-second");
const OTHER_WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/prompt-other");
#[cfg(not(windows))]
const MCP_COMMAND: &str = "/bin/true";
#[cfg(windows)]
const MCP_COMMAND: &str = r"C:\Windows\System32\cmd.exe";

fn workspace_file_uri(name: &str) -> String {
    format!(
        "file:///{}/{}",
        WORKSPACE.trim_start_matches('/').replace('\\', "/"),
        name
    )
}

/// The model a turn in this file resolved to.
///
/// A stated context window, because the one thing an `UsageUpdate` needs that
/// no event carries is the window it is a fraction of — so leaving it `None`
/// would suppress exactly the update whose delivery this file is checking.
fn caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
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
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
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

type RecordedMcpCapture = (usize, Vec<String>);
type RecordedMcpSource = Option<RecordedMcpCapture>;
type SharedMcpSourceLog = Arc<Mutex<Vec<RecordedMcpSource>>>;

#[derive(Clone)]
struct PromptExecutor {
    mode: PromptMode,
    turns: Arc<Mutex<Vec<(RunPrompt, PathBuf)>>>,
    roots: Arc<Mutex<HashMap<RunId, RunRoot>>>,
    mcp_sources: SharedMcpSourceLog,
    /// Hand back a handle for a run nobody asked about.
    ///
    /// The one way to stage an *integrity* failure — as opposed to an executor
    /// error — from outside `ego-core`: the controller checks the identity of
    /// what it attached to, and an executor that answers about another run is
    /// this agent being broken, not a provider being unavailable.
    misattach: Arc<AtomicBool>,
    /// Answer every submit with an executor error, settling the turn uncertain.
    failing_submit: Arc<AtomicBool>,
}

struct PromptAuthority {
    cancel: CancelToken,
    pause: PauseToken,
    mode: PromptMode,
}

struct PreparedToken;

impl PromptExecutor {
    fn publishing() -> Self {
        Self {
            mode: PromptMode::Publish,
            turns: Arc::new(Mutex::new(Vec::new())),
            roots: Arc::new(Mutex::new(HashMap::new())),
            mcp_sources: Arc::new(Mutex::new(Vec::new())),
            misattach: Arc::new(AtomicBool::new(false)),
            failing_submit: Arc::new(AtomicBool::new(false)),
        }
    }

    fn blocking() -> Self {
        Self {
            mode: PromptMode::Block(BlockingState::default()),
            turns: Arc::new(Mutex::new(Vec::new())),
            roots: Arc::new(Mutex::new(HashMap::new())),
            mcp_sources: Arc::new(Mutex::new(Vec::new())),
            misattach: Arc::new(AtomicBool::new(false)),
            failing_submit: Arc::new(AtomicBool::new(false)),
        }
    }

    fn turns(&self) -> Vec<(RunPrompt, PathBuf)> {
        self.turns.lock().expect("prompt turns").clone()
    }

    fn mcp_sources(&self) -> Vec<RecordedMcpSource> {
        self.mcp_sources.lock().expect("prompt MCP sources").clone()
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

    /// Break the next attach, so the turn after this call fails an internal
    /// invariant rather than an executor call.
    fn misattach_next(&self) {
        self.misattach.store(true, Ordering::SeqCst);
    }

    /// Fail every submit from here on, so the turn settles uncertain.
    ///
    /// The counterpart of [`misattach_next`](Self::misattach_next): an
    /// executor that cannot complete a turn is not this agent breaking, and
    /// the two settle differently on purpose.
    fn fail_submits(&self) {
        self.failing_submit.store(true, Ordering::SeqCst);
    }

    fn ran(&self) -> Vec<(String, bool)> {
        match &self.mode {
            PromptMode::Block(blocking) => blocking.ran.lock().expect("prompt runs").clone(),
            PromptMode::Publish => panic!("publishing executor has no blocking turn"),
        }
    }
}

#[derive(Default)]
struct NoMcpAdmitter;

impl McpSessionAdmitter for NoMcpAdmitter {
    fn admit(
        &self,
        _servers: Vec<v1::McpServer>,
        _connection: Option<agent_client_protocol::ConnectionTo<agent_client_protocol::Client>>,
        _context: ego_acp::serve::McpAdmissionContext,
    ) -> BoxFuture<'static, Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError>>
    {
        Box::pin(future::ready(Ok(None)))
    }
}

#[derive(Default)]
struct RecordingMcpSource;

impl AdmittedToolSource for RecordingMcpSource {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        CapturedToolCatalogue::new("acp-test-generation", Vec::new())
    }
}

struct RecordingMcpAdmission {
    source: Arc<dyn AdmittedToolSource>,
    errors: Vec<String>,
    runtime: Arc<dyn McpRuntimeBinding>,
}

impl SessionMcpAdmission for RecordingMcpAdmission {
    fn admitted_tools(&self) -> Arc<dyn AdmittedToolSource> {
        Arc::clone(&self.source)
    }

    fn mcp_errors(&self) -> &[String] {
        &self.errors
    }

    fn mcp_runtime(&self) -> Arc<dyn McpRuntimeBinding> {
        Arc::clone(&self.runtime)
    }
}

struct RejectingMcpPort;

impl ForeignQuestionPort for RejectingMcpPort {
    fn resume<'a>(
        &'a self,
        _continuation_id: &'a str,
        _canonical_json: &'a str,
        _causality: HookCausality,
    ) -> BoxFuture<'a, Result<ToolExecution, String>> {
        Box::pin(future::ready(Err(
            "prompt fixture must not invoke foreign resume".to_owned(),
        )))
    }
}

#[derive(Clone, Default)]
struct RecordingMcpRuntime;

struct UnusedHookPort;

impl HookInvocationPort for UnusedHookPort {
    fn invoke<'a>(&'a self, _request: HookInvocationRequest) -> HookFuture<'a> {
        Box::pin(future::ready(Err(HookFailure::new(
            HookFailureKind::HandlerFailed,
            None,
        ))))
    }
}

impl McpRuntimeBinding for RecordingMcpRuntime {
    fn load_pending_foreign_policy(
        &self,
        _continuation_id: &str,
    ) -> Result<Arc<ego_core::turn::GenerationPolicySnapshot>, String> {
        Err("prompt fixture has no pending foreign policy".to_owned())
    }

    fn bind_pending_foreign_continuation(
        &self,
        _continuation_id: &str,
        _snapshot: Arc<ego_core::turn::GenerationPolicySnapshot>,
    ) -> Result<(), String> {
        Err("prompt fixture cannot bind a pending continuation".to_owned())
    }

    fn hook_invocation_port<'a>(
        &self,
        _policy: Arc<ego_core::permit::PolicyEngine>,
        _effects: ToolEffectPorts<'a>,
    ) -> Result<Arc<dyn HookInvocationPort + 'a>, String> {
        Ok(Arc::new(UnusedHookPort))
    }

    fn foreign_question_port<'session, 'turn>(
        &self,
        _runtime: SessionHookRuntime<'session>,
        _floor: ResumeFloor,
        _effects: ToolEffectPorts<'turn>,
    ) -> Arc<dyn ForeignQuestionPort + 'turn>
    where
        'session: 'turn,
    {
        Arc::new(RejectingMcpPort)
    }
}

#[derive(Clone, Default)]
struct RecordingMcpAdmitter {
    requests: Arc<Mutex<Vec<Vec<v1::McpServer>>>>,
    source: Arc<RecordingMcpSource>,
}

impl RecordingMcpAdmitter {
    fn requests(&self) -> Vec<Vec<v1::McpServer>> {
        self.requests
            .lock()
            .expect("MCP admission requests")
            .clone()
    }
}

impl McpSessionAdmitter for RecordingMcpAdmitter {
    fn admit(
        &self,
        servers: Vec<v1::McpServer>,
        _connection: Option<agent_client_protocol::ConnectionTo<agent_client_protocol::Client>>,
        _context: ego_acp::serve::McpAdmissionContext,
    ) -> BoxFuture<'static, Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError>>
    {
        self.requests
            .lock()
            .expect("MCP admission requests")
            .push(servers);
        let admission = RecordingMcpAdmission {
            source: Arc::clone(&self.source) as Arc<dyn AdmittedToolSource>,
            errors: vec!["safe MCP diagnostic".to_owned()],
            runtime: Arc::new(RecordingMcpRuntime),
        };
        let admission: Arc<dyn SessionMcpAdmission> = Arc::new(admission);
        Box::pin(future::ready(Ok(Some(admission))))
    }
}

struct FailingMcpAdmitter;

impl McpSessionAdmitter for FailingMcpAdmitter {
    fn admit(
        &self,
        _servers: Vec<v1::McpServer>,
        _connection: Option<agent_client_protocol::ConnectionTo<agent_client_protocol::Client>>,
        _context: ego_acp::serve::McpAdmissionContext,
    ) -> BoxFuture<'static, Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError>>
    {
        Box::pin(future::ready(Err(McpSessionAdmissionError::new(
            "MCP admission failed safely",
        ))))
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

    fn admit_run_context<'a>(
        &'a self,
        _prepared: &'a Self::Prepared,
    ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        Box::pin(future::ready(Ok(EvidenceWrite::Published)))
    }

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        self.roots
            .lock()
            .expect("prompt roots")
            .insert(request.run_id, request.root.clone());
        let authority = PromptAuthority {
            cancel: CancelToken::new(),
            pause: PauseToken::new(),
            mode: self.mode.clone(),
        };
        let run_id = if self.misattach.load(Ordering::SeqCst) {
            RunId::new(request.run_id.session_id(), uuid::Uuid::from_u128(0xdead))
        } else {
            request.run_id
        };
        Box::pin(future::ready(Ok(RunHandle::new(
            run_id,
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
        prompt: &'a RunPrompt,
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
        let prompt = prompt.clone();
        let turns = Arc::clone(&self.turns);
        let mcp_sources = Arc::clone(&self.mcp_sources);
        let failing = self.failing_submit.load(Ordering::SeqCst);
        Box::pin(async move {
            if failing {
                return Err(RunExecutorError::new("the provider connection dropped"));
            }
            match mode {
                PromptMode::Publish => {
                    mcp_sources.lock().expect("prompt MCP sources").push(
                        bindings.admitted_tools().map(|source| {
                            (
                                Arc::as_ptr(&source).cast::<()>() as usize,
                                bindings.mcp_errors().to_vec(),
                            )
                        }),
                    );
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
                        .push((prompt.text().to_owned(), cancelled));
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

    fn pause<'a>(
        &'a self,
        handle: &'a RunHandle,
        request: RequestId,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        let authority = handle
            .authority::<Arc<PromptAuthority>>()
            .expect("prompt authority");
        authority.pause.pause(request);
        Box::pin(future::ready(Ok(())))
    }
}

#[derive(Default)]
struct PromptStore {
    writers: Mutex<BTreeSet<SessionId>>,
    entries: Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>,
}

impl SessionStore for PromptStore {
    fn open_writer_lease(
        &self,
        id: &SessionId,
        _: Timestamp,
    ) -> Result<ego_core::store::session_store::SessionWriterLease<'_>, StoreError> {
        session_writer::open(self, &self.writers, *id)
    }

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
                profiles: Default::default(),
                id: *id,
                workspace_root: PathBuf::from("/tmp"),
                model: None,
                created: Timestamp::EPOCH,
                updated: Timestamp::EPOCH,
                title: None,
                summary: None,
                summary_failures: 0,
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
                SessionEntry::Event(_)
                | SessionEntry::Evidence(_)
                | SessionEntry::NoticeDelivered(_)
                | SessionEntry::ModelSelected(_) => None,
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

impl PromptStore {
    /// Every run transaction this store was asked to keep, in order.
    fn transactions(&self) -> Vec<RunTransaction> {
        self.entries
            .lock()
            .expect("prompt entries")
            .values()
            .flatten()
            .filter_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.clone()),
                SessionEntry::Event(_)
                | SessionEntry::Evidence(_)
                | SessionEntry::NoticeDelivered(_)
                | SessionEntry::ModelSelected(_) => None,
            })
            .collect()
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
    store: Arc<PromptStore>,
}

impl Connected {
    fn to(executor: PromptExecutor) -> Self {
        Self::to_with_admitter(executor, Arc::new(NoMcpAdmitter))
    }

    fn to_with_admitter(executor: PromptExecutor, admitter: Arc<dyn McpSessionAdmitter>) -> Self {
        let (to_agent, to_agent_rx) = mpsc::unbounded::<String>();
        let (from_agent, from_agent_rx) = mpsc::unbounded::<String>();
        let transport = Lines::new(
            from_agent.sink_map_err(std::io::Error::other),
            to_agent_rx.map(Ok::<_, std::io::Error>),
        );
        let store = Arc::new(PromptStore::default());
        let executor = Arc::new(executor);
        let ids = Arc::new(PromptIds::default());
        let agent = support::agent_with_mcp(
            Arc::clone(&store),
            Arc::clone(&executor),
            Arc::new(PromptClock),
            Arc::clone(&ids),
            admitter,
        );
        Connected {
            to_agent,
            from_agent: from_agent_rx,
            agent: tokio::spawn(agent.connect_to(transport)),
            next_id: 1,
            store,
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
        self.send_as(id, method, params).await;
        id
    }

    /// Write one request under an id the caller chooses, so a test can re-send
    /// a request the agent has already answered.
    ///
    /// That is what a re-delivery *is* on the wire — JSON-RPC identifies a
    /// request by its id, and a reconnecting client replaying its unacknowledged
    /// writes sends the same id again. A helper that always minted a fresh id
    /// could only ever express new work.
    async fn send_as(&mut self, id: u64, method: &str, params: serde_json::Value) {
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
    let session = connection.open(SOME_WORKSPACE).await;

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
        turns
            .turns()
            .into_iter()
            .map(|(prompt, root)| (prompt.text().to_owned(), root))
            .collect::<Vec<_>>(),
        vec![("list the files".to_string(), PathBuf::from(SOME_WORKSPACE))],
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
    let first = connection.open(FIRST_WORKSPACE).await;
    let second = connection.open(SECOND_WORKSPACE).await;
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
/// the client believed it had attached, which is `prompt_content`'s stated
/// reason for keeping it.
#[tokio::test]
async fn the_two_block_kinds_every_agent_must_accept_reach_the_turn() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;
    let resource_uri = workspace_file_uri("notes.md");

    connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [
                    { "type": "text", "text": "summarise" },
                    { "type": "resource_link", "uri": resource_uri.clone(), "name": "notes" },
                ],
            }),
        )
        .await;

    assert_eq!(
        turns.turns()[0].0.text(),
        format!("summarise\n{resource_uri}"),
        "a resource link did not reach the turn as its URI"
    );

    connection.finish().await;
}

/// A block kind this build cannot read is refused by name, and no turn runs.
///
/// Both halves matter. `initialize` advertises no audio capability, so a
/// conforming client never sends one; a client that does must not have its
/// prompt silently answered without the recording it asked about.
#[tokio::test]
async fn audio_is_refused_by_name_and_no_turn_is_started() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;

    let (response, notifications) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "audio", "data": "aGk=", "mimeType": "audio/wav" }],
            }),
        )
        .await;

    let data = response["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("an audio prompt was not refused: {response}"));
    assert!(
        data.contains("audio"),
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

/// A one-pixel PNG, as the encoder emits it. Shared with `images_profiles.rs`
/// by value rather than by import, because a test fixture crossing a crate
/// boundary would need a public constant that production has no use for.
const PNG_1X1: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4, 0,
    0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5, 1, 1,
    39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// An image block reaches the turn decoded, and measured from its own bytes.
///
/// The dimensions are the point. A client states a media type and nothing else
/// about the picture, so the width and height that end up on the durable record
/// can only come from decoding it here — and a build that trusted a client's
/// numbers would write a size into the session store that the bytes contradict.
#[tokio::test]
async fn an_image_reaches_the_turn_decoded_and_measured() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;

    connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [
                    { "type": "text", "text": "what is in this" },
                    {
                        "type": "image",
                        "data": BASE64.encode(PNG_1X1),
                        "mimeType": "image/png",
                    },
                ],
            }),
        )
        .await;

    let prompt = &turns.turns()[0].0;
    assert_eq!(prompt.text(), "what is in this");
    let images = prompt.images();
    assert_eq!(images.len(), 1, "the image did not reach the turn");
    assert_eq!(images[0].bytes(), PNG_1X1, "the bytes were not decoded");
    assert_eq!(images[0].media_type(), ImageMediaType::Png);
    assert_eq!((images[0].width(), images[0].height()), (1, 1));

    connection.finish().await;
}

/// Bytes that contradict the media type declared over them are refused, and no
/// turn runs.
///
/// This is the whole reason the decode happens at the edge. A PNG announced as
/// a JPEG that got through here would be committed to the blob store, recorded
/// in a durable prompt event and only then rejected by a provider — leaving a
/// session whose transcript claims an image the model never saw.
#[tokio::test]
async fn an_image_whose_bytes_contradict_its_declared_type_is_refused() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;

    let (response, notifications) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{
                    "type": "image",
                    "data": BASE64.encode(PNG_1X1),
                    "mimeType": "image/jpeg",
                }],
            }),
        )
        .await;

    let data = response["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("a mislabelled image was not refused: {response}"));
    assert!(
        data.contains("image/jpeg"),
        "the refusal does not say which claim failed: {data}"
    );
    assert!(
        turns.turns().is_empty(),
        "a turn ran on an image whose type its bytes contradict"
    );
    assert!(
        notifications.is_empty(),
        "a refused prompt still published updates: {notifications:?}"
    );

    connection.finish().await;
}

/// An encoding this build admits from no model at all is refused by name, and
/// the sentence says which encodings would have been read.
///
/// Named rather than generic because the client has to be able to convert: a
/// refusal that only says "unsupported" leaves it guessing which of its formats
/// to try next.
#[tokio::test]
async fn an_image_encoding_this_build_cannot_read_is_refused_by_name() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;

    let (response, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{
                    "type": "image",
                    "data": BASE64.encode(b"<svg/>"),
                    "mimeType": "image/svg+xml",
                }],
            }),
        )
        .await;

    let data = response["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("an unreadable encoding was not refused: {response}"));
    assert!(
        data.contains("image/svg+xml") && data.contains("image/png"),
        "the refusal names neither what was sent nor what would be read: {data}"
    );
    assert!(
        turns.turns().is_empty(),
        "a turn ran on an unreadable image"
    );

    connection.finish().await;
}

/// Data that is not base64 at all is refused rather than decoded to rubbish.
///
/// The permissive alternative — dropping the invalid characters and decoding
/// what is left — would hand the blob store a truncated picture under a valid
/// media type, which is exactly the corruption the signature check cannot see.
#[tokio::test]
async fn image_data_that_is_not_base64_is_refused() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;

    let (response, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "image", "data": "not base64!!", "mimeType": "image/png" }],
            }),
        )
        .await;

    let data = response["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("invalid base64 was not refused: {response}"));
    assert!(
        data.contains("base64"),
        "the refusal does not say what was wrong with the data: {data}"
    );
    assert!(turns.turns().is_empty(), "a turn ran on undecodable data");

    connection.finish().await;
}

/// The digest a prompt is admitted under is derived from the request, never a
/// copy of it.
///
/// `RequestDigest` names an identity used to recognise a retry, and it is
/// written into the durable run transaction (`RunCommand::CommandAdmitted`),
/// which the session store serialises to its JSONL. `ego-mcp` builds it by
/// hashing (`run_server.rs` `request_digest`) and `ego-core`'s subagent service
/// builds a bounded `child:<invocation>:<sha256>`; this connection is the one
/// that handed the controller the serialized request body itself, so every ACP
/// prompt was persisted twice — once as `prompt`, once again inside `digest`.
///
/// Text made that a size defect. It stops being only that as soon as a block
/// can carry bytes: `docs/decisions/2026-08-29-images-are-session-blobs-before-
/// they-are-wire-parts.md` rules that durable prompt records carry a `BlobRef`,
/// a media type and dimensions and "never carry bytes", and a digest that is a
/// verbatim copy of the request would write base64 image data straight into the
/// event stream, outside the blob lifecycle that is supposed to own it. So this
/// is a prerequisite for accepting images, not a tidy-up beside it.
///
/// The assertion is about *derivation*, not about a particular hash: a bounded
/// value that does not contain the prompt it identifies. Pinning the exact
/// spelling here would make it a change-detector.
#[tokio::test]
async fn a_prompt_is_admitted_under_a_derived_digest_and_not_its_own_body() {
    const MARKER: &str = "MARKER-THAT-MUST-NOT-BE-COPIED-INTO-THE-DIGEST";

    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;

    connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": MARKER }],
            }),
        )
        .await;

    let digests: Vec<String> = connection
        .store
        .transactions()
        .iter()
        .filter_map(|transaction| match transaction.command() {
            RunCommand::CommandAdmitted { digest, .. } => Some(digest.as_str().to_owned()),
            RunCommand::CancelRequested { .. } | RunCommand::CommandSettled { .. } => None,
        })
        .collect();
    assert!(
        !digests.is_empty(),
        "the prompt was never admitted, so there is no digest to judge"
    );
    for digest in &digests {
        assert!(
            !digest.contains(MARKER),
            "the durable run transaction carries the request body as its digest: {digest}"
        );
        // A derived identity is bounded by construction. Without this, a digest
        // could stop containing the marker and still grow with the request.
        assert!(
            digest.len() <= 128,
            "a derived digest is bounded; this one is {} bytes: {digest}",
            digest.len()
        );
    }

    connection.finish().await;
}

/// A prompt for a session nobody opened is refused, and says how to open one.
#[tokio::test]
async fn a_prompt_for_an_unopened_session_is_refused() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    connection.open(WORKSPACE).await;

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
/// `ARCHITECTURE.md`
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
    let session = connection.open(WORKSPACE).await;

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

/// An internal invariant failure answers an error that says what broke — and
/// says the same thing again when the request is re-delivered.
///
/// Two rules meet here and neither was covered through the real router.
/// `Interrupted` used to share a match arm with `Cancelled`, so a host was
/// told its own cancellation had succeeded at the moment this agent broke.
/// And the sentence explaining the break reached the host only because the
/// handler holds the live execution's error in a channel beside the
/// subscription — a replay has no execution, so that channel is dropped and
/// the retry could only answer a generic sentence. Same request, two different
/// answers, decided by whether it was the first delivery.
///
/// The failure is staged as a misattach because that is what an integrity
/// failure is: the executor answers about a run nobody asked about. An
/// executor *error* would settle uncertain instead and prove the wrong arm.
#[tokio::test]
async fn an_integrity_failure_answers_an_error_and_not_a_cancelled_stop() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;
    turns.misattach_next();

    let (answer, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "go" }],
            }),
        )
        .await;

    assert!(
        answer.get("result").is_none(),
        "an internal failure answered as a successful stop: {answer}"
    );
    assert!(
        answer["error"]["message"]
            .as_str()
            .is_some_and(
                |message| message.contains("the turn failed") && message.contains("another run")
            ),
        "the error does not say why the agent broke: {answer}"
    );
    assert!(
        answer["error"]["data"]
            .as_str()
            .is_some_and(|data| data.contains("another run")),
        "the failure named no cause: {answer}"
    );

    connection.finish().await;
}

/// A re-delivered failed prompt answers what the first delivery answered.
///
/// The handler holds the live execution's error in a channel beside the
/// subscription, so the first delivery can say why the turn failed. A replay
/// has no execution and that channel is gone, so before the settlement carried
/// its own diagnostic the retry could only answer a generic sentence: the same
/// request, two different answers, decided by whether it was the first
/// delivery.
///
/// A continuation and not a start, because a *start* whose receipt failed is
/// refused outright rather than replayed — replay through `prompt_result` is
/// only reachable on a continuation, and that is the path under test.
#[tokio::test]
async fn a_retried_failed_prompt_answers_the_diagnostic_the_first_delivery_gave() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;

    connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "the good turn" }],
            }),
        )
        .await;

    turns.fail_submits();
    let body = serde_json::json!({
        "sessionId": session,
        "prompt": [{ "type": "text", "text": "the failing turn" }],
    });
    let failing = connection.send("session/prompt", body.clone()).await;
    let (answer, _) = connection.response_to(failing).await;
    assert_eq!(
        answer["error"]["data"].as_str(),
        Some("the provider connection dropped"),
        "the first delivery did not carry the cause: {answer}"
    );

    connection.send_as(failing, "session/prompt", body).await;
    let (retried, _) = connection.response_to(failing).await;
    assert!(
        retried.get("result").is_none(),
        "re-delivering a failed request answered as a successful stop: {retried}"
    );
    let message = retried["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the retry carried no explanation: {retried}"));
    assert!(
        message.contains("uncertain"),
        "the retry does not say the outcome is uncertain: {message}"
    );
    assert!(
        message.contains("the provider connection dropped"),
        "the retry answered a generic failure: the reason the first delivery \
         gave this host did not survive on the receipt: {message}"
    );

    connection.finish().await;
}

/// A prompt re-delivered after a later turn replays, and does not run twice.
///
/// The connection recognises a re-delivery by remembering which command each
/// answered prompt named. While it remembered only the most recent one, a
/// client that re-sent a prompt from two turns ago — a reconnect replaying its
/// unacknowledged writes is the ordinary way this happens — was handed a fresh
/// `RequestId`, and the run admitted the same work a second time. That is a
/// paid turn the client never asked for and a second answer to one request.
///
/// The window is sized from `RunRetention::completed_receipts`, which this
/// harness sets to two, so the prompt two turns back is exactly the boundary
/// case: the oldest request the run can still replay. The assertion is that
/// the host ran two turns, not three — a build that answered the re-delivery
/// correctly but ran the turn anyway would still have spent the money.
#[tokio::test]
async fn a_prompt_redelivered_after_a_later_turn_replays_instead_of_running_again() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;

    let body = serde_json::json!({
        "sessionId": session,
        "prompt": [{ "type": "text", "text": "the older turn" }],
    });
    let older = connection.send("session/prompt", body.clone()).await;
    connection.response_to(older).await;
    connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "the newer turn" }],
            }),
        )
        .await;

    connection.send_as(older, "session/prompt", body).await;
    let (replayed, _) = connection.response_to(older).await;
    assert_eq!(
        replayed["result"]["stopReason"], "end_turn",
        "the re-delivered prompt was not answered like the turn it repeats: {replayed}"
    );

    assert_eq!(
        turns
            .turns()
            .into_iter()
            .map(|(prompt, _)| prompt.text().to_owned())
            .collect::<Vec<_>>(),
        vec!["the older turn".to_string(), "the newer turn".to_string()],
        "the re-delivered prompt ran a third turn instead of replaying the \
         receipt the run still retains for it"
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
    let session = connection.open(WORKSPACE).await;

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
            serde_json::json!({ "cwd": OTHER_WORKSPACE, "mcpServers": [] }),
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
    let session = connection.open(WORKSPACE).await;

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

/// `session/new` admits supplied MCP servers into the session-scoped registry.
/// The server must not advertise capabilities that this request did not
/// implement merely because the host supplied an MCP entry.
#[tokio::test]
async fn acp_mcp_server_is_advertised_and_admitted_from_session_new() {
    let admitter = RecordingMcpAdmitter::default();
    let mut connection = Connected::to_with_admitter(
        PromptExecutor::publishing(),
        Arc::new(admitter.clone()) as Arc<dyn McpSessionAdmitter>,
    );
    let (initialized, _) = connection
        .request(
            "initialize",
            serde_json::json!({
                "protocolVersion": ProtocolVersion::V1.as_u16(),
                "clientCapabilities": {},
            }),
        )
        .await;
    assert_eq!(
        initialized["result"]["agentCapabilities"]["mcpCapabilities"]["acp"], true,
        "the host needs the ACP MCP capability before sending its server"
    );

    let (opened, _) = connection
        .request(
            "session/new",
            serde_json::json!({
                "cwd": WORKSPACE,
                "mcpServers": [{
                    "type": "acp",
                    "name": "tuicommander",
                    "serverId": "tuicommander"
                }]
            }),
        )
        .await;
    assert!(opened["result"]["sessionId"].is_string(), "{opened}");
    assert_eq!(admitter.requests().len(), 1);
    connection.finish().await;
}

#[tokio::test]
async fn session_new_accepts_mcp_servers_without_advertising_unsupported_capabilities() {
    let mut connection = Connected::to_with_admitter(
        PromptExecutor::publishing(),
        Arc::new(RecordingMcpAdmitter::default()) as Arc<dyn McpSessionAdmitter>,
    );
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
                "cwd": WORKSPACE,
                "mcpServers": [{
                    "name": "files",
                    "command": MCP_COMMAND,
                    "args": [],
                    "env": [{ "name": "TOKEN", "value": "session-secret" }]
                }],
            }),
        )
        .await;

    assert!(
        response.get("result").is_some(),
        "MCP session was refused: {response}"
    );
    let capabilities = initialized["result"]["agentCapabilities"]
        .as_object()
        .expect("agent capabilities object");
    let mcp = capabilities
        .get("mcpCapabilities")
        .and_then(serde_json::Value::as_object)
        .expect("MCP capabilities object");
    assert_eq!(mcp.get("http"), Some(&serde_json::json!(true)));
    assert_eq!(mcp.get("sse"), Some(&serde_json::json!(false)));
    assert_eq!(mcp.get("acp"), Some(&serde_json::json!(true)));
    assert_eq!(
        capabilities.get("loadSession"),
        Some(&serde_json::json!(false))
    );

    connection.finish().await;
}

#[tokio::test]
async fn session_new_retains_stdio_and_http_transport_material_losslessly() {
    let mut connection = Connected::to_with_admitter(
        PromptExecutor::publishing(),
        Arc::new(RecordingMcpAdmitter::default()) as Arc<dyn McpSessionAdmitter>,
    );
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
                "cwd": WORKSPACE,
                "mcpServers": [
                    {
                        "name": "files",
                        "command": MCP_COMMAND,
                        "args": ["--stdio"],
                        "env": [{ "name": "TOKEN", "value": "session-secret" }]
                    },
                    {
                        "type": "http",
                        "name": "remote",
                        "url": "https://mcp.example.test/mcp",
                        "headers": [{ "name": "Authorization", "value": "Bearer session-secret" }]
                    }
                ],
            }),
        )
        .await;
    assert!(
        response.get("result").is_some(),
        "MCP session was refused: {response}"
    );
    assert!(response["result"]["sessionId"].is_string());

    connection.finish().await;
}

#[tokio::test]
async fn session_new_accepts_exact_loopback_http_and_refuses_other_plaintext_hosts() {
    for url in [
        "http://localhost:8787/mcp",
        "http://127.0.0.1:8787/mcp",
        "http://[::1]:8787/mcp",
    ] {
        let mut connection = Connected::to_with_admitter(
            PromptExecutor::publishing(),
            Arc::new(RecordingMcpAdmitter::default()) as Arc<dyn McpSessionAdmitter>,
        );
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
                    "cwd": WORKSPACE,
                    "mcpServers": [{
                        "type": "http",
                        "name": "local",
                        "url": url,
                        "headers": []
                    }],
                }),
            )
            .await;
        assert!(
            response.get("result").is_some(),
            "exact loopback MCP server {url} was refused: {response}"
        );
        connection.finish().await;
    }

    let mut connection = Connected::to_with_admitter(
        PromptExecutor::publishing(),
        Arc::new(RecordingMcpAdmitter::default()) as Arc<dyn McpSessionAdmitter>,
    );
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
                "cwd": WORKSPACE,
                "mcpServers": [{
                    "type": "http",
                    "name": "remote",
                    "url": "http://192.0.2.1:8787/mcp",
                    "headers": []
                }],
            }),
        )
        .await;
    assert_eq!(
        response["error"]["data"],
        "plaintext HTTP requires allow_insecure_loopback = true"
    );
    connection.finish().await;
}

#[tokio::test]
async fn invalid_supplied_mcp_servers_are_rejected_atomically_before_session_insert() {
    let invalid_servers = [
        serde_json::json!({
            "type": "sse",
            "name": "events",
            "url": "https://mcp.example.test/events",
            "headers": []
        }),
        serde_json::json!({
            "name": "duplicate-env",
            "command": MCP_COMMAND,
            "args": [],
            "env": [
                { "name": "TOKEN", "value": "one" },
                { "name": "token", "value": "two" }
            ]
        }),
        serde_json::json!({
            "type": "http",
            "name": "forbidden-header",
            "url": "https://mcp.example.test/mcp",
            "headers": [{ "name": "Host", "value": "evil.example" }]
        }),
        serde_json::json!({
            "type": "future-transport",
            "name": "future",
            "url": "https://mcp.example.test/mcp"
        }),
    ];

    for server in invalid_servers {
        let mut connection = Connected::to_with_admitter(
            PromptExecutor::publishing(),
            Arc::new(RecordingMcpAdmitter::default()) as Arc<dyn McpSessionAdmitter>,
        );
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
                serde_json::json!({ "cwd": WORKSPACE, "mcpServers": [server] }),
            )
            .await;
        assert!(
            response.get("error").is_some() && response.get("result").is_none(),
            "invalid MCP server was accepted and a session may have been inserted: {response}"
        );
        connection.finish().await;
    }
}

#[tokio::test]
async fn supplied_mcp_authority_survives_prompt_continuations() {
    let host = PromptExecutor::publishing();
    let admitter = RecordingMcpAdmitter::default();
    let mut connection = Connected::to_with_admitter(
        host.clone(),
        Arc::new(admitter.clone()) as Arc<dyn McpSessionAdmitter>,
    );

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

    let (opened, _) = connection
        .request(
            "session/new",
            serde_json::json!({
                "cwd": WORKSPACE,
                "mcpServers": [
                    {
                        "name": "files",
                        "command": MCP_COMMAND,
                        "args": ["--stdio"],
                        "env": [{ "name": "TOKEN", "value": "session-secret" }]
                    },
                    {
                        "type": "http",
                        "name": "remote",
                        "url": "https://mcp.example.test/mcp",
                        "headers": [{ "name": "Authorization", "value": "Bearer session-secret" }]
                    }
                ],
            }),
        )
        .await;
    let session = opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new was refused: {opened}"))
        .to_owned();

    for text in ["first", "second"] {
        let (response, _) = connection
            .request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await;
        assert_eq!(response["result"]["stopReason"], "end_turn");
    }

    let requests = admitter.requests();
    assert_eq!(
        requests.len(),
        1,
        "admission must happen once per ACP session"
    );
    assert_eq!(requests[0].len(), 2);
    let stdio = serde_json::to_value(&requests[0][0]).expect("stdio server serializes");
    assert_eq!(stdio["name"], "files");
    assert_eq!(stdio["command"], MCP_COMMAND);
    assert_eq!(stdio["env"][0]["name"], "TOKEN");
    assert_eq!(stdio["env"][0]["value"], "session-secret");
    let http = serde_json::to_value(&requests[0][1]).expect("HTTP server serializes");
    assert_eq!(http["name"], "remote");
    assert_eq!(http["url"], "https://mcp.example.test/mcp");
    assert_eq!(http["headers"][0]["name"], "Authorization");
    assert_eq!(http["headers"][0]["value"], "Bearer session-secret");

    let bindings = host.mcp_sources();
    assert_eq!(bindings.len(), 2, "both prompts must carry MCP bindings");
    assert_eq!(
        bindings[0], bindings[1],
        "continuations changed live authority"
    );
    assert_eq!(
        bindings[0].as_ref().map(|(_, errors)| errors.as_slice()),
        Some(["safe MCP diagnostic".to_owned()].as_slice()),
        "continuations lost the admitted safe diagnostics"
    );

    connection.finish().await;
}

#[tokio::test]
async fn mcp_admission_failure_leaves_session_unknown_and_uninserted() {
    let mut connection = Connected::to_with_admitter(
        PromptExecutor::publishing(),
        Arc::new(FailingMcpAdmitter) as Arc<dyn McpSessionAdmitter>,
    );
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

    let (rejected, _) = connection
        .request(
            "session/new",
            serde_json::json!({
                "cwd": WORKSPACE,
                "mcpServers": [{
                    "name": "files",
                    "command": MCP_COMMAND,
                    "args": [],
                    "env": []
                }],
            }),
        )
        .await;
    assert!(rejected.get("error").is_some());
    assert!(rejected.get("result").is_none());

    let (unknown, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": "00000000-0000-0000-0000-000000000000",
                "prompt": [{ "type": "text", "text": "must not run" }],
            }),
        )
        .await;
    let data = unknown["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("failed admission inserted a session: {unknown}"));
    assert!(data.contains("session/new"));
    connection.finish().await;
}

/// Catches refusal of unsaved host context and leakage of acquisition secrets.
#[tokio::test]
async fn embedded_host_context_needs_no_disk_file_and_drops_private_metadata() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;
    let (response, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [
                    {"type":"text", "text":"before"},
                    {"type":"resource", "resource": {
                        "uri":"untitled://user:SECRET@editor/unsaved?token=SECRET",
                        "mimeType":"text/plain", "text":"unsaved host buffer",
                        "_meta":{"secret":"SECRET"}
                    }},
                    {"type":"text", "text":"after"}
                ]
            }),
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let prompt = turns.turns()[0].0.text().to_owned();
    assert!(prompt.contains("unsaved host buffer"));
    assert!(prompt.find("before") < prompt.find("unsaved host buffer"));
    assert!(prompt.find("unsaved host buffer") < prompt.find("after"));
    assert!(!prompt.contains("SECRET"));
    connection.finish().await;
}

/// Catches dropping binary host resources or moving images ahead of their text.
#[tokio::test]
async fn embedded_binary_images_keep_their_text_position() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;
    let (response, _) = connection.request("session/prompt", serde_json::json!({
        "sessionId":session, "prompt":[
            {"type":"text", "text":"before é"},
            {"type":"resource", "resource":{"uri":"untitled:picture", "mimeType":"image/png", "blob":BASE64.encode(PNG_1X1)}},
            {"type":"text", "text":"after"},
            {"type":"image", "mimeType":"image/png", "data":BASE64.encode(PNG_1X1)}
        ]
    })).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let prompt = turns.turns()[0].0.clone();
    assert_eq!(prompt.images().len(), 2);
    assert_eq!(prompt.images()[0].bytes(), PNG_1X1);
    let first = prompt.images()[0].text_offset().unwrap();
    assert!(first < prompt.text().len());
    assert_eq!(&prompt.text()[first..], "\nafter");
    assert_eq!(prompt.images()[1].text_offset(), Some(prompt.text().len()));
    connection.finish().await;
}

/// Catches permissive decoding of unsupported, oversized or unlabelled resources.
#[tokio::test]
async fn embedded_resource_bounds_refuse_without_starting_a_turn() {
    let turns = PromptExecutor::publishing();
    let mut connection = Connected::to(turns.clone());
    let session = connection.open(WORKSPACE).await;
    for resource in [
        serde_json::json!({"uri":"untitled:pdf", "mimeType":"application/pdf", "blob":"aGk="}),
        serde_json::json!({"uri":"untitled:unknown", "blob":BASE64.encode(PNG_1X1)}),
        serde_json::json!({"uri":"untitled:bad", "mimeType":"image/png", "blob":"bad!"}),
        serde_json::json!({"uri":"untitled:text", "text":"x".repeat(1024*1024+1)}),
        serde_json::json!({"uri":"x".repeat(4097), "text":"short"}),
        serde_json::json!({"uri":"untitled:text", "mimeType":"application/octet-stream", "text":"short"}),
    ] {
        let (response, _) = connection
            .request(
                "session/prompt",
                serde_json::json!({
                    "sessionId":session, "prompt":[{"type":"resource", "resource":resource}]
                }),
            )
            .await;
        assert!(response.get("error").is_some(), "{response}");
    }
    assert!(turns.turns().is_empty());
    connection.finish().await;
}
