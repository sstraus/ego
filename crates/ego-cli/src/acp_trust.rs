//! What one ACP session's workspace offers to be trusted with.
//!
//! The seat that asks lives in `ego-acp`, because asking is a protocol act.
//! Which files the answer is *about* lives here, because the trust ADR gives
//! `ego-cli` "which paths are probed, the reads that build the inventory".
//!
//! The adapter uses the same per-root ledger as the terminal seat.

use std::path::PathBuf;

use ego_acp::serve::{SessionTrustAdmission, SessionTrustStatus};
use ego_core::config::Trust;
use ego_core::run::RunRoot;

/// The CLI-owned lister behind one ACP connection's trust question.
#[derive(Debug, Default)]
pub struct CliSessionTrustAdmission {
    home: Option<PathBuf>,
}

impl CliSessionTrustAdmission {
    pub fn new(home: Option<PathBuf>) -> Self {
        Self { home }
    }
}

impl SessionTrustAdmission for CliSessionTrustAdmission {
    fn status(&self, root: &RunRoot) -> SessionTrustStatus {
        let path = root.as_path();
        let Some(content) = crate::trust::trustable_content(path) else {
            return SessionTrustStatus::Empty;
        };
        let home = self.home.as_deref();
        let surface = crate::trust::executable_surface(path);
        if let Some(decision) = crate::trust::remembered_decision(path, home, &surface) {
            return SessionTrustStatus::Remembered(decision);
        }
        let content = match crate::trust::executable_changes(path, home, &surface) {
            Some(changes) => format!("Executable changes since the last answer:\n{changes}\n"),
            None => content,
        };
        SessionTrustStatus::Question {
            content,
            fingerprint: surface.fingerprint(),
        }
    }

    fn record_answer(&self, root: &RunRoot, fingerprint: &str, decision: Trust) -> bool {
        crate::trust::record_answer(root.as_path(), self.home.as_deref(), fingerprint, decision)
    }
}
