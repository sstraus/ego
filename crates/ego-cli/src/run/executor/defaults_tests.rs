//! Contract tests for MCP's model-default composition boundary.
//!
//! `for_mcp` captures the process configuration, so the fixture runs the
//! private assertion in a child test process with an isolated `EGO_HOME`.
//! This keeps the test deterministic without mutating the parent process
//! environment (which is forbidden by the workspace's unsafe-code policy).

use std::process::Command;
use std::sync::Arc;

use ego_ai::catalogue::{
    FactFreshness, ModelAvailability, ModelCatalogueEntry, ModelCatalogueSnapshot,
    ModelFactProvenance, ModelSlug, SuppliedAuthority,
};
use ego_core::evidence::EvidenceAdmissionService;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::public::MeteringKind;
use ego_core::run::RunRoot;
use ego_core::store::run_artifact::RunArtifactStore;
use ego_tools::store::{JsonlStore, OsIo};
use tempfile::tempdir;

const CHILD_ENV: &str = "EGO_DEFAULTS_CHILD";

fn capabilities(default_reasoning_effort: Option<ThinkingLevel>) -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(16_000),
        max_tools: Some(128),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort,
    }
}

pub(super) fn catalogue(default_reasoning_effort: Option<ThinkingLevel>) -> ModelCatalogueSnapshot {
    ModelCatalogueSnapshot::from_entries_and_sources(
        vec![ModelCatalogueEntry {
            slug: ModelSlug::parse("openai/fixture").expect("fixture slug"),
            capabilities: capabilities(default_reasoning_effort),
            metering: MeteringKind::Currency,
            provenance: ModelFactProvenance::supplied(SuppliedAuthority::Builtin),
            freshness: FactFreshness::Current,
            availability: ModelAvailability::Available,
        }],
        Vec::new(),
    )
}

pub(super) fn run_child(case: &str, thinking: Option<&str>) {
    let home = tempdir().expect("isolated EGO_HOME");
    let config = format!(
        "model = \"openai/fixture\"\nbudget = \"1k\"\n{}",
        thinking.map_or(String::new(), |value| format!("thinking = \"{value}\"\n"))
    );
    std::fs::write(home.path().join("config.toml"), config).expect("fixture config");

    let test_name = if case == "overlapping_readmission" {
        "run::executor::readmission_tests::overlapping_readmission_permissions_keep_the_session_event_sequence_unique".to_owned()
    } else if case == "cancelled_readmission" {
        "run::executor::defaults_tests::cancelled_readmission_does_not_complete_measurement"
            .to_owned()
    } else if case == "retried_continuation" {
        "run::executor::readmission_tests::retried_continuation_replays_while_the_admitted_turn_holds_the_session_writer"
            .to_owned()
    } else {
        format!("run::executor::defaults_tests::{case}_model_default")
    };
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .env(CHILD_ENV, case)
        .env("EGO_HOME", home.path())
        .args(["--exact", test_name.as_str(), "--nocapture"])
        .output()
        .expect("spawn isolated defaults test");
    let expected_test = format!("test {test_name} ... ok");
    assert!(
        child.status.success(),
        "isolated {case} case failed:\n{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(
        String::from_utf8_lossy(&child.stdout).contains(expected_test.as_str()),
        "isolated {case} child did not execute `{test_name}`; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

fn exercise(case: &str, expected: Option<ThinkingLevel>, default: Option<ThinkingLevel>) {
    if std::env::var(CHILD_ENV).as_deref() != Ok(case) {
        return;
    }

    let owner = tempdir().expect("workspace fixture owner");
    let workspace = owner.path().join("workspace");
    let sessions = owner.path().join("sessions");
    std::fs::create_dir(&workspace).expect("workspace fixture");
    let root = RunRoot::new(workspace.clone()).expect("absolute MCP root");
    let store = Arc::new(JsonlStore::new(sessions, workspace, Box::new(OsIo)));
    let artifacts: Arc<dyn RunArtifactStore> = store.clone();
    let admission = Arc::new(EvidenceAdmissionService::new());
    let catalogue = catalogue(default);

    let result = super::CliRunExecutor::for_mcp(
        root,
        None,
        store,
        artifacts,
        std::num::NonZeroUsize::new(1).expect("positive concurrency"),
        admission,
        Some(&catalogue),
    );

    match (expected, result) {
        (Some(expected), Ok((_executor, policy))) => {
            assert_eq!(policy.model.0, "openai/fixture");
            assert_eq!(policy.thinking, expected);
        }
        (None, Err(error)) => {
            let message = error.to_string();
            assert!(
                message.contains("--thinking"),
                "unknown model default must name the explicit remedy: {message}"
            );
        }
        (Some(_), Err(error)) => panic!("MCP deployment policy refused: {error}"),
        (None, Ok((_executor, policy))) => panic!(
            "unknown model default must refuse rather than invent {:?}",
            policy.thinking
        ),
    }
}

#[test]
fn known_model_default() {
    if std::env::var(CHILD_ENV).as_deref() == Ok("known") {
        exercise(
            "known",
            Some(ThinkingLevel::Medium),
            Some(ThinkingLevel::Medium),
        );
    } else {
        run_child("known", None);
    }
}

#[test]
fn unknown_model_default() {
    if std::env::var(CHILD_ENV).as_deref() == Ok("unknown") {
        exercise("unknown", None, None);
    } else {
        run_child("unknown", None);
    }
}

#[test]
fn explicit_off_model_default() {
    if std::env::var(CHILD_ENV).as_deref() == Ok("explicit_off") {
        exercise(
            "explicit_off",
            Some(ThinkingLevel::Off),
            Some(ThinkingLevel::Medium),
        );
    } else {
        run_child("explicit_off", Some("off"));
    }
}

// Catches: re-measure ignoring cancellation before or during a running probe.
#[test]
fn cancelled_readmission_does_not_complete_measurement() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("cancelled_readmission") {
        run_child("cancelled_readmission", Some("off"));
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use ego_core::run::RunExecutor;
            let owner = tempdir().unwrap();
            let workspace = owner.path().join("workspace");
            let sessions = owner.path().join("sessions");
            std::fs::create_dir(&workspace).unwrap();
            let root = RunRoot::new(workspace.clone()).unwrap();
            let store = Arc::new(JsonlStore::new(sessions, workspace.clone(), Box::new(OsIo)));
            let artifacts: Arc<dyn RunArtifactStore> = store.clone();
            let (executor, _) = super::CliRunExecutor::for_mcp(
                root.clone(),
                None,
                store,
                artifacts,
                std::num::NonZeroUsize::new(1).unwrap(),
                Arc::new(EvidenceAdmissionService::new()),
                Some(&catalogue(None)),
            )
            .unwrap();
            let invocation = ego_core::evidence::RunInvocationId::new();
            let old = ego_core::permit::ExecObservation::new(
                ego_core::permit::BackendIdentity::new("null", "1"),
                ego_core::permit::ExecCaps::empty(),
                ego_core::permit::PolicyDigest::try_from("1".repeat(64)).unwrap(),
            );
            let preparation = ego_core::run::RunPreparation {
                run_id: ego_core::run::RunId::new(
                    ego_core::event::id::SessionId::new(),
                    invocation.uuid(),
                ),
                invocation,
                root,
                selections: ego_core::run::RunSelections::new(None, None, None, None, None)
                    .with_sandbox_observation(ego_core::permit::Confinement::Off, old),
                continuation: true,
                origin: ego_core::run::RunOrigin::TopLevel,
            };
            let (_, mut prepared) = executor.prepare(&preparation).await.unwrap().into_parts();
            prepared.confinement = ego_core::permit::RunConfinement::new(
                ego_core::permit::Confinement::Off,
                true,
                ego_core::permit::CanonPath::new(&workspace),
            )
            .unwrap();
            executor.admit_run_context(&prepared).await.unwrap();
            let cancel = ego_core::turn::CancelToken::new();
            cancel.cancel(ego_core::event::domain::CancelCause::User);
            let observed = executor.execution_observation(&mut prepared, cancel).await;
            assert!(
                observed.is_err(),
                "cancelled re-measure completed instead of stopping"
            );
            // Catches: evidence-only re-measure acquiring a second session writer.
            let opened = super::session::open(
                executor.store.as_ref(),
                &prepared.session_id,
                &super::SystemClock,
            )
            .unwrap();
            let cancel = ego_core::turn::CancelToken::new();
            let signal = cancel.clone();
            let started = workspace.join("probe-started");
            let finished = workspace.join("probe-finished");
            std::fs::write(
                workspace.join("readmission-probe.sh"),
                "printf started > probe-started\nsleep 10\nprintf finished > probe-finished\n",
            )
            .unwrap();
            let measurement = executor.execution_observation(&mut prepared, cancel);
            let cancellation = async {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !started.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("scripted probe must signal its actual start");
                signal.cancel(ego_core::event::domain::CancelCause::User);
            };
            let (observed, _) = tokio::join!(measurement, cancellation);
            assert!(started.exists(), "cancellation preceded probe startup");
            assert!(
                !finished.exists(),
                "cancellation waited for the scripted probe's natural completion"
            );
            assert!(
                observed.is_err(),
                "cancel during re-measure must refuse continuation"
            );
            assert!(
                observed.unwrap_err().to_string().contains("cancelled"),
                "re-measure must report cancellation, not a second writer lease refusal"
            );
            drop(opened);
        });
}
