//! `PreparedCall`: canonical operations, resolved at `prepare` time — never
//! prose, never a rendered shell string (`ARCHITECTURE.md` §3.2, §3.4).
//! This is what the policy engine classifies to mint a `Permit`, and what a
//! judge model evaluates in `auto` mode.

use std::fmt;

use serde::{Deserialize, Serialize};

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
    Read { path: CanonPath },
    /// The names under `root` matching `pattern`, and nothing else about
    /// them.
    List { root: CanonPath, pattern: String },
    /// The lines under `root` matching `query`.
    ///
    /// The query is part of the operation, so a permit minted for one search
    /// authorises *that* search: a tool approved to look for `TODO` cannot
    /// then go looking for private keys.
    Search { root: CanonPath, query: String },
    /// A named file, after [`Change`] is applied to it.
    Write { path: CanonPath, change: Change },
}

/// What a [`Op::Write`] does to the file it names.
///
/// The variation is here, in the payload, and **not** in a sibling top-level
/// `Op` variant. Two variants both meaning "write to this path" would need
/// [`Op::name`], [`Op::mutates`], `Rule::matches` and `scope_of` — four places
/// — to independently agree they are the same authority, with nothing holding
/// them to it. One `Op::Write` has one arm in each of the four.
///
/// That is the same test [`Op::List`] and [`Op::Search`] pass and these two
/// fail: those are separate variants because they are separate *authorities*
/// (see `2026-08-21-reading-without-reading-everything.md` §1–2). Replacing a
/// whole file and replacing a string inside it are one authority applied two
/// ways.
///
/// **[`Change::Replace`] does not determine the resulting bytes** and
/// [`Change::Whole`] does. That is a real difference in what a party
/// authorising this call is being shown, and it is unavoidable: the bytes an
/// edit produces are a function of the bytes already there, and the only
/// moment that could compute them is `prepare` — which has no read authority,
/// because it runs before a `Permit` exists. See
/// `docs/decisions/2026-08-23-an-edit-is-two-authorities-and-a-conditional-write.md`.
/// What is *not* weakened is the authority granted: both reach exactly the one
/// path the op names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Every byte of the file, replaced.
    Whole(Vec<u8>),
    /// The one occurrence of `old_string`, replaced by `new_string`.
    ///
    /// One, not the first: a tool preparing this must refuse a call whose
    /// `old_string` is not in the file exactly once. Choosing among several
    /// would be choosing a place the call never named.
    Replace {
        old_string: String,
        new_string: String,
    },
}

/// Which kind of operation a denial or a question is about.
///
/// A closed vocabulary rather than the `&'static str` this used to be. Two
/// reasons, and the second is the one that forced it:
///
/// - It is closed in fact — there are exactly four [`Op`] variants — so a
///   type that admits any string admits values the rest of the system cannot
///   act on.
/// - It is recorded in a durable event (`EventKind::PermissionRequested`),
///   and `&'static str` cannot deserialise: there is no owned data to borrow
///   from. `String` would decode, and would also let a hand-edited log
///   reappear as an op nothing can match on. This decodes to one of four
///   values or fails.
///
/// `Copy`, so carrying it costs nothing and no producer allocates on the path
/// that has already decided to stop and ask somebody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpName {
    Read,
    List,
    Search,
    Write,
}

impl fmt::Display for OpName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            OpName::Read => "read",
            OpName::List => "list",
            OpName::Search => "search",
            OpName::Write => "write",
        };
        write!(f, "{name}")
    }
}

impl Op {
    /// The violating-operation name a denial or an approval prompt shows.
    pub fn name(&self) -> OpName {
        match self {
            Op::Read { .. } => OpName::Read,
            Op::List { .. } => OpName::List,
            Op::Search { .. } => OpName::Search,
            Op::Write { .. } => OpName::Write,
        }
    }

    /// Does this operation change the world?
    ///
    /// The one axis the mode floor and ceiling turn on
    /// (`2026-08-21-a-permit-nobody-can-widen.md` §6): `plan` refuses every
    /// mutating op outright, and `default` reviews unresolved *sensitive*
    /// actions rather than every action, because `ARCHITECTURE.md:238` counts
    /// prompt fatigue as a security bug. A read inside the workspace that
    /// stopped the run to ask would be that bug.
    ///
    /// Derived from the variant and not from a table, because unlike §6a's
    /// head→operations mapping this is not a fact about *this build* — a
    /// write mutates in every build there will ever be.
    pub fn mutates(&self) -> bool {
        match self {
            Op::Read { .. } | Op::List { .. } | Op::Search { .. } => false,
            Op::Write { .. } => true,
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
