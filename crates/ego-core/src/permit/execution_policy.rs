//! Semantic execution authority retained beside its versioned digest.

use serde::{Deserialize, Serialize};

use super::{AdaptivePerimeter, CanonPath, Confinement, ProfileRoot, RootAccess, RunConfinement};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicy {
    level: Confinement,
    offline: bool,
    workspace: CanonPath,
    roots: Vec<ProfileRoot>,
    exec_paths: Vec<CanonPath>,
    runtime_roots: Vec<CanonPath>,
    adaptive: Option<AdaptivePerimeter>,
}

impl ExecutionPolicy {
    pub fn capture(policy: &RunConfinement, runtime_roots: Vec<CanonPath>) -> Self {
        let confined = policy.level() != Confinement::Off;
        Self {
            level: policy.level(),
            offline: policy.offline(),
            workspace: policy.workspace().clone(),
            roots: if confined {
                policy.roots().to_vec()
            } else {
                Vec::new()
            },
            exec_paths: if confined {
                policy.exec_paths().to_vec()
            } else {
                Vec::new()
            },
            runtime_roots,
            adaptive: if confined {
                policy.adaptive_perimeter().cloned()
            } else {
                None
            },
        }
    }

    /// Conservative proof: an unrecognized change requires the human seat.
    pub fn not_more_permissive_than(&self, old: &Self) -> bool {
        self.level.within(old.level)
            && (!old.offline || self.offline)
            && self.workspace == old.workspace
            && (old.level == Confinement::Off
                || (self.adaptive == old.adaptive
                    && self
                        .exec_paths
                        .iter()
                        .all(|path| covered(path, &old.exec_paths))
                    && self
                        .runtime_roots
                        .iter()
                        .all(|path| covered(path, &old.runtime_roots))
                    && self.roots.iter().all(|root| {
                        old.roots.iter().any(|previous| {
                            root.path().as_path().starts_with(previous.path().as_path())
                                && (root.access() == RootAccess::Read
                                    || previous.access() == RootAccess::ReadWrite)
                        })
                    })))
    }

    pub fn changes_from(&self, old: &Self) -> Vec<String> {
        let mut changes = Vec::new();
        if self.level != old.level {
            changes.push(format!("confinement {} -> {}", old.level, self.level));
        }
        if self.offline != old.offline {
            changes.push(format!(
                "process network access offline={} -> offline={}",
                old.offline, self.offline
            ));
        }
        if self.workspace != old.workspace {
            changes.push(format!("workspace {} -> {}", old.workspace, self.workspace));
        }
        if self.roots != old.roots {
            changes.push(format!(
                "filesystem roots/access {:?} -> {:?}",
                old.roots, self.roots
            ));
        }
        if self.exec_paths != old.exec_paths {
            changes.push(format!(
                "executable paths {:?} -> {:?}",
                old.exec_paths, self.exec_paths
            ));
        }
        if self.runtime_roots != old.runtime_roots {
            changes.push(format!(
                "runtime roots {:?} -> {:?}",
                old.runtime_roots, self.runtime_roots
            ));
        }
        if self.adaptive != old.adaptive {
            changes.push(format!(
                "adaptive perimeter {:?} -> {:?}",
                old.adaptive, self.adaptive
            ));
        }
        changes
    }
}

fn covered(path: &CanonPath, roots: &[CanonPath]) -> bool {
    roots
        .iter()
        .any(|root| path.as_path().starts_with(root.as_path()))
}
