//! `Scope`: what a minted [`Permit`](super::Permit) allows, expressed over
//! canonical structure — paths, network endpoints and argv — never over
//! rendered shell strings (`ARCHITECTURE.md` §3.4 axis 2).

use super::path::CanonPath;

/// The boundaries a [`Permit`](super::Permit) enforces.
///
/// Every list is a set of allowed *prefixes*: a path is in scope if it is,
/// or is a descendant of, one of `write_paths`/`read_paths`; an argv is in
/// scope if it starts with one of `argv`'s patterns; an endpoint is in
/// scope if it exactly matches one of `endpoints`. Read and write are
/// scoped separately — a permit that can write somewhere is not thereby
/// allowed to read it back, or vice versa.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub write_paths: Vec<CanonPath>,
    pub read_paths: Vec<CanonPath>,
    pub endpoints: Vec<String>,
    pub argv: Vec<Vec<String>>,
}
