//! Real Git fixtures: no model, network, global Git config, or user index mutation.
use std::fs;
use std::path::Path;
use std::process::Command;

use ego_core::event::id::SessionId;
use ego_core::permit::{CanonPath, Fs};
use ego_tools::checkpoint::CheckpointFs;
use tempfile::{TempDir, tempdir};

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn repository() -> TempDir {
    let directory = tempdir().unwrap();
    git(directory.path(), &["init", "--quiet"]);
    directory
}

fn write(fs: &CheckpointFs, path: &Path, contents: &[u8]) {
    let parent = fs::canonicalize(path.parent().unwrap()).unwrap();
    fs.write(
        &CanonPath::new(parent.join(path.file_name().unwrap())),
        contents,
    )
    .unwrap();
}

#[test]
fn selective_restore_preserves_staged_unstaged_and_unrelated_files() {
    // Catches: snapshotting HEAD/index instead of the actual pre-edit dirty bytes.
    let repo = repository();
    let root = repo.path();
    fs::write(root.join("target"), b"staged").unwrap();
    git(root, &["add", "target"]);
    let index = git(root, &["ls-files", "--stage"]);
    fs::write(root.join("target"), b"dirty before").unwrap();
    fs::write(root.join("unrelated"), b"keep").unwrap();
    let session = SessionId::new();
    write(
        &CheckpointFs::new(session),
        &root.join("target"),
        b"agent result",
    );
    let preview = CheckpointFs::preview(root, session).unwrap();
    assert_eq!(preview.len(), 1);
    assert!(!preview[0].conflict);
    CheckpointFs::restore(root, session, &preview[0].checkpoint).unwrap();
    assert_eq!(fs::read(root.join("target")).unwrap(), b"dirty before");
    assert_eq!(fs::read(root.join("unrelated")).unwrap(), b"keep");
    assert_eq!(git(root, &["ls-files", "--stage"]), index);
}

#[test]
fn binary_preimage_and_new_file_deletion_survive_adapter_restart() {
    // Catches: lossy UTF-8 capture and loss of restore state at process exit.
    let repo = repository();
    let session = SessionId::new();
    let path = repo.path().join("binary");
    fs::write(&path, [0, 255, 128, 0]).unwrap();
    write(&CheckpointFs::new(session), &path, b"text");
    let id = CheckpointFs::preview(repo.path(), session)
        .unwrap()
        .remove(0)
        .checkpoint;
    CheckpointFs::restore(repo.path(), session, &id).unwrap();
    assert_eq!(fs::read(path).unwrap(), [0, 255, 128, 0]);
    let created = repo.path().join("created");
    write(&CheckpointFs::new(session), &created, b"new");
    let preview = CheckpointFs::preview(repo.path(), session).unwrap();
    assert_eq!(preview[0].effect, "delete");
    CheckpointFs::restore(repo.path(), session, &preview[0].checkpoint).unwrap();
    assert!(!created.exists());
}

#[test]
fn external_edit_or_delete_refuses_restore_without_touching_worktree() {
    // Catches: checkout overwriting a later editor save or recreating an external deletion.
    let repo = repository();
    let session = SessionId::new();
    let path = repo.path().join("target");
    fs::write(&path, b"before").unwrap();
    write(&CheckpointFs::new(session), &path, b"after");
    let id = CheckpointFs::preview(repo.path(), session)
        .unwrap()
        .remove(0)
        .checkpoint;
    fs::write(&path, b"external").unwrap();
    assert!(CheckpointFs::restore(repo.path(), session, &id).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"external");
    fs::remove_file(&path).unwrap();
    assert!(CheckpointFs::restore(repo.path(), session, &id).is_err());
    assert!(!path.exists());
    // Explicit resolution after deletion: the operator preserves external changes
    // and manually restores the preimage (available as COMMIT:before).
    // A recreated postimage is not the inode owned by ego. No force flag exists.
    fs::write(&path, b"before").unwrap();
    assert!(
        CheckpointFs::restore(repo.path(), session, &id)
            .unwrap()
            .already_original
    );
    assert_eq!(fs::read(path).unwrap(), b"before");
}

#[test]
fn child_checkpoint_cannot_be_restored_or_closed_by_parent() {
    // Catches: parent/child sharing a mutable checkpoint namespace.
    let repo = repository();
    let parent = SessionId::new();
    let child = SessionId::new();
    write(
        &CheckpointFs::new(child),
        &repo.path().join("child"),
        b"owned",
    );
    let id = CheckpointFs::preview(repo.path(), child)
        .unwrap()
        .remove(0)
        .checkpoint;
    assert!(
        CheckpointFs::preview(repo.path(), parent)
            .unwrap()
            .is_empty()
    );
    assert!(CheckpointFs::restore(repo.path(), parent, &id).is_err());
    CheckpointFs::close(repo.path(), parent).unwrap();
    assert_eq!(CheckpointFs::preview(repo.path(), child).unwrap().len(), 1);
    CheckpointFs::close(repo.path(), child).unwrap();
    assert!(
        CheckpointFs::preview(repo.path(), child)
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read(repo.path().join("child")).unwrap(), b"owned");
}

#[test]
fn successive_edits_restore_in_reverse_order_and_retries_are_idempotent() {
    // Catches: losing an earlier preimage or refusing a completed restore after restart.
    let repo = repository();
    let session = SessionId::new();
    let adapter = CheckpointFs::new(session);
    let path = repo.path().join("target");
    fs::write(&path, b"zero").unwrap();
    write(&adapter, &path, b"one");
    write(&adapter, &path, b"two");
    let previews = CheckpointFs::preview(repo.path(), session).unwrap();
    assert!(previews[1].conflict);
    CheckpointFs::restore(repo.path(), session, &previews[0].checkpoint).unwrap();
    assert!(
        CheckpointFs::restore(repo.path(), session, &previews[0].checkpoint)
            .unwrap()
            .already_original
    );
    CheckpointFs::restore(repo.path(), session, &previews[1].checkpoint).unwrap();
    assert_eq!(fs::read(path).unwrap(), b"zero");
}

#[test]
fn concurrent_session_writes_keep_all_checkpoint_objects_reachable() {
    // Catches: last-writer-wins ref publication losing another builtin write's preimage.
    let repo = repository();
    let session = SessionId::new();
    std::thread::scope(|scope| {
        for index in 0..4 {
            let path = repo.path().join(format!("file-{index}"));
            scope.spawn(move || write(&CheckpointFs::new(session), &path, b"value"));
        }
    });
    assert_eq!(
        CheckpointFs::preview(repo.path(), session).unwrap().len(),
        4
    );
}

#[test]
fn outside_git_write_has_explicit_no_checkpoint_receipt() {
    // Catches: implying a file is recoverable when no Git object store exists.
    let root = tempdir().unwrap();
    let session = SessionId::new();
    let adapter = CheckpointFs::new(session);
    let path = root.path().join("file");
    write(&adapter, &path, b"contents");
    assert!(
        adapter
            .write_notice(&CanonPath::new(path))
            .unwrap()
            .contains("No checkpoint")
    );
    assert!(CheckpointFs::preview(root.path(), session).is_err());
}

#[cfg(unix)]
#[test]
fn external_symlink_and_mode_changes_are_not_overwritten() {
    // Catches: restore following a replacement symlink or undoing an external chmod.
    use std::os::unix::fs::{PermissionsExt, symlink};
    let repo = repository();
    let session = SessionId::new();
    let path = repo.path().join("target");
    fs::write(&path, b"before").unwrap();
    let permissions = fs::metadata(&path).unwrap().permissions();
    write(&CheckpointFs::new(session), &path, b"after");
    let id = CheckpointFs::preview(repo.path(), session)
        .unwrap()
        .remove(0)
        .checkpoint;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(CheckpointFs::restore(repo.path(), session, &id).is_err());
    fs::set_permissions(&path, permissions).unwrap();
    fs::remove_file(&path).unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("file"), b"outside").unwrap();
    symlink(outside.path().join("file"), &path).unwrap();
    assert!(CheckpointFs::preview(repo.path(), session).unwrap()[0].conflict);
    assert!(CheckpointFs::restore(repo.path(), session, &id).is_err());
    assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"outside");
}

#[test]
fn identical_bytes_from_external_atomic_replacement_are_a_conflict() {
    // Catches: trusting content equality after another editor replaced the file identity.
    let repo = repository();
    let session = SessionId::new();
    let path = repo.path().join("target");
    fs::write(&path, b"before").unwrap();
    write(&CheckpointFs::new(session), &path, b"after");
    let id = CheckpointFs::preview(repo.path(), session)
        .unwrap()
        .remove(0)
        .checkpoint;
    let staged = tempfile::NamedTempFile::new_in(repo.path()).unwrap();
    fs::write(staged.path(), b"after").unwrap();
    // Match permissions so only identity, not bytes or mode, distinguishes this save.
    fs::set_permissions(staged.path(), fs::metadata(&path).unwrap().permissions()).unwrap();
    staged.persist(&path).unwrap();
    assert!(CheckpointFs::restore(repo.path(), session, &id).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"after");
}

#[cfg(unix)]
#[test]
fn checkpoint_capture_restore_and_close_never_execute_repository_hooks() {
    // Catches: private update-ref operations executing repository reference-transaction hooks.
    use std::os::unix::fs::PermissionsExt;
    let repo = repository();
    let root = repo.path();
    let hooks = root.join("hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    fs::write(&hook, "#!/bin/sh\ncat >/dev/null\ntouch hook-ran\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    git(root, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let marker = root.join("hook-ran");
    let session = SessionId::new();
    let path = root.join("target");
    fs::write(&path, b"before").unwrap();
    write(&CheckpointFs::new(session), &path, b"after");
    assert!(
        !marker.exists(),
        "checkpoint capture executed a repository hook"
    );
    let id = CheckpointFs::preview(root, session)
        .unwrap()
        .remove(0)
        .checkpoint;
    CheckpointFs::restore(root, session, &id).unwrap();
    assert!(
        !marker.exists(),
        "checkpoint restore executed a repository hook"
    );
    CheckpointFs::close(root, session).unwrap();
    assert!(
        !marker.exists(),
        "checkpoint close executed a repository hook"
    );
    assert_eq!(fs::read(path).unwrap(), b"before");
}

#[test]
fn a_disabled_adapter_writes_the_file_and_creates_no_ref_or_note_of_a_checkpoint() {
    // Catches: with_enabled(false) still captures into the user's repository, or the tool
    // result keeps claiming a checkpoint was saved.
    let repo = repository();
    let root = repo.path();
    fs::write(root.join("target"), b"before").unwrap();
    let session = SessionId::new();
    let adapter = CheckpointFs::new(session).with_enabled(false);
    write(&adapter, &root.join("target"), b"after");
    assert_eq!(fs::read(root.join("target")).unwrap(), b"after");
    assert_eq!(git(root, &["for-each-ref", "refs/ego/checkpoints"]), b"");
    assert!(CheckpointFs::preview(root, session).unwrap().is_empty());
    let parent = fs::canonicalize(root).unwrap();
    let notice = adapter
        .write_notice(&CanonPath::new(parent.join("target")))
        .unwrap();
    assert!(notice.contains("checkpoints are off"), "{notice}");
}
