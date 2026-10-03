//! Round-twelve critic cases for the seek-then-write private diagnostic log.
//!
//! Unix only: the cases use mode bits, umask and process re-execution.
#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ego_core::turn::McpRegistryJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::{OsIo, StoreIo};

const MAX_RECORD: usize = 128 * 1024;

fn append(root: &Path, line: &str, max: u64) -> io::Result<()> {
    OsIo.append_private_log(root, line.as_bytes(), max)
}

fn log(root: &Path) -> Vec<u8> {
    fs::read(root.join("errors.jsonl")).unwrap_or_default()
}

fn private_dir(path: &Path) {
    fs::create_dir(path).expect("create private dir");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("chmod dir");
}

fn journal(dir: &Path) -> JsonlMcpRegistryJournal {
    JsonlMcpRegistryJournal::with_private_diagnostic_root(
        dir.join("registry"),
        dir.join("private"),
        Box::new(OsIo),
    )
}

/// Catches: recovery that keeps a torn fragment, drops a committed record, or
/// mis-handles one specific cut position (start of file, just after a newline,
/// inside the last byte of a record, across a 4096-byte chunk edge), so the
/// appended record fuses with the fragment or an earlier record disappears.
#[test]
fn truncation_at_every_byte_offset_keeps_exactly_the_committed_records() {
    let mut full = b"a\n".to_vec();
    full.extend(std::iter::repeat_n(b'y', 4200));
    full.extend_from_slice(b"\nb\n");
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    let path = root.join("errors.jsonl");
    fs::write(&path, b"").expect("seed");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
    let offsets = (0..=9).chain(4090..=full.len());
    for cut in offsets {
        let prefix = &full[..cut];
        fs::write(&path, prefix).expect("write prefix");
        let committed = prefix
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(&prefix[..0], |index| &prefix[..=index]);
        append(&root, "NEW", 1 << 20).expect("append after recovery");
        let mut expected = committed.to_vec();
        expected.extend_from_slice(b"NEW\n");
        assert!(log(&root) == expected, "cut at byte {cut}");
    }
}

/// Catches: the 128 KiB record ceiling being `>=` (a record of exactly the
/// ceiling refused) or `+ 1` (one byte over accepted and retained).
#[test]
fn a_record_of_exactly_the_record_ceiling_is_kept_and_one_byte_more_is_refused() {
    let probe = tempfile::tempdir().expect("dir");
    journal(probe.path())
        .record_private_diagnostic("s", "c")
        .expect("probe");
    let base = log(&probe.path().join("private")).len() - 1;
    let pad = MAX_RECORD - base;

    let exact = tempfile::tempdir().expect("dir");
    journal(exact.path())
        .record_private_diagnostic(&format!("s{}", "a".repeat(pad)), "c")
        .expect("exactly at the ceiling");
    assert_eq!(log(&exact.path().join("private")).len(), MAX_RECORD + 1);

    let over = tempfile::tempdir().expect("dir");
    let result =
        journal(over.path()).record_private_diagnostic(&format!("s{}", "a".repeat(pad + 1)), "c");
    assert!(result.is_err(), "one byte over the ceiling");
    assert!(log(&over.path().join("private")).is_empty());
}

/// Child entry point for the first-use race; a no-op in normal runs.
#[test]
fn critic_r12_worker() {
    let Ok(root) = std::env::var("CRITIC_R12_ROOT") else {
        return;
    };
    let go = std::env::var("CRITIC_R12_GO").expect("go");
    let id = std::env::var("CRITIC_R12_ID").expect("id");
    let started = Instant::now();
    while !Path::new(&go).exists() {
        assert!(started.elapsed() < Duration::from_secs(20), "no start");
        std::hint::spin_loop();
    }
    append(Path::new(&root), &format!("worker-{id}"), 1 << 20)
        .unwrap_or_else(|error| panic!("worker {id} first append: {error}"));
}

/// Catches: processes racing to create the private directory under a umask
/// that strips owner bits. The loser of `mkdir` opens the directory while the
/// winner has not yet widened it to 0700, fails validation, and silently loses
/// its diagnostic. Probabilistic: it can only turn red, never flake green->red.
#[test]
fn concurrent_first_use_under_a_restrictive_umask_loses_no_diagnostic() {
    const WORKERS: usize = 6;
    const TRIALS: usize = 40;
    let exe = std::env::current_exe().expect("exe");
    let base = tempfile::tempdir().expect("dir");
    fs::set_permissions(base.path(), fs::Permissions::from_mode(0o700)).expect("base");
    let mut failures = Vec::new();
    for trial in 0..TRIALS {
        let trial_dir = base.path().join(format!("t{trial}"));
        fs::create_dir(&trial_dir).expect("trial dir");
        fs::set_permissions(&trial_dir, fs::Permissions::from_mode(0o700)).expect("trial");
        let root = trial_dir.join("private");
        let go = trial_dir.join("go");
        let children: Vec<_> = (0..WORKERS)
            .map(|id| {
                Command::new("sh")
                    .arg("-c")
                    .arg("umask 0277; exec \"$@\"")
                    .arg("sh")
                    .arg(&exe)
                    .args(["critic_r12_worker", "--exact", "--nocapture"])
                    .env("CRITIC_R12_ROOT", &root)
                    .env("CRITIC_R12_GO", &go)
                    .env("CRITIC_R12_ID", id.to_string())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("spawn")
            })
            .collect();
        std::thread::sleep(Duration::from_millis(150));
        fs::write(&go, b"").expect("go");
        for child in children {
            let out = child.wait_with_output().expect("worker");
            if !out.status.success() {
                failures.push(format!(
                    "trial {trial}: {}",
                    String::from_utf8_lossy(&out.stderr)
                        .lines()
                        .find(|line| line.contains("first append"))
                        .unwrap_or("worker failed")
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Catches: a record shorter than the retention but a rotation that leaves the
/// cursor stale and appends the next record after a NUL gap, or a log that
/// keeps an earlier record after rotation (checked at the exact fit edge on a
/// log that was recovered from a torn tail first).
#[test]
fn rotation_after_tail_recovery_leaves_only_the_new_record_without_gaps() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    let path = root.join("errors.jsonl");
    fs::write(&path, b"1234567\nTORN").expect("seed");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
    // Recovery leaves "1234567\n" (8); 8 + 6 > 12 rotates.
    append(&root, "12345", 12).expect("append");
    assert_eq!(log(&root), b"12345\n");
}
