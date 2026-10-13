//! Private execution-backend seam.

use std::ffi::{OsStr, OsString};
use std::sync::Arc;

use ego_core::permit::{
    BackendIdentity, CanonPath, EnvName, EnvPolicy, ExecCaps, ProcessError, RunConfinement,
    SandboxGrant, SpawnSpec,
};
use tokio::process::Command;

/// Fixed backend wrapper placed before the exact target argv.
pub(crate) struct LaunchPrefix {
    pub(crate) program: Option<OsString>,
    pub(crate) args_before_target: Vec<OsString>,
}

/// Every backend reports its mechanism's eligible upper bound and supplies
/// only fixed launch data. Verified bits live in [`MeasuredBackend::capabilities`].
pub(crate) trait Backend: Send + Sync {
    fn identity(&self) -> BackendIdentity;
    fn prerequisite(&self, _policy: &RunConfinement) -> Result<(), ProcessError> {
        Ok(())
    }
    fn maximum_demonstrable_caps(&self) -> ExecCaps;
    fn launch_prefix(&self) -> LaunchPrefix;

    /// This backend with allowed paths added and exec denied under
    /// `no_exec`, or `None` when the mechanism cannot express a per-path grant
    /// after admission. The result keeps the identity and verified
    /// capabilities of the backend it came from; it only adds reach a human
    /// or the judge named.
    fn widened(
        &self,
        _grants: &[SandboxGrant],
        _no_exec: &[CanonPath],
    ) -> Result<Option<Arc<dyn Backend>>, ProcessError> {
        Ok(None)
    }

    /// Whether [`Self::widened`] returns a backend. A human is asked to open a
    /// path only on a backend that can act on the answer.
    fn can_widen(&self) -> bool {
        false
    }
}

/// A mechanism selected only after paired production-path probes. Its
/// [`Backend`] implementation forwards mechanism facts and launch data;
/// verified bits remain separate in [`Self::capabilities`].
pub(crate) struct MeasuredBackend {
    inner: Arc<dyn Backend>,
    capabilities: ExecCaps,
    _scratch: Option<tempfile::TempDir>,
}

impl MeasuredBackend {
    pub(crate) fn new(
        inner: Arc<dyn Backend>,
        capabilities: ExecCaps,
        scratch: Option<tempfile::TempDir>,
    ) -> Result<Self, ProcessError> {
        if !inner.maximum_demonstrable_caps().contains(capabilities) {
            return Err(ProcessError::Invariant(
                "verified process capabilities exceed the backend maximum".into(),
            ));
        }
        Ok(Self {
            inner,
            capabilities,
            _scratch: scratch,
        })
    }

    pub(crate) fn capabilities(&self) -> ExecCaps {
        self.capabilities
    }
}

impl Backend for MeasuredBackend {
    fn identity(&self) -> BackendIdentity {
        self.inner.identity()
    }

    fn prerequisite(&self, policy: &RunConfinement) -> Result<(), ProcessError> {
        self.inner.prerequisite(policy)
    }

    fn maximum_demonstrable_caps(&self) -> ExecCaps {
        self.inner.maximum_demonstrable_caps()
    }

    fn launch_prefix(&self) -> LaunchPrefix {
        self.inner.launch_prefix()
    }

    fn widened(
        &self,
        grants: &[SandboxGrant],
        no_exec: &[CanonPath],
    ) -> Result<Option<Arc<dyn Backend>>, ProcessError> {
        self.inner.widened(grants, no_exec)
    }

    fn can_widen(&self) -> bool {
        self.inner.can_widen()
    }
}

/// Build an exact argv-native command and install one transient environment
/// snapshot. Restricted backends wrap this same specification rather than
/// deriving a shell string from it.
pub(crate) fn prefixed_command(
    prefix: LaunchPrefix,
    spec: &SpawnSpec,
) -> Result<Command, ProcessError> {
    let executable = spec
        .argv()
        .first()
        .ok_or_else(|| ProcessError::Invariant("an admitted SpawnSpec had empty argv".into()))?;
    let executable = spec.identity().map_or(executable.as_os_str(), |identity| {
        identity.target.as_path().as_os_str()
    });
    let mut command = match prefix.program {
        Some(wrapper) => {
            super::identity::require_launchable(executable, spec)?;
            let mut command = Command::new(wrapper);
            command.args(prefix.args_before_target);
            command.arg(executable);
            command.args(&spec.argv()[1..]);
            command
        }
        None => {
            if !prefix.args_before_target.is_empty() {
                return Err(ProcessError::Invariant(
                    "a direct backend supplied arguments before the exact argv".into(),
                ));
            }
            let mut command = Command::new(executable);
            command.args(&spec.argv()[1..]);
            command
        }
    };
    if let Some(cwd) = spec.cwd() {
        command.current_dir(cwd.as_path());
    }
    command.env_clear();
    command.envs(environment(spec)?);
    Ok(command)
}

pub(super) fn environment(spec: &SpawnSpec) -> Result<Vec<(OsString, OsString)>, ProcessError> {
    let snapshot = match spec.env_policy() {
        EnvPolicy::Exact { bindings, .. } => bindings
            .iter()
            .map(|binding| {
                binding
                    .value_os_string()
                    .map(|value| (binding.name().as_os_str().to_os_string(), value))
                    .map_err(|error| ProcessError::Invariant(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?,
        policy => std::env::vars_os()
            // Windows exposes pseudo-variables such as `=C:` for per-drive
            // working directories. Rust cannot install them through
            // `Command::env`, and the spawn contract carries an explicit cwd,
            // so they are not part of the executable environment authority.
            .filter(|(name, _)| admitted_name(name, policy) && EnvName::new(name.clone()).is_ok())
            .collect::<Vec<_>>(),
    };
    Ok(snapshot)
}

fn admitted_name(name: &OsStr, policy: &EnvPolicy) -> bool {
    match policy {
        EnvPolicy::Inherited => true,
        EnvPolicy::AllowOnly(names) => contains_name(names, name),
        EnvPolicy::InheritedExcept(names) => !contains_name(names, name),
        EnvPolicy::Exact { .. } => false,
    }
}

fn contains_name(names: &[ego_core::permit::EnvName], candidate: &OsStr) -> bool {
    names
        .iter()
        .any(|name| env_name_eq(name.as_os_str(), candidate))
}

#[cfg(windows)]
pub(super) fn env_name_eq(left: &OsStr, right: &OsStr) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
pub(super) fn env_name_eq(left: &OsStr, right: &OsStr) -> bool {
    left == right
}

pub(crate) fn executable(spec: &SpawnSpec) -> OsString {
    spec.argv().first().cloned().unwrap_or_default()
}

#[cfg(target_os = "linux")]
impl Backend for super::landlock::LandlockBackend {
    fn identity(&self) -> BackendIdentity {
        BackendIdentity::new(self.name(), self.revision())
    }

    fn maximum_demonstrable_caps(&self) -> ExecCaps {
        super::landlock::LandlockBackend::maximum_demonstrable_caps(self)
    }

    fn launch_prefix(&self) -> LaunchPrefix {
        super::landlock::LandlockBackend::launch_prefix(self)
    }
}

#[cfg(target_os = "macos")]
impl Backend for super::seatbelt::SeatbeltBackend {
    fn identity(&self) -> BackendIdentity {
        super::seatbelt::SeatbeltBackend::identity(self)
    }

    fn prerequisite(&self, policy: &RunConfinement) -> Result<(), ProcessError> {
        super::seatbelt::SeatbeltBackend::prerequisite(self, policy)
    }

    fn maximum_demonstrable_caps(&self) -> ExecCaps {
        super::seatbelt::SeatbeltBackend::maximum_demonstrable_caps(self)
    }

    fn launch_prefix(&self) -> LaunchPrefix {
        super::seatbelt::SeatbeltBackend::launch_prefix(self)
    }

    fn widened(
        &self,
        grants: &[SandboxGrant],
        no_exec: &[CanonPath],
    ) -> Result<Option<Arc<dyn Backend>>, ProcessError> {
        let widened: Arc<dyn Backend> = Arc::new(super::seatbelt::SeatbeltBackend::widened(
            self, grants, no_exec,
        )?);
        Ok(Some(widened))
    }

    fn can_widen(&self) -> bool {
        true
    }
}
