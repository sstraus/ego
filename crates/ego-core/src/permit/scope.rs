//! `Scope`: what a minted [`Permit`](super::Permit) allows, expressed over
//! canonical structure — paths, network endpoints and argv — never over
//! rendered shell strings (`ARCHITECTURE.md` §3.4 axis 2).

use super::path::CanonPath;

/// The boundaries a [`Permit`](super::Permit) enforces.
///
/// Every list is a set of allowed *prefixes*: a path is in scope if it is,
/// or is a descendant of, one of the path lists; an argv is in scope if it
/// starts with one of `argv`'s patterns; an endpoint is in scope if it
/// exactly matches one of `endpoints`.
///
/// The four path lists are scoped separately and do not imply one another. A
/// permit that can write somewhere is not thereby allowed to read it back;
/// and, less obviously but more importantly, a permit that may *list* a
/// directory is not thereby allowed to read what it finds there. Knowing that
/// `.env` exists and knowing what is in it are different authorities, and
/// collapsing them would mean a glob that can exfiltrate a workspace
/// (`docs/decisions/2026-08-21-reading-without-reading-everything.md` §1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub write_paths: Vec<CanonPath>,
    pub read_paths: Vec<CanonPath>,
    /// Subtrees whose *names* may be enumerated. Grants no access to content.
    pub list_paths: Vec<CanonPath>,
    /// Subtrees that may be searched. Grants the matching lines of a query
    /// the permit was minted for, never arbitrary content.
    pub search_paths: Vec<CanonPath>,
    pub endpoints: Vec<String>,
    pub argv: Vec<Vec<String>>,
}
