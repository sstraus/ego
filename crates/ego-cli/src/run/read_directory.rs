//! Launch-owned read authority, separate from configuration-layer widening.

use std::path::{Component, Path};

use ego_core::config::Effective;
use ego_core::permit::{RootAccess, RootCfg};

use crate::cli::UsageError;

/// Admit exactly the named directory, without resolving an alias to broader reach.
pub(crate) fn apply(
    effective: &mut Effective,
    workspace: &Path,
    directory: &Path,
) -> Result<(), UsageError> {
    admit_directory(
        effective,
        workspace,
        directory,
        RootAccess::Read,
        "read-only directory",
    )
}

/// One `--allow-dir`: the same exact-directory rule with the access the flag names.
pub(crate) fn apply_allow(
    effective: &mut Effective,
    workspace: &Path,
    directory: &Path,
    access: RootAccess,
) -> Result<(), UsageError> {
    admit_directory(effective, workspace, directory, access, "--allow-dir")
}

fn admit_directory(
    effective: &mut Effective,
    workspace: &Path,
    directory: &Path,
    access: RootAccess,
    label: &str,
) -> Result<(), UsageError> {
    let refuse = |reason: &str| UsageError(format!("{label}: {reason}"));
    if !directory.is_absolute()
        || directory
            .components()
            .any(|part| part == Component::ParentDir)
    {
        return Err(refuse("requires an absolute path without .."));
    }
    let canonical = std::fs::canonicalize(directory)
        .map_err(|error| refuse(&format!("cannot resolve {}: {error}", directory.display())))?;
    if canonical != directory || !canonical.is_dir() {
        return Err(refuse(
            "requires an existing canonical directory, without symlink aliases",
        ));
    }
    let mut candidate = effective.clone();
    candidate.roots.push(RootCfg {
        consent: None,
        path: canonical.clone(),
        access,
    });
    // An explicit request, unlike a stale configured entry, is refused when it
    // cannot be admitted: the host asked for this exact root.
    let (admitted, notes) =
        super::profile_roots::admit_noted(&candidate, &ego_core::permit::CanonPath::new(workspace))
            .map_err(|error| refuse(&error))?;
    if !admitted
        .iter()
        .any(|root| root.path().as_path() == canonical && root.access() == access)
    {
        return Err(refuse(notes.last().map_or("not admitted", String::as_str)));
    }
    *effective = candidate;
    Ok(())
}

/// Check the same selected profile and host ceiling used by run preparation.
pub(crate) fn validate_session(
    root: &ego_core::run::RunRoot,
    args: &crate::cli::Args,
) -> Result<(), UsageError> {
    session_policy(root, args).map(|_| ())
}

pub(crate) fn session_policy(
    root: &ego_core::run::RunRoot,
    args: &crate::cli::Args,
) -> Result<ego_core::permit::RunConfinement, UsageError> {
    super::composition::rooted_settings(
        args,
        root.as_path().to_path_buf(),
        |_| ego_core::config::Trust::Untrusted,
        &crate::output::Stderr,
    )
    .map(|settings| settings.confinement)
}
