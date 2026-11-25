//! Reach that ordinary development work needs and the operator already owns.
//!
//! Derived once, from files the operator placed (the mbx shim and config, SSH
//! public keys, the `tuic` binary), never from a model call. The Seatbelt
//! backend turns it into grants beside the pnpm roots. Private keys and
//! credentials are not here: their floors stay.

use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DevRoots {
    /// Run and read: the cargo shim directory, the `tuic` binary.
    pub(super) exec: Vec<PathBuf>,
    /// Read only: the mbx configuration, SSH public keys and signers file.
    pub(super) read: Vec<PathBuf>,
    /// Read and write: the mbx cache and its managed target directories.
    pub(super) write: Vec<PathBuf>,
}

/// Where the operator keeps the Rust toolchain: `CARGO_HOME` and `RUSTUP_HOME`,
/// else the defaults under the home directory.
pub(super) struct Toolchain {
    pub(super) cargo: PathBuf,
    pub(super) rustup: PathBuf,
}

/// `tuic` is the resolved file of the CLI on `PATH`, only for a session that
/// TUICommander manages.
pub(super) fn derive(home: &Path, toolchain: &Toolchain, tuic: Option<&Path>) -> DevRoots {
    let mut roots = DevRoots::default();
    // Cargo and rustc run from here; the registry and git caches are what a
    // build reads and locks. `bin`, `env`, `credentials*` stay as they are.
    roots.exec.extend(
        [
            toolchain.cargo.join("bin"),
            toolchain.rustup.join("toolchains"),
        ]
        .iter()
        .filter_map(|path| canonical_dir(path)),
    );
    roots.read.extend(canonical_dir(&toolchain.rustup));
    roots.read.extend(
        ["config.toml", "config"]
            .iter()
            .filter_map(|name| canonical_file(&toolchain.cargo.join(name))),
    );
    for name in ["registry", "git"] {
        roots
            .write
            .extend(canonical_dir(&toolchain.cargo.join(name)));
    }
    roots.write.extend(
        [".package-cache", ".package-cache-mutate", ".global-cache"]
            .iter()
            .filter_map(|name| canonical_file(&toolchain.cargo.join(name))),
    );
    let mbx = home.join("Library/Application Support/mbx");
    if let Some(shims) = canonical_dir(&mbx.join("bin")) {
        roots.exec.push(shims);
        roots.read.extend(canonical_dir(&mbx));
        roots.write.extend(mbx_cache(home, &mbx));
    }
    // Apple build tooling: Xcode products and SwiftPM caches. The per-user
    // temporary directory stays closed: the capability probes use it as the
    // outside location that a write must not reach.
    roots.write.extend(
        [
            "Library/Developer/Xcode/DerivedData",
            "Library/org.swift.swiftpm",
            "Library/Caches/org.swift.swiftpm",
            ".swiftpm",
        ]
        .iter()
        .filter_map(|path| canonical_dir(&home.join(path))),
    );
    let ssh = home.join(".ssh");
    if let Ok(entries) = std::fs::read_dir(&ssh) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "allowed_signers" || name.ends_with(".pub"))
            })
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        roots.read.extend(files);
    }
    roots.exec.extend(
        tuic.and_then(|path| std::fs::canonicalize(path).ok())
            .filter(|path| path.is_file()),
    );
    roots
}

fn canonical_file(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path)
        .ok()
        .filter(|path| path.is_file())
}

fn canonical_dir(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path)
        .ok()
        .filter(|path| path.is_dir())
}

/// `cache_dir` of the mbx configuration, else its documented default.
fn mbx_cache(home: &Path, mbx: &Path) -> Option<PathBuf> {
    let configured = std::fs::read_to_string(mbx.join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| doc.get("cache_dir")?.as_str().map(PathBuf::from))
        .filter(|path| path.is_absolute());
    canonical_dir(&configured.unwrap_or_else(|| home.join("Library/Caches/mbx")))
        .filter(|path| !home.starts_with(path))
}

#[cfg(test)]
#[path = "dev_roots_tests.rs"]
mod tests;
