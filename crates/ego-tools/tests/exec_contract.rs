//! contract for argv-native execution, confinement, and process evidence.
//!
//! The provider is never involved here. Each process is a real argv-native
//! child launched through the same run-owned guarded process path production uses.
//! Platform evidence is accepted only when paired controls demonstrate it;
//! unsupported guarantees must return `MissingCapabilities` before spawn.
//!
//! Direct backend tests prove lifecycle, evidence, and paired confinement
//! behaviour. The composite test below also proves the production
//! ExecTool -> PolicyEngine -> Authorized -> GuardedEffects -> process adapter
//! path; direct backend calls never stand in for that authorization proof.

use std::ffi::OsString;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::event::domain::CancelCause;
use ego_core::event::id::SessionId;
use ego_core::permit::{
    BackendIdentity, CanonPath, Classification, Confinement, EnvName, EnvPolicy, ExecCaps,
    ExecObservation, Op, PolicyDenied, PolicyDigest, RunConfinement, SpawnSpec, StreamProvenance,
    ToolOutputProvenance, ToolResult,
};
use ego_core::store::evidence::{EvidenceRef, EvidenceStore as _};
use ego_core::turn::CancelToken;
use ego_core::turn::{ToolEffectPorts, Tools as TurnTools};
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, ProcessError, RunEffectPorts, SessionEvidenceStore,
};
use ego_tools::fs::OsFs;
use ego_tools::http::SystemResolver;
use ego_tools::tools::builtins;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test Tokio runtime")
        .block_on(future)
}

fn decode_standard_base64(encoded: &str) -> Vec<u8> {
    fn value(byte: u8) -> u8 {
        match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            other => panic!("invalid STANDARD base64 byte: {other:?}"),
        }
    }

    assert_eq!(encoded.len() % 4, 0, "STANDARD base64 is quartet-aligned");
    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
    for quartet in encoded.as_bytes().as_chunks::<4>().0 {
        let first = value(quartet[0]);
        let second = value(quartet[1]);
        decoded.push((first << 2) | (second >> 4));
        if quartet[2] != b'=' {
            let third = value(quartet[2]);
            decoded.push((second << 4) | (third >> 2));
            if quartet[3] != b'=' {
                let fourth = value(quartet[3]);
                decoded.push((third << 6) | fourth);
            }
        }
    }
    decoded
}

struct Run {
    _evidence_root: tempfile::TempDir,
    evidence: SessionEvidenceStore,
    ports: RunEffectPorts,
    observation: ExecObservation,
    cancel: CancelToken,
}

fn cwd() -> CanonPath {
    CanonPath::new(std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap())
}

fn exact_executable_rule(executable: &str) -> ego_core::permit::rule::Rule {
    let quoted = executable.replace('\\', "\\\\").replace('"', "\\\"");
    ego_core::permit::rule::Rule::load(&format!("exec(\"{quoted}\" *)"))
        .expect("the exact executable rule parses")
}

fn run_confinement(level: Confinement, offline: bool, workspace: CanonPath) -> RunConfinement {
    RunConfinement::new(level, offline, workspace).unwrap()
}

/// One attempt to open a run, keeping the session evidence store whether the
/// backend was accepted or refused. A refusal cites evidence stored in that
/// session, so a test that drops the store cannot read what it cites.
struct Attempt {
    evidence_root: tempfile::TempDir,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    outcome: Result<
        (
            RunEffectPorts,
            ExecObservation,
            Option<ego_core::store::evidence::EvidenceRef>,
        ),
        ProcessError,
    >,
}

impl Attempt {
    fn into_run(self) -> Result<Run, ProcessError> {
        let (ports, observation, _) = self.outcome?;
        Ok(Run {
            _evidence_root: self.evidence_root,
            evidence: self.evidence,
            ports,
            observation,
            cancel: self.cancel,
        })
    }
}

struct ConfigFixture {
    evidence_root: tempfile::TempDir,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    session: SessionId,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    config: ExecRunConfig,
}

fn config_fixture(
    level: Confinement,
    offline: bool,
    workspace: CanonPath,
    hard_cap: u64,
    preview: usize,
) -> ConfigFixture {
    config_fixture_for(
        run_confinement(level, offline, workspace),
        hard_cap,
        preview,
    )
}

fn config_fixture_for(policy: RunConfinement, hard_cap: u64, preview: usize) -> ConfigFixture {
    let evidence_root = tempfile::tempdir().expect("evidence root");
    let session = SessionId::new();
    let evidence = SessionEvidenceStore::open(evidence_root.path(), session, hard_cap)
        .expect("session evidence store");
    let cancel = CancelToken::new();
    let limits = ExecLimits::new(preview, Duration::from_millis(250)).expect("exec limits");
    let http_call_timeout =
        HttpCallTimeout::new(Duration::from_secs(1)).expect("HTTP call timeout");
    let config = ExecRunConfig::new(
        policy,
        evidence.clone(),
        cancel.clone(),
        limits,
        http_call_timeout,
        None,
        Arc::new(SystemResolver::new()),
    );
    ConfigFixture {
        evidence_root,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        session,
        evidence,
        cancel,
        config,
    }
}

#[cfg(target_os = "macos")]
fn attempt_run(
    level: Confinement,
    offline: bool,
    workspace: CanonPath,
    hard_cap: u64,
    preview: usize,
) -> Attempt {
    attempt_policy(
        run_confinement(level, offline, workspace),
        hard_cap,
        preview,
    )
}

fn attempt_policy(policy: RunConfinement, hard_cap: u64, preview: usize) -> Attempt {
    let fixture = config_fixture_for(policy, hard_cap, preview);
    let outcome = block_on(RunEffectPorts::for_run(fixture.config));
    Attempt {
        evidence_root: fixture.evidence_root,
        evidence: fixture.evidence,
        cancel: fixture.cancel,
        outcome,
    }
}

fn open_run(
    level: Confinement,
    offline: bool,
    workspace: CanonPath,
    hard_cap: u64,
    preview: usize,
) -> Result<Run, ProcessError> {
    open_policy(
        run_confinement(level, offline, workspace),
        hard_cap,
        preview,
    )
}

fn open_policy(policy: RunConfinement, hard_cap: u64, preview: usize) -> Result<Run, ProcessError> {
    let attempt = attempt_policy(policy, hard_cap, preview);
    if let Err(ProcessError::MissingCapabilities(missing)) = &attempt.outcome
        && let Some(reference) = missing.probe_evidence()
    {
        eprintln!(
            "confinement probe evidence: {}",
            String::from_utf8_lossy(&read_evidence_pages(&attempt.evidence, reference))
        );
    }
    attempt.into_run()
}

/// The one way a supported host may decline to demonstrate a confinement
/// guarantee.
///
/// `MissingCapabilities` used to be an ordinary accepted outcome here, so the
/// suite stayed green for months while `workspace` was refused on every macOS
/// host. An installed backend must now prove the guarantee. A host that has no
/// backend at all — no `sandbox-exec`, no usable Landlock ABI — has to say so
/// by name, because that is a statement about the machine, not about ego.
const BACKEND_ABSENT: &str = "EGO_TEST_CONFINEMENT_BACKEND_ABSENT";

/// Open a run that requires real confinement, or `None` on a declared
/// backend-less host, where the refusal itself is the contract.
fn open_confined_run(
    level: Confinement,
    offline: bool,
    workspace: CanonPath,
    hard_cap: u64,
    preview: usize,
) -> Option<Run> {
    match open_run(level, offline, workspace, hard_cap, preview) {
        Ok(run) => Some(run),
        Err(ProcessError::MissingCapabilities(missing))
            if cfg!(not(any(target_os = "linux", target_os = "macos")))
                || std::env::var_os(BACKEND_ABSENT).is_some() =>
        {
            assert_eq!(
                missing.required(),
                required_caps(level, offline),
                "a refusal must still name the exact guarantees the run asked for"
            );
            None
        }
        Err(error) => panic!(
            "{level} confinement must be demonstrated on this host: {error}. \
             Set {BACKEND_ABSENT}=1 only if this machine has no confinement backend."
        ),
    }
}

fn required_caps(level: Confinement, offline: bool) -> ExecCaps {
    let filesystem = match level {
        Confinement::Ro => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED),
        Confinement::Workspace => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
        Confinement::Off => ExecCaps::empty(),
    };
    if offline {
        filesystem.union(ExecCaps::NO_IP_NETWORK)
    } else {
        filesystem
    }
}

fn execute(run: &Run, spec: SpawnSpec) -> Result<(), ProcessError> {
    block_on(run.ports.process().execute(&spec)).map(|_| ())
}

fn assert_two_process_streams_with_full_output(result: &ToolResult) {
    let process_blocks = result
        .content
        .iter()
        .filter(|block| matches!(&block.provenance, ToolOutputProvenance::Process { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        process_blocks.len(),
        2,
        "completed execution must expose exactly stdout and stderr process blocks"
    );
    assert_eq!(
        process_blocks
            .iter()
            .filter(|block| {
                matches!(
                    &block.provenance,
                    ToolOutputProvenance::Process {
                        stream: StreamProvenance::Stdout,
                        ..
                    }
                )
            })
            .count(),
        1,
        "completed execution must expose one stdout block"
    );
    assert_eq!(
        process_blocks
            .iter()
            .filter(|block| {
                matches!(
                    &block.provenance,
                    ToolOutputProvenance::Process {
                        stream: StreamProvenance::Stderr,
                        ..
                    }
                )
            })
            .count(),
        1,
        "completed execution must expose one stderr block"
    );
    assert!(
        process_blocks
            .iter()
            .all(|block| block.full_output.is_some()),
        "both process streams must retain full evidence references"
    );
}

fn read_evidence_pages(evidence: &SessionEvidenceStore, reference: &EvidenceRef) -> Vec<u8> {
    let mut offset = 0_u64;
    let mut bytes = Vec::new();
    loop {
        let request = ego_core::store::evidence::EvidenceRead::new(reference.clone(), offset)
            .expect("page offset is within the complete reference");
        let page = block_on(evidence.read_page(&request)).expect("evidence page");
        assert_eq!(page.reference, *reference);
        assert_eq!(page.offset, offset);
        assert!(page.bytes.len() <= ego_core::store::evidence::EVIDENCE_PAGE_BYTES);
        bytes.extend_from_slice(&page.bytes);
        match page.next_offset {
            Some(next) => {
                assert!(next > offset, "evidence pagination must advance");
                offset = next;
            }
            None => break,
        }
    }
    bytes
}

fn fixture_arg_spec(
    name: &str,
    args: impl IntoIterator<Item = OsString>,
    timeout: Option<Duration>,
    workspace: CanonPath,
) -> SpawnSpec {
    fixture_arg_spec_with_env(name, args, timeout, workspace, EnvPolicy::Inherited)
}

fn fixture_arg_spec_with_env(
    name: &str,
    args: impl IntoIterator<Item = OsString>,
    timeout: Option<Duration>,
    workspace: CanonPath,
    env: EnvPolicy,
) -> SpawnSpec {
    let mut argv = vec![
        OsString::from(std::env::current_exe().unwrap()),
        OsString::from("--ignored"),
        OsString::from("--exact"),
        OsString::from(name),
        OsString::from("--nocapture"),
        OsString::from("--"),
    ];
    argv.extend(args);
    SpawnSpec::new(argv, Some(workspace), timeout, env).unwrap()
}

fn fixture_spec(
    name: &str,
    args: &[&std::path::Path],
    timeout: Option<Duration>,
    workspace: CanonPath,
) -> SpawnSpec {
    fixture_arg_spec(
        name,
        args.iter()
            .map(|arg| OsString::from(arg.to_str().expect("fixture path is UTF-8"))),
        timeout,
        workspace,
    )
}

/// One mutation a confined child can actually reach.
///
/// Confinement admits `process-exec*` only inside the runtime roots and the
/// workspace, and the test executable lives under `target/`, in neither. So a
/// mutation driven by this crate's own fixture binary could never run under
/// `ro` or `workspace` — which is how every confined assertion here used to
/// settle for a capability refusal instead. These are the installed utilities
/// the backend's own paired probes use, reached through the same production
/// exec path.
fn mutation_spec(operation: &str, paths: &[&std::path::Path], workspace: CanonPath) -> SpawnSpec {
    #[cfg(windows)]
    if operation == "create" {
        return fixture_spec("exec_contract_fixture_creates", paths, None, workspace);
    }

    let path = |index: usize| {
        OsString::from(
            paths
                .get(index)
                .unwrap_or_else(|| panic!("{operation} needs a path at {index}"))
                .to_str()
                .expect("fixture path is UTF-8"),
        )
    };
    let argv = match operation {
        "create" => vec![OsString::from("/usr/bin/touch"), path(0)],
        "overwrite" => vec![OsString::from("/bin/cp"), path(0), path(1)],
        "truncate" => vec![
            OsString::from("/usr/bin/truncate"),
            OsString::from("-s"),
            OsString::from("0"),
            path(0),
        ],
        "remove" => vec![OsString::from("/bin/rm"), path(0)],
        "rename" => vec![OsString::from("/bin/mv"), path(0), path(1)],
        "link" => vec![OsString::from("/bin/ln"), path(0), path(1)],
        other => panic!("unknown mutation operation: {other}"),
    };
    SpawnSpec::new(argv, Some(workspace), None, EnvPolicy::Inherited)
        .expect("a mutation specification is valid before launch")
}

#[test]
#[ignore]
fn exec_contract_fixture_emits_both_streams() {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    for _ in 0..4096 {
        stdout.write_all(b"stdout-fixture\n").unwrap();
        stderr.write_all(b"stderr-fixture\n").unwrap();
    }
    stdout.flush().unwrap();
    stderr.flush().unwrap();
}

#[test]
#[ignore]
fn exec_contract_fixture_emits_binary_streams() {
    std::io::stdout()
        .write_all(&[0xff, 0xfe, 0x00, b'o', b'u', b't'])
        .unwrap();
    std::io::stderr()
        .write_all(&[0x80, 0x81, 0x00, b'e', b'r', b'r'])
        .unwrap();
}

#[test]
#[ignore]
fn exec_contract_fixture_emits_until_killed() {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    loop {
        stdout.write_all(b"stdout-unbounded\n").unwrap();
        stderr.write_all(b"stderr-unbounded\n").unwrap();
        stdout.flush().unwrap();
        stderr.flush().unwrap();
    }
}

#[test]
#[ignore]
fn exec_contract_fixture_emits_both_streams_and_exits_nonzero() {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    for _ in 0..4096 {
        stdout.write_all(b"exit-stdout\n").unwrap();
        stderr.write_all(b"exit-stderr\n").unwrap();
    }
    stdout.flush().unwrap();
    stderr.flush().unwrap();
    std::process::exit(17);
}

#[cfg(unix)]
#[test]
#[ignore]
fn exec_contract_fixture_emits_both_streams_and_signals() {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    for _ in 0..4096 {
        stdout.write_all(b"signal-stdout\n").unwrap();
        stderr.write_all(b"signal-stderr\n").unwrap();
    }
    stdout.flush().unwrap();
    stderr.flush().unwrap();
    let pid = std::process::id().to_string();
    let _ = Command::new("/bin/kill")
        .args(["-TERM", pid.as_str()])
        .status();
    unreachable!("SIGTERM terminates this fixture");
}

#[test]
#[ignore]
fn exec_contract_fixture_echoes_argument() {
    let arg = std::env::args()
        .skip_while(|arg| arg != "--")
        .nth(1)
        .expect("the fixture receives one argument");
    print!("{arg}");
}

#[test]
#[ignore]
fn exec_contract_fixture_reads() {
    let path = std::env::args()
        .skip_while(|arg| arg != "--")
        .nth(1)
        .expect("the fixture receives a read path");
    let bytes = std::fs::read(path).expect("read fixture target");
    std::io::stdout().write_all(&bytes).unwrap();
}

#[test]
#[ignore]
fn exec_contract_fixture_creates() {
    let path = std::env::args_os()
        .skip_while(|arg| arg != "--")
        .nth(1)
        .expect("the fixture receives a create path");
    std::fs::File::create(path).expect("create fixture target");
}

#[test]
#[ignore]
fn exec_contract_fixture_reports_environment_presence_without_values() {
    let args = std::env::args().skip_while(|arg| arg != "--").skip(1);
    for name in args {
        println!(
            "{name}={}",
            if std::env::var_os(&name).is_some() {
                "present"
            } else {
                "absent"
            }
        );
    }
}

#[test]
#[ignore]
fn exec_contract_fixture_consumes_stdin_to_eof() {
    let mut stdin = std::io::stdin().lock();
    let mut bytes = Vec::new();
    stdin.read_to_end(&mut bytes).unwrap();
    print!("stdin-eof:{}", bytes.len());
}

#[test]
#[ignore]
fn exec_contract_fixture_exits_nonzero() {
    std::process::exit(17);
}

#[cfg(unix)]
#[test]
#[ignore]
fn exec_contract_fixture_terminates_by_signal() {
    let pid = std::process::id().to_string();
    let _ = Command::new("/bin/kill")
        .args(["-TERM", pid.as_str()])
        .status();
    unreachable!("the signal fixture must not return normally");
}

#[cfg(unix)]
#[test]
#[ignore]
fn exec_contract_fixture_rejects_inherited_fds() {
    for fd in 3..=64 {
        if std::path::Path::new(&format!("/dev/fd/{fd}")).exists() {
            std::process::exit(29);
        }
    }
    print!("fd-clean");
}

#[test]
#[ignore]
fn exec_contract_fixture_holds_a_descendant() {
    let pid_path = std::env::args()
        .skip_while(|arg| arg != "--")
        .nth(1)
        .expect("the fixture receives a pid path");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "exec_contract_fixture_emits_until_killed",
            "--nocapture",
        ])
        .spawn()
        .expect("sleep fixture executable");
    std::fs::write(pid_path, child.id().to_string()).expect("write child pid");
    let _ = child.wait();
}

#[test]
#[ignore]
fn exec_contract_fixture_connects() {
    let mut args = std::env::args().skip_while(|arg| arg != "--").skip(1);
    let protocol = args.next().expect("network protocol");
    let endpoint = args.next().expect("network endpoint");
    match protocol.as_str() {
        "tcp" => {
            TcpStream::connect(endpoint).expect("tcp control");
        }
        "udp" => {
            let socket = UdpSocket::bind("0.0.0.0:0").expect("udp socket");
            socket.send_to(b"probe", endpoint).expect("udp control");
        }
        other => panic!("unknown network fixture protocol: {other}"),
    }
}

/// The envelope is not merely accepted by the dispatcher — a real child spawned
/// with it delivers the sentinel.
///
/// This is the test the original defect needed. The probe spawns
/// `current_exe()`, which inside a test binary is libtest; the old envelope was
/// parsed there as a test-name filter, so the child ran zero tests, exited 0,
/// and connected to nothing. Every offline assertion in this file then passed
/// by taking its "the platform refused to promise it" arm. Asserting the
/// dispatcher parses an argv proves nothing about that; only watching the bytes
/// arrive from a separate process does.
///
/// Loopback only, deliberately: this proves the envelope reaches its host, not
/// which address classes this machine can pair on.
///
/// The envelope is spelled out here because the production builder is private
/// to `probe.rs`. That is a two-part contract, not a gap: this test pins "the
/// shape a libtest host dispatches", and `the_probe_envelope_is_the_one_both_
/// hosts_dispatch` pins "the shape production emits" to the same words. Change
/// the builder alone and that unit test fails.
#[cfg(target_os = "macos")]
#[test]
fn the_probe_envelope_delivers_its_sentinel_from_a_real_child() {
    use std::io::Read as _;
    use std::process::Command;

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("probe control listener");
    let endpoint = listener.local_addr().expect("probe control endpoint");
    let sentinel = format!("exec-contract-envelope:{}", endpoint.port());
    let status = Command::new(std::env::current_exe().expect("the test executable"))
        .args(["--ignored", "--exact", "__ego_internal_network_probe_v1"])
        .args(["--nocapture", "--", "tcp"])
        .args([endpoint.to_string(), sentinel.clone()])
        .status()
        .expect("the probe child runs");
    assert!(
        status.success(),
        "the probe child reported failure: {status}"
    );

    let (mut stream, _) = listener.accept().expect("the probe child never connected");
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("read timeout");
    let mut received = vec![0_u8; sentinel.len()];
    stream
        .read_exact(&mut received)
        .expect("the probe child connected but sent nothing");
    assert_eq!(received, sentinel.as_bytes());
}

/// The control child of the macOS offline probe, hosted by libtest.
///
/// The probe spawns `current_exe()` with its private envelope. Inside a test
/// binary that executable is libtest, so without this the envelope was parsed
/// as a test-name filter: the child ran zero tests, exited 0, and never
/// connected. The paired control then timed out, `NO_IP_NETWORK` was never
/// measured, and every offline assertion in this file passed by taking the
/// "the platform refused to promise it" arm — proving nothing, on a platform
/// where the shipped binary does deliver the guarantee.
///
/// The name is the token itself because `--exact` matches it, and the body
/// calls the production dispatcher rather than reimplementing the connect, so
/// what the tests measure cannot drift from what a real run measures.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "hosts the offline probe's control child; not a test of its own"]
fn __ego_internal_network_probe_v1() {
    let delivered = ego_tools::exec::dispatch_network_probe(std::env::args_os())
        .expect("libtest was handed the probe envelope but did not recognise it");
    // A failing assertion exits non-zero, which is exactly the signal the
    // confined half of the paired probe is looking for.
    assert!(delivered, "the probe child could not deliver its sentinel");
}

/// A libtest host handed the Landlock helper envelope runs the helper and
/// nothing else.
///
/// The old envelope reached libtest as test-name filters, so a helper child
/// ran the real tests matching `workspace`, each of which spawned more helper
/// children: an unbounded re-exec storm that outlived the test run. Here the
/// helper refuses a malformed protocol before any effect, so the child must
/// exit at once, after selecting exactly one test, with the production
/// refusal on stderr. A deadline bounds the child in case it regresses.
///
/// The envelope is spelled out because the builder is private to
/// `landlock.rs`; `the_helper_envelope_is_the_one_both_hosts_dispatch` pins
/// the builder to the same words.
#[cfg(target_os = "linux")]
#[test]
fn a_libtest_host_runs_only_the_landlock_helper_for_its_envelope() {
    use std::process::Stdio;

    let mut child = Command::new(std::env::current_exe().expect("the test executable"))
        .args(["--ignored", "--exact", "__ego_internal_landlock_exec_v1"])
        .args(["--nocapture", "--", "not-a-level"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the helper child runs");
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("helper child status") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the helper child did not exit: libtest ran more than the helper");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("stdout pipe")
        .read_to_string(&mut stdout)
        .expect("helper stdout");
    child
        .stderr
        .take()
        .expect("stderr pipe")
        .read_to_string(&mut stderr)
        .expect("helper stderr");

    assert!(!status.success(), "a refused helper must fail: {stdout}");
    assert!(
        stdout.contains("running 1 test\n"),
        "libtest must select only the helper: {stdout}"
    );
    assert!(
        stderr.contains("ego Landlock helper refused execution"),
        "the production dispatcher must claim the envelope: {stderr}"
    );
}

/// The Linux Landlock helper, hosted by libtest.
///
/// The backend re-execs `current_exe()` with its private envelope. Inside a
/// test binary that executable is libtest, so without this the protocol was
/// parsed as test-name filters: the child ran whichever real tests matched,
/// and every confined probe timed out instead of running `cat` or `touch`.
/// The body calls the production dispatcher, so the confinement these tests
/// measure is the one a real run installs. Success never returns: the helper
/// replaces this process with the confined target.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "hosts the Landlock helper; not a test of its own"]
fn __ego_internal_landlock_exec_v1() {
    let refused = ego_tools::exec::dispatch_landlock_helper(std::env::args_os())
        .expect("libtest was handed the helper envelope but did not recognise it");
    // A refusal exits non-zero, which is what the paired probe looks for.
    panic!("the Landlock helper refused execution: {refused:?}");
}

#[test]
fn guarded_exec_runs_the_exact_argv_without_reparse_or_mutation() {
    let workspace = cwd();
    let run = open_run(Confinement::Off, false, workspace, 1024 * 1024, 1024).unwrap();
    let prepared = fixture_arg_spec(
        "exec_contract_fixture_echoes_argument",
        [OsString::from("literal * ; $(false)")],
        None,
        cwd(),
    );
    let out = block_on(run.ports.process().execute(&prepared)).unwrap();

    let debug = format!("{out:?}");
    assert!(debug.contains("literal * ; $(false)"));
}

#[cfg(unix)]
#[test]
fn analyzed_target_mutation_executes_neither_old_nor_replacement_image() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("image");
    let old_marker = directory.path().join("old-marker");
    let new_marker = directory.path().join("new-marker");
    std::fs::write(&image, b"#!/bin/sh\nprintf old > \"$1\"\n").unwrap();
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = CanonPath::new(std::fs::canonicalize(directory.path()).unwrap());
    let run = open_run(Confinement::Off, false, root.clone(), 1024 * 1024, 1024).unwrap();
    let spec = SpawnSpec::new(
        vec![
            image.clone().into_os_string(),
            old_marker.clone().into_os_string(),
            new_marker.clone().into_os_string(),
        ],
        Some(root),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let captured = run.ports.process().observe(&spec).unwrap();
    assert!(captured.identity().unwrap().complete());
    std::fs::write(&image, b"#!/bin/sh\nprintf new > \"$2\"\n").unwrap();
    let error = block_on(run.ports.process().execute(&captured)).unwrap_err();
    assert!(error.to_string().contains("identity changed"));
    assert!(!old_marker.exists());
    assert!(!new_marker.exists());
}

#[test]
fn exec_tool_preparation_policy_authorization_and_guarded_execution_preserve_identity() {
    let root = cwd();
    let executable = std::env::current_exe().unwrap();
    let executable_text = executable.to_str().expect("test executable is UTF-8");
    let allow = vec![exact_executable_rule(executable_text)];
    let policy = ego_core::permit::PolicyEngine::new(
        root.clone(),
        Mode::Yolo,
        Ruleset::from_layer(
            PermCfg {
                allow,
                deny: Vec::new(),
                judge: None,
            },
            Source::Project,
        ),
    );
    let fs = OsFs;
    let registry = builtins();
    let run = open_run(Confinement::Off, false, root.clone(), 1024 * 1024, 1024).unwrap();
    let tools = TurnTools::new(
        &registry,
        ToolEffectPorts::new(
            &fs,
            run.ports.process(),
            &run.evidence,
            run.ports.resolver_handle(),
            run.ports.one_hop_handle(),
            run.ports.duplex_handle(),
        ),
        root.clone(),
    );
    let params = serde_json::json!({
        "argv": [
            executable_text,
            "--ignored",
            "--exact",
            "exec_contract_fixture_echoes_argument",
            "--nocapture",
            "--",
            "composite literal * ; $(false)"
        ],
        "cwd": root.to_string(),
        "timeout_ms": 1000
    });
    let (tool, call) = tools.prepare("exec", &params.to_string()).unwrap();
    let expected_call = call.clone();
    let Op::Spawn {
        spec: prepared_spec,
    } = &call.ops()[0]
    else {
        panic!("ExecTool must prepare exactly one Spawn operation");
    };
    assert_eq!(prepared_spec.argv()[0], OsString::from(executable_text));
    assert_eq!(prepared_spec.cwd(), Some(&root));
    assert_eq!(prepared_spec.timeout(), Some(Duration::from_millis(1000)));
    let EnvPolicy::Exact { bindings, .. } = prepared_spec.env_policy() else {
        panic!("preparation must capture one exact environment before policy");
    };
    let expected_environment = std::env::vars_os()
        .filter(|(name, _)| EnvName::new(name.clone()).is_ok())
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(bindings.len(), expected_environment.len());
    assert!(
        bindings
            .iter()
            .all(|binding| binding.value_os_string().ok().as_ref()
                == expected_environment.get(binding.name().as_os_str())),
        "captured environment must contain the exact native values"
    );
    #[cfg(unix)]
    assert!(
        prepared_spec
            .identity()
            .is_some_and(|identity| identity.complete())
    );
    let Classification::Settled(Ok(authorized)) = tools.authorize(&policy, call) else {
        panic!("the exact prepared exec must be authorized by its rule");
    };
    assert_eq!(authorized.call(), &expected_call);
    assert_eq!(authorized.call().tool(), "exec");
    assert_eq!(authorized.call().ops().len(), 1);
    assert!(format!("{:?}", authorized.call()).contains("composite literal * ; $(false)"));
    let result = block_on(tools.execute(&policy, tool.as_ref(), authorized, None)).unwrap();
    let debug = format!("{result:?}");
    assert!(debug.contains("composite literal * ; $(false)"));
}

#[test]
fn exec_tool_policy_denial_stops_before_guarded_effects() {
    let root = cwd();
    let executable = std::env::current_exe().unwrap();
    let executable_text = executable.to_str().expect("test executable is UTF-8");
    let rule = exact_executable_rule(executable_text);
    let policy = ego_core::permit::PolicyEngine::new(
        root.clone(),
        Mode::Yolo,
        Ruleset::from_layer(
            PermCfg {
                // The deny must win even when an allow with the same exact
                // argv pattern is present.  This is the authorization proof:
                // GuardedEffects is never constructed for a denied call.
                allow: vec![rule.clone()],
                deny: vec![rule],
                judge: None,
            },
            Source::Project,
        ),
    );
    let fs = OsFs;
    let registry = builtins();
    let run = open_run(Confinement::Off, false, root.clone(), 1024 * 1024, 1024).unwrap();
    let tools = TurnTools::new(
        &registry,
        ToolEffectPorts::new(
            &fs,
            run.ports.process(),
            &run.evidence,
            run.ports.resolver_handle(),
            run.ports.one_hop_handle(),
            run.ports.duplex_handle(),
        ),
        root.clone(),
    );
    let params = serde_json::json!({
        "argv": [
            executable_text,
            "--ignored",
            "--exact",
            "exec_contract_fixture_echoes_argument",
            "--nocapture",
            "--",
            "denied before spawn"
        ],
        "cwd": root.to_string(),
        "timeout_ms": 1000
    });
    let (_tool, call) = tools.prepare("exec", &params.to_string()).unwrap();
    match tools.authorize(&policy, call) {
        Classification::Settled(Err(PolicyDenied::Denied { tool, .. })) => {
            assert_eq!(tool, "exec");
        }
        other => panic!("deny must settle before guarded execution: {other:?}"),
    }
}

#[test]
fn environment_policy_materializes_names_without_values_in_identity() {
    let unique_absent = format!("EGO_CONTRACT_SECRET_{}", std::process::id());
    let allow = EnvPolicy::allow_only(["PATH", unique_absent.as_str()]).unwrap();
    let allow_identity = format!("{allow:?}");
    assert!(allow_identity.contains("PATH"));
    assert!(allow_identity.contains(&unique_absent));
    if let Ok(path) = std::env::var("PATH") {
        assert!(!allow_identity.contains(&path));
    }

    let run = open_run(Confinement::Off, false, cwd(), 1024 * 1024, 1024).unwrap();
    let inherited = block_on(run.ports.process().execute(&fixture_arg_spec_with_env(
        "exec_contract_fixture_reports_environment_presence_without_values",
        [OsString::from("PATH")],
        None,
        cwd(),
        EnvPolicy::Inherited,
    )))
    .unwrap();
    assert!(format!("{inherited:?}").contains("PATH=present"));

    let out = block_on(run.ports.process().execute(&fixture_arg_spec_with_env(
        "exec_contract_fixture_reports_environment_presence_without_values",
        [
            OsString::from("PATH"),
            OsString::from(unique_absent.clone()),
        ],
        None,
        cwd(),
        allow,
    )))
    .unwrap();
    let debug = format!("{out:?}");
    assert!(debug.contains("PATH=present"));
    assert!(debug.contains(&format!("{unique_absent}=absent")));

    let deny_path = EnvPolicy::inherited_except(["PATH"]).unwrap();
    let out = block_on(run.ports.process().execute(&fixture_arg_spec_with_env(
        "exec_contract_fixture_reports_environment_presence_without_values",
        [OsString::from("PATH")],
        None,
        cwd(),
        deny_path,
    )))
    .unwrap();
    assert!(format!("{out:?}").contains("PATH=absent"));
}

#[test]
fn child_stdin_is_a_closed_pipe_and_reaches_eof() {
    let run = open_run(Confinement::Off, false, cwd(), 1024 * 1024, 1024).unwrap();
    let out = block_on(run.ports.process().execute(&fixture_arg_spec(
        "exec_contract_fixture_consumes_stdin_to_eof",
        [],
        None,
        cwd(),
    )))
    .unwrap();
    assert!(format!("{out:?}").contains("stdin-eof:0"));
}

#[cfg(unix)]
#[test]
fn child_does_not_inherit_open_descriptors_at_or_above_three() {
    use std::os::fd::AsRawFd;

    let open_files = (0..16)
        .map(|_| std::fs::File::open("/dev/null").unwrap())
        .collect::<Vec<_>>();
    assert!(open_files.iter().any(|file| file.as_raw_fd() >= 3));

    let run = open_run(Confinement::Off, false, cwd(), 1024 * 1024, 1024).unwrap();
    let out = block_on(run.ports.process().execute(&fixture_arg_spec(
        "exec_contract_fixture_rejects_inherited_fds",
        [],
        None,
        cwd(),
    )))
    .unwrap();
    assert!(format!("{out:?}").contains("fd-clean"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_helper_receives_an_eof_pipe_and_refuses_inherited_fd_above_stderr() {
    // Keep these Linux-specific assertions next to the helper contract.  A
    // successful test on another platform would not prove the Landlock
    // re-exec path, while this pair proves both sides of the helper's input
    // boundary: stdin is deliberately EOF and ambient descriptors are never
    // passed to the exact target argv.
    child_stdin_is_a_closed_pipe_and_reaches_eof();
    child_does_not_inherit_open_descriptors_at_or_above_three();
}

#[test]
fn real_process_output_keeps_stream_provenance_and_full_evidence_reference() {
    let workspace = cwd();
    let run = open_run(Confinement::Off, false, workspace, 1024 * 1024, 1024).unwrap();
    let out = block_on(run.ports.process().execute(&fixture_arg_spec(
        "exec_contract_fixture_echoes_argument",
        [OsString::from("stdout evidence")],
        None,
        cwd(),
    )))
    .unwrap();

    assert_two_process_streams_with_full_output(&out);
}

#[test]
fn concurrent_stdout_and_stderr_draining_preserves_both_streams() {
    let workspace = cwd();
    let run = open_run(
        Confinement::Off,
        false,
        workspace.clone(),
        1024 * 1024,
        1024,
    )
    .unwrap();
    let out = block_on(run.ports.process().execute(&fixture_spec(
        "exec_contract_fixture_emits_both_streams",
        &[],
        None,
        workspace,
    )))
    .unwrap();

    assert_two_process_streams_with_full_output(&out);
}

#[test]
fn binary_process_output_uses_explicit_base64_without_replacement_characters() {
    let workspace = cwd();
    let run = open_run(
        Confinement::Off,
        false,
        workspace.clone(),
        1024 * 1024,
        1024,
    )
    .unwrap();
    let out = block_on(run.ports.process().execute(&fixture_spec(
        "exec_contract_fixture_emits_binary_streams",
        &[],
        None,
        workspace,
    )))
    .unwrap();
    let wire = serde_json::to_value(&out).expect("binary result serialises");
    for block in &out.content {
        assert!(!block.text.contains('\u{fffd}'));
        let expected_bytes: &[u8] = match &block.provenance {
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stdout,
                ..
            } => &[0xff, 0xfe, 0x00, b'o', b'u', b't'],
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stderr,
                ..
            } => &[0x80, 0x81, 0x00, b'e', b'r', b'r'],
            other => panic!("binary fixture returned unexpected provenance: {other:?}"),
        };
        let encoded = block
            .text
            .strip_prefix("[base64]\n")
            .expect("binary process text is explicitly labelled");
        let decoded = decode_standard_base64(encoded);
        assert!(
            decoded
                .windows(expected_bytes.len())
                .any(|window| window == expected_bytes),
            "decoded stream does not retain the fixture bytes: {decoded:?}"
        );
    }
    assert!(
        out.model_text().contains("[base64]\n"),
        "binary process model text must label its base64 payload"
    );
    assert!(wire.to_string().contains("base64"));
}

#[test]
fn finite_large_output_succeeds_with_head_tail_preview_and_reference() {
    let workspace = cwd();
    let run = open_run(Confinement::Off, false, workspace.clone(), 128 * 1024, 64).unwrap();
    let out = block_on(run.ports.process().execute(&fixture_spec(
        "exec_contract_fixture_emits_both_streams",
        &[],
        None,
        workspace,
    )))
    .unwrap();

    let debug = format!("{out:?}");
    assert!(debug.contains("truncated"));
    let bounded_streams = out
        .content
        .iter()
        .filter(|block| matches!(&block.provenance, ToolOutputProvenance::Process { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        bounded_streams.len(),
        2,
        "stdout and stderr each retain evidence"
    );
    for block in bounded_streams {
        assert!(block.is_truncated());
        assert!(block.preview_is_head_and_tail());
        let reference = block
            .evidence_reference()
            .expect("complete stream evidence");
        let stored = read_evidence_pages(&run.evidence, reference);
        assert_eq!(stored.len() as u64, reference.bytes);
        assert!(
            reference.bytes > 64,
            "preview must not be the stored evidence"
        );
        let ToolOutputProvenance::Process { omitted_bytes, .. } = block.provenance() else {
            panic!("stream evidence must retain process provenance");
        };
        assert!(
            *omitted_bytes > 0,
            "large output must report omitted preview bytes"
        );
    }
}

fn assert_committed_stream_result(run: &Run, result: &ego_core::permit::ToolResult) {
    assert_eq!(
        result.content.len(),
        2,
        "stdout and stderr both drain before status"
    );
    for block in &result.content {
        let reference = block
            .evidence_reference()
            .expect("every process stream has complete evidence");
        let bytes = read_evidence_pages(&run.evidence, reference);
        assert_eq!(bytes.len() as u64, reference.bytes);
        assert!(matches!(
            &block.provenance,
            ToolOutputProvenance::Process { .. }
        ));
    }
}

#[test]
fn hard_evidence_cap_fails_and_cleans_up_infinite_output() {
    let dir = tempfile::tempdir().expect("process fixture directory");
    let pid_path = dir.path().join("evidence-descendant.pid");
    let workspace = cwd();
    let run = open_run(Confinement::Off, false, workspace, 4096, 64).unwrap();
    let err = execute(
        &run,
        fixture_spec(
            "exec_contract_fixture_holds_a_descendant",
            &[pid_path.as_path()],
            None,
            cwd(),
        ),
    )
    .unwrap_err();

    assert!(matches!(err, ProcessError::EvidenceLimit { .. }));
    let pid = std::fs::read_to_string(pid_path).expect("fixture wrote descendant pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while descendant_is_alive(pid.trim()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !descendant_is_alive(pid.trim()),
        "evidence cap left descendant alive"
    );
}

#[test]
fn hard_evidence_cap_is_distinct_from_preview_truncation() {
    let workspace = cwd();
    let run = open_run(Confinement::Off, false, workspace, 4096, 64).unwrap();
    let err = execute(
        &run,
        fixture_arg_spec("exec_contract_fixture_emits_until_killed", [], None, cwd()),
    )
    .unwrap_err();

    assert!(matches!(err, ProcessError::EvidenceLimit { .. }));
}

#[test]
fn nonzero_exit_is_a_typed_process_failure() {
    let run = open_run(Confinement::Off, false, cwd(), 1024 * 1024, 1024).unwrap();
    let err = execute(
        &run,
        fixture_arg_spec("exec_contract_fixture_exits_nonzero", [], None, cwd()),
    )
    .unwrap_err();
    let ProcessError::Exit {
        code: Some(17),
        result,
    } = err
    else {
        panic!("non-zero process must carry typed completed evidence");
    };
    assert_committed_stream_result(&run, &result);
}

#[test]
fn nonzero_exit_drains_both_streams_before_carrying_failed_result() {
    let run = open_run(Confinement::Off, false, cwd(), 1024 * 1024, 1024).unwrap();
    let err = block_on(run.ports.process().execute(&fixture_arg_spec(
        "exec_contract_fixture_emits_both_streams_and_exits_nonzero",
        [],
        None,
        cwd(),
    )))
    .unwrap_err();
    let ProcessError::Exit {
        code: Some(17),
        result,
    } = err
    else {
        panic!("non-zero process must carry both drained streams");
    };
    assert_committed_stream_result(&run, &result);
}

#[cfg(unix)]
#[test]
fn signal_termination_is_a_typed_process_failure() {
    let run = open_run(Confinement::Off, false, cwd(), 1024 * 1024, 1024).unwrap();
    let err = execute(
        &run,
        fixture_arg_spec(
            "exec_contract_fixture_terminates_by_signal",
            [],
            None,
            cwd(),
        ),
    )
    .unwrap_err();
    let ProcessError::Signal {
        signal: Some(15),
        result,
    } = err
    else {
        panic!("signal termination must carry typed completed evidence");
    };
    assert_committed_stream_result(&run, &result);
}

#[cfg(unix)]
#[test]
fn signal_termination_drains_both_streams_before_carrying_failed_result() {
    let run = open_run(Confinement::Off, false, cwd(), 1024 * 1024, 1024).unwrap();
    let err = block_on(run.ports.process().execute(&fixture_arg_spec(
        "exec_contract_fixture_emits_both_streams_and_signals",
        [],
        None,
        cwd(),
    )))
    .unwrap_err();
    let ProcessError::Signal {
        signal: Some(15),
        result,
    } = err
    else {
        panic!("signal termination must carry both drained streams");
    };
    assert_committed_stream_result(&run, &result);
}

#[test]
fn missing_executable_is_a_launch_failure_not_a_capability_refusal() {
    let run = open_run(Confinement::Off, false, cwd(), 1024 * 1024, 1024).unwrap();
    let executable = std::env::temp_dir().join(format!(
        "ego-exec-contract-does-not-exist-{}",
        std::process::id()
    ));
    let err = execute(
        &run,
        SpawnSpec::new(
            vec![executable.as_os_str().to_os_string()],
            Some(cwd()),
            None,
            EnvPolicy::Inherited,
        )
        .unwrap(),
    )
    .unwrap_err();
    assert!(matches!(err, ProcessError::Launch { .. }));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn linux_target_argv_must_name_an_exact_executable_file() {
    let workspace_dir = tempfile::tempdir().expect("workspace fixture directory");
    let root = CanonPath::new(std::fs::canonicalize(workspace_dir.path()).unwrap());
    let Some(run) = open_confined_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) else {
        return;
    };

    let fixture_dir = tempfile::tempdir_in(workspace_dir.path()).expect("target fixture directory");
    let non_executable = fixture_dir.path().join("not-executable");
    std::fs::write(&non_executable, b"not a program").expect("target fixture");
    let directory = fixture_dir.path().join("directory-target");
    std::fs::create_dir(&directory).expect("target directory fixture");

    for target in [non_executable, directory] {
        let spec = SpawnSpec::new(
            vec![target.as_os_str().to_os_string()],
            Some(root.clone()),
            None,
            EnvPolicy::Inherited,
        )
        .expect("argv is valid before launch");
        let error = match execute(&run, spec) {
            Ok(()) => panic!("a non-file target must not launch"),
            Err(error) => error,
        };
        assert!(matches!(error, ProcessError::Launch { .. }));
    }
}

#[test]
fn filesystem_confinement_matrix_is_enforced_by_real_processes() {
    let workspace_dir = tempfile::tempdir().expect("workspace fixture directory");
    let outside = tempfile::tempdir().expect("outside fixture directory");
    let root = CanonPath::new(std::fs::canonicalize(workspace_dir.path()).unwrap());
    let inside = workspace_dir.path().join("inside");
    let outside_target = outside.path().join("outside");

    if let Some(run) = open_confined_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) {
        execute(&run, mutation_spec("create", &[&inside], root.clone()))
            .expect("workspace allows inside write");
        assert!(inside.exists());
        assert!(
            execute(
                &run,
                mutation_spec("create", &[&outside_target], root.clone())
            )
            .is_err()
        );
        assert!(!outside_target.exists());
    }

    if let Some(run) = open_confined_run(Confinement::Ro, false, root.clone(), 1024 * 1024, 1024) {
        let ro_inside = workspace_dir.path().join("ro-inside");
        let ro_outside = outside.path().join("ro-outside");
        assert!(execute(&run, mutation_spec("create", &[&ro_inside], root.clone())).is_err());
        assert!(!ro_inside.exists());
        assert!(execute(&run, mutation_spec("create", &[&ro_outside], root.clone())).is_err());
        assert!(!ro_outside.exists());
    }

    let run = open_run(Confinement::Off, false, root.clone(), 1024 * 1024, 1024).unwrap();
    execute(
        &run,
        mutation_spec("create", &[&outside_target], root.clone()),
    )
    .expect("off allows outside write");
    assert!(outside_target.exists());
}

#[test]
fn filesystem_read_matrix_covers_inside_and_outside_for_each_level() {
    let workspace_dir = tempfile::tempdir().expect("workspace fixture directory");
    let outside = tempfile::tempdir().expect("outside fixture directory");
    let root = CanonPath::new(std::fs::canonicalize(workspace_dir.path()).unwrap());
    let inside = workspace_dir.path().join("inside-read");
    let outside_target = outside.path().join("outside-read");
    std::fs::write(&inside, b"inside").unwrap();
    std::fs::write(&outside_target, b"outside").unwrap();

    let read_root = root.clone();
    let read = move |run: &Run, target: &std::path::Path| {
        #[cfg(windows)]
        let spec = fixture_spec(
            "exec_contract_fixture_reads",
            &[target],
            None,
            read_root.clone(),
        );
        #[cfg(not(windows))]
        let spec = SpawnSpec::new(
            vec![
                OsString::from("/bin/cat"),
                target.as_os_str().to_os_string(),
            ],
            Some(read_root.clone()),
            None,
            EnvPolicy::Inherited,
        )
        .expect("cat read specification is valid");
        execute(run, spec)
    };

    if let Some(run) = open_confined_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) {
        read(&run, &inside).expect("workspace allows an inside read");
        assert!(read(&run, &outside_target).is_err());
    }

    if let Some(run) = open_confined_run(Confinement::Ro, false, root.clone(), 1024 * 1024, 1024) {
        read(&run, &inside).expect("ro allows an inside read");
        assert!(read(&run, &outside_target).is_err());
    }

    let off_run = open_run(Confinement::Off, false, root.clone(), 1024 * 1024, 1024).unwrap();
    read(&off_run, &inside).expect("off allows an inside read");
    read(&off_run, &outside_target).expect("off allows an outside read");
}

#[test]
fn workspace_confinement_covers_required_mutations_inside_and_outside() {
    let workspace = tempfile::tempdir().expect("workspace fixture directory");
    let outside = tempfile::tempdir().expect("outside fixture directory");
    let root = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
    let Some(run) = open_confined_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) else {
        return;
    };

    let inside_create = workspace.path().join("create");
    execute(
        &run,
        mutation_spec("create", &[&inside_create], root.clone()),
    )
    .expect("create inside workspace");

    let inside_overwrite_source = workspace.path().join("overwrite-source");
    let inside_overwrite = workspace.path().join("overwrite-destination");
    std::fs::write(&inside_overwrite_source, "after").unwrap();
    std::fs::write(&inside_overwrite, "before").unwrap();
    execute(
        &run,
        mutation_spec(
            "overwrite",
            &[&inside_overwrite_source, &inside_overwrite],
            root.clone(),
        ),
    )
    .expect("overwrite inside workspace");
    assert_eq!(std::fs::read_to_string(&inside_overwrite).unwrap(), "after");

    let inside_truncate = workspace.path().join("truncate");
    std::fs::write(&inside_truncate, "before").unwrap();
    execute(
        &run,
        mutation_spec("truncate", &[&inside_truncate], root.clone()),
    )
    .expect("truncate inside workspace");
    assert!(std::fs::read(&inside_truncate).unwrap().is_empty());

    let inside_remove = workspace.path().join("remove");
    std::fs::write(&inside_remove, "remove").unwrap();
    execute(
        &run,
        mutation_spec("remove", &[&inside_remove], root.clone()),
    )
    .expect("remove inside workspace");
    assert!(!inside_remove.exists());

    let inside_rename_source = workspace.path().join("rename-source");
    let inside_rename_destination = workspace.path().join("rename-destination");
    std::fs::write(&inside_rename_source, "rename").unwrap();
    execute(
        &run,
        mutation_spec(
            "rename",
            &[&inside_rename_source, &inside_rename_destination],
            root.clone(),
        ),
    )
    .expect("rename inside workspace");
    assert!(!inside_rename_source.exists());
    assert!(inside_rename_destination.exists());

    let inside_link_source = workspace.path().join("link-source");
    let inside_link_destination = workspace.path().join("link-destination");
    std::fs::write(&inside_link_source, "link").unwrap();
    execute(
        &run,
        mutation_spec(
            "link",
            &[&inside_link_source, &inside_link_destination],
            root.clone(),
        ),
    )
    .expect("link inside workspace");
    assert!(inside_link_destination.exists());

    let outside_create = outside.path().join("create");
    let outside_truncate = outside.path().join("truncate");
    let outside_remove = outside.path().join("remove");
    let outside_rename_source = outside.path().join("rename-source");
    let outside_rename_destination = outside.path().join("rename-destination");
    let outside_link_source = outside.path().join("link-source");
    let outside_link_destination = outside.path().join("link-destination");
    std::fs::write(&outside_remove, "remove").unwrap();
    std::fs::write(&outside_truncate, "truncate").unwrap();
    std::fs::write(&outside_rename_source, "rename").unwrap();
    std::fs::write(&outside_link_source, "link").unwrap();
    let denied = |spawn: SpawnSpec| execute(&run, spawn).is_err();
    assert!(denied(mutation_spec(
        "create",
        &[&outside_create],
        root.clone()
    )));
    assert!(denied(mutation_spec(
        "truncate",
        &[&outside_truncate],
        root.clone()
    )));
    assert!(denied(mutation_spec(
        "remove",
        &[&outside_remove],
        root.clone()
    )));
    assert!(denied(mutation_spec(
        "rename",
        &[&outside_rename_source, &outside_rename_destination],
        root.clone(),
    )));
    assert!(denied(mutation_spec(
        "link",
        &[&outside_link_source, &outside_link_destination],
        root.clone(),
    )));
    assert!(!outside_create.exists());
    assert_eq!(
        std::fs::read_to_string(&outside_truncate).unwrap(),
        "truncate"
    );
    assert!(outside_remove.exists());
    assert!(outside_rename_source.exists());
    assert!(!outside_rename_destination.exists());
    assert!(!outside_link_destination.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_landlock_rights_probe_repeats_the_full_mutation_matrix() {
    // Keep the ABI-sensitive gate explicit: the shared matrix is the paired
    // behavioural proof for create, overwrite/truncate, remove, rename and
    // link. A kernel that cannot demonstrate all required rights must be
    // declared with `EGO_TEST_CONFINEMENT_BACKEND_ABSENT`; otherwise the
    // refusal fails this test rather than passing it.
    workspace_confinement_covers_required_mutations_inside_and_outside();
}

#[test]
fn read_only_confinement_denies_every_required_mutation_inside_the_root() {
    let workspace = tempfile::tempdir().expect("workspace fixture directory");
    let root = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
    let Some(run) = open_confined_run(Confinement::Ro, false, root.clone(), 1024 * 1024, 1024)
    else {
        return;
    };

    let create = workspace.path().join("ro-create");
    assert!(execute(&run, mutation_spec("create", &[&create], root.clone())).is_err());
    assert!(!create.exists());

    let overwrite_source = workspace.path().join("ro-overwrite-source");
    let overwrite = workspace.path().join("ro-overwrite");
    std::fs::write(&overwrite_source, "after").unwrap();
    std::fs::write(&overwrite, "before").unwrap();
    assert!(
        execute(
            &run,
            mutation_spec("overwrite", &[&overwrite_source, &overwrite], root.clone())
        )
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(&overwrite).unwrap(), "before");

    let truncate = workspace.path().join("ro-truncate");
    std::fs::write(&truncate, "before").unwrap();
    assert!(execute(&run, mutation_spec("truncate", &[&truncate], root.clone())).is_err());
    assert_eq!(std::fs::read_to_string(&truncate).unwrap(), "before");

    let remove = workspace.path().join("ro-remove");
    std::fs::write(&remove, "remove").unwrap();
    assert!(execute(&run, mutation_spec("remove", &[&remove], root.clone())).is_err());
    assert!(remove.exists());

    let rename_source = workspace.path().join("ro-rename-source");
    let rename_destination = workspace.path().join("ro-rename-destination");
    std::fs::write(&rename_source, "rename").unwrap();
    assert!(
        execute(
            &run,
            mutation_spec(
                "rename",
                &[&rename_source, &rename_destination],
                root.clone()
            ),
        )
        .is_err()
    );
    assert!(rename_source.exists());
    assert!(!rename_destination.exists());

    let link_source = workspace.path().join("ro-link-source");
    let link_destination = workspace.path().join("ro-link-destination");
    std::fs::write(&link_source, "link").unwrap();
    assert!(
        execute(
            &run,
            mutation_spec("link", &[&link_source, &link_destination], root),
        )
        .is_err()
    );
    assert!(!link_destination.exists());
}

#[test]
fn off_confinement_allows_inside_and_outside_mutations() {
    let workspace = tempfile::tempdir().expect("workspace fixture directory");
    let outside = tempfile::tempdir().expect("outside fixture directory");
    let root = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
    let run = open_run(Confinement::Off, false, root.clone(), 1024 * 1024, 1024).unwrap();
    let inside = workspace.path().join("off-inside");
    let outside_target = outside.path().join("off-outside");

    execute(&run, mutation_spec("create", &[&inside], root.clone())).unwrap();
    execute(&run, mutation_spec("create", &[&outside_target], root)).unwrap();
    assert!(inside.exists());
    assert!(outside_target.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_workspace_confinement_is_demonstrated_across_workspace_and_private_scratch() {
    let workspace = tempfile::tempdir().expect("workspace fixture directory");
    let outside = tempfile::tempdir().expect("outside fixture directory");
    let root = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
    let run = open_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    )
    .expect("the default sandbox must be demonstrable wherever sandbox-exec is installed");

    // The private scratch root belongs to the backend, so no test can name it.
    // WRITE_SCOPED is exactly its proof: the bit is set only after the paired
    // mutation suite created, overwrote, truncated, removed, renamed and
    // linked inside *both* writable roots, scratch included, and was refused
    // outside them.
    assert!(
        run.observation
            .capabilities()
            .contains(ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED)),
        "workspace confinement must measure both scoped reads and scoped writes: {:?}",
        run.observation.capabilities()
    );

    let inside = workspace.path().join("workspace-create");
    execute(&run, mutation_spec("create", &[&inside], root.clone()))
        .expect("workspace confinement allows a mutation inside the workspace");
    assert!(inside.exists());

    let outside_target = outside.path().join("outside-create");
    assert!(execute(&run, mutation_spec("create", &[&outside_target], root)).is_err());
    assert!(!outside_target.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_profile_path_fixture_is_data_not_profile_injection() {
    let parent = tempfile::tempdir().expect("profile fixture directory");
    let hostile = parent.path().join(r#"quote");(deny file-read*)"#);
    std::fs::create_dir(&hostile).expect("hostile path fixture");
    let root = CanonPath::new(std::fs::canonicalize(&hostile).unwrap());
    let Some(run) = open_confined_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) else {
        return;
    };
    let inside = hostile.join("inside");
    let outside = parent.path().join("outside");

    execute(&run, mutation_spec("create", &[&inside], root.clone()))
        .expect("escaped path remains an allowed root");
    assert!(inside.exists());
    assert!(execute(&run, mutation_spec("create", &[&outside], root)).is_err());
    assert!(!outside.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_refuses_an_unrepresentable_profile_root() {
    let parent = tempfile::tempdir().expect("profile fixture directory");
    let control_path = parent.path().join("line\nbreak");
    std::fs::create_dir(&control_path).expect("control-byte path fixture");
    let root = CanonPath::new(std::fs::canonicalize(control_path).unwrap());

    match open_run(Confinement::Workspace, false, root, 1024 * 1024, 1024) {
        Err(ProcessError::Backend(reason)) => {
            assert!(!reason.is_empty(), "backend refusal must carry evidence");
        }
        Err(ProcessError::MissingCapabilities(missing)) => assert_eq!(
            missing.required(),
            ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED)
        ),
        Ok(_) => panic!("Seatbelt accepted a profile root it cannot represent safely"),
        Err(err) => panic!("unrepresentable profile root failed unclearly: {err:?}"),
    }
}

// Catches: Landlock drops configured exec roots or admits them with write authority.
#[cfg(target_os = "linux")]
#[test]
fn linux_configured_exec_root_runs_real_binary_without_widening_writes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tools = tempfile::tempdir().expect("external tools");
    let outside = tempfile::tempdir().expect("unrelated directory");
    let workspace = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    let tools = std::fs::canonicalize(tools.path()).expect("canonical tools");
    let program = tools.join("cp");
    std::fs::copy("/bin/cp", &program).expect("record installed executable");
    let source = tools.join("source");
    std::fs::write(&source, b"original").expect("source");
    let inside = workspace.join("copied");
    let copy = |target: &std::path::Path| {
        SpawnSpec::new(
            vec![
                program.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
                target.as_os_str().to_owned(),
            ],
            Some(CanonPath::new(workspace.clone())),
            None,
            EnvPolicy::Inherited,
        )
        .expect("copy spec")
    };
    let policy = run_confinement(Confinement::Workspace, false, CanonPath::new(&workspace));
    let unlisted = open_policy(policy.clone(), 1024 * 1024, 1024).expect("unlisted run");
    execute(&unlisted, copy(&inside)).expect_err("unlisted tool must not execute");
    assert!(!inside.exists(), "unlisted tool copied a file");
    let admitted = open_policy(
        policy.with_exec_paths(vec![CanonPath::new(&tools)]),
        1024 * 1024,
        1024,
    )
    .expect("admitted run");
    execute(&admitted, copy(&inside)).expect("admitted tool copies into workspace");
    assert_eq!(
        std::fs::read(&inside).expect("copied contents"),
        b"original"
    );
    for target in [tools.join("existing"), outside.path().join("existing")] {
        std::fs::write(&target, b"preserve").expect("existing outside file");
        execute(&admitted, copy(&target)).expect_err("exec roots must not widen writes");
        assert_eq!(
            std::fs::read(&target).expect("preserved contents"),
            b"preserve"
        );
    }
}

/// An executable `tool.sh` in `dir`.
#[cfg(target_os = "macos")]
fn write_script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let script = dir.join("tool.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

/// A directory outside every default exec root holding one executable script.
#[cfg(target_os = "macos")]
fn script_dir(body: &str) -> (tempfile::TempDir, CanonPath, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("script directory");
    let root = std::fs::canonicalize(dir.path()).expect("canonical script directory");
    let script = write_script(&root, body);
    (dir, CanonPath::new(root), script)
}

#[cfg(target_os = "macos")]
fn workspace_policy(workspace: &std::path::Path, exec_paths: Vec<CanonPath>) -> RunConfinement {
    let workspace = CanonPath::new(std::fs::canonicalize(workspace).expect("canonical workspace"));
    run_confinement(Confinement::Workspace, false, workspace).with_exec_paths(exec_paths)
}

#[cfg(target_os = "macos")]
fn script_spec(
    script: &std::path::Path,
    args: &[&std::path::Path],
    workspace: &std::path::Path,
) -> SpawnSpec {
    let mut argv = vec![script.as_os_str().to_owned()];
    argv.extend(args.iter().map(|arg| arg.as_os_str().to_owned()));
    let workspace = CanonPath::new(std::fs::canonicalize(workspace).expect("canonical workspace"));
    SpawnSpec::new(argv, Some(workspace), None, EnvPolicy::Inherited).expect("script spec")
}

#[cfg(target_os = "macos")]
#[test]
fn macos_exec_paths_let_a_confined_child_run_a_program_outside_the_workspace() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (_keep, tools, script) = script_dir("echo ran");
    let spec = script_spec(&script, &[], workspace.path());

    // Standalone stays as it is today: no exec path, no exec.
    let standalone = open_policy(
        workspace_policy(workspace.path(), Vec::new()),
        1024 * 1024,
        1024,
    )
    .expect("standalone workspace run");
    assert!(
        execute(&standalone, spec.clone()).is_err(),
        "unlisted script ran"
    );

    let profiled = open_policy(
        workspace_policy(workspace.path(), vec![tools]),
        1024 * 1024,
        1024,
    )
    .expect("profiled workspace run");
    execute(&profiled, spec).expect("a script under an exec path runs");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_exec_paths_do_not_widen_writes_for_the_programs_they_admit() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside directory");
    let (_keep, tools, script) = script_dir(r#": > "$1""#);
    let run = open_policy(
        workspace_policy(workspace.path(), vec![tools.clone()]),
        1024 * 1024,
        1024,
    )
    .expect("profiled workspace run");

    let inside = workspace.path().join("inside");
    execute(&run, script_spec(&script, &[&inside], workspace.path()))
        .expect("the admitted script writes inside the workspace");
    assert!(inside.exists(), "control write never happened");

    for target in [
        outside.path().join("outside"),
        tools.as_path().join("planted"),
    ] {
        assert!(
            execute(&run, script_spec(&script, &[&target], workspace.path())).is_err(),
            "write to {} was allowed",
            target.display()
        );
        assert!(!target.exists(), "{} was created", target.display());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_exec_paths_admit_only_the_roots_they_name() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (_listed_keep, listed, listed_script) = script_dir("echo ran");
    let (_other_keep, _other, other_script) = script_dir("echo ran");
    let run = open_policy(
        workspace_policy(workspace.path(), vec![listed]),
        1024 * 1024,
        1024,
    )
    .expect("profiled workspace run");

    execute(&run, script_spec(&listed_script, &[], workspace.path())).expect("listed root runs");
    assert!(
        execute(&run, script_spec(&other_script, &[], workspace.path())).is_err(),
        "a sibling root outside the list ran"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_exec_paths_expose_the_metadata_of_their_ancestors_and_nothing_more() {
    // Node's realpath, bash's command lookup and `mkdir -p` lstat every
    // ancestor of a path they use; without that a tool under an exec path
    // starts and then dies with `Operation not permitted` on `/Users`.
    let workspace = tempfile::tempdir().expect("workspace");
    let outer = tempfile::tempdir().expect("outer directory");
    let outer_path = std::fs::canonicalize(outer.path()).expect("canonical outer");
    let tools = outer_path.join("tools");
    std::fs::create_dir(&tools).expect("tools directory");
    let secret = outer_path.join("secret");
    std::fs::write(&secret, b"secret").expect("secret");
    let script = write_script(
        &tools,
        r#"case "$1" in stat) /usr/bin/stat "$2";; ls) /bin/ls "$2";; cat) /bin/cat "$2";; esac"#,
    );
    let run = open_policy(
        workspace_policy(workspace.path(), vec![CanonPath::new(tools)]),
        1024 * 1024,
        1024,
    )
    .expect("profiled workspace run");
    let call = |verb: &str, target: &std::path::Path| {
        execute(
            &run,
            script_spec(
                &script,
                &[std::path::Path::new(verb), target],
                workspace.path(),
            ),
        )
    };

    call("stat", &outer_path).expect("an ancestor of an exec path can be stat'ed");
    assert!(
        call("ls", &outer_path).is_err(),
        "ancestor contents were listable"
    );
    assert!(
        call("cat", &secret).is_err(),
        "a file beside the exec path was readable"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_workspace_ancestor_metadata_allows_standalone_stat_and_mkdir() {
    // node's `realpathSync(cwd)` is how wiz-run finds its project: it lstats
    // every ancestor of the workspace.
    // The workspace sits in its own parent, which no exec path shares.
    let outer = tempfile::tempdir().expect("outer directory");
    let ancestor = std::fs::canonicalize(outer.path()).expect("canonical outer");
    let workspace = ancestor.join("ws");
    std::fs::create_dir(&workspace).expect("workspace");
    let (_keep, tools, script) = script_dir(r#"/usr/bin/stat "$1""#);
    let spec = script_spec(&script, &[&ancestor], &workspace);

    let profiled = open_policy(workspace_policy(&workspace, vec![tools]), 1024 * 1024, 1024)
        .expect("profiled workspace run");
    execute(&profiled, spec.clone()).expect("the workspace's parent can be stat'ed");

    // Catches: standalone workspace confinement refuses ancestor lstat.
    // Use a system binary so exec paths cannot supply the missing grant.
    let standalone = open_policy(workspace_policy(&workspace, Vec::new()), 1024 * 1024, 1024)
        .expect("standalone workspace run");
    let bare = SpawnSpec::new(
        vec!["/usr/bin/stat".into(), ancestor.clone().into_os_string()],
        spec.cwd().cloned(),
        None,
        EnvPolicy::Inherited,
    )
    .expect("spec");
    execute(&standalone, bare).expect("standalone workspace ancestors can be stat'ed");
    let nested = workspace.join("a/b");
    let mkdir = SpawnSpec::new(
        vec![
            "/bin/mkdir".into(),
            "-p".into(),
            nested.clone().into_os_string(),
        ],
        spec.cwd().cloned(),
        None,
        EnvPolicy::Inherited,
    )
    .expect("mkdir spec");
    execute(&standalone, mkdir).expect("mkdir -p can walk workspace ancestors");
    assert!(
        nested.is_dir(),
        "mkdir -p did not create the nested directory"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_workspace_ancestor_metadata_does_not_allow_content_or_outside_writes() {
    // Catches: ancestor metadata permission widens into listing, reads or writes.
    let outer = tempfile::tempdir().expect("outer directory");
    let ancestor = std::fs::canonicalize(outer.path()).expect("canonical outer");
    let workspace = ancestor.join("ws");
    std::fs::create_dir(&workspace).expect("workspace");
    let sibling = ancestor.join("secret");
    std::fs::write(&sibling, b"secret").expect("sibling fixture");
    let outside_write = ancestor.join("outside-write");
    let run = open_policy(workspace_policy(&workspace, Vec::new()), 1024 * 1024, 1024)
        .expect("standalone workspace run");
    let call = |program: &str, path: &std::path::Path| {
        execute(
            &run,
            SpawnSpec::new(
                vec![program.into(), path.as_os_str().to_owned()],
                Some(CanonPath::new(workspace.clone())),
                None,
                EnvPolicy::Inherited,
            )
            .expect("spec"),
        )
    };
    call("/usr/bin/stat", &ancestor).expect("metadata grant is active");
    call("/bin/cat", &workspace.join("../secret")).expect_err("sibling content must be denied");
    call("/bin/ls", &ancestor).expect_err("ancestor listing must be denied");
    call("/usr/bin/touch", &outside_write).expect_err("outside write must be denied");
    assert!(!outside_write.exists(), "outside file was created");
    assert_eq!(std::fs::read(&sibling).expect("read sibling"), b"secret");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_an_exec_path_the_backend_cannot_read_refuses_the_run_instead_of_being_dropped() {
    let workspace = tempfile::tempdir().expect("workspace");
    let missing = CanonPath::new(workspace.path().join("no-such-tools"));
    match open_policy(
        workspace_policy(workspace.path(), vec![missing]),
        1024 * 1024,
        1024,
    ) {
        Err(ProcessError::MissingCapabilities(_) | ProcessError::Backend(_)) => {}
        Ok(_) => panic!("a run opened with an exec path it cannot honour"),
        Err(error) => panic!("unclear refusal: {error:?}"),
    }
}

/// Whether the sandbox kept a child from starting, and what it reported.
///
/// `sandbox-exec` reports a refused exec of the target as EX_OSERR (71). A
/// timeout means the program started and kept running, so it was not blocked.
#[cfg(target_os = "macos")]
fn sandbox_blocked(run: &Run, spec: SpawnSpec) -> (bool, String) {
    const SANDBOX_EXEC_REFUSED: i32 = 71;
    match block_on(run.ports.process().execute(&spec)) {
        Ok(_) => (false, "exit 0".into()),
        Err(ProcessError::Exit { code, result }) => (
            code == Some(SANDBOX_EXEC_REFUSED),
            format!("exit {code:?}: {:?}", result.content),
        ),
        Err(ProcessError::Timeout { .. }) => (false, "timed out after starting".into()),
        Err(error) => (true, error.to_string()),
    }
}

/// `mdkb stats` must exit 0 inside ego where it exits 0 outside, in a
/// repository that has an index. Run by hand, like the test below.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "acceptance: needs mdkb and an indexed repository on this machine"]
fn macos_mdkb_stats_exits_as_outside_ego_in_an_indexed_workspace() {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let mdkb = std::fs::canonicalize(home.join(".local/bin/mdkb")).expect("mdkb");
    // A worktree resolves its index outside its own directory, which a
    // workspace run cannot reach; name the checkout that holds `.mdkb`.
    let repository = std::fs::canonicalize(
        std::env::var_os("EGO_ACCEPT_MDKB_REPOSITORY").expect("EGO_ACCEPT_MDKB_REPOSITORY"),
    )
    .expect("indexed repository");
    let outside = Command::new(&mdkb)
        .arg("stats")
        .current_dir(&repository)
        .output()
        .expect("unsandboxed mdkb");
    assert_eq!(
        outside.status.code(),
        Some(0),
        "no usable index outside ego"
    );

    let policy = RunConfinement::new(Confinement::Workspace, true, CanonPath::new(&repository))
        .expect("policy")
        .with_exec_paths(vec![CanonPath::new(mdkb.clone())]);
    let run = open_policy(policy, 1024 * 1024, 1024).expect("profiled run");
    let spec = SpawnSpec::new(
        vec![mdkb.into_os_string(), "stats".into()],
        Some(CanonPath::new(&repository)),
        Some(Duration::from_secs(30)),
        EnvPolicy::Inherited,
    )
    .expect("spec");
    let (blocked, note) = sandbox_blocked(&run, spec);
    assert!(
        !blocked && note == "exit 0",
        "mdkb stats inside ego: {note}"
    );
}

/// Acceptance on the coordinator machine: the tools maintainer named run under
/// `workspace` with profile exec paths, offline so a script cannot reach the
/// TUIC socket, from a scratch workspace so nothing real is written.
/// Run by hand: `cargo nextest run -p ego-tools --test exec_contract --run-ignored only`.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "acceptance: needs the coordinator toolchain installed on this machine"]
fn macos_coordinator_tools_run_under_workspace_with_profile_exec_paths() {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let canon = |path: &std::path::Path| {
        std::fs::canonicalize(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    };
    let tuic = canon(std::path::Path::new("/usr/local/bin/tuic"));
    let mdkb = canon(&home.join(".local/bin/mdkb"));
    let wiz_run = canon(&home.join(".local/bin/wiz-run"));
    let wiz_root = wiz_run
        .parent()
        .and_then(|bin| bin.parent())
        .expect("wiz plugin root");
    let tools = canon(&home.join("workspace/example-project/tools"));
    let exec_paths = [
        canon(std::path::Path::new("/opt/homebrew")),
        // The PATH directories themselves: bash finds `tuic` and `mdkb` there.
        canon(std::path::Path::new("/usr/local/bin")),
        canon(&home.join(".local/bin")),
        // git, and so wiz-run, dies with `unable to access ~/.gitconfig` when
        // the file is unreadable; an exec path is also a read root.
        canon(&home.join(".gitconfig")),
        tuic.clone(),
        mdkb.clone(),
        wiz_root.to_path_buf(),
        tools.clone(),
    ]
    .into_iter()
    .map(CanonPath::new)
    .collect::<Vec<_>>();

    let workspace = tempfile::tempdir().expect("workspace");
    // wiz-run wants a git root; the workspace is its own so the answer does not
    // depend on a `.git` above it, which would be a read-root question.
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(workspace.path())
            .status()
            .expect("git init")
            .success()
    );
    let canonical_workspace = CanonPath::new(canon(workspace.path()));
    let policy = RunConfinement::new(Confinement::Workspace, true, canonical_workspace)
        .expect("policy")
        .with_exec_paths(exec_paths);
    let run = open_policy(policy, 1024 * 1024, 1024).expect("profiled offline workspace run");

    let mut cases: Vec<(String, Vec<OsString>)> = vec![
        ("tuic --help".into(), vec![tuic.into(), "--help".into()]),
        ("mdkb stats".into(), vec![mdkb.into(), "stats".into()]),
        (
            "wiz-run stories-cli.js list".into(),
            vec![wiz_run.into(), "stories-cli.js".into(), "list".into()],
        ),
        (
            "git rev-parse".into(),
            vec![
                "/opt/homebrew/bin/git".into(),
                "rev-parse".into(),
                "--show-toplevel".into(),
            ],
        ),
        (
            "jq --version".into(),
            vec!["/opt/homebrew/bin/jq".into(), "--version".into()],
        ),
    ];
    let mut pending = vec![tools.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("tools directory").flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "sh")
                && !path.to_string_lossy().contains("/tests/")
                // Sourced libraries are not programs, outside ego either.
                && path.metadata().is_ok_and(|meta| {
                    std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0
                })
            {
                let label = path.strip_prefix(&tools).unwrap().display().to_string();
                cases.push((
                    format!("{label} --help"),
                    vec![path.into(), "--help".into()],
                ));
            }
        }
    }

    // The named tools must also exit exactly as they do outside ego.
    let mut diverged = Vec::new();
    for (label, argv) in cases.iter().take(4) {
        let outside = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(workspace.path())
            .output()
            .expect("unsandboxed run")
            .status
            .code();
        let spec = SpawnSpec::new(
            argv.clone(),
            Some(CanonPath::new(canon(workspace.path()))),
            Some(Duration::from_secs(20)),
            EnvPolicy::Inherited,
        )
        .expect("spec");
        let (_, note) = sandbox_blocked(&run, spec);
        let inside = note
            .strip_prefix("exit ")
            .map(|rest| rest.split(':').next().unwrap_or("").to_owned());
        let expected = match outside {
            Some(0) => "0".to_owned(),
            other => format!("{other:?}"),
        };
        if inside.as_deref() != Some(expected.as_str()) {
            eprintln!("DIVERGED {label}: {note}");
            diverged.push(label.clone());
        }
    }
    assert!(
        diverged.is_empty(),
        "differs from outside ego: {diverged:?}"
    );

    let blocked = cases
        .into_iter()
        .filter_map(|(label, argv)| {
            let spec = SpawnSpec::new(
                argv,
                Some(CanonPath::new(canon(workspace.path()))),
                Some(Duration::from_secs(20)),
                EnvPolicy::Inherited,
            )
            .expect("spec");
            let (blocked, note) = sandbox_blocked(&run, spec);
            eprintln!("{label}: {note}");
            blocked.then_some(label)
        })
        .collect::<Vec<_>>();
    assert!(blocked.is_empty(), "blocked by the sandbox: {blocked:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_measured_refusal_cites_the_probe_transcript_stored_in_its_session() {
    // Seatbelt claims offline only when all four paired network probes pass,
    // so this is the one request that may be measured in full and still
    // refused. Whichever way it lands, the run has to be able to say what it
    // measured rather than leave an absent bit as the only account.
    let attempt = attempt_run(Confinement::Off, true, cwd(), 1024 * 1024, 1024);
    let missing = match &attempt.outcome {
        Err(ProcessError::MissingCapabilities(missing)) => missing,
        Err(error) => panic!("offline failed without a precise capability refusal: {error}"),
        Ok(_) => {
            // Nothing was refused, so there is no refusal to make account for.
            // Said out loud: a silent return reads the same as having asserted.
            eprintln!("note: offline was granted here, so this test had no refusal to check");
            return;
        }
    };
    let reference = missing
        .probe_evidence()
        .expect("a refusal that follows a measurement carries its transcript");
    let transcript = String::from_utf8(read_evidence_pages(&attempt.evidence, reference))
        .expect("the probe transcript is UTF-8 JSON Lines");
    assert!(
        missing.to_string().contains(reference.id.as_str()),
        "the refusal must cite the transcript by name: {missing}"
    );

    let records = transcript
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("transcript record"))
        .collect::<Vec<_>>();
    assert!(!records.is_empty());
    for record in &records {
        assert!(record["suite"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(record["root"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(record["argv"].as_array().is_some_and(|a| !a.is_empty()));
        assert!(record["outcome"].as_str().is_some_and(|s| !s.is_empty()));
    }
    assert!(
        records.iter().any(|record| record["suite"]
            .as_str()
            .is_some_and(|suite| suite.starts_with("offline."))),
        "an offline refusal must name the network suites it ran: {transcript}"
    );
}

#[test]
fn observation_reports_capabilities_and_audit_identity_without_granting_them() {
    let root = cwd();
    let run = open_run(Confinement::Off, false, root, 1024 * 1024, 1024).unwrap();
    assert!(!run.observation.backend().name().is_empty());
    assert!(!run.observation.backend().revision().is_empty());
    assert_eq!(run.observation.policy_digest().as_str().len(), 64);
    assert_eq!(
        run.observation.capabilities(),
        ExecCaps::empty(),
        "sandbox off must not claim confinement or offline capabilities"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn observe_returns_the_measured_fact_before_an_unsupported_run_is_refused() {
    let fixture = config_fixture(Confinement::Off, true, cwd(), 1024 * 1024, 1024);
    let (observation, _probe_evidence) = block_on(RunEffectPorts::observe(&fixture.config))
        .expect("observation must not apply the run requirements");
    assert_eq!(observation.capabilities(), ExecCaps::empty());
    assert!(!observation.backend().name().is_empty());
    assert!(!observation.backend().revision().is_empty());

    let error = match block_on(RunEffectPorts::for_run(fixture.config)) {
        Ok(_) => panic!("the ordinary run must still refuse unsupported offline execution"),
        Err(error) => error,
    };
    let ProcessError::MissingCapabilities(missing) = error else {
        panic!("unsupported offline execution failed without a typed capability refusal");
    };
    assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
}

#[test]
fn observation_and_effect_construction_retain_one_measured_policy_fact() {
    let fixture = config_fixture(Confinement::Off, false, cwd(), 1024 * 1024, 1024);
    let (admitted, _probe_evidence) = block_on(RunEffectPorts::observe(&fixture.config))
        .expect("the off policy must be observable");
    let (_ports, constructed, _) = block_on(RunEffectPorts::for_run(fixture.config))
        .expect("the same admitted off policy must construct effects");
    assert_eq!(
        admitted, constructed,
        "effects drifted from the admitted observation"
    );
}

#[test]
fn admitted_observation_drift_is_refused_before_effect_construction() {
    let cases = ["backend", "revision", "capabilities", "policy digest"];

    for changed in cases {
        let fixture = config_fixture(Confinement::Off, false, cwd(), 1024 * 1024, 1024);
        let (observed, _) = block_on(RunEffectPorts::observe(&fixture.config))
            .expect("the off policy must be observable");
        let admitted = match changed {
            "backend" => ExecObservation::new(
                BackendIdentity::new(
                    format!("{}-changed", observed.backend().name()),
                    observed.backend().revision(),
                ),
                observed.capabilities(),
                observed.policy_digest().clone(),
            ),
            "revision" => ExecObservation::new(
                BackendIdentity::new(
                    observed.backend().name(),
                    format!("{}-changed", observed.backend().revision()),
                ),
                observed.capabilities(),
                observed.policy_digest().clone(),
            ),
            "capabilities" => {
                let capabilities = if observed.capabilities() == ExecCaps::empty() {
                    ExecCaps::READ_SCOPED
                } else {
                    ExecCaps::empty()
                };
                ExecObservation::new(
                    observed.backend().clone(),
                    capabilities,
                    observed.policy_digest().clone(),
                )
            }
            "policy digest" => {
                let digest = if observed.policy_digest().as_str() == "0".repeat(64) {
                    "f".repeat(64)
                } else {
                    "0".repeat(64)
                };
                ExecObservation::new(
                    observed.backend().clone(),
                    observed.capabilities(),
                    PolicyDigest::try_from(digest).expect("changed digest is a SHA-256 value"),
                )
            }
            _ => unreachable!("all drift dimensions are covered"),
        };
        let config = fixture.config.with_admitted_observation(admitted.clone());
        let error = match block_on(RunEffectPorts::for_run(config)) {
            Ok(_) => panic!("{changed} drift was admitted and constructed effects"),
            Err(error) => error,
        };
        match error {
            ProcessError::ConfinementChanged {
                admitted: actual_admitted,
                observed: actual_observed,
            } => {
                assert_eq!(
                    *actual_admitted, admitted,
                    "admitted {changed} fact changed"
                );
                assert_eq!(
                    *actual_observed, observed,
                    "observed {changed} fact changed"
                );
            }
            other => panic!("{changed} drift returned the wrong refusal: {other:?}"),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn observation_persists_supported_measurement_transcript_in_the_session_store() {
    let workspace = cwd();
    let Some(_) = open_confined_run(
        Confinement::Workspace,
        false,
        workspace.clone(),
        1024 * 1024,
        1024,
    ) else {
        return;
    };
    let fixture = config_fixture(Confinement::Workspace, false, workspace, 1024 * 1024, 1024);
    let (observation, probe_evidence) =
        block_on(RunEffectPorts::observe(&fixture.config)).expect("workspace observation");
    assert!(
        observation
            .capabilities()
            .contains(required_caps(Confinement::Workspace, false)),
        "a supported observation must satisfy its policy requirements"
    );
    let reference = probe_evidence.expect("supported measurement must commit its transcript");
    let live = read_evidence_pages(&fixture.evidence, &reference);
    assert!(
        !live.is_empty(),
        "the committed transcript must not be empty"
    );

    let reopened = SessionEvidenceStore::open(
        fixture.evidence_root.path(),
        fixture.session,
        fixture.evidence.hard_cap(),
    )
    .expect("session evidence store reopens");
    assert_eq!(
        read_evidence_pages(&reopened, &reference),
        live,
        "the observation transcript must survive reopening its session store"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn observation_persists_workspace_measurement_in_a_fresh_subprocess() {
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--ignored",
            "--exact",
            "--nocapture",
            "workspace_observation_child",
        ])
        .env("EGO_WORKSPACE_OBSERVATION_CHILD", "1")
        .output()
        .expect("workspace observation child starts");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("workspace observation child stdout:\n{stdout}");
    eprint!("workspace observation child stderr:\n{stderr}");
    assert!(output.status.success(), "child failed: {stderr}");
    let record: serde_json::Value = stdout
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("child emits a JSON observation record");
    assert_eq!(record["status"], "observed");
    assert_eq!(record["confinement"], "workspace");
    let reference: EvidenceRef = serde_json::from_value(record["probe_reference"].clone())
        .expect("child emits the EvidenceRef string accepted by serde");
    let transcript_bytes = record["probe_bytes_len"]
        .as_u64()
        .expect("child emits the exact transcript byte count");
    assert_eq!(reference.bytes, transcript_bytes);
    assert!(transcript_bytes > 0);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "fresh subprocess entry point for workspace observation proof"]
fn workspace_observation_child() {
    if std::env::var_os("EGO_WORKSPACE_OBSERVATION_CHILD").is_none() {
        return;
    }
    let workspace = tempfile::tempdir().expect("writable workspace");
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    let fixture = config_fixture(
        Confinement::Workspace,
        false,
        CanonPath::new(canonical),
        1024 * 1024,
        1024,
    );
    let result = block_on(RunEffectPorts::observe(&fixture.config));
    let (observation, reference, bytes, error) = match result {
        Ok((observation, reference)) => {
            let bytes = reference
                .as_ref()
                .map(|reference| read_evidence_pages(&fixture.evidence, reference));
            (Some(observation), reference, bytes, None)
        }
        Err(error) => (None, None, None, Some(error.to_string())),
    };
    println!(
        "{}",
        serde_json::json!({
            "status": if error.is_some() { "error" } else { "observed" },
            "confinement": "workspace",
            "observation": observation,
            "probe_reference": reference,
            "probe_bytes": bytes.as_ref().map(|bytes| String::from_utf8_lossy(bytes)),
            "probe_bytes_len": bytes.as_ref().map(Vec::len),
            "error": error,
        })
    );
    let observation = observation.expect("workspace observation was unavailable");
    let reference = reference.expect("workspace observation probe reference");
    let bytes = bytes.expect("workspace probe transcript");
    assert!(
        observation
            .capabilities()
            .contains(required_caps(Confinement::Workspace, false)),
        "workspace observation lacks required capabilities: {observation:?}"
    );
    assert!(!bytes.is_empty(), "workspace probe transcript is empty");
    let _ = reference;
}

#[cfg(target_os = "linux")]
#[test]
fn linux_landlock_abi_status_and_workspace_rights_are_fail_closed() {
    let root = cwd();
    let Some(run) = open_confined_run(Confinement::Workspace, false, root, 1024 * 1024, 1024)
    else {
        return;
    };
    assert!(
        run.observation
            .capabilities()
            .contains(ExecCaps::READ_SCOPED)
    );
    assert!(
        run.observation
            .capabilities()
            .contains(ExecCaps::WRITE_SCOPED)
    );
    assert!(
        !run.observation
            .capabilities()
            .contains(ExecCaps::NO_IP_NETWORK)
    );

    // The helper path is an implementation detail, but the target remains a
    // public exact-argv contract: a confined launch must execute the same
    // executable and arguments, not a helper substitute. The target is an
    // installed utility because a confined child may execute only beneath the
    // runtime roots and the workspace, and this test binary lives in the
    // target directory, outside both (the same rule as the mutation matrix).
    let echo = std::fs::canonicalize("/bin/echo").expect("an installed echo utility");
    let spec = SpawnSpec::new(
        vec![echo.into_os_string(), OsString::from("linux exact target")],
        Some(cwd()),
        None,
        EnvPolicy::Inherited,
    )
    .expect("argv is valid before launch");
    let out = block_on(run.ports.process().execute(&spec))
        .expect("a complete Landlock probe must execute its exact target argv");
    assert!(format!("{out:?}").contains("linux exact target"));

    let missing = match open_run(Confinement::Off, true, cwd(), 1024 * 1024, 1024) {
        Err(ProcessError::MissingCapabilities(missing)) => missing,
        Err(err) => panic!("offline failed without a precise capability refusal: {err:?}"),
        Ok(_) => panic!("Landlock-only Linux backend claimed complete offline capability"),
    };
    assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_landlock_workspace_requires_refer_truncate_and_known_rights() {
    // Refer is exercised by hard-link creation and Truncate by both the
    // OpenOptions path and overwrite.  The complete paired matrix is the
    // public behavioural proof for these ABI rights; no private Landlock
    // AccessFs builder is imported here.
    workspace_confinement_covers_required_mutations_inside_and_outside();

    let root = cwd();
    let Some(run) = open_confined_run(Confinement::Workspace, false, root, 1024 * 1024, 1024)
    else {
        return;
    };
    assert!(
        run.observation
            .capabilities()
            .contains(ExecCaps::WRITE_SCOPED)
    );
    assert!(
        run.observation
            .capabilities()
            .contains(ExecCaps::READ_SCOPED)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_offline_for_run_refuses_unconditionally_without_no_ip_network_proof() {
    let error = match open_run(Confinement::Off, true, cwd(), 1024 * 1024, 1024) {
        Ok(_) => panic!("Landlock must never claim NO_IP_NETWORK without paired controls"),
        Err(error) => error,
    };
    let ProcessError::MissingCapabilities(missing) = error else {
        panic!("offline Linux refusal must name the missing capability");
    };
    assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn null_backend_refuses_confinement_but_off_online_executes() {
    let root = cwd();
    let missing = match open_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) {
        Err(ProcessError::MissingCapabilities(missing)) => missing,
        Err(err) => panic!("workspace failed without a precise capability refusal: {err:?}"),
        Ok(_) => panic!("null backend claimed workspace confinement"),
    };
    assert!(
        missing
            .missing()
            .contains(ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED))
    );
    let missing = match open_run(Confinement::Off, true, root.clone(), 1024 * 1024, 1024) {
        Err(ProcessError::MissingCapabilities(missing)) => missing,
        Err(err) => panic!("offline failed without a precise capability refusal: {err:?}"),
        Ok(_) => panic!("null backend claimed offline confinement"),
    };
    assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
    let run = open_run(Confinement::Off, false, root, 1024 * 1024, 1024).unwrap();
    execute(
        &run,
        fixture_arg_spec(
            "exec_contract_fixture_echoes_argument",
            [OsString::from("windows exact argv")],
            None,
            cwd(),
        ),
    )
    .expect("off-only backend still executes exact argv");
}

#[test]
fn cancellation_uses_the_caller_token_and_returns_cancelled() {
    let root = cwd();
    let run = open_run(
        Confinement::Off,
        false,
        root.clone(),
        64 * 1024 * 1024,
        1024,
    )
    .unwrap();
    let cancel = run.cancel.clone();
    let handle = std::thread::spawn(move || {
        execute(
            &run,
            fixture_arg_spec("exec_contract_fixture_emits_until_killed", [], None, cwd()),
        )
    });
    std::thread::sleep(Duration::from_millis(100));
    cancel.cancel(CancelCause::User);
    let result = handle.join().expect("executor thread");
    assert!(matches!(result, Err(ProcessError::Cancelled)));
}

#[test]
fn cancellation_kills_a_descendant_process_group_not_only_the_direct_child() {
    let dir = tempfile::tempdir().expect("process fixture directory");
    let pid_path = dir.path().join("cancel-descendant.pid");
    let root = cwd();
    let run = open_run(
        Confinement::Off,
        false,
        root.clone(),
        64 * 1024 * 1024,
        1024,
    )
    .unwrap();
    let cancel = run.cancel.clone();
    let child_pid_path = pid_path.clone();
    let handle = std::thread::spawn(move || {
        execute(
            &run,
            fixture_spec(
                "exec_contract_fixture_holds_a_descendant",
                &[child_pid_path.as_path()],
                None,
                cwd(),
            ),
        )
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    while !pid_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        pid_path.exists(),
        "descendant fixture never reached its pid handoff"
    );
    cancel.cancel(CancelCause::User);
    let result = handle.join().expect("executor thread");
    assert!(matches!(result, Err(ProcessError::Cancelled)));

    let pid = std::fs::read_to_string(pid_path).expect("fixture wrote descendant pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while descendant_is_alive(pid.trim()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !descendant_is_alive(pid.trim()),
        "descendant process survived cancellation cleanup"
    );
}

#[test]
fn timeout_kills_a_descendant_process_group_not_only_the_direct_child() {
    let dir = tempfile::tempdir().expect("process fixture directory");
    let pid_path = dir.path().join("descendant.pid");
    let root = cwd();
    let run = open_run(
        Confinement::Off,
        false,
        root.clone(),
        64 * 1024 * 1024,
        1024,
    )
    .unwrap();
    let process = fixture_spec(
        "exec_contract_fixture_holds_a_descendant",
        &[pid_path.as_path()],
        Some(Duration::from_secs(2)),
        root,
    );
    let err = execute(&run, process).unwrap_err();

    assert!(matches!(err, ProcessError::Timeout { .. }));
    let pid = std::fs::read_to_string(pid_path).expect("fixture wrote descendant pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while descendant_is_alive(pid.trim()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !descendant_is_alive(pid.trim()),
        "descendant process survived timeout cleanup"
    );
}

#[cfg(unix)]
fn descendant_is_alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .status()
        .expect("kill utility")
        .success()
}

#[cfg(windows)]
fn descendant_is_alive(pid: &str) -> bool {
    let filter = format!("PID eq {pid}");
    let output = Command::new("tasklist")
        .args(["/FI", filter.as_str(), "/NH"])
        .output()
        .expect("tasklist utility");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.split_whitespace().any(|field| field == pid))
}

fn local_non_loopback() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").expect("local UDP socket");
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// Whether a paired control on a non-loopback address can be established here
/// at all — which is the precondition the offline measurement actually has.
///
/// Having an address is not the same as being able to pair on it. macOS gates
/// local-network traffic behind a per-application permission, and a process
/// without it sees `connect` report success while the packets never arrive; a
/// VPN default route can put a tunnel address in the same state. Either way the
/// probe cannot demonstrate the guarantee and refuses it, which is correct. The
/// test has to ask the same question the probe does, rather than asking the
/// weaker one and then reading a refusal as proof.
fn non_loopback_control_pairs(ip: std::net::IpAddr) -> bool {
    use std::io::Write as _;

    let Ok(listener) = TcpListener::bind((ip, 0)) else {
        return false;
    };
    let (Ok(endpoint), Ok(())) = (listener.local_addr(), listener.set_nonblocking(true)) else {
        return false;
    };
    let Ok(mut client) = TcpStream::connect_timeout(&endpoint, Duration::from_millis(250)) else {
        return false;
    };
    let _ = client.write_all(b"pair");
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    loop {
        match listener.accept() {
            Ok(_) => return true,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return false,
        }
    }
}

#[cfg(target_os = "macos")]
fn non_loopback_udp_control_pairs(ip: std::net::IpAddr) -> bool {
    let bind = if ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let Ok(receiver) = UdpSocket::bind((ip, 0)) else {
        return false;
    };
    let (Ok(endpoint), Ok(sender)) = (receiver.local_addr(), UdpSocket::bind(bind)) else {
        return false;
    };
    if receiver
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
        || sender.send_to(b"pair", endpoint).ok() != Some(4)
    {
        return false;
    }
    let mut received = [0_u8; 4];
    receiver
        .recv_from(&mut received)
        .is_ok_and(|(length, _)| length == received.len() && received == *b"pair")
}

enum NetworkControl {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

fn tcp_control(ip: std::net::IpAddr) -> (String, NetworkControl) {
    let listener = TcpListener::bind((ip, 0)).expect("tcp control listener");
    let endpoint = listener.local_addr().unwrap().to_string();
    (endpoint, NetworkControl::Tcp(listener))
}

fn udp_control(ip: std::net::IpAddr) -> (String, NetworkControl) {
    let socket = UdpSocket::bind((ip, 0)).expect("udp control listener");
    let endpoint = socket.local_addr().unwrap().to_string();
    (endpoint, NetworkControl::Udp(socket))
}

#[cfg(target_os = "macos")]
fn open_offline_control_run(
    workspace: CanonPath,
    nonloopback: Option<std::net::IpAddr>,
) -> Option<Run> {
    let attempt = attempt_run(Confinement::Off, true, workspace, 1024 * 1024, 1024);
    if let Err(ProcessError::MissingCapabilities(missing)) = &attempt.outcome {
        assert_eq!(missing.required(), ExecCaps::NO_IP_NETWORK);
        assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
        if std::env::var_os(BACKEND_ABSENT).is_some() {
            return None;
        }
        let Some(nonloopback) = nonloopback else {
            // Non-loopback coverage is unavailable, but the caller must still
            // run every loopback baseline and check any admitted offline child.
            return None;
        };
        let reference = missing
            .probe_evidence()
            .expect("measured refusal has evidence");
        let bytes = read_evidence_pages(&attempt.evidence, reference);
        let transcript = std::str::from_utf8(&bytes).expect("probe transcript is UTF-8");
        let records: Vec<serde_json::Value> = transcript
            .lines()
            .map(|line| serde_json::from_str(line).expect("probe record is JSON"))
            .collect();
        // The early host check is not a lease on network availability. Identity
        // acquisition can take time. Re-establish this existing prerequisite
        // at refusal, without accepting a failed confined half as host evidence.
        let receiver_failure = records
            .iter()
            .find_map(|record| {
                record["suite"]
                    .as_str()
                    .filter(|suite| suite.ends_with("-nonloopback.control.receiver"))
            })
            .expect("a failed baseline receiver names its protocol");
        let control = receiver_failure
            .strip_suffix(".receiver")
            .expect("receiver suite suffix");
        let confined = control
            .strip_suffix(".control")
            .expect("control suite suffix");
        let control_ran = records
            .iter()
            .any(|record| record["suite"] == control && record["outcome"] == "ok");
        let confined_ran = records.iter().any(|record| record["suite"] == confined);
        assert!(
            control_ran && !confined_ran,
            "offline failed without an unpaired host control: {transcript}"
        );
        let independent_control_pairs = if receiver_failure.starts_with("offline.tcp-") {
            non_loopback_control_pairs(nonloopback)
        } else if receiver_failure.starts_with("offline.udp-") {
            non_loopback_udp_control_pairs(nonloopback)
        } else {
            panic!("unknown receiver suite: {receiver_failure}");
        };
        assert!(
            !independent_control_pairs,
            "independent host control still works; confinement must be demonstrated: {transcript}"
        );
        eprintln!(
            "note: non-loopback pairing became unavailable during measurement; verified precise refusal against a fresh independent control"
        );
        return None;
    }
    Some(
        attempt
            .into_run()
            .unwrap_or_else(|error| panic!("offline run failed: {error}")),
    )
}

#[cfg(not(target_os = "macos"))]
#[test]
fn offline_requires_tcp_and_udp_loopback_and_nonloopback_controls() {
    assert_offline_requires_tcp_and_udp_loopback_and_nonloopback_controls();
}

fn assert_offline_requires_tcp_and_udp_loopback_and_nonloopback_controls() {
    let root = cwd();
    let mut endpoints = vec![
        ("tcp", tcp_control("127.0.0.1".parse().unwrap())),
        ("udp", udp_control("127.0.0.1".parse().unwrap())),
    ];
    let nonloopback = local_non_loopback().filter(|ip| non_loopback_control_pairs(*ip));
    #[cfg(target_os = "macos")]
    let nonloopback = nonloopback.or_else(|| {
        eprintln!("note: initial non-loopback TCP control unavailable; re-pairing a fresh control");
        local_non_loopback().filter(|ip| non_loopback_control_pairs(*ip))
    });
    if let Some(nonloopback) = nonloopback {
        endpoints.extend([
            ("tcp", tcp_control(nonloopback)),
            ("udp", udp_control(nonloopback)),
        ]);
    } else {
        #[cfg(target_os = "macos")]
        eprintln!(
            "not-verifiable: no pairable non-loopback control after a fresh measurement; \
             non-loopback offline coverage is unavailable; checking loopback controls"
        );
        #[cfg(not(target_os = "macos"))]
        {
            // A missing host prerequisite must cause a precise refusal.
            eprintln!(
                "note: no pairable non-loopback address on this host; asserting the refusal is \
                 precise rather than the guarantee is demonstrated"
            );
            let missing = match open_run(Confinement::Off, true, root, 1024 * 1024, 1024) {
                Err(ProcessError::MissingCapabilities(missing)) => missing,
                Err(err) => panic!("offline failed without precise capability refusal: {err:?}"),
                Ok(_) => panic!("offline proceeded without a non-loopback probe prerequisite"),
            };
            assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
            return;
        }
    }

    for (protocol, (endpoint, control)) in endpoints {
        let workspace = root.clone();
        let baseline = open_run(
            Confinement::Off,
            false,
            workspace.clone(),
            1024 * 1024,
            1024,
        )
        .unwrap();
        let network_spec = fixture_arg_spec(
            "exec_contract_fixture_connects",
            [OsString::from(protocol), OsString::from(endpoint)],
            None,
            workspace.clone(),
        );
        execute(&baseline, network_spec.clone()).expect("paired network control must work");

        // Check every admitted offline child. macOS accepts a precise refusal
        // only when independent non-loopback coverage is unavailable.
        #[cfg(target_os = "macos")]
        let offline = open_offline_control_run(workspace, nonloopback);
        // Linux never demonstrates offline (ADR 2026-08-21 §5): Landlock's
        // network rules are port-based, cannot tell loopback from anything
        // else, and no second mechanism proves the no-IP contract. Offline
        // must therefore be refused, naming exactly the guarantee asked for,
        // while the paired control above shows the network was reachable.
        #[cfg(target_os = "linux")]
        let offline: Option<Run> =
            match open_run(Confinement::Off, true, workspace, 1024 * 1024, 1024) {
                Err(ProcessError::MissingCapabilities(missing)) => {
                    assert_eq!(missing.required(), required_caps(Confinement::Off, true));
                    assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
                    None
                }
                Err(error) => {
                    panic!("offline failed without a precise capability refusal: {error}")
                }
                Ok(_) => panic!("Linux offline proceeded without a mechanism that proves it"),
            };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let offline = open_confined_run(Confinement::Off, true, workspace, 1024 * 1024, 1024);
        if let Some(offline) = offline {
            assert!(
                execute(&offline, network_spec).is_err(),
                "offline child reached an IP endpoint"
            );
        } else if cfg!(not(any(target_os = "linux", target_os = "macos"))) {
            return;
        }
        match control {
            NetworkControl::Tcp(listener) => drop(listener),
            NetworkControl::Udp(socket) => drop(socket),
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_offline_controls_cover_tcp_udp_and_both_address_classes() {
    // Catches: an admitted offline child reaches a live loopback endpoint,
    // including when independent non-loopback coverage is unavailable.
    // The same paired controls are intentionally run under the Seatbelt-only
    // target gate. A missing profile compiler/backend must return a precise
    // capability refusal; it may not silently run the child unconfined.
    assert_offline_requires_tcp_and_udp_loopback_and_nonloopback_controls();
}

// Catches: granting metadata only to the immediate workspace parent, or only in workspace mode.
#[cfg(target_os = "macos")]
#[test]
fn macos_standalone_nested_workspace_stats_every_ancestor_even_read_only() {
    let outer = tempfile::tempdir().expect("outer directory");
    let workspace = outer.path().join("parent with spaces/child/workspace");
    std::fs::create_dir_all(&workspace).expect("nested workspace");
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let mut argv = vec![OsString::from("/usr/bin/stat")];
    argv.extend(
        workspace
            .ancestors()
            .map(|path| path.as_os_str().to_owned()),
    );
    for level in [Confinement::Workspace, Confinement::Ro] {
        let run = open_policy(
            run_confinement(level, false, CanonPath::new(workspace.clone())),
            1024 * 1024,
            1024,
        )
        .expect("standalone run");
        execute(
            &run,
            SpawnSpec::new(
                argv.clone(),
                Some(CanonPath::new(workspace.clone())),
                None,
                EnvPolicy::Inherited,
            )
            .expect("stat spec"),
        )
        .expect("every workspace ancestor must be statable");
        if level == Confinement::Ro {
            let forbidden = workspace.join("read-only-write");
            execute(
                &run,
                SpawnSpec::new(
                    vec!["/usr/bin/touch".into(), forbidden.clone().into_os_string()],
                    Some(CanonPath::new(workspace.clone())),
                    None,
                    EnvPolicy::Inherited,
                )
                .expect("touch spec"),
            )
            .expect_err("metadata permission must not grant read-only writes");
            assert!(!forbidden.exists(), "read-only workspace was modified");
        }
    }
}

#[cfg(target_os = "macos")]
fn homebrew_spec(program: &str, args: &[OsString], workspace: &std::path::Path) -> SpawnSpec {
    let path = std::path::Path::new("/opt/homebrew/bin").join(program);
    let canonical = std::fs::canonicalize(&path).expect("native acceptance requires Homebrew tool");
    assert!(canonical.starts_with("/opt/homebrew/Cellar"));
    // Use the installed symlink, not just the canonical binary: both paths must work.
    let mut argv = vec![path.into_os_string()];
    argv.extend_from_slice(args);
    SpawnSpec::new(
        argv,
        Some(CanonPath::new(std::fs::canonicalize(workspace).unwrap())),
        None,
        EnvPolicy::Inherited,
    )
    .expect("Homebrew spec")
}

#[cfg(target_os = "macos")]
#[test]
fn macos_exec_paths_default_python_catches_homebrew_prefix_missing_from_runtime_roots() {
    let workspace = tempfile::tempdir().unwrap();
    let run = open_policy(
        workspace_policy(workspace.path(), Vec::new()),
        1024 * 1024,
        1024,
    )
    .expect("default standalone workspace confinement");
    execute(
        &run,
        homebrew_spec("python3", &["--version".into()], workspace.path()),
    )
    .expect("standalone Homebrew Python runs");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_exec_paths_default_git_catches_cellar_symlink_target_not_admitted() {
    let workspace = tempfile::tempdir().unwrap();
    let run = open_policy(
        workspace_policy(workspace.path(), Vec::new()),
        1024 * 1024,
        1024,
    )
    .expect("default standalone workspace confinement");
    execute(
        &run,
        homebrew_spec("git", &["--version".into()], workspace.path()),
    )
    .expect("standalone Homebrew Git runs through its Cellar symlink");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_exec_paths_default_python_catches_runtime_root_widening_writes() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let run = open_policy(
        workspace_policy(workspace.path(), Vec::new()),
        1024 * 1024,
        1024,
    )
    .expect("default standalone workspace confinement");
    let write = |target: &std::path::Path| {
        execute(
            &run,
            homebrew_spec(
                "python3",
                &[
                    "-c".into(),
                    "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('written')".into(),
                    target.as_os_str().to_owned(),
                ],
                workspace.path(),
            ),
        )
    };
    let inside = workspace.path().join("inside");
    write(&inside).expect("control write inside workspace");
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "written");
    let target = outside.path().join("outside");
    assert!(
        write(&target).is_err(),
        "Homebrew child wrote outside confinement"
    );
    assert!(!target.exists());
}

// Catches: the selected environment policy is lost between config resolution,
// builtin preparation and the actual child process.
#[test]
fn configured_exec_environment_reaches_real_builtin_children() {
    use ego_core::config::{ConfigLayer, Trust, resolve};
    let root = cwd();
    let executable = std::env::current_exe().unwrap();
    let executable_text = executable.to_str().unwrap();
    let policy = ego_core::permit::PolicyEngine::new(
        root.clone(),
        Mode::Yolo,
        Ruleset::from_layer(
            PermCfg {
                allow: vec![exact_executable_rule(executable_text)],
                deny: vec![],
                judge: None,
            },
            Source::User,
        ),
    );
    let fs = OsFs;
    let run = open_run(Confinement::Off, false, root.clone(), 1024 * 1024, 1024).unwrap();
    for (config, expected) in [
        ("", "PATH=present"),
        ("[exec]\nenv='core'", "PATH=present"),
        ("[exec]\nenv='allowlist'\nenv_allowlist=[]", "PATH=absent"),
        (
            "[exec]\nenv='allowlist'\nenv_allowlist=['PATH']",
            "PATH=present",
        ),
    ] {
        let effective = resolve(
            &[ConfigLayer::parse(Source::User, config).unwrap()],
            Trust::Untrusted,
            None,
        )
        .unwrap();
        let registry =
            ego_tools::tools::builtins_with_environment(effective.exec.policy().unwrap());
        let tools = TurnTools::new(
            &registry,
            ToolEffectPorts::new(
                &fs,
                run.ports.process(),
                &run.evidence,
                run.ports.resolver_handle(),
                run.ports.one_hop_handle(),
                run.ports.duplex_handle(),
            ),
            root.clone(),
        );
        let params = serde_json::json!({
            "argv": [executable_text, "--ignored", "--exact",
                "exec_contract_fixture_reports_environment_presence_without_values",
                "--nocapture", "--", "PATH"],
            "timeout_ms": 10000
        });
        let (tool, call) = tools.prepare("exec", &params.to_string()).unwrap();
        let Classification::Settled(Ok(authorized)) = tools.authorize(&policy, call) else {
            panic!("fixture command must be authorized");
        };
        let output = block_on(tools.execute(&policy, tool.as_ref(), authorized, None)).unwrap();
        assert!(
            format!("{output:?}").contains(expected),
            "{config}: {output:?}"
        );
    }
}
