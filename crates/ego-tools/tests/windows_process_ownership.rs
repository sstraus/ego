#![cfg(windows)]

//! Native Windows contract for public process ownership and whole-tree cleanup.

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ego_core::credentials::SecretMaterial;
use ego_core::event::domain::CancelCause;
use ego_core::hook::EffectScope;
use ego_core::permit::{
    CanonPath, Confinement, DuplexEnd, DuplexSpawner, EnvPolicy, ProcessExecutor, RunConfinement,
    SpawnSpec,
};
use ego_core::turn::CancelToken;
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SecretChildError,
    SecretChildRequest, SessionEvidenceStore, run_editor_process, run_secret_child,
};
use ego_tools::http::SystemResolver;
use futures::io::AsyncReadExt as _;

const DESCENDANT_LIFETIME: Duration = Duration::from_millis(250);
const LATE_DESCENDANT_LIFETIME: Duration = Duration::from_secs(1);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
const MARKER_WAIT: Duration = Duration::from_secs(3);

struct ScenarioRuntime(tokio::runtime::Runtime);

impl ScenarioRuntime {
    fn new() -> Self {
        Self(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scenario runtime"),
        )
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.0.block_on(future)
    }
}

struct RunFixture {
    runtime: ScenarioRuntime,
    _evidence_root: tempfile::TempDir,
    cancel: CancelToken,
    ports: RunEffectPorts,
}

fn run_fixture() -> RunFixture {
    let runtime = ScenarioRuntime::new();
    let evidence_root = tempfile::tempdir().expect("evidence root");
    let evidence = SessionEvidenceStore::open(
        evidence_root.path(),
        ego_core::event::id::SessionId::new(),
        1024 * 1024,
    )
    .expect("session evidence");
    let workspace = CanonPath::new(
        std::fs::canonicalize(std::env::current_dir().expect("current directory"))
            .expect("canonical current directory"),
    );
    let policy =
        RunConfinement::new(Confinement::Off, false, workspace).expect("unconfined Windows run");
    let cancel = CancelToken::new();
    let config = ExecRunConfig::new(
        policy,
        evidence,
        cancel.clone(),
        ExecLimits::new(1024 * 1024, Duration::from_millis(500)).expect("process limits"),
        HttpCallTimeout::default(),
        None,
        Arc::new(SystemResolver::new()),
    );
    let (ports, _, _) = runtime
        .block_on(RunEffectPorts::for_run(config))
        .expect("Windows off run must construct process ports");
    RunFixture {
        runtime,
        _evidence_root: evidence_root,
        cancel,
        ports,
    }
}

fn fixture_argv(mode: &str, extras: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut argv = vec![
        std::env::current_exe()
            .expect("test executable")
            .into_os_string(),
        OsString::from("--ignored"),
        OsString::from("--exact"),
        OsString::from("windows_process_ownership_fixture"),
        OsString::from("--nocapture"),
        OsString::from("--test-threads=1"),
        OsString::from("--"),
        OsString::from(mode),
    ];
    argv.extend(extras);
    argv
}

fn fixture_spec(args: impl IntoIterator<Item = OsString>) -> SpawnSpec {
    fixture_spec_with_mode("args-and-stdin", args, None)
}

fn fixture_spec_with_mode(
    mode: &str,
    args: impl IntoIterator<Item = OsString>,
    timeout: Option<Duration>,
) -> SpawnSpec {
    SpawnSpec::new(
        fixture_argv(mode, args),
        Some(CanonPath::new(
            std::fs::canonicalize(std::env::current_dir().expect("current directory"))
                .expect("canonical current directory"),
        )),
        timeout,
        EnvPolicy::Inherited,
    )
    .expect("fixture spawn spec")
}

fn marker(path: &Path) {
    std::fs::write(path, b"started").expect("fixture marker");
}

fn descendant_started(path: &Path) -> PathBuf {
    path.with_extension("started")
}

fn wait_for_marker(path: &Path) {
    let deadline = Instant::now() + MARKER_WAIT;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "fixture did not write marker {path:?}");
}

fn assert_marker_absent_after(path: &Path) {
    assert_marker_absent_after_duration(path, DESCENDANT_LIFETIME);
}

fn assert_marker_absent_after_duration(path: &Path, lifetime: Duration) {
    thread::sleep(lifetime + Duration::from_millis(250));
    assert!(!path.exists(), "descendant survived cleanup: {path:?}");
}

fn marker_paths(args: &mut impl Iterator<Item = OsString>) -> (PathBuf, PathBuf) {
    let direct = PathBuf::from(args.next().expect("direct marker"));
    let descendant = PathBuf::from(args.next().expect("descendant marker"));
    (direct, descendant)
}

fn spawn_descendant(path: &Path) {
    spawn_descendant_with_mode(path, "tree-descendant");
}

fn spawn_descendant_with_mode(path: &Path, mode: &str) {
    Command::new(std::env::current_exe().expect("test executable"))
        .args([
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("windows_process_ownership_fixture"),
            OsString::from("--nocapture"),
            OsString::from("--test-threads=1"),
            OsString::from("--"),
            OsString::from(mode),
            path.as_os_str().to_os_string(),
        ])
        .stdin(Stdio::null())
        .spawn()
        .expect("descendant fixture");
    wait_for_marker(&descendant_started(path));
}

#[test]
#[ignore = "child fixture"]
fn windows_process_ownership_fixture() {
    let mut args = std::env::args_os().skip_while(|arg| arg != "--").skip(1);
    let mode = args.next().expect("fixture mode");

    match mode.to_string_lossy().as_ref() {
        "args-and-stdin" => {
            let mut input = Vec::new();
            std::io::stdin()
                .read_to_end(&mut input)
                .expect("fixture stdin");
            for (index, value) in args.enumerate() {
                println!("ARG[{index}]={:?}", value.to_string_lossy());
            }
            std::io::stdout()
                .write_all(b"INPUT=")
                .expect("input marker");
            std::io::stdout().write_all(&input).expect("input bytes");
            std::io::stdout().flush().expect("fixture output flush");
            Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--ignored",
                    "--exact",
                    "windows_process_ownership_fixture",
                    "--nocapture",
                    "--",
                    "descendant-marker",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("descendant fixture");
        }
        "descendant-marker" => {
            std::thread::sleep(DESCENDANT_LIFETIME);
            println!("DESCENDANT_DONE");
            std::io::stdout().flush().expect("descendant flush");
        }
        "tree-descendant" => {
            let path = PathBuf::from(args.next().expect("descendant marker path"));
            marker(&descendant_started(&path));
            thread::sleep(DESCENDANT_LIFETIME);
            marker(&path);
        }
        "tree-descendant-late" => {
            let path = PathBuf::from(args.next().expect("late descendant marker path"));
            marker(&descendant_started(&path));
            thread::sleep(LATE_DESCENDANT_LIFETIME);
            marker(&path);
        }
        "editor-natural" => {
            let (direct, descendant) = marker_paths(&mut args);
            spawn_descendant(&descendant);
            marker(&direct);
        }
        "secret-natural" => {
            let (direct, descendant) = marker_paths(&mut args);
            spawn_descendant(&descendant);
            marker(&direct);
            let mut input = Vec::new();
            std::io::stdin()
                .read_to_end(&mut input)
                .expect("secret fixture stdin");
            std::io::stdout()
                .write_all(&input)
                .expect("secret fixture stdout");
            std::io::stdout().flush().expect("secret fixture flush");
        }
        "editor-blocking" | "secret-blocking" => {
            let (direct, descendant) = marker_paths(&mut args);
            spawn_descendant(&descendant);
            marker(&direct);
            thread::sleep(Duration::from_secs(2));
        }
        "secret-timeout" => {
            let (direct, descendant) = marker_paths(&mut args);
            spawn_descendant_with_mode(&descendant, "tree-descendant-late");
            marker(&direct);
            thread::sleep(Duration::from_secs(2));
        }
        "duplex-natural" => {
            let (direct, descendant) = marker_paths(&mut args);
            spawn_descendant(&descendant);
            marker(&direct);
            std::io::stdout()
                .write_all(b"DUPLEX_DONE\0")
                .expect("duplex fixture output");
            std::io::stdout().flush().expect("duplex fixture flush");
        }
        "duplex-blocking" => {
            let (direct, descendant) = marker_paths(&mut args);
            spawn_descendant(&descendant);
            marker(&direct);
            std::io::stdout()
                .write_all(b"DUPLEX_READY\0")
                .expect("duplex fixture ready");
            std::io::stdout().flush().expect("duplex fixture flush");
            thread::sleep(Duration::from_secs(2));
        }
        "duplex-timeout" => {
            let (direct, descendant) = marker_paths(&mut args);
            spawn_descendant_with_mode(&descendant, "tree-descendant-late");
            marker(&direct);
            std::io::stdout()
                .write_all(b"DUPLEX_READY\0")
                .expect("duplex timeout fixture ready");
            std::io::stdout()
                .flush()
                .expect("duplex timeout fixture flush");
            thread::sleep(Duration::from_secs(2));
        }
        "duplex-drop" => {
            let (direct, descendant) = marker_paths(&mut args);
            spawn_descendant(&descendant);
            marker(&direct);
            std::io::stdout()
                .write_all(b"DUPLEX_READY\0")
                .expect("duplex drop fixture ready");
            std::io::stdout()
                .flush()
                .expect("duplex drop fixture flush");
            thread::sleep(Duration::from_millis(100));
        }
        other => panic!("unknown fixture mode {other:?}"),
    }
}

#[test]
fn public_process_path_preserves_args_and_stdin_until_owned_descendant_completes() {
    let fixture = run_fixture();
    let expected_args = [
        OsString::from(""),
        OsString::from("quote\"and spaces"),
        OsString::from("café😀"),
        OsString::from(r#"C:\trailing\"#),
    ];
    let input = b"stdin payload";
    let spec = fixture_spec(expected_args.iter().cloned());
    let result = fixture.runtime.block_on(async {
        tokio::time::timeout(
            COMPLETION_TIMEOUT,
            fixture.ports.process().execute_handler(
                &spec,
                input,
                EffectScope::new(COMPLETION_TIMEOUT),
            ),
        )
        .await
    });
    let result = result.expect("public process completion deadline");
    let result = result.expect("public process execution");
    let output = result
        .content
        .iter()
        .map(|block| block.text.as_str())
        .collect::<String>();
    for value in expected_args {
        assert!(
            output.contains(&format!("{:?}", value.to_string_lossy())),
            "child did not receive argument {value:?}; output was {output:?}"
        );
    }
    assert!(
        output.contains("INPUT=stdin payload"),
        "stdin was not preserved"
    );
    assert!(
        output.contains("DESCENDANT_DONE"),
        "public process path completed before the owned descendant"
    );
}

fn fixture_paths(root: &tempfile::TempDir, stem: &str) -> (PathBuf, PathBuf) {
    (
        root.path().join(format!("{stem}-direct")),
        root.path().join(format!("{stem}-descendant")),
    )
}

fn public_fixture_command(mode: &str, direct: &Path, descendant: &Path) -> Vec<OsString> {
    fixture_argv(
        mode,
        [
            direct.as_os_str().to_os_string(),
            descendant.as_os_str().to_os_string(),
        ],
    )
}

#[test]
fn public_editor_path_completes_and_drops_a_natural_descendant_tree() {
    let root = tempfile::tempdir().expect("editor fixture root");
    let (direct, descendant) = fixture_paths(&root, "editor-natural");
    let result = ScenarioRuntime::new().block_on(run_editor_process(
        public_fixture_command("editor-natural", &direct, &descendant),
        CancelToken::new(),
    ));
    assert!(result.is_ok(), "editor unexpectedly failed: {result:?}");
    wait_for_marker(&direct);
    wait_for_marker(&descendant);
}

#[test]
fn public_editor_cancel_cleans_the_complete_descendant_tree() {
    let root = tempfile::tempdir().expect("editor cancellation fixture root");
    let (direct, descendant) = fixture_paths(&root, "editor-cancel");
    let cancel = CancelToken::new();
    let cancel_for_observer = cancel.clone();
    let observer_path = direct.clone();
    let observer = thread::spawn(move || {
        wait_for_marker(&observer_path);
        cancel_for_observer.cancel(CancelCause::User);
    });
    let result = ScenarioRuntime::new().block_on(run_editor_process(
        public_fixture_command("editor-blocking", &direct, &descendant),
        cancel,
    ));
    observer.join().expect("editor observer");
    assert!(
        matches!(result, Err(ego_tools::exec::EditorProcessError::Cancelled)),
        "unexpected editor cancellation result: {result:?}"
    );
    assert_marker_absent_after(&descendant);
}

fn secret_request(
    mode: &str,
    direct: &Path,
    descendant: &Path,
    cancel: CancelToken,
    stdin: Option<SecretMaterial>,
    command_timeout: Duration,
) -> SecretChildRequest {
    SecretChildRequest {
        argv: public_fixture_command(mode, direct, descendant),
        cwd: Some(CanonPath::new(
            std::fs::canonicalize(std::env::current_dir().expect("current directory"))
                .expect("canonical current directory"),
        )),
        stdin,
        cancel,
        run_deadline: None,
        command_timeout,
        termination_grace: Duration::from_millis(500),
    }
}

#[test]
fn public_secret_child_path_returns_opaque_output_and_cleans_natural_tree() {
    let root = tempfile::tempdir().expect("secret fixture root");
    let (direct, descendant) = fixture_paths(&root, "secret-natural");
    let result = ScenarioRuntime::new().block_on(run_secret_child(secret_request(
        "secret-natural",
        &direct,
        &descendant,
        CancelToken::new(),
        Some(SecretMaterial::new(b"secret-input\n".to_vec())),
        COMPLETION_TIMEOUT,
    )));
    let output = result.expect("secret child natural completion");
    assert!(
        output
            .stdout()
            .expose_secret_bytes()
            .windows(b"secret-input\n".len())
            .any(|window| window == b"secret-input\n"),
        "the fixture output did not contain its opaque stdin"
    );
    wait_for_marker(&direct);
    wait_for_marker(&descendant);
}

#[test]
fn public_secret_child_cancel_cleans_the_complete_descendant_tree() {
    let root = tempfile::tempdir().expect("secret cancellation fixture root");
    let (direct, descendant) = fixture_paths(&root, "secret-cancel");
    let cancel = CancelToken::new();
    let cancel_for_observer = cancel.clone();
    let observer_path = direct.clone();
    let observer = thread::spawn(move || {
        wait_for_marker(&observer_path);
        cancel_for_observer.cancel(CancelCause::User);
    });
    let result = ScenarioRuntime::new().block_on(run_secret_child(secret_request(
        "secret-blocking",
        &direct,
        &descendant,
        cancel,
        None,
        COMPLETION_TIMEOUT,
    )));
    observer.join().expect("secret observer");
    assert!(
        matches!(result, Err(SecretChildError::Cancelled)),
        "unexpected secret cancellation result: {result:?}"
    );
    assert_marker_absent_after(&descendant);
}

#[test]
fn public_secret_child_timeout_cleans_the_complete_descendant_tree() {
    let root = tempfile::tempdir().expect("secret timeout fixture root");
    let (direct, descendant) = fixture_paths(&root, "secret-timeout");
    let observer_path = direct.clone();
    let observer = thread::spawn(move || wait_for_marker(&observer_path));
    let result = ScenarioRuntime::new().block_on(run_secret_child(secret_request(
        "secret-timeout",
        &direct,
        &descendant,
        CancelToken::new(),
        None,
        Duration::from_secs(2),
    )));
    observer.join().expect("secret timeout observer");
    assert!(
        matches!(result, Err(SecretChildError::CommandTimeout)),
        "unexpected secret timeout result: {result:?}"
    );
    assert_marker_absent_after_duration(&descendant, LATE_DESCENDANT_LIFETIME);
}

fn duplex_spec(
    mode: &str,
    direct: &Path,
    descendant: &Path,
    timeout: Option<Duration>,
) -> SpawnSpec {
    fixture_spec_with_mode(
        mode,
        [
            direct.as_os_str().to_os_string(),
            descendant.as_os_str().to_os_string(),
        ],
        timeout,
    )
}

fn read_duplex_marker(
    fixture: &RunFixture,
    handle: &mut ego_core::permit::DuplexHandle,
    expected: &[u8],
) {
    let mut output = Vec::new();
    loop {
        let mut byte = [0];
        fixture
            .runtime
            .block_on(handle.read_exact(&mut byte))
            .expect("duplex fixture marker");
        output.push(byte[0]);
        assert!(output.len() < 4096, "duplex fixture marker was not bounded");
        if byte[0] == 0 {
            break;
        }
    }
    assert!(
        output.ends_with(expected),
        "duplex output did not end with {expected:?}: {output:?}"
    );
}

fn read_duplex_ready(fixture: &RunFixture, handle: &mut ego_core::permit::DuplexHandle) {
    read_duplex_marker(fixture, handle, b"DUPLEX_READY\0");
}

#[test]
fn public_duplex_path_completes_naturally_and_cleans_the_descendant_tree() {
    let root = tempfile::tempdir().expect("duplex natural fixture root");
    let (direct, descendant) = fixture_paths(&root, "duplex-natural");
    let fixture = run_fixture();
    let mut handle = fixture
        .runtime
        .block_on(fixture.ports.duplex_handle().spawn(duplex_spec(
            "duplex-natural",
            &direct,
            &descendant,
            None,
        )))
        .expect("duplex natural spawn");
    read_duplex_marker(&fixture, &mut handle, b"DUPLEX_DONE\0");
    let settlement = fixture.runtime.block_on(handle.settle());
    assert!(matches!(settlement.end(), DuplexEnd::Exited { code: 0 }));
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    wait_for_marker(&descendant);
}

#[test]
fn public_duplex_cancel_cleans_the_complete_descendant_tree() {
    let root = tempfile::tempdir().expect("duplex cancellation fixture root");
    let (direct, descendant) = fixture_paths(&root, "duplex-cancel");
    let fixture = run_fixture();
    let mut handle = fixture
        .runtime
        .block_on(fixture.ports.duplex_handle().spawn(duplex_spec(
            "duplex-blocking",
            &direct,
            &descendant,
            None,
        )))
        .expect("duplex cancellation spawn");
    read_duplex_ready(&fixture, &mut handle);
    fixture.cancel.cancel(CancelCause::User);
    let settlement = fixture.runtime.block_on(handle.settle());
    assert!(matches!(settlement.end(), DuplexEnd::Cancelled));
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_marker_absent_after(&descendant);
}

#[test]
fn public_duplex_timeout_cleans_the_complete_descendant_tree() {
    let root = tempfile::tempdir().expect("duplex timeout fixture root");
    let (direct, descendant) = fixture_paths(&root, "duplex-timeout");
    let fixture = run_fixture();
    let mut handle = fixture
        .runtime
        .block_on(fixture.ports.duplex_handle().spawn(duplex_spec(
            "duplex-timeout",
            &direct,
            &descendant,
            Some(Duration::from_millis(500)),
        )))
        .expect("duplex timeout spawn");
    read_duplex_ready(&fixture, &mut handle);
    let settlement = fixture.runtime.block_on(handle.settle());
    assert!(matches!(
        settlement.end(),
        DuplexEnd::TimedOut { timeout } if *timeout == Duration::from_millis(500)
    ));
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_marker_absent_after_duration(&descendant, LATE_DESCENDANT_LIFETIME);
}

#[test]
fn public_duplex_interrupt_cleans_the_complete_descendant_tree() {
    let root = tempfile::tempdir().expect("duplex interrupt fixture root");
    let (direct, descendant) = fixture_paths(&root, "duplex-interrupt");
    let fixture = run_fixture();
    let mut handle = fixture
        .runtime
        .block_on(fixture.ports.duplex_handle().spawn(duplex_spec(
            "duplex-blocking",
            &direct,
            &descendant,
            None,
        )))
        .expect("duplex interrupt spawn");
    read_duplex_ready(&fixture, &mut handle);
    let settlement = fixture.runtime.block_on(handle.interrupt());
    assert!(matches!(settlement.end(), DuplexEnd::Interrupted));
    assert!(matches!(
        settlement.cleanup(),
        ego_core::permit::CleanupCertainty::Complete
    ));
    assert_marker_absent_after(&descendant);
}

#[test]
fn public_duplex_drop_cleans_the_complete_descendant_tree() {
    let root = tempfile::tempdir().expect("duplex drop fixture root");
    let (direct, descendant) = fixture_paths(&root, "duplex-drop");
    let fixture = run_fixture();
    let mut handle = fixture
        .runtime
        .block_on(fixture.ports.duplex_handle().spawn(duplex_spec(
            "duplex-drop",
            &direct,
            &descendant,
            None,
        )))
        .expect("duplex drop spawn");
    read_duplex_ready(&fixture, &mut handle);
    drop(handle);
    assert_marker_absent_after(&descendant);
}
