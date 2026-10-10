//! One floor contract for native SBPL and ineffective-grant diagnostics.

use ego_core::permit::{AdaptivePerimeter, RunConfinement};
use std::path::Path;

pub(in crate::exec) const GIT_ENTRY: &str = "(^|/)[.][gG][iI][tT]$";
pub(in crate::exec) const GIT_METADATA: &str = "(^|/)[.][gG][iI][tT](/|$)";
pub(in crate::exec) const GIT_CONTROL: &str =
    "(^|/)[.][gG][iI][tT]/(.*/)?(config([.]worktree)?|hooks|commondir|gitdir|info/attributes)(/|$)";
pub(in crate::exec) const METADATA_CONTROL: &str =
    "/(config([.]worktree)?|hooks|commondir|gitdir|info/attributes)(/|$)";
pub(in crate::exec) const ENV: &str = "(^|/)[.]env[^/]*(/|$)";

pub(in crate::exec) fn metadata_roots(
    perimeter: &AdaptivePerimeter,
) -> impl Iterator<Item = &Path> {
    // Captured Git roots carry both controls, including absent paths. Plain
    // read-only grants do not become a new metadata-root source.
    perimeter.controls.iter().filter_map(|control| {
        let path = control.as_path();
        let root = path.parent()?;
        (path.file_name().is_some_and(|name| name == "config")
            && perimeter
                .controls
                .iter()
                .any(|other| other.as_path() == root.join("hooks")))
        .then_some(root)
    })
}

pub(in crate::exec) fn blocks(path: &Path, policy: &RunConfinement) -> bool {
    let Some(perimeter) = policy.adaptive_perimeter() else {
        return false;
    };
    let text = path.to_string_lossy();
    let matches = |pattern| {
        regex::Regex::new(pattern)
            .expect("fixed floor pattern")
            .is_match(&text)
    };
    perimeter
        .credentials
        .iter()
        .chain(&perimeter.controls)
        .any(|floor| path.starts_with(floor.as_path()))
        || matches(GIT_ENTRY)
        || matches(GIT_CONTROL)
        || metadata_roots(perimeter).any(|root| path.starts_with(root) && matches(METADATA_CONTROL))
        || (path.is_dir()
            && (matches(GIT_METADATA)
                || metadata_roots(perimeter).any(|root| path.starts_with(root))))
        || (!path.starts_with(policy.workspace().as_path()) && matches(ENV))
}
