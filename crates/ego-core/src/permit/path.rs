//! `CanonPath`: a path believed canonical at the moment it was produced.
//!
//! `ARCHITECTURE.md` §3.2: symlinks are chased into `PreparedCall`'s
//! canonical operations. "Believed" is the load-bearing word — a
//! `CanonPath` is a snapshot, not a promise that the target stays put.
//! [`GuardedFs`](super::GuardedFs) re-derives the current truth from
//! [`Fs::canonicalize`](super::Fs::canonicalize) before ever trusting one,
//! which is what defeats a symlink swapped in between `prepare` and
//! `execute` (`fs.rs`).

use std::fmt;
use std::path::{Path, PathBuf};

/// A path, as resolved by some [`Fs`](super::Fs) at a point in time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonPath(PathBuf);

impl CanonPath {
    /// Wraps `path` as-is. Callers are responsible for actually resolving
    /// it first (via [`PathResolver::canonicalize`](super::PathResolver::canonicalize))
    /// when the value needs to be trustworthy; this constructor does no
    /// resolution of its own, so it also serves fixtures and `Scope`
    /// boundaries that are configured directly rather than resolved.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// True if `self` is `base`, or a descendant of it.
    pub fn is_or_under(&self, base: &CanonPath) -> bool {
        self.0.starts_with(&base.0)
    }
}

impl fmt::Display for CanonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}
