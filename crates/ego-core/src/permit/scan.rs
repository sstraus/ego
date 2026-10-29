//! The vocabulary a traversal is expressed in: what to walk, what it may
//! cost, and what comes back.
//!
//! Glob and grep both walk subtrees but learn different things: names or
//! matching lines. Describe both shapes so guarded execution enforces
//! the difference rather than trusting tool behavior.

use std::path::PathBuf;

use super::path::CanonPath;

/// What a traversal is allowed to cost.
///
/// Not advisory. A scan that runs until the filesystem runs out is a denial of
/// service against the process the model is talking through, and `ego-core`
/// has no runtime to time it out on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanLimits {
    /// How far below the root to descend. `0` visits the root's own children
    /// and nothing deeper.
    pub max_depth: usize,
    /// How many entries to return before stopping.
    pub max_entries: usize,
    /// How many bytes of file content a search may read in total. Ignored by
    /// a listing, which reads no content at all.
    pub max_bytes: u64,
}

impl Default for ScanLimits {
    /// Bounds a builtin tool can work inside without thinking about it, and
    /// small enough that hitting one is a signal rather than an outage.
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_entries: 10_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// A subtree to walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRequest {
    pub root: CanonPath,
    /// Directory *names* not to descend into — `.git`, `target`,
    /// `node_modules` and the like.
    ///
    /// A usability device and **not a security boundary**. A path inside a
    /// skipped directory is refused by the permit exactly like any other, and
    /// this list is never consulted when deciding whether something is in
    /// scope. Confusing the two would make a convenience default look like a
    /// containment guarantee.
    pub skip_dirs: Vec<String>,
    /// Leave out what the `.gitignore` files at the root, below it and in the
    /// enclosing repository ignore. The root itself is never judged: a caller
    /// that names an ignored directory asked for it.
    ///
    /// Like `skip_dirs`, a usability device and not a security boundary.
    pub respect_ignore: bool,
    pub limits: ScanLimits,
}

impl ScanRequest {
    /// A scan of `root` with default limits and nothing skipped.
    pub fn new(root: CanonPath) -> Self {
        Self {
            root,
            skip_dirs: Vec::new(),
            respect_ignore: false,
            limits: ScanLimits::default(),
        }
    }

    /// True if `name` is a directory this scan must not descend into.
    pub fn skips(&self, name: &str) -> bool {
        self.skip_dirs.iter().any(|skip| skip == name)
    }
}

/// What an [`Entry`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

/// One thing a traversal found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The path relative to the scan's root.
    ///
    /// Carried alongside the canonical one because it is what a glob pattern
    /// is written against — `**/*.rs` means something relative to where the
    /// search started, and a [`CanonPath`] has had that context resolved away.
    pub relative: PathBuf,
    /// The canonical target. This, never [`Entry::relative`], is what a permit
    /// is checked against.
    pub path: CanonPath,
    pub kind: EntryKind,
}

/// Everything a traversal found, and whether it found all of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub entries: Vec<Entry>,
    /// True when a [`ScanLimits`] bound stopped the walk before the subtree
    /// was exhausted.
    ///
    /// Reported rather than inferred from `entries.len()`, and reported at
    /// all because a model that cannot tell a complete answer from a truncated
    /// one concludes the file it needed does not exist.
    pub truncated: bool,
}

/// A `Vec` and not a stream, deliberately: [`ScanLimits::max_entries`] bounds
/// it before it is built, and a fallible stream through a `&dyn` seam buys
/// laziness that the bound has already paid for.
impl Scan {
    pub fn new(entries: Vec<Entry>, truncated: bool) -> Self {
        Self { entries, truncated }
    }
}

/// Decides whether a line of a file is a hit.
///
/// Supplied by the tool and run *by the guarded seam*, over content the tool
/// never receives. This is what lets `grep` return matching lines without ever
/// holding read authority over the subtree it searched: the seam reads, the
/// matcher judges, and only the judgements come back.
///
/// `ego-core` therefore needs no regex engine, and the tool's choice of one is
/// its own business.
pub trait LineMatcher: Sync {
    fn matches(&self, line: &str) -> bool;
}

impl<F: Fn(&str) -> bool + Sync> LineMatcher for F {
    fn matches(&self, line: &str) -> bool {
        self(line)
    }
}

/// One line that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub path: CanonPath,
    /// 1-based, the way every editor and every other grep counts.
    pub line: usize,
    pub text: String,
}

/// Everything a search found, and whether it found all of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub matches: Vec<Match>,
    /// True when a limit stopped the search early — see [`Scan::truncated`].
    pub truncated: bool,
}
