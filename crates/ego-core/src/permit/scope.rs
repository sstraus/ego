//! `Scope`: what a minted [`Permit`](super::Permit) allows, expressed over
//! canonical structure — paths, network endpoints and process specs — never over
//! rendered shell strings (`ARCHITECTURE.md` §3.4 axis 2).

use super::path::CanonPath;
use super::spawn::SpawnSpec;
use super::{
    ForeignToolId, ForeignTransportTarget, HttpRequestDigest, InvocationDigest, NetworkTarget,
    SecretEgressAuthority,
};
use crate::store::evidence::EvidenceRead;

/// The boundaries a [`Permit`](super::Permit) enforces.
///
/// Every path list is a set of allowed prefixes: a path is in scope if it is,
/// or is a descendant of, one of them. An endpoint must exactly match one of
/// `endpoints`; a process must exactly equal `spawn`; session evidence must
/// exactly equal `evidence`.
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
    pub(super) network: Option<NetworkTarget>,
    pub(super) http_request_digest: Option<HttpRequestDigest>,
    pub(super) secret_egress: Option<SecretEgressAuthority>,
    pub(super) foreign_invocations: Vec<(ForeignToolId, InvocationDigest)>,
    pub(super) foreign_transport: Option<ForeignTransportTarget>,
    pub(super) spawn: Option<SpawnSpec>,
    pub(super) evidence: Option<EvidenceRead>,
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

    /// The exact logical or literal network target admitted for one hop.
    pub fn network(&self) -> Option<&NetworkTarget> {
        self.network.as_ref()
    }

    /// Every exact foreign operation fingerprint admitted by this call.
    pub fn foreign_invocations(&self) -> &[(ForeignToolId, InvocationDigest)] {
        &self.foreign_invocations
    }

    /// The exact route identity admitted for the foreign call.
    pub fn foreign_transport(&self) -> Option<&ForeignTransportTarget> {
        self.foreign_transport.as_ref()
    }

    /// The exact process specification this scope may spawn.
    pub fn spawn(&self) -> Option<&SpawnSpec> {
        self.spawn.as_ref()
    }

    /// The exact session evidence object this scope may read.
    pub fn evidence(&self) -> Option<&EvidenceRead> {
        self.evidence.as_ref()
    }
}
