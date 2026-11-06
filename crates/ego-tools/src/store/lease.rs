//! Per-session advisory writer lease. PID and heartbeat live in the lock file,
//! not the manifest, avoiding manifest rewrites at heartbeat frequency.
//!
//! Kernel locks release when the owning descriptors close, including on a crash.
//! Contention reports the recorded PID. Takeover requires both a dead PID verdict
//! and an old heartbeat; a live quiet process remains protected by the liveness probe.
//! Heartbeat is recorded at acquisition, not renewed. Callers supply PID, time,
//! and liveness observations; this module performs no OS process inspection.
//! Filesystems without reliable advisory locking are outside this guarantee.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ego_core::event::time::Timestamp;
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

// One process owns each PID path. Serialize its publish/remove operations so
// an older lease cannot unlink a replacement between checking and removing it.
static PID_INDEX_LOCK: Mutex<()> = Mutex::new(());

/// The lock file's on-disk contents: the writer's PID and the wall-clock
/// time it last confirmed its own liveness. JSON, exactly two fields —
/// `{"pid": <u32>, "heartbeat": <millis>}` — matching `Timestamp`'s own
/// bare-integer serde form (`ego_core::event::time::Timestamp`). Byte zero
/// is one JSON whitespace byte reserved for the advisory lock; the object
/// begins at byte one so Windows can read it while `LockFileEx` holds byte
/// zero exclusively.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct LockContents {
    pid: u32,
    heartbeat: Timestamp,
}

/// A [`Lease::acquire`] failure.
#[derive(Debug)]
pub enum LeaseError {
    /// Another writer currently holds the lease. `pid` is read from the
    /// lock file's own contents; it is `0` when the lease is held but the
    /// lock file's contents could not be read at all (missing or empty —
    /// see the module doc comment's note on corrupt/adversarial content).
    LeaseHeld { pid: u32 },
    /// The lock file is currently held by someone, but its contents could
    /// not be parsed as [`LockContents`] (present but not valid JSON in
    /// the expected shape). This implementation always refuses to take
    /// the lease in this case rather than guess it is safe to steal —
    /// unparseable content is exactly the case this module has no way to
    /// prove staleness for.
    Corrupt(PathBuf),
    /// The underlying I/O failed: open, read, write, rename or lock.
    Io(io::Error),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::LeaseHeld { pid } => write!(f, "writer lease already held by pid {pid}"),
            LeaseError::Corrupt(path) => write!(
                f,
                "lease at {} is held but its metadata could not be parsed",
                path.display()
            ),
            LeaseError::Io(e) => write!(f, "writer lease I/O error: {e}"),
        }
    }
}

impl std::error::Error for LeaseError {}

impl From<io::Error> for LeaseError {
    fn from(e: io::Error) -> Self {
        LeaseError::Io(e)
    }
}

/// Per-session advisory writer lease on a caller-chosen path, normally
/// `<session-dir>/session.lock`. See the module durability protocol and limits. Dropping
/// explicitly unlocks the Unix open file description before closing the file, so
/// inherited descriptors cannot extend the lease. Drop removes the owned PID binding.
/// Do not unlink the lock path on
/// release: a newer holder may already own its replacement inode.
pub struct Lease {
    // Owns the advisory lock until drop explicitly unlocks it on Unix
    // and closes the file on every platform.
    #[allow(dead_code)]
    lock: RwLock<File>,
    path: PathBuf,
    pid_index: Mutex<Option<(PathBuf, Vec<u8>)>>,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _guard = PID_INDEX_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((path, contents)) = &*self.pid_index.lock().unwrap_or_else(|error| error.into_inner())
            // A new lease for this PID may already have replaced the binding.
            && fs::read(path).is_ok_and(|current| current == *contents)
        {
            let _ = fs::remove_file(path);
        }
        // Unix flock follows the open file description, including duplicates in
        // children forked by other threads. Closing only our descriptor would
        // leave their inherited copies holding this lease until exec or exit.
        // Re-entering our own lock succeeds; dropping its guard explicitly
        // unlocks that description before the owned file is closed.
        #[cfg(unix)]
        if let Ok(guard) = self.lock.try_write() {
            drop(guard);
        }
    }
}

impl Lease {
    pub(super) fn publish_pid_index(
        &self,
        home: &Path,
        pid: u32,
        session_id: &ego_core::event::id::SessionId,
        cwd: &Path,
        io: &dyn super::io::StoreIo,
    ) -> io::Result<()> {
        let _guard = PID_INDEX_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = home.join("pids").join(format!("{pid}.json"));
        let contents = serde_json::to_vec(&serde_json::json!({
            "pid": pid,
            "session_id": session_id,
            "cwd": cwd,
        }))?;
        // Register cleanup before publication: a directory fsync error can be
        // reported after the rename has already made the binding visible.
        *self
            .pid_index
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((path.clone(), contents.clone()));
        io.replace_atomic(&path, &contents)
    }

    /// Hold the same exclusive kernel lock as a writer without changing the
    /// lease contents. `None` means a live holder already owns it.
    pub(crate) fn probe(path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let mut lock = RwLock::new(file);
        match lock.try_write() {
            Ok(guard) => std::mem::forget(guard),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        }
        Ok(Some(Self {
            lock,
            path: path.to_path_buf(),
            pid_index: Mutex::new(None),
        }))
    }

    /// Acquire the writer lease at `path`, recording `pid` and `now` as
    /// this holder's liveness record.
    ///
    /// `now`, `pid` and `is_alive` are all supplied by the caller rather
    /// than read from the environment — see the module doc comment for
    /// why: this is the seam a test needs to present a deterministically
    /// stale (or fresh) lease without racing a real process or the wall
    /// clock.
    ///
    /// A lease already held by a lock whose recorded PID `is_alive`
    /// reports as alive, or whose heartbeat is within `stale_after` of
    /// `now`, fails with [`LeaseError::LeaseHeld`]. A lease whose recorded
    /// holder is both not alive and last heard from at least `stale_after`
    /// before `now` is taken over: this replaces the lock file (write a
    /// new file, then rename it over the old path) rather than rewriting
    /// the existing one in place, exactly so a stale holder's own open
    /// descriptor — if `is_alive` was ever wrong — keeps referring to a
    /// now-unlinked inode instead of the one a new writer is using.
    ///
    /// That replace-based takeover is itself not race-free against a
    /// *second concurrent* takeover attempt: two callers who each observe
    /// the same lease as stale at the same moment both build their own
    /// replacement file, and only one process's `rename` wins the final
    /// directory entry — but both processes still hold a real advisory
    /// lock on their own now-or-formerly-linked inode, so both would
    /// believe they hold the lease. This implementation narrows that
    /// window by serialising takeover attempts through a shared, fixed
    /// temporary path (`path` with an added extension) whose own
    /// `flock` a second concurrent takeover contends on and loses — so two
    /// takeovers of the *same* stale lease cannot both succeed — but it
    /// does not attempt a stronger cross-process arbiter (for example, an
    /// inode recheck after `rename`) beyond that. This is a known,
    /// reported limitation, not a silently accepted one.
    pub fn acquire(
        path: &Path,
        pid: u32,
        now: Timestamp,
        stale_after: Duration,
        is_alive: &dyn Fn(u32) -> bool,
    ) -> Result<Lease, LeaseError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            // Explicitly do not truncate: an existing lock file's contents
            // are what a contended acquirer reads to name the holder, and
            // truncating on open would erase that before we know whether
            // we are even allowed to take the lock. `write_lock_contents`
            // does its own `set_len` once the lock is genuinely held.
            .truncate(false)
            .open(path)?;
        let mut lock = RwLock::new(file);
        // The guard returned by `try_write` borrows `lock`, and a `match`
        // holds its scrutinee's borrow alive across every arm — so moving
        // `lock` into the returned `Lease` inside an arm cannot compile.
        // Reduce the match to a plain `bool` so the borrow ends with it,
        // and move `lock` afterwards.
        let acquired = match lock.try_write() {
            Ok(mut guard) => {
                write_lock_contents(&mut guard, pid, now)?;
                // Deliberately outlive this guard's lexical scope: its
                // `Drop` only calls the OS unlock, which this lease must
                // *not* do yet. Nothing about the guard is an owned
                // resource beyond the `&mut RwLock` it borrows, so
                // forgetting it leaks nothing — the `RwLock<File>` moved
                // into the returned `Lease` still owns the real
                // descriptor. `Lease`'s own drop releases the lock (see
                // the struct doc comment).
                std::mem::forget(guard);
                true
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(e) => return Err(LeaseError::Io(e)),
        };

        if acquired {
            return Ok(Lease {
                lock,
                path: path.to_path_buf(),
                pid_index: Mutex::new(None),
            });
        }

        let held = read_lock_file(path)?;
        let (stale, held_pid) = match &held {
            Some(contents) => (is_stale(contents, now, stale_after, is_alive), contents.pid),
            // Held right now (contended `try_write`) but no readable
            // content: refuse to take over — see `LeaseError::LeaseHeld`'s
            // doc comment. `0` reports "holder unknown" rather than
            // fabricating a PID.
            None => (false, 0),
        };
        if stale {
            take_over(path, pid, now, held_pid)
        } else {
            Err(LeaseError::LeaseHeld { pid: held_pid })
        }
    }
}

/// The heartbeat a holder recorded, read without taking the lease (cold storage).
pub(super) fn recorded_heartbeat(path: &Path) -> io::Result<Option<Timestamp>> {
    match read_lock_file(path) {
        Ok(contents) => Ok(contents.map(|contents| contents.heartbeat)),
        Err(LeaseError::Io(error)) => Err(error),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    }
}

/// Whether a writer holds the lease right now. Never creates the lease file;
/// the probe is released before this returns.
pub(super) fn is_held(path: &Path) -> io::Result<bool> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut lock = RwLock::new(file);
    match lock.try_write() {
        Ok(_guard) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
}

/// Whether a recorded lease-holder should be treated as gone. Both signals
/// must agree: a PID `is_alive` reports as alive always wins, even with a
/// stale heartbeat (it may simply be busy — see the module doc comment on
/// heartbeat renewal not being implemented yet), and a dead PID whose
/// heartbeat is still within `stale_after` is not yet stale either (a
/// grace period against the narrow window between death and a takeover
/// attempt, and against clock skew). Only "not alive, and not heard from
/// for at least `stale_after`" is stale.
fn is_stale(
    held: &LockContents,
    now: Timestamp,
    stale_after: Duration,
    is_alive: &dyn Fn(u32) -> bool,
) -> bool {
    if is_alive(held.pid) {
        return false;
    }
    let age_millis = now.as_millis().saturating_sub(held.heartbeat.as_millis());
    let stale_after_millis = u64::try_from(stale_after.as_millis()).unwrap_or(u64::MAX);
    age_millis >= stale_after_millis
}

/// Overwrite `file`'s contents with `pid`/`heartbeat`, truncating whatever
/// was there before.
fn write_lock_contents(file: &mut File, pid: u32, heartbeat: Timestamp) -> io::Result<()> {
    let bytes = serde_json::to_vec(&LockContents { pid, heartbeat })
        .expect("LockContents always serialises");
    // Defensive: `set_len` is not documented to move the cursor, and this
    // must always write starting at offset 0 regardless of what a
    // previous read on this same handle (there is none on this path
    // today, but the seek costs nothing) may have left it at.
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(b" ")?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Read and parse the lock file at `path`, if it exists and is non-empty.
/// `Ok(None)` covers both "no file" and "empty file" — both are treated as
/// "no readable liveness record", not as evidence of anything.
fn read_lock_file(path: &Path) -> Result<Option<LockContents>, LeaseError> {
    match File::open(path) {
        Ok(mut file) => {
            file.seek(SeekFrom::Start(1))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            if bytes.is_empty() {
                return Ok(None);
            }
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| LeaseError::Corrupt(path.to_path_buf()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LeaseError::Io(e)),
    }
}

/// Take over a lease this caller has judged stale: build a replacement
/// lock file at a fixed temporary path next to `path`, lock and populate
/// *that*, then atomically rename it over `path`. See
/// [`Lease::acquire`]'s doc comment for why replace rather than rewrite in
/// place, and for the concurrent-takeover race this does and does not
/// close.
fn take_over(
    path: &Path,
    pid: u32,
    now: Timestamp,
    previous_pid: u32,
) -> Result<Lease, LeaseError> {
    let tmp_path = path.with_extension("lease-tmp");
    let tmp_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // The takeover scratch file is rewritten from offset 0 by
        // `write_lock_contents`, which sets its own length; truncating here
        // would additionally destroy a *concurrent* takeover's in-progress
        // contents before its `flock` has been contended for.
        .truncate(false)
        .open(&tmp_path)?;
    let mut tmp_lock = RwLock::new(tmp_file);
    let mut guard = match tmp_lock.try_write() {
        Ok(guard) => guard,
        // A second concurrent takeover of the same stale lease is already
        // in progress; the fixed temporary path's own lock is the arbiter
        // that keeps both from believing they won (see the doc comment).
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            return Err(LeaseError::LeaseHeld { pid: previous_pid });
        }
        Err(e) => return Err(LeaseError::Io(e)),
    };
    write_lock_contents(&mut guard, pid, now)?;
    fs::rename(&tmp_path, path)?;
    // Same reasoning as the uncontended acquire path: the guard's Drop
    // would only unlock, which must not happen while this lease is held.
    std::mem::forget(guard);
    Ok(Lease {
        lock: tmp_lock,
        path: path.to_path_buf(),
        pid_index: Mutex::new(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: an inherited descriptor keeps the writer locked after its owner drops.
    #[cfg(unix)]
    #[test]
    fn inherited_descriptor_does_not_extend_lease_lifetime() {
        for kind in ["writer", "probe", "takeover"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.lock");
            let now = Timestamp::from_millis(1_000);
            let first = Lease::acquire(&path, 111, now, Duration::ZERO, &|_| true).unwrap();
            let mut lease = match kind {
                "writer" => first,
                "probe" => {
                    drop(first);
                    Lease::probe(&path).unwrap().unwrap()
                }
                "takeover" => Lease::acquire(&path, 222, now, Duration::ZERO, &|_| false)
                    .expect("dead holder past grace permits replacement"),
                _ => unreachable!(),
            };
            // Like fork, try_clone retains the same open file description.
            let guard = lease.lock.try_write().unwrap();
            let inherited = guard.try_clone().unwrap();
            std::mem::forget(guard);
            assert!(matches!(
                Lease::acquire(&path, 333, now, Duration::ZERO, &|_| true),
                Err(LeaseError::LeaseHeld { .. })
            ));
            drop(lease);
            let reopened = Lease::acquire(&path, 333, now, Duration::ZERO, &|_| true)
                .unwrap_or_else(|error| panic!("{kind}: owner drop must release lease: {error}"));
            drop(inherited);
            // Closing an inherited descriptor must not release the new owner's lock.
            assert!(matches!(
                Lease::acquire(&path, 444, now, Duration::ZERO, &|_| true),
                Err(LeaseError::LeaseHeld { pid: 333 })
            ));
            drop(reopened);
        }
    }

    fn contents(pid: u32, heartbeat_millis: u64) -> LockContents {
        LockContents {
            pid,
            heartbeat: Timestamp::from_millis(heartbeat_millis),
        }
    }

    #[test]
    fn is_stale_dead_pid_past_grace_period_is_stale() {
        let held = contents(42, 1_000);
        assert!(is_stale(
            &held,
            Timestamp::from_millis(2_000),
            Duration::from_millis(500),
            &|_| false,
        ));
    }

    #[test]
    fn is_stale_alive_pid_past_grace_period_is_not_stale() {
        let held = contents(42, 1_000);
        assert!(!is_stale(
            &held,
            Timestamp::from_millis(2_000),
            Duration::from_millis(500),
            &|_| true,
        ));
    }

    #[test]
    fn is_stale_dead_pid_within_grace_period_is_not_stale() {
        let held = contents(42, 1_000);
        assert!(!is_stale(
            &held,
            Timestamp::from_millis(1_100),
            Duration::from_millis(500),
            &|_| false,
        ));
    }

    #[test]
    fn is_stale_alive_pid_within_grace_period_is_not_stale() {
        let held = contents(42, 1_000);
        assert!(!is_stale(
            &held,
            Timestamp::from_millis(1_100),
            Duration::from_millis(500),
            &|_| true,
        ));
    }

    #[test]
    fn is_stale_exactly_at_the_threshold_counts_as_stale() {
        let held = contents(42, 1_000);
        assert!(is_stale(
            &held,
            Timestamp::from_millis(1_500),
            Duration::from_millis(500),
            &|_| false,
        ));
    }
}
