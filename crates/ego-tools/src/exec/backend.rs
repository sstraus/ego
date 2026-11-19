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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    // Catches: re-resolving a mutable alias runs a different file after validation;
    // hard-link dispatch names and exact arguments must survive canonical launch.
    #[tokio::test]
    async fn canonical_target_survives_symlink_swap_and_hard_link_keeps_name() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let shim = root.join("tuic-agent-shim");
        std::fs::write(&shim, b"#!/bin/sh\nprintf '%s\\n' \"${0##*/}\" \"$@\"\n").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let alias = root.join("ego");
        symlink(&shim, &alias).unwrap();
        let hard_link = root.join("ego-hard-link");
        std::fs::hard_link(&shim, &hard_link).unwrap();
        let replacement = root.join("replacement");
        std::fs::write(&replacement, b"#!/bin/sh\nprintf 'replacement\\n'\n").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
        let environment = EnvPolicy::exact(vec![
            ego_core::permit::ExplicitEnvBinding::from_native(
                EnvName::new("PATH").unwrap(),
                root.as_os_str().to_owned(),
            )
            .unwrap(),
        ])
        .unwrap();
        let mut failures = Vec::new();
        for (invoked, expected_name, is_symlink) in [
            (alias.clone().into_os_string(), "tuic-agent-shim", true),
            (OsString::from("./ego"), "tuic-agent-shim", true),
            (OsString::from("ego"), "tuic-agent-shim", true),
            (hard_link.into_os_string(), "ego-hard-link", false),
            (shim.clone().into_os_string(), "tuic-agent-shim", false),
        ] {
            for (wrapped, swap) in [(false, false), (true, false), (false, true), (true, true)] {
                if swap && !is_symlink {
                    continue;
                }
                std::fs::remove_file(&alias).unwrap();
                symlink(&shim, &alias).unwrap();
                let spec = SpawnSpec::new(
                    vec![
                        invoked.clone(),
                        "models".into(),
                        "space argument".into(),
                        "".into(),
                    ],
                    Some(CanonPath::new(&root)),
                    None,
                    environment.clone(),
                )
                .unwrap();
                let captured = super::super::identity::capture(&spec).unwrap();
                super::super::identity::revalidate(&captured).unwrap();
                if swap {
                    std::fs::remove_file(&alias).unwrap();
                    symlink(&replacement, &alias).unwrap();
                }
                let prefix = if wrapped {
                    LaunchPrefix {
                        program: Some("/bin/sh".into()),
                        args_before_target: vec![
                            "-c".into(),
                            "exec \"$@\"".into(),
                            "wrapper".into(),
                        ],
                    }
                } else {
                    LaunchPrefix {
                        program: None,
                        args_before_target: vec![],
                    }
                };
                let output = prefixed_command(prefix, &captured)
                    .unwrap()
                    .output()
                    .await
                    .unwrap();
                let expected = format!("{expected_name}\nmodels\nspace argument\n\n");
                if !output.status.success()
                    || output.stdout != expected.as_bytes()
                    || !output.stderr.is_empty()
                {
                    failures.push(format!(
                        "{invoked:?}, wrapped={wrapped}, swap={swap}: status={}, stdout={:?}, stderr={:?}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
