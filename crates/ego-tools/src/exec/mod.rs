//! Exact argv-native execution behind one run-owned process boundary.

mod adaptive;
mod backend;
#[cfg(target_os = "linux")]
mod bwrap;
mod denial;
mod editor_process;
mod identity;
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

use ego_core::event::id::DetachedExecId;
use ego_core::permit::{
    CanonPath, DuplexSpawner, ExecCaps, ExecObservation, OneHop, PolicyDigest, Resolver,
    RunConfinement, SandboxGrantPort, SpawnSpec,
};
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;
use sha2::{Digest as _, Sha256};

use backend::{Backend, MeasuredBackend};
use null::NullBackend;

pub use crate::store::SessionEvidenceStore;
pub use adaptive::admit_confinement;
pub use editor_process::{EditorProcessError, run_editor_process};
pub use ego_core::permit::{ProcessError, ProcessExecutor};
#[cfg(target_os = "linux")]
pub use landlock::dispatch_helper as dispatch_landlock_helper;
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
    admitted_observation: Option<ExecObservation>,
    sandbox_grants: Option<Arc<dyn SandboxGrantPort>>,
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
            admitted_observation: None,
            sandbox_grants: None,
        }
    }

    /// Let a human widen the sandbox of this run's commands by one path when
    /// a command is refused it. The admitted observation does not change:
    /// the grant is a delta the person made, not a different confinement.
    #[must_use]
    pub fn with_sandbox_grants(mut self, grants: Arc<dyn SandboxGrantPort>) -> Self {
        self.sandbox_grants = Some(grants);
        self
    }

    /// Require the reconstructed effects to match the session's admitted facts.
    #[must_use]
    pub fn with_admitted_observation(mut self, observation: ExecObservation) -> Self {
        self.admitted_observation = Some(observation);
        self
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

/// One probe command and how the child ended.
///
/// A cleared capability bit names the guarantee that is absent and nothing
/// else. Which paired operation failed, in which root, and with what exit
/// status is the difference between "this host has no sandbox" and "one rule
/// in the generated profile is wrong", so the measurement keeps it.
#[derive(Clone)]
pub(crate) struct ProbeRecord {
    pub(crate) suite: String,
    pub(crate) root: std::path::PathBuf,
    pub(crate) argv: Vec<String>,
    pub(crate) outcome: String,
}

/// The transcript of one measurement.
///
/// Capabilities are cached per effective policy, so the transcript is cached
/// with them: a later run in the same process must still be able to explain a
/// refusal it did not measure itself.
#[derive(Default)]
pub(crate) struct ProbeLog(std::sync::Mutex<Vec<ProbeRecord>>);

impl ProbeLog {
    fn entries(&self) -> std::sync::MutexGuard<'_, Vec<ProbeRecord>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(any(test, target_os = "linux", target_os = "macos"))]
    pub(crate) fn record(&self, record: ProbeRecord) {
        self.entries().push(record);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn snapshot(&self) -> Arc<[ProbeRecord]> {
        self.entries().as_slice().into()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn extend(&self, records: &[ProbeRecord]) {
        self.entries().extend_from_slice(records);
    }

    /// The first probe whose unconfined host control failed, as
    /// `<suite>: <outcome>`.
    ///
    /// A control is the same command run without the backend. When it fails,
    /// the host rather than the sandbox level decided the measurement.
    pub(crate) fn host_control_failure(&self) -> Option<String> {
        self.entries()
            .iter()
            .find(|record| {
                (record.suite == "adaptive.setup"
                    || ((record.suite.starts_with("offline.")
                        || record.suite.starts_with("adaptive."))
                        && (record.suite.ends_with(".control")
                            || record.suite.ends_with(".control.receiver"))))
                    && record.outcome != "ok"
            })
            .map(|record| format!("{}: {}", record.suite, record.outcome))
    }

    /// The transcript as one JSON Lines blob, or `None` when nothing ran.
    fn transcript(&self) -> Option<Vec<u8>> {
        let entries = self.entries();
        if entries.is_empty() {
            return None;
        }
        let mut bytes = Vec::new();
        for record in entries.iter() {
            let line = serde_json::json!({
                "suite": record.suite,
                "root": record.root.to_string_lossy(),
                "argv": record.argv,
                "outcome": record.outcome,
            });
            bytes.extend_from_slice(line.to_string().as_bytes());
            bytes.push(b'\n');
        }
        Some(bytes)
    }
}

/// Store the measurement transcript as ordinary session evidence.
///
/// It is content-addressed like any other tool evidence, so it is readable
/// only inside the session that measured it, and a run that measured nothing
/// stores nothing.
fn commit_probe_transcript(
    evidence: &SessionEvidenceStore,
    log: &ProbeLog,
) -> Result<Option<ego_core::store::evidence::EvidenceRef>, ProcessError> {
    use ego_core::store::evidence::EvidenceStore as _;

    let Some(transcript) = log.transcript() else {
        return Ok(None);
    };
    let mut writer = evidence.begin().map_err(ProcessError::Evidence)?;
    if let Err(error) = writer.append(&transcript) {
        let _ = writer.abort();
        return Err(ProcessError::Evidence(error));
    }
    writer.commit().map(Some).map_err(ProcessError::Evidence)
}

/// The only public process executor. Policy, evidence ownership,
/// cancellation and lifecycle bounds are captured before any call can run.
pub struct GuardedExec {
    context: Arc<process::RunProcessContext>,
    session_owner: Option<Arc<GuardedExec>>,
}

impl GuardedExec {
    fn session_context(&self) -> &Arc<process::RunProcessContext> {
        self.session_owner
            .as_ref()
            .map_or(&self.context, |owner| owner.session_context())
    }

    fn new(context: Arc<process::RunProcessContext>) -> Self {
        Self {
            context,
            session_owner: None,
        }
    }
}

impl Drop for GuardedExec {
    fn drop(&mut self) {
        if self.session_owner.is_some() {
            return;
        }
        self.context.cancel_all_background();
        self.context.wait_background_shutdown();
    }
}

impl ProcessExecutor for GuardedExec {
    fn confinement(&self) -> Option<ego_core::permit::Confinement> {
        Some(self.context.confinement())
    }

    fn observe(&self, spec: &SpawnSpec) -> Result<SpawnSpec, ProcessError> {
        identity::capture(spec)
    }

    fn revalidate(&self, spec: &SpawnSpec) -> Result<(), ProcessError> {
        identity::revalidate(spec)
    }
    fn execute<'a>(
        &'a self,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ego_core::permit::ToolResult, ProcessError>> {
        Box::pin(async move {
            if let Some(owner) = &self.session_owner {
                self.context
                    .execute_promotable(owner.session_context(), spec)
                    .await
            } else {
                self.context.execute(spec).await
            }
        })
    }

    fn foreground_exec(&self) -> Option<DetachedExecId> {
        self.context.foreground_exec()
    }

    fn detach_foreground(&self, id: DetachedExecId) -> Result<(), ProcessError> {
        self.session_context().detach_foreground(id)
    }

    fn start_background<'a>(
        &'a self,
        id: DetachedExecId,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<(), ProcessError>> {
        Box::pin(async move { self.session_context().start_background(id, spec) })
    }

    fn background_status(
        &self,
        id: DetachedExecId,
    ) -> Result<ego_core::permit::DetachedExecSnapshot, ProcessError> {
        self.session_context().background_status(id)
    }

    fn cancel_background(&self, id: DetachedExecId) -> Result<(), ProcessError> {
        self.session_context().cancel_background(id)
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

/// Raw backend measurement before any session evidence or authority exists.
struct RawExecMeasurement {
    backend: Arc<MeasuredBackend>,
    runtime_roots: Vec<CanonPath>,
    probes: ProbeLog,
}

/// One reconstructed backend and the recorded facts it produced.
///
/// Private because the backend and its scratch are run mechanisms, while only
/// the observation and evidence reference cross the session admission port.
struct ExecMeasurement {
    backend: Arc<MeasuredBackend>,
    observation: ExecObservation,
    evidence: Option<ego_core::store::evidence::EvidenceRef>,
    host_control_failure: Option<String>,
}

impl RunEffectPorts {
    async fn measure_raw(
        policy: &RunConfinement,
        cancel: CancelToken,
        exec_limits: ExecLimits,
    ) -> Result<RawExecMeasurement, ProcessError> {
        let probes = ProbeLog::default();
        let (backend, runtime_roots) = select_backend(policy, cancel, exec_limits, &probes).await?;
        backend.prerequisite(policy)?;
        Ok(RawExecMeasurement {
            backend,
            runtime_roots,
            probes,
        })
    }

    async fn measure(config: &ExecRunConfig) -> Result<ExecMeasurement, ProcessError> {
        let measured =
            Self::measure_raw(&config.policy, config.cancel.clone(), config.exec_limits).await?;
        let capabilities = measured.backend.capabilities();
        // The transcript lands in the session before the refusal that cites
        // it, so an operator can read what was measured rather than infer it
        // from a capability that is merely absent.
        let transcript = commit_probe_transcript(&config.evidence, &measured.probes)?;
        let observation = ExecObservation::new(
            measured.backend.identity(),
            capabilities,
            policy_digest_with_runtime_roots(
                &config.policy,
                measured.backend.as_ref(),
                &measured.runtime_roots,
            )?,
        )
        .with_policy(ego_core::permit::ExecutionPolicy::capture(
            &config.policy,
            measured.runtime_roots,
        ));
        Ok(ExecMeasurement {
            backend: measured.backend,
            observation,
            evidence: transcript,
            host_control_failure: measured.probes.host_control_failure(),
        })
    }

    /// Measure process capabilities without creating session authority.
    pub async fn diagnostic_capabilities(
        policy: &RunConfinement,
        cancel: CancelToken,
        exec_limits: ExecLimits,
    ) -> Result<ExecCaps, ProcessError> {
        let measured = Self::measure_raw(policy, cancel, exec_limits).await?;
        Ok(measured.backend.capabilities())
    }

    /// Measure one policy and commit its transcript without constructing effects.
    ///
    /// Capability requirements are deliberately not applied here: a session
    /// offer needs the completed observation in order to explain why a real
    /// confinement is unsupported.
    pub async fn observe(
        config: &ExecRunConfig,
    ) -> Result<
        (
            ExecObservation,
            Option<ego_core::store::evidence::EvidenceRef>,
        ),
        ProcessError,
    > {
        let measured = Self::measure(config).await?;
        Ok((measured.observation, measured.evidence))
    }

    pub async fn for_run(
        config: ExecRunConfig,
    ) -> Result<
        (
            Self,
            ExecObservation,
            Option<ego_core::store::evidence::EvidenceRef>,
        ),
        ProcessError,
    > {
        let measured = Self::measure(&config).await?;
        if let Some(admitted) = config.admitted_observation.as_ref()
            && !admitted.same_measurement(&measured.observation)
        {
            return Err(ProcessError::ConfinementChanged {
                admitted: Box::new(admitted.clone()),
                observed: Box::new(measured.observation),
            });
        }
        let capabilities = measured.observation.capabilities();
        config
            .policy
            .requirements()
            .check(capabilities)
            .map_err(|missing| {
                ProcessError::MissingCapabilities(
                    missing
                        .with_probe_evidence(measured.evidence.clone())
                        .with_host_control_failure(measured.host_control_failure.clone())
                        .from_backend(measured.observation.backend().clone()),
                )
            })?;
        let backend: Arc<dyn Backend> = measured.backend;
        Ok((
            Self::build(config, backend, capabilities),
            measured.observation,
            measured.evidence,
        ))
    }

    fn build(config: ExecRunConfig, backend: Arc<dyn Backend>, capabilities: ExecCaps) -> Self {
        let confinement = config.policy.clone();
        let context = Arc::new(process::RunProcessContext::new(
            backend,
            config.evidence,
            config.cancel.clone(),
            config.exec_limits,
            config.policy,
            capabilities,
            config.sandbox_grants,
        ));
        Self {
            process: Arc::new(GuardedExec::new(Arc::clone(&context))),
            http: Arc::new(crate::http::OneShotHttpTransport::new(
                config.resolver,
                config.cancel,
                config.http_call_timeout,
                config.run_deadline,
                confinement,
            )),
            duplex: Arc::new(crate::stdio::GuardedDuplex::new(context)),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn for_probe(config: ExecRunConfig, backend: Arc<dyn Backend>) -> Self {
        let capabilities = backend.maximum_demonstrable_caps();
        Self::build(config, backend, capabilities)
    }

    /// Keep detached effects with the session while foreground effects retain
    /// this turn's cancellation and transport bounds.
    pub fn with_session_processes(&self, session: &Self) -> Self {
        Self {
            process: Arc::new(GuardedExec {
                context: Arc::clone(&self.process.context),
                session_owner: Some(Arc::clone(&session.process)),
            }),
            http: Arc::clone(&self.http),
            duplex: Arc::clone(&self.duplex),
        }
    }

    pub fn process_handle(&self) -> Arc<dyn ProcessExecutor> {
        Arc::clone(&self.process) as Arc<dyn ProcessExecutor>
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
    log: &ProbeLog,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    if policy.level() == ego_core::permit::Confinement::Off && !policy.offline() {
        return MeasuredBackend::new(Arc::new(NullBackend), ExecCaps::empty(), None)
            .map(|backend| (Arc::new(backend), Vec::new()));
    }
    match probe::bwrap(policy, cancel.clone(), limits, log).await {
        Ok(selected)
            if selected
                .0
                .capabilities()
                .contains(policy.requirements().required()) =>
        {
            return Ok(selected);
        }
        Err(ProcessError::Cancelled) => return Err(ProcessError::Cancelled),
        Err(error) => log.record(ProbeRecord {
            suite: "bwrap.prerequisite".into(),
            root: policy.workspace().as_path().to_path_buf(),
            argv: vec!["/usr/bin/bwrap".into()],
            outcome: error.to_string(),
        }),
        Ok(_) => {}
    }
    if policy.level() == ego_core::permit::Confinement::Off {
        return MeasuredBackend::new(Arc::new(NullBackend), ExecCaps::empty(), None)
            .map(|backend| (Arc::new(backend), Vec::new()));
    }
    probe::landlock(policy, cancel, limits, log).await
}

#[cfg(target_os = "macos")]
async fn select_backend(
    policy: &RunConfinement,
    cancel: CancelToken,
    limits: ExecLimits,
    log: &ProbeLog,
) -> Result<(Arc<MeasuredBackend>, Vec<CanonPath>), ProcessError> {
    if policy.level() != ego_core::permit::Confinement::Off || policy.offline() {
        return probe::seatbelt(policy, cancel, limits, log).await;
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
    _log: &ProbeLog,
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
    // An off sandbox does not enforce perimeter roots. ACP measures before
    // profile/default roots are materialized; those inactive fields must not
    // invalidate its observation or a durable unconfined run selection.
    if policy.level() != ego_core::permit::Confinement::Off {
        if let Some(adaptive) = policy.adaptive_perimeter() {
            update_path_field(
                &mut hash,
                b"adaptive.home",
                adaptive.home.as_path().as_os_str(),
            )?;
            for (tag, paths) in [
                (b"adaptive.write".as_slice(), &adaptive.writable),
                (b"adaptive.credential".as_slice(), &adaptive.credentials),
                (b"adaptive.control".as_slice(), &adaptive.controls),
            ] {
                update_digest_field(&mut hash, tag, &(paths.len() as u64).to_be_bytes())?;
                for path in paths {
                    update_path_field(&mut hash, tag, path.as_path().as_os_str())?;
                }
            }
        }
        update_digest_field(
            &mut hash,
            b"roots.count",
            &(policy.roots().len() as u64).to_be_bytes(),
        )?;
        for root in policy.roots() {
            update_path_field(&mut hash, b"root.path", root.path().as_path().as_os_str())?;
            update_digest_field(
                &mut hash,
                b"root.access",
                &[u8::from(
                    root.access() == ego_core::permit::RootAccess::ReadWrite,
                )],
            )?;
        }
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

    const TEST_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/exec-policy");

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

    // Catches: a saved unconfined run becomes unusable after perimeter fields are added.
    #[cfg(unix)]
    #[test]
    fn unconfined_policy_digest_retains_semantic_identity_across_root_materialization() {
        use ego_core::permit::{ProfileRoot, RootAccess};
        let backend = super::NullBackend;
        let initial = RunConfinement::new(
            Confinement::Off,
            false,
            CanonPath::new("/home/example/workspace"),
        )
        .expect("absolute workspace");
        let rooted = initial.clone().with_roots(vec![ProfileRoot::new(
            CanonPath::new("/home/example/workspace"),
            RootAccess::ReadWrite,
        )]);
        let admitted = policy_digest(&initial, &backend).expect("initial digest");
        assert_eq!(
            admitted.as_str(),
            "cfcf897964188137affec341edbfa33e68d65592e2183aae868bec7f0250a0be"
        );
        assert_eq!(
            admitted,
            policy_digest(&rooted, &backend).expect("rooted digest")
        );
        let offline = RunConfinement::new(Confinement::Off, true, initial.workspace().clone())
            .expect("offline policy");
        assert_ne!(
            admitted,
            policy_digest(&offline, &backend).expect("offline digest")
        );
    }

    // Catches: excluding inactive roots accidentally stops binding confined path/access changes.
    #[test]
    fn confined_policy_digest_binds_root_path_and_access() {
        use ego_core::permit::{ProfileRoot, RootAccess};
        let backend = IdentityBackend {
            identity: BackendIdentity::new("backend", "revision"),
        };
        for level in [Confinement::Ro, Confinement::Workspace] {
            let initial = RunConfinement::new(level, false, CanonPath::new(TEST_ROOT))
                .expect("confined policy");
            let digest = |path, access| {
                policy_digest(
                    &initial
                        .clone()
                        .with_roots(vec![ProfileRoot::new(CanonPath::new(path), access)]),
                    &backend,
                )
                .expect("rooted digest")
            };
            let read = digest("/perimeter/a", RootAccess::Read);
            assert_ne!(
                read,
                policy_digest(&initial, &backend).expect("empty roots digest")
            );
            assert_ne!(read, digest("/perimeter/b", RootAccess::Read));
            assert_ne!(read, digest("/perimeter/a", RootAccess::ReadWrite));
        }
    }

    #[test]
    fn policy_digest_frames_delimiter_bearing_backend_identity_fields() {
        let policy = policy(TEST_ROOT);
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
        let first_policy = policy(concat!(env!("CARGO_MANIFEST_DIR"), "/target/exec-policy-a"));
        let second_policy = policy(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/exec-policy-ab"
        ));
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
        let policy = policy(TEST_ROOT);
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

    fn transcript(rows: &[(&str, &str)]) -> super::ProbeLog {
        let log = super::ProbeLog::default();
        for (suite, outcome) in rows {
            log.record(super::ProbeRecord {
                suite: (*suite).to_owned(),
                root: TEST_ROOT.into(),
                argv: vec!["ego".to_owned()],
                outcome: (*outcome).to_owned(),
            });
        }
        log
    }

    // Catches: a refusal blames the sandbox level when the unconfined host control failed.
    #[test]
    fn a_failed_offline_host_control_is_named_from_the_recorded_transcript() {
        let log = transcript(&[
            ("offline.tcp-loopback.control", "ok"),
            ("offline.tcp-loopback", "exit 101"),
            ("offline.tcp-nonloopback.control", "ok"),
            (
                "offline.tcp-nonloopback.control.receiver",
                "accept failed: Resource temporarily unavailable (os error 35)",
            ),
        ]);

        assert_eq!(
            log.host_control_failure().as_deref(),
            Some(
                "offline.tcp-nonloopback.control.receiver: accept failed: \
                 Resource temporarily unavailable (os error 35)"
            )
        );
    }

    // Catches: a control child that exits non-zero is mistaken for a sandbox failure.
    #[test]
    fn a_failed_offline_control_child_is_a_host_control_failure() {
        let log = transcript(&[("offline.udp-nonloopback.control", "exit 1")]);

        assert_eq!(
            log.host_control_failure().as_deref(),
            Some("offline.udp-nonloopback.control: exit 1")
        );
    }

    // Catches: a failed confined probe is reported as a host control failure.
    #[test]
    fn a_confined_offline_failure_is_not_a_host_control_failure() {
        let log = transcript(&[
            ("offline.tcp-loopback.control", "ok"),
            ("offline.tcp-loopback", "exit 1"),
        ]);

        assert_eq!(log.host_control_failure(), None);
    }

    // Catches: a filesystem control failure is worded as an offline host failure.
    #[test]
    fn a_failed_filesystem_control_is_not_an_offline_host_control_failure() {
        let log = transcript(&[("read.control", "exit 1")]);

        assert_eq!(log.host_control_failure(), None);
    }
    // Catches: Adaptive host failures are omitted or confined failures are mislabelled.
    #[test]
    fn adaptive_setup_and_control_failures_reach_host_diagnostics() {
        for suite in ["adaptive.setup", "adaptive.write.outside.control"] {
            let log = transcript(&[(suite, "fixture failed: os error 1")]);
            assert_eq!(
                log.host_control_failure(),
                Some(format!("{suite}: fixture failed: os error 1"))
            );
        }
        let log = transcript(&[
            ("adaptive.read.credential-floor", "exit 1"),
            ("adaptive.setup", "ok"),
        ]);
        assert_eq!(log.host_control_failure(), None);
    }
}
