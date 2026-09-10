#![cfg(windows)]

//! RED contract for one public process-execution composition on Windows.
//!
//! This representative path covers exact argv and handler stdin while a
//! short-lived descendant keeps the inherited stdout pipe open. The remaining
//! editor, secret-child, duplex cancellation, and failure seams belong to
//! later native batches.

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use ego_core::hook::EffectScope;
use ego_core::permit::{
    CanonPath, Confinement, EnvPolicy, ProcessExecutor, RunConfinement, SpawnSpec,
};
use ego_core::turn::CancelToken;
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SessionEvidenceStore,
};
use ego_tools::http::SystemResolver;

const DESCENDANT_LIFETIME: Duration = Duration::from_millis(250);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);

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
    let config = ExecRunConfig::new(
        policy,
        evidence,
        CancelToken::new(),
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
        ports,
    }
}

fn fixture_argv(extras: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut argv = vec![
        std::env::current_exe()
            .expect("test executable")
            .into_os_string(),
        OsString::from("--ignored"),
        OsString::from("--exact"),
        OsString::from("windows_process_ownership_fixture"),
        OsString::from("--nocapture"),
        OsString::from("--"),
        OsString::from("args-and-stdin"),
    ];
    argv.extend(extras);
    argv
}

fn fixture_spec(args: impl IntoIterator<Item = OsString>) -> SpawnSpec {
    SpawnSpec::new(
        fixture_argv(args),
        Some(CanonPath::new(
            std::fs::canonicalize(std::env::current_dir().expect("current directory"))
                .expect("canonical current directory"),
        )),
        None,
        EnvPolicy::Inherited,
    )
    .expect("fixture spawn spec")
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
    let result = fixture.runtime.block_on(tokio::time::timeout(
        COMPLETION_TIMEOUT,
        fixture
            .ports
            .process()
            .execute_handler(&spec, input, EffectScope::new(COMPLETION_TIMEOUT)),
    ));
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
