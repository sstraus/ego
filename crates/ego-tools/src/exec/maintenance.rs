//! Exact owner file maintenance; never directory or toolchain authority.
use std::path::Path;
use std::sync::LazyLock;

use ego_core::permit::{AdaptivePerimeter, RunConfinement, SandboxAccess, SandboxGrant};

use super::adaptive::floor;

/// Candidate for an explicit owner Write question, not an automatic judge grant.
pub(super) fn target(path: &Path, workspace: &Path, perimeter: &AdaptivePerimeter) -> bool {
    // A missing file can be created, but a directory, symlink or special file
    // cannot become a maintenance root. Canonical identity is checked by readmit.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && path.parent().is_some_and(Path::is_dir) => {}
        _ => return false,
    }
    static ENV: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(floor::ENV).expect("fixed environment floor"));
    if perimeter
        .credentials
        .iter()
        .any(|root| path.starts_with(root.as_path()))
        || (!path.starts_with(workspace) && ENV.is_match(&path.to_string_lossy()))
    {
        return false;
    }
    static CONTROL: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(&format!("^{}", floor::metadata_control())).expect("fixed Git floor")
    });
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let metadata = floor::metadata_roots(perimeter).collect::<Vec<_>>();
    let git = name.eq_ignore_ascii_case(".git")
        || metadata.iter().any(|root| {
            path.strip_prefix(root)
                .is_ok_and(|relative| CONTROL.is_match(&format!("/{}", relative.to_string_lossy())))
        });
    // Other captured controls include installed toolchains and package launchers.
    // Even an instruction-looking name there cannot turn maintenance into install.
    if perimeter.controls.iter().any(|control| {
        path.starts_with(control.as_path())
            && !metadata
                .iter()
                .any(|root| control.as_path().starts_with(root))
    }) {
        return false;
    }
    let operator = path
        .strip_prefix(perimeter.home.as_path())
        .is_ok_and(|relative| {
            let directory = relative
                .components()
                .next()
                .map(|part| part.as_os_str().to_string_lossy().to_ascii_lowercase());
            matches!(
                directory.as_deref(),
                Some(".codex" | ".claude" | ".claude-private")
            ) && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        });
    git || operator
        || name.eq_ignore_ascii_case("AGENTS.md")
        || name.eq_ignore_ascii_case("CLAUDE.md")
}

pub(super) fn request(grant: &SandboxGrant, policy: &RunConfinement) -> bool {
    grant.access == SandboxAccess::Write
        && policy.adaptive_perimeter().is_some_and(|perimeter| {
            target(
                grant.path.as_path(),
                policy.workspace().as_path(),
                perimeter,
            )
        })
}
