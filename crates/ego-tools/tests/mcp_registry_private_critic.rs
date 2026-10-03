//! Round-ten critic cases for the private MCP diagnostic log adapter.
//! Unix only: symlinks, hard links and mode bits have no Windows equivalent here.
#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use ego_core::turn::McpRegistryJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::{OsIo, StoreIo};

const VICTIM: &str = "VICTIM-FILE-CONTENT-71c2";

fn adapter(root: &Path) -> JsonlMcpRegistryJournal {
    JsonlMcpRegistryJournal::with_private_diagnostic_root(
        root.join("registry"),
        root.join("private"),
        Box::new(OsIo),
    )
}

fn log(root: &Path) -> String {
    fs::read_to_string(root.join("private/errors.jsonl")).unwrap_or_default()
}

// Former RacyIo read-hook cases are superseded by the real OsIo races in
// private_log_regression_r11: live_swapping_the_log_leaf_never_writes_through_to_the_victim
// and live_replacing_the_private_directory_never_writes_into_the_decoy.
// Private append never calls StoreIo::read, so those hooks no longer ran.

/// Catches: no bound on one backend cause (the public path capped it at 256
/// characters); a hostile server fills the disk through one record.
#[test]
fn one_oversized_cause_cannot_become_an_oversized_record() {
    let dir = tempfile::tempdir().expect("dir");
    let journal = adapter(dir.path());
    let _ = journal.record_private_diagnostic("alpha", &"x".repeat(8 * 1024 * 1024));
    assert!(
        log(dir.path()).len() <= 1024 * 1024,
        "{}",
        log(dir.path()).len()
    );
}

/// Catches: unbounded retention; every failed refresh appends forever and each
/// append re-reads and rewrites the whole file.
#[test]
fn repeated_failures_do_not_grow_the_log_without_bound() {
    let dir = tempfile::tempdir().expect("dir");
    let journal = adapter(dir.path());
    let cause = "y".repeat(16 * 1024);
    for _ in 0..100 {
        journal
            .record_private_diagnostic("alpha", &cause)
            .expect("record");
    }
    assert!(
        log(dir.path()).len() <= 1024 * 1024,
        "{}",
        log(dir.path()).len()
    );
}

/// Catches: a cause with newlines or JSON forging an extra record.
#[test]
fn newline_bearing_cause_stays_inside_its_own_record() {
    let dir = tempfile::tempdir().expect("dir");
    let journal = adapter(dir.path());
    journal
        .record_private_diagnostic(
            "alpha",
            "a\n{\"server\":\"forged\",\"cause\":\"c\"}\r\n\u{2028}",
        )
        .expect("record");
    assert_eq!(log(dir.path()).lines().count(), 1);
}

/// Catches: concurrent adapters on distinct registry roots sharing one private
/// root losing or interleaving records.
#[test]
fn concurrent_adapters_sharing_a_private_root_lose_no_record() {
    let dir = tempfile::tempdir().expect("dir");
    let private = dir.path().join("private");
    let mut handles = Vec::new();
    for writer in 0..6 {
        let registry = dir.path().join(format!("registry-{writer}"));
        let private = private.clone();
        handles.push(std::thread::spawn(move || {
            let journal = JsonlMcpRegistryJournal::with_private_diagnostic_root(
                registry,
                private,
                Box::new(OsIo),
            );
            for index in 0..8 {
                journal
                    .record_private_diagnostic("alpha", &format!("w{writer}-{index}"))
                    .expect("record");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("writer");
    }
    assert_eq!(log(dir.path()).lines().count(), 48);
}

/// Catches: a private directory that is a symlink to a 0700 directory elsewhere
/// is trusted and written through.
#[test]
fn private_directory_symlink_is_refused_and_target_stays_empty() {
    let dir = tempfile::tempdir().expect("dir");
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir(&elsewhere).expect("dir");
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).expect("mode");
    symlink(&elsewhere, dir.path().join("private")).expect("link");
    assert!(
        adapter(dir.path())
            .record_private_diagnostic("alpha", "c")
            .is_err()
    );
    assert_eq!(fs::read_dir(&elsewhere).expect("read").count(), 0);
}

/// Catches: a failing storage layer whose io::Error text (carrying the cause)
/// reaches the typed error's Display or Debug.
#[test]
fn storage_error_text_never_reaches_the_typed_error() {
    struct Leaky;
    impl StoreIo for Leaky {
        fn append_private_log(&self, _: &Path, bytes: &[u8], _: u64) -> io::Result<()> {
            Err(io::Error::other(
                String::from_utf8_lossy(bytes).into_owned(),
            ))
        }
        fn append_line(&self, _: &Path, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn fsync(&self, _: &Path) -> io::Result<()> {
            Ok(())
        }
        fn read(&self, _: &Path) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn exists(&self, _: &Path) -> io::Result<bool> {
            Ok(false)
        }
        fn replace_atomic(&self, _: &Path, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn replace_atomic_private(&self, _: &Path, bytes: &[u8]) -> io::Result<()> {
            Err(io::Error::other(
                String::from_utf8_lossy(bytes).into_owned(),
            ))
        }
        fn truncate(&self, _: &Path, _: u64) -> io::Result<()> {
            Ok(())
        }
        fn remove_file(&self, _: &Path) -> io::Result<()> {
            Ok(())
        }
        fn list_dirs(&self, _: &Path) -> io::Result<Vec<std::path::PathBuf>> {
            Ok(Vec::new())
        }
    }
    let dir = tempfile::tempdir().expect("dir");
    let journal = JsonlMcpRegistryJournal::with_private_diagnostic_root(
        dir.path().join("registry"),
        dir.path().join("private"),
        Box::new(Leaky),
    );
    let error = journal
        .record_private_diagnostic("alpha", "LEAK-MARKER-55")
        .expect_err("fails");
    assert!(!format!("{error} {error:?}").contains("LEAK-MARKER-55"));
}
