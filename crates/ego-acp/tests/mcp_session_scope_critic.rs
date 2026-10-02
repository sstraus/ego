//! Adversarial contracts for session-scoped MCP admission (regression case).
//!
//! Every request crosses the real router. The admitter, the admission and the
//! seat are fakes of systems the router does not own; each test names the
//! plausible defect it catches.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{Client, ConnectionTo};
use agent_client_protocol_schema::v1;
use ego_acp::serve::{
    McpAdmissionContext, McpSessionAdmissionError, McpSessionAdmissionFuture, McpSessionAdmitter,
    SessionConfinementAdmission, SessionConfinementFuture, SessionMcpAdmission,
};
use ego_core::config::{Mode, ModelRef};
use ego_core::control::{
    ControlError, ControlSelect, SessionControlChange, SessionControlState, SessionControls,
};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::mcp::McpRuntimeBinding;
use ego_core::permit::{
    BackendIdentity, Confinement, ExecCaps, ExecObservation, ExecRequirements, PolicyDigest,
};
use ego_core::run::RunRoot;
use ego_core::turn::{AdmittedToolSource, CapturedToolCatalogue, ToolCatalogueError};
use tokio::sync::Notify;

mod support;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/regression-root");
const WAIT: Duration = Duration::from_secs(5);
#[cfg(not(windows))]
const MCP_COMMAND: &str = "/bin/true";
#[cfg(windows)]
const MCP_COMMAND: &str = r"C:\Windows\System32\cmd.exe";

type Events = Arc<Mutex<Vec<String>>>;

fn servers() -> serde_json::Value {
    serde_json::json!([{
        "name": "supplied",
        "command": MCP_COMMAND,
        "args": [],
        "env": [],
    }])
}

// ---- the confinement admission and seat the router measures against -------

struct Measure {
    events: Events,
    unsupported: Mutex<Vec<Confinement>>,
    fail: AtomicBool,
}

impl Measure {
    fn new(events: &Events) -> Arc<Self> {
        Arc::new(Self {
            events: Arc::clone(events),
            unsupported: Mutex::default(),
            fail: AtomicBool::new(false),
        })
    }
}

impl SessionConfinementAdmission for Measure {
    fn initialize(&self, session: SessionId, _root: RunRoot) -> SessionConfinementFuture {
        self.events
            .lock()
            .expect("events")
            .push(format!("initialize:{session}"));
        let fail = self.fail.load(Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                Err(ControlError::Refused {
                    control: "sandbox",
                    why: "the measurement failed".to_owned(),
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
        self.events
            .lock()
            .expect("events")
            .push(format!("observe:{session}:{confinement}"));
        if self
            .unsupported
            .lock()
            .expect("unsupported")
            .contains(&confinement)
        {
            let missing = ExecRequirements::new(confinement.required_caps())
                .check(ExecCaps::empty())
                .expect_err("the fixture has no capability");
            return Err(ControlError::MissingCapabilities {
                control: "sandbox",
                value: confinement.as_str().to_owned(),
                missing: Box::new(missing.from_backend(BackendIdentity::new("fixture", "test"))),
            });
        }
        Ok(ExecObservation::new(
            BackendIdentity::new("fixture", "test"),
            ExecCaps::empty(),
            PolicyDigest::try_from(
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
            .expect("digest"),
        ))
    }
}

#[derive(Debug)]
struct Seat {
    current: Confinement,
    chosen: Mutex<Option<Confinement>>,
    staged: Mutex<Vec<Confinement>>,
}

impl Seat {
    fn new(current: Confinement) -> Arc<Self> {
        Arc::new(Self {
            current,
            chosen: Mutex::default(),
            staged: Mutex::default(),
        })
    }
}

impl SessionControls for Seat {
    fn state(&self, _session: SessionId) -> Result<SessionControlState, ControlError> {
        let chosen = *self.chosen.lock().expect("chosen");
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
                available: vec![Confinement::Ro, Confinement::Workspace],
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
                why: "sandbox only".to_owned(),
            });
        };
        self.staged.lock().expect("staged").push(confinement);
        *self.chosen.lock().expect("chosen") = Some(confinement);
        self.state(session)
    }
}

// ---- the admitter and the admission the router hands the servers to -------

#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}

struct Admitted {
    pin: Option<Confinement>,
    closes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

struct EmptyTools;

impl AdmittedToolSource for EmptyTools {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        CapturedToolCatalogue::new("regression", Vec::new())
    }
}

struct NoRuntime;

impl McpRuntimeBinding for NoRuntime {
    fn load_pending_foreign_policy(
        &self,
        _continuation_id: &str,
    ) -> Result<Arc<ego_core::turn::GenerationPolicySnapshot>, String> {
        Err("none".to_owned())
    }

    fn bind_pending_foreign_continuation(
        &self,
        _continuation_id: &str,
        _snapshot: Arc<ego_core::turn::GenerationPolicySnapshot>,
    ) -> Result<(), String> {
        Err("none".to_owned())
    }

    fn hook_invocation_port<'a>(
        &self,
        _policy: Arc<ego_core::permit::PolicyEngine>,
        _effects: ego_core::turn::ToolEffectPorts<'a>,
    ) -> Result<Arc<dyn ego_core::hook::HookInvocationPort + 'a>, String> {
        panic!("not exercised")
    }

    fn foreign_question_port<'session, 'turn>(
        &self,
        _runtime: ego_core::hook::SessionHookRuntime<'session>,
        _floor: ego_core::suspension::ResumeFloor,
        _effects: ego_core::turn::ToolEffectPorts<'turn>,
    ) -> Arc<dyn ego_core::mcp::ForeignQuestionPort + 'turn>
    where
        'session: 'turn,
    {
        panic!("not exercised")
    }
}

impl SessionMcpAdmission for Admitted {
    fn admitted_tools(&self) -> Arc<dyn AdmittedToolSource> {
        Arc::new(EmptyTools)
    }

    fn mcp_errors(&self) -> &[String] {
        &[]
    }

    fn mcp_runtime(&self) -> Arc<dyn McpRuntimeBinding> {
        Arc::new(NoRuntime)
    }

    fn pinned_confinement(&self) -> Option<Confinement> {
        self.pin
    }

    fn close(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct Admitter {
    events: Events,
    gate: Option<Arc<Gate>>,
    refuse: AtomicBool,
    pin: Option<Confinement>,
    contexts: Mutex<Vec<(SessionId, Option<Confinement>, bool)>>,
    closes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Admitter {
    fn new(events: &Events) -> Self {
        Self {
            events: Arc::clone(events),
            gate: None,
            refuse: AtomicBool::new(false),
            pin: None,
            contexts: Mutex::default(),
            closes: Arc::default(),
            drops: Arc::default(),
        }
    }

    fn last_session(&self) -> SessionId {
        self.contexts
            .lock()
            .expect("contexts")
            .last()
            .expect("admit was called")
            .0
    }
}

impl McpSessionAdmitter for Admitter {
    fn admit(
        &self,
        _servers: Vec<v1::McpServer>,
        _connection: Option<ConnectionTo<Client>>,
        context: McpAdmissionContext,
    ) -> McpSessionAdmissionFuture {
        let level = context.confinement.as_ref().map(|pick| pick.level);
        self.events.lock().expect("events").push(format!(
            "admit:{}:{}",
            context.session,
            level.map_or("none".to_owned(), |level| level.to_string())
        ));
        self.contexts.lock().expect("contexts").push((
            context.session,
            level,
            context
                .confinement
                .as_ref()
                .is_some_and(|pick| pick.observation.is_some()),
        ));
        let gate = self.gate.clone();
        let refuse = self.refuse.load(Ordering::SeqCst);
        let admission = Admitted {
            pin: self.pin,
            closes: Arc::clone(&self.closes),
            drops: Arc::clone(&self.drops),
        };
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            if refuse {
                return Err(McpSessionAdmissionError::new("the server was refused"));
            }
            Ok(Some(Arc::new(admission) as Arc<dyn SessionMcpAdmission>))
        })
    }
}

// ---- harness ---------------------------------------------------------------

struct World {
    fixture: support::DurableAcpFixture,
    events: Events,
    admitter: Arc<Admitter>,
    measure: Arc<Measure>,
    seat: Arc<Seat>,
}

impl World {
    fn new(configure: impl FnOnce(&mut Admitter)) -> Self {
        let events: Events = Arc::default();
        let mut admitter = Admitter::new(&events);
        configure(&mut admitter);
        Self {
            fixture: support::DurableAcpFixture::new(),
            measure: Measure::new(&events),
            seat: Seat::new(Confinement::Ro),
            admitter: Arc::new(admitter),
            events,
        }
    }

    async fn connect(&self) -> support::Connected {
        let mut connection = support::Connected::new(self.fixture.router_with_mcp_admitter(
            self.admitter.clone(),
            Some(self.seat.clone()),
            Some(self.measure.clone()),
        ));
        connection.initialize().await;
        connection
    }
}

async fn listed(connection: &mut support::Connected) -> Vec<String> {
    let (response, _) = tokio::time::timeout(
        WAIT,
        connection.request("session/list", serde_json::json!({ "cwd": ROOT })),
    )
    .await
    .expect("session/list was answered while an admission was pending");
    response["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("no sessions array: {response}"))
        .iter()
        .map(|session| session["sessionId"].as_str().expect("id").to_owned())
        .collect()
}

async fn open_with_servers(
    connection: &mut support::Connected,
    method: &str,
    id: &str,
) -> serde_json::Value {
    let params = match method {
        "session/new" => serde_json::json!({ "cwd": ROOT, "mcpServers": servers() }),
        _ => serde_json::json!({ "sessionId": id, "cwd": ROOT, "mcpServers": servers() }),
    };
    connection.request(method, params).await.0
}

fn seeded(world: &World) -> String {
    world
        .fixture
        .seed_replay_session(ROOT, "Source", 200_000)
        .session_id
}

// ---- cases ------------------------------------------------------------------

/// Catches: `session/new` publishing the session before its servers are
/// admitted, so a second request sees a session that may yet be refused.
#[tokio::test]
async fn unadmitted_new_session_is_not_listed_while_its_servers_are_admitted() {
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| admitter.gate = Some(Arc::clone(&gate)));
    let mut connection = world.connect().await;

    let pending = connection
        .send_request(
            "session/new",
            serde_json::json!({ "cwd": ROOT, "mcpServers": servers() }),
        )
        .await;
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("admission started");
    let session = world.admitter.last_session().to_string();
    let during = listed(&mut connection).await;
    gate.release.notify_one();
    let (response, _) = connection.response(pending).await;

    assert!(response["result"].is_object(), "{response}");
    assert!(
        !during.contains(&session),
        "a session whose servers were not yet admitted was listed: {during:?}"
    );
    connection.abort().await;
}

/// Catches: `session/fork` publishing the child before its servers are
/// admitted, so a client can list or load a child that is about to be taken
/// back.
#[tokio::test]
async fn unadmitted_fork_child_is_not_listed_while_its_servers_are_admitted() {
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| admitter.gate = Some(Arc::clone(&gate)));
    let source = seeded(&world);
    let mut connection = world.connect().await;

    let pending = connection
        .send_request(
            "session/fork",
            serde_json::json!({ "sessionId": source, "cwd": ROOT, "mcpServers": servers() }),
        )
        .await;
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("admission started");
    let child = world.admitter.last_session().to_string();
    let during = listed(&mut connection).await;
    gate.release.notify_one();
    let (response, _) = connection.response(pending).await;

    assert!(response["result"].is_object(), "{response}");
    assert!(
        !during.contains(&child),
        "a fork child whose servers were not yet admitted was listed: {during:?}"
    );
    connection.abort().await;
}

/// Catches: a refused admission leaving the session or the fork child behind,
/// listed and loadable, with no servers.
#[tokio::test]
async fn refused_admission_leaves_no_listed_session_for_new_and_fork() {
    let world = World::new(|admitter| admitter.refuse = AtomicBool::new(true));
    let source = seeded(&world);
    let mut connection = world.connect().await;
    let before = listed(&mut connection).await;

    for (method, params) in [
        (
            "session/new",
            serde_json::json!({ "cwd": ROOT, "mcpServers": servers() }),
        ),
        (
            "session/fork",
            serde_json::json!({ "sessionId": source, "cwd": ROOT, "mcpServers": servers() }),
        ),
    ] {
        let (response, _) = connection.request(method, params).await;
        assert!(response["error"].is_object(), "{method}: {response}");
        let refused = world.admitter.last_session().to_string();
        let after = listed(&mut connection).await;
        assert_eq!(after, before, "{method} left a listed session: {after:?}");
        let (loaded, _) = connection
            .request(
                "session/load",
                serde_json::json!({ "sessionId": refused, "cwd": ROOT, "mcpServers": [] }),
            )
            .await;
        assert!(
            loaded["error"].is_object(),
            "{method}: the refused session is still loadable: {loaded}"
        );
    }
    connection.abort().await;
}

/// Catches: a measurement that fails (or a pick the backend cannot enforce)
/// still reaching admission, which would start the servers unconfined, or
/// leaving the session behind.
#[tokio::test]
async fn failed_measurement_never_reaches_admission_and_leaves_no_session() {
    for unsupported in [false, true] {
        let world = World::new(|_| {});
        if unsupported {
            world
                .measure
                .unsupported
                .lock()
                .expect("unsupported")
                .push(Confinement::Ro);
        } else {
            world.measure.fail.store(true, Ordering::SeqCst);
        }
        let source = seeded(&world);
        let mut connection = world.connect().await;
        let before = listed(&mut connection).await;
        for (method, params) in [
            (
                "session/new",
                serde_json::json!({ "cwd": ROOT, "mcpServers": servers() }),
            ),
            (
                "session/fork",
                serde_json::json!({ "sessionId": source, "cwd": ROOT, "mcpServers": servers() }),
            ),
            (
                "session/load",
                serde_json::json!({ "sessionId": source, "cwd": ROOT, "mcpServers": servers() }),
            ),
            (
                "session/resume",
                serde_json::json!({ "sessionId": source, "cwd": ROOT, "mcpServers": servers() }),
            ),
        ] {
            let (response, _) = connection.request(method, params).await;
            assert!(
                response["error"].is_object(),
                "{method} (unsupported={unsupported}) survived: {response}"
            );
        }
        assert!(
            world.admitter.contexts.lock().expect("contexts").is_empty(),
            "admission ran although confinement could not be measured (unsupported={unsupported})"
        );
        assert_eq!(listed(&mut connection).await, before);
        connection.abort().await;
    }
}

/// Catches: admission before measurement, or a context that carries a level
/// or observation other than the session's own pick, on every entry path.
#[tokio::test]
async fn every_entry_path_measures_before_admitting_with_the_sessions_pick() {
    for method in [
        "session/new",
        "session/load",
        "session/resume",
        "session/fork",
    ] {
        let world = World::new(|_| {});
        let source = seeded(&world);
        let mut connection = world.connect().await;
        let response = open_with_servers(&mut connection, method, &source).await;
        assert!(response["result"].is_object(), "{method}: {response}");

        let (session, level, observed) = world
            .admitter
            .contexts
            .lock()
            .expect("contexts")
            .last()
            .cloned()
            .expect("admitted");
        assert_eq!(level, Some(Confinement::Ro), "{method}");
        assert!(observed, "{method}: the context carried no observation");
        let events = world.events.lock().expect("events").clone();
        let measured = events
            .iter()
            .position(|event| event == &format!("initialize:{session}"))
            .unwrap_or_else(|| panic!("{method}: never measured {session}: {events:?}"));
        let admitted = events
            .iter()
            .position(|event| event.starts_with(&format!("admit:{session}")))
            .expect("admitted");
        assert!(measured < admitted, "{method}: admitted first: {events:?}");
        connection.abort().await;
    }
}

/// Catches: `session/close` dropping the registration without reaching the
/// admission, so its stdio scope outlives the closed session.
#[tokio::test]
async fn closing_a_session_closes_its_admission_on_every_entry_path() {
    for method in [
        "session/new",
        "session/load",
        "session/resume",
        "session/fork",
    ] {
        let world = World::new(|_| {});
        let source = seeded(&world);
        let mut connection = world.connect().await;
        let response = open_with_servers(&mut connection, method, &source).await;
        let session = response["result"]["sessionId"]
            .as_str()
            .unwrap_or(&source)
            .to_owned();
        let (closed, _) = connection
            .request("session/close", serde_json::json!({ "sessionId": session }))
            .await;
        assert!(closed["result"].is_object(), "{method}: {closed}");
        assert_eq!(
            world.admitter.closes.load(Ordering::SeqCst),
            1,
            "{method}: close did not reach the admission exactly once"
        );
        assert_eq!(
            world.admitter.drops.load(Ordering::SeqCst),
            1,
            "{method}: the admission outlived its closed session"
        );
        connection.abort().await;
    }
}

/// Catches: a re-load that replaces a session's registration and leaves the
/// previous admission (and its confined scope) alive.
#[tokio::test]
async fn a_reloaded_session_releases_the_admission_it_replaces() {
    let world = World::new(|_| {});
    let source = seeded(&world);
    let mut connection = world.connect().await;
    for _ in 0..2 {
        let response = open_with_servers(&mut connection, "session/load", &source).await;
        assert!(response["result"].is_object(), "{response}");
    }
    assert_eq!(
        world.admitter.drops.load(Ordering::SeqCst),
        1,
        "the first load's admission is still alive after the second replaced it"
    );
    connection.abort().await;
}

async fn pick(
    connection: &mut support::Connected,
    session: &str,
    value: &str,
) -> serde_json::Value {
    connection
        .request(
            "session/set_config_option",
            serde_json::json!({ "sessionId": session, "configId": "sandbox", "value": value }),
        )
        .await
        .0
}

/// Catches: a later sandbox pick that differs from the one the servers were
/// admitted under being staged, so the run would use a confinement the
/// servers were not measured for.
#[tokio::test]
async fn a_pick_other_than_the_admitted_sandbox_is_refused_and_not_staged() {
    for method in [
        "session/new",
        "session/load",
        "session/resume",
        "session/fork",
    ] {
        let world = World::new(|admitter| admitter.pin = Some(Confinement::Ro));
        let source = seeded(&world);
        let mut connection = world.connect().await;
        let response = open_with_servers(&mut connection, method, &source).await;
        let session = response["result"]["sessionId"]
            .as_str()
            .unwrap_or(&source)
            .to_owned();

        let refused = pick(&mut connection, &session, "workspace").await;
        assert!(refused["error"].is_object(), "{method}: {refused}");
        assert!(
            world.seat.staged.lock().expect("staged").is_empty(),
            "{method}: a refused pick reached the seat"
        );
        let same = pick(&mut connection, &session, "ro").await;
        assert!(
            same["result"].is_object(),
            "{method}: re-picking the admitted sandbox must stay allowed: {same}"
        );
        connection.abort().await;
    }
}

/// Catches: the pin applied to sessions that run no stdio server, which
/// would remove the sandbox picker from HTTP-only and server-less sessions.
#[tokio::test]
async fn an_admission_without_a_pin_leaves_the_sandbox_pick_free() {
    let world = World::new(|_| {});
    let mut connection = world.connect().await;
    let opened = open_with_servers(&mut connection, "session/new", "").await;
    let session = opened["result"]["sessionId"]
        .as_str()
        .expect("id")
        .to_owned();
    let picked = pick(&mut connection, &session, "workspace").await;
    assert!(picked["result"].is_object(), "{picked}");

    let bare = connection.new_session(ROOT).await;
    let bare = bare["result"]["sessionId"].as_str().expect("id").to_owned();
    let picked = pick(&mut connection, &bare, "workspace").await;
    assert!(picked["result"].is_object(), "{picked}");
    connection.abort().await;
}
