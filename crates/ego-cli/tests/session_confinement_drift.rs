//! contract for truthful CLI handling of an admitted confinement drift.
//!
//! The pinned observation is carried by the same `RunSelections` value that
//! the protocol admits. The changed backend identity must be refused before
//! the provider or a tool sink is reached, and the prepared executor must
//! settle the proven-before-effects failure as `Interrupted`, not `Uncertain`.

mod support;

use std::sync::Arc;
use std::time::Duration;
use std::{fs, path::Path, process::Command};

use ego_cli::run::CliRunExecutor;
use ego_core::config::{Mode, ModelRef};
use ego_core::event::id::SessionId;
use ego_core::evidence::{EvidenceAdmissionService, RunInvocationId};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::permit::{BackendIdentity, CanonPath, Confinement, ExecObservation, RunConfinement};
use ego_core::public::PublicEvent;
use ego_core::run::{
    EventSinkFactory, RunAttachment, RunExecutionBindings, RunExecutor, RunId, RunOrigin,
    RunPreparation, RunPrompt, RunRoot, RunSelections, RunSettlement,
};
use ego_core::sink::{CollectingSink, EventSink};
use ego_core::turn::CancelToken;
use ego_tools::exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts};
use ego_tools::http::SystemResolver;
use ego_tools::store::{JsonlStore, OsIo, SessionEvidenceStore};
use support::{Plan, Reply, Stub, text};

struct CollectingSinkFactory(Arc<CollectingSink>);

impl EventSinkFactory for CollectingSinkFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        let sink: Arc<CollectingSink> = Arc::clone(&self.0);
        sink
    }
}

async fn observed_off(workspace: &Path) -> ExecObservation {
    let evidence_root = tempfile::tempdir().expect("measurement evidence root");
    let session = SessionId::new();
    let evidence = SessionEvidenceStore::open(evidence_root.path(), session, 1024 * 1024)
        .expect("measurement evidence store");
    let workspace = CanonPath::new(workspace.to_path_buf());
    let policy = RunConfinement::new(Confinement::Off, false, workspace).expect("off policy");
    let config = ExecRunConfig::new(
        policy,
        evidence,
        CancelToken::new(),
        ExecLimits::new(1024, Duration::from_millis(250)).expect("execution limits"),
        HttpCallTimeout::new(Duration::from_secs(1)).expect("HTTP timeout"),
        None,
        Arc::new(SystemResolver::new()),
    );
    RunEffectPorts::observe(&config)
        .await
        .expect("off observation")
        .0
}

async fn submit_case(
    executor: &CliRunExecutor,
    root: RunRoot,
    session: SessionId,
    observation: ExecObservation,
    prompt: &str,
) -> (RunSettlement, Vec<PublicEvent>) {
    let run_id = RunId::new(session, uuid::Uuid::now_v7());
    let request = RunPreparation {
        run_id,
        invocation: RunInvocationId::new(),
        root: root.clone(),
        selections: RunSelections::new(
            Some(ModelRef("anthropic/claude-sonnet-5".to_owned())),
            None,
            Some(Mode::Default),
            None,
            None,
        )
        .with_sandbox_observation(Confinement::Off, observation),
        continuation: false,
        origin: RunOrigin::TopLevel,
    };
    assert!(
        request.selections.sandbox_observation().is_some(),
        "the CLI fixture must carry the pinned session observation"
    );
    let prepared = executor
        .prepare(&request)
        .await
        .expect("CLI prepares without provider effects");
    let model = prepared.model().clone();
    let (_, prepared) = prepared.into_parts();
    executor
        .admit_run_context(&prepared)
        .await
        .expect("the controller admits run-context evidence before submission");
    let handle = executor
        .attach(&RunAttachment {
            run_id,
            root,
            model,
            origin: RunOrigin::TopLevel,
        })
        .await
        .expect("CLI attaches the prepared run");
    let sink = Arc::new(CollectingSink::new());
    let bindings =
        RunExecutionBindings::new(Arc::new(CollectingSinkFactory(Arc::clone(&sink))), None);
    let result = executor
        .submit(&handle, &RunPrompt::from(prompt), prepared, bindings)
        .await
        .expect("the prepared case settles through the executor");
    (result, sink.events())
}

async fn run_drift_case() {
    let sessions = std::env::var_os("EGO_DRIFT_SESSIONS")
        .map(std::path::PathBuf::from)
        .expect("isolated child session root");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace = fs::canonicalize(workspace.path()).expect("workspace path");
    let observation = observed_off(&workspace).await;
    let changed = ExecObservation::new(
        BackendIdentity::new(
            format!("{}-changed", observation.backend().name()),
            observation.backend().revision(),
        ),
        observation.capabilities(),
        observation.policy_digest().clone(),
    );
    let root = RunRoot::new(&workspace).expect("absolute workspace");
    let store = Arc::new(JsonlStore::new(sessions, &workspace, Box::new(OsIo)));
    let admission = Arc::new(EvidenceAdmissionService::new());
    let executor = CliRunExecutor::for_acp(None, Arc::clone(&store), admission);

    let (control, control_events) = submit_case(
        &executor,
        root.clone(),
        SessionId::new(),
        observation,
        "control must reach the local provider",
    )
    .await;
    assert!(
        matches!(control, RunSettlement::Completed),
        "the no-drift control must reach the local provider and complete: {control:?}"
    );
    assert!(
        !control_events.is_empty(),
        "the no-drift control must publish its normal execution events"
    );

    let (result, drift_events) = submit_case(
        &executor,
        root,
        SessionId::new(),
        changed,
        "must not reach a provider",
    )
    .await;
    let RunSettlement::Interrupted { diagnostic } = result else {
        panic!("confinement drift must settle as Interrupted, not Uncertain");
    };
    let diagnostic = diagnostic.expect("interrupted drift carries a truthful diagnostic");
    assert!(
        diagnostic.contains("confinement"),
        "diagnostic: {diagnostic}"
    );
    assert!(
        drift_events.is_empty(),
        "provider/tool effects must not publish before admitted observation drift is refused"
    );
}

#[test]
fn cli_refuses_changed_admitted_observation_before_provider_or_tool_effects() {
    if std::env::var_os("EGO_DRIFT_CHILD").is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("isolated child Tokio runtime")
            .block_on(run_drift_case());
        return;
    }

    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("control reached local stub")),
            Reply::Sse(text("must not be reached")),
        ],
        ..Plan::default()
    });
    let home = tempfile::tempdir().expect("isolated child home");
    let sessions = tempfile::tempdir().expect("isolated child sessions");
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .env("EGO_HOME", home.path())
        .env("EGO_DRIFT_CHILD", "1")
        .env("EGO_DRIFT_SESSIONS", sessions.path())
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .env("OPENAI_BASE_URL", format!("http://{}", stub.address))
        .env("ANTHROPIC_BASE_URL", format!("http://{}", stub.address))
        .env("ANTHROPIC_API_KEY", "fixture-only-key")
        .args([
            "--exact",
            "cli_refuses_changed_admitted_observation_before_provider_or_tool_effects",
            "--nocapture",
        ])
        .output()
        .expect("isolated CLI drift child");
    assert!(
        child.status.success(),
        "isolated CLI drift child failed:\n{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );

    let provider_generations = stub.generations();
    assert_eq!(
        provider_generations.len(),
        1,
        "the no-drift control must reach the loopback stub exactly once; changed drift must not: {provider_generations:?}"
    );
    assert_eq!(
        durable_tool_call_count(sessions.path()),
        0,
        "no tool invocation may be durable before drift refusal"
    );
}

fn durable_tool_call_count(sessions: &Path) -> usize {
    std::fs::read_dir(sessions)
        .expect("isolated session root")
        .flatten()
        .map(|entry| entry.path().join("events.jsonl"))
        .filter(|path| path.is_file())
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .expect("durable session events")
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .filter_map(|record| {
            record
                .get("payload")
                .and_then(|payload| payload.get("event"))
                .cloned()
        })
        .filter(|event| {
            event.get("kind").and_then(serde_json::Value::as_str) == Some("tool_call_started")
        })
        .count()
}
