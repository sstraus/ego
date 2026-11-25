use super::{ExecutableSurface, Trust, TrustLedger};

impl ExecutableSurface {
    /// Only bytes and executable bits of already inventoried programs may change.
    pub(super) fn development_edit_of(&self, old: &Self) -> bool {
        self.declarations == old.declarations
            && self.external_instructions == old.external_instructions
            && self.programs.len() == old.programs.len()
            && self.programs.iter().all(|(path, current)| {
                old.programs.get(path).is_some_and(|prior| {
                    if std::path::Path::new(path).is_absolute() {
                        // Command Markdown contains executable shell fragments, not named programs.
                        current == prior
                    } else {
                        current.link_target == prior.link_target
                            && current.resolved_target == prior.resolved_target
                    }
                })
            })
    }
}

impl TrustLedger {
    /// Approve the current workspace and edits to its current named programs.
    pub fn remember_development(&mut self, root: &str, surface: ExecutableSurface) {
        self.remember(root, surface, Trust::Trusted);
        if let Some(record) = self.workspaces.get_mut(root) {
            record.development_programs = true;
        }
    }

    /// Describe edits covered by a standing development approval.
    pub fn development_changes(&self, root: &str, surface: &ExecutableSurface) -> Option<String> {
        let record = self.workspaces.get(root)?;
        let old = record.executable_surface.as_ref()?;
        (record.development_programs
            && record.decision == Trust::Trusted
            && old != surface
            && surface.development_edit_of(old))
        .then(|| surface.changes_from(old))
    }
}
