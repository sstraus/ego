use super::*;

fn identity(workspace: &Path) {
    git(workspace, &["config", "user.name", "critic"]);
    git(
        workspace,
        &["config", "user.email", "critic@example.invalid"],
    );
    git(workspace, &["config", "commit.gpgsign", "false"]);
}

/// Adds and commits submodule `sub`, sourced from a fresh repository.
fn submodule(root: &Path, workspace: &Path) -> PathBuf {
    let source = tempfile::tempdir_in(root).unwrap().keep();
    git(&source, &["init", "-q", "-b", "main"]);
    identity(&source);
    commit(&source, "base");
    git(
        workspace,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            source.to_str().unwrap(),
            "sub",
        ],
    );
    git(workspace, &["commit", "-q", "-m", "sub"]);
    workspace.join("sub")
}

/// A committed workspace whose metadata lives in `root/meta.git`.
fn separate(root: &Path) -> (PathBuf, PathBuf) {
    let meta = root.join("meta.git");
    let workspace = root.join("workspace");
    git(
        root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            meta.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
    );
    identity(&workspace);
    commit(&workspace, "base");
    (workspace, meta)
}

// Catches: rooted floors for separately named metadata are written as
// backslash-doubled text inside a raw SBPL `#"..."` regex, so they never match
// and another worktree's selector under a separate-git-dir stays writable.
#[test]
fn adaptive_child_cannot_rewrite_other_worktree_selector_under_separate_git_dir() {
    let Some(root) = isolated(
        "r2::adaptive_child_cannot_rewrite_other_worktree_selector_under_separate_git_dir",
    ) else {
        return;
    };
    let (workspace, meta) = separate(&root);
    let other = root.join("other");
    git(
        &workspace,
        &["worktree", "add", "-q", other.to_str().unwrap()],
    );
    let selector = std::fs::canonicalize(meta.join("worktrees/other/gitdir")).unwrap();
    let before = std::fs::read(&selector).unwrap();

    let result = confined(
        &workspace,
        &root,
        &format!("printf forged > '{}'", selector.display()),
    );

    assert_eq!(
        std::fs::read(&selector).unwrap(),
        before,
        "other-worktree selector under separate metadata was rewritten: {result}"
    );
}

// Catches: the module boundary floor `/modules(/.*)?` covers every directory
// of a submodule's metadata, so a confined rebase in an admitted submodule
// workspace cannot remove .git/modules/<name>/rebase-merge.
#[test]
fn adaptive_confined_rebase_in_submodule_workspace_leaves_no_rebase_state() {
    let Some(root) =
        isolated("r2::adaptive_confined_rebase_in_submodule_workspace_leaves_no_rebase_state")
    else {
        return;
    };
    let workspace = repository(&root);
    let sub = submodule(&root, &workspace);
    identity(&sub);
    git(&sub, &["switch", "-q", "-c", "topic"]);
    commit(&sub, "topic");
    git(&sub, &["switch", "-q", "main"]);
    commit(&sub, "upstream");
    git(&sub, &["switch", "-q", "topic"]);

    let result = confined(&sub, &root, "git rebase --merge main");

    assert!(
        !workspace.join(".git/modules/sub/rebase-merge").exists(),
        "confined submodule rebase left its state directory behind: {result}"
    );
    assert_eq!(
        git(&sub, &["rev-parse", "topic~1"]),
        git(&sub, &["rev-parse", "main"]),
        "{result}"
    );
}

// Catches: the module transient carve-out is missing from one of the two
// boundary forms, so Git cannot clean operation state or empty fan-out
// directories inside .git/modules/<name> or <separate metadata>/modules/<name>.
#[test]
fn adaptive_confined_git_can_remove_transient_submodule_metadata_directories() {
    let Some(root) =
        isolated("r2::adaptive_confined_git_can_remove_transient_submodule_metadata_directories")
    else {
        return;
    };
    let (separate, meta) = separate(&root);
    submodule(&root, &separate);
    let inline = root.join("inline");
    std::fs::create_dir(&inline).unwrap();
    git(&inline, &["init", "-q", "-b", "main"]);
    identity(&inline);
    commit(&inline, "base");
    submodule(&root, &inline);
    let transient = [
        "rebase-apply",
        "sequencer",
        "objects/empty",
        "refs/heads/empty",
        "logs/refs/empty",
    ];
    for (workspace, module) in [
        (&separate, meta.join("modules/sub")),
        (&inline, inline.join(".git/modules/sub")),
    ] {
        for name in transient {
            std::fs::create_dir_all(module.join(name)).unwrap();
        }
        let module = std::fs::canonicalize(module).unwrap();
        let result = confined(
            workspace,
            &root,
            &format!(
                "cd '{}' && rmdir rebase-apply sequencer objects/empty refs/heads/empty logs/refs/empty",
                module.display()
            ),
        );
        for name in transient {
            assert!(
                !module.join(name).exists(),
                "{}: {name}: {result}",
                module.display()
            );
        }
    }
}

// Catches: the module transient carve-out also matches the module directory
// itself, so a confined child relocates submodule metadata beneath a
// separate-git-dir together with its protected config and hooks.
#[test]
fn adaptive_child_cannot_relocate_submodule_metadata_under_separate_git_dir() {
    let Some(root) =
        isolated("r2::adaptive_child_cannot_relocate_submodule_metadata_under_separate_git_dir")
    else {
        return;
    };
    let (workspace, meta) = separate(&root);
    submodule(&root, &workspace);
    let module = std::fs::canonicalize(meta.join("modules/sub")).unwrap();
    let relocated = workspace.join("relocated");

    let result = confined(
        &workspace,
        &root,
        &format!("mv '{}' '{}'", module.display(), relocated.display()),
    );

    assert!(
        module.is_dir(),
        "submodule metadata was relocated: {result}"
    );
    assert!(!relocated.exists(), "{result}");
}
