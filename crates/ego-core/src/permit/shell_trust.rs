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
    let observation = super::observe(&argv);
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
    if argv.first().is_some_and(|program| reads_content(program))
        || observation
            .programs
            .iter()
            .any(|program| reads_content(program))
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
        // An earlier workspace, relative, empty or untrusted root invalidates
        // the proof even when capture found no executable there at that instant.
        if !trusted_path(&root) {
            return false;
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
