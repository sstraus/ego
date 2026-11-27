//! The checkouts that share the workspace's git repository.
//!
//! One repository is one workspace: the main checkout, every linked worktree
//! and the common git directory. Membership comes only from validated git
//! metadata (`commondir`, `worktrees/<id>/gitdir` and the `.git` file that
//! points back), never from a path name.

use std::path::{Path, PathBuf};

use ego_core::permit::CanonPath;

/// Canonical roots of the repository that contains `workspace`, the
/// workspace's own checkout included. Empty outside a git working tree or when
/// the metadata is not self-consistent; an inconsistent entry is skipped, so a
/// foreign or forged pointer never widens the set. A root that would contain
/// the home directory or `/etc/ego` is skipped, as `admit_roots` does for
/// configured roots. Cheap enough to call again before each spawn.
pub fn same_repository_roots(workspace: &Path) -> Vec<CanonPath> {
    let Ok((git, _)) = super::adaptive::git_roots(workspace) else {
        return Vec::new();
    };
    let Some(common) = git.get(1) else {
        return Vec::new();
    };
    let mut roots = vec![common.clone()];
    if common.file_name().is_some_and(|name| name == ".git")
        && let Some(checkout) = common.parent()
    {
        roots.push(checkout.to_path_buf());
    }
    if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
        roots.extend(entries.flatten().filter_map(|entry| linked(&entry.path())));
    }
    roots.sort();
    roots.dedup();
    let home = std::env::var_os("HOME").and_then(|home| std::fs::canonicalize(home).ok());
    roots
        .into_iter()
        .filter(|root| {
            !home.as_ref().is_some_and(|home| home.starts_with(root))
                && !Path::new("/etc/ego").starts_with(root)
        })
        .map(CanonPath::new)
        .collect()
}

/// The checkout of one `worktrees/<id>` entry, only when its back pointer and
/// the checkout's `.git` file name each other.
fn linked(entry: &Path) -> Option<PathBuf> {
    let entry = std::fs::canonicalize(entry).ok()?;
    let marker = std::fs::canonicalize(entry_gitdir(&entry)?).ok()?;
    let text = std::fs::read_to_string(&marker).ok()?;
    let pointed = std::fs::canonicalize(text.trim().strip_prefix("gitdir: ")?).ok()?;
    (pointed == entry).then(|| marker.parent().map(Path::to_path_buf))?
}

fn entry_gitdir(entry: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(entry.join("gitdir")).ok()?;
    Some(PathBuf::from(text.trim())).filter(|path| path.is_absolute())
}

#[cfg(test)]
#[path = "same_repository_tests.rs"]
mod tests;
