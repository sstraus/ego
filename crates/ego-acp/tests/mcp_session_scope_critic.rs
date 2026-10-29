//! Adversarial contracts for session-scoped MCP admission (regression).
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

/// Read `session/list` page by page until the cursor ends.
async fn list_pages(connection: &mut support::Connected) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(cursor) => serde_json::json!({ "cwd": ROOT, "cursor": cursor }),
            None => serde_json::json!({ "cwd": ROOT }),
        };
        let (response, _) = tokio::time::timeout(WAIT, connection.request("session/list", params))
            .await
            .expect("session/list was answered while an admission was pending");
        pages.push(
            response["result"]["sessions"]
                .as_array()
                .unwrap_or_else(|| panic!("no sessions array: {response}"))
                .iter()
                .map(|session| session["sessionId"].as_str().expect("id").to_owned())
                .collect(),
        );
        cursor = response["result"]["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            return pages;
        }
    }
}

/// The list page size the router uses.
const PAGE: usize = 50;

/// The session id `offset` places away from `from` in id order.
fn shifted(from: SessionId, offset: i128) -> SessionId {
    let bits = from.to_string().replace('-', "");
    let value = u128::from_str_radix(&bits, 16).expect("a session id is 128 bits of hex");
    let moved = value
        .checked_add_signed(offset)
        .expect("the offset stays inside the id space");
    let hex = format!("{moved:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
    .parse()
    .expect("a shifted id is a UUID")
}

/// Catches: the hidden session being filtered out of its page after the page
/// was cut, so a page that ends on it comes back one row short and a host that
/// stops at a short page misses the tail.
///
/// Placement is by construction: equal timestamps list by session id, and
/// the seeded ids are the parked id minus 1..=49 and plus 1..=11, so the
/// hidden session is exactly the 50th row, the last slot of the first page.
#[tokio::test]
async fn a_hidden_session_on_the_page_boundary_does_not_shorten_the_page() {
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| admitter.gate = Some(Arc::clone(&gate)));
    let mut connection = world.connect().await;
    let _parked = connection
        .send_request(
            "session/new",
            serde_json::json!({ "cwd": ROOT, "mcpServers": servers() }),
        )
        .await;
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("admission started");
    let parked = world.admitter.last_session();
    let hidden = parked.to_string();
    for offset in 1..PAGE as u128 {
        world
            .fixture
            .seed_session_with_id(ROOT, shifted(parked, -(offset as i128)));
    }
    for offset in 1..=11 {
        world
            .fixture
            .seed_session_with_id(ROOT, shifted(parked, offset));
    }

    let pages = list_pages(&mut connection).await;
    assert!(
        pages[0].len() >= PAGE,
        "the page ending on the hidden session came back short: {}",
        pages[0].len()
    );
    let all: Vec<String> = pages.concat();
    assert!(!all.contains(&hidden), "the hidden session was listed");
    assert_eq!(all.len(), 60, "a visible row was lost: {}", all.len());
    let distinct: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(distinct.len(), all.len(), "a row was repeated across pages");
    connection.abort().await;
}

/// Catches: a page made only of hidden sessions coming back empty (with or
/// without a cursor), so the visible rows behind it are never reached by a
/// host that stops at an empty page.
///
/// 50 parked `session/new` calls fill the whole first page; 5 seeded
/// sessions with ids just above the largest parked id are the only visible
/// rows.
#[tokio::test]
async fn a_page_made_only_of_hidden_sessions_still_reaches_the_visible_rows() {
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| admitter.gate = Some(Arc::clone(&gate)));
    let mut connection = world.connect().await;
    let mut hidden = Vec::new();
    let mut newest = None;
    for _ in 0..PAGE {
        connection
            .send_request(
                "session/new",
                serde_json::json!({ "cwd": ROOT, "mcpServers": servers() }),
            )
            .await;
        tokio::time::timeout(WAIT, gate.entered.notified())
            .await
            .expect("admission started");
        let parked = world.admitter.last_session();
        newest = newest.max(Some(parked));
        hidden.push(parked.to_string());
    }
    // Every id above the largest parked one, so the 50 hidden rows are the
    // whole first page.
    let newest = newest.expect("sessions were parked");
    for offset in 1..=5 {
        world
            .fixture
            .seed_session_with_id(ROOT, shifted(newest, offset));
    }

    let pages = list_pages(&mut connection).await;
    let all: Vec<String> = pages.concat();
    assert_eq!(
        all.len(),
        5,
        "visible rows behind the hidden page: {pages:?}"
    );
    assert!(
        pages[0].len() == 5,
        "the first page came back short or empty: {pages:?}"
    );
    assert!(
        all.iter().all(|id| !hidden.contains(id)),
        "a hidden session was listed"
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

// ---- round 2 ----------------------------------------------------------------

/// Which request creates the hidden session.
#[derive(Clone, Copy, Debug)]
enum Origin {
    New,
    Fork,
}

/// Park a `session/new` or `session/fork` inside its admission, send `build`'s
/// request against the hidden session, release the admission, and return the
/// answer to the probe, the answer to the parked request and the listing taken
/// afterwards.
async fn probe_hidden(
    origin: Origin,
    build: impl Fn(&str) -> (&'static str, serde_json::Value),
) -> (serde_json::Value, serde_json::Value, Vec<String>, String) {
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| admitter.gate = Some(Arc::clone(&gate)));
    let source = seeded(&world);
    let mut connection = world.connect().await;
    let (method, params) = match origin {
        Origin::New => (
            "session/new",
            serde_json::json!({ "cwd": ROOT, "mcpServers": servers() }),
        ),
        Origin::Fork => (
            "session/fork",
            serde_json::json!({ "sessionId": source, "cwd": ROOT, "mcpServers": servers() }),
        ),
    };
    let pending = connection.send_request(method, params).await;
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("admission started");
    let hidden = world.admitter.last_session().to_string();
    let (probe_method, probe_params) = build(&hidden);
    let probe = tokio::time::timeout(WAIT, connection.request(probe_method, probe_params))
        .await
        .expect("the probe was answered while the admission was pending")
        .0;
    gate.release.notify_one();
    let (parked, _) = connection.response(pending).await;
    let after = listed(&mut connection).await;
    connection.abort().await;
    (probe, parked, after, hidden)
}

fn assert_hidden_untouched(
    label: &str,
    probe: &serde_json::Value,
    parked: &serde_json::Value,
    after: &[String],
    hidden: &str,
) {
    assert!(
        probe["error"].is_object(),
        "{label}: a session still being admitted answered: {probe}"
    );
    assert!(
        parked["result"].is_object(),
        "{label}: the probe broke the parked request: {parked}"
    );
    assert!(
        after.iter().any(|id| id == hidden),
        "{label}: the probe removed the session the parked request opened: {after:?}"
    );
}

/// Catches: a hidden session answering `session/load`, `session/resume`,
/// `session/fork` (as source), `session/delete`, `session/close`,
/// `session/prompt`, `session/set_config_option`, `_ego/pause` or `_ego/compact`
/// because only some of those methods consult the hidden set.
#[tokio::test]
async fn a_hidden_session_is_unusable_through_every_method() {
    type Build = fn(&str) -> (&'static str, serde_json::Value);
    let methods: [(&str, Build); 9] = [
        ("load", |id| {
            (
                "session/load",
                serde_json::json!({ "sessionId": id, "cwd": ROOT, "mcpServers": [] }),
            )
        }),
        ("resume", |id| {
            (
                "session/resume",
                serde_json::json!({ "sessionId": id, "cwd": ROOT, "mcpServers": [] }),
            )
        }),
        ("fork", |id| {
            (
                "session/fork",
                serde_json::json!({ "sessionId": id, "cwd": ROOT, "mcpServers": [] }),
            )
        }),
        ("delete", |id| {
            ("session/delete", serde_json::json!({ "sessionId": id }))
        }),
        ("close", |id| {
            ("session/close", serde_json::json!({ "sessionId": id }))
        }),
        ("prompt", |id| {
            (
                "session/prompt",
                serde_json::json!({ "sessionId": id, "prompt": [{ "type": "text", "text": "hi" }] }),
            )
        }),
        ("set_config_option", |id| {
            (
                "session/set_config_option",
                serde_json::json!({ "sessionId": id, "configId": "sandbox", "value": "workspace" }),
            )
        }),
        ("pause", |id| {
            (
                "_ego/pause",
                serde_json::json!({ "v": 1, "sessionId": id, "requestId": RequestIdText::VALUE }),
            )
        }),
        ("compact", |id| {
            (
                "_ego/compact",
                serde_json::json!({ "v": 1, "sessionId": id, "requestId": RequestIdText::VALUE }),
            )
        }),
    ];
    let mut failures = Vec::new();
    for origin in [Origin::New, Origin::Fork] {
        for (name, build) in methods {
            let (probe, parked, after, hidden) = probe_hidden(origin, build).await;
            let outcome = std::panic::catch_unwind(|| {
                assert_hidden_untouched(name, &probe, &parked, &after, &hidden);
            });
            if outcome.is_err() {
                failures.push(format!("{origin:?}/{name}: probe={probe} parked={parked}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "hidden session reachable:\n{}",
        failures.join("\n")
    );
}

struct RequestIdText;

impl RequestIdText {
    const VALUE: &'static str = "11111111-1111-4111-8111-111111111111";
}

/// Catches: the raw-versus-parsed count of `mcpServers` letting an entry the
/// protocol library dropped (unknown transport, null, nested array) through
/// as an empty selection on any of the four session methods, or refusing a
/// well-formed selection.
#[tokio::test]
async fn a_dropped_mcp_entry_refuses_the_request_on_every_session_method() {
    let valid = servers()[0].clone();
    let cases = [
        (
            "unknown transport",
            serde_json::json!([{ "type": "future", "name": "x" }]),
        ),
        ("null entry", serde_json::json!([null])),
        ("nested array", serde_json::json!([[valid.clone()]])),
        ("scalar entry", serde_json::json!([7])),
        (
            "valid then unknown",
            serde_json::json!([valid.clone(), { "type": "future", "name": "x" }]),
        ),
        (
            "unknown then valid",
            serde_json::json!([{ "type": "future", "name": "x" }, valid.clone()]),
        ),
        ("valid then null", serde_json::json!([valid.clone(), null])),
    ];
    for (label, entries) in cases {
        let world = World::new(|_| {});
        let source = seeded(&world);
        let mut connection = world.connect().await;
        let before = listed(&mut connection).await;
        for method in [
            "session/new",
            "session/load",
            "session/resume",
            "session/fork",
        ] {
            let params = match method {
                "session/new" => serde_json::json!({ "cwd": ROOT, "mcpServers": entries }),
                _ => serde_json::json!({ "sessionId": source, "cwd": ROOT, "mcpServers": entries }),
            };
            let (response, _) = connection.request(method, params).await;
            assert!(
                response["error"].is_object(),
                "{label} on {method} was accepted: {response}"
            );
        }
        assert!(
            world.admitter.contexts.lock().expect("contexts").is_empty(),
            "{label}: admission ran for a refused selection"
        );
        assert_eq!(
            listed(&mut connection).await,
            before,
            "{label}: left a session"
        );
        connection.abort().await;
    }
}

/// Catches: the count check refusing selections that were not dropped:
/// duplicates the library keeps are the admitter's business, an empty list is
/// a session with no servers.
#[tokio::test]
async fn whole_selections_are_not_refused_by_the_dropped_entry_check() {
    let valid = servers()[0].clone();
    for (label, params) in [
        // No "absent" case: ACP schema 1.5.0 makes `mcpServers` a required
        // field of `session/new` and `session/load` (agent.rs:1026, :1175), so
        // the library refuses it before ego sees the request. `resume` and
        // `fork` default it; the dropped-entry check treats those as empty.
        (
            "empty",
            serde_json::json!({ "cwd": ROOT, "mcpServers": [] }),
        ),
        (
            "one stdio",
            serde_json::json!({ "cwd": ROOT, "mcpServers": [valid] }),
        ),
    ] {
        let world = World::new(|_| {});
        let mut connection = world.connect().await;
        let (response, _) = connection.request("session/new", params).await;
        assert!(response["result"].is_object(), "{label}: {response}");
        connection.abort().await;
    }
}

/// Catches: two concurrent `session/new` calls sharing one hidden slot, so the
/// first to finish publishes (or hides) the other, and a list racing both
/// seeing a session that is still being admitted.
#[tokio::test]
async fn two_concurrent_new_sessions_are_hidden_and_published_independently() {
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| admitter.gate = Some(Arc::clone(&gate)));
    let mut connection = world.connect().await;
    let params = serde_json::json!({ "cwd": ROOT, "mcpServers": servers() });
    let first = connection.send_request("session/new", params.clone()).await;
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("first admission started");
    let first_id = world.admitter.last_session().to_string();
    let second = connection.send_request("session/new", params).await;
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("second admission started");
    let second_id = world.admitter.last_session().to_string();
    assert_ne!(first_id, second_id);

    let during = listed(&mut connection).await;
    assert!(
        !during.contains(&first_id) && !during.contains(&second_id),
        "a pending session was listed: {during:?}"
    );

    // Let exactly one finish; the other must stay hidden.
    gate.release.notify_one();
    let (finished, _) = tokio::time::timeout(WAIT, connection.response(first))
        .await
        .expect("the first admission finished");
    assert!(finished["result"].is_object(), "{finished}");
    let midway = listed(&mut connection).await;
    assert!(
        midway.contains(&first_id),
        "published session missing: {midway:?}"
    );
    assert!(
        !midway.contains(&second_id),
        "finishing one session published the other: {midway:?}"
    );
    gate.release.notify_one();
    let (finished, _) = connection.response(second).await;
    assert!(finished["result"].is_object(), "{finished}");
    let after = listed(&mut connection).await;
    assert!(
        after.contains(&first_id) && after.contains(&second_id),
        "{after:?}"
    );
    connection.abort().await;
}

/// Catches: a refused request whose id stays in the hidden set (or one whose
/// drop republishes the deleted session), so a later session cannot reuse the
/// slot or the refused one reappears.
#[tokio::test]
async fn a_refusal_while_another_session_is_pending_does_not_publish_either() {
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| {
        admitter.gate = Some(Arc::clone(&gate));
        admitter.refuse = AtomicBool::new(true);
    });
    let mut connection = world.connect().await;
    let params = serde_json::json!({ "cwd": ROOT, "mcpServers": servers() });
    let first = connection.send_request("session/new", params.clone()).await;
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("first admission started");
    let second = connection.send_request("session/new", params).await;
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("second admission started");
    gate.release.notify_one();
    gate.release.notify_one();
    for id in [first, second] {
        let (response, _) = connection.response(id).await;
        assert!(response["error"].is_object(), "{response}");
    }
    assert_eq!(listed(&mut connection).await, Vec::<String>::new());
    connection.abort().await;
}

/// Catches: failed deletion hiding a refused durable session only in the
/// current connection, so reconnect makes that unadmitted session usable.
#[tokio::test]
async fn refused_admission_with_a_busy_writer_does_not_reappear_after_reconnect() {
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| {
        admitter.gate = Some(Arc::clone(&gate));
        admitter.refuse.store(true, Ordering::SeqCst);
    });
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
    let hidden = world.admitter.last_session().to_string();
    let lease = world.fixture.hold_lease(&hidden);
    gate.release.notify_one();
    let (refused, _) = connection.response(pending).await;
    assert!(refused["error"].is_object(), "{refused}");
    assert!(listed(&mut connection).await.is_empty());
    drop(lease);
    connection.abort().await;

    let mut reconnected = world.connect().await;
    let rows = listed(&mut reconnected).await;
    reconnected.abort().await;
    assert!(
        rows.is_empty(),
        "a refused session survived failed deletion and reappeared after reconnect: {rows:?}"
    );
}

/// Catches: a refused admission silently loses its quarantine failure when
/// another writer also prevents deletion, leaving operators without the error.
#[tokio::test]
async fn refused_admission_with_failed_quarantine_logs_the_double_fault() {
    struct ErrorLog(Arc<Mutex<Vec<String>>>);

    impl tracing::field::Visit for ErrorLog {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .lock()
                .expect("error log")
                .push(format!("{field}={value:?}"));
        }
    }

    impl tracing::Subscriber for ErrorLog {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::ERROR
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() == tracing::Level::ERROR {
                let fields = Arc::new(Mutex::new(Vec::new()));
                event.record(&mut ErrorLog(Arc::clone(&fields)));
                self.0
                    .lock()
                    .expect("error log")
                    .push(fields.lock().expect("error fields").join(" "));
            }
        }
    }

    // The default current-thread runtime also covers the router's spawned tasks.
    let errors = Arc::new(Mutex::new(Vec::new()));
    let _logs = tracing::subscriber::set_default(ErrorLog(Arc::clone(&errors)));
    let gate = Arc::new(Gate::default());
    let world = World::new(|admitter| {
        admitter.gate = Some(Arc::clone(&gate));
        admitter.refuse.store(true, Ordering::SeqCst);
    });
    world.fixture.fail_quarantine();
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
    let hidden = world.admitter.last_session().to_string();
    let lease = world.fixture.hold_lease(&hidden);
    gate.release.notify_one();
    let (refused, _) = connection.response(pending).await;
    assert!(refused["error"].is_object(), "{refused}");
    drop(lease);
    connection.abort().await;

    let errors = errors.lock().expect("error log");
    assert!(
        errors.iter().any(|line| {
            line.contains("a refused session could not be quarantined")
                && line.contains("injected quarantine marker write failure")
                && line.contains(&hidden)
        }),
        "the refused session's quarantine failure was not logged: {errors:?}"
    );
}
