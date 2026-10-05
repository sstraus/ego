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

/// Materialize the default at the process edge, keeping missing paths visible.
pub(crate) fn defaults(effective: &mut Effective, home: &std::path::Path) -> Vec<String> {
    let mut warnings = Vec::new();
    if effective.roots_source.is_none() {
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
        let mut effective = resolve(&[], Trust::Untrusted, None).unwrap();
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
        let mut missing = resolve(&[], Trust::Untrusted, None).unwrap();
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
