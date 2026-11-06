//! Independent path, network, process and evidence scopes.

use super::SessionProcessAction;
use super::path::CanonPath;
use super::spawn::SpawnSpec;
use super::{
    ForeignToolId, ForeignTransportTarget, HttpRequestDigest, InvocationDigest, NetworkTarget,
    SecretEgressAuthority,
};
use crate::event::id::DetachedExecId;
use crate::store::evidence::EvidenceRead;

/// The boundaries a [`Permit`](super::Permit) enforces.
/// Path lists contain allowed prefixes; endpoints, processes and evidence
/// require exact matches. Read, write, list and search remain independent.
/// `pub(super)` permits inspection within `permit`, but private minting in
/// `grant.rs` prevents external scope construction from producing authority.
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
    /// The owner named this exact endpoint, so web admission may dial a
    /// loopback, LAN or CGNAT address it resolves to. Never set by a mode.
    pub(super) owner_private_network: bool,
    pub(super) secret_egress: Option<SecretEgressAuthority>,
    pub(super) foreign_invocations: Vec<(ForeignToolId, InvocationDigest)>,
    pub(super) foreign_transport: Option<ForeignTransportTarget>,
    pub(super) spawns: Vec<SpawnSpec>,
    pub(super) evidence: Option<EvidenceRead>,
    pub(super) session_process: Option<(DetachedExecId, SessionProcessAction)>,
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

    /// The exact process specifications this scope may spawn.
    pub fn spawns(&self) -> &[SpawnSpec] {
        &self.spawns
    }

    /// The exact session evidence object this scope may read.
    pub fn evidence(&self) -> Option<&EvidenceRead> {
        self.evidence.as_ref()
    }
}
