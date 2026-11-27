//! Paired behavioural probes through the common process/evidence lifecycle.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::ffi::OsString;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt as _;
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
use nix::fcntl::{Flock, FlockArg, OFlag};
use sha2::{Digest as _, Sha256};

use crate::store::SessionEvidenceStore;

use super::backend::{Backend, MeasuredBackend};
use super::{ExecLimits, ExecRunConfig, HttpCallTimeout, ProbeLog, ProbeRecord, RunEffectPorts};

#[cfg(target_os = "macos")]
#[path = "adaptive_probe.rs"]
mod adaptive;

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
    validate_probe_scope(policy)?;
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
    validate_probe_scope(policy)?;
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
    validate_probe_scope(policy)?;
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
    // Sweep even on cache hits: another process may have died since the last
    // measurement. Only a code-marked fixture with no active owner is eligible.
    sweep_stale_fixtures(policy.workspace()).map_err(|error| {
        ProcessError::Backend(format!("cannot remove stale workspace probe: {error}"))
    })?;
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
                    nonloopback_addresses(),
                )
                .await
            {
                capabilities = capabilities.union(ExecCaps::NO_IP_NETWORK);
            }
            if cancel.is_cancelled() {
                return Err(ProcessError::Cancelled);
            }
            // A control that the selected filesystem policy must allow has
            // not measured absent authority when it cannot execute. Return
            // before OnceCell commits so a later caller can measure again.
            if let Some(failure) =
                evidence.log.snapshot().iter().find(|record| {
                    positive_filesystem_control(&record.suite) && record.outcome != "ok"
                })
            {
                return Err(ProcessError::Backend(format!(
                    "positive filesystem capability control {} failed: {}",
                    failure.suite, failure.outcome
                )));
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
    nonloopback: impl IntoIterator<Item = IpAddr>,
) -> bool {
    let network = OfflineProbe {
        backend,
        evidence,
        cancel,
        limits,
        cwd,
    };
    if !probe(
        network.backend.clone(),
        network.evidence.clone(),
        network.cancel.clone(),
        limits,
        &network.suite("offline.launch-control"),
        vec!["/usr/bin/true".into()],
    )
    .await
    {
        return false;
    }
    let addresses: Vec<_> = nonloopback.into_iter().collect();
    if addresses.is_empty() {
        return record_control_failure(
            &network.evidence,
            "offline.nonloopback.control".into(),
            cwd.as_path(),
            "no non-loopback route to bind a probe listener".into(),
        );
    }

    // Keep skipped addresses out of the selected address's evidence. If no
    // address pairs, keep the failures to explain the refusal.
    let failures = ProbeLog::default();
    let selected = select_nonloopback(addresses, |address, timeout| {
        let mut candidate = network.clone();
        candidate.evidence.log = Arc::new(ProbeLog::default());
        let selected_log = network.evidence.log.clone();
        let failures = &failures;
        async move {
            let controls = candidate.controls(address, NONLOOPBACK, timeout).await;
            let records = candidate.evidence.log.snapshot();
            if controls.is_some() {
                selected_log.extend(&records);
            } else {
                failures.extend(&records);
            }
            controls
        }
    })
    .await;
    let Some(nonloopback) = selected else {
        network.evidence.log.extend(&failures.snapshot());
        return false;
    };
    let Some(loopback) = network
        .controls(Ipv4Addr::LOCALHOST.into(), LOOPBACK, PROBE_TIMEOUT)
        .await
    else {
        return false;
    };
    network.isolated(loopback, LOOPBACK).await && network.isolated(nonloopback, NONLOOPBACK).await
}

#[derive(Clone)]
struct OfflineProbe<'a> {
    backend: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    cwd: &'a CanonPath,
}

#[derive(Clone, Copy)]
struct NetworkSuites {
    tcp: &'static str,
    udp: &'static str,
}

const LOOPBACK: NetworkSuites = NetworkSuites {
    tcp: "offline.tcp-loopback",
    udp: "offline.udp-loopback",
};
const NONLOOPBACK: NetworkSuites = NetworkSuites {
    tcp: "offline.tcp-nonloopback",
    udp: "offline.udp-nonloopback",
};

struct NetworkControls {
    tcp: tokio::net::TcpListener,
    udp: tokio::net::UdpSocket,
}

impl OfflineProbe<'_> {
    fn suite<'a>(&'a self, name: &'a str) -> ProbeSuite<'a> {
        ProbeSuite {
            name,
            cwd: self.cwd,
            root: self.cwd.as_path(),
        }
    }

    async fn controls(
        &self,
        address: IpAddr,
        suites: NetworkSuites,
        timeout: Duration,
    ) -> Option<NetworkControls> {
        if self.cancel.is_cancelled() {
            return None;
        }
        let tcp_suite = self.suite(suites.tcp);
        let udp_suite = self.suite(suites.udp);
        let tcp = std::net::TcpListener::bind((address, 0))
            .and_then(|socket| {
                socket.set_nonblocking(true)?;
                tokio::net::TcpListener::from_std(socket)
            })
            .map_err(|error| self.control_error(&tcp_suite, format!("bind failed: {error}")))
            .ok()?;
        let udp = std::net::UdpSocket::bind((address, 0))
            .and_then(|socket| {
                socket.set_nonblocking(true)?;
                tokio::net::UdpSocket::from_std(socket)
            })
            .map_err(|error| self.control_error(&udp_suite, format!("bind failed: {error}")))
            .ok()?;

        let (tcp_ok, udp_ok) = tokio::join!(
            self.tcp_control(&tcp, &tcp_suite, timeout),
            self.udp_control(&udp, &udp_suite, timeout),
        );
        (tcp_ok && udp_ok).then_some(NetworkControls { tcp, udp })
    }

    fn control_error(&self, suite: &ProbeSuite<'_>, reason: String) -> bool {
        record_control_failure(&self.evidence, suite.control_name(), suite.root, reason)
    }

    async fn isolated(&self, controls: NetworkControls, suites: NetworkSuites) -> bool {
        self.tcp_isolated(controls.tcp, &self.suite(suites.tcp))
            .await
            && self
                .udp_isolated(controls.udp, &self.suite(suites.udp))
                .await
    }
}

/// Records why the unconfined half of the offline probe could not run, so a
/// refusal and the transcript name the host cause instead of staying silent.
fn record_control_failure(
    evidence: &ProbeEvidence,
    suite: String,
    root: &std::path::Path,
    reason: String,
) -> bool {
    evidence.log.record(ProbeRecord {
        suite,
        root: root.to_path_buf(),
        argv: Vec::new(),
        outcome: reason,
    });
    false
}

fn nonloopback_addresses() -> Vec<IpAddr> {
    use nix::net::if_::InterfaceFlags;

    let Ok(interfaces) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    let mut ranked = interfaces
        .filter(|interface| {
            interface.flags.contains(InterfaceFlags::IFF_UP)
                && !interface.flags.contains(InterfaceFlags::IFF_LOOPBACK)
        })
        .filter_map(|interface| {
            // The offline probe uses IPv4 endpoints for both address classes.
            let address = interface.address?.as_sockaddr_in()?.ip();
            (!address.is_loopback() && !address.is_unspecified()).then_some((
                interface.flags.contains(InterfaceFlags::IFF_POINTOPOINT),
                IpAddr::from(address),
            ))
        })
        .collect::<Vec<_>>();
    // A point-to-point address may be visible locally but fail its round trip.
    ranked.sort_unstable();
    let mut seen = std::collections::HashSet::new();
    ranked
        .into_iter()
        .filter_map(|(_, address)| seen.insert(address).then_some(address))
        .collect()
}

async fn select_nonloopback<T, F>(
    addresses: Vec<IpAddr>,
    mut control: impl FnMut(IpAddr, Duration) -> F,
) -> Option<T>
where
    F: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let mut addresses = addresses.into_iter();
    while let Some(address) = addresses.next() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let timeout = remaining / u32::try_from(addresses.len() + 1).unwrap_or(u32::MAX);
        if timeout < Duration::from_millis(1) {
            break;
        }
        if let Some(ready) = control(address, timeout).await {
            return Some(ready);
        }
    }
    None
}

impl OfflineProbe<'_> {
    async fn tcp_control(
        &self,
        listener: &tokio::net::TcpListener,
        suite: &ProbeSuite<'_>,
        timeout: Duration,
    ) -> bool {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let endpoint = match listener.local_addr() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return self.control_error(suite, format!("socket address unavailable: {error}"));
            }
        };
        let sentinel = sentinel(endpoint);
        let receive = async {
            let (mut stream, _) = listener.accept().await?;
            let mut received = vec![0; sentinel.len()];
            stream.read_exact(&mut received).await?;
            if received != sentinel.as_bytes() {
                return Err(std::io::Error::other("sentinel mismatch"));
            }
            stream.write_all(&received).await
        };
        self.control_exchange("tcp", endpoint, &sentinel, suite, timeout, receive)
            .await
    }

    async fn udp_control(
        &self,
        socket: &tokio::net::UdpSocket,
        suite: &ProbeSuite<'_>,
        timeout: Duration,
    ) -> bool {
        let endpoint = match socket.local_addr() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return self.control_error(suite, format!("socket address unavailable: {error}"));
            }
        };
        let sentinel = sentinel(endpoint);
        let receive = async {
            let mut received = [0_u8; 128];
            let (len, sender) = socket.recv_from(&mut received).await?;
            if &received[..len] != sentinel.as_bytes() {
                return Err(std::io::Error::other("sentinel mismatch"));
            }
            let written = socket.send_to(&received[..len], sender).await?;
            if written != len {
                return Err(std::io::Error::other("incomplete sentinel acknowledgement"));
            }
            Ok(())
        };
        self.control_exchange("udp", endpoint, &sentinel, suite, timeout, receive)
            .await
    }

    async fn control_exchange(
        &self,
        protocol: &str,
        endpoint: SocketAddr,
        sentinel: &str,
        suite: &ProbeSuite<'_>,
        timeout: Duration,
        receive: impl Future<Output = std::io::Result<()>>,
    ) -> bool {
        let control = suite.control_name();
        let argv = network_probe_argv(protocol, endpoint, sentinel);
        let control_suite = suite.renamed(&control);
        let sending = probe_result_with_output(
            Arc::new(super::null::NullBackend),
            self.evidence.clone(),
            self.cancel.clone(),
            self.limits,
            &control_suite,
            argv.clone(),
            ProbeOutput::Control(timeout),
        );
        tokio::pin!(sending);
        let exchanged = tokio::select! {
            biased;
            received = tokio::time::timeout(timeout, receive) => {
                match (received, sending.await) {
                    (Ok(Ok(())), Ok(())) => return true,
                    (Ok(Ok(())), Err(error)) => format!("sender did not confirm acknowledgement: {error}"),
                    (Ok(Err(error)), _) => format!("sentinel exchange failed: {error}"),
                    (Err(_), _) => "sentinel exchange timed out".into(),
                }
            }
            result = &mut sending => format!("sender exited before acknowledgement: {result:?}"),
        };
        self.evidence.log.record(ProbeRecord {
            suite: format!("{control}.receiver"),
            root: suite.root.to_path_buf(),
            argv: argv
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
            outcome: exchanged,
        });
        false
    }

    async fn tcp_isolated(
        &self,
        listener: tokio::net::TcpListener,
        suite: &ProbeSuite<'_>,
    ) -> bool {
        let Ok(endpoint) = listener.local_addr() else {
            return false;
        };
        if !probe_network_denied(
            self.backend.clone(),
            self.evidence.clone(),
            self.cancel.clone(),
            self.limits,
            suite,
            network_probe_argv("tcp", endpoint, &sentinel(endpoint)),
        )
        .await
        {
            return false;
        }
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    }

    async fn udp_isolated(&self, socket: tokio::net::UdpSocket, suite: &ProbeSuite<'_>) -> bool {
        let Ok(endpoint) = socket.local_addr() else {
            return false;
        };
        let nonce = SessionId::new().to_string();
        #[cfg(target_os = "linux")]
        let isolation_input = match linux_network_namespace() {
            Ok(namespace) => format!("{namespace} {nonce}"),
            Err(error) => {
                return record_control_failure(
                    &self.evidence,
                    suite.name.into(),
                    suite.root,
                    format!("host network namespace unavailable: {error}"),
                );
            }
        };
        #[cfg(target_os = "macos")]
        let isolation_input = nonce.clone();
        // A quiet socket is inconclusive. Require the controlled child to
        // report positive evidence of isolation.
        let proof = udp_isolation_proof(endpoint, &nonce);
        let limits = ExecLimits {
            preview_bytes: self.limits.preview_bytes().max(1024),
            ..self.limits
        };
        if probe_result_with_output(
            self.backend.clone(),
            self.evidence.clone(),
            self.cancel.clone(),
            limits,
            suite,
            network_probe_argv("udp-isolation", endpoint, &isolation_input),
            ProbeOutput::RequiredStdoutLine(&proof),
        )
        .await
        .is_err()
        {
            return false;
        }
        // Check the kernel queue directly, without depending on reactor
        // readiness or waiting for a time window to prove non-delivery.
        match socket
            .into_std()
            .and_then(|socket| socket.recv_from(&mut [0; 128]))
        {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            result => record_control_failure(
                &self.evidence,
                suite.name.into(),
                suite.root,
                format!("UDP isolation contradicted by receiver: {result:?}"),
            ),
        }
    }
}

fn udp_isolation_proof(endpoint: SocketAddr, nonce: &str) -> String {
    format!("ego-udp-isolation-confirmed:{endpoint}:{nonce}")
}

#[cfg(target_os = "linux")]
fn linux_network_namespace() -> std::io::Result<String> {
    fs::read_link("/proc/self/ns/net")?
        .into_os_string()
        .into_string()
        .map_err(|_| std::io::Error::other("network namespace identity is not UTF-8"))
}

#[cfg(target_os = "linux")]
fn udp_isolation_proven(host_namespace: &str) -> bool {
    // Bubblewrap mounts a private procfs and drops all capabilities. A
    // distinct namespace with only loopback cannot reach any host endpoint.
    linux_network_namespace().is_ok_and(|namespace| namespace != host_namespace)
        && fs::read_to_string("/proc/self/status").is_ok_and(|status| {
            status
                .lines()
                .filter_map(|line| line.strip_prefix("CapEff:"))
                .any(|value| u64::from_str_radix(value.trim(), 16) == Ok(0))
        })
        && fs::read_to_string("/proc/net/dev").is_ok_and(|devices| {
            let mut interfaces = devices.lines().skip(2);
            interfaces.next().is_some_and(|line| {
                line.split_once(':')
                    .is_some_and(|(name, _)| name.trim() == "lo")
            }) && interfaces.next().is_none()
        })
}

#[cfg(target_os = "macos")]
fn udp_isolation_proven(endpoint: std::net::SocketAddr, sentinel: &str) -> bool {
    udp_probe_socket(endpoint)
        .and_then(|socket| socket.send_to(sentinel.as_bytes(), endpoint))
        .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
}

fn udp_probe_socket(endpoint: std::net::SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let bind = if endpoint.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    std::net::UdpSocket::bind(bind)
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
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Dispatch the fixed controlled network probe before the ordinary CLI starts.
///
/// `Some(true)` means the operation established its requested evidence.
/// `Some(false)` means the probe could not establish it, and `None` means
/// these arguments are outside the probe envelope.
/// Reporting the outcome rather than an exit code is what lets the libtest
/// host run the very same dispatcher and assert on it: choosing a process exit
/// status is the binary's job, not this library's.
pub fn dispatch_network_probe<I>(arguments: I) -> Option<bool>
where
    I: IntoIterator<Item = OsString>,
{
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;

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
        let input = arguments.next()?.into_string().ok()?;
        if arguments.next().is_some() || input.is_empty() {
            return None;
        }
        if protocol == "udp-isolation" {
            #[cfg(target_os = "linux")]
            let (host_namespace, nonce) = input.split_once(' ')?;
            #[cfg(target_os = "linux")]
            let isolated = udp_isolation_proven(host_namespace);
            #[cfg(target_os = "macos")]
            let (nonce, isolated) = (input.as_str(), udp_isolation_proven(endpoint, &input));
            if !isolated || nonce.parse::<SessionId>().is_err() {
                return None;
            }
            // The libtest host may have written its test name without a newline.
            println!("\n{}", udp_isolation_proof(endpoint, nonce));
            let sent = udp_probe_socket(endpoint)
                .and_then(|socket| socket.send_to(nonce.as_bytes(), endpoint));
            #[cfg(target_os = "macos")]
            if !sent.is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied) {
                return None;
            }
            #[cfg(target_os = "linux")]
            drop(sent);
            return Some(());
        }
        let sentinel = input.into_bytes();
        match protocol.as_str() {
            "tcp" => TcpStream::connect(endpoint)
                .and_then(|mut stream| {
                    stream.set_read_timeout(Some(PROBE_TIMEOUT))?;
                    stream.write_all(&sentinel)?;
                    let mut acknowledged = vec![0_u8; sentinel.len()];
                    stream.read_exact(&mut acknowledged)?;
                    if acknowledged != sentinel {
                        return Err(std::io::Error::other("sentinel acknowledgement mismatch"));
                    }
                    Ok(())
                })
                .ok(),
            "udp" => udp_probe_socket(endpoint)
                .and_then(|socket| {
                    let written = socket.send_to(&sentinel, endpoint)?;
                    if written != sentinel.len() {
                        return Err(std::io::Error::other("incomplete sentinel send"));
                    }
                    socket.set_read_timeout(Some(PROBE_TIMEOUT))?;
                    let mut acknowledged = vec![0_u8; sentinel.len() + 1];
                    let (len, peer) = socket.recv_from(&mut acknowledged)?;
                    if peer != endpoint || acknowledged[..len] != sentinel {
                        return Err(std::io::Error::other("sentinel acknowledgement mismatch"));
                    }
                    Ok(())
                })
                .ok(),
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
    #[cfg(target_os = "macos")]
    if maximum.contains(ExecCaps::ADAPTIVE) {
        return if adaptive::measure(backend, policy, scratch, evidence, cancel, limits).await {
            ExecCaps::ADAPTIVE
        } else {
            ExecCaps::empty()
        };
    }
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

const FIXTURE_OWNER: &str = ".ego-probe-owner";
fn fixture_marker(path: &Path) -> Option<String> {
    Some(format!(
        "ego-exec-probe-owner-v1\n{}\n",
        path.file_name()?.to_str()?
    ))
}

/// A released kernel lock proves the fixture has no active owner, without
/// guessing from a PID that may have been reused. Unmarked legacy directories
/// remain untouched: their prefix alone does not establish ownership.
fn sweep_stale_fixtures(workspace: &CanonPath) -> std::io::Result<()> {
    let entries = match fs::read_dir(workspace.as_path()) {
        Ok(entries) => entries,
        // A missing or non-directory workspace cannot contain stale fixtures.
        // Let measurement retain the actual setup cause in its refusal evidence.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".ego-exec-probe-")
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let path = entry.path();
        let Some(expected_marker) = fixture_marker(&path) else {
            continue;
        };
        let owner_path = path.join(FIXTURE_OWNER);
        // Do not follow marker symlinks or block opening an unrelated FIFO.
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
            .open(&owner_path)
        else {
            continue;
        };
        let Ok(mut owner) = Flock::lock(file, FlockArg::LockExclusiveNonblock) else {
            continue;
        };
        let Ok(metadata) = owner.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let mut marker = Vec::new();
        if (&mut *owner)
            .take((expected_marker.len() + 1) as u64)
            .read_to_end(&mut marker)
            .is_err()
            || marker != expected_marker.as_bytes()
        {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct ProbeFixture {
    // Fields drop in order: remove the directory before releasing its lock.
    _inside: tempfile::TempDir,
    _inside_owner: Flock<fs::File>,
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
        let owner_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(inside.path().join(FIXTURE_OWNER))
            .ok()?;
        let mut owner = Flock::lock(owner_file, FlockArg::LockExclusiveNonblock).ok()?;
        // Publish the complete marker only while the lock is held. A sweeper
        // ignores an incomplete marker if creation is interrupted here.
        owner
            .write_all(fixture_marker(inside.path())?.as_bytes())
            .ok()?;
        let outside = tempfile::tempdir().ok()?;
        let inside_path = fs::canonicalize(inside.path()).ok()?;
        let outside_path = fs::canonicalize(outside.path()).ok()?;
        Some(Self {
            _inside: inside,
            _inside_owner: owner,
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
    fs::write(&link_source, b"link").is_ok()
        && probe(
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
            == allowed
        && link_destination.exists() == allowed
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

fn positive_filesystem_control(suite: &str) -> bool {
    matches!(
        suite,
        "read.workspace" | "read.runtime-root" | "write-scoped.workspace" | "write-scoped.scratch"
    )
}

async fn probe_result(
    backend: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    suite: &ProbeSuite<'_>,
    argv: Vec<OsString>,
) -> Result<(), String> {
    let output = if positive_filesystem_control(suite.name) {
        ProbeOutput::FailureStderr
    } else {
        ProbeOutput::Status
    };
    probe_result_with_output(backend, evidence, cancel, limits, suite, argv, output).await
}

async fn probe_result_with_output(
    backend: Arc<dyn Backend>,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
    suite: &ProbeSuite<'_>,
    argv: Vec<OsString>,
    output: ProbeOutput<'_>,
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
        output,
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

#[derive(Clone, Copy)]
enum ProbeOutput<'a> {
    Status,
    Control(Duration),
    FailureStderr,
    RequiredStdoutLine(&'a str),
}

async fn attempt(
    backend: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    cwd: &CanonPath,
    argv: Vec<OsString>,
    output: ProbeOutput<'_>,
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
    let registry: Vec<Arc<dyn Tool>> = vec![Arc::new(Typed(crate::tools::ExecTool::default()))];
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
        "timeout_ms": match output {
            ProbeOutput::Control(timeout) => timeout,
            _ => PROBE_TIMEOUT,
        }
        .as_millis() as u64,
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
        .map_err(|error| {
            let mut outcome = probe_outcome(&error);
            if matches!(output, ProbeOutput::FailureStderr)
                && let ego_core::permit::ToolError::Process(
                    ProcessError::Exit { result, .. } | ProcessError::Signal { result, .. },
                ) = &error
                && let Some(block) = result.content.iter().find(|block| {
                    matches!(
                        block.provenance(),
                        ego_core::permit::ToolOutputProvenance::Process {
                            stream: ego_core::permit::StreamProvenance::Stderr,
                            ..
                        }
                    )
                })
                && !block.text.is_empty()
            {
                // Preserve a bounded preview before the disposable probe
                // evidence store disappears. At most 1024 Unicode scalars
                // (4096 UTF-8 bytes), independent of the child's output size.
                let stderr = block.text.chars().take(1024).collect::<String>();
                outcome.push_str("; stderr: ");
                outcome.push_str(&stderr);
            }
            outcome
        })
        .and_then(|execution| {
            if let ProbeOutput::RequiredStdoutLine(expected) = output
                && !matches!(
                    execution,
                    ego_core::mcp::ToolExecution::Complete(result)
                        if result.content.iter().any(|block| {
                            matches!(
                                block.provenance(),
                                ego_core::permit::ToolOutputProvenance::Process {
                                    stream: ego_core::permit::StreamProvenance::Stdout,
                                    ..
                                }
                            ) && block.text.lines().any(|line| line == expected)
                        })
                )
            {
                return Err("UDP isolation proof missing from child stdout".into());
            }
            Ok(())
        })
}

/// Negative production probes must stay outside every granted perimeter root.
fn validate_probe_scope(policy: &RunConfinement) -> Result<(), ProcessError> {
    if policy.level() == ego_core::permit::Confinement::Off {
        return Ok(());
    }
    if policy.level() == Confinement::Workspace
        && let Some(root) = policy.roots().iter().find(|root| {
            root.restricts_writes(policy.workspace(), policy.roots())
                && policy.workspace().is_or_under(root.path())
        })
    {
        return Err(ProcessError::Backend(format!(
            "workspace {} is covered by read-only root {}; workspace confinement cannot prove scoped writes. Inspect `ego roots ls` and the effective perimeter configuration; remove the conflicting read root or change its access to read-write with `ego roots add --access read-write <path>` if writes are intended, then start a new session. Use --sandbox ro if the workspace must stay read-only",
            policy.workspace(),
            root.path()
        )));
    }
    let tmp = fs::canonicalize(std::env::temp_dir())
        .map_err(|error| ProcessError::Backend(format!("cannot resolve probe TMPDIR: {error}")))?;
    for root in policy.roots() {
        if tmp.starts_with(root.path().as_path()) {
            return Err(ProcessError::Backend(format!(
                "perimeter-probe-temp-overlap: TMPDIR {} is inside granted root {}; the negative filesystem probe cannot be proven",
                tmp.display(),
                root.path()
            )));
        }
    }
    Ok(())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "probe_udp_tests.rs"]
mod udp_tests;

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

    // Catches: the first address's control failure prevents trying a usable address.
    #[tokio::test]
    async fn offline_tries_another_address_after_an_incomplete_control() {
        let skipped = "192.0.2.1".parse().expect("first address");
        let usable = "192.0.2.2".parse().expect("second address");
        let mut attempted = Vec::new();
        let selected = super::select_nonloopback(vec![skipped, usable], |address, _| {
            attempted.push(address);
            std::future::ready((address == usable).then_some(address))
        })
        .await;
        assert_eq!(selected, Some(usable));
        assert_eq!(attempted, [skipped, usable]);
    }

    // Catches: an absent datagram and a bare successful exit grant offline capability.
    #[tokio::test]
    async fn paired_udp_requires_evidence_of_confinement() {
        struct NoEvidence;

        impl Backend for NoEvidence {
            fn identity(&self) -> BackendIdentity {
                BackendIdentity::new("no-udp-evidence", "v1")
            }

            fn maximum_demonstrable_caps(&self) -> ExecCaps {
                ExecCaps::NO_IP_NETWORK
            }

            fn launch_prefix(&self) -> LaunchPrefix {
                LaunchPrefix {
                    program: Some("/usr/bin/true".into()),
                    args_before_target: Vec::new(),
                }
            }
        }

        let workspace = tempfile::tempdir().expect("workspace");
        let cwd = CanonPath::new(workspace.path().canonicalize().expect("absolute workspace"));
        let network = super::OfflineProbe {
            backend: Arc::new(NoEvidence),
            evidence: super::probe_evidence().expect("evidence"),
            cancel: ego_core::turn::CancelToken::new(),
            limits: crate::exec::ExecLimits::default(),
            cwd: &cwd,
        };
        let suite = network.suite("offline.udp-loopback");
        let socket = tokio::net::UdpSocket::bind(("127.0.0.1", 0))
            .await
            .expect("UDP receiver");
        assert!(
            network
                .udp_control(&socket, &suite, super::PROBE_TIMEOUT)
                .await,
            "positive UDP control did not complete",
        );
        assert!(
            !network.udp_isolated(socket, &suite).await,
            "bare exit supplied no UDP isolation evidence",
        );
    }

    // Catches: a read-only workspace reaches a write probe and reports an internal touch failure.
    #[test]
    fn workspace_read_root_reports_actionable_conflict() {
        use ego_core::permit::{ProfileRoot, RootAccess};

        let workspace = CanonPath::new("/parent/workspace");
        for frozen in ["/parent"] {
            let policy = RunConfinement::new(Confinement::Workspace, false, workspace.clone())
                .unwrap()
                .with_roots(vec![
                    ProfileRoot::new(CanonPath::new(frozen), RootAccess::Read),
                    ProfileRoot::new(CanonPath::new("/parent"), RootAccess::ReadWrite),
                ]);
            let error = super::validate_probe_scope(&policy).expect_err("read-only workspace");
            let message = error.to_string();
            for required in [
                "read-only root",
                frozen,
                "/workspace",
                "ego roots",
                "read-write",
                "--sandbox ro",
            ] {
                assert!(
                    message.contains(required),
                    "missing {required:?}: {message}"
                );
            }
            assert!(
                !message.contains("positive filesystem capability"),
                "{message}"
            );
        }
    }

    // Catches: the conflict check rejects a broad read grant, nested restriction or Ro run.
    #[test]
    fn workspace_read_root_conflict_preserves_nonconflicting_policies() {
        use ego_core::permit::{ProfileRoot, RootAccess};

        for (level, frozen) in [
            (Confinement::Workspace, "/parent/workspace"),
            (Confinement::Workspace, "/parent/workspace/frozen"),
            (Confinement::Workspace, "/parent"),
            (Confinement::Ro, "/parent/workspace"),
        ] {
            let policy = RunConfinement::new(level, false, CanonPath::new("/parent/workspace"))
                .unwrap()
                .with_roots(vec![ProfileRoot::new(
                    CanonPath::new(frozen),
                    RootAccess::Read,
                )]);
            super::validate_probe_scope(&policy).expect("compatible read root");
        }
    }

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
                    program: Some("/usr/bin/false".into()),
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
            super::nonloopback_addresses(),
        ));
        assert!(!denied, "a wrapper failure cannot prove offline");
    }

    // Catches: probe_offline returns false with no record when there is no
    // non-loopback route, so the refusal names no cause.
    #[test]
    fn offline_records_why_it_stops_without_a_non_loopback_route() {
        let workspace = tempfile::tempdir().expect("workspace");
        let cwd = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
        let evidence = super::probe_evidence().expect("evidence");
        let denied = futures::executor::block_on(super::probe_offline(
            Arc::new(super::super::null::NullBackend),
            &cwd,
            evidence.clone(),
            ego_core::turn::CancelToken::new(),
            crate::exec::ExecLimits::default(),
            None,
        ));
        assert!(!denied);
        assert_eq!(
            evidence.log.host_control_failure().as_deref(),
            Some("offline.nonloopback.control: no non-loopback route to bind a probe listener")
        );
    }

    // Catches: a failed bind returns false with no record, or names the wrong socket.
    #[tokio::test]
    async fn offline_records_which_bind_failed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let cwd = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
        let evidence = super::probe_evidence().expect("evidence");
        // 192.0.2.1 is TEST-NET-1: no local interface owns it, so binding fails.
        let unowned: std::net::IpAddr = "192.0.2.1".parse().unwrap();
        let denied = super::probe_offline(
            Arc::new(super::super::null::NullBackend),
            &cwd,
            evidence.clone(),
            ego_core::turn::CancelToken::new(),
            crate::exec::ExecLimits::default(),
            Some(unowned),
        )
        .await;
        assert!(!denied);
        let failure = evidence.log.host_control_failure().expect("a record");
        assert!(
            failure.starts_with("offline.tcp-nonloopback.control: bind failed: "),
            "{failure}"
        );
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
            super::ProbeOutput::Status,
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

    // Catches: a skipped address's failed control stays in the evidence, so a confined refusal is blamed on the host.
    #[tokio::test]
    async fn skipped_address_does_not_turn_a_confined_refusal_into_a_host_failure() {
        let workspace = tempfile::tempdir().expect("workspace");
        let cwd = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
        let evidence = super::probe_evidence().expect("evidence");
        // 192.0.2.1 is TEST-NET-1: no interface owns it, so its control cannot bind.
        let unowned: std::net::IpAddr = "192.0.2.1".parse().unwrap();
        let addresses = std::iter::once(unowned).chain(super::nonloopback_addresses());
        // A wrapper that never runs the target proves nothing, so admission must be refused.
        struct Wrapper;

        impl Backend for Wrapper {
            fn identity(&self) -> BackendIdentity {
                BackendIdentity::new("true-wrapper", "v1")
            }

            fn maximum_demonstrable_caps(&self) -> ExecCaps {
                ExecCaps::NO_IP_NETWORK
            }

            fn launch_prefix(&self) -> LaunchPrefix {
                LaunchPrefix {
                    program: Some("/usr/bin/true".into()),
                    args_before_target: Vec::new(),
                }
            }
        }

        let admitted = super::probe_offline(
            Arc::new(Wrapper),
            &cwd,
            evidence.clone(),
            ego_core::turn::CancelToken::new(),
            crate::exec::ExecLimits::default(),
            addresses,
        )
        .await;
        assert!(
            !admitted,
            "a wrapper that proves nothing was admitted as offline"
        );
        let records = evidence.log.snapshot();
        if !records.iter().any(|record| {
            record.suite == "offline.udp-nonloopback.control" && record.outcome == "ok"
        }) {
            eprintln!("note: no local address paired on this host; selection not exercised");
            return;
        }
        assert_eq!(
            evidence.log.host_control_failure(),
            None,
            "the skipped address's bind failure leaked into the selected evidence",
        );
    }

    // Catches: every candidate gets the whole shared budget, or an exhausted budget keeps looping.
    #[tokio::test]
    async fn address_selection_splits_one_budget_and_fails_closed() {
        let addresses: Vec<std::net::IpAddr> = (1..=3)
            .map(|last| format!("192.0.2.{last}").parse().unwrap())
            .collect();
        let mut slices = Vec::new();
        let selected = super::select_nonloopback(addresses, |_, timeout| {
            slices.push(timeout);
            std::future::ready(None::<()>)
        })
        .await;
        assert_eq!(selected, None, "no working address must select nothing");
        assert_eq!(slices.len(), 3, "every address is tried");
        assert!(
            slices[0] <= super::PROBE_TIMEOUT / 3,
            "first slice {:?}",
            slices[0]
        );
        assert!(slices.iter().all(|slice| *slice <= super::PROBE_TIMEOUT));
        // Exhausted budget: with too many addresses the slice drops below 1 ms and the loop stops.
        let many: Vec<std::net::IpAddr> = (0..60_000_u32)
            .map(|n| std::net::Ipv4Addr::from(0xC000_0200 + n).into())
            .collect();
        let mut tried = 0_u32;
        let none = super::select_nonloopback(many, |_, _| {
            tried += 1;
            std::future::ready(None::<()>)
        })
        .await;
        assert_eq!(none, None);
        assert!(tried < 60_000, "budget exhaustion did not stop the loop");
    }
}
