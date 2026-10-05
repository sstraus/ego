use ego_core::config::{ConfigLayer, Source, Trust, resolve};
use ego_core::permit::{CanonPath, RootAccess, RootCfg, admit_roots, check_root_ceilings};
use ego_tools::fs::OsFs;

// Catches: lexical prefix comparisons, a symlink escape, wider write access,
// or a broad writable root erases a nested read-only system boundary.
#[cfg(unix)]
#[test]
fn system_root_ceilings_use_canonical_paths_and_preserve_access() {
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let home = base.join("home");
    let workspace = base.join("workspace");
    let allowed = base.join("allowed");
    let writable = allowed.join("writable");
    let frozen = writable.join("frozen");
    let sibling = base.join("allowed-sibling");
    for path in [&home, &workspace, &frozen, &sibling] {
        std::fs::create_dir_all(path).unwrap();
    }
    let escape = allowed.join("escape");
    std::os::unix::fs::symlink(&sibling, &escape).unwrap();
    let cfg = |path: &std::path::Path, access| RootCfg {
        path: path.into(),
        access,
    };
    assert!(
        ConfigLayer::parse(
            Source::User,
            &format!(
                "roots=[{{path='{}',access='read-write'}},{{path='{}',access='read'}}]",
                writable.display(),
                frozen.display()
            )
        )
        .unwrap_err()
        .to_string()
        .contains("nested-read-only-root")
    );
    let system = ConfigLayer::parse(
        Source::System,
        &format!(
            "roots=[{{path='{}',access='read'}},{{path='{}',access='read-write'}}]",
            allowed.display(),
            writable.display()
        ),
    )
    .unwrap();
    let effective = resolve(&[system], Trust::Untrusted, None).unwrap();
    for (path, access, permitted) in [
        (&allowed, RootAccess::Read, true),
        (&allowed, RootAccess::ReadWrite, false),
        (&writable, RootAccess::ReadWrite, true),
        (&frozen, RootAccess::ReadWrite, true),
        (&sibling, RootAccess::Read, false),
        (&escape, RootAccess::Read, false),
        (&workspace, RootAccess::ReadWrite, true),
    ] {
        let roots = admit_roots(&[cfg(path, access)], &CanonPath::new(&home), &OsFs).unwrap();
        assert_eq!(
            check_root_ceilings(
                &roots,
                &effective.root_ceilings,
                &CanonPath::new(&home),
                &CanonPath::new(&workspace),
                &OsFs
            )
            .is_ok(),
            permitted,
            "{} {access:?}",
            path.display()
        );
    }
    let roots = admit_roots(
        &[cfg(&sibling, RootAccess::Read)],
        &CanonPath::new(&home),
        &OsFs,
    )
    .unwrap();
    assert!(
        check_root_ceilings(
            &roots,
            &[vec![]],
            &CanonPath::new(&home),
            &CanonPath::new(&workspace),
            &OsFs
        )
        .is_err()
    );
}

// Catches: a trusted project read allowlist adds paths beyond operator authority,
// or an explicit empty profile fails to replace a base list.
#[test]
fn project_read_restriction_cannot_widen_operator_roots() {
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let home = base.join("home");
    let allowed = home.join("Gits");
    let outside = base.join("outside");
    let workspace = allowed.join("workspace");
    for path in [&workspace, &outside] {
        std::fs::create_dir_all(path).unwrap();
    }
    let user = ConfigLayer::parse(
        Source::User,
        &format!(
            "roots=[{{path='{}',access='read-write'}}]
[profile.strict]
roots=[]",
            allowed.display()
        ),
    )
    .unwrap();
    assert!(
        resolve(std::slice::from_ref(&user), Trust::Trusted, Some("strict"))
            .unwrap()
            .roots
            .is_empty()
    );
    for (path, permitted) in [(&allowed, true), (&outside, false)] {
        let project = ConfigLayer::parse(
            Source::Project,
            &format!("roots=[{{path='{}',access='read'}}]", path.display()),
        )
        .unwrap();
        let effective = resolve(&[user.clone(), project], Trust::Trusted, None).unwrap();
        let roots = admit_roots(&effective.roots, &CanonPath::new(&home), &OsFs).unwrap();
        assert_eq!(
            check_root_ceilings(
                &roots,
                &effective.root_ceilings,
                &CanonPath::new(&home),
                &CanonPath::new(&workspace),
                &OsFs
            )
            .is_ok(),
            permitted
        );
    }
}
