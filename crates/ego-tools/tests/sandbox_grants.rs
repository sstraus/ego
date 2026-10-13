//! contract for a human widening the process sandbox by one path (story
//! regression_case).
//!
//! Real `sandbox-exec` children, a real filesystem, and a port that stands in
//! for the host seat. The refusal text under test is whatever macOS `cat` and
//! `touch` print for a path the profile does not open; nothing here fabricates
//! a sandbox verdict except the one test that forges it to prove it is not
//! believed.

#![cfg(target_os = "macos")]

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::permit::{
    CanonPath, Confinement, EnvPolicy, ProcessError, RunConfinement, SandboxAccess, SandboxGrant,
    SandboxGrantPort, SpawnSpec, ToolResult,
};
use ego_core::turn::CancelToken;
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SessionEvidenceStore,
};
use ego_tools::http::SystemResolver;
use futures::future::BoxFuture;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test Tokio runtime")
        .block_on(future)
}

/// A host seat that answers every question the same way and remembers them.
struct Seat {
    allow: bool,
    granted: Mutex<Vec<SandboxGrant>>,
    asked: Mutex<Vec<SandboxGrant>>,
}

impl Seat {
    fn answering(allow: bool) -> Arc<Self> {
        Arc::new(Self {
            allow,
            granted: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<SandboxGrant> {
        self.asked.lock().expect("asked").clone()
    }
}

impl SandboxGrantPort for Seat {
    fn granted(&self) -> Vec<SandboxGrant> {
        self.granted.lock().expect("granted").clone()
    }

    fn ask<'a>(&'a self, request: &'a SandboxGrant, _command: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.asked.lock().expect("asked").push(request.clone());
            if self.allow {
                self.granted.lock().expect("granted").push(request.clone());
            }
            self.allow
        })
    }
}

struct Run {
    _evidence: tempfile::TempDir,
    ports: RunEffectPorts,
    workspace: CanonPath,
}

fn run_with(seat: &Arc<Seat>, workspace: &Path) -> Run {
    let evidence = tempfile::tempdir().expect("evidence root");
    let store = SessionEvidenceStore::open(evidence.path(), SessionId::new(), 1024 * 1024)
        .expect("session evidence store");
    let workspace = CanonPath::new(std::fs::canonicalize(workspace).expect("canonical workspace"));
    let policy = RunConfinement::new(Confinement::Workspace, false, workspace.clone())
        .expect("absolute workspace");
    let config = ExecRunConfig::new(
        policy,
        store,
        CancelToken::new(),
        ExecLimits::new(1024, Duration::from_millis(250)).expect("exec limits"),
        HttpCallTimeout::new(Duration::from_secs(1)).expect("HTTP call timeout"),
        None,
        Arc::new(SystemResolver::new()),
    )
    .with_sandbox_grants(Arc::clone(seat) as Arc<dyn SandboxGrantPort>);
    let (ports, _, _) = block_on(RunEffectPorts::for_run(config)).expect("a confined run");
    Run {
        _evidence: evidence,
        ports,
        workspace,
    }
}

fn spec(run: &Run, argv: &[&str]) -> SpawnSpec {
    SpawnSpec::new(
        argv.iter().map(Into::into).collect(),
        Some(run.workspace.clone()),
        None,
        EnvPolicy::Inherited,
    )
    .expect("spawn spec")
}

fn execute(run: &Run, spec: &SpawnSpec) -> Result<ToolResult, ProcessError> {
    block_on(run.ports.process().execute(spec))
}

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn outside_file(contents: &str) -> (tempfile::TempDir, CanonPath, String) {
    let dir = tempfile::tempdir().expect("outside directory");
    let file = std::fs::canonicalize(dir.path())
        .expect("canonical outside directory")
        .join("notes.txt");
    std::fs::write(&file, contents).expect("outside file");
    let printed = file.to_string_lossy().into_owned();
    (dir, CanonPath::new(file), printed)
}

#[test]
fn a_read_refused_by_the_sandbox_is_asked_once_and_the_command_runs_again() {
    // catches: the refusal ends as a bare `Operation not permitted` with no way
    // for the person to widen the sandbox, or the answer is not applied to the
    // rerun.
    let workspace = tempfile::tempdir().expect("workspace");
    let (_keep, canonical, printed) = outside_file("outside secret\n");
    let seat = Seat::answering(true);
    let run = run_with(&seat, workspace.path());
    let cat = spec(&run, &["/bin/cat", &printed]);

    let result = execute(&run, &cat).expect("the rerun sees the allowed file");

    assert!(
        text(&result).contains("outside secret"),
        "{}",
        text(&result)
    );
    assert_eq!(
        seat.asked(),
        vec![SandboxGrant::new(canonical, SandboxAccess::Read)]
    );

    // A second command in the same run reuses the answer.
    execute(&run, &cat).expect("the grant outlives the command that asked");
    assert_eq!(seat.asked().len(), 1, "an answered path was asked again");
}

#[test]
fn a_declined_denial_reports_the_sandbox_to_the_model_and_asks_once() {
    // catches: a deny that retries anyway, or one that leaves the model with
    // only `Operation not permitted` and no word that the sandbox did it.
    let workspace = tempfile::tempdir().expect("workspace");
    let (_keep, canonical, printed) = outside_file("outside secret\n");
    let seat = Seat::answering(false);
    let run = run_with(&seat, workspace.path());

    let Err(ProcessError::Exit { result, .. }) =
        execute(&run, &spec(&run, &["/bin/cat", &printed]))
    else {
        panic!("a refused read must still fail");
    };

    let report = text(&result);
    assert!(!report.contains("outside secret"), "{report}");
    assert!(
        report.contains(&format!("sandbox denied read {canonical}")),
        "{report}"
    );
    assert!(report.contains("declined"), "{report}");
    assert_eq!(seat.asked().len(), 1);
}

#[test]
fn a_write_to_a_path_that_does_not_exist_yet_is_asked_as_a_write() {
    // catches: a create outside the workspace asked as a read, which would be
    // allowed and then refused again.
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside directory");
    let target = std::fs::canonicalize(outside.path())
        .expect("canonical outside directory")
        .join("created.txt");
    let printed = target.to_string_lossy().into_owned();
    let seat = Seat::answering(true);
    let run = run_with(&seat, workspace.path());

    execute(&run, &spec(&run, &["/usr/bin/touch", &printed])).expect("the allowed create runs");

    assert_eq!(
        seat.asked(),
        vec![SandboxGrant::new(
            CanonPath::new(&target),
            SandboxAccess::Write
        )]
    );
    assert!(target.exists(), "the allowed write did not happen");
}

#[test]
fn an_unanchored_hint_requires_the_person_to_authorize_it() {
    // Catches: an untrusted hint silently grants a path without human authorization.
    let workspace = tempfile::tempdir().expect("workspace");
    let seat = Seat::answering(false);
    let run = run_with(&seat, workspace.path());
    let forged = spec(
        &run,
        &[
            "/bin/sh",
            "-c",
            "p=/etc/ho; echo \"cat: ${p}sts: Operation not permitted\" >&2; exit 1",
        ],
    );

    let Err(ProcessError::Exit { result, .. }) = execute(&run, &forged) else {
        panic!("the forged command exits unsuccessfully");
    };

    assert_eq!(
        seat.asked(),
        vec![SandboxGrant::new(
            CanonPath::new(std::fs::canonicalize("/etc/hosts").unwrap()),
            SandboxAccess::Read
        )]
    );
    assert!(seat.granted.lock().unwrap().is_empty());
    assert!(
        text(&result).contains("the user declined"),
        "{}",
        text(&result)
    );
}

/// An executable outside the workspace the profile does not open, and the
/// path it is asked to run under.
fn outside_program() -> (tempfile::TempDir, CanonPath, String) {
    use std::os::unix::fs::PermissionsExt;
    let (dir, program, printed) = outside_file("#!/bin/sh\necho outside program ran\n");
    std::fs::set_permissions(program.as_path(), std::fs::Permissions::from_mode(0o755))
        .expect("executable mode");
    (dir, program, printed)
}

#[test]
fn a_program_the_sandbox_refuses_is_reported_as_the_sandbox_with_the_way_to_allow_it() {
    // catches: a sandbox denial of the program itself reported as a bare
    // `sandbox-exec: execvp() ... Operation not permitted` exit 71, which an
    // allowed call makes look like a permission failure.
    let workspace = tempfile::tempdir().expect("workspace");
    let (_keep, program, printed) = outside_program();
    let seat = Seat::answering(false);
    let run = run_with(&seat, workspace.path());

    let Err(ProcessError::Exit { result, .. }) = execute(&run, &spec(&run, &[&printed])) else {
        panic!("a refused program must still fail");
    };

    let report = text(&result);
    assert!(
        report.contains(&format!(
            "sandbox denied exec {}",
            program.as_path().display()
        )),
        "{report}"
    );
    assert!(report.contains("to allow it"), "{report}");
    assert!(
        !report.contains("permission rule can"),
        "the sandbox is not overridable by a permission rule: {report}"
    );
    assert_eq!(
        seat.asked(),
        vec![SandboxGrant::new(program, SandboxAccess::Exec)]
    );
}

#[test]
fn an_allowed_program_runs_again_under_the_widened_sandbox() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (_keep, _program, printed) = outside_program();
    let seat = Seat::answering(true);
    let run = run_with(&seat, workspace.path());

    let result = execute(&run, &spec(&run, &[&printed])).expect("the allowed program runs");

    assert!(
        text(&result).contains("outside program ran"),
        "{}",
        text(&result)
    );
}

#[test]
fn an_ordinary_failing_exit_is_not_labelled_a_sandbox_block() {
    // catches: false sandbox attribution of a command that merely failed.
    let workspace = tempfile::tempdir().expect("workspace");
    let seat = Seat::answering(true);
    let run = run_with(&seat, workspace.path());

    let Err(ProcessError::Exit { code, result }) = execute(
        &run,
        &spec(&run, &["/bin/sh", "-c", "echo oops >&2; exit 3"]),
    ) else {
        panic!("a failing command fails");
    };

    assert_eq!(code, Some(3));
    assert!(!text(&result).contains("sandbox"), "{}", text(&result));
    assert!(seat.asked().is_empty());
}

#[path = "support/errno_sandbox.rs"]
mod errno_sandbox;

#[path = "support/internal_sandbox.rs"]
mod internal_sandbox;
