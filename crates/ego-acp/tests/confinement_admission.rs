//! RED contracts for session-scoped confinement admission at the real router.
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

const ROOT: &str = "/tmp/regression case";

#[derive(Debug, Default)]
struct Admission {
    initialized: Mutex<Vec<(SessionId, PathBuf)>>,
    reused: Mutex<Vec<SessionId>>,
    observed: Mutex<Vec<(SessionId, Confinement)>>,
    unsupported: Mutex<Vec<Confinement>>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
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
        self.release.notify_one();
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
        Box::pin(async move {
            if blocked {
                release.notified().await;
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
async fn initialization_is_a_barrier_before_session_new_exposes_controls() {
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
        early.is_err(),
        "session/new exposed its response before admission completed"
    );

    admission.release();
    let (response, _) = tokio::time::timeout(Duration::from_secs(1), connection.response(request))
        .await
        .expect("session/new answered after admission release");
    assert!(response["result"]["configOptions"].is_array(), "{response}");
    connection.abort().await;
}

#[tokio::test]
async fn failed_initialization_publishes_no_session_controls() {
    let fixture = support::DurableAcpFixture::new();
    let admission = Arc::new(Admission::failing());
    let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
    let mut connection =
        support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
    connection.initialize().await;
    let response = connection.new_session(ROOT).await;
    assert!(
        response["error"].is_object(),
        "failed admission was accepted: {response}"
    );
    assert!(
        response["result"].get("configOptions").is_none(),
        "failed admission exposed controls: {response}"
    );
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
