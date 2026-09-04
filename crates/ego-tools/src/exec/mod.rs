//! Exact argv-native execution behind one run-owned process boundary.

mod backend;
mod editor_process;
#[cfg(target_os = "linux")]
mod landlock;
mod null;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod probe;
pub(crate) mod process;
mod process_tree;
#[cfg(target_os = "macos")]
mod profile;
#[cfg(target_os = "macos")]
mod seatbelt;
mod secret_child;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ego_core::permit::{
    CanonPath, DuplexSpawner, ExecCaps, ExecObservation, OneHop, PolicyDigest, Resolver,
    RunConfinement, SpawnSpec,
};
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;
use sha2::{Digest as _, Sha256};

use backend::{Backend, MeasuredBackend};
use null::NullBackend;

pub use crate::store::SessionEvidenceStore;
pub use editor_process::{EditorProcessError, run_editor_process};
pub use ego_core::permit::{ProcessError, ProcessExecutor};
#[cfg(target_os = "linux")]
pub use landlock::dispatch_helper as dispatch_landlock_helper;
#[cfg(target_os = "macos")]
pub use probe::dispatch_network_probe;
pub use secret_child::{SecretChildError, SecretChildOutput, SecretChildRequest, run_secret_child};

pub(crate) fn runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("ego-effects")
                .build()
                .expect("the run effect runtime must be constructible")
        })
        .handle()
        .clone()
}

/// Per-run bounds that are not model-controlled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecLimits {
    preview_bytes: usize,
    termination_grace: Duration,
}

impl ExecLimits {
    pub fn new(preview_bytes: usize, termination_grace: Duration) -> Result<Self, ProcessError> {
        if preview_bytes == 0 {
            return Err(ProcessError::Invariant(
                "process preview budget must be greater than zero".into(),
            ));
        }
        if termination_grace.is_zero() {
            return Err(ProcessError::Invariant(
                "process termination grace must be greater than zero".into(),
            ));
        }
        Ok(Self {
            preview_bytes,
            termination_grace,
        })
    }

    pub const fn preview_bytes(self) -> usize {
        self.preview_bytes
    }

    pub const fn termination_grace(self) -> Duration {
        self.termination_grace
    }
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            preview_bytes: 64 * 1024,
            termination_grace: Duration::from_millis(500),
        }
    }
}

/// Complete immutable input for constructing one run's executor.
pub struct ExecRunConfig {
    policy: RunConfinement,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    exec_limits: ExecLimits,
    http_call_timeout: HttpCallTimeout,
    run_deadline: Option<Instant>,
    resolver: Arc<dyn Resolver>,
}

impl ExecRunConfig {
    pub fn new(
        policy: RunConfinement,
        evidence: SessionEvidenceStore,
        cancel: CancelToken,
        exec_limits: ExecLimits,
        http_call_timeout: HttpCallTimeout,
        run_deadline: Option<Instant>,
        resolver: Arc<dyn Resolver>,
    ) -> Self {
        Self {
            policy,
            evidence,
            cancel,
            exec_limits,
            http_call_timeout,
            run_deadline,
            resolver,
        }
    }
}

/// The maximum lifetime of one logical provider HTTP call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpCallTimeout(Duration);

impl HttpCallTimeout {
    pub fn new(timeout: Duration) -> Result<Self, HttpCallTimeoutError> {
        if timeout.is_zero() {
            return Err(HttpCallTimeoutError::Zero);
        }
        if Instant::now().checked_add(timeout).is_none() {
            return Err(HttpCallTimeoutError::Unrepresentable);
        }
        Ok(Self(timeout))
    }

    pub const fn duration(self) -> Duration {
        self.0
    }
}

impl Default for HttpCallTimeout {
    fn default() -> Self {
        Self(Duration::from_secs(30 * 60))
    }
}

/// Why a logical HTTP timeout cannot be represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpCallTimeoutError {
    Zero,
    Unrepresentable,
}

impl std::fmt::Display for HttpCallTimeoutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Zero => "HTTP call timeout must be greater than zero",
            Self::Unrepresentable => "HTTP call timeout is too large for the monotonic clock",
        })
    }
}

impl std::error::Error for HttpCallTimeoutError {}

impl From<HttpCallTimeoutError> for ProcessError {
    fn from(error: HttpCallTimeoutError) -> Self {
        Self::Invariant(error.to_string())
    }
}

/// The only public process executor. Policy, evidence ownership,
/// cancellation and lifecycle bounds are captured before any call can run.
pub struct GuardedExec {
    context: Arc<process::RunProcessContext>,
}

impl GuardedExec {
    fn new(context: Arc<process::RunProcessContext>) -> Self {
        Self { context }
    }
}

impl ProcessExecutor for GuardedExec {
    fn execute<'a>(
        &'a self,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ego_core::permit::ToolResult, ProcessError>> {
        Box::pin(self.context.execute(spec))
    }

    fn execute_handler<'a>(
        &'a self,
        spec: &'a SpawnSpec,
        input: &'a [u8],
        scope: ego_core::hook::EffectScope,
    ) -> BoxFuture<'a, Result<ego_core::permit::ToolResult, ProcessError>> {
        Box::pin(self.context.execute_handler(spec, input, scope))
    }
}

/// The complete external-effect adapter set owned by one run.
pub struct RunEffectPorts {
    process: Arc<GuardedExec>,
    http: Arc<crate::http::OneShotHttpTransport>,
    duplex: Arc<crate::stdio::GuardedDuplex>,
}

impl RunEffectPorts {
    pub async fn for_run(config: ExecRunConfig) -> Result<(Self, ExecObservation), ProcessError> {
        let (selected, runtime_roots) =
            select_backend(&config.policy, config.cancel.clone(), config.exec_limits).await?;
        selected.prerequisite(&config.policy)?;
        let capabilities = selected.capabilities();
        config
            .policy
            .requirements()
            .check(capabilities)
            .map_err(ProcessError::MissingCapabilities)?;
        let observation = ExecObservation::new(
            selected.identity(),
            capabilities,
            policy_digest_with_runtime_roots(&config.policy, selected.as_ref(), &runtime_roots)?,
        );
        let backend: Arc<dyn Backend> = selected;
        Ok((Self::build(config, backend, capabilities), observation))
    }

    fn build(config: ExecRunConfig, backend: Arc<dyn Backend>, capabilities: ExecCaps) -> Self {
        let context = Arc::new(process::RunProcessContext::new(
            backend,
            config.evidence,
            config.cancel.clone(),
            config.exec_limits,
            config.policy,
            capabilities,
        ));
        Self {
            process: Arc::new(GuardedExec::new(Arc::clone(&context))),
            http: Arc::new(crate::http::OneShotHttpTransport::new(
                config.resolver,
                config.cancel,
                config.http_call_timeout,
                config.run_deadline,
                context.offline(),
            )),
            duplex: Arc::new(crate::stdio::GuardedDuplex::new(context)),
        }
    }

    pub(crate) fn for_probe(config: ExecRunConfig, backend: Arc<dyn Backend>) -> Self {
        let capabilities = backend.maximum_demonstrable_caps();
        Self::build(config, backend, capabilities)
    }

    pub fn process(&self) -> &dyn ProcessExecutor {
        self.process.as_ref()
    }

    pub fn resolver(&self) -> &dyn Resolver {
        self.http.as_ref()
    }

    pub fn resolver_handle(&self) -> Arc<dyn Resolver> {
        Arc::clone(&self.http) as Arc<dyn Resolver>
    }

    pub fn one_hop(&self) -> &dyn OneHop {
        self.http.as_ref()
    }

    pub fn one_hop_handle(&self) -> Arc<dyn OneHop> {
        Arc::clone(&self.http) as Arc<dyn OneHop>
    }

    pub fn begin_http_call(&self) -> Instant {
        self.http.begin_call()
    }

    pub fn duplex(&self) -> &dyn DuplexSpawner {
        self.duplex.as_ref()
    }

    pub fn duplex_handle(&self) -> Arc<dyn DuplexSpawner> {
        Arc::clone(&self.duplex) as Arc<dyn DuplexSpawner>
    }
}

#[cfg(target_os = "linux")]
async fn select_backend(
    policy: &RunConfinement,
    cancel: CancelToken,
    limits: ExecLimits,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    if policy.level() == ego_core::permit::Confinement::Off {
        return MeasuredBackend::new(Arc::new(NullBackend), ExecCaps::empty(), None)
            .map(|backend| (Arc::new(backend), Vec::new()));
    }
    match probe::landlock(policy, cancel, limits).await {
        Ok(backend) => Ok(backend),
        Err(ProcessError::Cancelled) => Err(ProcessError::Cancelled),
        Err(_) => MeasuredBackend::new(Arc::new(NullBackend), ExecCaps::empty(), None)
            .map(|backend| (Arc::new(backend), Vec::new())),
    }
}

#[cfg(target_os = "macos")]
async fn select_backend(
    policy: &RunConfinement,
    cancel: CancelToken,
    limits: ExecLimits,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    if policy.level() != ego_core::permit::Confinement::Off || policy.offline() {
        return match probe::seatbelt(policy, cancel, limits).await {
            Ok(backend) => Ok(backend),
            Err(ProcessError::Cancelled) => Err(ProcessError::Cancelled),
            Err(_) => MeasuredBackend::new(Arc::new(NullBackend), ExecCaps::empty(), None)
                .map(|backend| (Arc::new(backend), Vec::new())),
        };
    }
    // Restricted platform backends plug into this private selection point.
    // The null backend is permanent and truthful: its empty capability set
    // admits only online `off`, and every stronger request fails above.
    MeasuredBackend::new(Arc::new(NullBackend), ExecCaps::empty(), None)
        .map(|backend| (Arc::new(backend), Vec::new()))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn select_backend(
    _policy: &RunConfinement,
    _cancel: CancelToken,
    _limits: ExecLimits,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    MeasuredBackend::new(Arc::new(NullBackend), ExecCaps::empty(), None)
        .map(|backend| (Arc::new(backend), Vec::new()))
}

#[cfg(test)]
fn policy_digest(
    policy: &RunConfinement,
    backend: &dyn Backend,
) -> Result<PolicyDigest, ProcessError> {
    policy_digest_with_runtime_roots(policy, backend, &[])
}

fn policy_digest_with_runtime_roots(
    policy: &RunConfinement,
    backend: &dyn Backend,
    runtime_roots: &[CanonPath],
) -> Result<PolicyDigest, ProcessError> {
    let mut hash = Sha256::new();
    update_digest_field(&mut hash, b"domain", b"ego.exec.policy-digest.v1")?;
    let level = policy.level().to_string();
    update_digest_field(&mut hash, b"level", level.as_bytes())?;
    update_digest_field(&mut hash, b"offline", &[u8::from(policy.offline())])?;
    update_path_field(
        &mut hash,
        b"workspace",
        policy.workspace().as_path().as_os_str(),
    )?;
    update_digest_field(
        &mut hash,
        b"runtime_roots.count",
        &u64::try_from(runtime_roots.len())
            .map_err(|_| ProcessError::Invariant("too many runtime roots to digest".into()))?
            .to_be_bytes(),
    )?;
    for root in runtime_roots {
        update_path_field(&mut hash, b"runtime_root", root.as_path().as_os_str())?;
    }
    let identity = backend.identity();
    update_digest_field(&mut hash, b"backend.name", identity.name().as_bytes())?;
    update_digest_field(
        &mut hash,
        b"backend.revision",
        identity.revision().as_bytes(),
    )?;
    let digest = hash.finalize();
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    PolicyDigest::try_from(text).map_err(|error| {
        ProcessError::Invariant(format!(
            "SHA-256 emitted an invalid lowercase policy digest: {error}"
        ))
    })
}

fn update_digest_field(hash: &mut Sha256, tag: &[u8], bytes: &[u8]) -> Result<(), ProcessError> {
    update_digest_field_prefix(hash, tag, bytes.len())?;
    hash.update(bytes);
    Ok(())
}

fn update_digest_field_prefix(
    hash: &mut Sha256,
    tag: &[u8],
    byte_len: usize,
) -> Result<(), ProcessError> {
    let byte_len = u64::try_from(byte_len).map_err(|_| {
        ProcessError::Invariant(format!(
            "policy digest field {tag:?} exceeds the u64 framing limit"
        ))
    })?;
    hash.update(tag);
    hash.update(byte_len.to_be_bytes());
    Ok(())
}

#[cfg(unix)]
fn update_path_field(
    hash: &mut Sha256,
    tag: &[u8],
    path: &std::ffi::OsStr,
) -> Result<(), ProcessError> {
    use std::os::unix::ffi::OsStrExt as _;
    update_digest_field(hash, tag, path.as_bytes())
}

#[cfg(windows)]
fn update_path_field(
    hash: &mut Sha256,
    tag: &[u8],
    path: &std::ffi::OsStr,
) -> Result<(), ProcessError> {
    use std::os::windows::ffi::OsStrExt as _;
    let byte_len = path
        .encode_wide()
        .count()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| {
            ProcessError::Invariant("policy digest workspace length overflowed usize".into())
        })?;
    update_digest_field_prefix(hash, tag, byte_len)?;
    for unit in path.encode_wide() {
        hash.update(unit.to_le_bytes());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn update_path_field(
    _hash: &mut Sha256,
    _tag: &[u8],
    _path: &std::ffi::OsStr,
) -> Result<(), ProcessError> {
    Err(ProcessError::Invariant(
        "policy digest has no lossless path encoding for this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use ego_core::permit::{BackendIdentity, CanonPath, Confinement, ExecCaps, RunConfinement};

    use super::backend::{Backend, LaunchPrefix};
    use super::{policy_digest, policy_digest_with_runtime_roots};

    struct IdentityBackend {
        identity: BackendIdentity,
    }

    impl Backend for IdentityBackend {
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

    fn policy(workspace: &str) -> RunConfinement {
        RunConfinement::new(Confinement::Workspace, false, CanonPath::new(workspace))
            .expect("absolute workspace policy")
    }

    #[test]
    fn policy_digest_frames_delimiter_bearing_backend_identity_fields() {
        let policy = policy("/workspace");
        let first = IdentityBackend {
            identity: BackendIdentity::new("backend|alpha", "revision"),
        };
        let second = IdentityBackend {
            identity: BackendIdentity::new("backend", "alpha|revision"),
        };

        let first_digest = policy_digest(&policy, &first).expect("first digest");
        let second_digest = policy_digest(&policy, &second).expect("second digest");

        assert_ne!(
            first_digest, second_digest,
            "typed backend identity fields must not collapse at delimiter boundaries"
        );
        assert_eq!(first_digest.as_str().len(), 64);
        assert!(
            first_digest
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }

    #[test]
    fn policy_digest_distinguishes_raw_concatenation_collision_records() {
        let first_policy = policy("/x/a");
        let second_policy = policy("/x/ab");
        let first = IdentityBackend {
            identity: BackendIdentity::new("bc", "d"),
        };
        let second = IdentityBackend {
            identity: BackendIdentity::new("c", "d"),
        };

        let first_digest = policy_digest(&first_policy, &first).expect("first digest");
        let second_digest = policy_digest(&second_policy, &second).expect("second digest");

        assert_ne!(
            first_digest, second_digest,
            "workspace and backend fields must not collapse under raw concatenation"
        );
    }

    #[test]
    fn policy_digest_frames_runtime_root_cardinality_and_order() {
        let backend = IdentityBackend {
            identity: BackendIdentity::new("backend", "revision"),
        };
        let policy = policy("/workspace");
        let empty = policy_digest_with_runtime_roots(&policy, &backend, &[])
            .expect("empty runtime-root vector");
        let one =
            policy_digest_with_runtime_roots(&policy, &backend, &[CanonPath::new("/runtime/a")])
                .expect("one runtime root");
        let ordered = policy_digest_with_runtime_roots(
            &policy,
            &backend,
            &[CanonPath::new("/runtime/a"), CanonPath::new("/runtime/b")],
        )
        .expect("ordered runtime roots");
        let reversed = policy_digest_with_runtime_roots(
            &policy,
            &backend,
            &[CanonPath::new("/runtime/b"), CanonPath::new("/runtime/a")],
        )
        .expect("reversed runtime roots");
        let flattened = policy_digest_with_runtime_roots(
            &policy,
            &backend,
            &[CanonPath::new("/runtime/a|/runtime/b")],
        )
        .expect("one delimiter-bearing runtime root");

        assert_ne!(empty, one, "empty and nonempty roots need distinct digests");
        assert_ne!(ordered, reversed, "runtime-root order is semantic");
        assert_ne!(ordered, flattened, "root-vector boundaries are semantic");
    }
}
