use super::same_repository_roots;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(root: &Path) -> std::path::PathBuf {
    let main = root.join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "first"]);
    main.canonicalize().unwrap()
}

// Catches: a linked worktree sees only its own checkout, so the main checkout,
// its siblings and the common git directory count as outside the workspace.
#[test]
fn a_linked_worktree_reaches_main_siblings_and_the_common_dir() {
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let main = repository(&base);
    git(&main, &["worktree", "add", "-q", "../a", "-b", "a"]);
    git(&main, &["worktree", "add", "-q", "../b", "-b", "b"]);
    let found: Vec<_> = same_repository_roots(&base.join("a"))
        .iter()
        .map(|root| root.as_path().to_path_buf())
        .collect();
    for expected in [
        main.join(".git"),
        main.clone(),
        base.join("a"),
        base.join("b"),
    ] {
        assert!(
            found.contains(&expected),
            "{expected:?} missing in {found:?}"
        );
    }
    assert_eq!(found.len(), 4, "{found:?}");
}

// Catches: membership taken from path names or an unchecked pointer, so a
// forged worktrees entry pulls a foreign directory into the repository.
#[test]
fn a_forged_worktree_entry_never_joins() {
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let main = repository(&base);
    let foreign = base.join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    git(&foreign, &["init", "-q"]);
    let forged = main.join(".git/worktrees/forged");
    std::fs::create_dir_all(&forged).unwrap();
    std::fs::write(
        forged.join("gitdir"),
        format!("{}\n", foreign.join(".git").display()),
    )
    .unwrap();
    let found = same_repository_roots(&main);
    assert!(
        found
            .iter()
            .all(|root| !root.as_path().starts_with(&foreign)),
        "{found:?}"
    );
}

// Catches: a directory outside any git working tree gaining roots.
#[test]
fn outside_a_repository_the_set_is_empty() {
    let tree = tempfile::tempdir().unwrap();
    assert!(same_repository_roots(tree.path()).is_empty());
}

fn context(
    workspace: &Path,
    level: ego_core::permit::Confinement,
) -> super::super::process::RunProcessContext {
    use ego_core::event::id::SessionId;
    use ego_core::permit::{CanonPath, ExecCaps, ProfileRoot, RootAccess, RunConfinement};
    let started: Vec<_> = super::same_repository_roots(workspace)
        .into_iter()
        .filter(|root| !root.is_or_under(&CanonPath::new(workspace)))
        .map(|root| ProfileRoot::new(root, RootAccess::ReadWrite))
        .collect();
    let confinement = RunConfinement::new(level, false, CanonPath::new(workspace))
        .unwrap()
        .with_roots(started);
    let evidence = tempfile::tempdir().unwrap();
    let store = crate::exec::SessionEvidenceStore::open(evidence.path(), SessionId::new(), 1 << 20)
        .unwrap();
    super::super::process::RunProcessContext::new(
        std::sync::Arc::new(super::super::null::NullBackend),
        store,
        ego_core::turn::CancelToken::new(),
        crate::exec::ExecLimits::new(4096, std::time::Duration::from_millis(250)).unwrap(),
        confinement,
        ExecCaps::empty(),
        None,
    )
}

// Catches: roots fixed at process start, so a worktree an agent creates mid-session
// stays outside the sandbox of every later spawn until a restart.
#[test]
fn a_worktree_added_after_start_is_granted_to_the_next_spawn() {
    use ego_core::permit::{Confinement, SandboxAccess};
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let main = repository(&base);
    git(&main, &["worktree", "add", "-q", "../a", "-b", "a"]);
    let run = context(&base.join("a"), Confinement::Workspace);
    assert!(run.repository_grants().is_empty());
    git(&main, &["worktree", "add", "-q", "../late", "-b", "late"]);
    let grants = run.repository_grants();
    assert_eq!(grants.len(), 1, "{grants:?}");
    assert_eq!(grants[0].path.as_path(), base.join("late"));
    assert_eq!(grants[0].access, SandboxAccess::Write);
    // sandbox ro never gains write.
    let ro = context(&base.join("a"), Confinement::Ro);
    git(&main, &["worktree", "add", "-q", "../later", "-b", "later"]);
    assert!(
        ro.repository_grants()
            .iter()
            .all(|g| g.access == SandboxAccess::Read)
    );
}

// Catches: a worktrees entry forged after start (back pointer to a foreign
// repository) being granted to the next spawn.
#[test]
fn a_forged_entry_added_after_start_is_never_granted() {
    use ego_core::permit::Confinement;
    let tree = tempfile::tempdir().unwrap();
    let base = tree.path().canonicalize().unwrap();
    let main = repository(&base);
    let foreign = base.join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    git(&foreign, &["init", "-q"]);
    let run = context(&main, Confinement::Workspace);
    let forged = main.join(".git/worktrees/forged");
    std::fs::create_dir_all(&forged).unwrap();
    std::fs::write(
        forged.join("gitdir"),
        format!("{}\n", foreign.join(".git").display()),
    )
    .unwrap();
    assert!(run.repository_grants().is_empty());
}
