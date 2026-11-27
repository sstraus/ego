use std::path::{Component, Path};

use super::super::{SpawnSpec, analysis::ExecutionIdentity};

fn trusted_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && [
            "/bin",
            "/usr/bin",
            "/usr/sbin",
            // Ubuntu 26.04 links `ls`, `wc` and friends from /usr/bin to the
            // root-owned multicall binaries that its package installs here.
            "/usr/lib/cargo/bin/coreutils",
            "/usr/lib/cargo/bin/findutils",
            "/opt/homebrew/bin",
            "/opt/homebrew/opt",
            "/opt/homebrew/Cellar",
            "/usr/local/bin",
            "/usr/local/opt",
            "/usr/local/Cellar",
        ]
        .iter()
        .any(|root| path.starts_with(root))
}

/// Require host-observed executables before shell syntax can establish read-only authority.
pub fn trusted_read_only(spec: &SpawnSpec) -> bool {
    proven_read_only(spec, false)
}

/// [`trusted_read_only`] for a run whose sandbox refuses credential and control
/// files at the operating system: content readers (`cat`, `head`, `wc`,
/// `grep`...) and repository inspection (`git status|log|diff|show|config --get`)
/// qualify too, so a read-only question is not asked. The caller owns the
/// guarantee that the sandbox is in force.
pub fn sandboxed_read_only(spec: &SpawnSpec) -> bool {
    proven_read_only(spec, true)
}

fn proven_read_only(spec: &SpawnSpec, sandboxed: bool) -> bool {
    let Some(identity) = spec.identity().filter(|identity| identity.complete()) else {
        return false;
    };
    if !trusted_path(identity.target.as_path())
        || spec.env_policy().exact_bindings().iter().any(|binding| {
            let name = binding.name().as_os_str().to_string_lossy();
            name.starts_with("LD_")
                || name.starts_with("DYLD_")
                || name.starts_with("BASH_FUNC_")
                || matches!(name.as_ref(), "BASH_ENV" | "ENV" | "ZDOTDIR")
        })
    {
        return false;
    }
    let argv: Vec<_> = spec
        .argv()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let observation = if sandboxed {
        super::observe_sandboxed(&argv)
    } else {
        super::observe(&argv)
    };
    if !observation.complete || !observation.read_only {
        return false;
    }
    // Executable capture does not pin input files. Even a static benign name
    // can resolve through a symlink to a credential/control file. Content reads
    // therefore need ordinary call-bound authorization, not this syntax proof.
    let reads_content = |program: &str| {
        matches!(
            program.rsplit('/').next(),
            Some("cat" | "head" | "tail" | "wc" | "sed" | "rg" | "grep")
        )
    };
    if !sandboxed
        && (argv.first().is_some_and(|program| reads_content(program))
            || observation
                .programs
                .iter()
                .any(|program| reads_content(program)))
    {
        return false;
    }
    if observation.programs.is_empty() {
        return true;
    }
    // Login/startup files and extra shell options are outside the static proof.
    if argv.len() != 3 || argv[1] != "-c" {
        return false;
    }
    observation
        .programs
        .iter()
        .all(|program| trusted_inner(program, spec, identity))
}

fn trusted_inner(program: &str, spec: &SpawnSpec, identity: &ExecutionIdentity) -> bool {
    let path = Path::new(program);
    if path.is_absolute() {
        return trusted_path(path)
            && identity
                .files
                .iter()
                .any(|file| file.named.as_path() == path && trusted_path(file.resolved.as_path()));
    }
    if program.is_empty() || program.contains('/') || program == "." || program == ".." {
        return false;
    }
    let Some(value) = spec
        .env_policy()
        .exact_bindings()
        .iter()
        .find(|binding| binding.name().as_os_str() == "PATH")
        .and_then(|binding| binding.value_os_string().ok())
    else {
        return false;
    };
    for root in std::env::split_paths(&value) {
        // Capture resolved the first executable. The process adapter repeats
        // that resolution before spawn, including earlier untrusted roots.
        if !trusted_path(&root) {
            if !root.is_absolute() {
                return false;
            }
            continue;
        }
        if let Some(file) = identity
            .files
            .iter()
            .find(|file| file.named.as_path() == root.join(program))
        {
            return trusted_path(file.resolved.as_path());
        }
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::trusted_path;
    use std::path::Path;

    // Catches: a distribution whose system tools resolve to packaged multicall
    // binaries outside /usr/bin loses read-only approval for `ls`.
    #[test]
    fn packaged_system_multicall_binaries_are_trusted_and_lookalikes_are_not() {
        assert!(trusted_path(Path::new("/usr/lib/cargo/bin/coreutils/ls")));
        assert!(trusted_path(Path::new("/usr/lib/cargo/bin/findutils/find")));
        assert!(!trusted_path(Path::new("/usr/lib/cargo/bin/coreutilsx/ls")));
        assert!(!trusted_path(Path::new(
            "/usr/lib/cargo/bin/coreutils/../x"
        )));
    }
}
