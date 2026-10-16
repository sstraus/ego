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
    host_profile: bool,
}

impl CliSessionTrustAdmission {
    pub fn new(home: Option<PathBuf>) -> Self {
        Self {
            home,
            host_profile: false,
        }
    }
    /// Enable attestations only after the launch profile has been resolved.
    pub fn with_host_profile(mut self) -> Self {
        self.host_profile = true;
        self
    }
}

impl SessionTrustAdmission for CliSessionTrustAdmission {
    fn available_commands(
        &self,
        root: &RunRoot,
    ) -> Vec<ego_core::event::skill::SkillDescriptorEvidence> {
        let trusted = matches!(
            self.status(root),
            SessionTrustStatus::Remembered(Trust::Trusted)
        );
        let Some(discovery) = crate::skill::SkillDiscovery::for_workspace(root.as_path(), trusted)
        else {
            return Vec::new();
        };
        let actions = crate::repl::actions::ActionRegistry::final_surface();
        let fixed = actions
            .names()
            .into_iter()
            .chain(actions.aliases())
            .collect::<Vec<_>>();
        let Ok(catalogue) = crate::skill::SkillCatalogue::discover(discovery, &fixed) else {
            return Vec::new();
        };
        let Ok(capture) = catalogue.capture() else {
            return Vec::new();
        };
        capture
            .descriptors()
            .iter()
            .filter(|spec| spec.descriptor().user_invocable)
            .map(|spec| spec.descriptor().clone())
            .collect()
    }
    fn attest_trusted(&self, root: &RunRoot) {
        if self.host_profile {
            let surface = crate::trust::executable_surface(root.as_path());
            if !surface.external_instructions.is_empty() {
                return; // Generic host trust is not external disclosure consent.
            }
            if !self.record_answer(root, &surface.fingerprint(), Trust::Trusted) {
                tracing::warn!("the host workspace trust attestation could not be recorded");
            }
        }
    }

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
            Some(changes) => {
                let mut disclosure = String::new();
                for (path, digest) in &surface.external_instructions {
                    disclosure.push_str(&format!("- {path:?} SHA-256 {digest}\n"));
                }
                format!(
                    "Authority changes since the last answer:\n{changes}\n\
                         External instructions offered for disclosure to the model:\n{disclosure}"
                )
            }
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
