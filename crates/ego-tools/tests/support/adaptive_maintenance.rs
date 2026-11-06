//! Exact owner maintenance, exercised through admitted process ports.
use super::*;
use ego_core::permit::{SandboxAccess, SandboxGrant, SandboxGrantPort};
use futures::future::BoxFuture;
use std::sync::Mutex;

#[derive(Default)]
struct Owner(Mutex<Vec<SandboxGrant>>);

impl SandboxGrantPort for Owner {
    fn granted(&self) -> Vec<SandboxGrant> {
        self.0.lock().unwrap().clone()
    }

    fn ask<'a>(&'a self, grant: &'a SandboxGrant, _: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.0.lock().unwrap().push(grant.clone());
            true
        })
    }
}

fn write(path: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf maintained > \"$1\"".into(),
        "maintenance".into(),
        path.as_os_str().into(),
    ]
}

// Catches: owner approval of an exact instruction or Git control file cannot
// lift the floor, or approval automatically replays a partially run command.
#[test]
fn exact_maintenance_grant_updates_instructions_without_sandbox_off() {
    let Some(root) =
        isolated("maintenance::exact_maintenance_grant_updates_instructions_without_sandbox_off")
    else {
        return;
    };
    let workspace = root.join("workspace");
    git_at(&workspace, &["init".as_ref()]);
    let policy = admit(&workspace).unwrap();
    for relative in [
        "workspace/AGENTS.md",
        "home/other/CLAUDE.md",
        "home/.codex/rules/maintenance.md",
        "workspace/.git/config",
        "workspace/.git/hooks/pre-commit",
        "workspace/.git/info/attributes",
        "workspace/new/AGENTS.md",
    ] {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let creating = relative == "workspace/new/AGENTS.md";
        if !creating {
            std::fs::write(&path, "original").unwrap();
        }
        let owner = Arc::new(Owner::default());
        let first = execute_with_grants(policy.clone(), &root, write(&path), Some(owner.clone()));
        assert!(first.is_err(), "approval replayed {relative}: {first:?}");
        if creating {
            assert!(!path.exists(), "approval replayed file creation");
        } else {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        }
        assert_eq!(
            owner.granted(),
            vec![SandboxGrant::new(
                CanonPath::new(&path),
                SandboxAccess::Write
            )],
            "owner was not asked for {relative}: {first:?}"
        );
        // The 778 session receipt restores the exact same capability.
        let desk = ego_core::permit::residual::ResidualWriteDesk::new();
        desk.restore_owner_grants(&owner.granted());
        let next = execute_with_desk(policy.clone(), &root, write(&path), None, Some(desk));
        assert!(
            next.is_ok(),
            "exact maintenance refused {relative}: {next:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "maintained");
    }
}

// Catches: an exact owner file grant opens sibling controls, credentials,
// global toolchain installation or relocation of a protected ancestor.
#[test]
fn untrusted_hook_cannot_write_controls_without_exact_maintenance_grant() {
    let Some(root) = isolated(
        "maintenance::untrusted_hook_cannot_write_controls_without_exact_maintenance_grant",
    ) else {
        return;
    };
    let workspace = root.join("workspace");
    git_at(&workspace, &["init".as_ref()]);
    let operator = root.join("home/.codex/rules");
    std::fs::create_dir_all(&operator).unwrap();
    let approved = operator.join("approved.md");
    std::fs::write(&approved, "original").unwrap();
    let credential = root.join("home/.ssh/synthetic");
    let toolchain = root.join("cargo/bin/synthetic");
    for path in [&credential, &toolchain] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "original").unwrap();
    }
    let policy = admit(&workspace).unwrap();
    for target in [
        workspace.join("AGENTS.md"),
        operator.join("other.md"),
        workspace.join(".git/config"),
        credential,
        toolchain,
    ] {
        std::fs::write(&target, "original").unwrap();
        let mut grants = vec![
            SandboxGrant::new(CanonPath::new(&approved), SandboxAccess::Write),
            SandboxGrant::new(
                CanonPath::new(target.parent().unwrap()),
                SandboxAccess::Write,
            ),
        ];
        if target.starts_with(root.join("cargo")) || target.starts_with(root.join("home/.ssh")) {
            grants.push(SandboxGrant::new(
                CanonPath::new(&target),
                SandboxAccess::Write,
            ));
        }
        let owner = Arc::new(Owner(Mutex::new(grants)));
        // The untrusted command has no owner seat. Only supplied grants apply.
        let result = execute_with_desk(policy.clone(), &root, write(&target), None, {
            let desk = ego_core::permit::residual::ResidualWriteDesk::new();
            desk.restore_owner_grants(&owner.granted());
            Some(desk)
        });
        assert!(result.is_err(), "unapproved target changed: {target:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }
    let owner = Arc::new(Owner(Mutex::new(vec![SandboxGrant::new(
        CanonPath::new(&approved),
        SandboxAccess::Write,
    )])));
    for ancestor in [root.join("home/.codex"), workspace.join(".git")] {
        let moved = ancestor.with_extension("moved");
        let result = execute_with_grants(
            policy.clone(),
            &root,
            vec![
                "/bin/mv".into(),
                ancestor.as_os_str().into(),
                moved.as_os_str().into(),
            ],
            Some(owner.clone()),
        );
        assert!(result.is_err(), "protected ancestor moved: {ancestor:?}");
        assert!(ancestor.is_dir());
        assert!(!moved.exists());
    }
}
