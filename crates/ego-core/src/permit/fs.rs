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

use futures::future::{self, BoxFuture};

use super::grant::{Permit, PermitDenied};
use super::path::CanonPath;
use super::scan::{Entry, EntryKind, Found, LineMatcher, Match, Scan, ScanRequest};
use super::spawn::{ProcessError, ProcessExecutor, SpawnSpec};
use super::tool::{ToolError, ToolResult};
use crate::store::evidence::{EvidencePage, EvidenceRead, EvidenceStore};

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
    /// A traversal was rooted at something that is not a directory.
    ///
    /// The mirror of [`FsError::NotAFile`], and the two are not
    /// interchangeable: one says "you asked to read a directory", the other
    /// "you asked to walk a file". A model told the wrong one goes looking in
    /// the wrong place.
    NotADirectory {
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
            | FsError::NotADirectory { path }
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
            FsError::NotADirectory { path } => write!(f, "{path}: not a directory"),
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
/// the loop and wrapped in [`GuardedEffects`] before a tool ever sees it.
pub trait Fs: PathResolver {
    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError>;

    /// Replace the whole file at `path` with `bytes`.
    ///
    /// A persistent implementation publishes the replacement atomically and
    /// makes both its contents and directory entry durable before returning
    /// success. An in-memory test double has no durability boundary, but must
    /// still make the replacement visible as one operation.
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
    /// Atomically and durably replace one permit-checked whole file.
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

    /// Execute the exact process specification admitted by this permit.
    fn exec<'a>(&'a self, spec: &'a SpawnSpec) -> BoxFuture<'a, Result<ToolResult, ProcessError>>;

    /// Read the exact bounded evidence request admitted inside this session.
    fn read_evidence<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, ToolError>>;
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
/// paired with the raw effect ports it governs.
///
/// **Owns the permit rather than borrowing it.** The seam that enforces a
/// grant is where the grant should end up: once a `GuardedEffects` exists the
/// permit is inside it, unreachable, and it dies with the seam. A borrowed
/// permit meant somebody upstream still held one for the duration of the
/// call, and that somebody was the only remaining place a permit and a
/// foreign call could be brought together.
///
/// It is also the only shape that compiles. With `permit: &'a Permit`, a
/// caller cannot both hand an [`Authorized`](super::Authorized) to
/// `Tool::execute` by value and keep a `GuardedEffects` built from that same
/// permit — `E0505`, move out of a borrowed value, whose suggested fix is
/// `Clone` on `Authorized`. That is the one thing Invariant W forbids.
pub struct GuardedEffects<'a> {
    permit: Permit,
    fs: &'a dyn Fs,
    process: &'a dyn ProcessExecutor,
    evidence: &'a dyn EvidenceStore,
}

impl<'a> GuardedEffects<'a> {
    pub fn new(
        permit: Permit,
        fs: &'a dyn Fs,
        process: &'a dyn ProcessExecutor,
        evidence: &'a dyn EvidenceStore,
    ) -> Self {
        Self {
            permit,
            fs,
            process,
            evidence,
        }
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
        allows(&self.permit, &fresh)?;
        Ok(ScanRequest {
            root: fresh,
            skip_dirs: req.skip_dirs.clone(),
            limits: req.limits.clone(),
        })
    }

    /// Re-derive what a write is about to land on, which may not exist yet.
    ///
    /// [`read`](GuardedIo::read) can simply canonicalize: a file it is about
    /// to read is on disk or the call was a mistake. A write's target
    /// frequently is not on disk — creating a file is most of what writing is
    /// for — and `Fs::canonicalize` cannot resolve what is not there, so the
    /// two cases are separated here rather than in every caller:
    ///
    /// - **The target exists.** It is judged as what it *is*. A final
    ///   component that is a symlink out of the workspace resolves to its
    ///   target, so the permit is shown the file the bytes would actually
    ///   reach — the same rule `Fs::scan` follows when it reports a symlinked
    ///   file under its canonical name.
    /// - **It does not.** The *parent* is resolved and the final component
    ///   appended, so every component but the last is still chased. The last
    ///   is the one a creating write must be allowed not to find.
    ///
    /// A missing parent stays `NotFound`, naming the parent. A write is not a
    /// `mkdir -p`: creating directories is an effect the call did not
    /// describe and the permit was never shown.
    fn write_target(&self, path: &CanonPath) -> Result<CanonPath, IoDenied> {
        match self.fs.canonicalize(path.as_path()) {
            Ok(fresh) => Ok(fresh),
            Err(FsError::NotFound { .. }) => {
                let named = path.as_path();
                let (Some(parent), Some(name)) = (named.parent(), named.file_name()) else {
                    // A path that is a root, or ends in `..`. There is no file
                    // for a write to name here, and the error says which path
                    // was refused rather than which component was missing.
                    return Err(FsError::NotAFile { path: path.clone() }.into());
                };
                let parent = self.fs.canonicalize(parent)?;
                Ok(CanonPath::new(parent.as_path().join(name)))
            }
            Err(other) => Err(other.into()),
        }
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
            allows(&self.permit, &entry.path)?;
        }
        Ok(())
    }
}

impl GuardedIo for GuardedEffects<'_> {
    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), IoDenied> {
        // Re-resolve now, at use time, rather than trusting `path` — see
        // the trait doc comment. `write_target` and not `canonicalize`,
        // because a write's target is allowed not to exist yet.
        let fresh = self.write_target(path)?;
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
            // The scan's path was true when the directory entry was visited,
            // not necessarily now. Resolve it again immediately before the
            // read and cash the search authority against that fresh target.
            // A swapped symlink that leaves the grant is a denial of the
            // whole search; bytes from the escaped target are never opened.
            let fresh = match self.fs.canonicalize(entry.path.as_path()) {
                Ok(fresh) => fresh,
                // Preserve the traversal contract below: a file that
                // disappears or becomes unreadable mid-search is skipped.
                Err(_) => continue,
            };
            self.permit.allows_search(&fresh)?;

            // A file the OS refuses mid-traversal is skipped, not fatal: one
            // unreadable file in a tree is a fact about that file, and
            // abandoning the whole search over it would make grep unusable in
            // any directory containing a socket or a root-owned log.
            let Ok(bytes) = self.fs.read(&fresh) else {
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
                        path: fresh.clone(),
                        line: index + 1,
                        text: line.to_string(),
                    });
                }
            }
        }

        Ok(Found { matches, truncated })
    }

    fn exec<'a>(&'a self, spec: &'a SpawnSpec) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        if let Err(denied) = self.permit.allows_spawn(spec) {
            return Box::pin(future::ready(Err(ProcessError::Invariant(
                denied.to_string(),
            ))));
        }
        self.process.execute(spec)
    }

    fn read_evidence<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, ToolError>> {
        if let Err(denied) = self.permit.allows_evidence(request) {
            return Box::pin(future::ready(Err(denied.into())));
        }
        Box::pin(async move { self.evidence.read_page(request).await.map_err(Into::into) })
    }
}
