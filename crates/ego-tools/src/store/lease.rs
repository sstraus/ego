//! The per-session writer lease.
//!
//! See `ARCHITECTURE.md` §5 and `plans/m0a-contracts.md` Step 5. One writer
//! per session: a CLI and an ACP connection attached to the same session
//! must not interleave records into one log.
//!
//! ## Where liveness data lives (ratified)
//!
//! The PID and heartbeat that back stale-lease detection live in the lock
//! file's own contents, **not** in `manifest.json`. regression case deliberately
//! rewrites the manifest only at session creation and turn boundaries,
//! because `StoreIo::replace_atomic` is a tmp+rename and streamed deltas
//! arrive many times a second; a heartbeat written into the manifest would
//! force that same tmp+rename at heartbeat frequency and destroy the
//! cadence 005 just established, and it would make liveness — transient
//! runtime state — part of the catalogue entry every `list()` deserialises.
//!
//! ## What the OS-level lock already gives us, and what it does not
//!
//! `fd-lock` (`flock` on Unix, `LockFileEx` on Windows) ties the lock to
//! the *open file description*, which the kernel releases unconditionally
//! when every descriptor referencing it closes — including on a crash,
//! `SIGKILL` included, since process teardown closes all of a process's
//! descriptors. For the ordinary crash case, that already gives correct
//! single-writer semantics for free: [`Lease::acquire`]'s contended branch
//! (`ErrorKind::WouldBlock`) is reachable *only* when some open descriptor
//! genuinely still holds the lock right now, which on a local filesystem
//! implies a live holder. The PID/heartbeat recorded here therefore is not
//! what rescues the ordinary crash — `flock`'s own kernel-enforced release
//! does — and this module's report to the story tracker says so explicitly
//! rather than implying otherwise in a doc comment.
//!
//! What the PID/heartbeat *is* for, and what this implementation actually
//! covers:
//!
//! - **A useful `LeaseHeld { pid }` error.** `flock` contention alone does
//!   not reveal who holds the lock; reading the file's own recorded PID is
//!   how [`Lease::acquire`] reports it.
//! - **PID-reuse protection for a genuinely dead holder.** If a process
//!   dies, the kernel releases its `flock` immediately, so a *new*
//!   `try_write` on a truly abandoned lock simply succeeds — no staleness
//!   logic is even reached. The staleness path in [`Lease::acquire`]
//!   ([`is_stale`]) exists for the residual case that path leaves
//!   unhandled: a caller's `is_alive` check reports a PID as dead while an
//!   OS-level lock on that file is nonetheless still contended (for
//!   example, a caller supplying a liveness probe that races or is wrong).
//!   Requiring *both* "not alive" and "heartbeat older than `stale_after`"
//!   before taking over is this module's defence against a false "dead"
//!   verdict: a live-but-quiet holder (long-running turn, no heartbeat
//!   renewal implemented yet — see below) is still protected as long as
//!   `is_alive` correctly reports it as alive.
//! - **What this module does *not* cover:** filesystems where `flock` is
//!   not reliably kernel-enforced (NFS is the classic case; the plan
//!   already flags NFS session directories as a reason to revisit this
//!   design, not something this milestone handles) — there, a contended
//!   lock might not mean a live holder, and this module cannot tell the
//!   difference. Nor does it renew a held lease's heartbeat over its
//!   lifetime: the heartbeat is written once, at acquisition. A
//!   long-running holder is protected from wrongful takeover only by its
//!   `is_alive` check reporting true, not by a fresh heartbeat — renewal
//!   is future work, not attempted here.
//!
//! ## No OS-backed liveness probe ships in this crate
//!
//! [`Lease::acquire`] takes the current PID, the current time and a
//! liveness predicate as plain parameters — nothing here reads
//! `std::process::id()` or the wall clock, so a test can present a lock
//! file naming a PID that is definitively not alive and a heartbeat
//! definitively in the past without racing a real process or a real clock.
//! This crate deliberately does not also ship a real, OS-backed
//! implementation of that predicate: checking whether an arbitrary PID is
//! alive needs either an FFI call (`kill(pid, 0)` on Unix,
//! `OpenProcess`/`CloseHandle` on Windows) or a process-inspection
//! dependency, and the workspace denies `unsafe_code` outright while
//! `ego-tools`'s `Cargo.toml` carries no such dependency. Adding either is
//! outside this file's ownership. Until a real predicate is wired in
//! (wherever that unsafe/dependency boundary is judged to belong), callers
//! must supply their own; that wiring is explicitly out of scope for this
//! story.

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

/// A held per-session writer lease, backed by an advisory OS lock
/// (`fd-lock`) on a lock file at a caller-chosen path (`ARCHITECTURE.md`
/// §5: `<session-dir>/session.lock`). See the module doc comment for the
/// durability protocol this implements and its documented limits.
///
/// Released by dropping: this struct owns the locked [`File`], and closing
/// its descriptor releases the OS-level lock. Drop also removes this holder
/// from the optional host PID index, unless a newer binding replaced it.
/// The lock file itself is deliberately
/// left in place on release rather than removed: an unlink here could race
/// a *new* holder that has already taken the path over (see
/// [`Lease::acquire`]'s takeover path, which replaces rather than rewrites
/// in place precisely so the old holder's descriptor keeps pointing at an
/// orphaned inode instead of the live one) — deleting blind could destroy
/// a legitimately active lease file out from under its new owner.
pub struct Lease {
    // Kept alive for its `Drop` impl, which is what releases the
    // underlying advisory lock; never read again after acquisition.
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
                // descriptor, and closing that on `Lease`'s own drop is
                // what actually releases the lock (see the struct doc
                // comment).
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
