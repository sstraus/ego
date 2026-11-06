//! The cold storage job: zstd siblings for sessions no writer touched for a while.
//!
//! A cold `events.jsonl`, `manifest.json` or `evidence/<id>.blob` is replaced by
//! `<name>.zst` (see `cold_read` for readers and restore). A live writer — a
//! held lease, a lease heartbeat newer than the threshold, or a held
//! `events.lock` — keeps the session plain. A writer that starts while the job
//! compresses wins: the job sees the changed lease or file and keeps the original.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ego_core::event::id::SessionId;

use ego_core::store::session_store::StoreError;

use super::JsonlStore;
use super::cold_read::packed_path;
use super::io::{FileRevision, open_directory, tmp_path_for};
use super::lease;

/// Measured on a real 5.7 MB session log: level 9 gives 5.6x at 174 MB/s,
/// level 19 6.0x at 3 MB/s. Level 9 converts a 5 GB store in minutes instead
/// of hours; decompression speed is the same for both.
const LEVEL: i32 = 9;
const LOG: &str = "events.jsonl";
const MANIFEST: &str = "manifest.json";
/// A file of one disk block or less saves no allocated space when
/// compressed, yet costs a create, fsync, rename and unlink: it stays plain.
const MIN_COMPRESSED_LEN: u64 = 4096;

/// When to compress, and how much of one core the job may use.
#[derive(Debug, Clone)]
pub struct ColdStoragePolicy {
    /// A session with no write for this long is cold.
    pub cold_after: Duration,
    /// Idle time per unit of work time; 3 bounds the job to 25% of one core.
    pub idle_ratio: u32,
}

impl ColdStoragePolicy {
    /// Seven days cold and a 25% duty cycle.
    pub fn new(cold_after: Duration) -> Self {
        Self {
            cold_after,
            idle_ratio: 3,
        }
    }
}

/// What one pass over the session root did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ColdStorageReport {
    pub compressed: usize,
    /// Skipped because a writer was live or became live during the job.
    pub live: usize,
    pub failed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Compressed { before: u64, after: u64 },
    NotCold,
    Live,
}

/// Nice increment for the job thread: below every interactive thread.
pub const JOB_NICE_INCREMENT: i32 = 10;

/// Lower the scheduling priority of the calling thread, which must be the
/// dedicated job thread. Linux nice is per task, so only this thread drops.
/// macOS and Windows have no safe thread-scoped call: there `setpriority`
/// would slow the whole process, turn path included, so the duty cycle in
/// [`ColdStoragePolicy`] is the only bound.
pub fn lower_job_thread_priority() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{getpriority_process, setpriority_process};
        let thread = Some(rustix::thread::gettid());
        let current = getpriority_process(thread)?;
        setpriority_process(thread, (current + JOB_NICE_INCREMENT).min(19))?;
    }
    Ok(())
}

/// One plain file and its pending compressed copy.
struct Target {
    plain: PathBuf,
    revision: Option<FileRevision>,
    staged: PathBuf,
}

impl JsonlStore {
    /// Compress every cold session under this store's root once. Another
    /// process running the same job makes this pass return an empty report.
    pub fn compress_cold_sessions(
        &self,
        policy: &ColdStoragePolicy,
        now: SystemTime,
    ) -> io::Result<ColdStorageReport> {
        let mut report = ColdStorageReport::default();
        let mut job = fd_lock::RwLock::new(open_job_lock(self.root())?);
        let _job = match job.try_write() {
            Ok(guard) => guard,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(report),
            Err(error) => return Err(error),
        };
        for dir in self.io.list_dirs(self.root())? {
            let Some(id) = dir
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<SessionId>().ok())
            else {
                continue;
            };
            match self.compress_session(&id, policy, now, &mut || {}) {
                Ok(Outcome::Compressed { before, after }) => {
                    report.compressed += 1;
                    report.bytes_before += before;
                    report.bytes_after += after;
                }
                Ok(Outcome::NotCold) => {}
                Ok(Outcome::Live) => report.live += 1,
                Err(error) => {
                    report.failed += 1;
                    tracing::debug!(session = %id, %error, "cold storage skipped a session");
                }
            }
        }
        Ok(report)
    }

    /// `before_commit` runs between compression and publication; tests use it
    /// to start a writer at the worst moment.
    pub(super) fn compress_session(
        &self,
        id: &SessionId,
        policy: &ColdStoragePolicy,
        now: SystemTime,
        before_commit: &mut dyn FnMut(),
    ) -> io::Result<Outcome> {
        let dir = self.session_dir(id);
        let log = dir.join(LOG);
        let manifest = dir.join(MANIFEST);
        let cutoff = now
            .checked_sub(policy.cold_after)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let Some(log_modified) = modified(&log)? else {
            return Ok(Outcome::NotCold);
        };
        if log_modified > cutoff || modified(&manifest)?.is_some_and(|time| time > cutoff) {
            return Ok(Outcome::NotCold);
        }
        let lease = dir.join("lease");
        let lease_revision = revision(&lease)?;
        if lease::recorded_heartbeat(&lease)?.is_some_and(|beat| beat.as_millis() > millis(cutoff))
            || lease::is_held(&lease)?
            || events_lock_held(&mut self.open_events_lock(id).map_err(store_error)?)?
        {
            return Ok(Outcome::Live);
        }
        let evidence = dir.join("evidence");
        remove_stale_staging(&dir)?;
        remove_stale_staging(&evidence)?;

        let mut plains = vec![log];
        if manifest.try_exists()? {
            plains.push(manifest);
        }
        for path in self.io.list_files(&evidence)? {
            if path
                .extension()
                .is_some_and(|extension| extension == "blob")
            {
                plains.push(path);
            }
        }
        // Small files stay plain. Evidence is immutable and a small plain log
        // or manifest is read and appended as before, so no revision of a
        // skipped file decides the commit.
        let mut large = Vec::with_capacity(plains.len());
        for plain in plains {
            if fs::metadata(&plain)?.len() > MIN_COMPRESSED_LEN {
                large.push(plain);
            }
        }
        if large.is_empty() {
            return Ok(Outcome::NotCold);
        }
        let plains = large;
        let mut targets = Vec::with_capacity(plains.len());
        let mut sizes = (0, 0);
        for plain in plains {
            let packed = packed_path(&plain);
            let target = Target {
                revision: revision(&plain)?,
                staged: tmp_path_for(&packed),
                plain,
            };
            let compressed = compress_file(&target.plain, &target.staged, policy);
            targets.push(target);
            match compressed {
                Ok((before, after)) => {
                    sizes.0 += before;
                    sizes.1 += after;
                }
                Err(error) => {
                    discard(&targets);
                    return Err(error);
                }
            }
        }

        before_commit();
        let result = self.commit(id, &lease, lease_revision, &targets);
        if !matches!(result, Ok(true)) {
            discard(&targets);
        }
        Ok(if result? {
            Outcome::Compressed {
                before: sizes.0,
                after: sizes.1,
            }
        } else {
            Outcome::Live
        })
    }

    /// Publish under `events.lock` only when no writer appeared: the lease
    /// file and every plain file still have the revision seen before
    /// compression. `false` means a writer won and nothing was published.
    fn commit(
        &self,
        id: &SessionId,
        lease: &Path,
        lease_revision: Option<FileRevision>,
        targets: &[Target],
    ) -> io::Result<bool> {
        let mut events = self.open_events_lock(id).map_err(store_error)?;
        let _guard = match events.try_write() {
            Ok(guard) => guard,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        };
        if revision(lease)? != lease_revision {
            return Ok(false);
        }
        for target in targets {
            if revision(&target.plain)? != target.revision {
                return Ok(false);
            }
        }
        publish(targets)?;
        Ok(true)
    }

    /// A writer's first act on a session: plain files back under
    /// `events.lock`, before replay and before the first new event. Evidence
    /// blobs are immutable and stay compressed; readers decompress them.
    pub(super) fn restore_plain_session(&self, id: &SessionId) -> Result<(), StoreError> {
        let dir = self.session_dir(id);
        if !self.io.exists(&dir)? {
            return Ok(());
        }
        let mut events = self.open_events_lock(id)?;
        let _guard = events.write()?;
        self.io.restore_plain(&dir.join(LOG))?;
        self.io.restore_plain(&dir.join(MANIFEST))?;
        Ok(())
    }
}

fn store_error(error: StoreError) -> io::Error {
    match error {
        StoreError::Io(error) => error,
        other => io::Error::other(other.to_string()),
    }
}

/// Probe without waiting; a writer holds this lock only for one mutation.
fn events_lock_held(lock: &mut fd_lock::RwLock<fs::File>) -> io::Result<bool> {
    match lock.try_write() {
        Ok(_guard) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
}

fn revision(path: &Path) -> io::Result<Option<FileRevision>> {
    match fs::metadata(path) {
        Ok(metadata) => FileRevision::from_metadata(&metadata).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Rename every staged copy into place, then drop the plain files.
fn publish(targets: &[Target]) -> io::Result<()> {
    for target in targets {
        fs::rename(&target.staged, packed_path(&target.plain))?;
    }
    sync_parents(targets)?;
    for target in targets {
        fs::remove_file(&target.plain)?;
    }
    sync_parents(targets)
}

fn sync_parents(targets: &[Target]) -> io::Result<()> {
    let mut parents: Vec<&Path> = targets.iter().filter_map(|t| t.plain.parent()).collect();
    parents.dedup();
    for parent in parents {
        open_directory(parent)?.sync_all()?;
    }
    Ok(())
}

fn discard(targets: &[Target]) {
    for target in targets {
        let _ = fs::remove_file(&target.staged);
    }
}

/// Stream `plain` into a fsynced zstd file at `staged`, then rest for
/// `idle_ratio` times the work time so the job stays a bounded background load.
fn compress_file(
    plain: &Path,
    staged: &Path,
    policy: &ColdStoragePolicy,
) -> io::Result<(u64, u64)> {
    let started = Instant::now();
    let source = fs::File::open(plain)?;
    let before = source.metadata()?.len();
    let mut file = fs::File::create(staged)?;
    // Session data is private: the copy gets the plain file's mode before
    // any byte is written, and rename keeps it.
    file.set_permissions(source.metadata()?.permissions())?;
    zstd::stream::copy_encode(source, &mut file, LEVEL)?;
    file.sync_all()?;
    let after = file.metadata()?.len();
    std::thread::sleep(started.elapsed() * policy.idle_ratio);
    Ok((before, after))
}

fn modified(path: &Path) -> io::Result<Option<SystemTime>> {
    match fs::metadata(path) {
        Ok(metadata) => metadata.modified().map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Staged copies left by a crashed or killed job. Only this job writes them,
/// and the job lock is held.
fn remove_stale_staging(dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        if path.to_str().is_some_and(|name| name.ends_with(".zst.tmp")) {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// One job per session root across processes, held for the whole pass.
fn open_job_lock(root: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(root)?;
    fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join("compress.lock"))
}

#[cfg(test)]
#[path = "cold_tests.rs"]
mod tests;
