//! `CanonPath`: a path believed canonical at the moment it was produced.
//!
//! [`GuardedFs`](super::GuardedFs) re-derives the current truth from
//! [`Fs::canonicalize`](super::Fs::canonicalize) before ever trusting one, which is what
//! defeats a symlink swapped in between `prepare` and `execute` (`fs.rs`).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A path, as resolved by some [`Fs`](super::Fs) at a point in time.
/// Serialization adds no constructor authority: `CanonPath::new` is already
/// public and performs no resolution. Unlike scope, this value cannot mint
/// a permit merely by being constructed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
