use super::*;

// Catches: requiring the common metadata directory to be named .git rejects
// real separate-git-dir, submodule, and bare-backed worktree checkouts.
#[test]
fn adaptive_git_accepts_real_metadata_layouts_and_protects_their_controls() {
    let Some(root) = isolated(
        "git_layouts::adaptive_git_accepts_real_metadata_layouts_and_protects_their_controls",
    ) else {
        return;
    };
    git_at(
        &root,
        &[
            "init".as_ref(),
            "--separate-git-dir=metadata".as_ref(),
            "separate".as_ref(),
        ],
    );
    git_at(&root, &["init".as_ref(), "source".as_ref()]);
    git_at(
        &root.join("source"),
        &[
            "-c".as_ref(),
            "user.name=Fixture".as_ref(),
            "-c".as_ref(),
            "user.email=fixture@example.invalid".as_ref(),
            "commit".as_ref(),
            "--allow-empty".as_ref(),
            "-m".as_ref(),
            "fixture".as_ref(),
        ],
    );
    git_at(&root, &["init".as_ref(), "superproject".as_ref()]);
    git_at(
        &root.join("superproject"),
        &[
            "-c".as_ref(),
            "protocol.file.allow=always".as_ref(),
            "submodule".as_ref(),
            "add".as_ref(),
            root.join("source").as_os_str(),
            "nested".as_ref(),
        ],
    );
    git_at(
        &root,
        &["init".as_ref(), "--bare".as_ref(), "bare.git".as_ref()],
    );
    git_at(
        &root.join("bare.git"),
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--orphan".as_ref(),
            "-b".as_ref(),
            "fixture".as_ref(),
            "../bare-linked".as_ref(),
        ],
    );
    for (checkout, metadata) in [
        ("separate", "metadata"),
        ("superproject/nested", "superproject/.git/modules/nested"),
        ("bare-linked", "bare.git"),
    ] {
        let workspace = root.join(checkout);
        let policy = admit(&workspace).unwrap_or_else(|e| panic!("{checkout}: {e}"));
        std::fs::write(workspace.join("tracked"), "fixture").unwrap();
        execute(
            policy.clone(),
            &root,
            vec!["git".into(), "add".into(), "tracked".into()],
        )
        .unwrap();
        let config = root.join(metadata).join("config");
        let before = std::fs::read(&config).unwrap();
        let result = execute(
            policy,
            &root,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf compromised > \"$1\"".into(),
                "fixture".into(),
                config.clone().into_os_string(),
            ],
        );
        assert!(result.is_err(), "control write admitted: {checkout}");
        assert_eq!(std::fs::read(config).unwrap(), before);
    }
}

// Catches: relative worktree back pointers are resolved against the process cwd.
#[test]
fn adaptive_git_accepts_real_relative_worktree_back_pointers() {
    let Some(root) =
        isolated("git_layouts::adaptive_git_accepts_real_relative_worktree_back_pointers")
    else {
        return;
    };
    git_at(&root, &["init".as_ref(), "main".as_ref()]);
    git_at(
        &root.join("main"),
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--relative-paths".as_ref(),
            "--orphan".as_ref(),
            "-b".as_ref(),
            "fixture".as_ref(),
            "../linked".as_ref(),
        ],
    );
    let workspace = root.join("linked");
    let policy = admit(&workspace).unwrap();
    std::fs::write(workspace.join("tracked"), "fixture").unwrap();
    execute(
        policy,
        &root,
        vec!["git".into(), "add".into(), "tracked".into()],
    )
    .unwrap();
}

// Catches: path-only control denies allow metadata-root renames and miss
// nested modules, other worktrees, and newly created .git selector files.
#[test]
fn adaptive_git_control_floors_block_rename_and_nested_control_writes() {
    let Some(root) =
        isolated("git_layouts::adaptive_git_control_floors_block_rename_and_nested_control_writes")
    else {
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
            "fixture".as_ref(),
            "../linked".as_ref(),
        ],
    );
    git_at(&root, &["init".as_ref(), "source".as_ref()]);
    git_at(
        &root.join("source"),
        &[
            "-c".as_ref(),
            "user.name=Fixture".as_ref(),
            "-c".as_ref(),
            "user.email=fixture@example.invalid".as_ref(),
            "commit".as_ref(),
            "--allow-empty".as_ref(),
            "-m".as_ref(),
            "fixture".as_ref(),
        ],
    );
    git_at(
        &workspace,
        &[
            "-c".as_ref(),
            "protocol.file.allow=always".as_ref(),
            "submodule".as_ref(),
            "add".as_ref(),
            root.join("source").as_os_str(),
            "nested".as_ref(),
        ],
    );
    std::fs::create_dir(workspace.join("fresh")).unwrap();
    let policy = admit(&workspace).unwrap();
    let metadata = workspace.join(".git");
    for relative in [
        "modules/nested/hooks/post-checkout",
        "modules/nested/config",
        "worktrees/linked/gitdir",
        "worktrees/linked/commondir",
    ] {
        let path = metadata.join(relative);
        let before = std::fs::read(&path).ok();
        let result = execute(
            policy.clone(),
            &root,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf compromised > \"$1\"".into(),
                "fixture".into(),
                path.clone().into_os_string(),
            ],
        );
        assert!(result.is_err(), "nested control write admitted: {relative}");
        assert_eq!(std::fs::read(path).ok(), before);
    }
    let selector = workspace.join("fresh/.git");
    let result = execute(
        policy.clone(),
        &root,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'gitdir: ../nested' > \"$1\"".into(),
            "fixture".into(),
            selector.clone().into_os_string(),
        ],
    );
    assert!(result.is_err(), "new Git selector admitted");
    assert!(!selector.exists());
    let renamed = workspace.join("renamed-metadata");
    let result = execute(
        policy.clone(),
        &root,
        vec![
            "/bin/mv".into(),
            metadata.clone().into_os_string(),
            renamed.clone().into_os_string(),
        ],
    );
    assert!(result.is_err(), "Git metadata rename admitted");
    assert!(metadata.is_dir());
    assert!(!renamed.exists());
    std::fs::write(workspace.join("tracked"), "fixture").unwrap();
    execute(
        policy,
        &root,
        vec!["git".into(), "add".into(), "tracked".into()],
    )
    .unwrap();
}
