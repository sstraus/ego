//! Path patterns: canonicalisation and matching.
//!
//! `docs/internals/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md` §5.
//! Two constructs and one meaning each — `*` within a segment, `**` across
//! whole segments — defined here rather than inherited from a glob crate,
//! because the two things that differ between glob libraries are exactly
//! those two, and a permission boundary that changes meaning on a
//! `cargo update` is not a boundary. A library may implement these rules; it
//! may not define them.

use std::fmt;
use std::path::{Component, Path};

use super::glob::{Case, Elem, Glob, GlobBuilder, matches_sequence};

/// NTFS ignores case, so on Windows `.SSH\id` and `.ssh\id` are one file and
/// an exact comparison would let a different spelling walk past a deny rule.
/// The rule ID stays the same everywhere: only the comparison changes.
const PATH_CASE: Case = if cfg!(windows) {
    Case::Folded
} else {
    Case::Exact
};
use super::parse::RuleParseError;

/// One segment of a canonicalised path pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// A single segment, in which `*` matches any run of characters —
    /// including none — and never crosses a `/`.
    One(Glob),
    /// `**`: zero or more whole segments.
    Any,
}

/// A canonicalised path pattern: the AST, not the text it was written as.
///
/// `read( ./src//lib.rs )` and `read(src/lib.rs)` produce the same value
/// here, and therefore the same [`RuleId`](super::RuleId).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern {
    /// A pattern written with a leading `/` is absolute and stays absolute.
    /// Everything else is anchored to the workspace root **at match time**,
    /// which is what lets a rule committed in `.ego/config.toml` mean the
    /// same thing in every checkout — and is why the ID is computed over the
    /// unanchored pattern.
    absolute: bool,
    segments: Vec<Segment>,
}

impl PathPattern {
    pub fn is_absolute(&self) -> bool {
        self.absolute
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Canonicalise the text between a head's parentheses.
    ///
    /// The whole of §5's path list happens here, so that no caller has to
    /// remember any of it and no second spelling can survive into an AST.
    pub fn parse(raw: &str) -> Result<Self, RuleParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(RuleParseError::EmptyPattern);
        }

        let absolute = trimmed.starts_with('/');
        let mut segments: Vec<Segment> = Vec::new();

        for piece in trimmed.split('/') {
            // `x//y`, `x/./y` and a trailing `x/` all collapse: an empty
            // piece or a `.` contributes no segment.
            if piece.is_empty() || piece == "." {
                continue;
            }

            if piece == ".." {
                // A `..` cancels the segment before it when that segment is a
                // literal, because `a/../b` denotes `b` and nothing else. It
                // may not cancel a wildcard: `*/..` is not `.`, since `*`
                // stands for a segment nobody has named, and pruning it would
                // silently widen the pattern.
                //
                // A `..` with nothing cancellable in front of it *survives*,
                // and a surviving `..` is refused rather than normalised. A
                // relative rule that leaves the workspace is either a mistake
                // or an attack, and rewriting either one into a rule that
                // works is the wrong service to perform.
                let cancellable = matches!(
                    segments.last(),
                    Some(Segment::One(glob)) if glob.as_literal().is_some()
                );
                if !cancellable {
                    return Err(RuleParseError::EscapingPattern(trimmed.to_string()));
                }
                segments.pop();
                continue;
            }

            segments.push(segment(piece, trimmed)?);
        }

        if segments.is_empty() {
            return Err(RuleParseError::EmptyPattern);
        }

        Ok(Self { absolute, segments })
    }

    /// The absolute pattern for a canonical `path`, and with `subtree`
    /// everything below it too.
    ///
    /// A path is not pattern text: on Windows its spelling is `C:\…` or
    /// `\\?\C:\…`, which [`parse`](Self::parse) reads as one relative segment
    /// that matches nothing. The pattern is built from the path's components,
    /// so the drive or UNC prefix is dropped as in [`matches`](Self::matches):
    /// on Windows the pattern covers the same path on every volume.
    /// `None` for a path without a root, a volume root, a non-UTF-8 component,
    /// a name holding `*`, which a pattern cannot state literally, or a name
    /// with leading or trailing whitespace, which its text form would lose.
    pub fn from_path(path: &Path, subtree: bool) -> Option<Self> {
        // `has_root`, not `is_absolute`: on Windows `\x` has a root but no
        // drive, and names the same place on every volume as the pattern does.
        if !path.has_root() {
            return None;
        }
        let mut segments = Vec::new();
        for name in segments_of(path)? {
            // Rule text is trimmed when it is reloaded, so edge whitespace
            // would turn `a.txt ` into the broader `a.txt` after a restart.
            if name.contains('*') || name != name.trim() {
                return None;
            }
            let mut glob = GlobBuilder::default();
            name.chars().for_each(|ch| glob.literal(ch));
            segments.push(Segment::One(glob.build()));
        }
        // A volume root is never one exact target, and `/**` would be every path.
        if segments.is_empty() {
            return None;
        }
        if subtree {
            segments.push(Segment::Any);
        }
        Some(Self {
            absolute: true,
            segments,
        })
    }

    /// Does this pattern cover `path`?
    ///
    /// `root` is the workspace root, supplied at match time and used only to
    /// anchor a relative pattern. `path` is a canonical path — what the
    /// platform's own `canonicalize` returned. There is no Unicode
    /// normalisation, and case is folded only on Windows, whose file system
    /// ignores it. The residual is that a pattern whose composition (or, off
    /// Windows, casing) differs from the on-disk name does not match, which
    /// fails closed and does so visibly.
    ///
    /// A Windows drive or UNC prefix is not a segment: an absolute pattern's
    /// leading `/` is the root of every volume, so `/**/.ssh/**` covers
    /// `C:\Users\u\.ssh` and `D:\home\.ssh` alike.
    pub fn matches(&self, path: &Path, root: &Path) -> bool {
        let target = if self.absolute {
            segments_of(path)
        } else {
            match path.strip_prefix(root) {
                Ok(relative) => segments_of(relative),
                Err(_) => return false,
            }
        };

        let Some(target) = target else { return false };
        let elements: Vec<Elem<'_>> = self
            .segments
            .iter()
            .map(|segment| match segment {
                Segment::One(glob) => Elem::One(glob),
                Segment::Any => Elem::Rest,
            })
            .collect();
        matches_sequence(&elements, &target, PATH_CASE)
    }

    /// The canonical bytes this pattern contributes to a rule's ID.
    pub fn canonical_bytes(&self, out: &mut Vec<u8>) {
        out.push(u8::from(self.absolute));
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        for segment in &self.segments {
            match segment {
                Segment::Any => out.push(1),
                Segment::One(glob) => {
                    out.push(0);
                    glob.canonical_bytes(out);
                }
            }
        }
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.absolute {
            f.write_str("/")?;
        }
        for (at, segment) in self.segments.iter().enumerate() {
            if at > 0 {
                f.write_str("/")?;
            }
            match segment {
                Segment::Any => f.write_str("**")?,
                Segment::One(glob) => write!(f, "{glob}")?,
            }
        }
        Ok(())
    }
}

/// One piece of a split pattern, validated.
fn segment(piece: &str, whole: &str) -> Result<Segment, RuleParseError> {
    if piece == "**" {
        return Ok(Segment::Any);
    }
    // `**` means "whole segments" and only that. `a**b` and `***` are neither
    // of the two constructs, so they are refused rather than given a third
    // meaning nobody would guess the same way twice.
    if piece.contains("**") {
        return Err(RuleParseError::BadStar(whole.to_string()));
    }

    let mut glob = GlobBuilder::default();
    for ch in piece.chars() {
        if ch == '*' {
            glob.star();
        } else {
            glob.literal(ch);
        }
    }
    Ok(Segment::One(glob.build()))
}

/// A path's segments, or `None` if it holds a component a canonical path
/// never has — a `..` or a `.`. The root and a Windows prefix are dropped.
fn segments_of(path: &Path) -> Option<Vec<&str>> {
    path.components()
        .filter(|component| !matches!(component, Component::RootDir | Component::Prefix(_)))
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect()
}
