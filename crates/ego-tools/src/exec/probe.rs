//! Paired behavioural probes through the common process/evidence lifecycle.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::event::id::SessionId;
use ego_core::permit::{
    CanonPath, Classification, Confinement, ExecCaps, ProcessError, RunConfinement, Tool, Typed,
};
use ego_core::turn::{CancelToken, ToolEffectPorts, Tools};
use sha2::{Digest as _, Sha256};

use crate::store::SessionEvidenceStore;

use super::backend::{Backend, MeasuredBackend};
use super::{ExecLimits, ExecRunConfig, HttpCallTimeout, ProbeLog, ProbeRecord, RunEffectPorts};

#[cfg(target_os = "linux")]
use super::landlock::{LandlockBackend, derived_runtime_roots};
#[cfg(target_os = "macos")]
use super::seatbelt::{SeatbeltBackend, derived_runtime_roots};

#[cfg(target_os = "linux")]
pub(crate) async fn landlock(
    policy: &RunConfinement,
    cancel: CancelToken,
    limits: ExecLimits,
    log: &ProbeLog,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    let scratch = private_scratch()?;
    let scratch_path = canonical_scratch(&scratch)?;
    let mut runtime_roots = derived_runtime_roots()?;
    runtime_roots.extend(policy.exec_paths().iter().cloned());
    let backend = Arc::new(LandlockBackend::prepare(
        policy,
        &scratch_path,
        &runtime_roots,
    )?);
    let measurement = measure_cached(
        backend.clone(),
        policy,
        &scratch_path,
        &runtime_roots,
        cancel,
        limits,
    )
    .await?;
    log.extend(&measurement.records);
    let capabilities = measurement.capabilities;
    let selected = Arc::new(MeasuredBackend::new(backend, capabilities, Some(scratch))?);
    Ok((selected, runtime_roots))
}

#[cfg(target_os = "macos")]
pub(crate) async fn seatbelt(
    policy: &RunConfinement,
    cancel: CancelToken,
    limits: ExecLimits,
    log: &ProbeLog,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    let scratch = private_scratch()?;
    let scratch_path = canonical_scratch(&scratch)?;
    // Configured exec paths are read and exec roots like the Apple runtime: one
    // list feeds the profile, the read probe and the policy digest, and no
    // write grant is ever derived from it.
    let mut runtime_roots = derived_runtime_roots()?;
    runtime_roots.extend(policy.exec_paths().iter().cloned());
    let backend = Arc::new(SeatbeltBackend::prepare(
        policy,
        &scratch_path,
        &runtime_roots,
    )?);
    backend.prerequisite(policy)?;
    let measurement = measure_cached(
        backend.clone(),
        policy,
        &scratch_path,
        &runtime_roots,
        cancel,
        limits,
    )
    .await?;
    log.extend(&measurement.records);
    let capabilities = measurement.capabilities;
    let selected = Arc::new(MeasuredBackend::new(backend, capabilities, Some(scratch))?);
    Ok((selected, runtime_roots))
}

#[cfg(target_os = "linux")]
pub(crate) async fn bwrap(
    policy: &RunConfinement,
    cancel: CancelToken,
    limits: ExecLimits,
    log: &ProbeLog,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    let scratch = private_scratch()?;
    let scratch_path = canonical_scratch(&scratch)?;
    let mut runtime_roots = derived_runtime_roots()?;
    runtime_roots.extend(policy.exec_paths().iter().cloned());
    if policy.offline() && policy.level() != Confinement::Off {
        // The same controlled network child is used by the shipped binary and
        // libtest hosts. Bind only its executable, never its parent directory.
        let executable = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|error| {
                ProcessError::Backend(format!("cannot locate network probe: {error}"))
            })?;
        runtime_roots.push(CanonPath::new(executable));
    }
    let backend = Arc::new(super::bwrap::BwrapBackend::prepare(
        policy,
        &scratch_path,
        &runtime_roots,
    )?);
    let measurement = measure_cached(
        backend.clone(),
        policy,
        &scratch_path,
        &runtime_roots,
        cancel,
        limits,
    )
    .await?;
    log.extend(&measurement.records);
    let selected = Arc::new(MeasuredBackend::new(
        backend,
        measurement.capabilities,
        Some(scratch),
    )?);
    Ok((selected, runtime_roots))
}

async fn measure_cached(
    backend: Arc<dyn Backend>,
    policy: &RunConfinement,
    scratch: &CanonPath,
    runtime_roots: &[CanonPath],
    cancel: CancelToken,
    limits: ExecLimits,
) -> Result<Measurement, ProcessError> {
    let cache_key =
        cache_key_async(policy.clone(), backend.clone(), runtime_roots.to_vec()).await?;
    let cached = cached_capabilities(cache_key);
    cached
        .get_or_try_init(|| async {
            let evidence = probe_evidence()?;
            let mut capabilities = measure(
                backend.clone(),
                policy,
                scratch,
                runtime_roots,
                evidence.clone(),
                cancel.clone(),
                limits,
            )
            .await;
            if backend
                .maximum_demonstrable_caps()
                .contains(ExecCaps::NO_IP_NETWORK)
                && probe_offline(
                    backend.clone(),
                    policy.workspace(),
                    evidence.clone(),
                    cancel.clone(),
                    limits,
                )
                .await
            {
                capabilities = capabilities.union(ExecCaps::NO_IP_NETWORK);
            }
            if cancel.is_cancelled() {
                return Err(ProcessError::Cancelled);
            }
            clear_scratch(scratch.as_path())?;
            Ok(Measurement {
                capabilities,
                records: evidence.log.snapshot(),
            })
        })
        .await
        .cloned()
}

/// The evidence one measurement writes into: the probe children's own output
/// through a disposable store, and the transcript of what was tried.
///
/// The transcript outlives the store on purpose. Probe output is bulk the run
/// never asked for; the transcript is the part a refusal has to cite.
#[derive(Clone)]
struct ProbeEvidence {
    _root: Arc<tempfile::TempDir>,
    store: SessionEvidenceStore,
    log: Arc<ProbeLog>,
}

fn probe_evidence() -> Result<ProbeEvidence, ProcessError> {
    let root = tempfile::tempdir().map_err(|error| {
        ProcessError::Backend(format!("cannot create probe evidence root: {error}"))
    })?;
    let store = SessionEvidenceStore::open(root.path(), SessionId::new(), 1024 * 1024)
        .map_err(ProcessError::Evidence)?;
    Ok(ProbeEvidence {
        _root: Arc::new(root),
        store,
        log: Arc::new(ProbeLog::default()),
    })
}

/// One paired probe group: what it is proving, and where.
struct ProbeSuite<'a> {
    name: &'a str,
    cwd: &'a CanonPath,
    root: &'a std::path::Path,
}

impl<'a> ProbeSuite<'a> {
    /// The unconfined half of a pair. A refusal under the backend proves
    /// nothing until the same command has been seen to work without it.
    fn control_name(&self) -> String {
        format!("{}.control", self.name)
    }

    fn renamed(&self, name: &'a str) -> ProbeSuite<'a> {
        ProbeSuite {
            name,
            cwd: self.cwd,
            root: self.root,
        }
    }
}

/// What one measurement established, kept together because a cached
/// capability without its transcript cannot explain itself.
#[derive(Clone)]
struct Measurement {
    capabilities: ExecCaps,
    records: Arc<[ProbeRecord]>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct CapabilityCacheKey {
    os: &'static str,
    arch: &'static str,
    build_identity: Vec<u8>,
    binary_hash: [u8; 32],
    backend_name: String,
    backend_revision: String,
    policy_digest: String,
    runtime_roots: Vec<CanonPath>,
}

type CachedCapabilities = tokio::sync::OnceCell<Measurement>;

/// Process-wide measurement facts, never run-owned mechanisms.
///
/// The cell makes a typed key single-flight while leaving every caller to
/// prepare and retain its own backend, profile and private scratch.
fn capability_cache()
-> &'static Mutex<std::collections::HashMap<CapabilityCacheKey, Arc<CachedCapabilities>>> {
    static CACHE: OnceLock<
        Mutex<std::collections::HashMap<CapabilityCacheKey, Arc<CachedCapabilities>>>,
    > = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn cached_capabilities(key: CapabilityCacheKey) -> Arc<CachedCapabilities> {
    let mut cache = capability_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        cache
            .entry(key)
            .or_insert_with(|| Arc::new(CachedCapabilities::new())),
    )
}

fn cache_key(
    policy: &RunConfinement,
    backend: &dyn Backend,
    runtime_roots: &[CanonPath],
) -> Result<CapabilityCacheKey, ProcessError> {
    // The running image cannot change under a process, so its identity is
    // read once: the hash is a full read of a binary tens of megabytes large.
    static IMAGE_HASH: OnceLock<Result<[u8; 32], String>> = OnceLock::new();
    let binary_hash = IMAGE_HASH
        .get_or_init(|| {
            let binary = std::env::current_exe()
                .map_err(|error| format!("cannot locate the current executable: {error}"))?;
            binary_hash(&binary).map_err(|error| error.to_string())
        })
        .clone()
        .map_err(ProcessError::Backend)?;
    cache_key_with_hash(policy, backend, runtime_roots, binary_hash)
}

async fn cache_key_async(
    policy: RunConfinement,
    backend: Arc<dyn Backend>,
    runtime_roots: Vec<CanonPath>,
) -> Result<CapabilityCacheKey, ProcessError> {
    tokio::task::spawn_blocking(move || cache_key(&policy, backend.as_ref(), &runtime_roots))
        .await
        .map_err(|error| {
            ProcessError::Backend(format!("capability cache identity task failed: {error}"))
        })?
}

#[cfg(test)]
fn cache_key_for_binary(
    policy: &RunConfinement,
    backend: &dyn Backend,
    runtime_roots: &[CanonPath],
    binary: &Path,
) -> Result<CapabilityCacheKey, ProcessError> {
    cache_key_with_hash(policy, backend, runtime_roots, binary_hash(binary)?)
}

fn binary_hash(binary: &Path) -> Result<[u8; 32], ProcessError> {
    let binary = fs::read(binary).map_err(|error| {
        ProcessError::Backend(format!(
            "cannot read the current executable for cache identity: {error}"
        ))
    })?;
    Ok(Sha256::digest(&binary).into())
}

fn cache_key_with_hash(
    policy: &RunConfinement,
    backend: &dyn Backend,
    runtime_roots: &[CanonPath],
    binary_hash: [u8; 32],
) -> Result<CapabilityCacheKey, ProcessError> {
    let identity = backend.identity();
    let digest = super::policy_digest_with_runtime_roots(policy, backend, runtime_roots)?;
    #[cfg(target_os = "linux")]
    let build_identity = fs::read("/proc/sys/kernel/osrelease").map_err(|error| {
        ProcessError::Backend(format!("cannot read Linux build identity: {error}"))
    })?;
    #[cfg(target_os = "macos")]
    let build_identity =
        fs::read("/System/Library/CoreServices/SystemVersion.plist").map_err(|error| {
            ProcessError::Backend(format!("cannot read macOS build identity: {error}"))
        })?;
    Ok(CapabilityCacheKey {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        build_identity,
        binary_hash,
        backend_name: identity.name().to_owned(),
        backend_revision: identity.revision().to_owned(),
        policy_digest: digest.as_str().to_owned(),
        runtime_roots: runtime_roots.to_vec(),
    })
}

fn private_scratch() -> Result<tempfile::TempDir, ProcessError> {
    let scratch = tempfile::tempdir().map_err(|error| {
        ProcessError::Backend(format!("cannot create process scratch: {error}"))
    })?;
    fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o700)).map_err(|error| {
        ProcessError::Backend(format!("cannot secure process scratch: {error}"))
    })?;
    Ok(scratch)
}

fn clear_scratch(path: &std::path::Path) -> Result<(), ProcessError> {
    for entry in fs::read_dir(path).map_err(|error| {
        ProcessError::Backend(format!("cannot inspect process scratch: {error}"))
    })? {
        let entry = entry.map_err(|error| {
            ProcessError::Backend(format!("cannot inspect process scratch entry: {error}"))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            ProcessError::Backend(format!("cannot classify process scratch entry: {error}"))
        })?;
        if file_type.is_dir() {
            fs::remove_dir_all(entry.path()).map_err(|error| {
                ProcessError::Backend(format!("cannot clear process scratch directory: {error}"))
            })?;
        } else {
            fs::remove_file(entry.path()).map_err(|error| {
                ProcessError::Backend(format!("cannot clear process scratch file: {error}"))
            })?;
        }
    }
    Ok(())
}

fn canonical_scratch(scratch: &tempfile::TempDir) -> Result<CanonPath, ProcessError> {
    fs::canonicalize(scratch.path())
        .map(CanonPath::new)
        .map_err(|error| ProcessError::Backend(format!("cannot resolve process scratch: {error}")))
}

async fn probe_offline(
    backend: Arc<dyn Backend>,
    cwd: &CanonPath,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
) -> bool {
    use std::net::{TcpListener, UdpSocket};

    let network_suite = |name: &'static str| ProbeSuite {
        name,
        cwd,
        root: cwd.as_path(),
    };
    if !probe(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        &network_suite("offline.launch-control"),
        vec!["/bin/true".into()],
    )
    .await
    {
        return false;
    }
    let Some(nonloopback) = local_nonloopback() else {
        return false;
    };
    let Ok(tcp_loopback) = TcpListener::bind(("127.0.0.1", 0)) else {
        return false;
    };
    let Ok(udp_loopback) = UdpSocket::bind(("127.0.0.1", 0)) else {
        return false;
    };
    let Ok(tcp_nonloopback) = TcpListener::bind((nonloopback, 0)) else {
        return false;
    };
    let Ok(udp_nonloopback) = UdpSocket::bind((nonloopback, 0)) else {
        return false;
    };
    let baseline: Arc<dyn Backend> = Arc::new(super::null::NullBackend);
    paired_tcp(
        &tcp_loopback,
        backend.clone(),
        baseline.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        &network_suite("offline.tcp-loopback"),
    )
    .await
        && paired_udp(
            &udp_loopback,
            backend.clone(),
            baseline.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &network_suite("offline.udp-loopback"),
        )
        .await
        && paired_tcp(
            &tcp_nonloopback,
            backend.clone(),
            baseline.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &network_suite("offline.tcp-nonloopback"),
        )
        .await
        && paired_udp(
            &udp_nonloopback,
            backend,
            baseline,
            evidence,
            cancel,
            limits,
            &network_suite("offline.udp-nonloopback"),
        )
        .await
}

fn local_nonloopback() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback()).then_some(address)
}

async fn paired_tcp(
    listener: &std::net::TcpListener,
    backend: Arc<dyn Backend>,
    baseline: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    suite: &ProbeSuite<'_>,
) -> bool {
    use std::io::Read as _;

    let control = suite.control_name();
    let Ok(endpoint) = listener.local_addr() else {
        return false;
    };
    if listener.set_nonblocking(true).is_err() {
        return false;
    }
    let sentinel = sentinel(endpoint);
    let receiver_failure = |reason: String| {
        evidence.log.record(ProbeRecord {
            suite: format!("{control}.receiver"),
            root: suite.root.to_path_buf(),
            argv: network_probe_argv("tcp", endpoint, &sentinel)
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
            outcome: reason,
        });
        false
    };
    if !probe(
        baseline,
        evidence.clone(),
        cancel.clone(),
        limits,
        &suite.renamed(&control),
        network_probe_argv("tcp", endpoint, &sentinel),
    )
    .await
    {
        return false;
    }
    let baseline_deadline = std::time::Instant::now() + Duration::from_millis(250);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < baseline_deadline =>
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => return receiver_failure(format!("accept failed: {error}")),
        }
    };
    if stream.set_nonblocking(false).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .is_err()
    {
        return receiver_failure("receiver socket configuration failed".into());
    }
    let mut received = vec![0_u8; sentinel.len()];
    if let Err(error) = stream.read_exact(&mut received) {
        return receiver_failure(format!("sentinel read failed: {error}"));
    }
    if received != sentinel.as_bytes() {
        return receiver_failure("sentinel mismatch".into());
    }
    if !probe_network_denied(
        backend,
        evidence,
        cancel,
        limits,
        suite,
        network_probe_argv("tcp", endpoint, &sentinel),
    )
    .await
    {
        return false;
    }
    let denied_deadline = std::time::Instant::now() + Duration::from_millis(100);
    loop {
        match listener.accept() {
            Ok(_) => return false,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < denied_deadline =>
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
}

async fn paired_udp(
    socket: &std::net::UdpSocket,
    backend: Arc<dyn Backend>,
    baseline: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    suite: &ProbeSuite<'_>,
) -> bool {
    let control = suite.control_name();
    let Ok(endpoint) = socket.local_addr() else {
        return false;
    };
    let sentinel = sentinel(endpoint);
    let receiver_failure = |reason: String| {
        evidence.log.record(ProbeRecord {
            suite: format!("{control}.receiver"),
            root: suite.root.to_path_buf(),
            argv: network_probe_argv("udp", endpoint, &sentinel)
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
            outcome: reason,
        });
        false
    };
    if !probe(
        baseline,
        evidence.clone(),
        cancel.clone(),
        limits,
        &suite.renamed(&control),
        network_probe_argv("udp", endpoint, &sentinel),
    )
    .await
    {
        return false;
    }
    let mut received = [0_u8; 128];
    if socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
    {
        return receiver_failure("receiver socket configuration failed".into());
    }
    let (received_len, _) = match socket.recv_from(&mut received) {
        Ok(received) => received,
        Err(error) => return receiver_failure(format!("sentinel receive failed: {error}")),
    };
    if received[..received_len] != sentinel.as_bytes()[..] {
        return receiver_failure("sentinel mismatch".into());
    }
    if !probe_network_denied(
        backend,
        evidence,
        cancel,
        limits,
        suite,
        network_probe_argv("udp", endpoint, &sentinel),
    )
    .await
    {
        return false;
    }
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .is_ok()
        && socket.recv_from(&mut received).is_err_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
}

fn sentinel(endpoint: std::net::SocketAddr) -> String {
    format!("ego-network-probe:{}:{endpoint}", std::process::id())
}

/// The one envelope the control child is ever spawned with.
///
/// It is shaped so that *both* executables that can host this library dispatch
/// it: the shipped binary intercepts it before clap, and a libtest binary runs
/// the `#[ignore]`d test named for the token, which calls the same dispatcher.
/// One shape, one parser, no test-only parameter — because the alternative is
/// two definitions of "perform the probe", and the one the tests exercise would
/// not be the one that measures a real capability.
fn network_probe_argv(
    protocol: &str,
    endpoint: std::net::SocketAddr,
    sentinel: &str,
) -> Vec<OsString> {
    vec![
        std::env::current_exe()
            .map(|path| path.into_os_string())
            .unwrap_or_default(),
        OsString::from("--ignored"),
        OsString::from("--exact"),
        OsString::from(NETWORK_PROBE_TOKEN),
        OsString::from("--nocapture"),
        OsString::from("--"),
        OsString::from(protocol),
        OsString::from(endpoint.to_string()),
        OsString::from(sentinel),
    ]
}

const NETWORK_PROBE_TOKEN: &str = "__ego_internal_network_probe_v1";

/// Dispatch the fixed controlled network probe before the ordinary CLI starts.
///
/// `Some(true)` means the sentinel was delivered, `Some(false)` that it was
/// not, and `None` that these arguments are not the probe envelope at all.
/// Reporting the outcome rather than an exit code is what lets the libtest
/// host run the very same dispatcher and assert on it: choosing a process exit
/// status is the binary's job, not this library's.
pub fn dispatch_network_probe<I>(arguments: I) -> Option<bool>
where
    I: IntoIterator<Item = OsString>,
{
    use std::io::Write as _;
    use std::net::{TcpStream, UdpSocket};

    let mut arguments = arguments.into_iter();
    let _executable = arguments.next()?;
    for expected in [
        "--ignored",
        "--exact",
        NETWORK_PROBE_TOKEN,
        "--nocapture",
        "--",
    ] {
        if arguments.next().as_deref() != Some(std::ffi::OsStr::new(expected)) {
            return None;
        }
    }
    let result = (|| {
        let protocol = arguments.next()?.into_string().ok()?;
        let endpoint = arguments
            .next()?
            .into_string()
            .ok()?
            .parse::<std::net::SocketAddr>()
            .ok()?;
        let sentinel = arguments.next()?.into_string().ok()?.into_bytes();
        if arguments.next().is_some() || sentinel.is_empty() {
            return None;
        }
        match protocol.as_str() {
            "tcp" => TcpStream::connect(endpoint)
                .and_then(|mut stream| stream.write_all(&sentinel))
                .ok(),
            "udp" => {
                let bind = if endpoint.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                };
                UdpSocket::bind(bind)
                    .and_then(|socket| {
                        let written = socket.send_to(&sentinel, endpoint)?;
                        if written == sentinel.len() {
                            // On macOS a process that closes its last UDP
                            // socket immediately after `send_to` can exit
                            // before a non-loopback local delivery becomes
                            // observable by the paired receiver.
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Ok(written)
                    })
                    .ok()
                    .filter(|written| *written == sentinel.len())
                    .map(|_| ())
            }
            _ => None,
        }
    })();
    Some(result.is_some())
}

async fn measure(
    backend: Arc<dyn Backend>,
    policy: &RunConfinement,
    scratch: &CanonPath,
    runtime_roots: &[CanonPath],
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
) -> ExecCaps {
    let maximum = backend.maximum_demonstrable_caps();
    let Some(fixture) = ProbeFixture::new(policy.workspace(), scratch) else {
        return ExecCaps::empty();
    };
    let mut measured = ExecCaps::empty();
    if maximum.contains(ExecCaps::READ_SCOPED)
        && probe_reads(
            backend.clone(),
            &fixture,
            evidence.clone(),
            cancel.clone(),
            limits,
            runtime_roots,
        )
        .await
    {
        measured = measured.union(ExecCaps::READ_SCOPED);
    }
    if maximum.contains(ExecCaps::WRITE_DENIED)
        && probe_denied_mutations(
            backend.clone(),
            &fixture,
            evidence.clone(),
            cancel.clone(),
            limits,
        )
        .await
    {
        measured = measured.union(ExecCaps::WRITE_DENIED);
    }
    if maximum.contains(ExecCaps::WRITE_SCOPED)
        && probe_scoped_mutations(backend, &fixture, evidence, cancel, limits).await
    {
        measured = measured.union(ExecCaps::WRITE_SCOPED);
    }
    measured
}

struct ProbeFixture {
    _inside: tempfile::TempDir,
    _outside: tempfile::TempDir,
    cwd: CanonPath,
    inside: std::path::PathBuf,
    outside: std::path::PathBuf,
    scratch: std::path::PathBuf,
}

impl ProbeFixture {
    fn new(workspace: &CanonPath, scratch: &CanonPath) -> Option<Self> {
        let inside = tempfile::Builder::new()
            .prefix(".ego-exec-probe-")
            .tempdir_in(workspace.as_path())
            .ok()?;
        let outside = tempfile::tempdir().ok()?;
        let inside_path = fs::canonicalize(inside.path()).ok()?;
        let outside_path = fs::canonicalize(outside.path()).ok()?;
        Some(Self {
            _inside: inside,
            _outside: outside,
            cwd: workspace.clone(),
            inside: inside_path,
            outside: outside_path,
            scratch: scratch.as_path().to_path_buf(),
        })
    }

    fn suite<'a>(&'a self, name: &'a str, root: &'a std::path::Path) -> ProbeSuite<'a> {
        ProbeSuite {
            name,
            cwd: &self.cwd,
            root,
        }
    }
}

async fn probe_reads(
    backend: Arc<dyn Backend>,
    fixture: &ProbeFixture,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    runtime_roots: &[CanonPath],
) -> bool {
    let inside = fixture.inside.join("read-control");
    let outside = fixture.outside.join("read-denied");
    if fs::write(&inside, b"inside").is_err() || fs::write(&outside, b"outside").is_err() {
        return false;
    }
    if !probe(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        &fixture.suite("read.workspace", &fixture.inside),
        vec!["/bin/cat".into(), inside.into_os_string()],
    )
    .await
        || probe(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &fixture.suite("read.outside", &fixture.outside),
            vec!["/bin/cat".into(), outside.into_os_string()],
        )
        .await
    {
        return false;
    }
    for root in runtime_roots {
        if !probe(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &fixture.suite("read.runtime-root", root.as_path()),
            vec!["/bin/ls".into(), root.as_path().as_os_str().to_owned()],
        )
        .await
        {
            return false;
        }
    }
    true
}

async fn probe_denied_mutations(
    backend: Arc<dyn Backend>,
    fixture: &ProbeFixture,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
) -> bool {
    mutation_suite(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        &fixture.suite("write-denied.workspace", &fixture.inside),
        false,
    )
    .await
        && mutation_suite(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &fixture.suite("write-denied.scratch", &fixture.scratch),
            false,
        )
        .await
        && mutation_suite(
            backend,
            evidence,
            cancel,
            limits,
            &fixture.suite("write-denied.outside", &fixture.outside),
            false,
        )
        .await
}

async fn probe_scoped_mutations(
    backend: Arc<dyn Backend>,
    fixture: &ProbeFixture,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
) -> bool {
    mutation_suite(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        &fixture.suite("write-scoped.workspace", &fixture.inside),
        true,
    )
    .await
        && mutation_suite(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &fixture.suite("write-scoped.scratch", &fixture.scratch),
            true,
        )
        .await
        && mutation_suite(
            backend,
            evidence,
            cancel,
            limits,
            &fixture.suite("write-scoped.outside", &fixture.outside),
            false,
        )
        .await
}

async fn mutation_suite(
    backend: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    suite: &ProbeSuite<'_>,
    allowed: bool,
) -> bool {
    let root = suite.root;
    let create = root.join("create");
    if probe(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        suite,
        vec!["/usr/bin/touch".into(), create.clone().into_os_string()],
    )
    .await
        != allowed
        || create.exists() != allowed
    {
        return false;
    }

    let overwrite_source = root.join("overwrite-source");
    let overwrite = root.join("overwrite-destination");
    if fs::write(&overwrite_source, b"after").is_err() || fs::write(&overwrite, b"before").is_err()
    {
        return false;
    }
    let overwrite_succeeded = probe(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        suite,
        vec![
            "/bin/cp".into(),
            overwrite_source.into_os_string(),
            overwrite.clone().into_os_string(),
        ],
    )
    .await;
    let overwrite_observed = fs::read(&overwrite)
        .map(|bytes| bytes == b"after")
        .unwrap_or(false);
    if overwrite_succeeded != allowed || overwrite_observed != allowed {
        return false;
    }

    let truncate = root.join("truncate");
    if fs::write(&truncate, b"before").is_err()
        || probe(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            suite,
            vec![
                "/usr/bin/truncate".into(),
                "-s".into(),
                "0".into(),
                truncate.clone().into_os_string(),
            ],
        )
        .await
            != allowed
        || fs::metadata(&truncate)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(false)
            != allowed
    {
        return false;
    }

    let remove = root.join("remove");
    if fs::write(&remove, b"remove").is_err()
        || probe(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            suite,
            vec!["/bin/rm".into(), remove.clone().into_os_string()],
        )
        .await
            != allowed
        || remove.exists() == allowed
    {
        return false;
    }

    let rename_source = root.join("rename-source");
    let rename_destination = root.join("rename-destination");
    if fs::write(&rename_source, b"rename").is_err()
        || probe(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            suite,
            vec![
                "/bin/mv".into(),
                rename_source.clone().into_os_string(),
                rename_destination.clone().into_os_string(),
            ],
        )
        .await
            != allowed
        || rename_destination.exists() != allowed
        || rename_source.exists() == allowed
    {
        return false;
    }

    let link_source = root.join("link-source");
    let link_destination = root.join("link-destination");
    if fs::write(&link_source, b"link").is_err()
        || probe(
            backend,
            evidence,
            cancel,
            limits,
            suite,
            vec![
                "/bin/ln".into(),
                link_source.into_os_string(),
                link_destination.clone().into_os_string(),
            ],
        )
        .await
            != allowed
        || link_destination.exists() != allowed
    {
        return false;
    }
    true
}

/// Run one probe command through the production path and keep the outcome.
///
/// The boolean is what the paired suites decide on; the record is what a
/// later refusal is explained with.
async fn probe(
    backend: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    suite: &ProbeSuite<'_>,
    argv: Vec<OsString>,
) -> bool {
    probe_result(backend, evidence, cancel, limits, suite, argv)
        .await
        .is_ok()
}

async fn probe_network_denied(
    backend: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    suite: &ProbeSuite<'_>,
    argv: Vec<OsString>,
) -> bool {
    matches!(probe_result(backend, evidence, cancel, limits, suite, argv).await,
        Err(reason) if reason == "exit 125" || reason == "exit 101")
}

async fn probe_result(
    backend: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    suite: &ProbeSuite<'_>,
    argv: Vec<OsString>,
) -> Result<(), String> {
    let attempted = argv
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let outcome = attempt(
        backend,
        evidence.store.clone(),
        cancel,
        limits,
        suite.cwd,
        argv,
    )
    .await;
    evidence.log.record(ProbeRecord {
        suite: suite.name.to_owned(),
        root: suite.root.to_path_buf(),
        argv: attempted,
        outcome: match &outcome {
            Ok(()) => "ok".to_owned(),
            Err(reason) => reason.clone(),
        },
    });
    outcome
}

/// How one probe command ended, in the words the transcript records.
///
/// A denied mutation is an ordinary non-zero exit, so the exit status is the
/// fact that distinguishes "the sandbox refused this" from "the command was
/// never reached".
fn probe_outcome(error: &ego_core::permit::ToolError) -> String {
    match error {
        ego_core::permit::ToolError::Process(ProcessError::Exit {
            code: Some(code), ..
        }) => format!("exit {code}"),
        ego_core::permit::ToolError::Process(ProcessError::Signal {
            signal: Some(signal),
            ..
        }) => format!("signal {signal}"),
        other => other.to_string(),
    }
}

async fn attempt(
    backend: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    cwd: &CanonPath,
    argv: Vec<OsString>,
) -> Result<(), String> {
    let Some(argv) = argv
        .into_iter()
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .ok()
    else {
        return Err("not attempted: probe argv is not UTF-8".to_owned());
    };
    let Ok(rule) = ego_core::permit::rule::Rule::load("exec(*)") else {
        return Err("not attempted: the probe rule did not parse".to_owned());
    };
    let policy = ego_core::permit::PolicyEngine::new(
        cwd.clone(),
        Mode::Default,
        Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![rule],
                deny: Vec::new(),
            },
            Source::Project,
        ),
    );
    let Ok(run_policy) = RunConfinement::new(Confinement::Off, false, cwd.clone()) else {
        return Err("not attempted: the probe workspace is not absolute".to_owned());
    };
    let ports = RunEffectPorts::for_probe(
        ExecRunConfig::new(
            run_policy,
            evidence.clone(),
            cancel,
            limits,
            HttpCallTimeout::default(),
            None,
            Arc::new(crate::http::SystemResolver::new()),
        ),
        backend,
    );
    let fs = crate::fs::OsFs;
    let registry: Vec<Arc<dyn Tool>> = vec![Arc::new(Typed(crate::tools::ExecTool))];
    let tools = Tools::new(
        &registry,
        ToolEffectPorts::new(
            &fs,
            ports.process(),
            &evidence,
            ports.resolver_handle(),
            ports.one_hop_handle(),
            ports.duplex_handle(),
        ),
        cwd.clone(),
    );
    let Ok(arguments) = serde_json::to_string(&serde_json::json!({
        "argv": argv,
        "cwd": cwd.to_string(),
        // Probe success is a capability observation, not a latency target.
        // A short deadline can turn scheduler contention into a false refusal.
        "timeout_ms": Duration::from_secs(30).as_millis() as u64,
    })) else {
        return Err("not attempted: probe arguments did not serialise".to_owned());
    };
    let Ok((tool, call)) = tools.prepare("exec", &arguments) else {
        return Err("not attempted: the probe call was not prepared".to_owned());
    };
    let Classification::Settled(Ok(authorized)) = tools.authorize(&policy, call) else {
        return Err("not attempted: the probe call was not authorized".to_owned());
    };
    tools
        .execute(&policy, tool.as_ref(), authorized, None)
        .await
        .map(|_| ())
        .map_err(|error| probe_outcome(&error))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ego_core::permit::{BackendIdentity, CanonPath, Confinement, ExecCaps, RunConfinement};

    use crate::exec::backend::{Backend, LaunchPrefix};

    use super::{
        CachedCapabilities, CapabilityCacheKey, Measurement, cache_key, cache_key_for_binary,
        cached_capabilities,
    };

    /// The envelope the probe emits is the envelope the dispatcher accepts, and
    /// it is spelled so libtest runs the `#[ignore]`d fixture named for the
    /// token. Every element is load-bearing: drop one and a libtest host either
    /// skips the fixture or swallows the arguments as a filter.
    #[test]
    fn the_probe_envelope_is_the_one_both_hosts_dispatch() {
        use std::ffi::OsString;

        let endpoint = "127.0.0.1:1".parse().expect("probe endpoint");
        let sentinel = "probe-sentinel";
        let argv = super::network_probe_argv("tcp", endpoint, sentinel);
        let executable = std::env::current_exe()
            .expect("probe test executable")
            .into_os_string();
        assert_eq!(
            argv,
            vec![
                executable,
                OsString::from("--ignored"),
                OsString::from("--exact"),
                OsString::from(super::NETWORK_PROBE_TOKEN),
                OsString::from("--nocapture"),
                OsString::from("--"),
                OsString::from("tcp"),
                OsString::from(endpoint.to_string()),
                OsString::from(sentinel),
            ]
        );
        assert!(super::dispatch_network_probe(argv.clone()).is_some());

        // Every guard word is required, one at a time. An argv that merely
        // contains the token is not the envelope, so an ordinary CLI argument
        // can never be mistaken for a probe.
        for dropped in 1..6 {
            let mut partial = argv.clone();
            partial.remove(dropped);
            assert!(
                super::dispatch_network_probe(partial).is_none(),
                "dropping argv[{dropped}] must stop being the probe envelope"
            );
        }
    }

    #[test]
    fn offline_rejects_a_wrapper_that_never_reaches_the_target() {
        // Catches: userns/setup failures are misreported as network isolation,
        // particularly when filesystem off supplies no positive read probes.
        struct FailedWrapper;
        impl Backend for FailedWrapper {
            fn identity(&self) -> BackendIdentity {
                BackendIdentity::new("failed-wrapper", "v1")
            }
            fn maximum_demonstrable_caps(&self) -> ExecCaps {
                ExecCaps::NO_IP_NETWORK
            }
            fn launch_prefix(&self) -> LaunchPrefix {
                LaunchPrefix {
                    program: Some("/bin/false".into()),
                    args_before_target: Vec::new(),
                }
            }
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let cwd = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
        let evidence = super::probe_evidence().expect("evidence");
        let denied = futures::executor::block_on(super::probe_offline(
            Arc::new(FailedWrapper),
            &cwd,
            evidence,
            ego_core::turn::CancelToken::new(),
            crate::exec::ExecLimits::default(),
        ));
        assert!(!denied, "a wrapper failure cannot prove offline");
    }

    struct CacheBackend {
        identity: BackendIdentity,
    }

    impl Backend for CacheBackend {
        fn identity(&self) -> BackendIdentity {
            self.identity.clone()
        }

        fn maximum_demonstrable_caps(&self) -> ExecCaps {
            ExecCaps::empty()
        }

        fn launch_prefix(&self) -> LaunchPrefix {
            LaunchPrefix {
                program: None,
                args_before_target: Vec::new(),
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_slow_allowed_probe_does_not_lose_its_capability_to_scheduler_delay() {
        use std::ffi::OsString;

        use ego_core::turn::CancelToken;

        let dir = tempfile::tempdir().expect("probe workspace");
        let cwd = CanonPath::new(std::fs::canonicalize(dir.path()).expect("canonical workspace"));
        let evidence = super::probe_evidence().expect("probe evidence");
        let backend: Arc<dyn Backend> = Arc::new(CacheBackend {
            identity: BackendIdentity::new("test-probe", "v1"),
        });

        let result = futures::executor::block_on(super::attempt(
            backend,
            evidence.store,
            CancelToken::new(),
            crate::exec::ExecLimits::default(),
            &cwd,
            vec![OsString::from("/bin/sleep"), OsString::from("3")],
        ));

        assert!(
            result.is_ok(),
            "a delayed successful probe is still allowed: {result:?}"
        );
    }

    fn policy() -> RunConfinement {
        RunConfinement::new(Confinement::Off, false, CanonPath::new("/workspace"))
            .expect("absolute workspace policy")
    }

    fn key(backend: &CacheBackend, runtime_roots: &[CanonPath]) -> CapabilityCacheKey {
        cache_key(&policy(), backend, runtime_roots).expect("cache key")
    }

    #[test]
    fn capability_cache_key_distinguishes_delimiter_bearing_backend_identity() {
        let first = CacheBackend {
            identity: BackendIdentity::new("backend|alpha", "revision"),
        };
        let second = CacheBackend {
            identity: BackendIdentity::new("backend", "alpha|revision"),
        };
        let roots = [CanonPath::new("/runtime/root")];

        assert_ne!(
            key(&first, &roots),
            key(&second, &roots),
            "cache keys must retain typed backend identity boundaries"
        );
    }

    #[test]
    fn capability_cache_key_distinguishes_typed_runtime_root_vectors() {
        let backend = CacheBackend {
            identity: BackendIdentity::new("backend", "revision"),
        };
        let first = [CanonPath::new("/runtime/a"), CanonPath::new("/b")];
        let second = [CanonPath::new("/runtime/a|/b")];

        assert_ne!(
            key(&backend, &first),
            key(&backend, &second),
            "cache keys must not flatten typed runtime roots into delimiter text"
        );
    }

    fn raw_key(
        build_identity: &[u8],
        backend_name: &str,
        backend_revision: &str,
        runtime_roots: &[&str],
    ) -> CapabilityCacheKey {
        CapabilityCacheKey {
            os: "test-os",
            arch: "test-arch",
            build_identity: build_identity.to_vec(),
            binary_hash: [0; 32],
            backend_name: backend_name.to_owned(),
            backend_revision: backend_revision.to_owned(),
            policy_digest: "0".repeat(64),
            runtime_roots: runtime_roots
                .iter()
                .map(|root| CanonPath::new(*root))
                .collect(),
        }
    }

    fn cache_test_key(label: &str) -> CapabilityCacheKey {
        let build_identity = format!("cache-test-build-{label}");
        let backend_name = format!("cache-test-backend-{label}");
        raw_key(
            build_identity.as_bytes(),
            &backend_name,
            "revision",
            &["/runtime/root"],
        )
    }

    #[test]
    fn capability_cache_invalidates_when_binary_bytes_change() {
        let dir = tempfile::tempdir().expect("binary fixture directory");
        let binary = dir.path().join("ego");
        let backend = CacheBackend {
            identity: BackendIdentity::new("backend", "revision"),
        };
        let roots = [CanonPath::new("/runtime/root")];
        std::fs::write(&binary, b"binary version one").unwrap();
        let first = cache_key_for_binary(&policy(), &backend, &roots, &binary).unwrap();
        let same = cache_key_for_binary(&policy(), &backend, &roots, &binary).unwrap();
        assert_eq!(first, same);
        std::fs::write(&binary, b"binary version two").unwrap();
        let changed = cache_key_for_binary(&policy(), &backend, &roots, &binary).unwrap();
        assert_ne!(first, changed, "an upgraded binary must re-probe");
    }

    #[test]
    fn the_once_read_process_image_hash_is_the_full_hash_of_the_running_binary() {
        let backend = CacheBackend {
            identity: BackendIdentity::new("backend", "revision"),
        };
        let roots = [CanonPath::new("/runtime/root")];
        let image = std::env::current_exe().unwrap();
        let once = cache_key(&policy(), &backend, &roots).unwrap();
        let again = cache_key(&policy(), &backend, &roots).unwrap();
        let full = cache_key_for_binary(&policy(), &backend, &roots, &image).unwrap();
        assert_eq!(once, again);
        assert_eq!(
            once, full,
            "the memoized image hash must be the file's hash"
        );
    }

    fn cached_fact(cell: &CachedCapabilities) -> Option<ExecCaps> {
        cell.get().map(|measurement| measurement.capabilities)
    }

    fn measured(capabilities: ExecCaps) -> Measurement {
        Measurement {
            capabilities,
            records: Arc::from([]),
        }
    }

    #[test]
    fn capability_cache_hashmap_identity_keeps_typed_fields_distinct() {
        let base = raw_key(
            b"raw-build|bytes",
            "backend",
            "revision",
            &["/runtime/a", "/b"],
        );
        let mut cache = HashMap::new();
        cache.insert(base, "base");

        let build_variant = raw_key(
            b"raw-build-bytes",
            "backend",
            "revision",
            &["/runtime/a", "/b"],
        );
        let delimiter_variant = raw_key(
            b"raw-build|bytes",
            "backend|name",
            "revision",
            &["/runtime/a", "/b"],
        );
        let roots_variant = raw_key(
            b"raw-build|bytes",
            "backend",
            "revision",
            &["/runtime/a|/b"],
        );

        assert!(!cache.contains_key(&build_variant));
        assert!(!cache.contains_key(&delimiter_variant));
        assert!(!cache.contains_key(&roots_variant));
    }

    #[tokio::test]
    async fn capability_cache_single_flights_concurrent_same_key_initialization() {
        let cell = cached_capabilities(cache_test_key("single-flight"));
        let initializers = Arc::new(AtomicUsize::new(0));
        let first_cell = Arc::clone(&cell);
        let first_initializers = Arc::clone(&initializers);
        let first = async move {
            first_cell
                .get_or_try_init(|| async move {
                    first_initializers.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok::<Measurement, &'static str>(measured(ExecCaps::READ_SCOPED))
                })
                .await
                .map(|measurement| measurement.capabilities)
        };
        let second_cell = Arc::clone(&cell);
        let second_initializers = Arc::clone(&initializers);
        let second = async move {
            second_cell
                .get_or_try_init(|| async move {
                    second_initializers.fetch_add(1, Ordering::SeqCst);
                    Ok::<Measurement, &'static str>(measured(ExecCaps::READ_SCOPED))
                })
                .await
                .map(|measurement| measurement.capabilities)
        };

        let (first, second) = tokio::join!(first, second);
        assert_eq!(
            first.expect("first caller initializes the fact"),
            ExecCaps::READ_SCOPED
        );
        assert_eq!(
            second.expect("second caller observes the fact"),
            ExecCaps::READ_SCOPED
        );
        assert_eq!(initializers.load(Ordering::SeqCst), 1);
        assert_eq!(cached_fact(cell.as_ref()), Some(ExecCaps::READ_SCOPED));
    }

    #[tokio::test]
    async fn capability_cache_retry_after_error_stores_only_measurement_facts() {
        let cell = cached_capabilities(cache_test_key("retry"));

        // The private cache value is exactly the measurement: the capabilities
        // and the transcript they came out of. Backends and scratch
        // directories are per-call inputs and cannot be retained here.
        let failed = cell
            .get_or_try_init(|| async {
                Err::<Measurement, &'static str>("transient probe failure")
            })
            .await;
        assert!(failed.is_err(), "the first measurement attempt failed");
        assert!(cached_fact(cell.as_ref()).is_none());

        let retried = cell
            .get_or_try_init(|| async {
                Ok::<Measurement, &'static str>(measured(ExecCaps::NO_IP_NETWORK))
            })
            .await
            .expect("a failed OnceCell initialization remains retryable");
        assert_eq!(retried.capabilities, ExecCaps::NO_IP_NETWORK);
        assert_eq!(cached_fact(cell.as_ref()), Some(ExecCaps::NO_IP_NETWORK));
    }
}
