//! Paired behavioural probes through the common process/evidence lifecycle.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::event::id::SessionId;
use ego_core::permit::{
    CanonPath, Classification, ExecCaps, ProcessError, RunConfinement, Tool, Typed,
};
use ego_core::turn::{CancelToken, Tools};

use crate::store::SessionEvidenceStore;

use super::backend::{Backend, MeasuredBackend};
use super::{ExecLimits, GuardedExec};

#[cfg(target_os = "linux")]
use super::landlock::{LandlockBackend, derived_runtime_roots};
#[cfg(target_os = "macos")]
use super::seatbelt::{SeatbeltBackend, derived_runtime_roots};

#[cfg(target_os = "linux")]
pub(crate) async fn landlock(
    policy: &RunConfinement,
    cancel: CancelToken,
    limits: ExecLimits,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    if policy.offline() {
        let missing = policy
            .requirements()
            .check(ExecCaps::empty())
            .expect_err("offline always requires no-IP-network capability");
        return Err(ProcessError::MissingCapabilities(missing));
    }
    let scratch = tempfile::tempdir().map_err(|error| {
        ProcessError::Backend(format!("cannot create process scratch: {error}"))
    })?;
    fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o700)).map_err(|error| {
        ProcessError::Backend(format!("cannot secure process scratch: {error}"))
    })?;
    let scratch_path = CanonPath::new(fs::canonicalize(scratch.path()).map_err(|error| {
        ProcessError::Backend(format!("cannot resolve process scratch: {error}"))
    })?);
    let runtime_roots = derived_runtime_roots()?;
    let backend = Arc::new(LandlockBackend::prepare(
        policy,
        &scratch_path,
        &runtime_roots,
    )?);
    let maximum = backend.maximum_demonstrable_caps();
    let cache_key = cache_key(policy, backend.as_ref(), &runtime_roots)?;
    let cached = cached_capabilities(cache_key);
    let capabilities = *cached
        .get_or_try_init(|| async {
            let evidence_root = tempfile::tempdir().map_err(|error| {
                ProcessError::Backend(format!("cannot create probe evidence root: {error}"))
            })?;
            let evidence =
                SessionEvidenceStore::open(evidence_root.path(), SessionId::new(), 1024 * 1024)
                    .map_err(ProcessError::Evidence)?;
            let capabilities = measure(
                backend.clone(),
                policy,
                &scratch_path,
                &runtime_roots,
                evidence,
                cancel.clone(),
                limits,
            )
            .await;
            if cancel.is_cancelled() {
                return Err(ProcessError::Cancelled);
            }
            clear_scratch(scratch.path())?;
            Ok(capabilities)
        })
        .await?;
    let selected = Arc::new(MeasuredBackend::new(backend, capabilities, Some(scratch))?);
    policy
        .requirements()
        .check(selected.maximum_demonstrable_caps())
        .map_err(ProcessError::MissingCapabilities)?;
    Ok((selected, runtime_roots))
}

#[cfg(target_os = "macos")]
pub(crate) async fn seatbelt(
    policy: &RunConfinement,
    cancel: CancelToken,
    limits: ExecLimits,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    let scratch = private_scratch()?;
    let scratch_path = canonical_scratch(&scratch)?;
    let runtime_roots = derived_runtime_roots()?;
    let backend = Arc::new(SeatbeltBackend::prepare(
        policy,
        &scratch_path,
        &runtime_roots,
    )?);
    backend.prerequisite(policy)?;
    let maximum = backend.maximum_demonstrable_caps();
    let cache_key = cache_key(policy, backend.as_ref(), &runtime_roots)?;
    let cached = cached_capabilities(cache_key);
    let capabilities = *cached
        .get_or_try_init(|| async {
            let evidence_root = tempfile::tempdir().map_err(|error| {
                ProcessError::Backend(format!("cannot create probe evidence root: {error}"))
            })?;
            let evidence =
                SessionEvidenceStore::open(evidence_root.path(), SessionId::new(), 1024 * 1024)
                    .map_err(ProcessError::Evidence)?;
            let mut capabilities = measure(
                backend.clone(),
                policy,
                &scratch_path,
                &runtime_roots,
                evidence.clone(),
                cancel.clone(),
                limits,
            )
            .await;
            if maximum.contains(ExecCaps::NO_IP_NETWORK)
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
            clear_scratch(scratch.path())?;
            Ok(capabilities)
        })
        .await?;
    let selected = Arc::new(MeasuredBackend::new(backend, capabilities, Some(scratch))?);
    policy
        .requirements()
        .check(selected.maximum_demonstrable_caps())
        .map_err(ProcessError::MissingCapabilities)?;
    Ok((selected, runtime_roots))
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct CapabilityCacheKey {
    os: &'static str,
    arch: &'static str,
    build_identity: Vec<u8>,
    backend_name: String,
    backend_revision: String,
    policy_digest: String,
    runtime_roots: Vec<CanonPath>,
}

type CachedCapabilities = tokio::sync::OnceCell<ExecCaps>;

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

#[cfg(target_os = "macos")]
async fn probe_offline(
    backend: Arc<dyn Backend>,
    cwd: &CanonPath,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
) -> bool {
    use std::net::{TcpListener, UdpSocket};

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
        cwd,
    )
    .await
        && paired_udp(
            &udp_loopback,
            backend.clone(),
            baseline.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            cwd,
        )
        .await
        && paired_tcp(
            &tcp_nonloopback,
            backend.clone(),
            baseline.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            cwd,
        )
        .await
        && paired_udp(
            &udp_nonloopback,
            backend,
            baseline,
            evidence,
            cancel,
            limits,
            cwd,
        )
        .await
}

#[cfg(target_os = "macos")]
fn local_nonloopback() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback()).then_some(address)
}

#[cfg(target_os = "macos")]
async fn paired_tcp(
    listener: &std::net::TcpListener,
    backend: Arc<dyn Backend>,
    baseline: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    cwd: &CanonPath,
) -> bool {
    use std::io::Read as _;

    let Ok(endpoint) = listener.local_addr() else {
        return false;
    };
    if listener.set_nonblocking(true).is_err() {
        return false;
    }
    let sentinel = sentinel(endpoint);
    if !probe(
        baseline,
        evidence.clone(),
        cancel.clone(),
        limits,
        cwd,
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
            Err(_) => return false,
        }
    };
    if stream.set_nonblocking(false).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .is_err()
    {
        return false;
    }
    let mut received = vec![0_u8; sentinel.len()];
    if stream.read_exact(&mut received).is_err() || received != sentinel.as_bytes() {
        return false;
    }
    if probe(
        backend,
        evidence,
        cancel,
        limits,
        cwd,
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

#[cfg(target_os = "macos")]
async fn paired_udp(
    socket: &std::net::UdpSocket,
    backend: Arc<dyn Backend>,
    baseline: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    cwd: &CanonPath,
) -> bool {
    let Ok(endpoint) = socket.local_addr() else {
        return false;
    };
    let sentinel = sentinel(endpoint);
    if !probe(
        baseline,
        evidence.clone(),
        cancel.clone(),
        limits,
        cwd,
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
        return false;
    }
    let Ok((received_len, _)) = socket.recv_from(&mut received) else {
        return false;
    };
    if received[..received_len] != sentinel.as_bytes()[..] {
        return false;
    }
    if probe(
        backend,
        evidence,
        cancel,
        limits,
        cwd,
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

#[cfg(target_os = "macos")]
fn sentinel(endpoint: std::net::SocketAddr) -> String {
    format!("ego-network-probe:{}:{endpoint}", std::process::id())
}

#[cfg(target_os = "macos")]
fn network_probe_argv(
    protocol: &str,
    endpoint: std::net::SocketAddr,
    sentinel: &str,
) -> Vec<OsString> {
    vec![
        std::env::current_exe()
            .map(|path| path.into_os_string())
            .unwrap_or_default(),
        OsString::from(NETWORK_PROBE_TOKEN),
        OsString::from(protocol),
        OsString::from(endpoint.to_string()),
        OsString::from(sentinel),
    ]
}

#[cfg(target_os = "macos")]
const NETWORK_PROBE_TOKEN: &str = "__ego_internal_network_probe_v1";

/// Dispatch the fixed controlled network probe before the ordinary CLI starts.
#[cfg(target_os = "macos")]
pub fn dispatch_network_probe<I>(arguments: I) -> Option<std::process::ExitCode>
where
    I: IntoIterator<Item = OsString>,
{
    use std::io::Write as _;
    use std::net::{TcpStream, UdpSocket};

    let mut arguments = arguments.into_iter();
    let _executable = arguments.next()?;
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(NETWORK_PROBE_TOKEN)) {
        return None;
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
                    .and_then(|socket| socket.send_to(&sentinel, endpoint))
                    .ok()
                    .filter(|written| *written == sentinel.len())
                    .map(|_| ())
            }
            _ => None,
        }
    })();
    Some(if result.is_some() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(125)
    })
}

async fn measure(
    backend: Arc<dyn Backend>,
    policy: &RunConfinement,
    scratch: &CanonPath,
    runtime_roots: &[CanonPath],
    evidence: SessionEvidenceStore,
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
}

async fn probe_reads(
    backend: Arc<dyn Backend>,
    fixture: &ProbeFixture,
    evidence: SessionEvidenceStore,
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
        &fixture.cwd,
        vec!["/bin/cat".into(), inside.into_os_string()],
    )
    .await
        || probe(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &fixture.cwd,
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
            &fixture.cwd,
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
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
) -> bool {
    mutation_suite(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        &fixture.cwd,
        &fixture.inside,
        false,
    )
    .await
        && mutation_suite(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &fixture.cwd,
            &fixture.scratch,
            false,
        )
        .await
        && mutation_suite(
            backend,
            evidence,
            cancel,
            limits,
            &fixture.cwd,
            &fixture.outside,
            false,
        )
        .await
}

async fn probe_scoped_mutations(
    backend: Arc<dyn Backend>,
    fixture: &ProbeFixture,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
) -> bool {
    mutation_suite(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        &fixture.cwd,
        &fixture.inside,
        true,
    )
    .await
        && mutation_suite(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &fixture.cwd,
            &fixture.scratch,
            true,
        )
        .await
        && mutation_suite(
            backend,
            evidence,
            cancel,
            limits,
            &fixture.cwd,
            &fixture.outside,
            false,
        )
        .await
}

async fn mutation_suite(
    backend: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    cwd: &CanonPath,
    root: &std::path::Path,
    allowed: bool,
) -> bool {
    let create = root.join("create");
    if probe(
        backend.clone(),
        evidence.clone(),
        cancel.clone(),
        limits,
        cwd,
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
        cwd,
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
            cwd,
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
            cwd,
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
            cwd,
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
            cwd,
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

async fn probe(
    backend: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    cwd: &CanonPath,
    argv: Vec<OsString>,
) -> bool {
    let Some(argv) = argv
        .into_iter()
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .ok()
    else {
        return false;
    };
    let Ok(rule) = ego_core::permit::rule::Rule::load("exec(*)") else {
        return false;
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
    let executor = GuardedExec::provisional(backend, evidence.clone(), cancel, limits);
    let fs = crate::fs::OsFs;
    let registry: Vec<Arc<dyn Tool>> = vec![Arc::new(Typed(crate::tools::ExecTool))];
    let tools = Tools::new(&registry, &policy, &fs, &executor, &evidence, cwd.clone());
    let Ok(arguments) = serde_json::to_string(&serde_json::json!({
        "argv": argv,
        "cwd": cwd.to_string(),
        "timeout_ms": Duration::from_secs(2).as_millis() as u64,
    })) else {
        return false;
    };
    let Ok((tool, call)) = tools.prepare("exec", &arguments) else {
        return false;
    };
    let Classification::Settled(Ok(authorized)) = tools.authorize(call) else {
        return false;
    };
    tools.execute(tool, authorized).await.is_ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ego_core::permit::{BackendIdentity, CanonPath, Confinement, ExecCaps, RunConfinement};

    use crate::exec::backend::{Backend, LaunchPrefix};

    use super::{CachedCapabilities, CapabilityCacheKey, cache_key, cached_capabilities};

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

    fn cached_fact(cell: &CachedCapabilities) -> Option<ExecCaps> {
        cell.get().copied()
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
                    Ok::<ExecCaps, &'static str>(ExecCaps::READ_SCOPED)
                })
                .await
                .copied()
        };
        let second_cell = Arc::clone(&cell);
        let second_initializers = Arc::clone(&initializers);
        let second = async move {
            second_cell
                .get_or_try_init(|| async move {
                    second_initializers.fetch_add(1, Ordering::SeqCst);
                    Ok::<ExecCaps, &'static str>(ExecCaps::READ_SCOPED)
                })
                .await
                .copied()
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

        // The private cache value is exactly OnceCell<ExecCaps>; backends and
        // scratch directories are per-call inputs and cannot be retained here.
        let failed = cell
            .get_or_try_init(|| async { Err::<ExecCaps, &'static str>("transient probe failure") })
            .await;
        assert!(failed.is_err(), "the first measurement attempt failed");
        assert!(cached_fact(cell.as_ref()).is_none());

        let retried = cell
            .get_or_try_init(|| async { Ok::<ExecCaps, &'static str>(ExecCaps::NO_IP_NETWORK) })
            .await
            .expect("a failed OnceCell initialization remains retryable");
        assert_eq!(retried, &ExecCaps::NO_IP_NETWORK);
        assert_eq!(cached_fact(cell.as_ref()), Some(ExecCaps::NO_IP_NETWORK));
    }
}
