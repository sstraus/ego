use super::*;

fn identity(workspace: &Path) {
    git(workspace, &["config", "user.name", "critic"]);
    git(
        workspace,
        &["config", "user.email", "critic@example.invalid"],
    );
    git(workspace, &["config", "commit.gpgsign", "false"]);
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
    let meta = root.join("meta.git");
    let workspace = root.join("workspace");
    git(
        &root,
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
    let other = root.join("other");
    git(&workspace, &["worktree", "add", "-q", other.to_str().unwrap()]);
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
    let source = root.join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    identity(&source);
    commit(&source, "base");
    let workspace = repository(&root);
    git(
        &workspace,
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
    git(&workspace, &["commit", "-q", "-m", "sub"]);
    let sub = workspace.join("sub");
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
