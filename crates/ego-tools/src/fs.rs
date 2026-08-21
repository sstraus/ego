//! `OsFs`: the [`Fs`] a model's tools actually run against.
//!
//! The only *model-controlled* filesystem adapter in the workspace — the
//! session store touches `std::fs` too, but behind a seam no tool can reach
//! (`docs/decisions/2026-08-21-reading-without-reading-everything.md` §7).
//! Nothing here consults a `Permit`: this is the raw seam, held by the turn
//! loop and wrapped in `GuardedFs` before a tool ever sees it.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

use ego_core::permit::{CanonPath, Entry, EntryKind, Fs, FsError, PathResolver, Scan, ScanRequest};

/// `std::fs`, behind the port.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsFs;

/// True when the platform gave up chasing symlinks.
///
/// `std::io::ErrorKind::FilesystemLoop` is the name for this and is still
/// unstable (rust#86442), so the errno is read directly. `ELOOP` is frozen in
/// each platform's ABI — it cannot drift the way an `ErrorKind` mapping can —
/// and the test that builds a real symlink cycle is what proves the value we
/// were compiled against.
#[cfg(unix)]
fn is_symlink_loop(err: &io::Error) -> bool {
    const ELOOP: i32 = if cfg!(target_os = "linux") { 40 } else { 62 };
    err.raw_os_error() == Some(ELOOP)
}

/// Nothing to recognise: a platform whose `ELOOP` this code does not know
/// reports the loop as [`FsError::Io`], carrying the message the OS gave.
/// Guessing would be worse than quoting.
#[cfg(not(unix))]
fn is_symlink_loop(_err: &io::Error) -> bool {
    false
}

impl OsFs {
    /// Translate what the OS said into what a model can act on.
    ///
    /// The four named variants are the four different next moves: look
    /// elsewhere, read it as a directory instead, walk it as a file instead,
    /// or stop chasing the link. Everything else keeps the platform's own
    /// words rather than being flattened into one of them.
    fn error(path: &Path, err: &io::Error) -> FsError {
        let path = CanonPath::new(path);
        match err.kind() {
            io::ErrorKind::NotFound => FsError::NotFound { path },
            io::ErrorKind::IsADirectory => FsError::NotAFile { path },
            io::ErrorKind::NotADirectory => FsError::NotADirectory { path },
            _ if is_symlink_loop(err) => FsError::Loop { path },
            _ => FsError::Io {
                path,
                message: err.to_string(),
            },
        }
    }
}

impl PathResolver for OsFs {
    fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError> {
        fs::canonicalize(path)
            .map(CanonPath::new)
            .map_err(|err| Self::error(path, &err))
    }
}

impl Fs for OsFs {
    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError> {
        fs::read(path.as_path()).map_err(|err| Self::error(path.as_path(), &err))
    }

    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), FsError> {
        fs::write(path.as_path(), bytes).map_err(|err| Self::error(path.as_path(), &err))
    }

    fn scan(&self, req: &ScanRequest) -> Result<Scan, FsError> {
        let mut walk = Walk {
            req,
            entries: Vec::new(),
            truncated: false,
        };
        // The root's own failure is the caller's problem — a scan rooted at a
        // file is `NotADirectory`, and there is no partial answer to give.
        walk.descend(req.root.as_path(), Path::new(""), 0)?;
        Ok(Scan::new(walk.entries, walk.truncated))
    }
}

/// One traversal in progress.
struct Walk<'a> {
    req: &'a ScanRequest,
    entries: Vec<Entry>,
    truncated: bool,
}

impl Walk<'_> {
    /// Visit everything directly inside `dir`, and recurse where allowed.
    ///
    /// `depth` counts levels already descended below the root, so the entries
    /// this call yields are at `depth`, and `max_depth: 0` means the root's own
    /// children and nothing deeper.
    ///
    /// Failure is not uniform, and deliberately so. This returns `Err` for the
    /// directory it was asked to open, but a *nested* directory it cannot open
    /// is skipped and logged: one unreadable subdirectory is a fact about that
    /// subdirectory, and abandoning the whole walk over it would make `glob`
    /// unusable in any tree containing a root-owned directory. Only the root's
    /// failure is fatal, because there is no partial answer to give when the
    /// thing you were pointed at is not walkable at all.
    fn descend(&mut self, dir: &Path, relative: &Path, depth: usize) -> Result<(), FsError> {
        // Read the names first and sort them: `read_dir` yields whatever order
        // the filesystem feels like, and a scan that truncates has to truncate
        // the same way twice or a model gets a different answer to the same
        // question.
        let mut names: Vec<OsString> = Vec::new();
        for entry in fs::read_dir(dir).map_err(|err| OsFs::error(dir, &err))? {
            match entry {
                Ok(entry) => names.push(entry.file_name()),
                Err(err) => tracing::warn!(
                    dir = %dir.display(),
                    %err,
                    "a directory entry could not be read and was skipped"
                ),
            }
        }
        names.sort();

        for name in names {
            if self.entries.len() >= self.req.limits.max_entries {
                self.truncated = true;
                return Ok(());
            }

            let at = dir.join(&name);
            let child = relative.join(&name);
            // `symlink_metadata`, never `metadata`: the question here is what
            // this entry *is*, not what it points at.
            let Ok(meta) = fs::symlink_metadata(&at) else {
                // Deleted between the listing and the stat, or refused. An
                // entry that cannot be described is not reported.
                continue;
            };

            if meta.is_symlink() {
                self.link(child, &at);
            } else if meta.is_dir() {
                self.directory(child, &at, &name, depth);
            } else if meta.is_file() {
                self.entries.push(Entry {
                    relative: child,
                    path: CanonPath::new(at),
                    kind: EntryKind::File,
                });
            }
            // A socket, fifo or device is neither a file to read nor a
            // directory to walk, and reporting one as either would be a lie
            // about what the next operation can do with it.
        }
        Ok(())
    }

    /// A symlink: resolved, reported if it lands on a file, and never
    /// descended into.
    ///
    /// This is the no-follow rule (`docs/decisions/2026-08-21-reading-without-reading-everything.md`
    /// §4). A walk resolves many paths where a read resolves one, so a
    /// symlinked directory inside an in-scope root is an escape the root check
    /// cannot see; refusing to enter one closes it and makes cycles impossible
    /// rather than merely detectable. A symlinked *file* is still reported —
    /// under its canonical target, so an escape is something the permit check
    /// can see and refuse.
    fn link(&mut self, relative: std::path::PathBuf, at: &Path) {
        let Ok(target) = fs::canonicalize(at) else {
            return; // dangling, or a loop
        };
        if fs::metadata(&target).is_ok_and(|meta| meta.is_file()) {
            self.entries.push(Entry {
                relative,
                path: CanonPath::new(target),
                kind: EntryKind::File,
            });
        }
    }

    /// A real directory: skipped, reported, or reported and walked.
    fn directory(
        &mut self,
        relative: std::path::PathBuf,
        at: &Path,
        name: &OsString,
        depth: usize,
    ) {
        // A skipped directory is not reported either. `skip_dirs` exists so
        // `target/` stays out of a glob's results, and a result listing the
        // directory it refused to look inside serves nobody. It is not a
        // security boundary: everything under it is still refused by the
        // permit exactly like any other path.
        if self.req.skips(&name.to_string_lossy()) {
            return;
        }
        self.entries.push(Entry {
            relative: relative.clone(),
            path: CanonPath::new(at),
            kind: EntryKind::Dir,
        });

        if depth >= self.req.limits.max_depth {
            // Truncated even though this directory might have been empty:
            // finding that out means opening it, which is the one thing the
            // limit forbids. The only answer available without doing the
            // forbidden I/O is "the walk stopped here", and a model that is
            // told a listing is complete when it is not concludes the file it
            // needs does not exist.
            self.truncated = true;
            return;
        }
        if let Err(err) = self.descend(at, &relative, depth + 1) {
            tracing::warn!(
                dir = %at.display(),
                %err,
                "a directory could not be walked and was skipped"
            );
        }
    }
}
