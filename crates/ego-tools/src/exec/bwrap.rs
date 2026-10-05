//! Linux namespace confinement. Capability claims belong to the shared probes.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use ego_core::permit::{
    BackendIdentity, CanonPath, Confinement, ExecCaps, ProcessError, RunConfinement,
};
use sha2::{Digest as _, Sha256};

use super::backend::{Backend, LaunchPrefix};

pub(crate) struct BwrapBackend {
    executable: OsString,
    arguments: Vec<OsString>,
    revision: String,
    maximum_caps: ExecCaps,
}

impl BwrapBackend {
    pub(crate) fn prepare(
        policy: &RunConfinement,
        scratch: &CanonPath,
        runtime_roots: &[CanonPath],
    ) -> Result<Self, ProcessError> {
        // Use the installed system utility, never a model-controlled PATH lookup.
        let executable = fs::canonicalize("/usr/bin/bwrap")
            .map_err(|error| ProcessError::Backend(format!("bwrap unavailable: {error}")))?;
        let metadata = fs::metadata(&executable)
            .map_err(|error| ProcessError::Backend(format!("cannot inspect bwrap: {error}")))?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(ProcessError::Backend(
                "bwrap is not an executable file".into(),
            ));
        }
        let image = fs::read(&executable)
            .map_err(|error| ProcessError::Backend(format!("cannot identify bwrap: {error}")))?;
        let revision = format!("bwrap-namespaces-v1:{:x}", Sha256::digest(image));
        let mut arguments: Vec<OsString> = [
            "--die-with-parent",
            "--unshare-user",
            "--unshare-pid",
            "--cap-drop",
            "ALL",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        if policy.offline() {
            arguments.push("--unshare-net".into());
        }
        if policy.level() == Confinement::Off {
            bind(&mut arguments, "--bind", Path::new("/"));
        } else {
            for root in runtime_roots {
                bind(&mut arguments, "--ro-bind", root.as_path());
            }
            // Canonical runtime binds need the host's loader/shebang aliases too.
            for alias in ["/bin", "/sbin", "/lib", "/lib64"] {
                match fs::read_link(alias) {
                    Ok(target) => {
                        arguments.push("--symlink".into());
                        arguments.push(target.into_os_string());
                        arguments.push(alias.into());
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
                        ) => {}
                    Err(error) => {
                        return Err(ProcessError::Backend(format!(
                            "cannot inspect runtime alias {alias}: {error}"
                        )));
                    }
                }
            }
            let mode = if policy.level() == Confinement::Workspace {
                "--bind"
            } else {
                "--ro-bind"
            };
            for root in policy.roots() {
                let root_mode = if policy.level() == Confinement::Workspace
                    && root.access() == ego_core::permit::RootAccess::ReadWrite
                {
                    "--bind"
                } else {
                    "--ro-bind"
                };
                bind(&mut arguments, root_mode, root.path().as_path());
            }
            bind(&mut arguments, mode, policy.workspace().as_path());
            bind(&mut arguments, mode, scratch.as_path());
        }
        // A private PID namespace prevents /proc from exposing host namespace
        // handles. No new session: the common lifecycle owns the process group.
        arguments.extend(["--proc", "/proc"].map(OsString::from));
        for device in ["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"] {
            bind(&mut arguments, "--ro-bind", Path::new(device));
        }
        if policy.level() != Confinement::Off {
            // The empty root's scaffolding is not another writable scratch.
            arguments.extend(["--remount-ro", "/proc", "--remount-ro", "/"].map(OsString::from));
        }
        arguments.push("--".into());
        Ok(Self {
            executable: executable.into_os_string(),
            arguments,
            revision,
            maximum_caps: policy.requirements().required(),
        })
    }
}

fn bind(arguments: &mut Vec<OsString>, mode: &str, path: &Path) {
    arguments.push(mode.into());
    arguments.push(path.as_os_str().to_owned());
    arguments.push(path.as_os_str().to_owned());
}

impl Backend for BwrapBackend {
    fn identity(&self) -> BackendIdentity {
        BackendIdentity::new("bwrap", &self.revision)
    }

    fn maximum_demonstrable_caps(&self) -> ExecCaps {
        self.maximum_caps
    }

    fn launch_prefix(&self) -> LaunchPrefix {
        LaunchPrefix {
            program: Some(self.executable.clone()),
            args_before_target: self.arguments.clone(),
        }
    }
}
