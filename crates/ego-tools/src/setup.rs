//! Bounded native setup reads. No command, writer, hook or transport is owned.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ego_core::permit::CanonPath;
use ego_core::setup::{Observation, ProbeReader};

pub struct NativeProbeReader {
    path: Vec<PathBuf>,
}

impl NativeProbeReader {
    pub fn capture() -> Self {
        Self {
            path: std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default(),
        }
    }
}

fn identity(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime_nsec()
        )
    }
    #[cfg(not(unix))]
    {
        format!("{}:{:?}", metadata.len(), metadata.modified().ok())
    }
}

fn observe(
    path: &Path,
    canonical: PathBuf,
    metadata: &fs::Metadata,
    bytes: Vec<u8>,
    truncated: bool,
) -> Observation {
    Observation {
        requested: path.to_path_buf(),
        canonical: CanonPath::new(canonical),
        regular: metadata.is_file(),
        bytes,
        truncated,
        identity: identity(metadata),
    }
}

#[cfg(unix)]
fn metadata_no_links(path: &Path) -> Result<fs::Metadata, String> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    let mut directory = fs::File::open("/").map_err(|error| error.to_string())?;
    let parts = path
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (index, name) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        let mut flags = OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
        #[cfg(target_os = "linux")]
        {
            flags |= OFlag::O_PATH;
        }
        #[cfg(target_os = "macos")]
        {
            flags |= OFlag::from_bits_retain(nix::libc::O_EVTONLY);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            return Err("race-safe setup metadata is unsupported on this platform".into());
        }
        if !last {
            flags |= OFlag::O_DIRECTORY;
        }
        let file = fs::File::from(
            openat(&directory, Path::new(name), flags, Mode::empty())
                .map_err(|error| format!("probe no-link metadata failed: {error}"))?,
        );
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err("probe metadata path became a symlink".into());
        }
        if last {
            return Ok(metadata);
        }
        directory = file;
    }
    directory.metadata().map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn metadata_no_links(_path: &Path) -> Result<fs::Metadata, String> {
    Err("race-safe setup metadata is unsupported on this platform".into())
}

#[cfg(unix)]
fn absent_no_links(path: &Path) -> Result<bool, String> {
    use nix::fcntl::{AtFlags, OFlag, openat};
    use nix::sys::stat::{Mode, fstatat};
    let parent = path.parent().ok_or("absence needs an existing parent")?;
    let name = path.file_name().ok_or("absence needs an ordinary leaf")?;
    let mut directory = fs::File::open("/").map_err(|error| error.to_string())?;
    for part in parent.components() {
        if let std::path::Component::Normal(name) = part {
            directory = fs::File::from(
                openat(
                    &directory,
                    Path::new(name),
                    OFlag::O_RDONLY
                        | OFlag::O_DIRECTORY
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC
                        | OFlag::O_NONBLOCK,
                    Mode::empty(),
                )
                .map_err(|error| error.to_string())?,
            );
        }
    }
    match fstatat(&directory, Path::new(name), AtFlags::AT_SYMLINK_NOFOLLOW) {
        Err(nix::errno::Errno::ENOENT) => Ok(true),
        Ok(_) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(unix))]
fn absent_no_links(_path: &Path) -> Result<bool, String> {
    Err("race-safe setup absence is unsupported on this platform".into())
}

#[cfg(unix)]
fn link_target_no_links(path: &Path) -> Result<Option<PathBuf>, String> {
    use nix::fcntl::AtFlags;
    use nix::fcntl::{OFlag, openat, readlinkat};
    use nix::sys::stat::{Mode, SFlag, fstatat};
    let mut directory = fs::File::open("/").map_err(|error| error.to_string())?;
    let parts = path
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (index, name) in parts.iter().enumerate() {
        let name = Path::new(name);
        if index + 1 == parts.len() {
            let metadata = fstatat(&directory, name, AtFlags::AT_SYMLINK_NOFOLLOW)
                .map_err(|error| error.to_string())?;
            if SFlag::from_bits_truncate(metadata.st_mode).contains(SFlag::S_IFLNK) {
                return readlinkat(&directory, name)
                    .map(|target| Some(PathBuf::from(target)))
                    .map_err(|error| error.to_string());
            }
            return Ok(None);
        }
        directory = fs::File::from(
            openat(
                &directory,
                name,
                OFlag::O_RDONLY
                    | OFlag::O_DIRECTORY
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC
                    | OFlag::O_NONBLOCK,
                Mode::empty(),
            )
            .map_err(|error| error.to_string())?,
        );
    }
    Ok(None)
}

#[cfg(not(unix))]
fn link_target_no_links(_path: &Path) -> Result<Option<PathBuf>, String> {
    Err("race-safe setup metadata is unsupported on this platform".into())
}

fn diagnostic_path(path: &Path) -> Result<PathBuf, String> {
    use std::collections::VecDeque;
    use std::path::Component;
    let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
    let mut pending = absolute
        .components()
        .map(|part| part.as_os_str().to_os_string())
        .collect::<VecDeque<_>>();
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == ".." {
            resolved.pop();
            continue;
        }
        if part == "." {
            continue;
        }
        resolved.push(&part);
        let mut destination = resolved.clone();
        for component in &pending {
            destination.push(component);
        }
        if ego_core::config::Ruleset::is_credential_path(&CanonPath::new(&resolved))
            || ego_core::config::Ruleset::is_credential_path(&CanonPath::new(destination))
        {
            return Err("credential diagnostic access is forbidden".into());
        }
        if let Some(target) = link_target_no_links(&resolved)? {
            links += 1;
            if links > 32 {
                return Err("diagnostic symlink limit exceeded".into());
            }
            resolved.pop();
            if target.is_absolute() {
                resolved.clear();
            }
            for part in target.components().rev() {
                match part {
                    Component::CurDir => {}
                    _ => pending.push_front(part.as_os_str().to_os_string()),
                }
            }
        }
    }
    Ok(resolved)
}

#[cfg(unix)]
fn open_regular_no_links(path: &Path) -> Result<fs::File, String> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    let mut directory = fs::File::open("/").map_err(|error| error.to_string())?;
    let parts = path
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return Err("probe read requires a regular file".into());
    }
    for (index, name) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        let mut flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
        if !last {
            flags |= OFlag::O_DIRECTORY;
        }
        let file = fs::File::from(
            openat(&directory, Path::new(name), flags, Mode::empty())
                .map_err(|error| format!("probe no-link open failed: {error}"))?,
        );
        if last {
            return Ok(file);
        }
        directory = file;
    }
    Err("probe read requires a regular file".into())
}

#[cfg(not(unix))]
fn open_regular_no_links(_path: &Path) -> Result<fs::File, String> {
    Err("race-safe setup content reads are unsupported on this platform".into())
}

impl ProbeReader for NativeProbeReader {
    fn metadata(&self, path: &Path) -> Result<Observation, String> {
        let canonical = diagnostic_path(path)?;
        let metadata = metadata_no_links(&canonical)?;
        Ok(observe(path, canonical, &metadata, Vec::new(), false))
    }

    fn write_metadata(&self, path: &Path) -> Result<Observation, String> {
        match self.metadata(path) {
            Ok(observation) => Ok(observation),
            Err(error) => {
                // Only one absent ordinary leaf is representable. Never treat
                // a dangling symlink or inaccessible component as absence.
                let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
                if ego_core::config::Ruleset::is_credential_path(&CanonPath::new(&absolute)) {
                    return Err("credential diagnostic access is forbidden".into());
                }
                let parent = absolute.parent().ok_or("write target has no parent")?;
                let canonical_parent = diagnostic_path(parent)?;
                let name = absolute
                    .file_name()
                    .ok_or("write target needs an ordinary leaf")?;
                let selected = canonical_parent.join(name);
                if !absent_no_links(&selected)? {
                    return Err(error);
                }
                let parent = absolute.parent().ok_or("write target has no parent")?;
                let ancestor = self.metadata(parent)?;
                if ancestor.regular {
                    return Err("write target parent is not a directory".into());
                }
                let name = absolute
                    .file_name()
                    .ok_or("write target needs an ordinary leaf")?;
                let canonical = ancestor.canonical.as_path().join(name);
                if ego_core::config::Ruleset::is_credential_path(&CanonPath::new(&canonical)) {
                    return Err("credential diagnostic access is forbidden".into());
                }
                if !absent_no_links(&canonical)? {
                    return Err("write target identity changed".into());
                }
                Ok(Observation {
                    requested: path.to_path_buf(),
                    canonical: CanonPath::new(canonical),
                    regular: false,
                    bytes: Vec::new(),
                    truncated: false,
                    identity: format!("absent-leaf:{}", ancestor.identity),
                })
            }
        }
    }

    fn read(&self, canonical: &CanonPath, max_bytes: usize) -> Result<Observation, String> {
        let resolved = diagnostic_path(canonical.as_path())?;
        if resolved != canonical.as_path() {
            return Err("probe canonical path changed".into());
        }
        let path = resolved.as_path();
        let before = metadata_no_links(path)?;
        if !before.is_file() {
            return Err("probe read requires a regular file".into());
        }
        let file = open_regular_no_links(path)?;
        let opened = file
            .metadata()
            .map_err(|error| format!("probe metadata failed: {error}"))?;
        if !opened.is_file() || identity(&opened) != identity(&before) {
            return Err("probe file identity changed before open".into());
        }
        let mut bytes = Vec::new();
        file.take((max_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("probe read failed: {error}"))?;
        let truncated = bytes.len() > max_bytes;
        bytes.truncate(max_bytes);
        Ok(observe(path, path.to_path_buf(), &opened, bytes, truncated))
    }

    fn tool(&self, name: &str) -> Result<Observation, String> {
        if name.is_empty() || name.contains(['/', '\\']) {
            return Err("tool probe needs one PATH name".into());
        }
        for directory in self.path.iter().take(128) {
            let target = directory.join(name);
            if let Ok(observation) = self.metadata(&target) {
                if !observation.regular {
                    continue;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata_no_links(observation.canonical.as_path())
                        .map_err(|error| error.to_string())?
                        .permissions()
                        .mode()
                        & 0o111
                        == 0
                    {
                        continue;
                    }
                }
                return Ok(observation);
            }
        }
        Err("requested tool is missing from bounded operator PATH inventory".into())
    }
}

#[cfg(all(test, unix))]
mod metadata_tests {
    // Catches: an absent-leaf check traverses a replaced ancestor and accepts credential absence.
    #[test]
    fn setup_absence_refuses_replaced_credential_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join(".ssh");
        std::fs::create_dir(&secret).unwrap();
        let alias = root.path().join("safe");
        std::os::unix::fs::symlink(&secret, &alias).unwrap();
        assert!(
            super::absent_no_links(&alias.join("missing")).is_err(),
            "absence check traversed credential ancestor alias"
        );
    }

    // Catches: metadata follows a replaced credential alias after diagnostic resolution.
    #[test]
    fn setup_metadata_refuses_replaced_leaf_and_ancestor_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let safe = root.path().join("safe");
        let secret = root.path().join(".ssh");
        std::fs::create_dir(&safe).unwrap();
        std::fs::create_dir(&secret).unwrap();
        std::fs::write(secret.join("id_fixture"), "synthetic").unwrap();
        let target = safe.join("target");
        std::os::unix::fs::symlink(secret.join("id_fixture"), &target).unwrap();
        assert!(
            super::metadata_no_links(&target).is_err(),
            "metadata followed replaced leaf alias"
        );
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_dir(&safe).unwrap();
        std::os::unix::fs::symlink(&secret, &safe).unwrap();
        assert!(
            super::metadata_no_links(&safe.join("id_fixture")).is_err(),
            "metadata followed replaced ancestor alias"
        );
    }
}

/// Expand selected instruction/Git selectors through bounded native reads only.
/// The caller still classifies every result under the target policy.
pub fn inventory(workspace: &Path) -> Vec<ego_core::setup::Probe> {
    inventory_admitted(workspace, &|path| path.as_path().starts_with(workspace))
}

pub fn inventory_admitted(
    workspace: &Path,
    content_admitted: &dyn Fn(&CanonPath) -> bool,
) -> Vec<ego_core::setup::Probe> {
    use ego_core::setup::Probe;
    let reader = NativeProbeReader::capture();
    let mut probes = Vec::new();
    let mut queue = std::collections::VecDeque::from([
        (workspace.join("AGENTS.md"), 0usize),
        (workspace.join("CLAUDE.md"), 0),
    ]);
    let mut visited = std::collections::HashSet::new();
    while let Some((path, depth)) = queue.pop_front() {
        if probes.len() >= 32 {
            break;
        }
        probes.push(Probe::Instructions {
            path: path.clone(),
            reason: format!("instruction reference depth {depth}"),
        });
        let Ok(metadata) = reader.metadata(&path) else {
            continue;
        };
        if depth >= 8
            || !content_admitted(&metadata.canonical)
            || !metadata.regular
            || !metadata.canonical.as_path().starts_with(workspace)
            || !visited.insert(metadata.canonical.clone())
        {
            continue;
        }
        let Ok(content) = reader.read(&metadata.canonical, 64 * 1024) else {
            continue;
        };
        if content.truncated {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&content.bytes) else {
            continue;
        };
        for reference in ego_core::config::instruction_references(text)
            .into_iter()
            .take(16)
        {
            if reference.is_empty() {
                continue;
            }
            let requested = PathBuf::from(&reference);
            let target = if requested.is_absolute() {
                requested
            } else {
                path.parent().unwrap_or(workspace).join(requested)
            };
            queue.push_back((target, depth + 1));
        }
    }
    let marker = workspace.join(".git");
    probes.push(Probe::Metadata {
        path: marker.clone(),
        reason: "Git workspace selector".into(),
    });
    if let Ok(observation) = reader.metadata(&marker) {
        let git = if observation.regular && content_admitted(&observation.canonical) {
            reader
                .read(&observation.canonical, 4096)
                .ok()
                .filter(|read| !read.truncated)
                .and_then(|read| String::from_utf8(read.bytes).ok())
                .and_then(|text| {
                    text.trim()
                        .strip_prefix("gitdir: ")
                        .map(|path| workspace.join(path))
                })
        } else if !observation.regular {
            Some(marker)
        } else {
            None
        };
        if let Some(git) = git {
            probes.push(Probe::Metadata {
                path: git.clone(),
                reason: "Git operational metadata".into(),
            });
            let common_marker = git.join("commondir");
            probes.push(Probe::Read {
                path: common_marker.clone(),
                reason: "linked Git common-dir selector".into(),
            });
            let common = reader
                .metadata(&common_marker)
                .ok()
                .filter(|metadata| content_admitted(&metadata.canonical))
                .and_then(|metadata| reader.read(&metadata.canonical, 4096).ok())
                .filter(|read| !read.truncated)
                .and_then(|read| String::from_utf8(read.bytes).ok())
                .map(|text| git.join(text.trim()))
                .unwrap_or_else(|| git.clone());
            for name in ["objects", "refs", "HEAD", "config", "hooks"] {
                probes.push(Probe::Metadata {
                    path: common.join(name),
                    reason: "Git common metadata/control (not executed)".into(),
                });
            }
        }
    }
    for name in ["git", "cargo", "rustc", "node", "pnpm", "xcodebuild"] {
        probes.push(Probe::Tool {
            name: name.into(),
            reason: "operator PATH inventory; health/exec not proven".into(),
        });
    }
    for key in [
        "DEVELOPER_DIR",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "TMPDIR",
        "PNPM_HOME",
        "npm_config_cache",
    ] {
        if let Some(path) = std::env::var_os(key) {
            probes.push(Probe::Metadata {
                path: PathBuf::from(path),
                reason: format!("operator environment {key}; presence only"),
            });
        }
    }
    // Derived sources are operator-owned paths, not model-selected authority.
    // Keep the catalogue finite and inspect presence only; never create caches.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let cargo = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cargo"));
        let rustup = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".rustup"));
        let source = |key: &str| {
            if std::env::var_os(key).is_some() {
                format!("operator {key}")
            } else {
                "operator HOME default".into()
            }
        };
        for (path, reason) in [
            (
                cargo.join("registry"),
                format!("derived Cargo registry cache ({})", source("CARGO_HOME")),
            ),
            (
                cargo.join("git"),
                format!("derived Cargo Git cache ({})", source("CARGO_HOME")),
            ),
            (
                rustup.join("downloads"),
                format!("derived rustup downloads ({})", source("RUSTUP_HOME")),
            ),
            (
                rustup.join("tmp"),
                format!("derived rustup temporary cache ({})", source("RUSTUP_HOME")),
            ),
        ] {
            probes.push(Probe::Metadata { path, reason });
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let configured = |key: &str| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let pnpm = configured("PNPM_HOME")
            .or_else(|| configured("XDG_DATA_HOME").map(|path| path.join("pnpm")))
            .unwrap_or_else(|| home.join("Library/pnpm"));
        let caches = configured("XDG_CACHE_HOME").unwrap_or_else(|| home.join("Library/Caches"));
        let npm = configured("npm_config_cache")
            .or_else(|| configured("NPM_CONFIG_CACHE"))
            .unwrap_or_else(|| home.join(".npm"));
        for (path, reason) in [
            (
                pnpm.join("store"),
                "derived pnpm content store (operator PNPM_HOME/XDG_DATA_HOME/HOME); links stay protected",
            ),
            (
                caches.join("pnpm"),
                "derived pnpm cache (operator XDG_CACHE_HOME/HOME); dlx stays protected",
            ),
            (
                npm.join("_cacache"),
                "derived npm content cache (operator npm_config_cache/NPM_CONFIG_CACHE/HOME)",
            ),
            (
                npm.join("_logs"),
                "derived npm log cache (operator npm_config_cache/NPM_CONFIG_CACHE/HOME)",
            ),
        ] {
            probes.push(Probe::Metadata {
                path,
                reason: reason.into(),
            });
        }
        let config = home.join("Library/Application Support/mbx/config.toml");
        probes.push(Probe::Metadata {
            path: config.clone(),
            reason: "operator mbx cache_dir source; bounded TOML only, no command".into(),
        });
        if let Ok(metadata) = reader.metadata(&config)
            && let Ok(content) = reader.read(&metadata.canonical, 64 * 1024)
            && !content.truncated
            && let Ok(text) = std::str::from_utf8(&content.bytes)
            && let Ok(document) = text.parse::<toml_edit::DocumentMut>()
            && let Some(cache) = document.get("cache_dir").and_then(|value| value.as_str())
        {
            let path = cache
                .strip_prefix("~/")
                .map_or_else(|| PathBuf::from(cache), |suffix| home.join(suffix));
            if path.is_absolute()
                && !path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
            {
                probes.push(Probe::Metadata {
                    path,
                    reason: "derived mbx cache_dir from bounded operator TOML; presence only"
                        .into(),
                });
            }
        }
    }
    #[cfg(target_os = "macos")]
    probes.push(Probe::Metadata { path: std::env::var_os("DEVELOPER_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/db/xcode_select_link")), reason: "operator/platform developer selection; inspect selector only, never xcode-select execution".into() });
    probes.truncate(64);
    probes
}
