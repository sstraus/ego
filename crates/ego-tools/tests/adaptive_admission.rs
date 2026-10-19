//! Public admission regressions with task-owned operator environments.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use ego_core::permit::{CanonPath, Confinement};
use ego_tools::exec::admit_confinement;

#[path = "support/admission_fixture.rs"]
mod admission_fixture;
#[path = "support/reexec.rs"]
mod reexec;

fn isolated(name: &str) -> Option<PathBuf> {
    if std::env::var("EGO_ADMISSION_CASE").as_deref() == Ok(name) {
        return Some(PathBuf::from(
            std::env::var_os("EGO_ADMISSION_ROOT").unwrap(),
        ));
    }
    let root = admission_fixture::root();
    for dir in [
        "home",
        "workspace",
        "tmp",
        "cargo",
        "rustup",
        "ego",
        "github",
    ] {
        std::fs::create_dir(root.path().join(dir)).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("EGO_ADMISSION_CASE", name)
        .env("EGO_ADMISSION_ROOT", root.path())
        .env("HOME", root.path().join("home"))
        .env("TMPDIR", root.path().join("tmp"))
        .env("CARGO_HOME", root.path().join("cargo"))
        .env("RUSTUP_HOME", root.path().join("rustup"))
        .env("EGO_HOME", root.path().join("ego"))
        .env("GH_CONFIG_DIR", root.path().join("github"))
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("PNPM_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("npm_config_cache")
        .env_remove("NPM_CONFIG_CACHE")
        .output()
        .unwrap();
    reexec::assert_one_child_test(&output);
    None
}

fn admit(
    workspace: &Path,
) -> Result<ego_core::permit::RunConfinement, ego_core::permit::ProcessError> {
    admit_confinement(
        Confinement::Adaptive,
        false,
        CanonPath::new(std::fs::canonicalize(workspace).unwrap()),
    )
}

fn git_at(root: &Path, args: &[&std::ffi::OsStr]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[path = "support/adaptive_exec.rs"]
mod adaptive_exec;
use adaptive_exec::{execute, execute_with_desk, execute_with_grants};
#[path = "support/adaptive_grants.rs"]
mod adaptive_grants;
#[path = "support/adaptive_instruction_grants.rs"]
mod adaptive_instruction_grants;
#[path = "support/adaptive_residual.rs"]
mod adaptive_residual;
#[path = "support/adaptive_git_controls.rs"]
mod git_controls;
#[path = "support/adaptive_git_layouts.rs"]
mod git_layouts;
#[path = "support/adaptive_js_caches.rs"]
mod js_caches;
#[path = "support/adaptive_js_host_code.rs"]
mod js_host_code;

// Catches: the outside probe uses only the workspace parent even when that
// parent is host-unwritable.
#[test]
fn adaptive_probe_uses_safe_outside_location_for_locked_workspace() {
    use std::os::unix::fs::PermissionsExt;
    let Some(root) = isolated("adaptive_probe_uses_safe_outside_location_for_locked_workspace")
    else {
        return;
    };
    let locked = root.join("locked");
    std::fs::create_dir(&locked).unwrap();
    let workspace = locked.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = std::panic::catch_unwind(|| {
        execute(
            admit(&workspace).unwrap(),
            &root,
            vec![
                "/usr/bin/touch".into(),
                workspace.join("created").into_os_string(),
            ],
        )
    });
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.unwrap().is_ok());
    assert!(workspace.join("created").is_file());
}

// Catches: production admission omits validation and opens broad or dangling operator roots.
#[test]
fn adaptive_sources_reject_broad_roots_and_dangling_aliases() {
    let Some(root) = isolated("adaptive_sources_reject_broad_roots_and_dangling_aliases") else {
        return;
    };
    if std::env::var_os("EGO_INVALID_SOURCE").is_some() {
        let error = admit(&root.join("workspace")).expect_err("unsafe operator root admitted");
        let message = error.to_string();
        assert!(
            message.contains("hard write floor") || message.contains("dangling symlink"),
            "{message}"
        );
        return;
    }
    let workspace = root.join("workspace");
    let alias = root.join("alias");
    std::os::unix::fs::symlink(root.join("absent"), &alias).unwrap();
    // A valid environment must admit before each invalid source is tried.
    assert!(admit(&workspace).is_ok());
    for source in [root.join("home"), root.clone(), alias] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "adaptive_sources_reject_broad_roots_and_dangling_aliases",
                "--nocapture",
            ])
            .env("EGO_INVALID_SOURCE", "1")
            .env("CARGO_HOME", source)
            .output()
            .unwrap();
        reexec::assert_one_child_test(&output);
    }
}

// Catches: malformed linked-worktree metadata admits unrelated writable metadata.
#[test]
fn adaptive_git_sources_validate_the_real_worktree_back_pointer() {
    let Some(root) = isolated("adaptive_git_sources_validate_the_real_worktree_back_pointer")
    else {
        return;
    };
    let main = root.join("main");
    let linked = root.join("linked");
    let git = |args: &[&std::ffi::OsStr]| {
        let output = Command::new("git").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init".as_ref(), main.as_os_str()]);
    git(&[
        "-C".as_ref(),
        main.as_os_str(),
        "worktree".as_ref(),
        "add".as_ref(),
        "--orphan".as_ref(),
        "-b".as_ref(),
        "fixture".as_ref(),
        linked.as_os_str(),
    ]);
    std::fs::write(linked.join("tracked"), b"fixture").unwrap();
    execute(
        admit(&linked).unwrap(),
        &root,
        vec![
            "git".into(),
            "-C".into(),
            linked.clone().into_os_string(),
            "add".into(),
            "tracked".into(),
        ],
    )
    .unwrap();
    let output = Command::new("git")
        .arg("-C")
        .arg(&linked)
        .args(["diff", "--cached", "--name-only"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "tracked\n");
    let marker = std::fs::read_to_string(linked.join(".git")).unwrap();
    let metadata = PathBuf::from(marker.trim().strip_prefix("gitdir: ").unwrap());
    std::fs::write(metadata.join("gitdir"), main.join(".git").to_str().unwrap()).unwrap();
    let error = admit(&linked).expect_err("malformed back pointer admitted");
    assert!(
        error.to_string().contains("back pointer does not match"),
        "{error}"
    );
}

// Catches: GH_CONFIG_DIR and the legacy Cargo store escape the credential floor.
#[test]
fn adaptive_denies_relocated_github_and_both_cargo_token_spellings() {
    let Some(root) = isolated("adaptive_denies_relocated_github_and_both_cargo_token_spellings")
    else {
        return;
    };
    for (directory, relative, variable) in [
        ("github", "hosts.yml", "GH_CONFIG_DIR"),
        ("home", ".cargo/credentials", "HOME"),
        ("cargo", "credentials", "CARGO_HOME"),
        ("cargo", "credentials.toml", "CARGO_HOME"),
    ] {
        let path = root.join(directory).join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"synthetic").unwrap();
        // Internal path lookup reaches Seatbelt without the argv credential guard.
        let script = format!("exec /bin/cat \"${variable}/$1\"");
        let result = execute(
            admit(&root.join("workspace")).unwrap(),
            &root,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "probe".into(),
                relative.into(),
            ],
        );
        assert!(
            matches!(result, Err(ego_core::permit::ProcessError::Exit { .. })),
            "credential read was not refused by the child: {result:?}"
        );
    }
    let ordinary = root.join("home/instruction.txt");
    std::fs::write(&ordinary, b"ordinary").unwrap();
    execute(
        admit(&root.join("workspace")).unwrap(),
        &root,
        vec!["/bin/cat".into(), ordinary.into_os_string()],
    )
    .unwrap();
}

#[path = "support/adaptive_toolchains.rs"]
mod toolchains;

#[path = "support/adaptive_fixture_repositories.rs"]
mod fixture_repositories;

// Catches: a renamed child test silently succeeds without running any tests.
#[test]
#[should_panic(expected = "child must pass exactly one test")]
fn reexec_rejects_missing_child_test() {
    isolated("regression_case");
}

// Catches: a stricter child oracle rejects a real single-test run.
#[test]
fn reexec_accepts_one_successful_child_test() {
    isolated("reexec_accepts_one_successful_child_test");
}

// Catches: child process failure is accepted based on its output alone.
#[test]
#[should_panic(expected = "child must pass exactly one test")]
fn reexec_rejects_failed_child_process() {
    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--invalid-libtest-option-for-regression")
        .output()
        .unwrap();
    reexec::assert_one_child_test(&output);
}
