//! RED contract for argv-native execution, confinement, and process evidence.
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
    CanonPath, Classification, Confinement, EnvPolicy, ExecCaps, ExecObservation, Op, PolicyDenied,
    RunConfinement, SpawnSpec, StreamProvenance, ToolOutputProvenance, ToolResult,
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

fn run_confinement(level: Confinement, offline: bool, workspace: CanonPath) -> RunConfinement {
    RunConfinement::new(level, offline, workspace).unwrap()
}

fn open_run(
    level: Confinement,
    offline: bool,
    workspace: CanonPath,
    hard_cap: u64,
    preview: usize,
) -> Result<Run, ProcessError> {
    let evidence_root = tempfile::tempdir().expect("evidence root");
    let evidence = SessionEvidenceStore::open(evidence_root.path(), SessionId::new(), hard_cap)
        .map_err(ProcessError::Evidence)?;
    let cancel = CancelToken::new();
    let limits = ExecLimits::new(preview, Duration::from_millis(250))?;
    let http_call_timeout = HttpCallTimeout::new(Duration::from_secs(1))?;
    let policy = run_confinement(level, offline, workspace);
    let config = ExecRunConfig::new(
        policy,
        evidence.clone(),
        cancel.clone(),
        limits,
        http_call_timeout,
        None,
        Arc::new(SystemResolver::new()),
    );
    let (ports, observation) = block_on(RunEffectPorts::for_run(config))?;
    Ok(Run {
        _evidence_root: evidence_root,
        evidence,
        ports,
        observation,
        cancel,
    })
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

fn mutation_spec(operation: &str, paths: &[&std::path::Path], workspace: CanonPath) -> SpawnSpec {
    let mut args = vec![OsString::from(operation)];
    args.extend(
        paths
            .iter()
            .map(|path| OsString::from(path.to_str().expect("fixture path is UTF-8"))),
    );
    fixture_arg_spec("exec_contract_fixture_mutates", args, None, workspace)
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
fn exec_contract_fixture_mutates() {
    let mut args = std::env::args().skip_while(|arg| arg != "--").skip(1);
    let operation = args.next().expect("mutation operation");
    let target = std::path::PathBuf::from(args.next().expect("mutation target"));
    match operation.as_str() {
        "create" => std::fs::write(target, b"created").expect("create target"),
        "overwrite" => std::fs::write(target, b"").expect("overwrite target"),
        "truncate" => {
            std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(target)
                .expect("truncate target");
        }
        "remove" => std::fs::remove_file(target).expect("remove target"),
        "rename" => {
            let destination = std::path::PathBuf::from(args.next().expect("rename destination"));
            std::fs::rename(target, destination).expect("rename target");
        }
        "link" => {
            let destination = std::path::PathBuf::from(args.next().expect("link destination"));
            std::fs::hard_link(target, destination).expect("link target");
        }
        other => panic!("unknown mutation operation: {other}"),
    }
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

#[test]
fn exec_tool_preparation_policy_authorization_and_guarded_execution_preserve_identity() {
    let root = cwd();
    let executable = std::env::current_exe().unwrap();
    let executable_text = executable.to_str().expect("test executable is UTF-8");
    let allow =
        vec![ego_core::permit::rule::Rule::load(&format!("exec({executable_text} *)")).unwrap()];
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
    assert_eq!(prepared_spec.env_policy(), &EnvPolicy::Inherited);
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
    let rule = ego_core::permit::rule::Rule::load(&format!("exec({executable_text} *)"))
        .expect("the exact executable rule parses");
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

#[cfg(target_os = "linux")]
#[test]
fn linux_target_argv_must_name_an_exact_executable_file() {
    let root = cwd();
    let run = match open_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) {
        Ok(run) => run,
        Err(ProcessError::MissingCapabilities(missing)) => {
            // The helper/backend prerequisite is itself fail-closed when
            // this host cannot demonstrate the requested workspace contract.
            assert!(missing.missing().contains(ExecCaps::READ_SCOPED));
            return;
        }
        Err(error) => panic!("workspace failed without a precise capability refusal: {error:?}"),
    };

    let fixture_dir = tempfile::tempdir_in(&root).expect("target fixture directory");
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

    let run = open_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    );
    match run {
        Ok(run) => {
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
        Err(ProcessError::MissingCapabilities(missing)) => assert_eq!(
            missing.required(),
            ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED)
        ),
        Err(err) => panic!("workspace failed without a precise capability refusal: {err:?}"),
    }

    let run = open_run(Confinement::Ro, false, root.clone(), 1024 * 1024, 1024);
    match run {
        Ok(run) => {
            let ro_inside = workspace_dir.path().join("ro-inside");
            let ro_outside = outside.path().join("ro-outside");
            assert!(execute(&run, mutation_spec("create", &[&ro_inside], root.clone())).is_err());
            assert!(!ro_inside.exists());
            assert!(execute(&run, mutation_spec("create", &[&ro_outside], root.clone())).is_err());
            assert!(!ro_outside.exists());
        }
        Err(ProcessError::MissingCapabilities(missing)) => assert_eq!(
            missing.required(),
            ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED)
        ),
        Err(err) => panic!("ro failed without a precise capability refusal: {err:?}"),
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

    match open_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) {
        Ok(run) => {
            read(&run, &inside).expect("workspace allows an inside read");
            assert!(read(&run, &outside_target).is_err());
        }
        Err(ProcessError::MissingCapabilities(missing)) => assert_eq!(
            missing.required(),
            ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED)
        ),
        Err(err) => panic!("workspace failed without a precise capability refusal: {err:?}"),
    }

    match open_run(Confinement::Ro, false, root.clone(), 1024 * 1024, 1024) {
        Ok(run) => {
            read(&run, &inside).expect("ro allows an inside read");
            assert!(read(&run, &outside_target).is_err());
        }
        Err(ProcessError::MissingCapabilities(missing)) => assert_eq!(
            missing.required(),
            ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED)
        ),
        Err(err) => panic!("ro failed without a precise capability refusal: {err:?}"),
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
    let run = match open_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) {
        Ok(run) => run,
        Err(ProcessError::MissingCapabilities(missing)) => {
            assert_eq!(
                missing.required(),
                ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED)
            );
            return;
        }
        Err(err) => panic!("workspace failed without a precise capability refusal: {err:?}"),
    };

    let inside_create = workspace.path().join("create");
    execute(
        &run,
        mutation_spec("create", &[&inside_create], root.clone()),
    )
    .expect("create inside workspace");

    let inside_overwrite = workspace.path().join("overwrite");
    std::fs::write(&inside_overwrite, "before").unwrap();
    execute(
        &run,
        mutation_spec("overwrite", &[&inside_overwrite], root.clone()),
    )
    .expect("overwrite inside workspace");
    assert!(std::fs::read(&inside_overwrite).unwrap().is_empty());

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
    // link, while its MissingCapabilities branch proves fail-closed status
    // when the kernel/backend cannot demonstrate all required rights.
    workspace_confinement_covers_required_mutations_inside_and_outside();
}

#[test]
fn read_only_confinement_denies_every_required_mutation_inside_the_root() {
    let workspace = tempfile::tempdir().expect("workspace fixture directory");
    let root = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
    let run = match open_run(Confinement::Ro, false, root.clone(), 1024 * 1024, 1024) {
        Ok(run) => run,
        Err(ProcessError::MissingCapabilities(missing)) => {
            assert!(
                missing
                    .missing()
                    .contains(ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED))
            );
            return;
        }
        Err(err) => panic!("ro failed without a precise capability refusal: {err:?}"),
    };

    let create = workspace.path().join("ro-create");
    assert!(execute(&run, mutation_spec("create", &[&create], root.clone())).is_err());
    assert!(!create.exists());

    let overwrite = workspace.path().join("ro-overwrite");
    std::fs::write(&overwrite, "before").unwrap();
    assert!(
        execute(
            &run,
            mutation_spec("overwrite", &[&overwrite], root.clone())
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
fn macos_seatbelt_profile_path_fixture_is_data_not_profile_injection() {
    let parent = tempfile::tempdir().expect("profile fixture directory");
    let hostile = parent.path().join(r#"quote");(deny file-read*)"#);
    std::fs::create_dir(&hostile).expect("hostile path fixture");
    let root = CanonPath::new(std::fs::canonicalize(&hostile).unwrap());
    let run = match open_run(
        Confinement::Workspace,
        false,
        root.clone(),
        1024 * 1024,
        1024,
    ) {
        Ok(run) => run,
        Err(ProcessError::MissingCapabilities(missing)) => {
            assert_eq!(
                missing.required(),
                ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED)
            );
            return;
        }
        Err(err) => panic!("hostile profile path failed without capability refusal: {err:?}"),
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
fn linux_landlock_abi_status_and_workspace_rights_are_fail_closed() {
    let root = cwd();
    let run = match open_run(Confinement::Workspace, false, root, 1024 * 1024, 1024) {
        Ok(run) => run,
        Err(ProcessError::MissingCapabilities(missing)) => {
            assert!(
                missing
                    .missing()
                    .contains(ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED))
            );
            return;
        }
        Err(err) => panic!("workspace failed without a precise capability refusal: {err:?}"),
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
    // current-test executable and fixture arguments, not a helper substitute.
    let out = block_on(run.ports.process().execute(&fixture_arg_spec(
        "exec_contract_fixture_echoes_argument",
        [OsString::from("linux exact target")],
        None,
        cwd(),
    )))
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
    let run = match open_run(Confinement::Workspace, false, root, 1024 * 1024, 1024) {
        Ok(run) => run,
        Err(ProcessError::MissingCapabilities(missing)) => {
            assert!(
                missing
                    .missing()
                    .contains(ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED))
            );
            return;
        }
        Err(error) => panic!("workspace rights refused without capability evidence: {error:?}"),
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
        Some(Duration::from_millis(100)),
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
    let Some(nonloopback) = local_non_loopback() else {
        // A host with no non-loopback interface cannot provide the paired
        // address-class control. It must still refuse the requested offline
        // guarantee rather than silently treating the missing probe as proof.
        let missing = match open_run(Confinement::Off, true, root, 1024 * 1024, 1024) {
            Err(ProcessError::MissingCapabilities(missing)) => missing,
            Err(err) => panic!("offline failed without precise capability refusal: {err:?}"),
            Ok(_) => panic!("offline proceeded without a non-loopback probe prerequisite"),
        };
        assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
        return;
    };
    endpoints.extend([
        ("tcp", tcp_control(nonloopback)),
        ("udp", udp_control(nonloopback)),
    ]);

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

        match open_run(Confinement::Off, true, workspace, 1024 * 1024, 1024) {
            Err(ProcessError::MissingCapabilities(missing)) => {
                assert!(missing.missing().contains(ExecCaps::NO_IP_NETWORK));
            }
            Err(err) => panic!("offline failed without precise capability refusal: {err:?}"),
            Ok(offline) => {
                assert!(
                    execute(&offline, network_spec).is_err(),
                    "offline child reached an IP endpoint"
                );
            }
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
    // The same paired controls are intentionally run under the Seatbelt-only
    // target gate. A missing profile compiler/backend must return a precise
    // capability refusal; it may not silently run the child unconfined.
    assert_offline_requires_tcp_and_udp_loopback_and_nonloopback_controls();
}
