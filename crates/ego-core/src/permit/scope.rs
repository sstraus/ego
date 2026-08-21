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
/// **The fields are `pub(super)`, not `pub`.** `Scope` is public because a
/// `rules explain` and the audit event both have to render one, and it is
/// readable through the accessors below. What no longer exists is the ability
/// to *compose* one from outside `crate::permit`: a `Scope` literal is a
/// statement about what some permit may do, and until this was closed any
/// holder of a `&PolicyEngine` could write that statement itself and mint it
/// (`docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §2).
///
/// Rust has no per-file visibility, so `pub(super)` is the tightest bound
/// available and it admits the whole `permit` module rather than `grant.rs`
/// alone. That is weaker than the ADR's sentence and it does not matter:
/// [`mint`](super::PolicyEngine) *is* private to `grant.rs`, and a `Scope`
/// nobody can mint from is an inert list of paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub(super) write_paths: Vec<CanonPath>,
    pub(super) read_paths: Vec<CanonPath>,
    /// Subtrees whose *names* may be enumerated. Grants no access to content.
    pub(super) list_paths: Vec<CanonPath>,
    /// Subtrees that may be searched. Grants the matching lines of a query
    /// the permit was minted for, never arbitrary content.
    pub(super) search_paths: Vec<CanonPath>,
    pub(super) endpoints: Vec<String>,
    pub(super) argv: Vec<Vec<String>>,
}

impl Scope {
    /// Everything this scope may write to.
    pub fn write_paths(&self) -> &[CanonPath] {
        &self.write_paths
    }

    /// Everything this scope may read.
    pub fn read_paths(&self) -> &[CanonPath] {
        &self.read_paths
    }

    /// Everything this scope may enumerate.
    pub fn list_paths(&self) -> &[CanonPath] {
        &self.list_paths
    }

    /// Everything this scope may search.
    pub fn search_paths(&self) -> &[CanonPath] {
        &self.search_paths
    }

    /// Every endpoint this scope may connect to.
    pub fn endpoints(&self) -> &[String] {
        &self.endpoints
    }

    /// Every argv prefix this scope may spawn.
    pub fn argv(&self) -> &[Vec<String>] {
        &self.argv
    }
}
