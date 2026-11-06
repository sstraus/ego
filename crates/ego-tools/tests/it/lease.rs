//! `Lease`: one writer per session, backed by an advisory OS lock plus a
//! PID/heartbeat record for stale-lease takeover.
//!
//! The lock file's on-disk shape (`{"pid": <u32>, "heartbeat": <millis>}`) is documented
//! on `ego_tools::store::lease`'s module doc comment; several tests here construct it by
//! hand to exercise the contended branch deterministically, exactly as that module's doc
//! comment describes.

use std::io::Write;
use std::time::Duration;

use ego_core::event::time::Timestamp;
use ego_tools::store::{Lease, LeaseError};
use tempfile::tempdir;

// Wrapped in a module named `lease` so `cargo test -p ego-tools lease` (the
// story's VERIFY command) matches these tests: cargo's test filter matches
// the full test path, not the source file name, and tests/lease.rs's bare
// functions wouldn't otherwise contain "lease" in their path.
// The module keeps the test names `lease::<test>` that `--test lease` selected
// before the binaries were consolidated.
#[allow(clippy::module_inception)]
mod lease {
    use super::*;

    const NEVER_ALIVE: &dyn Fn(u32) -> bool = &|_| false;
    const ALWAYS_ALIVE: &dyn Fn(u32) -> bool = &|_| true;

    /// Hold a *real* advisory lock on `path` directly (bypassing `Lease`
    /// entirely) with hand-written contents, so a test can exercise
    /// `Lease::acquire`'s contended (`WouldBlock`) branch deterministically
    /// instead of relying on a second real process. The returned guard
    /// must stay alive for as long as the simulated holder should still be
    /// holding the lock.
    fn hold_raw_lock(
        path: &std::path::Path,
        contents: &[u8],
    ) -> fd_lock::RwLockWriteGuard<'static, std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(path)
            .unwrap();
        // Leaked deliberately: this helper exists only to simulate another
        // process's held lock for the lifetime of one test, so there is no
        // later point at which reclaiming the `RwLock<File>` itself
        // matters — only the guard's continued existence (keeping the
        // advisory lock held) does.
        let lock: &'static mut fd_lock::RwLock<std::fs::File> =
            Box::leak(Box::new(fd_lock::RwLock::new(file)));
        let mut guard = lock.try_write().unwrap();
        guard.write_all(b" ").unwrap();
        guard.write_all(contents).unwrap();
        guard
    }

    // -- AC1: a second writer on the same session fails with LeaseHeld ---

    #[test]
    fn lease_second_writer_on_same_session_fails_with_lease_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.lock");
        let now = Timestamp::from_millis(1_000);

        let _first = Lease::acquire(&path, 111, now, Duration::from_secs(30), ALWAYS_ALIVE)
            .expect("first acquire on an unheld lease must succeed");

        match Lease::acquire(&path, 222, now, Duration::from_secs(30), ALWAYS_ALIVE) {
            Err(LeaseError::LeaseHeld { pid: 111 }) => {}
            other => panic!("expected Err(LeaseHeld {{ pid: 111 }}), got {other:?}"),
        }
    }

    // -- AC2: a stale lease (dead PID + expired heartbeat) is taken over -

    #[test]
    fn lease_stale_lease_dead_pid_and_expired_heartbeat_is_taken_over() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.lock");
        // A holder that recorded pid 999_999 at t=1_000ms and never
        // updated it again — the "crashed, kernel didn't clean up the
        // content, and the caller's own liveness check says it is dead"
        // case this module's staleness path exists for.
        let _held = hold_raw_lock(&path, br#"{"pid":999999,"heartbeat":1000}"#);

        let now = Timestamp::from_millis(1_000 + 501);
        let is_alive: &dyn Fn(u32) -> bool = &|pid| pid != 999_999;

        let lease = Lease::acquire(&path, 42, now, Duration::from_millis(500), is_alive)
            .expect("a dead pid past the grace period must be taken over");
        drop(lease);

        let contents = std::fs::read(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&contents).unwrap();
        assert_eq!(
            parsed["pid"], 42,
            "the lock file must now record the new holder's pid"
        );
    }

    #[test]
    fn lease_alive_pid_with_expired_heartbeat_is_not_taken_over() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.lock");
        let _held = hold_raw_lock(&path, br#"{"pid":999999,"heartbeat":1000}"#);

        let now = Timestamp::from_millis(1_000 + 501);
        // A stale heartbeat alone must not be enough: the holder may
        // simply be busy (no heartbeat renewal is implemented yet).
        let result = Lease::acquire(&path, 42, now, Duration::from_millis(500), ALWAYS_ALIVE);
        match result {
            Err(LeaseError::LeaseHeld { pid: 999_999 }) => {}
            other => panic!("expected Err(LeaseHeld {{ pid: 999999 }}), got {other:?}"),
        }
    }

    #[test]
    fn lease_dead_pid_with_fresh_heartbeat_is_not_taken_over() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.lock");
        let _held = hold_raw_lock(&path, br#"{"pid":999999,"heartbeat":1000}"#);

        // Well within the grace period: a dead pid is not enough on its
        // own either.
        let now = Timestamp::from_millis(1_100);
        let result = Lease::acquire(&path, 42, now, Duration::from_millis(500), NEVER_ALIVE);
        match result {
            Err(LeaseError::LeaseHeld { pid: 999_999 }) => {}
            other => panic!("expected Err(LeaseHeld {{ pid: 999999 }}), got {other:?}"),
        }
    }

    // -- AC3: the lease is released on drop -------------------------------

    #[test]
    fn lease_is_released_on_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.lock");
        let now = Timestamp::from_millis(1_000);

        let first = Lease::acquire(&path, 111, now, Duration::from_secs(30), ALWAYS_ALIVE).unwrap();
        assert!(matches!(
            Lease::acquire(&path, 222, now, Duration::from_secs(30), ALWAYS_ALIVE),
            Err(LeaseError::LeaseHeld { pid: 111 })
        ));

        drop(first);

        Lease::acquire(&path, 222, now, Duration::from_secs(30), ALWAYS_ALIVE)
            .expect("dropping the first lease must release it for a new acquirer");
    }

    // -- baseline / hygiene -------------------------------------------------

    #[test]
    fn lease_acquire_creates_parent_directories_and_the_lock_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("session.lock");
        let now = Timestamp::from_millis(1_000);

        let lease = Lease::acquire(&path, 111, now, Duration::from_secs(30), ALWAYS_ALIVE).unwrap();
        assert!(path.exists());
        drop(lease);
    }

    // -- judgement call: unreadable lock-file content while contended -----

    #[test]
    fn lease_missing_content_while_contended_refuses_with_pid_zero() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.lock");
        // Held, but nothing written yet — an empty file.
        let _held = hold_raw_lock(&path, b"");

        let now = Timestamp::from_millis(10_000);
        let result = Lease::acquire(&path, 42, now, Duration::from_millis(1), NEVER_ALIVE);
        match result {
            Err(LeaseError::LeaseHeld { pid: 0 }) => {}
            other => panic!("expected Err(LeaseHeld {{ pid: 0 }}), got {other:?}"),
        }
    }

    #[test]
    fn lease_garbage_content_while_contended_is_reported_as_corrupt_not_taken_over() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.lock");
        let _held = hold_raw_lock(&path, b"not json");

        let now = Timestamp::from_millis(10_000);
        let result = Lease::acquire(&path, 42, now, Duration::from_millis(1), NEVER_ALIVE);
        match result {
            Err(LeaseError::Corrupt(p)) => assert_eq!(p, path),
            other => panic!("expected Err(Corrupt(..)), got {other:?}"),
        }
    }
}
