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

/// Drop canonical extra roots that the host ceiling does not admit.
pub(crate) fn clamp(
    effective: &mut Effective,
    ceiling: &Effective,
    workspace: &std::path::Path,
) -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("profile ceilings require HOME")?;
    let home = OsFs
        .canonicalize(std::path::Path::new(&home))
        .map_err(|error| error.to_string())?;
    let workspace = OsFs
        .canonicalize(workspace)
        .map_err(|error| error.to_string())?;
    let mut bounds = ceiling.roots.clone();
    let roots = admit_roots(&ceiling.roots, &home, &OsFs).map_err(|error| error.to_string())?;
    for path in &ceiling.exec_paths {
        let path = OsFs.canonicalize(path).map_err(|error| error.to_string())?;
        if !roots.iter().any(|root| path.is_or_under(root.path())) {
            bounds.push(ego_core::permit::RootCfg {
                path: path.as_path().to_path_buf(),
                access: ego_core::permit::RootAccess::Read,
            });
        }
    }
    // Validate the whole ceiling first, even when the request has no roots.
    admit_roots(&bounds, &home, &OsFs).map_err(|error| error.to_string())?;
    let ceilings = vec![bounds];
    let mut kept = Vec::new();
    for root in &effective.roots {
        let admitted = admit_roots(std::slice::from_ref(root), &home, &OsFs)
            .map_err(|error| error.to_string())?;
        match ego_core::permit::check_root_ceilings(&admitted, &ceilings, &home, &workspace, &OsFs)
        {
            Ok(()) => kept.push(root.clone()),
            Err(ego_core::permit::RootsError::OutsideCeiling { .. }) => {
                let read = vec![ProfileRoot::new(
                    admitted[0].path().clone(),
                    ego_core::permit::RootAccess::Read,
                )];
                if root.access == ego_core::permit::RootAccess::ReadWrite
                    && ego_core::permit::check_root_ceilings(
                        &read, &ceilings, &home, &workspace, &OsFs,
                    )
                    .is_ok()
                {
                    kept.push(ego_core::permit::RootCfg {
                        path: root.path.clone(),
                        access: ego_core::permit::RootAccess::Read,
                    });
                    effective.profile_warnings.push(format!(
                        "write access to {} is blocked by the profile; read access only",
                        root.path.display()
                    ));
                } else {
                    effective.profile_warnings.push(format!(
                        "file access to {} is blocked by the profile; path skipped",
                        root.path.display()
                    ));
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    effective.roots = kept;
    // Keep an explicit empty perimeter empty when defaults are applied later.
    effective.roots_source = Some(ego_core::config::Source::Flags);
    let mut kept = Vec::new();
    for path in &effective.exec_paths {
        let admitted = vec![ProfileRoot::new(
            OsFs.canonicalize(path).map_err(|error| error.to_string())?,
            ego_core::permit::RootAccess::Read,
        )];
        match ego_core::permit::check_root_ceilings(&admitted, &ceilings, &home, &workspace, &OsFs)
        {
            Ok(()) => kept.push(path.clone()),
            Err(ego_core::permit::RootsError::OutsideCeiling { .. }) => {
                effective.profile_warnings.push(format!(
                    "exec file access to {} is blocked by the profile; path skipped",
                    path.display()
                ))
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    effective.exec_paths = kept;
    effective
        .root_ceilings
        .extend(ceiling.root_ceilings.clone());
    effective.root_ceilings.extend(ceilings);
    Ok(())
}

/// Report confinement warnings without adding implicit filesystem authority.
pub(crate) fn defaults(effective: &mut Effective, _home: &std::path::Path) -> Vec<String> {
    let mut warnings = Vec::new();
    if effective.sandbox == Some(ego_core::permit::Confinement::Off) {
        warnings.push(
            "sandbox is off; commands can access any file your user account can access".into(),
        );
    }
    warnings
}

#[cfg(test)]
mod portable_defaults {
    use super::*;
    // Catches: a home folder name grants read-write reach without user configuration.
    #[test]
    fn portable_defaults_do_not_grant_home_layout_roots() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir(tree.path().join("Gits")).unwrap();
        let mut effective =
            ego_core::config::resolve(&[], ego_core::config::Trust::Untrusted, None).unwrap();
        defaults(&mut effective, tree.path());
        assert!(effective.roots.is_empty());
    }
}
