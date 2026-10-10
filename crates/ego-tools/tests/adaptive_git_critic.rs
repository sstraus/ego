//! Native critic regressions for Adaptive Git metadata floors.
#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::permit::{CanonPath, Confinement, EnvPolicy, SpawnSpec};
use ego_core::turn::CancelToken;
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SessionEvidenceStore,
    admit_confinement,
};

/// Runs `name` again in a child test process with a task-owned operator
/// environment. Returns the root inside that child, `None` in the parent.
fn isolated(name: &str) -> Option<PathBuf> {
    if std::env::var("EGO_GIT_CRITIC_CASE").as_deref() == Ok(name) {
        return Some(PathBuf::from(
            std::env::var_os("EGO_GIT_CRITIC_ROOT").unwrap(),
        ));
    }
    let root = tempfile::Builder::new()
        .prefix(".ego-git-critic-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    for dir in ["home", "workspace", "tmp", "cargo", "rustup", "ego"] {
        std::fs::create_dir(root.path().join(dir)).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("EGO_GIT_CRITIC_CASE", name)
        .env("EGO_GIT_CRITIC_ROOT", root.path())
        .env("HOME", root.path().join("home"))
        .env("TMPDIR", root.path().join("tmp"))
        .env("CARGO_HOME", root.path().join("cargo"))
        .env("RUSTUP_HOME", root.path().join("rustup"))
        .env("EGO_HOME", root.path().join("ego"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

/// Host-side Git, outside the sandbox.
fn git(workspace: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit(workspace: &Path, file: &str) {
    std::fs::write(workspace.join(file), file).unwrap();
    git(workspace, &["add", file]);
    git(workspace, &["commit", "-q", "-m", file]);
}

/// One confined spawn of a production Adaptive run.
fn confined(workspace: &Path, root: &Path, script: &str) -> String {
    let policy = admit_confinement(
        Confinement::Adaptive,
        false,
        CanonPath::new(std::fs::canonicalize(workspace).unwrap()),
    )
    .unwrap();
    let evidence =
        SessionEvidenceStore::open(root.join("evidence"), SessionId::new(), 1024 * 1024).unwrap();
    let spec = SpawnSpec::new(
        vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
        ],
        Some(policy.workspace().clone()),
        Some(Duration::from_secs(10)),
        EnvPolicy::Inherited,
    )
    .unwrap();
    let config = ExecRunConfig::new(
        policy,
        evidence,
        CancelToken::new(),
        ExecLimits::default(),
        HttpCallTimeout::default(),
        None,
        Arc::new(ego_tools::http::SystemResolver::new()),
    );
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (ports, _, _) = RunEffectPorts::for_run(config).await.unwrap();
            ports.process().execute(&spec).await
        });
    format!("{result:?}")
}

fn repository(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    git(&workspace, &["init", "-q", "-b", "main"]);
    git(&workspace, &["config", "user.name", "critic"]);
    git(
        &workspace,
        &["config", "user.email", "critic@example.invalid"],
    );
    git(&workspace, &["config", "commit.gpgsign", "false"]);
    commit(&workspace, "base");
    workspace
}

// Catches: the directory-unlink floor covers every directory below .git, so a
// confined `git rebase` cannot remove .git/rebase-merge when it finishes and
// the human's repository stays in "rebase in progress".
#[test]
fn adaptive_confined_git_rebase_finishes_and_leaves_no_rebase_state() {
    let Some(root) = isolated("adaptive_confined_git_rebase_finishes_and_leaves_no_rebase_state")
    else {
        return;
    };
    let workspace = repository(&root);
    git(&workspace, &["switch", "-q", "-c", "topic"]);
    commit(&workspace, "topic");
    git(&workspace, &["switch", "-q", "main"]);
    commit(&workspace, "upstream");
    git(&workspace, &["switch", "-q", "topic"]);

    let result = confined(&workspace, &root, "git rebase --merge main");

    assert!(
        !workspace.join(".git/rebase-merge").exists(),
        "confined rebase left its state directory behind: {result}"
    );
    assert_eq!(
        git(&workspace, &["rev-parse", "topic~1"]),
        git(&workspace, &["rev-parse", "main"]),
        "{result}"
    );
}
