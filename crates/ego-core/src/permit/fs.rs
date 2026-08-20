//! `Fs`, the raw filesystem seam, and `GuardedIo`, the permit-checking
//! facade tools actually receive.
//!
//! `ARCHITECTURE.md` §2/§3.3: `ego-core` opens no files itself — `Fs` is a
//! trait implemented elsewhere (a fake, in-memory one in this crate's
//! tests; the real OS-backed one lives in `ego-tools`, a later story).
//! `PathResolver` is the narrower slice of it exposed at `prepare` time,
//! before any `Permit` exists: enough to chase symlinks into canonical
//! operations (§3.2), never enough to read or write content.

use std::path::Path;

use super::grant::{Permit, PermitDenied};
use super::path::CanonPath;

/// Read-only path resolution available at `prepare` time. Deliberately
/// smaller than [`Fs`]: a tool being prepared has no `Permit` yet, so it
/// must not be able to read or write through this seam — only ask what a
/// path currently resolves to.
pub trait PathResolver: Send + Sync {
    fn canonicalize(&self, path: &Path) -> CanonPath;
}

/// The raw filesystem seam. Unchecked: nothing here consults a `Permit`.
/// Tools never hold this directly (`ARCHITECTURE.md` §3.2); it is held by
/// the loop and wrapped in [`GuardedFs`] before a tool ever sees it.
pub trait Fs: PathResolver {
    fn read(&self, path: &CanonPath) -> Option<Vec<u8>>;
    fn write(&self, path: &CanonPath, bytes: &[u8]);
}

/// The fs subset of the permit-checking facade a `Tool::execute` actually
/// receives (`ARCHITECTURE.md` §3.2). Every method re-derives the current
/// canonical target before checking the permit: trusting a path resolved
/// back at `prepare` time would let a symlink swapped in between `prepare`
/// and `execute` slip a write through to a target that was never checked —
/// the TOCTOU this seam exists to close.
pub trait GuardedIo: Send + Sync {
    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), PermitDenied>;
    fn read(&self, path: &CanonPath) -> Result<Option<Vec<u8>>, PermitDenied>;
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
}

impl GuardedIo for GuardedFs<'_> {
    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), PermitDenied> {
        // Re-resolve now, at use time, rather than trusting `path` — see
        // the trait doc comment.
        let fresh = self.fs.canonicalize(path.as_path());
        self.permit.allows_write(&fresh)?;
        self.fs.write(&fresh, bytes);
        Ok(())
    }

    fn read(&self, path: &CanonPath) -> Result<Option<Vec<u8>>, PermitDenied> {
        let fresh = self.fs.canonicalize(path.as_path());
        self.permit.allows_read(&fresh)?;
        Ok(self.fs.read(&fresh))
    }
}
