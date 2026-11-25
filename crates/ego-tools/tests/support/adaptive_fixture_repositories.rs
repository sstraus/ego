use super::*;

// Catches: global .git floors prevent fixture repositories and their own hooks.
#[test]
fn adaptive_allows_fixture_hooks_without_reopening_workspace_or_common_hooks() {
    let Some(root) = isolated(
        "fixture_repositories::adaptive_allows_fixture_hooks_without_reopening_workspace_or_common_hooks",
    ) else {
        return;
    };
    let workspace = root.join("workspace");
    git_at(&workspace, &["init".as_ref()]);
    git_at(
        &workspace,
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--orphan".as_ref(),
            "-b".as_ref(),
            "linked".as_ref(),
            "../linked".as_ref(),
        ],
    );
    let policy = admit(&workspace).unwrap();
    for fixture in [workspace.join("tmp/fixture"), root.join("tmp/fixture")] {
        std::fs::create_dir_all(&fixture).unwrap();
        execute(
            policy.clone(),
            &root,
            vec![
                "git".into(),
                "init".into(),
                "-q".into(),
                "--template=".into(),
                fixture.clone().into_os_string(),
            ],
        )
        .unwrap();
        execute(policy.clone(), &root, vec!["/bin/sh".into(), "-c".into(),
            "mkdir -p \"$1/.git/hooks\" && printf '#!/bin/sh\\nprintf fired > hook-fired\\n' > \"$1/.git/hooks/pre-commit\" && chmod +x \"$1/.git/hooks/pre-commit\"".into(),
            "fixture".into(), fixture.clone().into_os_string()]).unwrap();
        execute(
            policy.clone(),
            &root,
            vec![
                "git".into(),
                "-C".into(),
                fixture.clone().into_os_string(),
                "-c".into(),
                "user.name=Fixture".into(),
                "-c".into(),
                "user.email=fixture@example.invalid".into(),
                "commit".into(),
                "--allow-empty".into(),
                "-qm".into(),
                "fixture".into(),
            ],
        )
        .unwrap();
        assert_eq!(std::fs::read(fixture.join("hook-fired")).unwrap(), b"fired");
    }
    let linked_policy = admit(&root.join("linked")).unwrap();
    for policy in [policy, linked_policy] {
        let hook = workspace.join(".git/hooks/pre-commit");
        let result = execute(
            policy,
            &root,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf planted > \"$1\"".into(),
                "fixture".into(),
                hook.clone().into_os_string(),
            ],
        );
        assert!(result.is_err(), "workspace/common-dir hook reopened");
        assert!(!hook.exists());
    }
}
