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
    let refuse = |reason: &str| UsageError(format!("read-only directory: {reason}"));
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
        path: canonical,
        access: RootAccess::Read,
    });
    super::profile_roots::admit(&candidate, &ego_core::permit::CanonPath::new(workspace))
        .map_err(|error| refuse(&error))?;
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
