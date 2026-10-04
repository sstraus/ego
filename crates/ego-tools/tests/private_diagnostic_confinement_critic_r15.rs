//! Round-fifteen critic case: raw cause stays only in its owner-only private log.
#![cfg(unix)]

#[path = "support/private_logs.rs"]
mod private_logs;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ego_core::turn::McpRegistryJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::OsIo;

const MARKER: &str = "CRITIC-R15-RAW-CAUSE-77aa";

fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            walk(&path, found);
        } else {
            found.push(path);
        }
    }
}

/// Catches: the machine journal, its lock, or any sibling file also receiving
/// the raw cause (or the private file/dir being group/world readable).
#[test]
fn raw_cause_exists_only_in_the_owner_only_private_log() {
    let dir = tempfile::tempdir().expect("dir");
    let journal = JsonlMcpRegistryJournal::with_private_diagnostic_root(
        dir.path().join("registry"),
        dir.path().join("private"),
        Box::new(OsIo),
    );
    journal
        .begin_refresh("req-1".to_owned(), "add".to_owned(), None)
        .expect("begin");
    journal
        .record_private_diagnostic("alpha", &format!("token={MARKER}"))
        .expect("private");
    let mut files = Vec::new();
    walk(dir.path(), &mut files);
    let holders: Vec<_> = files
        .iter()
        .filter(|path| {
            fs::read(path).is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(MARKER))
        })
        .collect();
    assert_eq!(holders.len(), 1);
    assert_eq!(
        holders[0].parent(),
        Some(dir.path().join("private").as_path())
    );
    assert_eq!(
        fs::metadata(holders[0]).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(dir.path().join("private"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let replay = format!("{:?}", journal.replay().expect("replay"));
    assert!(!replay.contains(MARKER));
}
