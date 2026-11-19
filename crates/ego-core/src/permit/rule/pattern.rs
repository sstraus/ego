//! Path patterns: canonicalisation and matching.
//!
//! `*` matches within one segment; `**` spans whole segments. These
//! permission semantics are defined here, not inherited from a glob
//! library whose update could change authority.

use std::fmt;
use std::path::{Component, Path};

use super::glob::{Elem, Glob, GlobBuilder, matches_sequence};
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
/// `read(./src//lib.rs )` and `read(src/lib.rs)` produce the same value
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

    /// Canonicalize path-pattern text in one place so equivalent spellings cannot survive into
    /// different AST identities.
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

    /// Does this pattern cover `path`?
    ///
    /// `root` is the workspace root, supplied at match time and used only to
    /// anchor a relative pattern. `path` is a canonical path — what the
    /// platform's own `canonicalize` returned — and the comparison is exact:
    /// no case folding and no Unicode normalisation, so an ID means the same
    /// thing on every machine. The residual is that a pattern whose casing or
    /// composition differs from the on-disk name does not match, which fails
    /// closed and does so visibly.
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
        matches_sequence(&elements, &target)
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
/// never has — a `..`, a `.`, or a Windows prefix.
fn segments_of(path: &Path) -> Option<Vec<&str>> {
    path.components()
        .filter(|component| !matches!(component, Component::RootDir))
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect()
}
