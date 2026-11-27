//! Contracts for the process-level ExecCaps diagnostic.
//!
//! Boot measurement is deliberately weaker than session admission. It returns
//! only the backend's measured capability bits, owns no session evidence store,
//! and must leave the existing recorded measurement path authoritative.

use std::future::Future;
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Arc;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use ego_core::event::domain::CancelCause;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ego_core::event::id::SessionId;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ego_core::permit::ProcessError;
use ego_core::permit::{CanonPath, Confinement, ExecCaps, RunConfinement};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ego_core::store::blob::BlobStore;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ego_core::store::evidence::EvidenceRef;
use ego_core::turn::CancelToken;
use ego_tools::exec::{ExecLimits, RunEffectPorts};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ego_tools::exec::{ExecRunConfig, HttpCallTimeout, SessionEvidenceStore};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ego_tools::http::SystemResolver;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test Tokio runtime")
        .block_on(future)
}

fn policy(level: Confinement, offline: bool, workspace: &Path) -> RunConfinement {
    RunConfinement::new(level, offline, CanonPath::new(workspace))
        .expect("the diagnostic policy uses an absolute workspace")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn recorded_config(
    policy: RunConfinement,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
) -> ExecRunConfig {
    ExecRunConfig::new(
        policy,
        evidence,
        cancel,
        ExecLimits::default(),
        HttpCallTimeout::default(),
        None,
        Arc::new(SystemResolver::new()),
    )
}

fn diagnostic_capabilities(policy: &RunConfinement) -> ExecCaps {
    block_on(RunEffectPorts::diagnostic_capabilities(
        policy,
        CancelToken::new(),
        ExecLimits::default(),
    ))
    .unwrap_or_else(|error| panic!("boot capability diagnostic failed unexpectedly: {error}"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_transcript(store: &SessionEvidenceStore, reference: &EvidenceRef) -> Vec<u8> {
    BlobStore::read(store, &reference.blob_ref()).expect("recorded probe transcript")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_transcript_for_workspace(bytes: &[u8], workspace: &Path) {
    let text = std::str::from_utf8(bytes).expect("probe transcript is UTF-8 JSON Lines");
    assert!(!text.is_empty(), "the recorded probe transcript is empty");

    let records = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("probe record JSON"))
        .collect::<Vec<_>>();
    assert!(
        !records.is_empty(),
        "the recorded probe transcript has no records"
    );
    assert!(
        records.iter().all(|record| {
            record["suite"]
                .as_str()
                .is_some_and(|suite| !suite.is_empty())
                && record["root"]
                    .as_str()
                    .is_some_and(|root| Path::new(root).is_absolute())
                && record["argv"]
                    .as_array()
                    .is_some_and(|argv| !argv.is_empty())
                && record["outcome"]
                    .as_str()
                    .is_some_and(|outcome| !outcome.is_empty())
        }),
        "every replayed probe record must retain its measured fields: {text}"
    );
    assert!(
        records.iter().any(|record| {
            record["suite"].as_str() == Some("read.workspace")
                && record["root"]
                    .as_str()
                    .is_some_and(|root| Path::new(root).starts_with(workspace))
        }),
        "the transcript must retain a probe rooted in the recorded workspace: {text}"
    );
}

#[test]
fn diagnostic_capabilities_returns_only_exec_caps_and_creates_no_session_artifact() {
    let workspace = tempfile::tempdir().expect("diagnostic workspace");
    let before = std::fs::read_dir(workspace.path()).expect("empty workspace");
    assert_eq!(before.count(), 0, "the fixture starts without artifacts");

    let policy = policy(Confinement::Off, false, workspace.path());
    let capabilities: ExecCaps = diagnostic_capabilities(&policy);

    assert_eq!(
        capabilities,
        ExecCaps::empty(),
        "an unconfined boot observation must not claim confinement"
    );
    assert!(
        !workspace.path().join("events.jsonl").exists(),
        "boot diagnostics must not create a session event log"
    );
    assert!(
        !workspace.path().join("evidence").exists(),
        "boot diagnostics must not create a session evidence store"
    );
    let after = std::fs::read_dir(workspace.path()).expect("workspace remains readable");
    assert_eq!(
        after.count(),
        0,
        "boot diagnostics created a workspace artifact"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn diagnostic_then_recorded_measurement_replays_cached_transcript_before_digest_or_refusal() {
    let workspace = tempfile::tempdir().expect("cache workspace");
    let workspace_path = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    // `offline` makes the ordinary run refusal deterministic on Linux, while
    // macOS may prove the additional network bit and return a successful run.
    let policy = policy(Confinement::Ro, true, &workspace_path);
    let capabilities = diagnostic_capabilities(&policy);

    let evidence_root = tempfile::tempdir().expect("recorded evidence root");
    let session = SessionId::new();
    let evidence = SessionEvidenceStore::open(evidence_root.path(), session, 64 * 1024 * 1024)
        .expect("real recorded session evidence store");
    let cancelled = CancelToken::new();
    cancelled.cancel(CancelCause::Host);
    let outcome = block_on(RunEffectPorts::for_run(recorded_config(
        policy,
        evidence.clone(),
        cancelled,
    )));

    let reference = match outcome {
        Ok((_ports, observation, reference)) => {
            assert_eq!(
                observation.capabilities(),
                capabilities,
                "recorded measurement changed the cached capability fact"
            );
            reference.expect("a measured run must publish its replayed transcript")
        }
        Err(ProcessError::MissingCapabilities(missing)) => {
            assert_eq!(
                missing.available(),
                capabilities,
                "the refusal must use the diagnostic's cached capability fact"
            );
            let reference = missing
                .probe_evidence()
                .cloned()
                .expect("a cached-probe refusal must cite its replayed transcript");
            assert!(
                missing.to_string().contains(reference.id.as_str()),
                "the refusal must name the transcript committed before it: {missing}"
            );
            reference
        }
        Err(ProcessError::Cancelled) => {
            panic!("recorded measurement re-ran probes instead of replaying the cached result")
        }
        Err(error) => panic!("recorded measurement failed without a typed result: {error}"),
    };

    assert_transcript_for_workspace(&read_transcript(&evidence, &reference), &workspace_path);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn recorded_observations_keep_real_per_root_evidence_after_boot_diagnostic() {
    let first_workspace = tempfile::tempdir().expect("first workspace");
    let second_workspace = tempfile::tempdir().expect("second workspace");
    let first_path = std::fs::canonicalize(first_workspace.path()).expect("first canonical root");
    let second_path =
        std::fs::canonicalize(second_workspace.path()).expect("second canonical root");

    // The process observation and each session observation deliberately use
    // distinct roots. A boot result must not be reused as session evidence.
    let boot_policy = policy(Confinement::Ro, false, &first_path);
    let _ = diagnostic_capabilities(&boot_policy);

    let first_evidence_root = tempfile::tempdir().expect("first evidence root");
    let first_session = SessionId::new();
    let first_evidence =
        SessionEvidenceStore::open(first_evidence_root.path(), first_session, 64 * 1024 * 1024)
            .expect("first real session evidence store");
    let (first_observation, first_reference) = block_on(RunEffectPorts::observe(&recorded_config(
        policy(Confinement::Ro, false, &first_path),
        first_evidence.clone(),
        CancelToken::new(),
    )))
    .expect("first session observation");
    let first_reference = first_reference.expect("first session probe transcript");

    let second_evidence_root = tempfile::tempdir().expect("second evidence root");
    let second_session = SessionId::new();
    let second_evidence = SessionEvidenceStore::open(
        second_evidence_root.path(),
        second_session,
        64 * 1024 * 1024,
    )
    .expect("second real session evidence store");
    let (second_observation, second_reference) =
        block_on(RunEffectPorts::observe(&recorded_config(
            policy(Confinement::Ro, false, &second_path),
            second_evidence.clone(),
            CancelToken::new(),
        )))
        .expect("second session observation");
    let second_reference = second_reference.expect("second session probe transcript");

    let first_bytes = read_transcript(&first_evidence, &first_reference);
    let second_bytes = read_transcript(&second_evidence, &second_reference);
    assert_transcript_for_workspace(&first_bytes, &first_path);
    assert_transcript_for_workspace(&second_bytes, &second_path);
    assert_ne!(
        first_reference, second_reference,
        "distinct session roots must not share one boot or session evidence object"
    );
    assert_ne!(
        first_observation.policy_digest(),
        second_observation.policy_digest(),
        "per-root session observations must retain root-specific policy authority"
    );
}

// Catches: default/configured roots falsely invalidate the unconfined ACP observation.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn unconfined_admission_ignores_inactive_roots() {
    block_on(async {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = tempfile::tempdir().expect("configured perimeter root");
        let evidence_root = tempfile::tempdir().expect("session evidence");
        let evidence =
            SessionEvidenceStore::open(evidence_root.path(), SessionId::new(), 1024 * 1024)
                .expect("evidence store");
        let initial = policy(Confinement::Off, false, workspace.path());
        let admitted = RunEffectPorts::observe(&recorded_config(
            initial.clone(),
            evidence.clone(),
            CancelToken::new(),
        ))
        .await
        .expect("ACP observation")
        .0;
        let execution = initial.with_roots(vec![ego_core::permit::ProfileRoot::new(
            CanonPath::new(root.path()),
            ego_core::permit::RootAccess::ReadWrite,
        )]);
        for _ in 0..3 {
            let result = RunEffectPorts::for_run(
                recorded_config(execution.clone(), evidence.clone(), CancelToken::new())
                    .with_admitted_observation(admitted.clone()),
            )
            .await;
            assert!(
                result.is_ok(),
                "inactive roots cannot change null confinement: {:?}",
                result.err()
            );
        }
    });
}
