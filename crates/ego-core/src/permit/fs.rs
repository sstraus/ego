//! `Fs`, the raw filesystem seam, and `GuardedIo`, the permit-checking
//! facade tools actually receive.
//!
//! `ARCHITECTURE.md` §2/§3.3: `ego-core` opens no files itself — `Fs` is a
//! trait implemented elsewhere (a fake, in-memory one in this crate's
//! tests; the real OS-backed one lives in `ego-tools`). `PathResolver` is the
//! narrower slice of it exposed at `prepare` time, before any `Permit`
//! exists: enough to chase symlinks into canonical operations (§3.2), never
//! enough to read or write content.

use std::fmt;
use std::path::Path;

use super::grant::{Permit, PermitDenied};
use super::path::CanonPath;
use super::scan::{Entry, EntryKind, Found, LineMatcher, Match, Scan, ScanRequest};

/// Why a filesystem operation failed, as opposed to being *denied*.
///
/// The distinction is the point: a tool that was refused and a tool that
/// looked and found nothing are telling the model two different things, and a
/// single "it didn't work" collapses them. See
/// `docs/decisions/2026-08-21-reading-without-reading-everything.md` §6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    NotFound {
        path: CanonPath,
    },
    NotAFile {
        path: CanonPath,
    },
    /// Too many symlinks were chased. A loop, or a chain deep enough to be
    /// indistinguishable from one.
    Loop {
        path: CanonPath,
    },
    /// Anything the platform reported that the variants above do not name.
    /// Carries the message rather than `std::io::Error`, which is neither
    /// `Clone` nor `PartialEq` and would infect every type that holds one.
    Io {
        path: CanonPath,
        message: String,
    },
}

impl FsError {
    pub fn path(&self) -> &CanonPath {
        match self {
            FsError::NotFound { path }
            | FsError::NotAFile { path }
            | FsError::Loop { path }
            | FsError::Io { path, .. } => path,
        }
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound { path } => write!(f, "{path}: no such file or directory"),
            FsError::NotAFile { path } => write!(f, "{path}: not a file"),
            FsError::Loop { path } => write!(f, "{path}: too many levels of symbolic links"),
            FsError::Io { path, message } => write!(f, "{path}: {message}"),
        }
    }
}

impl std::error::Error for FsError {}

/// Read-only path resolution available at `prepare` time. Deliberately
/// smaller than [`Fs`]: a tool being prepared has no `Permit` yet, so it
/// must not be able to read or write through this seam — only ask what a
/// path currently resolves to.
pub trait PathResolver: Send + Sync {
    /// Resolve `path` to its current canonical target.
    ///
    /// Fallible, because a real filesystem cannot canonicalize what is not
    /// there — and "not there" is exactly what `prepare` is handed when the
    /// model names a file that does not exist.
    fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError>;
}

/// The raw filesystem seam. Unchecked: nothing here consults a `Permit`.
/// Tools never hold this directly (`ARCHITECTURE.md` §3.2); it is held by
/// the loop and wrapped in [`GuardedFs`] before a tool ever sees it.
pub trait Fs: PathResolver {
    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError>;

    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), FsError>;

    /// Walk the subtree `req` names, honouring its skips and limits.
    ///
    /// **An implementation must not descend into a directory symlink.** A walk
    /// resolves many paths where a read resolves one, and a symlinked
    /// directory inside an in-scope root is an escape the root check cannot
    /// see. Refusing to follow them closes it, and makes cycles impossible
    /// rather than merely detectable. A symlinked *file* is still resolved and
    /// still reported, because that is one path and the permit check handles
    /// it.
    fn scan(&self, req: &ScanRequest) -> Result<Scan, FsError>;
}

/// The fs subset of the permit-checking facade a `Tool::execute` actually
/// receives (`ARCHITECTURE.md` §3.2). Every method re-derives the current
/// canonical target before checking the permit: trusting a path resolved
/// back at `prepare` time would let a symlink swapped in between `prepare`
/// and `execute` slip a write through to a target that was never checked.
///
/// That is the window this seam closes, and it is worth being precise about
/// which one it is. The check and the use still name the target by *path*,
/// not by handle, so a component swapped between `allows_read` and the open
/// is resolved a second time and is not re-checked. Closing that needs
/// handle-relative, no-follow opens, which is `POTENTIALS.md` P23.
pub trait GuardedIo: Send + Sync {
    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), IoDenied>;

    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, IoDenied>;

    /// The names under `req.root`, and nothing else about them.
    fn list(&self, req: &ScanRequest) -> Result<Scan, IoDenied>;

    /// The lines under `req.root` that `matcher` accepts.
    ///
    /// The seam reads the files; the caller supplies only the judgement and
    /// receives only the verdicts. That is what lets `grep` work without ever
    /// holding read authority over the subtree
    /// (`docs/decisions/2026-08-21-reading-without-reading-everything.md` §2).
    fn search(&self, req: &ScanRequest, matcher: &dyn LineMatcher) -> Result<Found, IoDenied>;
}

/// Why a guarded operation did not produce what was asked for: the permit
/// said no, or the filesystem did.
///
/// Two variants and not one string, because a model that is told "denied"
/// should stop asking, and a model that is told "not found" should try
/// somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoDenied {
    Permit(PermitDenied),
    Fs(FsError),
}

impl fmt::Display for IoDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IoDenied::Permit(denied) => write!(f, "{denied}"),
            IoDenied::Fs(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for IoDenied {}

impl From<PermitDenied> for IoDenied {
    fn from(denied: PermitDenied) -> Self {
        IoDenied::Permit(denied)
    }
}

impl From<FsError> for IoDenied {
    fn from(err: FsError) -> Self {
        IoDenied::Fs(err)
    }
}

/// The only implementation of [`GuardedIo`] in this crate: a [`Permit`]
/// paired with the raw [`Fs`] it governs.
pub struct GuardedFs<'a> {
    permit: &'a Permit,
    fs: &'a dyn Fs,
}

impl<'a> GuardedFs<'a> {
    pub fn new(permit: &'a Permit, fs: &'a dyn Fs) -> Self {
        Self { permit, fs }
    }

    /// Re-resolve `root`, check it against the permit with `allows`, and hand
    /// back a request rooted at the *fresh* target.
    ///
    /// This runs before anything is opened. A traversal that walked first and
    /// filtered afterwards would already have read the directory it was about
    /// to refuse — the denial would have been performed and only its report
    /// suppressed — and would turn a refusal into a partial success, which
    /// tells a model "here are three matches" when the true answer is "you may
    /// not look there" (`plans/m0b-vertical-slice.md:59-60`).
    fn admit(
        &self,
        req: &ScanRequest,
        allows: fn(&Permit, &CanonPath) -> Result<(), PermitDenied>,
    ) -> Result<ScanRequest, IoDenied> {
        let fresh = self.fs.canonicalize(req.root.as_path())?;
        allows(self.permit, &fresh)?;
        Ok(ScanRequest {
            root: fresh,
            skip_dirs: req.skip_dirs.clone(),
            limits: req.limits.clone(),
        })
    }

    /// Check every entry a traversal yielded, and fail the whole operation if
    /// any of them left the authorised subtree.
    ///
    /// Whole-operation, not per-entry: dropping the offenders and returning
    /// the rest is the silent post-filtering the previous method exists to
    /// avoid. An escape means the tree moved under the walk, and a partial
    /// answer assembled from a tree that moved is not worth handing to a
    /// model.
    fn all_in_scope(
        &self,
        entries: &[Entry],
        allows: fn(&Permit, &CanonPath) -> Result<(), PermitDenied>,
    ) -> Result<(), IoDenied> {
        for entry in entries {
            allows(self.permit, &entry.path)?;
        }
        Ok(())
    }
}

impl GuardedIo for GuardedFs<'_> {
    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), IoDenied> {
        // Re-resolve now, at use time, rather than trusting `path` — see
        // the trait doc comment.
        let fresh = self.fs.canonicalize(path.as_path())?;
        self.permit.allows_write(&fresh)?;
        Ok(self.fs.write(&fresh, bytes)?)
    }

    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, IoDenied> {
        let fresh = self.fs.canonicalize(path.as_path())?;
        self.permit.allows_read(&fresh)?;
        Ok(self.fs.read(&fresh)?)
    }

    fn list(&self, req: &ScanRequest) -> Result<Scan, IoDenied> {
        let admitted = self.admit(req, Permit::allows_list)?;
        let scan = self.fs.scan(&admitted)?;
        self.all_in_scope(&scan.entries, Permit::allows_list)?;
        Ok(scan)
    }

    fn search(&self, req: &ScanRequest, matcher: &dyn LineMatcher) -> Result<Found, IoDenied> {
        let admitted = self.admit(req, Permit::allows_search)?;
        let scan = self.fs.scan(&admitted)?;
        self.all_in_scope(&scan.entries, Permit::allows_search)?;

        let mut matches = Vec::new();
        let mut budget = admitted.limits.max_bytes;
        let mut truncated = scan.truncated;

        for entry in scan.entries.iter().filter(|e| e.kind == EntryKind::File) {
            // A file the OS refuses mid-traversal is skipped, not fatal: one
            // unreadable file in a tree is a fact about that file, and
            // abandoning the whole search over it would make grep unusable in
            // any directory containing a socket or a root-owned log.
            let Ok(bytes) = self.fs.read(&entry.path) else {
                continue;
            };
            if bytes.len() as u64 > budget {
                truncated = true;
                break;
            }
            budget -= bytes.len() as u64;

            // Lossy, and on purpose: a binary file is not a reason to fail a
            // text search, and a matcher that never accepts a replacement
            // character simply will not match one.
            for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if matcher.matches(line) {
                    matches.push(Match {
                        path: entry.path.clone(),
                        line: index + 1,
                        text: line.to_string(),
                    });
                }
            }
        }

        Ok(Found { matches, truncated })
    }
}
