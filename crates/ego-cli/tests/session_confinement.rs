//! contracts for the CLI's measured ACP confinement adapter (regression case).
//!
//! The router already owns the protocol ordering and `AcpSessionControls`
//! already owns the ceiling/offer policy. These tests cover the missing outer
//! composition: the CLI adapter must measure through `RunEffectPorts`, retain
//! the observation for the durable session, and expose typed missing-capability
//! refusals to the existing controls port.
//!
//! The adapter name and constructor below are deliberately the smallest
//! fixture-facing API accepted by the story ADR. They remain compile-until
//! the CLI composition wires the real `SessionConfinementAdmission` port.

mod support;

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

#[tokio::test]
async fn cli_adapter_measures_the_offer_and_preserves_typed_unsupported_rows() {
    let sessions = tempfile::tempdir().expect("session root");
    let workspace = tempfile::tempdir().expect("workspace");
    let session = SessionId::new();
    let root = RunRoot::new(workspace.path()).expect("absolute workspace");
    let store = Arc::new(JsonlStore::new(
        sessions.path(),
        workspace.path(),
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

#[tokio::test]
async fn cli_adapter_commits_measurement_evidence_before_publishing_the_offer() {
    let sessions = tempfile::tempdir().expect("session root");
    let workspace = tempfile::tempdir().expect("workspace");
    let session = SessionId::new();
    let root = RunRoot::new(workspace.path()).expect("absolute workspace");
    let store = Arc::new(JsonlStore::new(
        sessions.path(),
        workspace.path(),
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
