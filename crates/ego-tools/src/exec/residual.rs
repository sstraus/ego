//! The executor's half of a residual-write decision: bounded evidence about
//! one refused path, and the floor recheck of whatever the judge returns.
//!
//! Everything here is read from the host filesystem by ego, not reported by
//! the child. Names and sizes only; contents are never read.

use std::path::Path;

use ego_core::permit::residual::{
    Admissible, ResidualEntry, ResidualListing, ResidualPathKind, ResidualWriteEvidence,
};
use ego_core::permit::{CanonPath, RunConfinement, SandboxGrant};

use super::denial::Denial;

/// Entries one listing may name.
const LISTING_ENTRIES: usize = 32;
/// Bytes of the refusal line the judge may read.
const REFUSAL_BYTES: usize = 2048;
/// Roots outside home whose writes change the machine, not one project.
const SYSTEM_ROOTS: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/bin",
    "/sbin",
    "/usr",
    "/opt",
    "/etc",
    "/private/etc",
    "/private/var/db",
    "/private/var/root",
];

/// The facts the judge reads about `request`, which `denial` found in the
/// stderr of a finished child.
pub(super) fn evidence(
    denial: &Denial,
    request: &SandboxGrant,
    stderr: &str,
    confinement: &RunConfinement,
    judged: &[CanonPath],
) -> ResidualWriteEvidence {
    let candidate = request.path.clone();
    let path = candidate.as_path();
    let exists = std::fs::symlink_metadata(path).is_ok();
    let home = confinement
        .adaptive_perimeter()
        .map(|perimeter| perimeter.home.clone());
    let mut writable = vec![confinement.workspace().clone()];
    if let Some(perimeter) = confinement.adaptive_perimeter() {
        writable.extend(perimeter.writable.iter().cloned());
    }
    writable.extend(judged.iter().cloned());
    ResidualWriteEvidence {
        requested: denial.printed.clone(),
        kind: kind(path, home.as_ref().map(CanonPath::as_path)),
        owned_by_workspace_user: owned(path, confinement.workspace().as_path()),
        listing: listing(if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        }),
        refusal: refusal(stderr, &denial.printed),
        candidate,
        exists,
        writable,
    }
}

/// The executor-side recheck of a path the judge allowed: the floors, the
/// breadth rule and the canonical identity still hold at answer time.
pub(super) fn admissible(confinement: RunConfinement) -> Admissible {
    Box::new(move |path: &CanonPath| {
        if super::adaptive::floor::blocks(path.as_path(), &confinement) {
            return Err("the path is under a protected floor".to_owned());
        }
        let home = confinement
            .adaptive_perimeter()
            .map(|perimeter| perimeter.home.as_path());
        if kind(path.as_path(), home) == ResidualPathKind::SystemDirectory {
            return Err("the path is a system directory".to_owned());
        }
        if path.is_or_under(confinement.workspace()) || super::denial::too_broad(path, &confinement)
        {
            return Err("the path is the workspace, above it, or too broad".to_owned());
        }
        if !super::denial::renderable(path.as_path()) {
            return Err("the sandbox cannot express the path".to_owned());
        }
        match super::denial::canonical(path.as_path()) {
            Some((resolved, _)) if resolved == *path => Ok(()),
            _ => Err("the path no longer resolves to the decided location".to_owned()),
        }
    })
}

fn kind(path: &Path, home: Option<&Path>) -> ResidualPathKind {
    if let Some(relative) = home.and_then(|home| path.strip_prefix(home).ok()) {
        if relative
            .components()
            .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
        {
            return ResidualPathKind::Dotfile;
        }
        let home = home.expect("a relative path came from home");
        if path
            .ancestors()
            .skip(1)
            .take_while(|ancestor| *ancestor != home)
            .any(|ancestor| ancestor.join(".git").exists())
        {
            return ResidualPathKind::OtherRepository;
        }
        return ResidualPathKind::Other;
    }
    if SYSTEM_ROOTS
        .iter()
        .any(|root| path.starts_with(Path::new(root)))
    {
        return ResidualPathKind::SystemDirectory;
    }
    ResidualPathKind::Other
}

#[cfg(unix)]
fn owned(path: &Path, workspace: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let owner = std::fs::metadata(workspace).ok()?.uid();
    let existing = path
        .ancestors()
        .find_map(|ancestor| std::fs::symlink_metadata(ancestor).ok())?;
    Some(existing.uid() == owner)
}

#[cfg(not(unix))]
fn owned(_: &Path, _: &Path) -> Option<bool> {
    None
}

fn listing(directory: &Path) -> ResidualListing {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return ResidualListing::default();
    };
    let mut entries = entries
        .filter_map(Result::ok)
        .take(LISTING_ENTRIES + 1)
        .map(|entry| {
            let metadata = entry.metadata().ok();
            let kind = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => "symlink",
                Ok(kind) if kind.is_dir() => "dir",
                Ok(kind) if kind.is_file() => "file",
                _ => "other",
            };
            ResidualEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                bytes: metadata.map_or(0, |metadata| metadata.len()),
                kind,
            }
        })
        .collect::<Vec<_>>();
    let complete = entries.len() <= LISTING_ENTRIES;
    entries.truncate(LISTING_ENTRIES);
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    ResidualListing { complete, entries }
}

fn refusal(stderr: &str, printed: &str) -> String {
    let line = stderr
        .lines()
        .find(|line| line.contains(printed))
        .unwrap_or(printed);
    let mut end = line.len().min(REFUSAL_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}
