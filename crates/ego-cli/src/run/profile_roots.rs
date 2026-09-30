//! Admission of the active profile's extra roots at the process edge.

use ego_core::config::Effective;
use ego_core::permit::{CanonPath, PathResolver, ProfileRoot, admit_roots};
use ego_tools::fs::OsFs;

/// Resolve the roots the active profile declared against the real filesystem.
/// Any refusal is a startup failure: a profile that asks for more reach than
/// it may have must not quietly run with less.
pub(super) fn admit(effective: &Effective) -> Result<Vec<ProfileRoot>, String> {
    if effective.roots.is_empty() {
        return Ok(Vec::new());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        "profile roots need HOME to resolve `~` and to keep it out of reach".to_owned()
    })?;
    let home = OsFs
        .canonicalize(std::path::Path::new(&home))
        .map_err(|error| format!("HOME cannot be resolved: {error}"))?;
    admit_roots(&effective.roots, &CanonPath::new(home.as_path()), &OsFs)
        .map_err(|error| error.to_string())
}
