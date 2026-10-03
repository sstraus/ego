//! Round-ten critic cases for the private MCP diagnostic log adapter.

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

/// Delegates to `OsIo`, running `before_read` just before `read` of the log: the
/// window between the adapter's metadata check and its use of the path.
struct RacyIo<F: Fn(&Path) + Send + Sync>(F);

impl<F: Fn(&Path) + Send + Sync> StoreIo for RacyIo<F> {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        OsIo.fsync(path)
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        (self.0)(path);
        OsIo.read(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.replace_atomic(path, bytes)
    }
    fn replace_atomic_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.replace_atomic_private(path, bytes)
    }
}

/// Catches: the log path swapped for a symlink after the check; the adapter's
/// read follows it and copies the target's bytes into the private log.
#[test]
fn symlink_swapped_in_after_the_check_is_not_read_into_the_log() {
    let dir = tempfile::tempdir().expect("dir");
    let victim = dir.path().join("victim.txt");
    fs::write(&victim, VICTIM).expect("victim");
    let target = victim.clone();
    let journal = JsonlMcpRegistryJournal::with_private_diagnostic_root(
        dir.path().join("registry"),
        dir.path().join("private"),
        Box::new(RacyIo(move |path: &Path| {
            if !path.exists() && path.symlink_metadata().is_err() {
                let _ = symlink(&target, path);
            }
        })),
    );
    let _ = journal.record_private_diagnostic("alpha", "cause");
    assert!(!log(dir.path()).contains(VICTIM), "{}", log(dir.path()));
}

/// Catches: the log path replaced by a hard link after the check; the victim's
/// bytes are folded into the log.
#[test]
fn hard_link_swapped_in_after_the_check_is_not_read_into_the_log() {
    let dir = tempfile::tempdir().expect("dir");
    let victim = dir.path().join("victim.txt");
    fs::write(&victim, VICTIM).expect("victim");
    let target = victim.clone();
    let journal = JsonlMcpRegistryJournal::with_private_diagnostic_root(
        dir.path().join("registry"),
        dir.path().join("private"),
        Box::new(RacyIo(move |path: &Path| {
            if path.symlink_metadata().is_err() {
                let _ = fs::hard_link(&target, path);
            }
        })),
    );
    let _ = journal.record_private_diagnostic("alpha", "cause");
    assert!(!log(dir.path()).contains(VICTIM), "{}", log(dir.path()));
}

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

/// Catches: a hand-edited or truncated log without a final newline; the next
/// record is glued to the old tail and both become unparseable.
#[test]
fn record_after_an_unterminated_tail_stays_one_json_object_per_line() {
    let dir = tempfile::tempdir().expect("dir");
    let private = dir.path().join("private");
    fs::create_dir_all(&private).expect("dir");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("mode");
    let path = private.join("errors.jsonl");
    fs::write(&path, br#"{"server":"old","cause":"old"}"#).expect("seed");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    adapter(dir.path())
        .record_private_diagnostic("alpha", "new")
        .expect("record");
    for line in log(dir.path()).lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|_| panic!("{line}"));
    }
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
