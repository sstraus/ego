//! contracts for the CLI's measured ACP confinement adapter.
//! The CLI adapter measures through `RunEffectPorts`, retains confinement
//! observations for the durable session and reports typed capability refusals
//! through the existing controls port. Router ordering and ceiling policy
//! remain owned by their existing collaborators.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod support;

#[cfg(target_os = "linux")]
#[path = "../../ego-acp/tests/support/mod.rs"]
mod acp_support;

use std::path::Path;
use std::sync::Arc;

use ego_acp::serve::SessionConfinementAdmission;
use ego_cli::acp_confinement::CliSessionConfinementAdmission;
use ego_cli::acp_controls::AcpSessionControls;
use ego_core::config::{Mode, ModelRef};
use ego_core::control::{ControlError, SessionControls};
use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::id::SessionId;
use ego_core::evidence::{EvidenceLogSource, EvidenceRecord};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::{Confinement, ExecObservation, MissingExecCaps};
use ego_core::run::RunRoot;
use ego_core::store::blob::BlobStore;
use ego_core::turn::CancelToken;
use ego_tools::store::{JsonlStore, OsIo, SessionEvidenceStore};
use support::{AcpHost, Ego, Plan, Reply, Stub, text, tool_call};

/// Host the production Landlock dispatcher when the backend re-execs this binary.
/// Without it, libtest exits successfully without running the probe target,
/// so allowed controls cannot demonstrate any confinement capability.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "hosts the Landlock helper; not a test of its own"]
fn __ego_internal_landlock_exec_v1() {
    if std::env::args_os().nth(7).is_some_and(|workspace| {
        Path::new(&workspace)
            .join(".ego-test-helper-failure")
            .exists()
    }) {
        eprintln!("injected capability helper execution failure");
        std::process::exit(101);
    }
    let refused = ego_tools::exec::dispatch_landlock_helper(std::env::args_os())
        .expect("libtest was handed the helper envelope but did not recognise it");
    panic!("the Landlock helper refused execution: {refused:?}");
}

fn seat(ceiling: Confinement) -> AcpSessionControls {
    AcpSessionControls::seeded(
        Some(ModelRef("fixture/model".to_owned())),
        Some(ThinkingLevel::Off),
        Mode::Default,
        false,
        ceiling,
        None,
    )
}

/// Ask the real CLI adapter for every confinement under the process ceiling.
/// Unsupported values stay typed; they are not silently omitted before the
/// controls policy gets a chance to explain the refusal.
async fn measured_rows(
    admission: &CliSessionConfinementAdmission,
    session: SessionId,
) -> (
    Vec<(Confinement, ExecObservation)>,
    Vec<(Confinement, MissingExecCaps)>,
) {
    let mut supported = Vec::new();
    let mut unsupported = Vec::new();
    for confinement in [Confinement::Ro, Confinement::Workspace] {
        match admission.observation(session, confinement) {
            Ok(observation) => supported.push((confinement, observation)),
            Err(ControlError::MissingCapabilities {
                control: "sandbox",
                value,
                missing,
            }) => {
                assert_eq!(value, confinement.as_str());
                unsupported.push((confinement, *missing));
            }
            Err(error) => panic!(
                "the adapter returned an untyped or unrelated confinement error for `{confinement}`: {error}"
            ),
        }
    }
    (supported, unsupported)
}

// Direct adapter calls must resolve operator config in an isolated process,
// just like the stdio fixtures, rather than inheriting the runner's ~/Gits root.
fn isolated_adapter_test(name: &str) -> bool {
    if std::env::var_os("EGO_ISOLATED_ADAPTER_TEST").is_some() {
        return false;
    }
    let home = tempfile::tempdir().expect("isolated adapter home");
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", name, "--nocapture"])
        .env("EGO_ISOLATED_ADAPTER_TEST", "1")
        .env("HOME", home.path())
        .env("EGO_HOME", home.path())
        .output()
        .expect("isolated adapter process");
    assert!(output.status.success(), "{output:?}");
    true
}

// Catches: a real helper failure leaves an ACP host waiting forever for its config offer.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_filesystem_probe_publishes_terminal_acp_config_update() {
    if isolated_adapter_test("failed_filesystem_probe_publishes_terminal_acp_config_update") {
        return;
    }
    let workspace_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let read_root = tempfile::tempdir().unwrap();
    let home = std::env::var_os("EGO_HOME").unwrap();
    // Read-only roots select Landlock rather than the host's available bwrap backend.
    std::fs::write(
        Path::new(&home).join("config.toml"),
        format!(
            "roots = [{{ path = {:?}, access = 'read' }}]\n",
            read_root.path()
        ),
    )
    .unwrap();
    std::fs::write(workspace.join(".ego-test-helper-failure"), "fail helper").unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = Arc::new(JsonlStore::new(sessions.path(), &workspace, Box::new(OsIo)));
    let controls = Arc::new(seat(Confinement::Workspace));
    let admission = Arc::new(CliSessionConfinementAdmission::new(
        store,
        Arc::clone(&controls),
        Confinement::Workspace,
        CancelToken::new(),
    ));
    let fixture = acp_support::DurableAcpFixture::new();
    let mut connection = acp_support::Connected::new(
        fixture.router_with_confinement_and_controls(Some(controls), Some(admission)),
    );
    connection.initialize().await;
    let opened = connection.new_session(workspace.to_str().unwrap()).await;
    let session = opened["result"]["sessionId"].as_str().unwrap();
    assert!(opened["result"].get("configOptions").is_none(), "{opened}");
    let (prompt, notifications) = connection.request(
        "session/prompt",
        serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "hello"}]}),
    ).await;
    assert_eq!(prompt["error"]["code"], -32603, "{prompt}");
    assert!(
        prompt["error"]["data"]
            .as_str()
            .unwrap()
            .contains("injected capability helper execution failure"),
        "the real helper failure must reach admission: {prompt}"
    );
    assert_eq!(fixture.executor_effect_counts(), (0, 0, 0));
    let update = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut frames = notifications.into_iter();
        loop {
            let frame = match frames.next() {
                Some(frame) => frame,
                None => connection.frame().await,
            };
            if frame["params"]["update"]["sessionUpdate"] == "config_option_update" {
                break frame;
            }
        }
    })
    .await
    .expect("failed probe left ACP config offer pending forever");
    assert_eq!(update["params"]["sessionId"], session, "{update}");
    let update = &update["params"]["update"];
    assert_eq!(update["configOptions"], serde_json::json!([]), "{update}");
    assert_eq!(update["_meta"]["ego"]["error"]["code"], -32603, "{update}");
    assert!(
        update["_meta"]["ego"]["error"]["data"]
            .as_str()
            .unwrap()
            .contains("injected capability helper execution failure"),
        "the terminal update must explain the real measurement failure: {update}"
    );
    connection.abort().await;
}

#[tokio::test]
async fn cli_adapter_measures_the_offer_and_preserves_typed_unsupported_rows() {
    if isolated_adapter_test("cli_adapter_measures_the_offer_and_preserves_typed_unsupported_rows")
    {
        return;
    }
    let sessions = tempfile::tempdir().expect("session root");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let session = SessionId::new();
    let root = RunRoot::new(&workspace_root).expect("absolute workspace");
    let store = Arc::new(JsonlStore::new(
        sessions.path(),
        &workspace_root,
        Box::new(OsIo),
    ));
    let controls = Arc::new(seat(Confinement::Workspace));
    let admission = CliSessionConfinementAdmission::new(
        Arc::clone(&store),
        Arc::clone(&controls),
        Confinement::Workspace,
        CancelToken::new(),
    );

    admission
        .initialize(session, root)
        .await
        .expect("session measurement completes");
    let (supported, unsupported) = measured_rows(&admission, session).await;
    eprintln!("measured confinement: supported={supported:?}, unsupported={unsupported:?}");
    assert!(
        !supported.is_empty(),
        "the real backend measured no confinement under the workspace ceiling"
    );

    let state = controls
        .state(session)
        .expect("controls state remains readable");
    assert_eq!(
        state.sandbox.available,
        supported
            .iter()
            .map(|(confinement, _)| *confinement)
            .collect::<Vec<_>>(),
        "the offer must contain measured candidates only, not Confinement::ALL"
    );
    for (confinement, _) in unsupported {
        let refusal = controls
            .stage(
                session,
                ego_core::control::SessionControlChange::Sandbox(confinement),
            )
            .expect_err("an unsupported measured confinement must remain refused");
        assert!(
            matches!(
                refusal,
                ControlError::MissingCapabilities {
                    control: "sandbox",
                    ..
                }
            ),
            "unsupported `{confinement}` lost its typed missing-capability reason: {refusal}"
        );
    }
}

// Catches: offer initialization admits a candidate authority or loses its probe evidence.
#[tokio::test]
async fn cli_adapter_commits_measurement_evidence_before_publishing_the_offer() {
    if isolated_adapter_test("cli_adapter_commits_measurement_evidence_before_publishing_the_offer")
    {
        return;
    }
    let sessions = tempfile::tempdir().expect("session root");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let session = SessionId::new();
    let root = RunRoot::new(&workspace_root).expect("absolute workspace");
    let store = Arc::new(JsonlStore::new(
        sessions.path(),
        &workspace_root,
        Box::new(OsIo),
    ));
    let controls = Arc::new(seat(Confinement::Workspace));
    let admission = CliSessionConfinementAdmission::new(
        Arc::clone(&store),
        Arc::clone(&controls),
        Confinement::Workspace,
        CancelToken::new(),
    );

    admission
        .initialize(session, root)
        .await
        .expect("session measurement completes");
    let evidence = sessions.path().join(session.to_string()).join("evidence");
    assert!(
        evidence_exists(&evidence),
        "the offer was published without a committed probe transcript in the session CAS"
    );
    assert!(
        !controls
            .state(session)
            .expect("controls state after initialization")
            .sandbox
            .available
            .is_empty(),
        "controls were not seeded after the observation event was committed"
    );
    let events = sessions
        .path()
        .join(session.to_string())
        .join("events.jsonl");
    let events = std::fs::read_to_string(events).expect("initialization event log");
    assert!(
        events.contains("probe_evidence"),
        "the durable observation event must carry the probe reference"
    );
    let opened = ego_cli::session::open(store.as_ref(), &session, &ego_tools::clock::SystemClock)
        .expect("reopen measured session");
    assert!(
        opened.state.execution_observation.is_none(),
        "an offer is not admission"
    );
}

fn evidence_exists(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "blob"))
        })
        .unwrap_or(false)
}

fn write_plan() -> Plan {
    let arguments = serde_json::json!({
        "argv": [
            "/bin/sh",
            "-c",
            "printf confined-write > \"$1\"",
            "ego-confinement",
            "confinement-marker",
        ],
        "cwd": null,
        "timeout_ms": 1000,
    });
    Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "confinement-write",
                "exec",
                &arguments.to_string(),
            )),
            Reply::Sse(text("write attempted")),
        ],
        ..Plan::default()
    }
}

fn assert_probe_is_durable(ego: &Ego, session: &str, confinement: &str) {
    let events = ego
        .home_path()
        .join("sessions")
        .join(session)
        .join("events.jsonl");
    let events = std::fs::read_to_string(&events).expect("durable session events");
    let (_observation, reference) = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("event JSON"))
        .filter_map(|record| record["payload"]["event"].as_object().cloned())
        .map(|event| {
            serde_json::from_value::<DomainEvent>(serde_json::Value::Object(event))
                .expect("domain event")
        })
        .find_map(|event| match event.kind {
            EventKind::ExecutionObserved {
                observation,
                probe_evidence,
            } => Some((observation, probe_evidence.expect("probe reference"))),
            _ => None,
        })
        .expect("durable execution observation");
    let session_id = session.parse().expect("session id");
    let selected = JsonlStore::new(
        ego.home_path().join("sessions"),
        ego.workspace_path(),
        Box::new(OsIo),
    )
    .evidence_records(&session_id)
    .expect("durable evidence records")
    .into_iter()
    .find_map(|record| match record {
        EvidenceRecord::RunContext(context) => Some(context.execution().confinement()),
        _ => None,
    })
    .expect("durable run context evidence");
    assert_eq!(selected.as_str(), confinement, "selected policy authority");
    let store =
        SessionEvidenceStore::open(ego.home_path().join("sessions"), session_id, 1024 * 1024)
            .expect("evidence store");
    let bytes = BlobStore::read(&store, &reference.blob_ref()).expect("probe blob retained");
    assert!(!bytes.is_empty(), "probe blob was empty");
}

#[test]
fn the_same_acp_prompt_reaches_a_workspace_write_and_refuses_it_under_ro() {
    for confinement in ["workspace", "ro"] {
        let target_name = "confinement-marker";
        let stub = Stub::start(write_plan());
        let ego = Ego::against(&stub);
        let target = ego.workspace_path().join(target_name);
        ego.home_file(
            "config.toml",
            "model = \"ollama/qwen3\"\nthinking = \"off\"\npermissions.allow = [\"exec(/bin/sh *)\"]\n",
        );
        let workspace = ego.workspace_path().to_owned();
        let running = Ego::at(ego.home_path(), &workspace, Some(&stub))
            .subcommand("acp")
            .args(["--sandbox", confinement])
            .spawn_conversing();
        let mut host = AcpHost::adopt(running);
        host.initialize();
        let cwd = workspace.to_string_lossy().into_owned();
        let session = host.new_session(&cwd);
        let answered = host.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "write the marker file"}],
            }),
        );
        let stderr = host.finish();
        assert_probe_is_durable(&ego, &session, confinement);
        assert_eq!(
            stub.generations().len(),
            2,
            "provider/tool loop changed: {stderr}"
        );
        if confinement == "workspace" {
            assert_eq!(
                std::fs::read_to_string(&target).expect("workspace write"),
                "confined-write"
            );
            assert_eq!(answered["result"]["stopReason"], "end_turn");
            assert_eq!(durable_tool_result(&ego, &session), Some(true));
        } else {
            assert!(
                !target.exists(),
                "ro confinement allowed the write: {answered}"
            );
            assert_eq!(answered["result"]["stopReason"], "end_turn");
            assert_eq!(durable_tool_result(&ego, &session), Some(false));
        }
    }
}

fn durable_tool_result(ego: &Ego, session: &str) -> Option<bool> {
    let path = ego
        .home_path()
        .join("sessions")
        .join(session)
        .join("events.jsonl");
    std::fs::read_to_string(path)
        .expect("durable session events")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|record| record["payload"]["event"].as_object().cloned())
        .filter_map(|event| {
            serde_json::from_value::<DomainEvent>(serde_json::Value::Object(event)).ok()
        })
        .find_map(|event| match event.kind {
            EventKind::ToolCallCompleted { ok, .. } => Some(ok),
            _ => None,
        })
}

// Catches: ACP measures a bare policy instead of the resolved default/configured
// filesystem authority, refusing the first prompt before the provider is reached.
#[test]
fn fresh_acp_prompt_preserves_resolved_filesystem_authority() {
    for policy in ["default", "empty", "read", "exec"] {
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(text("first prompt reached provider"))],
            ..Plan::default()
        });
        let ego = Ego::against(&stub);
        let home = tempfile::tempdir().expect("operator home distinct from EGO_HOME");
        std::fs::create_dir(home.path().join("Gits")).expect("default root");
        let extra = ego.home_path().join("extra");
        std::fs::create_dir(&extra).expect("extra root");
        let extra = extra.canonicalize().expect("canonical extra root");
        let perimeter = match policy {
            "default" => String::new(),
            "empty" => "roots=[]\n".to_owned(),
            "read" => format!("roots=[{{path='{}',access='read'}}]\n", extra.display()),
            "exec" => format!("roots=[]\nexec_paths=['{}']\n", extra.display()),
            _ => unreachable!(),
        };
        ego.home_file(
            "config.toml",
            &format!("model='ollama/qwen3'\nthinking='off'\nsandbox='workspace'\n{perimeter}"),
        );
        let running = Ego::at(ego.home_path(), ego.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args([
                "--bare",
                "--tools",
                "read,edit",
                "--allow",
                "exec(/bin/sh *)",
            ])
            .env("HOME", home.path().to_str().expect("UTF-8 operator home"))
            .spawn_conversing();
        let mut host = AcpHost::adopt(running);
        host.initialize();
        let session = host.new_session(&ego.workspace_path().to_string_lossy());
        let answer = host.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "Reply without tools."}],
            }),
        );
        let stderr = host.finish();
        assert_eq!(
            answer["result"]["stopReason"], "end_turn",
            "{policy} authority refused the first prompt: {answer}; {stderr}"
        );
        assert_eq!(stub.generations().len(), 1, "{policy} provider request");
    }
}
