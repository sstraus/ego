//! regression critic cases for the owner-file private diagnostic log (regression case).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ego_tools::store::{OsIo, StoreIo};

const CAP: u64 = 4096;

fn files(root: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// Catches: prune keeping every pre-existing file (or counting the new owner file
// twice), so aggregate retention grows past 16 files x cap.
#[test]
fn opening_prunes_old_owner_files_to_the_retention_count() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    drop(OsIo.open_private_log(&root, CAP).unwrap());
    for index in 0..40 {
        let path = root.join(format!("old-{index}.jsonl"));
        fs::write(&path, b"secret\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut writer = OsIo.open_private_log(&root, CAP).unwrap();
    writer.append(b"raw").unwrap();
    let all = files(&root);
    assert!(all.len() <= 16, "retained {} files", all.len());
    assert!(all.iter().all(|path| mode(path) == 0o600));
    assert_eq!(mode(&root), 0o700);
}

// Catches: an unlink of a live owner file (pruned by a later opener) making the
// surviving writer error, recreate the path with a wider mode, or reopen by name.
#[test]
fn pruning_a_live_owner_file_leaves_the_writer_private_and_the_dir_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    let mut first = OsIo.open_private_log(&root, CAP).unwrap();
    first.append(b"first").unwrap();
    let mut others = Vec::new();
    for _ in 0..24 {
        others.push(OsIo.open_private_log(&root, CAP).unwrap());
    }
    first.append(b"after-unlink").unwrap();
    let all = files(&root);
    assert!(all.len() <= 16 + others.len());
    assert!(all.iter().all(|path| mode(path) == 0o600));
    assert_eq!(mode(&root), 0o700);
}

// Catches: off-by-one in the cap accounting (newline not charged) that lets one
// owner file exceed its 128 KiB-class cap, or an overflow that errors instead of
// discarding.
#[test]
fn owner_file_never_exceeds_cap_and_overflow_is_discarded_not_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    let mut writer = OsIo.open_private_log(&root, CAP).unwrap();
    let fits = vec![b'a'; usize::try_from(CAP).unwrap() - 1];
    writer.append(&fits).unwrap();
    writer.append(b"b").unwrap();
    writer.append(&fits).unwrap();
    let path = files(&root).pop().unwrap();
    assert_eq!(fs::metadata(path).unwrap().len(), CAP);
}

// Catches: a symlink or FIFO planted in the private directory being followed,
// opened (FIFO blocks) or deleted by prune, or counted as a retained owner file.
#[test]
fn prune_ignores_symlinks_and_fifos() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    drop(OsIo.open_private_log(&root, CAP).unwrap());
    let victim = temp.path().join("victim");
    fs::write(&victim, b"keep").unwrap();
    for index in 0..20 {
        std::os::unix::fs::symlink(&victim, root.join(format!("link-{index}"))).unwrap();
    }
    nix::unistd::mkfifo(&root.join("fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
    let _writer = OsIo.open_private_log(&root, CAP).unwrap();
    assert_eq!(fs::read(&victim).unwrap(), b"keep");
    assert!(
        fs::symlink_metadata(root.join("link-0"))
            .unwrap()
            .is_symlink()
    );
    assert!(fs::symlink_metadata(root.join("fifo")).is_ok());
}
