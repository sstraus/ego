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

use serde::{Deserialize, Serialize};

/// A path, as resolved by some [`Fs`](super::Fs) at a point in time.
///
/// Serialisable because `Escalation` is, and `Escalation` is recorded in a
/// durable event. Unlike [`Scope`](super::Scope) — where a `Deserialize` impl
/// would be a public constructor for a value the permit ADR deliberately made
/// unconstructible — `CanonPath::new` is already public and does no
/// resolution, so serde adds no capability that was not already there.
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

/// True when `text` names an NTFS alternate data stream: a `:` anywhere past
/// the drive or verbatim prefix.
///
/// Policy judges `dir\\name:stream` by its text, but NTFS opens or creates the
/// base file `dir\\name`, so a write the rules admitted as `a.md` can land on
/// `a.exe`, and `.git::$DATA` creates the protected `.git`. Pure text on
/// purpose, so the rule is testable on every host; [`names_alternate_stream`]
/// decides where it applies.
pub fn has_stream_spelling(text: &str) -> bool {
    let rest = ["\\\\?\\", "\\\\.\\", "//?/", "//./"]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .unwrap_or(text);
    let mut chars = rest.chars();
    let rest = match (chars.next(), chars.next()) {
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => chars.as_str(),
        _ => rest,
    };
    rest.contains(':')
}

/// [`has_stream_spelling`] on the hosts whose file system has streams.
///
/// A colon in a Unix file name is an ordinary character.
pub fn names_alternate_stream(path: &Path) -> bool {
    cfg!(windows) && has_stream_spelling(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::has_stream_spelling;

    // Catches: a stream spelling slipping past because the drive colon was
    // mistaken for it, or a drive-qualified plain path refused.
    #[test]
    fn only_a_colon_past_the_drive_prefix_is_a_stream() {
        for stream in [
            "C:\\ws\\.git::$DATA",
            "C:\\ws\\a.exe:x.md",
            "\\\\?\\C:\\ws\\a:b",
            "C:\\ws:evil\\a",
            "ws\\a:b",
            "C::$DATA",
        ] {
            assert!(has_stream_spelling(stream), "{stream}");
        }
        for plain in [
            "C:\\ws\\a.md",
            "\\\\?\\C:\\ws",
            "ws\\a",
            "C:a",
            "/ws/a",
            "\\\\host\\share\\a",
        ] {
            assert!(!has_stream_spelling(plain), "{plain}");
        }
    }
}
