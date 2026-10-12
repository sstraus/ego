//! Resolve operator-owned adaptive sources once, before a child can mutate them.

use ego_core::permit::{AdaptivePerimeter, CanonPath, Confinement, ProcessError, RunConfinement};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[path = "adaptive_floor.rs"]
pub(super) mod floor;

/// The process-edge constructor shared by CLI, ACP, and diagnostics.
pub fn admit_confinement(
    level: Confinement,
    offline: bool,
    workspace: CanonPath,
) -> Result<RunConfinement, ProcessError> {
    let policy = RunConfinement::new(level, offline, workspace.clone())
        .map_err(|error| ProcessError::Backend(error.to_string()))?;
    if level != Confinement::Adaptive {
        return Ok(policy);
    }
    if !cfg!(target_os = "macos") {
        return Err(error(
            "adaptive confinement is currently demonstrated only on macOS; select workspace, ro or off explicitly",
        ));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| error("adaptive requires HOME"))?;
    let home = resolve(Path::new(&home))?;
    let env = |key| std::env::var_os(key).map(PathBuf::from);
    let cargo = env("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
    let rustup = env("RUSTUP_HOME").unwrap_or_else(|| home.join(".rustup"));
    for source in [&cargo, &rustup] {
        check_write_root(&home, workspace.as_path(), &resolve(source)?, &[])?;
    }
    let temporary = env("TMPDIR").unwrap_or_else(std::env::temp_dir);
    let mbx = mbx_cache(&home)?;
    let mut writable = vec![
        cargo.join("registry"),
        cargo.join("git"),
        cargo.join(".package-cache"),
        cargo.join(".package-cache-mutate"),
        cargo.join(".global-cache"),
        rustup.join("downloads"),
        rustup.join("tmp"),
        temporary,
    ];
    writable.extend(mbx);
    writable.extend(env("CARGO_TARGET_DIR"));
    // Empty variables mean unset. Content caches open; host-executed pnpm
    // home/bin, store links, dlx tools and npm `_npx` remain read-only.
    let set = |key| env(key).filter(|path| !path.as_os_str().is_empty());
    let pnpm = set("PNPM_HOME")
        .or_else(|| set("XDG_DATA_HOME").map(|data| data.join("pnpm")))
        .unwrap_or_else(|| home.join("Library/pnpm"));
    let caches = set("XDG_CACHE_HOME").unwrap_or_else(|| home.join("Library/Caches"));
    let npm = set("npm_config_cache")
        .or_else(|| set("NPM_CONFIG_CACHE"))
        .unwrap_or_else(|| home.join(".npm"));
    writable.extend([
        pnpm.join("store"),
        caches.join("pnpm"),
        npm.join("_cacache"),
        npm.join("_logs"),
    ]);
    let (git, mut controls) = git_roots(workspace.as_path())?;
    // Broad workspace/temp/grant roots must not reopen host-executed controls.
    // Protect defaults too when an operator relocates the active toolchain home.
    for root in [home.join(".cargo"), cargo.clone()] {
        controls.extend(
            [
                "env",
                "bin",
                "config",
                "config.toml",
                ".crates.toml",
                ".crates2.json",
            ]
            .map(|path| root.join(path)),
        );
    }
    for root in [home.join(".rustup"), rustup] {
        controls
            .extend(["settings.toml", "toolchains", "update-hashes"].map(|path| root.join(path)));
    }
    writable.extend(git);
    let store = pnpm.join("store");
    let versions = match std::fs::read_dir(&store) {
        Ok(entries) => entries.collect::<Result<Vec<_>, _>>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
    .map_err(|e| error(format!("cannot inspect pnpm store versions: {e}")))?;
    let mut links: Vec<_> = versions
        .into_iter()
        .filter(|entry| entry.file_name().as_encoded_bytes().starts_with(b"v"))
        .map(|entry| entry.path().join("links"))
        .collect();
    if links.is_empty() {
        links.push(store.join("v11/links")); // pnpm 12's default store version.
    }
    controls.extend(links);
    controls.push(caches.join("pnpm/dlx"));
    let mut credentials = [
        ".ssh",
        ".aws",
        ".gnupg",
        ".netrc",
        ".git-credentials",
        ".config/gh/hosts.yml",
        ".cargo/credentials",
        ".cargo/credentials.toml",
        ".codex/auth.json",
        ".claude/.credentials.json",
        ".config/ego/credentials.json",
        "Library/Keychains",
        "Library/Application Support/Google/Chrome",
        "Library/Application Support/Microsoft Edge",
        "Library/Application Support/Firefox",
        "Library/Safari",
        ".mozilla",
        ".config/google-chrome",
        ".config/chromium",
    ]
    .into_iter()
    .map(|path| home.join(path))
    .collect::<Vec<_>>();
    credentials.extend(env("GH_CONFIG_DIR").map(|root| root.join("hosts.yml")));
    credentials.extend([cargo.join("credentials"), cargo.join("credentials.toml")]);
    let ego = env("EGO_HOME").unwrap_or_else(|| home.join(".ego"));
    credentials.extend([ego.join("auth.json"), ego.join("auth.json.tmp")]);
    let perimeter = capture(&home, workspace.as_path(), writable, credentials, controls)?;
    Ok(policy.with_adaptive_perimeter(perimeter))
}

fn error(message: impl Into<String>) -> ProcessError {
    ProcessError::Backend(message.into())
}

/// Resolve absent paths through the nearest existing ancestor without creating it.
fn resolve(path: &Path) -> Result<PathBuf, ProcessError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(error(
            "adaptive roots must be absolute and contain no parent traversal",
        ));
    }
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for name in suffix.into_iter().rev() {
                    canonical.push(name);
                }
                return Ok(canonical);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink must never be treated as a missing directory.
                if std::fs::symlink_metadata(existing).is_ok() {
                    return Err(error("adaptive source contains a dangling symlink"));
                }
                suffix.push(
                    existing
                        .file_name()
                        .ok_or_else(|| error("unresolvable adaptive root"))?,
                );
                existing = existing
                    .parent()
                    .ok_or_else(|| error("unresolvable adaptive ancestor"))?;
            }
            Err(e) => return Err(error(format!("cannot resolve adaptive source: {e}"))),
        }
    }
}

fn capture(
    home: &Path,
    workspace: &Path,
    writable: Vec<PathBuf>,
    credentials: Vec<PathBuf>,
    controls: Vec<PathBuf>,
) -> Result<AdaptivePerimeter, ProcessError> {
    let canonical_set = |paths: Vec<PathBuf>| -> Result<Vec<CanonPath>, ProcessError> {
        paths
            .into_iter()
            .map(|p| resolve(&p))
            .collect::<Result<BTreeSet<_>, _>>()
            .map(|set| set.into_iter().map(CanonPath::new).collect())
    };
    // Keep both lexical and canonical names: the OS resolves aliases while
    // protection must also survive replacement of a previously captured link.
    let protected = |paths: Vec<PathBuf>| -> Result<Vec<CanonPath>, ProcessError> {
        let mut set = canonical_set(paths.clone())?;
        set.extend(paths.into_iter().map(CanonPath::new));
        set.sort_by(|a, b| a.as_path().cmp(b.as_path()));
        set.dedup();
        Ok(set)
    };
    let credentials = protected(credentials)?;
    let controls = protected(controls)?;
    let writable = canonical_set(writable)?;
    for root in &writable {
        check_write_root(home, workspace, root.as_path(), &credentials)?;
    }
    Ok(AdaptivePerimeter {
        home: CanonPath::new(home),
        writable,
        credentials,
        controls,
    })
}

fn check_write_root(
    home: &Path,
    workspace: &Path,
    path: &Path,
    credentials: &[CanonPath],
) -> Result<(), ProcessError> {
    if path == Path::new("/")
        || home.starts_with(path)
        || workspace.starts_with(path)
        || ["/System", "/Library", "/usr", "/bin", "/sbin", "/etc"]
            .iter()
            .any(|root| path.starts_with(root))
        || credentials
            .iter()
            .any(|floor| path.starts_with(floor.as_path()))
    {
        return Err(error("adaptive source crosses a hard write floor"));
    }
    Ok(())
}

fn mbx_cache(home: &Path) -> Result<Option<PathBuf>, ProcessError> {
    let config = home.join("Library/Application Support/mbx/config.toml");
    let text = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(error(format!("cannot read trusted mbx config: {e}"))),
    };
    let config = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| error("invalid trusted mbx config"))?;
    config
        .get("cache_dir")
        .map(|item| {
            let value = item
                .as_str()
                .ok_or_else(|| error("mbx cache_dir must be a path string"))?;
            Ok(value
                .strip_prefix("~/")
                .map_or_else(|| PathBuf::from(value), |p| home.join(p)))
        })
        .transpose()
}

fn git_roots(workspace: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>), ProcessError> {
    let mut roots = Vec::new();
    let mut controls = Vec::new();
    // A nested working directory uses the closest real git metadata boundary.
    for ancestor in workspace.ancestors() {
        let marker = ancestor.join(".git");
        let metadata = match std::fs::symlink_metadata(&marker) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(error(format!("cannot inspect git boundary: {e}"))),
        };
        let git = if metadata.is_dir() {
            marker
        } else {
            let text = std::fs::read_to_string(&marker)
                .map_err(|_| error("cannot read git worktree marker"))?;
            let path = text
                .trim()
                .strip_prefix("gitdir: ")
                .ok_or_else(|| error("invalid git worktree marker"))?;
            ancestor.join(path)
        };
        let git = std::fs::canonicalize(&git).map_err(|_| error("cannot resolve git directory"))?;
        roots.push(git.clone());
        let common_marker = git.join("commondir");
        let common = match std::fs::read_to_string(&common_marker) {
            Ok(text) => std::fs::canonicalize(git.join(text.trim()))
                .map_err(|_| error("cannot resolve git common directory"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => git.clone(),
            Err(e) => return Err(error(format!("cannot read git common directory: {e}"))),
        };
        roots.push(common.clone());
        if !common.join("objects").is_dir() || !common.join("HEAD").is_file() {
            return Err(error(
                "git common directory is not a verified repository metadata root",
            ));
        }
        if git != common {
            if !git.starts_with(common.join("worktrees")) {
                return Err(error(
                    "linked worktree directory is outside git common worktrees",
                ));
            }
            let back = std::fs::read_to_string(git.join("gitdir"))
                .map_err(|_| error("linked worktree has no back pointer"))?;
            let back = std::fs::canonicalize(git.join(back.trim()))
                .map_err(|_| error("invalid worktree back pointer"))?;
            if back
                != std::fs::canonicalize(ancestor.join(".git"))
                    .map_err(|_| error("invalid worktree marker"))?
            {
                return Err(error(
                    "linked worktree back pointer does not match workspace",
                ));
            }
        }
        // Protect metadata selectors/config/code while allowing index, objects,
        // refs and worktree operation state required by real git commands.
        if !metadata.is_dir() {
            controls.push(ancestor.join(".git"));
        }
        for root in [git, common] {
            for name in floor::GIT_CONTROLS {
                controls.push(root.join(name));
            }
        }
        break;
    }
    Ok((roots, controls))
}
#[cfg(test)]
#[path = "adaptive_tests.rs"]
mod tests;
