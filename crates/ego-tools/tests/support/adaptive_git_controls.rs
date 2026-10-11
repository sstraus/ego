use super::*;

// Catches: path-only control denies allow metadata-root renames and miss
// nested modules, other worktrees, and newly created .git selector files.
#[test]
fn adaptive_git_control_floors_block_rename_and_nested_control_writes() {
    let Some(root) = isolated(
        "git_controls::adaptive_git_control_floors_block_rename_and_nested_control_writes",
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
    let config_alias = workspace.join("config-alias");
    std::os::unix::fs::symlink(metadata.join("config"), &config_alias).unwrap();
    let before = std::fs::read(metadata.join("config")).unwrap();
    let result = execute(
        policy.clone(),
        &root,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf compromised > \"$1\"".into(),
            "fixture".into(),
            config_alias.into_os_string(),
        ],
    );
    assert!(result.is_err(), "Git config symlink alias write admitted");
    assert_eq!(std::fs::read(metadata.join("config")).unwrap(), before);
    let hardlink = workspace.join("config-hardlink");
    let linked = execute(
        policy.clone(),
        &root,
        vec![
            "/bin/ln".into(),
            metadata.join("config").into_os_string(),
            hardlink.clone().into_os_string(),
        ],
    );
    if linked.is_ok() {
        let result = execute(
            policy.clone(),
            &root,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf compromised > \"$1\"".into(),
                "fixture".into(),
                hardlink.into_os_string(),
            ],
        );
        assert!(result.is_err(), "Git config hardlink write admitted");
    } else {
        assert!(!hardlink.exists());
    }
    assert_eq!(std::fs::read(metadata.join("config")).unwrap(), before);
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
    let module = metadata.join("modules/nested");
    let relocated_module = workspace.join("relocated-module");
    let result = execute(
        policy.clone(),
        &root,
        vec![
            "/bin/mv".into(),
            module.clone().into_os_string(),
            relocated_module.clone().into_os_string(),
        ],
    );
    assert!(result.is_err(), "nested metadata directory rename admitted");
    assert!(module.is_dir());
    assert!(!relocated_module.exists());
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
        policy.clone(),
        &root,
        vec!["git".into(), "add".into(), "tracked".into()],
    )
    .unwrap();
    std::fs::write(workspace.join("nested/tracked"), "fixture").unwrap();
    execute(
        policy.clone(),
        &root,
        vec![
            "git".into(),
            "-C".into(),
            "nested".into(),
            "add".into(),
            "tracked".into(),
        ],
    )
    .unwrap();
}

// Catches: dropping a top-level Git floor name silently permits its write.
#[test]
fn adaptive_top_level_git_floor_writes_report_native_refusal() {
    let Some(root) =
        isolated("git_controls::adaptive_top_level_git_floor_writes_report_native_refusal")
    else {
        return;
    };
    let workspace = root.join("workspace");
    git_at(&workspace, &["init".as_ref()]);
    let policy = admit(&workspace).unwrap();
    for relative in ["info/attributes", "config.worktree", "commondir"] {
        let path = workspace.join(".git").join(relative);
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
        let Err(ego_core::permit::ProcessError::Exit { result, .. }) = result else {
            panic!("expected native refusal for {relative}: {result:?}");
        };
        let diagnostic = result
            .content
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            diagnostic.contains("Operation not permitted"),
            "{relative}: {diagnostic}"
        );
        assert_eq!(std::fs::read(&path).ok(), before, "{relative} changed");
    }
    let ordinary = workspace.join("ordinary.txt");
    execute(
        policy,
        &root,
        vec!["/usr/bin/touch".into(), ordinary.clone().into_os_string()],
    )
    .unwrap();
    assert!(
        ordinary.is_file(),
        "ordinary workspace writes remain available"
    );
}
