//! macOS Seatbelt mechanism adapter.
//!
//! This module supplies a private profile and the fixed launch prefix. It
//! deliberately does not turn either fact into capability evidence: the
//! common executor grants capabilities only after its paired behavioural and
//! production-path probes succeed.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use ego_core::permit::{
    BackendIdentity, CanonPath, Confinement, ExecCaps, ProcessError, RunConfinement, SandboxGrant,
};

use super::backend::LaunchPrefix;
use super::profile::{self, ProfileInput};

#[cfg(test)]
#[path = "seatbelt_tests.rs"]
mod tests;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const BACKEND_REVISION: &str = "seatbelt-inline-profile-v4-strict-credentials";

/// Policy text is owned by the host and passed directly to each launcher.
/// No child-writable file can change a later spawn's policy.
pub(crate) struct SeatbeltBackend {
    profile: String,
    maximum_caps: ExecCaps,
    base: ProfileBase,
}

/// Everything the profile text is rendered from except the human grants, kept
/// so the same profile can be rendered again with more of them.
#[derive(Clone)]
struct ProfileBase {
    adaptive: Option<ego_core::permit::AdaptivePerimeter>,
    level: Confinement,
    offline: bool,
    workspace: PathBuf,
    private_scratch: PathBuf,
    runtime_roots: Vec<PathBuf>,
    exec_paths: Vec<PathBuf>,
    roots: Vec<SandboxGrant>,
}

impl SeatbeltBackend {
    /// Prepare one fixed profile for the exact effective run policy.
    pub(crate) fn prepare(
        policy: &RunConfinement,
        private_scratch: &CanonPath,
        derived_runtime_roots: &[CanonPath],
    ) -> Result<Self, ProcessError> {
        prerequisite()?;
        validate_private_scratch(private_scratch.as_path())?;

        // Every program must be able to stat the workspace ancestor chain:
        // resolving cwd and `mkdir -p` need it even without exec paths.
        // Runtime roots also need ancestor metadata for Homebrew command lookup.
        // The renderer grants exact metadata literals, never ancestor contents.
        let mut exec_paths = derived_runtime_roots
            .iter()
            .map(|path| path.as_path().to_path_buf())
            .collect::<Vec<_>>();
        exec_paths.push(policy.workspace().as_path().to_path_buf());
        let mut adaptive = policy.adaptive_perimeter().cloned();
        if let Some(perimeter) = adaptive.as_mut() {
            perimeter.controls.extend(
                policy
                    .roots()
                    .iter()
                    .filter(|root| root.access() == ego_core::permit::RootAccess::Read)
                    .map(|root| root.path().clone()),
            );
        }
        let base = ProfileBase {
            adaptive,
            level: policy.level(),
            offline: policy.offline(),
            workspace: policy.workspace().as_path().to_path_buf(),
            private_scratch: private_scratch.as_path().to_path_buf(),
            runtime_roots: derived_runtime_roots
                .iter()
                .map(|root| root.as_path().to_path_buf())
                .collect(),
            exec_paths,
            roots: policy
                .roots()
                .iter()
                .map(|root| SandboxGrant {
                    path: root.path().clone(),
                    access: if root.access() == ego_core::permit::RootAccess::ReadWrite
                        && matches!(
                            policy.level(),
                            Confinement::Workspace | Confinement::Adaptive
                        ) {
                        ego_core::permit::SandboxAccess::Write
                    } else {
                        ego_core::permit::SandboxAccess::Read
                    },
                })
                .collect(),
        };
        Self::render_profile(base, &[], &[], policy.requirements().required())
    }

    /// The same backend with `grants` added to its profile and exec denied
    /// under `no_exec`. The scratch root stays the one the measured backend
    /// owns, so the caller keeps that backend alive for as long as it uses
    /// the result.
    pub(crate) fn widened(
        &self,
        grants: &[SandboxGrant],
        no_exec: &[CanonPath],
    ) -> Result<Self, ProcessError> {
        Self::render_profile(self.base.clone(), grants, no_exec, self.maximum_caps)
    }

    fn render_profile(
        base: ProfileBase,
        grants: &[SandboxGrant],
        no_exec: &[CanonPath],
        maximum_caps: ExecCaps,
    ) -> Result<Self, ProcessError> {
        let runtime_roots = base
            .runtime_roots
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let exec_paths = base
            .exec_paths
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let grants = base.roots.iter().chain(grants).cloned().collect::<Vec<_>>();
        let mut profile = profile::render(&ProfileInput {
            adaptive: base.adaptive.as_ref(),
            level: base.level,
            offline: base.offline,
            workspace: &base.workspace,
            private_scratch: &base.private_scratch,
            runtime_roots: &runtime_roots,
            exec_paths: &exec_paths,
            grants: &grants,
        })
        .map_err(|error| ProcessError::Backend(error.to_string()))?;
        profile.push_str(
            &profile::deny_exec(no_exec)
                .map_err(|error| ProcessError::Backend(error.to_string()))?,
        );

        Ok(Self {
            profile,
            maximum_caps,
            base,
        })
    }

    pub(crate) const fn name(&self) -> &'static str {
        "seatbelt"
    }

    pub(crate) const fn revision(&self) -> &'static str {
        BACKEND_REVISION
    }

    pub(crate) fn identity(&self) -> BackendIdentity {
        BackendIdentity::new(self.name(), self.revision())
    }

    pub(crate) fn prerequisite(&self, _policy: &RunConfinement) -> Result<(), ProcessError> {
        prerequisite()
    }

    /// Bits the mechanism is eligible to prove for this profile.
    ///
    /// This is an upper bound, never an observation. The common executor must
    /// intersect it with complete paired runtime evidence before publishing a
    /// capability.
    pub(crate) const fn maximum_demonstrable_caps(&self) -> ExecCaps {
        self.maximum_caps
    }

    pub(crate) fn launch_prefix(&self) -> LaunchPrefix {
        LaunchPrefix {
            program: Some(OsString::from(SANDBOX_EXEC)),
            args_before_target: vec![OsString::from("-p"), OsString::from(&self.profile)],
        }
    }
}

fn prerequisite() -> Result<(), ProcessError> {
    let metadata = fs::metadata(SANDBOX_EXEC).map_err(|error| {
        ProcessError::Backend(format!(
            "Seatbelt launcher {SANDBOX_EXEC} is unavailable: {error}"
        ))
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(ProcessError::Backend(format!(
            "Seatbelt launcher {SANDBOX_EXEC} is not an executable file"
        )));
    }
    Ok(())
}

fn validate_private_scratch(path: &Path) -> Result<(), ProcessError> {
    if !path.is_absolute() {
        return Err(ProcessError::Backend(format!(
            "Seatbelt private scratch is not absolute: {}",
            path.display()
        )));
    }
    let metadata = fs::metadata(path).map_err(|error| {
        ProcessError::Backend(format!(
            "Seatbelt private scratch {} is unavailable: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(ProcessError::Backend(format!(
            "Seatbelt private scratch is not a directory: {}",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ProcessError::Backend(format!(
            "Seatbelt private scratch is not private: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Fixed runtime roots required by Apple's installed command-line runtime.
///
/// The list is backend-owned input. It is intentionally conservative: paired
/// read probes, rather than this table, decide whether scoped read capability
/// can be reported on the current host.
pub(crate) fn derived_runtime_roots() -> Result<Vec<CanonPath>, ProcessError> {
    [
        "/System",
        "/Library/Apple",
        "/usr/bin",
        "/usr/lib",
        "/usr/libexec",
        "/usr/sbin",
        "/usr/share",
        "/bin",
        "/sbin",
    ]
    .into_iter()
    .map(PathBuf::from)
    .map(|root| {
        fs::canonicalize(&root).map_err(|error| {
            ProcessError::Backend(format!(
                "Seatbelt runtime root {} is unavailable: {error}",
                root.display()
            ))
        })
    })
    .collect::<Result<BTreeSet<_>, _>>()
    .and_then(|mut roots| {
        // Homebrew's stable prefix contains its symlinked binaries, Cellar
        // targets and runtime libraries. An absent installation adds no grant.
        match fs::canonicalize("/opt/homebrew") {
            Ok(root) => {
                roots.insert(root);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ProcessError::Backend(format!(
                    "cannot resolve Homebrew runtime root /opt/homebrew: {error}"
                )));
            }
        }
        // Git reads global configuration even for `git --version`. Admit
        // only its standard config files, not the user's home directory.
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".config")));
        for path in home
            .as_ref()
            .map(|home| home.join(".gitconfig"))
            .into_iter()
            .chain(xdg.map(|xdg| xdg.join("git/config")))
        {
            match fs::canonicalize(&path) {
                Ok(root) => {
                    roots.insert(root);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ProcessError::Backend(format!(
                        "cannot resolve Git runtime config {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Ok(roots.into_iter().map(CanonPath::new).collect())
    })
}
