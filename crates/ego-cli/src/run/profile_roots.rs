//! Admission of the active profile's extra roots at the process edge.

use ego_core::config::Effective;
use ego_core::permit::{CanonPath, PathResolver, ProfileRoot, admit_roots};
use ego_tools::fs::OsFs;

/// Resolve the roots the active profile declared against the real filesystem.
/// Any refusal is a startup failure: a profile that asks for more reach than
/// it may have must not quietly run with less.
pub(crate) fn admit(
    effective: &Effective,
    workspace: &CanonPath,
) -> Result<Vec<ProfileRoot>, String> {
    if effective.roots.is_empty() && effective.root_ceilings.is_empty() {
        return Ok(Vec::new());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        "profile roots need HOME to resolve `~` and to keep it out of reach".to_owned()
    })?;
    let home = OsFs
        .canonicalize(std::path::Path::new(&home))
        .map_err(|error| format!("HOME cannot be resolved: {error}"))?;
    let roots = admit_roots(&effective.roots, &home, &OsFs).map_err(|error| error.to_string())?;
    let mut extra = roots.clone();
    for path in &effective.exec_paths {
        extra.push(ProfileRoot::new(
            OsFs.canonicalize(path).map_err(|error| error.to_string())?,
            ego_core::permit::RootAccess::Read,
        ));
    }
    ego_core::permit::check_root_ceilings(
        &extra,
        &effective.root_ceilings,
        &home,
        workspace,
        &OsFs,
    )
    .map_err(|error| error.to_string())?;
    Ok(roots)
}
