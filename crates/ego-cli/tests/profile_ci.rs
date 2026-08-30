//! RED contract for regression case's explicit `profile.ci` posture.
//!
//! These tests run the real binary for the process-level contract. The two
//! publication tests use the existing public artifact port because the CLI
//! currently has no public StoreIo injection seam for a faulted publisher.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::evidence::RunContextEvidence;
use ego_core::run::RunId;
use ego_core::store::run_artifact::{
    ArtifactPublication, PublishedRunArtifact, RunArtifactId, RunArtifactStore,
    RunArtifactStoreError,
};
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{AtomicReplacePublication, JsonlStore, OsIo, StoreIo};
use serde_json::Value;
use tempfile::TempDir;

mod support;

use support::{Ego, Finished, Plan, Reply, Stub, costing};

const CI_PROMPT: &str = "CI-PROMPT-SECRET";
const CI_RESULT: &str = "CI-VISIBLE-RESULT";
const CI_INSTRUCTIONS: &str = "CI-INSTRUCTION-SECRET";

fn profile_ci_config(session_root: &Path, artifact_root: &Path) -> String {
    format!(
        "[profile.ci]\nmodel = \"ollama/qwen3\"\nsandbox = \"off\"\nsession_dir = {:?}\nartifact_dir = {:?}\n",
        session_root.to_string_lossy(),
        artifact_root.to_string_lossy(),
    )
}

const OPENAI_CODEX_FACTS: &str = r#"
[model."openai-codex/gpt-5.1".capabilities]
schema_version = 2
max_context_tokens = 400000
max_output_tokens = 128000
max_tools = 128
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
cancel = "cooperative"
cache = "long"
transport = "sse"
"#;

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    session_root: PathBuf,
    artifact_root: PathBuf,
    stub: Stub,
}

impl Fixture {
    fn new() -> Self {
        Self::with_plan(Plan {
            completions: vec![Reply::Sse(costing(CI_RESULT, 11, 5))],
            ..Plan::default()
        })
    }

    fn with_plan(plan: Plan) -> Self {
        let root = tempfile::tempdir().expect("CI fixture root");
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let session_root = root.path().join("sessions");
        let artifact_root = root.path().join("artifacts");
        fs::create_dir_all(&home).expect("CI fixture home");
        fs::create_dir_all(&workspace).expect("CI fixture workspace");
        fs::create_dir_all(&session_root).expect("CI fixture session root");
        fs::create_dir_all(&artifact_root).expect("CI fixture artifact root");
        fs::write(
            home.join("config.toml"),
            profile_ci_config(&session_root, &artifact_root),
        )
        .expect("CI profile config");
        fs::write(workspace.join("ci-instructions.md"), CI_INSTRUCTIONS)
            .expect("CI instruction file");
        let stub = Stub::start(plan);
        Self {
            _root: root,
            home,
            workspace,
            session_root,
            artifact_root,
            stub,
        }
    }

    fn command(
        &self,
        profile: Option<&str>,
        session_root: &Path,
        artifact_root: &Path,
        instructions: bool,
    ) -> Ego {
        self.command_with_model(
            "ollama/qwen3",
            profile,
            session_root,
            artifact_root,
            instructions,
        )
    }

    fn command_with_model(
        &self,
        model: &str,
        profile: Option<&str>,
        session_root: &Path,
        artifact_root: &Path,
        instructions: bool,
    ) -> Ego {
        let mut command = Ego::at(&self.home, &self.workspace, Some(&self.stub)).args([
            "--model",
            model,
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--session-dir",
            session_root.to_str().expect("UTF-8 session root"),
            "--artifact-dir",
            artifact_root.to_str().expect("UTF-8 artifact root"),
            "--sandbox",
            "off",
        ]);
        if let Some(profile) = profile {
            command = command.args(["--profile", profile]);
        }
        if instructions {
            command = command.args([
                "--instructions",
                self.workspace
                    .join("ci-instructions.md")
                    .to_str()
                    .expect("UTF-8 instruction path"),
            ]);
        }
        command
    }

    fn run_ci(&self, output: &str) -> Finished {
        self.command(Some("ci"), &self.session_root, &self.artifact_root, true)
            .args(["--output", output, CI_PROMPT])
            .run()
    }

    fn session_dir(&self) -> PathBuf {
        self.session_root.join(support::FIXTURE_SESSION_ID)
    }

    fn manifest(&self) -> PathBuf {
        self.session_dir().join("manifest.json")
    }

    fn log(&self) -> PathBuf {
        self.session_dir().join("events.jsonl")
    }

    fn artifact_session_dir(&self) -> PathBuf {
        self.artifact_root.join(support::FIXTURE_SESSION_ID)
    }

    fn sanitized_bundles(&self) -> Vec<PathBuf> {
        let artifacts = self.artifact_session_dir().join("artifacts");
        let Ok(entries) = fs::read_dir(artifacts) else {
            return Vec::new();
        };
        let mut bundles = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("sanitized_bundle.json"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        bundles.sort();
        bundles
    }

    fn assert_preflight_refused(
        &self,
        finished: &Finished,
        session_root: &Path,
        artifact_root: &Path,
        expected: &str,
    ) {
        assert_ne!(
            finished.code,
            Some(0),
            "CI preflight unexpectedly succeeded"
        );
        assert!(
            !finished.stderr.trim().is_empty(),
            "CI preflight refusal omitted its stderr diagnostic"
        );
        assert!(
            finished.stderr.contains(expected),
            "CI preflight diagnostic omitted {expected:?}: {}",
            finished.stderr
        );
        assert!(
            !finished.stderr.contains("unexpected argument"),
            "CI preflight was rejected by Clap argument handling: {}",
            finished.stderr
        );
        assert!(
            finished.stdout.trim().is_empty(),
            "CI preflight wrote protocol data to stdout: {}",
            finished.stdout
        );
        assert!(
            self.stub.generations().is_empty(),
            "CI preflight reached the provider"
        );
        assert!(
            !resolve_root(&self.workspace, session_root)
                .join(support::FIXTURE_SESSION_ID)
                .exists(),
            "CI preflight created a session before refusal"
        );
        assert!(
            !resolve_root(&self.workspace, artifact_root)
                .join(support::FIXTURE_SESSION_ID)
                .exists(),
            "CI preflight created an artifact directory before refusal"
        );
    }
}

fn resolve_root(workspace: &Path, root: &Path) -> PathBuf {
    if root.is_absolute() {
        root.to_path_buf()
    } else {
        workspace.join(root)
    }
}

fn stored_bundle_bytes(path: &Path) -> Vec<u8> {
    let stored: Value = serde_json::from_slice(&fs::read(path).expect("stored artifact"))
        .expect("stored artifact JSON");
    serde_json::from_value(stored["bytes"].clone()).expect("stored artifact bytes")
}

struct CleanRunner {
    manifest: PathBuf,
    bundle: PathBuf,
}

fn contains_key(value: &Value, wanted: &str) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| key == wanted || contains_key(value, wanted)),
        Value::Array(values) => values.iter().any(|value| contains_key(value, wanted)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

impl CleanRunner {
    fn consume(&self) -> (usize, bool, bool, bool) {
        let manifest: Value =
            serde_json::from_slice(&fs::read(&self.manifest).expect("clean runner manifest"))
                .expect("clean runner manifest JSON");
        let bundle: Value = serde_json::from_slice(&stored_bundle_bytes(&self.bundle))
            .expect("clean runner sanitized bundle JSON");
        let records = bundle["records"]
            .as_array()
            .expect("sanitized records")
            .len();
        let has_evidence = manifest.get("evidence").is_some();
        let has_exit = contains_key(&bundle, "exit_reason");
        let has_usage = contains_key(&bundle, "usage");
        (records, has_evidence, has_exit, has_usage)
    }
}

struct BeforePublicationFailure;

impl StoreIo for BeforePublicationFailure {
    fn append_line(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, bytes)
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
        OsIo.fsync(path)
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        OsIo.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.replace_atomic(path, bytes)
    }

    fn replace_atomic_publication(
        &self,
        _path: &Path,
        _bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        Err(io::Error::other("injected pre-publication failure"))
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OsIo.truncate(path, len)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        OsIo.list_dirs(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        OsIo.remove_file(path)
    }
}

struct UncertainAfterWrite {
    replace_calls: Arc<AtomicUsize>,
    remove_calls: Arc<AtomicUsize>,
}

impl StoreIo for UncertainAfterWrite {
    fn append_line(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, bytes)
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
        OsIo.fsync(path)
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        OsIo.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.replace_atomic(path, bytes)
    }

    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        self.replace_calls.fetch_add(1, Ordering::SeqCst);
        OsIo.replace_atomic(path, bytes)?;
        Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
            diagnostic: "injected post-publication uncertainty".to_owned(),
        })
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        OsIo.remove_file(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OsIo.truncate(path, len)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        OsIo.list_dirs(path)
    }
}

fn run_id() -> RunId {
    "00000000-0000-0000-0000-000000000070.00000000-0000-0000-0000-000000000001"
        .parse()
        .expect("valid CI run id")
}

fn artifact(bytes: &[u8]) -> PublishedRunArtifact {
    PublishedRunArtifact::sanitized_bundle(bytes.to_vec())
}

fn seed_session_with_unopenable_evidence(fixture: &Fixture) {
    let session: SessionId = support::FIXTURE_SESSION_ID
        .parse()
        .expect("fixture session id");
    let store = JsonlStore::new(
        fixture.session_root.clone(),
        fixture.workspace.clone(),
        Box::new(OsIo),
    );
    let mut seq = Seq::ZERO;
    store
        .append(
            &session,
            &DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: None,
                    parent: None,
                    ts: Timestamp::EPOCH,
                    producer: Producer::Main,
                },
                kind: EventKind::UserPrompt {
                    text: "durable prefix".to_owned(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            },
        )
        .expect("seed durable session prefix");
    fs::write(fixture.session_dir().join("evidence"), b"not a directory")
        .expect("block evidence directory");
}

#[test]
fn only_explicit_profile_ci_activates_the_ci_posture() {
    let fixture = Fixture::new();
    let finished = fixture.run_ci("jsonl");
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    assert_eq!(fixture.sanitized_bundles().len(), 1);
    assert!(
        finished
            .stdout
            .lines()
            .all(|line| { serde_json::from_str::<Value>(line).is_ok() })
    );
}

#[test]
fn ego_profile_ci_activates_the_same_posture_without_ambient_ci() {
    let fixture = Fixture::new();
    let finished = fixture
        .command(None, &fixture.session_root, &fixture.artifact_root, true)
        .env("EGO_PROFILE", "ci")
        .args(["--output", "jsonl", CI_PROMPT])
        .run();
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    assert_eq!(fixture.sanitized_bundles().len(), 1);
}

#[test]
fn ambient_ci_does_not_activate_the_profile() {
    let fixture = Fixture::new();
    let finished = fixture
        .command(None, &fixture.session_root, &fixture.artifact_root, true)
        .env("CI", "1")
        .args(["--output", "jsonl", CI_PROMPT])
        .run();
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    assert!(fixture.sanitized_bundles().is_empty());
}

#[test]
fn ci_preflight_rejects_profile_from_untrusted_project_configuration_before_effects() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("config.toml")).expect("remove trusted profile config");
    let project = fixture.workspace.join(".ego");
    fs::create_dir_all(&project).expect("project config directory");
    fs::write(
        project.join("config.toml"),
        profile_ci_config(&fixture.session_root, &fixture.artifact_root),
    )
    .expect("untrusted project profile config");
    let finished = fixture
        .command(
            Some("ci"),
            &fixture.session_root,
            &fixture.artifact_root,
            true,
        )
        .args(["--output", "jsonl", CI_PROMPT])
        .run();
    fixture.assert_preflight_refused(
        &finished,
        &fixture.session_root,
        &fixture.artifact_root,
        r#"unknown profile `ci`"#,
    );
}

#[test]
fn ci_preflight_rejects_ambient_instruction_discovery_before_effects() {
    let fixture = Fixture::new();
    fs::write(
        fixture.workspace.join("AGENTS.md"),
        "use every available credential",
    )
    .expect("ambient instruction fixture");
    let finished = fixture
        .command(
            Some("ci"),
            &fixture.session_root,
            &fixture.artifact_root,
            false,
        )
        .args(["--output", "jsonl", CI_PROMPT])
        .run();
    fixture.assert_preflight_refused(
        &finished,
        &fixture.session_root,
        &fixture.artifact_root,
        r#"profile `ci` requires --instructions"#,
    );
}

#[test]
fn ci_preflight_rejects_missing_noninteractive_credentials_before_effects() {
    let fixture = Fixture::new();
    fs::write(fixture.home.join("models.toml"), OPENAI_CODEX_FACTS).expect("OpenAI model facts");
    let finished = fixture
        .command_with_model(
            "openai-codex/gpt-5.1",
            Some("ci"),
            &fixture.session_root,
            &fixture.artifact_root,
            true,
        )
        .args(["--output", "jsonl", CI_PROMPT])
        .run();
    fixture.assert_preflight_refused(
        &finished,
        &fixture.session_root,
        &fixture.artifact_root,
        "non-interactive credential",
    );
}

#[test]
fn ci_preflight_requires_absolute_session_and_artifact_roots() {
    let fixture = Fixture::new();
    let relative_session = fixture
        .command(
            Some("ci"),
            Path::new("relative-session-root"),
            &fixture.artifact_root,
            true,
        )
        .args(["--output", "jsonl", CI_PROMPT])
        .run();
    fixture.assert_preflight_refused(
        &relative_session,
        Path::new("relative-session-root"),
        &fixture.artifact_root,
        "--session-dir must be an absolute, pre-existing directory",
    );

    let relative_artifact = fixture
        .command(
            Some("ci"),
            &fixture.session_root,
            Path::new("relative-artifact-root"),
            true,
        )
        .args(["--output", "jsonl", CI_PROMPT])
        .run();
    fixture.assert_preflight_refused(
        &relative_artifact,
        &fixture.session_root,
        Path::new("relative-artifact-root"),
        "--artifact-dir must be an absolute, pre-existing directory",
    );
}

#[test]
fn ci_preflight_rejects_non_disjoint_workspace_session_and_artifact_roots() {
    let fixture = Fixture::new();
    let nested_session = fixture.workspace.join("nested-session");
    let nested_artifact = fixture.workspace.join("nested-artifact");
    fs::create_dir_all(&nested_session).expect("nested session root");
    fs::create_dir_all(&nested_artifact).expect("nested artifact root");

    for (session_root, artifact_root) in [
        (fixture.workspace.as_path(), fixture.artifact_root.as_path()),
        (fixture.session_root.as_path(), fixture.workspace.as_path()),
        (
            fixture.session_root.as_path(),
            fixture.session_root.as_path(),
        ),
        (nested_session.as_path(), fixture.artifact_root.as_path()),
        (fixture.session_root.as_path(), nested_artifact.as_path()),
    ] {
        let finished = fixture
            .command(Some("ci"), session_root, artifact_root, true)
            .args(["--output", "jsonl", CI_PROMPT])
            .run();
        fixture.assert_preflight_refused(
            &finished,
            session_root,
            artifact_root,
            "workspace, session, and artifact roots must be pairwise disjoint",
        );
    }
}

#[test]
fn ci_preflight_rejects_prompt_capable_invocation_before_effects() {
    let fixture = Fixture::new();
    let finished = fixture
        .command(
            Some("ci"),
            &fixture.session_root,
            &fixture.artifact_root,
            true,
        )
        .args(["--output", "jsonl"])
        .run();
    fixture.assert_preflight_refused(
        &finished,
        &fixture.session_root,
        &fixture.artifact_root,
        r#"profile `ci` requires a headless prompt"#,
    );
}

#[test]
fn ci_preflight_rejects_conversational_invocation_before_seat_or_effects() {
    let fixture = Fixture::new();
    let finished = fixture
        .command(
            Some("ci"),
            &fixture.session_root,
            &fixture.artifact_root,
            true,
        )
        .conversing()
        .run();
    fixture.assert_preflight_refused(
        &finished,
        &fixture.session_root,
        &fixture.artifact_root,
        r#"profile `ci` is available only through ego run or ego resume"#,
    );
}

#[test]
fn each_admitted_terminal_ci_run_publishes_exactly_one_sanitized_bundle() {
    let fixture = Fixture::new();
    assert!(fixture.sanitized_bundles().is_empty());
    let finished = fixture.run_ci("json");
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let report = finished.report();
    assert_eq!(report["ok"], true);
    assert!(report["usage"].is_object(), "{report}");
    let bundles = fixture.sanitized_bundles();
    assert_eq!(bundles.len(), 1);
    let bundle: Value =
        serde_json::from_slice(&stored_bundle_bytes(&bundles[0])).expect("sanitized bundle JSON");
    assert_eq!(bundle["v"], 1);
    assert!(bundle["records"].is_array());
}

#[test]
fn direct_ci_run_context_records_complete_admitted_pre_effect_facts() {
    let fixture = Fixture::new();
    let finished = fixture.run_ci("json");
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let context = fs::read_to_string(fixture.log())
        .expect("session log")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|record| {
            let payload = record.get("payload")?;
            let evidence = payload.get("evidence")?;
            (evidence["kind"] == "run_context").then(|| evidence.clone())
        })
        .expect("direct run persisted run-context evidence");
    assert_eq!(context["kind"], "run_context");
    let mut body = context.clone();
    body.as_object_mut()
        .expect("run-context evidence object")
        .remove("kind");
    let exact_fields = [
        "version",
        "session",
        "invocation",
        "run_id",
        "ego_version",
        "config",
        "config_digest",
        "policy_digest",
        "ceilings",
        "instructions",
        "mcp",
        "execution",
    ];
    let mut keys: Vec<_> = body
        .as_object()
        .expect("run-context evidence object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut expected = exact_fields.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected, "direct run-context schema drifted: {body}");
    serde_json::from_value::<RunContextEvidence>(body.clone())
        .expect("direct run-context evidence uses the exact v2 schema");
    for field in exact_fields {
        assert!(
            body.get(field).is_some_and(|value| !value.is_null()),
            "direct run-context evidence omitted {field}: {body}"
        );
        let mut missing = body.clone();
        missing
            .as_object_mut()
            .expect("run-context evidence object")
            .remove(field);
        assert!(
            serde_json::from_value::<RunContextEvidence>(missing).is_err(),
            "direct run-context accepted missing required field {field}"
        );
    }
}

#[test]
fn provider_failed_terminal_ci_run_still_publishes_exactly_one_sanitized_bundle() {
    let fixture = Fixture::with_plan(Plan {
        completions: vec![Reply::Status(500)],
        ..Plan::default()
    });
    assert!(fixture.sanitized_bundles().is_empty());
    let finished = fixture.run_ci("json");
    assert_ne!(
        finished.code,
        Some(0),
        "provider failure unexpectedly succeeded"
    );
    assert_eq!(fixture.sanitized_bundles().len(), 1);
}

#[test]
fn post_admission_active_session_failure_finalizes_one_bundle_without_retry_or_delete() {
    let fixture = Fixture::new();
    seed_session_with_unopenable_evidence(&fixture);

    let finished = fixture.run_ci("json");
    assert_ne!(
        finished.code,
        Some(0),
        "durable activation unexpectedly succeeded"
    );
    assert!(
        finished
            .stderr
            .contains("could not open this session's tool evidence"),
        "primary activation failure was lost: {}",
        finished.stderr
    );
    assert_eq!(fixture.sanitized_bundles().len(), 1);
    assert!(fixture.session_dir().join("evidence").is_file());
    let log = fs::read_to_string(fixture.log()).expect("session log");
    assert_eq!(
        log.matches("\"run_context\"").count(),
        1,
        "post-admission failure retried run-context admission"
    );
}

#[test]
fn jsonl_stdout_is_parseable_and_diagnostics_stay_on_stderr() {
    let fixture = Fixture::new();
    let finished = fixture.run_ci("jsonl");
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    assert!(!finished.stdout.is_empty());
    for line in finished.stdout.lines() {
        serde_json::from_str::<Value>(line).expect("JSONL stdout line");
    }
}

#[test]
fn clean_runner_consumes_manifest_and_sanitized_records_without_transcript_access() {
    let fixture = Fixture::new();
    let finished = fixture.run_ci("json");
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let bundles = fixture.sanitized_bundles();
    assert_eq!(bundles.len(), 1);
    fs::remove_file(fixture.log()).expect("clean runner fixture removes transcript access");
    let clean = CleanRunner {
        manifest: fixture.manifest(),
        bundle: bundles[0].clone(),
    };
    assert_ne!(fixture.session_root, fixture.artifact_root);
    let (records, has_evidence, has_exit, has_usage) = clean.consume();
    assert!(records > 0);
    assert!(has_evidence);
    assert!(has_exit, "sanitized bundle omitted exit evidence");
    assert!(has_usage, "sanitized bundle omitted usage evidence");
    let manifest_bytes = fs::read(fixture.manifest()).expect("manifest");
    let bundle_bytes = stored_bundle_bytes(&bundles[0]);
    let manifest = String::from_utf8_lossy(&manifest_bytes);
    let bundle = String::from_utf8_lossy(&bundle_bytes);
    for marker in [CI_PROMPT, CI_INSTRUCTIONS] {
        assert!(!manifest.contains(marker), "manifest leaked {marker}");
        assert!(!bundle.contains(marker), "sanitized bundle leaked {marker}");
    }
}

#[test]
fn prepublication_failure_is_terminal_and_leaves_no_sanitized_bundle() {
    let root = tempfile::tempdir().expect("artifact root");
    let store = JsonlStore::new(
        root.path(),
        "/workspace",
        Box::new(BeforePublicationFailure),
    );
    assert!(matches!(
        store.publish(run_id(), artifact(br#"{\"v\":1}"#)),
        Err(RunArtifactStoreError::Io { message }) if message.contains("pre-publication")
    ));
    assert!(matches!(
        RunArtifactStore::read(&store, run_id(), RunArtifactId::sanitized_bundle()),
        Err(RunArtifactStoreError::Absent { .. })
    ));
}

#[test]
fn durability_uncertainty_is_truthful_nonretrying_and_keeps_the_artifact_readable() {
    let root = tempfile::tempdir().expect("artifact root");
    let replace_calls = Arc::new(AtomicUsize::new(0));
    let remove_calls = Arc::new(AtomicUsize::new(0));
    let store = JsonlStore::new(
        root.path(),
        "/workspace",
        Box::new(UncertainAfterWrite {
            replace_calls: Arc::clone(&replace_calls),
            remove_calls: Arc::clone(&remove_calls),
        }),
    );
    let bytes = br#"{\"v\":1}"#;
    assert!(matches!(
        store.publish(run_id(), artifact(bytes)),
        Ok(ArtifactPublication::PublishedDurabilityUncertain { diagnostic })
            if diagnostic.contains("post-publication uncertainty")
    ));
    assert_eq!(replace_calls.load(Ordering::SeqCst), 1);
    assert_eq!(remove_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        RunArtifactStore::read(&store, run_id(), RunArtifactId::sanitized_bundle())
            .expect("uncertain artifact remains readable")
            .bytes(),
        bytes
    );
}

#[test]
fn distinct_concurrent_ci_roots_publish_without_colliding() {
    let first = Fixture::new();
    let second = Fixture::new();
    let (first_run, second_run) = std::thread::scope(|scope| {
        let first_handle = scope.spawn(|| first.run_ci("jsonl"));
        let second_handle = scope.spawn(|| second.run_ci("jsonl"));
        (
            first_handle.join().expect("first CI process").code,
            second_handle.join().expect("second CI process").code,
        )
    });
    assert_eq!(first_run, Some(0));
    assert_eq!(second_run, Some(0));
    assert_eq!(first.sanitized_bundles().len(), 1);
    assert_eq!(second.sanitized_bundles().len(), 1);
}
