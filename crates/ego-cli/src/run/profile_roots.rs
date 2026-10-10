//! Admission of the active profile's extra roots at the process edge.

use ego_core::config::Effective;
use ego_core::permit::{CanonPath, PathResolver, ProfileRoot, admit_roots};
use ego_tools::fs::OsFs;

/// A derived source cannot raise a configured root ceiling.
pub(crate) fn check_adaptive(
    effective: &Effective,
    policy: &ego_core::permit::RunConfinement,
) -> Result<(), String> {
    let Some(perimeter) = policy.adaptive_perimeter() else {
        return Ok(());
    };
    let roots = perimeter
        .writable
        .iter()
        .cloned()
        .map(|path| ProfileRoot::new(path, ego_core::permit::RootAccess::ReadWrite))
        .collect::<Vec<_>>();
    ego_core::permit::check_root_ceilings(
        &roots,
        &effective.root_ceilings,
        &perimeter.home,
        policy.workspace(),
        &OsFs,
    )
    .map_err(|error| error.to_string())
}

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
                        "root {} exceeds profile write ceiling; using read",
                        root.path.display()
                    ));
                } else {
                    effective.profile_warnings.push(format!(
                        "root {} exceeds profile ceiling; dropped",
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
                    "exec root {} exceeds profile ceiling; dropped",
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

/// Materialize the default at the process edge, keeping missing paths visible.
pub(crate) fn defaults(effective: &mut Effective, home: &std::path::Path) -> Vec<String> {
    let mut warnings = Vec::new();
    if effective.roots_source.is_none()
        && effective.sandbox.unwrap_or_default() != ego_core::permit::Confinement::Adaptive
    {
        let path = home.join("Gits");
        if path.is_dir() {
            effective.roots = vec![ego_core::permit::RootCfg {
                path,
                access: ego_core::permit::RootAccess::ReadWrite,
            }];
        } else {
            warnings.push(
                "default-root-missing: ~/Gits is absent; using workspace-only perimeter".into(),
            );
        }
    }
    if effective.sandbox == Some(ego_core::permit::Confinement::Off) {
        warnings.push(
            "open-perimeter: sandbox is off; exec children have unrestricted filesystem access"
                .into(),
        );
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::config::{ConfigLayer, Source, Trust, resolve};
    // Catches: default HOME reach is broadened or an explicit empty profile cannot clear it.
    #[test]
    fn perimeter_default_uses_fake_home_and_explicit_profile_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("Gits")).unwrap();
        let mut adaptive = resolve(&[], Trust::Untrusted, None).unwrap();
        assert!(defaults(&mut adaptive, &home).is_empty());
        assert!(
            adaptive.roots.is_empty(),
            "adaptive must not inherit ~/Gits writes"
        );
        let workspace = ConfigLayer::parse(Source::User, "sandbox='workspace'").unwrap();
        let mut effective =
            resolve(std::slice::from_ref(&workspace), Trust::Untrusted, None).unwrap();
        assert!(defaults(&mut effective, &home).is_empty());
        assert_eq!(effective.roots[0].path, home.join("Gits"));
        assert_eq!(
            effective.roots[0].access,
            ego_core::permit::RootAccess::ReadWrite
        );
        let layer = ConfigLayer::parse(Source::User, "[profile.strict]\nroots=[]").unwrap();
        let mut strict = resolve(&[layer], Trust::Untrusted, Some("strict")).unwrap();
        assert!(defaults(&mut strict, &home).is_empty());
        assert!(strict.roots.is_empty());
        std::fs::remove_dir(home.join("Gits")).unwrap();
        let mut missing = resolve(&[workspace], Trust::Untrusted, None).unwrap();
        assert_eq!(defaults(&mut missing, &home).len(), 1);
        assert!(missing.roots.is_empty());
        missing.sandbox = Some(ego_core::permit::Confinement::Off);
        assert!(
            defaults(&mut missing, &home)
                .iter()
                .any(|warning| warning.contains("open-perimeter"))
        );
    }
}
