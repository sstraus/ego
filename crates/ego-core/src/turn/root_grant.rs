//! Human-owned root requests after a prepared workspace-boundary refusal.

use futures::future::BoxFuture;

use crate::event::id::{ItemId, SessionId};
use crate::permit::{CanonPath, OpName, RootAccess};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootGrantRequest {
    pub tool: String,
    pub operation: OpName,
    pub path: CanonPath,
    pub access: RootAccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootGrantChoice {
    Deny,
    Session,
    Permanent(crate::permit::RootConsent),
}

/// An interactive host owns the answer and the permanent config transaction.
/// Core journals pending intent before publication and commits only after durability.
pub trait RootGrantPort: Send + Sync {
    fn ask<'a>(
        &'a self,
        session: SessionId,
        item: ItemId,
        request: RootGrantRequest,
    ) -> BoxFuture<'a, Result<RootGrantChoice, String>>;

    /// Publish the approved config revision only after core persists its pending receipt.
    fn publish<'a>(
        &'a self,
        consent: &'a crate::permit::PermanentRootConsent,
    ) -> BoxFuture<'a, Result<(), String>>;
}
