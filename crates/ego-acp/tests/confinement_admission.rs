//! contracts for session-scoped confinement admission at the real router.
//!
//! These tests deliberately exercise the protocol handlers. A fake admission
//! is acceptable here because it records the external admission boundary and
//! supplies deterministic observations; it must never be called directly by
//! a test as a substitute for router behaviour.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_core::config::{Mode, ModelRef};
use ego_core::control::{
    ControlError, ControlSelect, SessionControlChange, SessionControlState, SessionControls,
};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::{
    BackendIdentity, Confinement, ExecCaps, ExecObservation, ExecRequirements, PolicyDigest,
};
use ego_core::run::RunRoot;
use tokio::sync::Notify;

mod support;

#[cfg(not(windows))]
const MCP_COMMAND: &str = "/bin/true";
#[cfg(windows)]
const MCP_COMMAND: &str = r"C:\Windows\System32\cmd.exe";

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/regression case");

#[derive(Debug, Default)]
struct Admission {
    initialized: Mutex<Vec<(SessionId, PathBuf)>>,
    reused: Mutex<Vec<SessionId>>,
    observed: Mutex<Vec<(SessionId, Confinement)>>,
    unsupported: Mutex<Vec<Confinement>>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    released: Arc<AtomicBool>,
    blocked: AtomicBool,
    fail: AtomicBool,
}

impl Admission {
    fn failing() -> Self {
        Self {
            fail: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn blocks(&self) {
        self.blocked.store(true, Ordering::SeqCst);
    }

    async fn entered(&self) {
        self.entered.notified().await;
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }

    fn refuses(&self, confinement: Confinement) {
        self.unsupported
            .lock()
            .expect("unsupported confinement lock")
            .push(confinement);
    }

    fn initialized(&self) -> Vec<(SessionId, PathBuf)> {
        self.initialized
            .lock()
            .expect("initialized admission lock")
            .clone()
    }

    fn reused(&self) -> Vec<SessionId> {
        self.reused.lock().expect("reused admission lock").clone()
    }

    fn observed(&self) -> Vec<(SessionId, Confinement)> {
        self.observed.lock().expect("observation lock").clone()
    }
}

impl ego_acp::serve::SessionConfinementAdmission for Admission {
    fn initialize(
        &self,
        session: SessionId,
        root: RunRoot,
    ) -> ego_acp::serve::SessionConfinementFuture {
        let root_path = root.as_path().to_path_buf();
        let mut initialized = self.initialized.lock().expect("initialized admission lock");
        if initialized
            .iter()
            .any(|(existing, existing_root)| *existing == session && *existing_root == root_path)
        {
            self.reused
                .lock()
                .expect("reused admission lock")
                .push(session);
        } else {
            initialized.push((session, root_path));
        }
        self.entered.notify_one();

        let blocked = self.blocked.load(Ordering::SeqCst);
        let fail = self.fail.load(Ordering::SeqCst);
        let release = Arc::clone(&self.release);
        let released = Arc::clone(&self.released);
        Box::pin(async move {
            if blocked {
                while !released.load(Ordering::SeqCst) {
                    let notified = release.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if !released.load(Ordering::SeqCst) {
                        notified.await;
                    }
                }
            }
            if fail {
                Err(ControlError::Refused {
                    control: "sandbox",
                    why: "the confinement measurement failed".to_owned(),
                })
            } else {
                Ok(())
            }
        })
    }

    fn observation(
        &self,
        session: SessionId,
        confinement: Confinement,
    ) -> Result<ExecObservation, ControlError> {
        self.observed
            .lock()
            .expect("observation lock")
            .push((session, confinement));
        if self
            .unsupported
            .lock()
            .expect("unsupported confinement lock")
            .contains(&confinement)
        {
            let missing = ExecRequirements::new(confinement.required_caps())
                .check(ExecCaps::empty())
                .expect_err("the fixture deliberately has no required capability");
            return Err(ControlError::MissingCapabilities {
                control: "sandbox",
                value: confinement.as_str().to_owned(),
                missing: Box::new(missing.from_backend(BackendIdentity::new("fixture", "103"))),
            });
        }
        Ok(observation(confinement))
    }
}

fn observation(confinement: Confinement) -> ExecObservation {
    let capabilities = match confinement {
        Confinement::Ro => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED),
        Confinement::Workspace => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
        Confinement::Off => ExecCaps::empty(),
    };
    ExecObservation::new(
        BackendIdentity::new("fixture", "103"),
        capabilities,
        PolicyDigest::try_from("0000000000000000000000000000000000000000000000000000000000000000")
            .expect("a valid fixture policy digest"),
    )
}

#[derive(Debug)]
struct Seat {
    current: Confinement,
    available: Vec<Confinement>,
    chosen: Mutex<Option<Confinement>>,
}

impl Seat {
    fn new(current: Confinement) -> Self {
        Self {
            current,
            available: vec![Confinement::Ro, Confinement::Workspace],
            chosen: Mutex::new(None),
        }
    }
}

impl SessionControls for Seat {
    fn state(&self, _session: SessionId) -> Result<SessionControlState, ControlError> {
        let chosen = *self.chosen.lock().expect("seat choice lock");
        Ok(SessionControlState {
            model: ControlSelect {
                current: Some(ModelRef("model/test".to_owned())),
                chosen: None,
                available: vec![ModelRef("model/test".to_owned())],
            },
            effort: ControlSelect {
                current: Some(ThinkingLevel::Off),
                chosen: None,
                available: vec![ThinkingLevel::Off],
            },
            mode: ControlSelect {
                current: Some(Mode::Default),
                chosen: None,
                available: vec![Mode::Default],
            },
            sandbox: ControlSelect {
                current: Some(chosen.unwrap_or(self.current)),
                chosen,
                available: self.available.clone(),
            },
        })
    }

    fn stage(
        &self,
        session: SessionId,
        change: SessionControlChange,
    ) -> Result<SessionControlState, ControlError> {
        let SessionControlChange::Sandbox(confinement) = change else {
            return Err(ControlError::Refused {
                control: "sandbox",
                why: "this fixture only stages sandbox".to_owned(),
            });
        };
        if !self.available.contains(&confinement) {
            return Err(ControlError::Unavailable {
                control: "sandbox",
                value: confinement.as_str().to_owned(),
            });
        }
        *self.chosen.lock().expect("seat choice lock") = Some(confinement);
        self.state(session)
    }
}

fn router(
    fixture: &support::DurableAcpFixture,
    controls: Option<Arc<dyn SessionControls>>,
    admission: Arc<Admission>,
) -> agent_client_protocol::AgentProtocolRouter {
    fixture.router_with_confinement_and_controls(controls, Some(admission))
}

async fn open(connection: &mut support::Connected, root: &str) -> String {
    connection.initialize().await;
    let response = connection.new_session(root).await;
    response["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a durable session id")
        .to_owned()
}

#[tokio::test]
async fn every_durable_session_entry_path_initializes_admission_with_its_id_and_root() {
    let fixture = support::DurableAcpFixture::new();

    let new_admission = Arc::new(Admission::default());
    let new_controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    let mut new_connection = support::Connected::new(router(
        &fixture,
        Some(new_controls),
        Arc::clone(&new_admission),
    ));
    let source = open(&mut new_connection, ROOT).await;
    assert_eq!(
        new_admission.initialized(),
        vec![(source.parse().unwrap(), PathBuf::from(ROOT))]
    );
    new_connection.abort().await;

    for method in ["session/load", "session/resume"] {
        let admission = Arc::new(Admission::default());
        let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
        let mut connection =
            support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
        connection.initialize().await;
        let (response, _) = connection
            .request(
                method,
                serde_json::json!({
                    "sessionId": source,
                    "cwd": ROOT,
                    "mcpServers": [],
                }),
            )
            .await;
        assert!(
            response["result"]["configOptions"].is_array(),
            "{method} failed: {response}"
        );
        assert_eq!(
            admission.initialized(),
            vec![(source.parse().unwrap(), PathBuf::from(ROOT))],
            "{method} used the wrong durable identity or root"
        );
        connection.abort().await;
    }

    let admission = Arc::new(Admission::default());
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    let mut connection =
        support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
    connection.initialize().await;
    let (response, _) = connection
        .request(
            "session/fork",
            serde_json::json!({
                "sessionId": source,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    let child = response["result"]["sessionId"]
        .as_str()
        .expect("session/fork returns a child id")
        .to_owned();
    assert!(
        response["result"]["configOptions"].is_array(),
        "fork failed: {response}"
    );
    assert_eq!(
        admission.initialized(),
        vec![(child.parse().unwrap(), PathBuf::from(ROOT))],
        "fork admission must bind the child, not an unrelated request id"
    );
    connection.abort().await;
}

#[tokio::test]
async fn reconnecting_the_same_session_reuses_its_admitted_snapshot() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    let mut opener = support::Connected::new(router(&fixture, None, Arc::clone(&admission)));
    let session = open(&mut opener, ROOT).await;
    opener.abort().await;

    let mut loader = support::Connected::new(router(&fixture, None, Arc::clone(&admission)));
    loader.initialize().await;
    let (response, _) = loader
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": session,
                "cwd": ROOT,
                "mcpServers": [],
            }),
        )
        .await;
    assert!(
        response["result"].is_object(),
        "session/load failed: {response}"
    );
    assert_eq!(
        admission.initialized(),
        vec![(session.parse().unwrap(), PathBuf::from(ROOT))]
    );
    assert_eq!(admission.reused(), vec![session.parse().unwrap()]);
    loader.abort().await;
}

#[tokio::test]
async fn session_new_settles_before_measurement_and_first_prompt_waits_for_it() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    admission.blocks();
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    let mut connection =
        support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
    connection.initialize().await;
    let request = connection
        .send_request(
            "session/new",
            serde_json::json!({"cwd": ROOT, "mcpServers": []}),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(1), admission.entered())
        .await
        .expect("session/new reached confinement admission");

    let early =
        tokio::time::timeout(Duration::from_millis(100), connection.response(request)).await;
    assert!(
        early.is_ok(),
        "session/new waited for the deliberately held measurement"
    );
    let (created, _) = early.expect("early session/new response");
    assert!(created["result"]["sessionId"].is_string(), "{created}");
    assert!(
        created["result"].get("configOptions").is_none(),
        "{created}"
    );
    let session = created["result"]["sessionId"].as_str().unwrap();
    let prompt = connection
        .send_request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session, "prompt": [{"type": "text", "text": "hello"}],
            }),
        )
        .await;
    let before = fixture.executor_effect_counts();
    let pending =
        tokio::time::timeout(Duration::from_millis(100), connection.response(prompt)).await;
    assert!(
        pending.is_err(),
        "the first prompt crossed the probe barrier"
    );
    assert_eq!(fixture.executor_effect_counts(), before);

    admission.release();
    let (response, notifications) =
        tokio::time::timeout(Duration::from_secs(1), connection.response(prompt))
            .await
            .expect("first prompt answered after admission release");
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let mut updates = notifications;
    updates.extend(connection.announced(ROOT).await);
    assert!(
        updates
            .iter()
            .any(
                |frame| frame["params"]["update"]["sessionUpdate"] == "config_option_update"
                    && frame["params"]["update"]["configOptions"].is_array()
            ),
        "measured offer was not announced: {updates:?}"
    );
    connection.abort().await;
}

#[tokio::test]
async fn closing_during_pending_measurement_cancels_the_first_prompt_without_a_run() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    admission.blocks();
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    let mut connection =
        support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
    let session = open(&mut connection, ROOT).await;
    tokio::time::timeout(Duration::from_secs(1), admission.entered())
        .await
        .expect("session/new started the held measurement");

    let prompt = connection
        .send_request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "should never run"}],
            }),
        )
        .await;
    let (busy, _) = connection
        .request(
            "session/set_config_option",
            serde_json::json!({
                "sessionId": session,
                "configId": "sandbox",
                "value": "workspace",
            }),
        )
        .await;
    assert!(
        busy["error"]["data"]
            .as_str()
            .is_some_and(|message| message.contains("already started")),
        "the first prompt was not admitted before close: {busy}"
    );
    let close = connection
        .send_request("session/close", serde_json::json!({"sessionId": session}))
        .await;
    let (answer, intervening) =
        tokio::time::timeout(Duration::from_secs(1), connection.response(prompt))
            .await
            .expect("close must settle the prompt without waiting for the probe");
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    let closed = if let Some(frame) = intervening
        .iter()
        .find(|frame| frame["id"].as_u64() == Some(close))
    {
        frame.clone()
    } else {
        connection.response(close).await.0
    };
    assert!(closed["result"].is_object(), "{closed}");
    assert_eq!(fixture.executor_effect_counts(), (0, 0, 0));
    assert!(!fixture.has_run(&session));
    admission.release();
    connection.abort().await;
}

#[tokio::test]
async fn failed_initialization_leaves_a_session_but_refuses_its_first_prompt() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::failing());
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    let mut connection =
        support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
    connection.initialize().await;
    let response = connection.new_session(ROOT).await;
    assert!(
        response["result"]["sessionId"].is_string(),
        "session/new did not settle: {response}"
    );
    assert!(
        response["result"].get("configOptions").is_none(),
        "failed admission exposed controls: {response}"
    );
    let session = response["result"]["sessionId"].as_str().unwrap();
    let (prompt, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session, "prompt": [{"type": "text", "text": "hello"}],
            }),
        )
        .await;
    assert!(
        prompt["error"].is_object(),
        "unmeasured prompt ran: {prompt}"
    );
    assert!(fixture.prepared_selections().is_none());
    connection.abort().await;
}

#[tokio::test]
async fn unsupported_default_confinement_refuses_before_executor_admission() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    admission.refuses(Confinement::Workspace);
    let before = fixture.executor_effect_counts();
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Workspace));
    let mut connection =
        support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
    let session = open(&mut connection, ROOT).await;
    let (response, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "hello"}],
            }),
        )
        .await;
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        response["error"].is_object(),
        "unsupported default was accepted: {response}"
    );
    assert!(
        message.contains("sandbox") && message.contains("workspace"),
        "{response}"
    );
    assert!(
        message.contains("write only") || message.contains("missing"),
        "{response}"
    );
    assert_eq!(
        fixture.executor_effect_counts(),
        before,
        "executor effects crossed a refusal"
    );
    assert!(
        fixture.prepared_selections().is_none(),
        "executor.prepare crossed a refusal"
    );
    assert!(
        !fixture.has_run(&session),
        "a refused prompt was durably admitted"
    );
    assert_eq!(
        admission.observed(),
        vec![(session.parse().unwrap(), Confinement::Workspace)]
    );
    connection.abort().await;
}

#[tokio::test]
async fn supported_confinement_observation_is_carried_into_preparation() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    let mut connection =
        support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
    let session = open(&mut connection, ROOT).await;
    let (response, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "hello"}],
            }),
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        fixture
            .prepared_selections()
            .expect("supported prompt reached preparation")
            .sandbox(),
        Some(Confinement::Ro)
    );
    assert_eq!(
        fixture
            .prepared_selections()
            .expect("supported prompt retained its selections")
            .sandbox_observation(),
        Some(&observation(Confinement::Ro)),
        "the measured observation admitted by the router was not carried into preparation"
    );
    assert_eq!(
        admission.observed(),
        vec![(session.parse().unwrap(), Confinement::Ro)]
    );
    connection.abort().await;
}

#[tokio::test]
async fn unsupported_selected_confinement_refuses_before_executor_admission() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    admission.refuses(Confinement::Workspace);
    let before = fixture.executor_effect_counts();
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    let mut connection = support::Connected::new(router(
        &fixture,
        Some(Arc::clone(&controls)),
        Arc::clone(&admission),
    ));
    let session = open(&mut connection, ROOT).await;
    let (staged, _) = connection
        .request(
            "session/set_config_option",
            serde_json::json!({
                "sessionId": session,
                "configId": "sandbox",
                "value": "workspace",
            }),
        )
        .await;
    assert!(
        staged["result"].is_object(),
        "sandbox selection was not staged: {staged}"
    );

    let (response, _) = connection
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "hello"}],
            }),
        )
        .await;
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        response["error"].is_object(),
        "unsupported selection was accepted: {response}"
    );
    assert!(
        message.contains("sandbox") && message.contains("workspace"),
        "{response}"
    );
    assert!(
        message.contains("write only") || message.contains("missing"),
        "{response}"
    );
    assert_eq!(
        fixture.executor_effect_counts(),
        before,
        "executor effects crossed a refusal"
    );
    assert!(
        fixture.prepared_selections().is_none(),
        "executor.prepare crossed a refusal"
    );
    assert!(
        !fixture.has_run(&session),
        "a refused prompt was durably admitted"
    );
    assert_eq!(
        admission.observed(),
        vec![(session.parse().unwrap(), Confinement::Workspace)]
    );
    connection.abort().await;
}

/// What one MCP admission was told, and whether the session's confinement had
/// been measured by the time it was asked.
struct Seen {
    session: SessionId,
    root: PathBuf,
    measured_first: bool,
    level: Option<Confinement>,
    observation: Option<ExecObservation>,
}

/// Records every admission and refuses it, so a test sees the context without
/// building a live MCP runtime.
struct RefusingMcpAdmitter {
    admission: Arc<Admission>,
    seen: Mutex<Vec<Seen>>,
}

impl RefusingMcpAdmitter {
    fn new(admission: Arc<Admission>) -> Arc<Self> {
        Arc::new(Self {
            admission,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl ego_acp::serve::McpSessionAdmitter for RefusingMcpAdmitter {
    fn admit(
        &self,
        _servers: Vec<agent_client_protocol_schema::v1::McpServer>,
        _connection: Option<agent_client_protocol::ConnectionTo<agent_client_protocol::Client>>,
        context: ego_acp::serve::McpAdmissionContext,
    ) -> ego_acp::serve::McpSessionAdmissionFuture {
        let measured_first = self
            .admission
            .initialized()
            .iter()
            .any(|(session, _)| *session == context.session);
        self.seen.lock().expect("seen admissions").push(Seen {
            session: context.session,
            root: context.root.as_path().to_path_buf(),
            measured_first,
            level: context.confinement.as_ref().map(|pick| pick.level),
            observation: context.confinement.and_then(|pick| pick.observation),
        });
        Box::pin(std::future::ready(Err(
            ego_acp::serve::McpSessionAdmissionError::new("refused by the fixture"),
        )))
    }
}

fn mcp_server() -> serde_json::Value {
    serde_json::json!([{
        "name": "files",
        "command": MCP_COMMAND,
        "args": [],
        "env": [],
    }])
}

fn mcp_router(
    fixture: &support::DurableAcpFixture,
    admitter: &Arc<RefusingMcpAdmitter>,
    admission: &Arc<Admission>,
) -> agent_client_protocol::AgentProtocolRouter {
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    fixture.router_with_mcp_admitter(
        Arc::clone(admitter) as Arc<dyn ego_acp::serve::McpSessionAdmitter>,
        Some(controls),
        Some(Arc::clone(admission) as Arc<dyn ego_acp::serve::SessionConfinementAdmission>),
    )
}

async fn listed(connection: &mut support::Connected) -> Vec<String> {
    let (listed, _) = connection
        .request("session/list", serde_json::json!({"cwd": ROOT}))
        .await;
    listed["result"]["sessions"]
        .as_array()
        .expect("session/list rows")
        .iter()
        .map(|row| row["sessionId"].as_str().expect("row id").to_owned())
        .collect()
}

/// Catches: servers admitted (their processes started) before the session's
/// confinement was measured, or without the sandbox the session picked.
#[tokio::test]
async fn every_entry_path_admits_mcp_after_measurement_under_the_picked_sandbox() {
    let fixture = support::DurableAcpFixture::new();
    let mut opener =
        support::Connected::new(router(&fixture, None, Arc::new(Admission::default())));
    let source = open(&mut opener, ROOT).await;
    opener.abort().await;

    for method in [
        "session/new",
        "session/load",
        "session/resume",
        "session/fork",
    ] {
        let admission = Arc::new(Admission::default());
        let admitter = RefusingMcpAdmitter::new(Arc::clone(&admission));
        let mut connection = support::Connected::new(mcp_router(&fixture, &admitter, &admission));
        connection.initialize().await;
        let params = if method == "session/new" {
            serde_json::json!({"cwd": ROOT, "mcpServers": mcp_server()})
        } else {
            serde_json::json!({"sessionId": source, "cwd": ROOT, "mcpServers": mcp_server()})
        };
        let (response, _) = connection.request(method, params).await;
        assert!(response["error"].is_object(), "{method}: {response}");
        let seen = admitter.seen.lock().expect("seen admissions");
        assert_eq!(seen.len(), 1, "{method} admission count");
        let seen = &seen[0];
        assert!(seen.measured_first, "{method} admitted before measurement");
        assert_eq!(seen.level, Some(Confinement::Ro), "{method} sandbox pick");
        assert_eq!(
            seen.observation,
            Some(observation(Confinement::Ro)),
            "{method} observation"
        );
        assert_eq!(seen.root, PathBuf::from(ROOT), "{method} root");
        if method == "session/load" || method == "session/resume" {
            assert_eq!(seen.session, source.parse().unwrap(), "{method} session");
        } else {
            assert_ne!(
                seen.session,
                source.parse().unwrap(),
                "{method} reused the source id"
            );
        }
        connection.abort().await;
    }
}

/// Catches: a refused `session/new` leaving an empty published session the
/// host can list.
#[tokio::test]
async fn refused_mcp_admission_on_session_new_leaves_no_listed_session() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    let admitter = RefusingMcpAdmitter::new(Arc::clone(&admission));
    let mut connection = support::Connected::new(mcp_router(&fixture, &admitter, &admission));
    connection.initialize().await;
    let (response, _) = connection
        .request(
            "session/new",
            serde_json::json!({"cwd": ROOT, "mcpServers": mcp_server()}),
        )
        .await;
    assert!(response["error"].is_object(), "{response}");
    assert_eq!(listed(&mut connection).await, Vec::<String>::new());
    connection.abort().await;
}

/// Catches: a refused `session/fork` leaving its child listed next to the
/// source.
#[tokio::test]
async fn refused_mcp_admission_on_session_fork_lists_only_the_source() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    let admitter = RefusingMcpAdmitter::new(Arc::clone(&admission));
    let mut connection = support::Connected::new(mcp_router(&fixture, &admitter, &admission));
    let source = open(&mut connection, ROOT).await;
    let (response, _) = connection
        .request(
            "session/fork",
            serde_json::json!({"sessionId": source, "cwd": ROOT, "mcpServers": mcp_server()}),
        )
        .await;
    assert!(response["error"].is_object(), "{response}");
    assert_eq!(listed(&mut connection).await, vec![source]);
    connection.abort().await;
}

/// Catches: `session/new` waiting for the sandbox measurement before it
/// admits servers that start no process (an ACP-channel server is what a host
/// chat sends), so every new chat pays the probe for nothing.
#[tokio::test]
async fn a_session_with_no_process_server_is_admitted_without_waiting_for_measurement() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::default());
    admission.blocks();
    let admitter = RefusingMcpAdmitter::new(Arc::clone(&admission));
    let mut connection = support::Connected::new(mcp_router(&fixture, &admitter, &admission));
    connection.initialize().await;
    let (response, _) = tokio::time::timeout(
        Duration::from_secs(5),
        connection.request(
            "session/new",
            serde_json::json!({
                "cwd": ROOT,
                "mcpServers": [{"type": "acp", "name": "tuic", "serverId": "tuic"}],
            }),
        ),
    )
    .await
    .expect("session/new waited for a measurement an ACP-channel server does not need");
    assert!(response["error"].is_object(), "{response}");
    {
        let seen = admitter.seen.lock().expect("seen admissions");
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].measured_first, "the ACP server waited for the probe");
        assert!(seen[0].level.is_none(), "an ACP server was given a sandbox pick");
    }
    admission.release();
    connection.abort().await;
}
