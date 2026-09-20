//! What one ACP session's workspace offers to be trusted with.
//!
//! The seat that asks lives in `ego-acp`, because asking is a protocol act.
//! Which files the answer is *about* lives here, because the trust ADR gives
//! `ego-cli` "which paths are probed, the reads that build the inventory".
//!
//! This adapter reads and nothing else. It consults no remembered answer and
//! writes none: an answer given to a host governs that session, and a `y`
//! typed once at a terminal is not a posture another program's session gets
//! to inherit unasked.

use ego_acp::serve::SessionTrustAdmission;
use ego_core::run::RunRoot;

/// The CLI-owned lister behind one ACP connection's trust question.
#[derive(Debug, Default)]
pub struct CliSessionTrustAdmission;

impl SessionTrustAdmission for CliSessionTrustAdmission {
    fn trustable(&self, root: &RunRoot) -> Option<String> {
        crate::trust::trustable_content(root.as_path())
    }
}
