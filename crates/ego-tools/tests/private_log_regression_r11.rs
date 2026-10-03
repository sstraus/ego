//! Round-eleven critic cases for the descriptor-based private diagnostic log.
//!
//! Unix only: the cases use symlinks, hard links, FIFOs and mode bits. The
//! Windows backend is covered by compilation and by `windows_private_paths.rs`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ego_core::turn::McpRegistryJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::{OsIo, StoreIo};

const VICTIM: &str = "VICTIM-FILE-CONTENT-r11";
const MAX_CAUSE: usize = 16 * 1024;

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

fn private_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write file");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("chmod file");
}

fn victim(dir: &Path, mode: u32) -> PathBuf {
    let path = dir.join("victim.txt");
    fs::write(&path, VICTIM).expect("victim");
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod victim");
    path
}

fn journal(dir: &Path) -> JsonlMcpRegistryJournal {
    JsonlMcpRegistryJournal::with_private_diagnostic_root(
        dir.join("registry"),
        dir.join("private"),
        Box::new(OsIo),
    )
}

fn only_record(dir: &Path) -> serde_json::Value {
    let bytes = log(&dir.join("private"));
    let text = String::from_utf8(bytes).expect("log is utf-8");
    let mut lines = text.lines();
    let record = lines.next().expect("one record");
    assert!(lines.next().is_none(), "exactly one record: {text:.200}");
    serde_json::from_str(record).expect("record is one JSON object")
}

/// Catches: `>=` instead of `>` in the fit test, which would rotate away a
/// record that still fits, or `>` instead of `>=` that lets the log pass `max`.
#[test]
fn a_log_exactly_at_the_limit_is_kept_and_the_next_record_starts_a_new_segment() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    append(&root, "111111111", 20).expect("first");
    append(&root, "222222222", 20).expect("second fills the segment exactly");
    assert_eq!(log(&root), b"111111111\n222222222\n");
    append(&root, "333333333", 20).expect("third rotates");
    assert_eq!(log(&root), b"333333333\n", "no NUL hole, no earlier record");
}

/// Catches: a record whose line plus newline equals the limit being refused, or
/// one byte over being accepted and then retained beyond the limit.
#[test]
fn a_record_that_exactly_fills_the_limit_is_accepted_and_one_byte_more_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    append(&root, "keep", 10).expect("small");
    append(&root, "123456789", 10).expect("9 + newline == 10");
    assert_eq!(log(&root), b"123456789\n");
    assert!(
        append(&root, "1234567890", 10).is_err(),
        "10 + newline > 10"
    );
    assert_eq!(
        log(&root),
        b"123456789\n",
        "a refused record changes nothing"
    );
}

/// Catches: an inherited oversized log (older version, or hostile growth) kept
/// and appended to, so the retained segment exceeds the ceiling.
#[test]
fn an_existing_log_above_the_limit_is_replaced_by_the_new_record_only() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    private_file(&root.join("errors.jsonl"), &[b'z'; 4096]);
    append(&root, "fresh", 100).expect("append");
    assert_eq!(log(&root), b"fresh\n");
}

/// Catches: a chunk-boundary error in the backwards newline scan (off by one at
/// 4096-byte multiples), which would keep a torn fragment or cut a whole record.
#[test]
fn torn_tails_of_every_chunk_boundary_length_are_dropped_and_the_prior_record_survives() {
    for tail in [1_usize, 2, 4095, 4096, 4097, 8191, 8192, 8193, 12289] {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("private");
        private_dir(&root);
        let mut body = b"keep\n".to_vec();
        body.extend(std::iter::repeat_n(b'x', tail));
        private_file(&root.join("errors.jsonl"), &body);
        append(&root, "new", 1 << 20).expect("append");
        assert_eq!(log(&root), b"keep\nnew\n", "tail length {tail}");
    }
}

/// Catches: a torn tail that holds no newline at all surviving, so the new
/// record is glued to the fragment and the line becomes invalid JSON.
#[test]
fn a_log_that_is_one_unterminated_fragment_is_emptied_before_the_append() {
    for len in [1_usize, 4096, 9000] {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("private");
        private_dir(&root);
        private_file(&root.join("errors.jsonl"), &vec![b'f'; len]);
        append(&root, "new", 1 << 20).expect("append");
        assert_eq!(log(&root), b"new\n", "fragment length {len}");
    }
}

/// Catches: recovery cutting at the first newline instead of the last, which
/// would throw away every complete record in front of the torn one.
#[test]
fn recovery_keeps_every_complete_record_before_the_torn_tail() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    private_file(&root.join("errors.jsonl"), b"a\nb\nc\ntorn");
    append(&root, "d", 1 << 20).expect("append");
    assert_eq!(log(&root), b"a\nb\nc\nd\n");
}

/// Catches: a symlink at the log (dangling or to a victim) being followed, so the
/// record is written through it or the target is created.
#[test]
fn a_symlink_at_the_log_or_lock_is_refused_and_nothing_is_written_through_it() {
    for leaf in ["errors.jsonl", "errors.lock"] {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("private");
        private_dir(&root);
        let victim = victim(dir.path(), 0o600);
        symlink(&victim, root.join(leaf)).expect("symlink");
        assert!(
            append(&root, "secret", 1 << 20).is_err(),
            "{leaf} to victim"
        );
        assert_eq!(fs::read_to_string(&victim).expect("victim"), VICTIM);
        fs::remove_file(root.join(leaf)).expect("unlink");

        let ghost = dir.path().join("ghost");
        symlink(&ghost, root.join(leaf)).expect("dangling symlink");
        assert!(append(&root, "secret", 1 << 20).is_err(), "{leaf} dangling");
        assert!(
            !ghost.exists(),
            "{leaf}: dangling target must not be created"
        );
    }
}

/// Catches: a hard link to an owner-only file elsewhere being accepted
/// (nlink check missing or applied after the write).
#[test]
fn a_hard_linked_log_is_refused_before_any_byte_is_written() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    let victim = victim(dir.path(), 0o600);
    fs::hard_link(&victim, root.join("errors.jsonl")).expect("hard link");
    assert!(append(&root, "secret", 1 << 20).is_err());
    assert_eq!(fs::read_to_string(&victim).expect("victim"), VICTIM);
}

/// Catches: a FIFO at the log path blocking the open forever (missing
/// O_NONBLOCK) or being accepted as a file.
#[test]
fn a_fifo_at_the_log_path_is_refused_without_blocking() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    nix::unistd::mkfifo(
        &root.join("errors.jsonl"),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .expect("mkfifo");
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = root.clone();
    std::thread::spawn(move || {
        let _ = tx.send(append(&worker, "secret", 1 << 20));
    });
    let result = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("append returned instead of blocking on the FIFO");
    assert!(result.is_err());
}

/// Catches: a directory or a wide-mode file or directory at the expected names
/// being used as the private log.
#[test]
fn wrong_type_or_wide_mode_objects_are_refused_and_left_unchanged() {
    // errors.jsonl is a directory
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    fs::create_dir(root.join("errors.jsonl")).expect("dir leaf");
    assert!(append(&root, "x", 1 << 20).is_err());

    // errors.jsonl is 0644
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    private_file(&root.join("errors.jsonl"), b"old\n");
    fs::set_permissions(root.join("errors.jsonl"), fs::Permissions::from_mode(0o644))
        .expect("widen");
    assert!(append(&root, "x", 1 << 20).is_err());
    assert_eq!(log(&root), b"old\n");

    // the private directory itself is 0755
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    private_dir(&root);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("widen dir");
    assert!(append(&root, "x", 1 << 20).is_err());
    assert!(
        log(&root).is_empty(),
        "nothing is written into a wide directory"
    );
}

/// Catches: the private directory path being a symlink or a regular file and
/// the log landing in (or the lock being created inside) the target.
#[test]
fn a_private_root_that_is_a_symlink_or_a_file_is_refused_and_the_target_stays_empty() {
    let dir = tempfile::tempdir().expect("dir");
    let decoy = dir.path().join("decoy");
    private_dir(&decoy);
    let root = dir.path().join("private");
    symlink(&decoy, &root).expect("symlink root");
    assert!(append(&root, "secret", 1 << 20).is_err());
    assert_eq!(fs::read_dir(&decoy).expect("decoy").count(), 0);
    fs::remove_file(&root).expect("unlink");

    symlink(dir.path().join("ghost"), &root).expect("dangling root");
    assert!(append(&root, "secret", 1 << 20).is_err());
    assert!(!dir.path().join("ghost").exists());
    fs::remove_file(&root).expect("unlink");

    fs::write(&root, "file").expect("file root");
    assert!(append(&root, "secret", 1 << 20).is_err());
    assert_eq!(fs::read_to_string(&root).expect("file"), "file");
}

/// Catches: first-use creation leaving a mode wider than the contract
/// (0700 directory, 0600 log and lock).
#[test]
fn fresh_creation_yields_owner_only_modes() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    append(&root, "x", 1 << 20).expect("append");
    let mode = |p: PathBuf| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(root.clone()), 0o700);
    assert_eq!(mode(root.join("errors.jsonl")), 0o600);
    assert_eq!(mode(root.join("errors.lock")), 0o600);
}

/// Live race: the log leaf is swapped between symlink-to-victim, hard-link-to-
/// victim and absent while appends run. Catches: any open that follows the
/// swapped object (missing O_NOFOLLOW, or validation after the write).
/// The victim is 0644, so even a hard-link win must fail validation.
#[test]
fn live_swapping_the_log_leaf_never_writes_through_to_the_victim() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    append(&root, "seed", 4096).expect("seed");
    let victim = victim(dir.path(), 0o644);
    let stop = Arc::new(AtomicBool::new(false));
    let attacker = {
        let (stop, root, victim) = (stop.clone(), root.clone(), victim.clone());
        std::thread::spawn(move || {
            let leaf = root.join("errors.jsonl");
            let mut round = 0_u32;
            while !stop.load(Ordering::Relaxed) {
                let _ = fs::remove_file(&leaf);
                let _ = match round % 3 {
                    0 => symlink(&victim, &leaf),
                    1 => fs::hard_link(&victim, &leaf),
                    _ => Ok(()),
                };
                round += 1;
                std::thread::sleep(Duration::from_micros(30));
            }
        })
    };
    for _ in 0..600 {
        let _ = append(&root, "payload-that-must-not-reach-the-victim", 4096);
    }
    stop.store(true, Ordering::Relaxed);
    attacker.join().expect("attacker");
    assert_eq!(fs::read_to_string(&victim).expect("victim"), VICTIM);
}

/// Live race: the private directory is renamed away and replaced by a symlink to
/// a decoy while appends run. Catches: the final component followed (missing
/// O_NOFOLLOW) or the directory re-resolved by path after the first open.
#[test]
fn live_replacing_the_private_directory_never_writes_into_the_decoy() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    let moved = dir.path().join("private.moved");
    let decoy = dir.path().join("decoy");
    private_dir(&decoy);
    append(&root, "seed", 4096).expect("seed");
    let stop = Arc::new(AtomicBool::new(false));
    let attacker = {
        let (stop, root, moved, decoy) = (stop.clone(), root.clone(), moved.clone(), decoy.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if fs::rename(&root, &moved).is_ok() {
                    let _ = symlink(&decoy, &root);
                    std::thread::sleep(Duration::from_micros(200));
                    let _ = fs::remove_file(&root);
                    let _ = fs::rename(&moved, &root);
                }
                std::thread::sleep(Duration::from_micros(30));
            }
        })
    };
    for _ in 0..600 {
        let _ = append(&root, "payload-that-must-not-reach-the-decoy", 4096);
    }
    stop.store(true, Ordering::Relaxed);
    attacker.join().expect("attacker");
    let leaked: Vec<_> = fs::read_dir(&decoy).expect("decoy").collect();
    assert!(leaked.is_empty(), "decoy received {leaked:?}");
}

/// Child entry point for the cross-process test; a no-op in normal runs.
#[test]
fn critic_r11_worker() {
    let Ok(root) = std::env::var("CRITIC_R11_ROOT") else {
        return;
    };
    let id: u32 = std::env::var("CRITIC_R11_ID")
        .expect("id")
        .parse()
        .expect("id");
    let count: u32 = std::env::var("CRITIC_R11_COUNT")
        .expect("count")
        .parse()
        .expect("count");
    let max: u64 = std::env::var("CRITIC_R11_MAX")
        .expect("max")
        .parse()
        .expect("max");
    for n in 0..count {
        let line = format!(r#"{{"w":{id},"n":{n},"pad":"{}"}}"#, "x".repeat(200));
        append(Path::new(&root), &line, max).expect("append under contention");
    }
}

/// Catches: appenders in separate processes interleaving, duplicating or
/// reordering records, or leaving a log above the limit at the rotation edge.
#[test]
fn separate_processes_appending_across_the_limit_keep_whole_ordered_records() {
    const WORKERS: u32 = 4;
    const COUNT: u32 = 120;
    const MAX: u64 = 4096;
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("private");
    let exe = std::env::current_exe().expect("exe");
    let children: Vec<_> = (0..WORKERS)
        .map(|id| {
            Command::new(&exe)
                .args(["critic_r11_worker", "--exact", "--nocapture"])
                .env("CRITIC_R11_ROOT", &root)
                .env("CRITIC_R11_ID", id.to_string())
                .env("CRITIC_R11_COUNT", COUNT.to_string())
                .env("CRITIC_R11_MAX", MAX.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn worker")
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().expect("worker");
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let bytes = log(&root);
    assert!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX,
        "len {}",
        bytes.len()
    );
    assert_eq!(bytes.last(), Some(&b'\n'));
    let mut last: BTreeMap<u64, u64> = BTreeMap::new();
    for line in String::from_utf8(bytes).expect("utf-8").lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("whole record");
        let (w, n) = (
            value["w"].as_u64().expect("w"),
            value["n"].as_u64().expect("n"),
        );
        if let Some(previous) = last.insert(w, n) {
            assert!(n > previous, "worker {w}: {n} after {previous}");
        }
    }
}

/// Catches: truncation at a raw byte offset inside a multi-byte character
/// (invalid UTF-8 / panic on slice) or a missing/incorrect `truncated` marker.
#[test]
fn cause_truncation_stops_on_a_character_boundary_and_marks_the_record() {
    // 'a' shifts the 2-byte characters so byte 16384 falls inside one.
    let two = format!("a{}", "é".repeat(9000));
    // 'a' + 4-byte characters: 16384 - 1 is not a multiple of four.
    let four = format!("a{}", "😀".repeat(5000));
    for (cause, expected_len) in [(two, 16_383_usize), (four, 16_381)] {
        let dir = tempfile::tempdir().expect("dir");
        journal(dir.path())
            .record_private_diagnostic("alpha", &cause)
            .expect("record");
        let record = only_record(dir.path());
        let kept = record["cause"].as_str().expect("cause");
        assert_eq!(kept.len(), expected_len);
        assert!(cause.starts_with(kept));
        assert_eq!(record["truncated"], true);
    }
}

/// Catches: an off-by-one in the cap (marking an exactly-fitting cause as
/// truncated, or letting one byte more through unmarked).
#[test]
fn a_cause_of_exactly_the_cap_is_whole_and_one_byte_more_is_marked() {
    let dir = tempfile::tempdir().expect("dir");
    journal(dir.path())
        .record_private_diagnostic("alpha", &"a".repeat(MAX_CAUSE))
        .expect("record");
    let record = only_record(dir.path());
    assert_eq!(record["cause"].as_str().expect("cause").len(), MAX_CAUSE);
    assert!(record.get("truncated").is_none(), "{record:.80}");

    let dir = tempfile::tempdir().expect("dir");
    journal(dir.path())
        .record_private_diagnostic("alpha", &"a".repeat(MAX_CAUSE + 1))
        .expect("record");
    let record = only_record(dir.path());
    assert_eq!(record["cause"].as_str().expect("cause").len(), MAX_CAUSE);
    assert_eq!(record["truncated"], true);
}

/// Catches: worst-case JSON escaping (six bytes per control byte, two per quote
/// or backslash) of a hostile 1 MiB cause pushing the record past the ceiling
/// so the diagnostic is refused instead of retained.
#[test]
fn a_hostile_escape_heavy_cause_is_retained_within_the_record_ceiling() {
    for hostile in ["\u{1}", "\"", "\\", "\n"] {
        let dir = tempfile::tempdir().expect("dir");
        journal(dir.path())
            .record_private_diagnostic(
                "a-server-name-of-the-maximum-length-0123456789-0123456789-0123456",
                &hostile.repeat(1 << 20),
            )
            .expect("record is retained");
        let record = only_record(dir.path());
        assert_eq!(record["truncated"], true);
        assert!(log(&dir.path().join("private")).len() <= 128 * 1024);
    }
}

/// Catches: the private-diagnostic append reporting success while nothing was
/// written (default trait method swallowed), for the production adapter.
#[test]
fn the_production_adapter_really_persists_a_record() {
    let dir = tempfile::tempdir().expect("dir");
    journal(dir.path())
        .record_private_diagnostic("alpha", "boom")
        .expect("record");
    let record = only_record(dir.path());
    assert_eq!(record["server"], "alpha");
    assert_eq!(record["cause"], "boom");
}
