//! Real local Git fixtures; no model, network or user-home mutation.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ego_cli::cli::{Invocation, WorktreeAction, parse};
use ego_cli::worktree::{LocalRepository, Receipt, ReceiptState, Repository};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Worktree Test",
            "-c",
            "user.email=worktree@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn fixture() -> (tempfile::TempDir, PathBuf, LocalRepository) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "master"]);
    fs::write(root.join("tracked"), "committed\n").unwrap();
    git(&root, &["add", "tracked"]);
    git(&root, &["commit", "-qm", "initial"]);
    let home = dir.path().join("ego");
    fs::create_dir(&home).unwrap();
    let home = home.canonicalize().unwrap();
    (
        dir,
        root.canonicalize().unwrap(),
        LocalRepository::new(home),
    )
}

fn create(
    repository: &LocalRepository,
    source: &Path,
    branch: &str,
) -> Result<(PathBuf, String), String> {
    let (path, text, _lease) = repository.create(source, branch)?;
    Ok((path, text))
}

// Catches: worktree launch silently attaches an existing or resumed session.
#[test]
fn worktree_branch_consent_is_explicit_and_new_session_only() {
    for args in [
        vec!["ego", "run", "--worktree"],
        vec!["ego", "resume", "--worktree", "feat/task"],
        vec!["ego", "run", "--worktree", "feat/task", "--session", "old"],
        vec!["ego", "acp", "--worktree", "feat/task"],
    ] {
        assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
    }
    let invocation = parse(
        ["ego", "run", "--worktree", "feat/task"]
            .map(str::to_owned)
            .to_vec(),
    )
    .unwrap();
    let Invocation::Run(args) = invocation else {
        panic!("run expected")
    };
    assert_eq!(args.worktree.as_deref(), Some("feat/task"));
    let id = uuid::Uuid::new_v4();
    assert_eq!(
        parse(vec![
            "ego".into(),
            "worktree".into(),
            "cleanup".into(),
            id.to_string()
        ])
        .unwrap(),
        Invocation::Worktree(WorktreeAction::Cleanup { receipt: id })
    );
}

// Catches: linked-source launch copies dirty source state or loses common-dir ownership.
#[test]
fn linked_checkout_launch_has_durable_identity_and_retains_source_changes() {
    let (_dir, root, repository) = fixture();
    fs::write(root.join("tracked"), "dirty source\n").unwrap();
    let (first, text) = create(&repository, &root, "feat/first").unwrap();
    let receipt: Receipt = serde_json::from_str(&text).unwrap();
    assert_eq!(receipt.state, ReceiptState::Ready);
    assert_eq!(
        repository.read(receipt.id).unwrap().common_dir,
        root.join(".git")
    );
    assert_eq!(
        fs::read_to_string(first.join("tracked")).unwrap(),
        "committed\n"
    );
    let (second, text) = create(&repository, &first, "feat/second").unwrap();
    let second_receipt: Receipt = serde_json::from_str(&text).unwrap();
    assert_eq!(second_receipt.common_dir, receipt.common_dir);
    assert_eq!(git(&second, &["branch", "--show-current"]), "feat/second");
    repository.cleanup(second_receipt.id).unwrap();
    assert!(!second.exists());
    assert!(first.exists());
    assert_eq!(
        fs::read_to_string(root.join("tracked")).unwrap(),
        "dirty source\n"
    );
    repository.cleanup(receipt.id).unwrap();
    assert_eq!(
        repository.cleanup(receipt.id).unwrap().state,
        ReceiptState::Removed
    );
    assert_eq!(
        git(&root, &["rev-parse", "refs/heads/feat/first"]),
        receipt.initial_head
    );
}

// Catches: cleanup destroys untracked work, local commits, or a changed branch.
#[test]
fn cleanup_refuses_dirty_unmerged_and_changed_branch_work() {
    let (_dir, root, repository) = fixture();
    let (checkout, text) = create(&repository, &root, "feat/safe").unwrap();
    let receipt: Receipt = serde_json::from_str(&text).unwrap();
    fs::write(checkout.join("untracked"), "keep me").unwrap();
    assert!(
        repository
            .cleanup(receipt.id)
            .unwrap_err()
            .contains("uncommitted")
    );
    assert!(checkout.join("untracked").exists());
    git(&checkout, &["add", "untracked"]);
    git(&checkout, &["commit", "-qm", "unmerged work"]);
    assert!(
        repository
            .cleanup(receipt.id)
            .unwrap_err()
            .contains("unmerged")
    );
    git(&root, &["merge", "--ff-only", "feat/safe"]);
    assert!(
        repository
            .cleanup(receipt.id)
            .unwrap_err()
            .contains("upstream")
    );
    git(&checkout, &["branch", "-m", "feat/renamed"]);
    assert!(
        repository
            .cleanup(receipt.id)
            .unwrap_err()
            .contains("branch changed")
    );
    assert!(checkout.exists());
}

// Catches: creation from a submodule treats the superproject as the Git owner.
#[test]
fn submodule_source_keeps_its_own_common_directory() {
    let (dir, root, repository) = fixture();
    let child = dir.path().join("child");
    fs::create_dir(&child).unwrap();
    git(&child, &["init", "-q", "-b", "master"]);
    fs::write(child.join("child-file"), "submodule").unwrap();
    git(&child, &["add", "."]);
    git(&child, &["commit", "-qm", "child"]);
    git(
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            child.to_str().unwrap(),
            "module",
        ],
    );
    git(&root, &["commit", "-qm", "submodule"]);
    let (checkout, text) = repository
        .create(&root.join("module"), "feat/child")
        .unwrap();
    let receipt: Receipt = serde_json::from_str(&text).unwrap();
    assert_eq!(receipt.common_dir, root.join(".git/modules/module"));
    assert_eq!(
        fs::read_to_string(checkout.join("child-file")).unwrap(),
        "submodule"
    );
    repository.cleanup(receipt.id).unwrap();
}

// Catches: a crash before receipt settlement hides a real checkout, or cleanup
// loses the result when Git removal precedes receipt publication.
#[test]
fn pending_and_post_removal_receipts_recover_without_deleting_other_work() {
    let (_dir, root, repository) = fixture();
    let (checkout, text) = create(&repository, &root, "feat/recovery").unwrap();
    let mut receipt: Receipt = serde_json::from_str(&text).unwrap();
    receipt.state = ReceiptState::Pending;
    let receipt_path = checkout
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("{}.json", receipt.id));
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    assert_eq!(
        repository.read(receipt.id).unwrap().state,
        ReceiptState::Pending
    );
    git(&root, &["worktree", "remove", checkout.to_str().unwrap()]);
    assert_eq!(
        repository.cleanup(receipt.id).unwrap().state,
        ReceiptState::Removed
    );
    assert!(root.join("tracked").exists());
}

// Catches: invalid/existing branch input overwrites existing work or invents a name.
#[test]
fn branch_validation_and_creation_failure_preserve_existing_checkout() {
    let (_dir, root, repository) = fixture();
    assert!(create(&repository, &root, "-bad").is_err());
    assert!(create(&repository, &root, "HEAD").is_err());
    assert!(create(&repository, &root, "@{-1}").is_err());
    assert!(create(&repository, &root, "master").is_err());
    assert_eq!(git(&root, &["branch", "--show-current"]), "master");
    assert_eq!(
        fs::read_to_string(root.join("tracked")).unwrap(),
        "committed\n"
    );
}

// Catches: checkout smudge filters execute project commands or fetch remote content.
#[test]
fn creation_does_not_execute_checkout_filters() {
    let (dir, root, repository) = fixture();
    fs::write(root.join(".gitattributes"), "tracked filter=trap\n").unwrap();
    git(&root, &["add", ".gitattributes"]);
    git(&root, &["commit", "-qm", "filter declaration"]);
    let marker = dir.path().join("filter-was-executed");
    git(
        &root,
        &[
            "config",
            "filter.trap.smudge",
            &format!("touch '{}'", marker.display()),
        ],
    );
    git(&root, &["config", "filter.trap.required", "true"]);
    let (checkout, _) = create(&repository, &root, "feat/no-filter").unwrap();
    assert!(!marker.exists());
    assert_eq!(
        fs::read_to_string(checkout.join("tracked")).unwrap(),
        "committed\n"
    );
}

// Catches: ignored operator data is discarded merely because Git calls it clean.
#[test]
fn cleanup_preserves_ignored_files() {
    let (_dir, root, repository) = fixture();
    fs::write(root.join(".gitignore"), "private-data\n").unwrap();
    git(&root, &["add", ".gitignore"]);
    git(&root, &["commit", "-qm", "ignore private data"]);
    let (checkout, text) = create(&repository, &root, "feat/ignored").unwrap();
    let receipt: Receipt = serde_json::from_str(&text).unwrap();
    fs::write(checkout.join("private-data"), "preserve").unwrap();
    assert!(
        repository
            .cleanup(receipt.id)
            .unwrap_err()
            .contains("uncommitted")
    );
    assert_eq!(
        fs::read_to_string(checkout.join("private-data")).unwrap(),
        "preserve"
    );
}

// Catches: a failed hosted launch falls back to local ownership, or local cleanup
// takes over a hosted lifecycle. Isolated HOME and PATH prohibit real host/model calls.
#[cfg(unix)]
#[test]
fn unavailable_host_never_falls_back_to_local_ownership() {
    let (dir, root, _) = fixture();
    let tools = dir.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let git_path = Command::new("which").arg("git").output().unwrap();
    assert!(git_path.status.success());
    std::os::unix::fs::symlink(
        String::from_utf8(git_path.stdout).unwrap().trim(),
        tools.join("git"),
    )
    .unwrap();
    let home = dir.path().join("isolated-home");
    fs::create_dir(&home).unwrap();
    let ego_home = home.join("ego");
    let mut command = Command::new(env!("CARGO_BIN_EXE_ego"));
    command
        .env_clear()
        .env("PATH", &tools)
        .env("HOME", &home)
        .env("EGO_HOME", &ego_home)
        .env("TUIC_SOCKET", dir.path().join("missing.sock"))
        .args([
            "run",
            "--worktree",
            "feat/host",
            "-C",
            root.to_str().unwrap(),
            "no generation",
        ]);
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!ego_home.join("worktrees").exists());
    assert!(!git(&root, &["branch", "--list", "feat/host"]).contains("feat/host"));
    let output = Command::new(env!("CARGO_BIN_EXE_ego"))
        .env_clear()
        .env("HOME", &home)
        .env("EGO_HOME", &ego_home)
        .env("TUIC_SOCKET", dir.path().join("missing.sock"))
        .args(["worktree", "cleanup", &uuid::Uuid::new_v4().to_string()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TUIC owns"));
    assert!(!ego_home.join("worktrees").exists());
}

// Catches: cleanup removes a clean checkout while its owning session is active.
#[test]
fn cleanup_refuses_an_active_launch_until_its_lease_is_released() {
    let (_dir, root, repository) = fixture();
    let (checkout, text, lease) = repository.create(&root, "feat/active").unwrap();
    let receipt: Receipt = serde_json::from_str(&text).unwrap();
    assert!(
        repository
            .cleanup(receipt.id)
            .unwrap_err()
            .contains("in use")
    );
    assert!(checkout.exists());
    drop(lease);
    repository.cleanup(receipt.id).unwrap();
    assert!(!checkout.exists());
}

// Catches: cleanup accepts a local merge as proof of push, or refuses verified
// merged-and-pushed work forever. The remote is a local bare fixture, not a service.
#[test]
fn cleanup_requires_both_merge_and_upstream_evidence_for_new_commits() {
    let (dir, root, repository) = fixture();
    let remote = dir.path().join("remote.git");
    git(&root, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    git(
        &root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&root, &["push", "-qu", "origin", "master"]);
    let (checkout, text) = create(&repository, &root, "feat/integrated").unwrap();
    let receipt: Receipt = serde_json::from_str(&text).unwrap();
    fs::write(checkout.join("tracked"), "new commit\n").unwrap();
    git(&checkout, &["add", "tracked"]);
    git(&checkout, &["commit", "-qm", "new work"]);
    git(&root, &["merge", "--ff-only", "feat/integrated"]);
    assert!(
        repository
            .cleanup(receipt.id)
            .unwrap_err()
            .contains("unpushed")
    );
    assert!(checkout.exists());
    git(&root, &["push", "-q", "origin", "master"]);
    repository.cleanup(receipt.id).unwrap();
    assert!(!checkout.exists());
    assert_eq!(
        git(&root, &["show", "feat/integrated:tracked"]),
        "new commit"
    );
}
