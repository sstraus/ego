//! Exact command approvals owned by one live session, never persisted as rules.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use super::{Op, PreparedCall};

/// Shared owner decisions for fresh policy captures of the same session.
/// Only the engine can record an approval, after the human answers Allow.
#[derive(Clone, Default)]
pub struct SessionApprovals(Arc<Mutex<HashSet<[u8; 32]>>>);

impl SessionApprovals {
    fn command(call: &PreparedCall) -> bool {
        !call.ops().is_empty() && call.ops().iter().all(|op| matches!(op, Op::Spawn { .. }))
    }

    pub(super) fn contains(&self, call: &PreparedCall) -> bool {
        Self::command(call)
            && self
                .0
                .lock()
                .expect("session approvals poisoned")
                .contains(&call.authority_fingerprint())
    }

    pub(super) fn record(&self, call: &PreparedCall) {
        if Self::command(call) {
            self.0
                .lock()
                .expect("session approvals poisoned")
                .insert(call.authority_fingerprint());
        }
    }
}
