//! Owner-file privacy and accepted best-effort retention contract.
use crate::private_logs;

use ego_tools::store::{OsIo, StoreIo};
use std::fs::{self, File, FileTimes};
use std::path::Path;
use std::time::{Duration, SystemTime};

const CAP: u64 = 128 * 1024;

fn set_age(path: &Path, age: Duration) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now() - age))
        .unwrap();
}

// Catches: reopening a previous owner's file or truncating its retained records.
#[test]
fn every_owner_creates_a_distinct_file_and_retains_it_after_drop() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    for index in 0..3 {
        let mut owner = OsIo.open_private_log(&root, CAP).unwrap();
        owner.append(format!("owner-{index}").as_bytes()).unwrap();
    }
    let files = private_logs::files(&root);
    assert_eq!(files.len(), 3);
    let mut records: Vec<_> = files
        .iter()
        .map(|p| fs::read_to_string(p).unwrap())
        .collect();
    records.sort();
    assert_eq!(records, ["owner-0\n", "owner-1\n", "owner-2\n"]);
}

// Catches: rotation/reopening after exhaustion or a newline crossing the cap.
#[test]
fn owner_cap_discards_later_records_without_rotation_or_growth() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    let mut owner = OsIo.open_private_log(&root, 8).unwrap();
    owner.append(b"1234567").unwrap();
    for _ in 0..100 {
        owner.append(b"fresh").unwrap();
    }
    assert_eq!(private_logs::bytes(&root), b"1234567\n");
    assert_eq!(private_logs::files(&root).len(), 1);
    assert!(owner.append(b"12345678").is_err());
    assert_eq!(private_logs::bytes(&root).len(), 8);
}

// Catches: per-owner caps leaving unlimited retained files under closed-session churn.
#[test]
fn sequential_owner_churn_prunes_to_sixteen_capped_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    for _ in 0..50 {
        let mut owner = OsIo.open_private_log(&root, 64).unwrap();
        owner.append(&[b'x'; 63]).unwrap();
    }
    assert_eq!(private_logs::files(&root).len(), 16);
    assert_eq!(private_logs::bytes(&root).len(), 16 * 64);
}

// Catches: pruning arbitrary directory order instead of oldest mtime first.
#[test]
fn prune_removes_oldest_mtime_and_keeps_newer_closed_owners() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    let mut files = Vec::new();
    for index in 0..16 {
        let mut owner = OsIo.open_private_log(&root, CAP).unwrap();
        owner.append(format!("{index}").as_bytes()).unwrap();
        let path = private_logs::files(&root)
            .into_iter()
            .find(|p| !files.contains(p))
            .unwrap();
        set_age(&path, Duration::from_secs((16 - index) * 3600));
        files.push(path);
    }
    drop(OsIo.open_private_log(&root, CAP).unwrap());
    assert!(!files[0].exists());
    assert!(files[1..].iter().all(|p| p.exists()));
    assert_eq!(private_logs::files(&root).len(), 16);
}

// Catches: concurrent cleanup ENOENT becoming an owner admission error, or
// owner names colliding and appending through another thread's handle.
#[test]
fn concurrent_creators_and_pruners_keep_exclusive_capped_owner_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    drop(OsIo.open_private_log(&root, 64).unwrap());
    std::thread::scope(|scope| {
        for index in 0..12 {
            let root = &root;
            scope.spawn(move || {
                for _ in 0..8 {
                    let mut owner = OsIo.open_private_log(root, 64).unwrap();
                    owner.append(format!("writer-{index}").as_bytes()).unwrap();
                }
            });
        }
    });
    // A final start converges the explicitly approximate concurrent target.
    drop(OsIo.open_private_log(&root, 64).unwrap());
    assert_eq!(private_logs::files(&root).len(), 16);
    for path in private_logs::files(&root) {
        let bytes = fs::read(path).unwrap();
        assert!(bytes.len() <= 64);
        assert!(bytes.is_empty() || String::from_utf8(bytes).unwrap().lines().count() == 1);
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::{PermissionsExt, symlink};

    // Catches: cleanup reopening a live file, rejecting its legitimate zero
    // link count or writing past the owner's cap after accepted unlink.
    #[test]
    fn pruning_a_live_owner_unlinks_it_but_its_pinned_handle_remains_capped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let mut active = OsIo.open_private_log(&root, 16).unwrap();
        active.append(b"first").unwrap();
        let path = private_logs::files(&root).pop().unwrap();
        let mut observer = File::open(&path).unwrap();
        set_age(&path, Duration::from_secs(8 * 24 * 3600));
        for _ in 0..15 {
            drop(OsIo.open_private_log(&root, 16).unwrap());
        }
        drop(OsIo.open_private_log(&root, 16).unwrap());
        assert!(!path.exists());
        active.append(b"second").unwrap();
        active.append(b"third").unwrap();
        let mut bytes = Vec::new();
        observer.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"first\nsecond\n");
        assert_eq!(private_logs::bytes(&root), b"");
    }

    // Catches: a leaf pathname replacement redirecting later raw diagnostics.
    #[test]
    fn replacing_the_owner_leaf_never_writes_through_the_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let mut owner = OsIo.open_private_log(&root, CAP).unwrap();
        owner.append(b"first").unwrap();
        let path = private_logs::files(&root).pop().unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&victim, &path).unwrap();
        owner.append(b"RAW-SECOND").unwrap();
        assert_eq!(fs::read(victim).unwrap(), b"unchanged");
    }

    // Catches: root pathname replacement redirecting a retained owner's writes
    // to a decoy rather than preserving the pinned directory/file.
    #[test]
    fn replacing_the_directory_does_not_redirect_an_existing_owner() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let mut owner = OsIo.open_private_log(&root, CAP).unwrap();
        owner.append(b"first").unwrap();
        let retained = temp.path().join("retained");
        fs::rename(&root, &retained).unwrap();
        let decoy = temp.path().join("decoy");
        fs::create_dir(&decoy).unwrap();
        symlink(&decoy, &root).unwrap();
        owner.append(b"second").unwrap();
        assert_eq!(private_logs::bytes(&retained), b"first\nsecond\n");
        assert_eq!(fs::read_dir(decoy).unwrap().count(), 0);
        assert!(OsIo.open_private_log(&root, CAP).is_err());
    }

    // Catches: trusting a symlink/file/FIFO or chmodding an existing wide-mode
    // root instead of rejecting it before raw bytes are created.
    #[test]
    fn unsafe_root_types_and_permissions_are_refused_unchanged() {
        for kind in ["symlink", "file", "fifo", "wide"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("private");
            match kind {
                "symlink" => symlink(temp.path().join("elsewhere"), &root).unwrap(),
                "file" => fs::write(&root, b"unchanged").unwrap(),
                "fifo" => nix::unistd::mkfifo(&root, nix::sys::stat::Mode::S_IRUSR).unwrap(),
                "wide" => {
                    fs::create_dir(&root).unwrap();
                    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(OsIo.open_private_log(&root, CAP).is_err(), "{kind}");
            if kind == "file" {
                assert_eq!(fs::read(&root).unwrap(), b"unchanged");
            }
            if kind == "wide" {
                assert_eq!(
                    fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            }
        }
    }

    // Catches: cleanup following a symlink, blocking on a FIFO, or modifying
    // outside contents rather than unlinking only directory entries.
    #[test]
    fn prune_never_follows_symlinks_or_opens_nonregular_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        drop(OsIo.open_private_log(&root, CAP).unwrap());
        let victim = temp.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, root.join("owner-symlink.jsonl")).unwrap();
        nix::unistd::mkfifo(
            &root.join("owner-fifo.jsonl"),
            nix::sys::stat::Mode::S_IRUSR,
        )
        .unwrap();
        drop(OsIo.open_private_log(&root, CAP).unwrap());
        assert_eq!(fs::read(victim).unwrap(), b"unchanged");
        assert!(
            fs::symlink_metadata(root.join("owner-symlink.jsonl"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
