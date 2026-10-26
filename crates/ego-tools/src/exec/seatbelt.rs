//! macOS Seatbelt mechanism adapter.
//!
//! This module supplies a private profile and the fixed launch prefix. It
//! deliberately does not turn either fact into capability evidence: the
//! common executor grants capabilities only after its paired behavioural and
//! production-path probes succeed.

use sha2::{Digest as _, Sha256};
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
const BACKEND_REVISION: &str = "seatbelt-inline-profile-v8-derived-write-precedence";

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
    user_config: Vec<PathBuf>,
    adaptive: Option<ego_core::permit::AdaptivePerimeter>,
    level: Confinement,
    offline: bool,
    workspace: PathBuf,
    private_scratch: PathBuf,
    runtime_roots: Vec<PathBuf>,
    exec_paths: Vec<PathBuf>,
    roots: Vec<SandboxGrant>,
    read_only_roots: Vec<CanonPath>,
    write_restrictions: Vec<CanonPath>,
    lookup_metadata: Vec<PathBuf>,
    lookup_targets: Vec<PathBuf>,
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
        let adaptive = policy.adaptive_perimeter().cloned();
        let pnpm = if matches!(
            policy.level(),
            Confinement::Workspace | Confinement::Adaptive
        ) {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| ProcessError::Backend("pnpm runtime requires HOME".into()))?;
            let home = super::adaptive::resolve(Path::new(&home))?;
            super::pnpm::roots(&home, policy.workspace().as_path())?.to_vec()
        } else {
            Vec::new()
        };
        let (lookup_metadata, lookup_targets) = lookup_metadata(derived_runtime_roots);
        let base = ProfileBase {
            lookup_metadata,
            lookup_targets,
            user_config: if policy.level() == Confinement::Off {
                Vec::new()
            } else {
                super::user_config::capture()?
            },
            adaptive,
            level: policy.level(),
            offline: policy.offline(),
            workspace: policy.workspace().as_path().to_path_buf(),
            private_scratch: private_scratch.as_path().to_path_buf(),
            runtime_roots: derived_runtime_roots
                .iter()
                .map(|root| root.as_path().to_path_buf())
                .chain(pnpm.iter().take(1).cloned())
                .collect(),
            exec_paths,
            read_only_roots: policy
                .roots()
                .iter()
                .filter(|root| root.access() == ego_core::permit::RootAccess::Read)
                .map(|root| root.path().clone())
                .collect(),
            write_restrictions: policy
                .roots()
                .iter()
                .filter(|root| root.restricts_writes(policy.workspace(), policy.roots()))
                .map(|root| root.path().clone())
                .collect(),
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
                .chain(pnpm.into_iter().map(|path| {
                    SandboxGrant::new(CanonPath::new(path), ego_core::permit::SandboxAccess::Write)
                }))
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
        // Read roots remain non-executable even under Adaptive's broad baseline.
        let no_exec = no_exec
            .iter()
            .cloned()
            .chain(base.read_only_roots.iter().cloned())
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
        if matches!(base.level, Confinement::Workspace | Confinement::Ro)
            && !base.lookup_metadata.is_empty()
        {
            // Shell lookup needs stat/access, not file contents or exec. Capture
            // the operator PATH only; a child PATH override grants no new reach.
            profile.push_str("(allow file-read-metadata\n");
            for root in &base.lookup_metadata {
                profile.push_str(&format!(
                    "  (subpath {})\n",
                    profile::path_literal(root)
                        .map_err(|error| ProcessError::Backend(error.to_string()))?
                ));
            }
            let literals = base
                .lookup_metadata
                .iter()
                .chain(&base.lookup_targets)
                .flat_map(|path| path.ancestors())
                .collect::<BTreeSet<_>>();
            for target in literals {
                profile.push_str(&format!(
                    "  (literal {})\n",
                    profile::path_literal(target)
                        .map_err(|error| ProcessError::Backend(error.to_string()))?
                ));
            }
            profile.push_str(")\n");
        }
        if let Some(perimeter) = &base.adaptive {
            // A broad read root does not erase a more-specific launch-derived
            // write root. Nested explicit read restrictions still win, as do
            // the credential and host-control floors rendered above.
            for root in &base.read_only_roots {
                let mut exceptions = Vec::new();
                if !base.write_restrictions.contains(root) {
                    exceptions.extend(
                        perimeter
                            .writable
                            .iter()
                            .map(|path| path.as_path())
                            .chain(std::iter::once(base.workspace.as_path()))
                            .chain(
                                base.roots
                                    .iter()
                                    .filter(|grant| {
                                        grant.access == ego_core::permit::SandboxAccess::Write
                                    })
                                    .map(|grant| grant.path.as_path()),
                            )
                            .filter(|path| {
                                *path != root.as_path() && path.starts_with(root.as_path())
                            }),
                    );
                }
                let literal = profile::path_literal(root.as_path())
                    .map_err(|error| ProcessError::Backend(error.to_string()))?;
                if exceptions.is_empty() {
                    profile.push_str(&format!("(deny file-write* (subpath {literal}))\n"));
                } else {
                    profile.push_str(&format!("(deny file-write* (require-all (subpath {literal}) (require-not (require-any\n"));
                    for path in exceptions {
                        profile.push_str(&format!(
                            "  (subpath {})\n",
                            profile::path_literal(path)
                                .map_err(|error| ProcessError::Backend(error.to_string()))?
                        ));
                    }
                    profile.push_str("))))\n");
                }
                for ancestor in root.as_path().ancestors() {
                    profile.push_str(&format!(
                        "(deny file-write-unlink (literal {}))\n",
                        profile::path_literal(ancestor)
                            .map_err(|error| ProcessError::Backend(error.to_string()))?
                    ));
                }
            }
        }
        if base.level != Confinement::Off {
            // Explicit denies win over workspace, adaptive and later human grants.
            // Protect ancestor entries too: renaming one would move the subtree
            // outside the path-based deny before a later write.
            for root in &base.write_restrictions {
                profile.push_str(&format!(
                    "(deny file-write* (subpath {}))\n",
                    profile::path_literal(root.as_path())
                        .map_err(|error| ProcessError::Backend(error.to_string()))?
                ));
                for ancestor in root.as_path().ancestors() {
                    profile.push_str(&format!(
                        "(deny file-write-unlink (literal {}))\n",
                        profile::path_literal(ancestor)
                            .map_err(|error| ProcessError::Backend(error.to_string()))?
                    ));
                }
            }
            profile.push_str("(deny file-write*\n");
            for directory in &base.user_config {
                profile.push_str(&format!(
                    "  (subpath {})\n",
                    profile::path_literal(directory)
                        .map_err(|error| ProcessError::Backend(error.to_string()))?
                ));
            }
            profile.push_str(")\n(deny file-write-unlink\n");
            // Renaming any ancestor would replace the captured authority too.
            for ancestor in base
                .user_config
                .iter()
                .flat_map(|directory| directory.ancestors())
            {
                profile.push_str(&format!(
                    "  (literal {})\n",
                    profile::path_literal(ancestor)
                        .map_err(|error| ProcessError::Backend(error.to_string()))?
                ));
            }
            profile.push_str(")\n");
        }
        profile.push_str(
            &profile::deny_exec(&no_exec)
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

    pub(crate) fn revision(&self) -> String {
        let mut hash = Sha256::new();
        for path in self
            .base
            .lookup_metadata
            .iter()
            .chain(&self.base.lookup_targets)
        {
            hash.update(path.as_os_str().as_encoded_bytes());
            hash.update([0]);
        }
        format!("{BACKEND_REVISION}:{:x}", hash.finalize())
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
            .chain(
                xdg.into_iter()
                    .flat_map(|xdg| [xdg.join("git/config"), xdg.join("git/ignore")]),
            )
        {
            match fs::canonicalize(&path) {
                Ok(root) if root.is_file() => {
                    roots.insert(root);
                }
                // A config-shaped directory must not grant its whole subtree.
                Ok(_) => {}
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

/// Metadata-only command discovery. Canonical symlink targets need their own
/// exact metadata literals, but never become read-data or executable roots.
fn lookup_metadata(runtime_roots: &[CanonPath]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut roots = BTreeSet::new();
    let mut targets = BTreeSet::new();
    if let Some(path) = std::env::var_os("PATH") {
        for root in std::env::split_paths(&path).filter(|root| root.is_absolute()) {
            let Ok(canonical) = fs::canonicalize(&root) else {
                continue;
            };
            if !canonical.is_dir() {
                continue;
            }
            roots.insert(root.clone());
            roots.insert(canonical);
            if let Ok(entries) = fs::read_dir(root) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_symlink())
                        && let Ok(target) = fs::canonicalize(entry.path())
                    {
                        // Seatbelt also checks the named target while walking
                        // intermediate directory aliases (for example an old
                        // tool-version directory linked to the current one).
                        if let Ok(named) = fs::read_link(entry.path()) {
                            targets.insert(if named.is_absolute() {
                                named
                            } else {
                                entry.path().parent().unwrap().join(named)
                            });
                        }
                        targets.insert(target);
                    }
                }
            }
        }
    }
    // Most PATH links point into an already-readable runtime (Homebrew,
    // Apple tools). Do not expand thousands of redundant target ancestors
    // into SBPL: large profiles fail to compile on the native backend.
    targets.retain(|target| {
        !roots.iter().any(|root| target.starts_with(root))
            && !runtime_roots
                .iter()
                .any(|root| target.starts_with(root.as_path()))
    });
    (roots.into_iter().collect(), targets.into_iter().collect())
}
