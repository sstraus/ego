//! Round-thirteen critic cases for directory/leaf publication of the private
//! diagnostic log: exclusive rename, existing handles never chmod'd, orphans.
//!
//! Unix only: the cases use mode bits, symlinks and process re-execution.
#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Barrier};

use ego_tools::store::{OsIo, StoreIo};

fn append(root: &Path, line: &str, max: u64) -> io::Result<()> {
    OsIo.append_private_log(root, line.as_bytes(), max)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf8")
        })
        .collect();
    found.sort();
    found
}

fn chmod(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).expect("chmod");
}

/// Catches: an initializer that loses the publication race (or a first-use
/// opener) leaves its unique staging directory or staging leaf behind, or
/// fails because it saw the winner's half-prepared state.
#[test]
fn racing_first_use_threads_all_succeed_and_leave_no_staging_entries() {
    const THREADS: usize = 12;
    for round in 0..25 {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("private");
        let gate = Arc::new(Barrier::new(THREADS));
        let workers: Vec<_> = (0..THREADS)
            .map(|index| {
                let root = root.clone();
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    append(&root, &format!("{{\"n\":{index}}}"), 64 * 1024)
                })
            })
            .collect();
        for worker in workers {
            worker
                .join()
                .expect("worker")
                .unwrap_or_else(|error| panic!("round {round}: {error}"));
        }
        assert_eq!(
            names(dir.path()),
            ["private"],
            "round {round}: parent orphan"
        );
        assert_eq!(
            names(&root),
            ["errors.jsonl", "errors.lock"],
            "round {round}: leaf orphan"
        );
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("errors.jsonl")), 0o600);
        assert_eq!(mode(&root.join("errors.lock")), 0o600);
        let body = fs::read_to_string(root.join("errors.jsonl")).expect("log");
        assert_eq!(body.lines().count(), THREADS, "round {round}: lost record");
    }
}

/// Catches: hardening an existing canonical log through a chmod, which would
/// silently adopt a group-readable file another process owns/created.
#[test]
fn existing_group_readable_log_is_refused_and_never_chmodded() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    fs::create_dir(&root).expect("root");
    chmod(&root, 0o700);
    for (name, bits) in [("errors.lock", 0o600), ("errors.jsonl", 0o644)] {
        fs::write(root.join(name), b"").expect("seed");
        chmod(&root.join(name), bits);
    }
    let error = append(&root, "{}", 4096).expect_err("0644 log must be refused");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(mode(&root.join("errors.jsonl")), 0o644);
    assert_eq!(fs::read(root.join("errors.jsonl")).unwrap(), b"");
    assert_eq!(names(&root), ["errors.jsonl", "errors.lock"]);
}

/// Catches: the lock leaf being the one that is hardened after the fact, or a
/// refused existing lock still letting the log be created/written.
#[test]
fn existing_world_readable_lock_is_refused_before_the_log_is_created() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    fs::create_dir(&root).expect("root");
    chmod(&root, 0o700);
    fs::write(root.join("errors.lock"), b"").expect("lock");
    chmod(&root.join("errors.lock"), 0o666);
    append(&root, "{}", 4096).expect_err("0666 lock must be refused");
    assert_eq!(mode(&root.join("errors.lock")), 0o666);
    assert_eq!(names(&root), ["errors.lock"]);
}

/// Catches: an existing 0755 canonical root being chmod'd to 0700 (the
/// previous "harden the winner through its path" behaviour) or replaced.
#[test]
fn existing_open_root_is_refused_not_hardened_and_leaves_no_sibling() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    fs::create_dir(&root).expect("root");
    chmod(&root, 0o755);
    let error = append(&root, "{}", 4096).expect_err("0755 root must be refused");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(mode(&root), 0o755);
    assert_eq!(names(dir.path()), ["private"]);
    assert!(names(&root).is_empty());
}

/// Catches: publication following or replacing a dangling-symlink root, so
/// the diagnostics land in an attacker-chosen target.
#[test]
fn dangling_symlink_root_is_refused_and_its_target_never_created() {
    let dir = tempfile::tempdir().expect("dir");
    let target = dir.path().join("elsewhere");
    let root = dir.path().join("private");
    symlink(&target, &root).expect("symlink");
    append(&root, "{}", 4096).expect_err("symlink root must be refused");
    assert!(!target.exists(), "target created through symlink");
    assert!(
        fs::symlink_metadata(&root)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(names(dir.path()), ["private"]);
}

/// Catches: a dangling-symlink lock leaf being created through (O_CREAT of the
/// canonical name) or replaced by the exclusive rename.
#[test]
fn dangling_symlink_lock_leaf_is_refused_and_its_target_never_created() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    fs::create_dir(&root).expect("root");
    chmod(&root, 0o700);
    let target = dir.path().join("stolen");
    symlink(&target, root.join("errors.lock")).expect("symlink");
    append(&root, "{}", 4096).expect_err("symlink lock must be refused");
    assert!(!target.exists(), "target created through symlink");
    assert!(
        fs::symlink_metadata(root.join("errors.lock"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(names(&root), ["errors.lock"], "staging leaf left behind");
}

/// Catches: a regular file squatting on the root name being replaced or
/// worked around, or its failure leaving a staging directory in the parent.
#[test]
fn regular_file_at_the_root_name_is_refused_unchanged_without_orphans() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    fs::write(&root, b"keep").expect("squatter");
    append(&root, "{}", 4096).expect_err("file root must be refused");
    assert_eq!(fs::read(&root).unwrap(), b"keep");
    assert_eq!(names(dir.path()), ["private"]);
}

/// Catches: the retention check running after directory preparation, so a
/// refused oversize record still creates the directory (state left behind).
#[test]
fn oversize_record_creates_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    append(&root, &"x".repeat(64), 16).expect_err("record exceeds retention");
    assert!(names(dir.path()).is_empty());
}

/// Catches: a bare relative root ("private", parent "") feeding an empty path
/// to the sibling-staging creation or the exclusive rename.
#[test]
fn bare_relative_root_publishes_in_the_current_directory() {
    const CHILD: &str = "EGO_PRIVATE_LOG_R13_CWD";
    if std::env::var_os(CHILD).is_some() {
        append(Path::new("private"), "{\"rel\":1}", 4096).expect("relative first use");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let status = Command::new(std::env::current_exe().expect("exe"))
        .args([
            "--exact",
            "bare_relative_root_publishes_in_the_current_directory",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .current_dir(dir.path())
        .status()
        .expect("child");
    assert!(status.success(), "relative-root child failed");
    assert_eq!(names(dir.path()), ["private"]);
    assert_eq!(mode(&dir.path().join("private")), 0o700);
    assert_eq!(
        fs::read(dir.path().join("private/errors.jsonl")).unwrap(),
        b"{\"rel\":1}\n"
    );
}

/// Catches: missing intermediate parents not being created before staging
/// (staging sibling needs an existing parent), failing the first append.
#[test]
fn missing_nested_parents_are_created_before_publication() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("a/b/c/private");
    append(&root, "{}", 4096).expect("nested first use");
    assert_eq!(names(&dir.path().join("a/b/c")), ["private"]);
    assert_eq!(mode(&root), 0o700);
}
