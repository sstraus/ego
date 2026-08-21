//! `PreparedCall`: canonical operations, resolved at `prepare` time — never
//! prose, never a rendered shell string (`ARCHITECTURE.md` §3.2, §3.4).
//! This is what the policy engine classifies to mint a `Permit`, and what a
//! judge model evaluates in `auto` mode.

use super::path::CanonPath;

/// One canonical operation a `Tool::execute` intends to perform.
///
/// M0a's [`GuardedIo`](super::GuardedIo) covers only the fs subset here;
/// `Connect`/`Spawn` variants belong to the process/network confinement
/// stories that build on this one (`ARCHITECTURE.md` §3.4 axis 4).
///
/// [`Op::List`] and [`Op::Search`] exist so that a tool which walks a subtree
/// does not have to be described as one that reads it. See
/// `docs/decisions/2026-08-21-reading-without-reading-everything.md` §1–2:
/// expressing a glob as `Read { path: root }` would hand it the authority to
/// read every byte under `root`, which is not a rounding error in the
/// description — it is a much larger grant, arrived at because no other
/// variant existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// One named file's contents.
    Read {
        path: CanonPath,
    },
    /// The names under `root` matching `pattern`, and nothing else about
    /// them.
    List {
        root: CanonPath,
        pattern: String,
    },
    /// The lines under `root` matching `query`.
    ///
    /// The query is part of the operation, so a permit minted for one search
    /// authorises *that* search: a tool approved to look for `TODO` cannot
    /// then go looking for private keys.
    Search {
        root: CanonPath,
        query: String,
    },
    Write {
        path: CanonPath,
        contents: Vec<u8>,
    },
}

impl Op {
    /// The violating-operation name a denial or an approval prompt shows.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Read { .. } => "read",
            Op::List { .. } => "list",
            Op::Search { .. } => "search",
            Op::Write { .. } => "write",
        }
    }

    /// The path this operation is judged against: the file for a read or a
    /// write, the root for a traversal.
    pub fn path(&self) -> &CanonPath {
        match self {
            Op::Read { path } | Op::Write { path, .. } => path,
            Op::List { root, .. } | Op::Search { root, .. } => root,
        }
    }
}

/// A tool call reduced to its canonical operations by `Tool::prepare`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCall {
    pub tool: String,
    pub ops: Vec<Op>,
}
