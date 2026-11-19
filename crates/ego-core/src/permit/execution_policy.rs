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

    /// Recognize only exact roots approved by the terminal in this session.
    /// Every other authority dimension still uses the normal readmission proof.
    pub fn not_more_permissive_than_with_roots(
        &self,
        old: &Self,
        approved: &[ProfileRoot],
    ) -> bool {
        let mut previous = old.clone();
        for root in approved {
            if !previous.roots.contains(root) {
                previous.roots.push(root.clone());
            }
        }
        self.not_more_permissive_than(&previous)
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
                    && old
                        .roots
                        .iter()
                        .filter(|root| root.restricts_writes(&old.workspace, &old.roots))
                        .all(|previous| {
                            self.roots.iter().any(|root| {
                                root.restricts_writes(&self.workspace, &self.roots)
                                    && previous.path().is_or_under(root.path())
                            })
                        })
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
        path_changes(
            &mut changes,
            "filesystem root",
            old.roots
                .iter()
                .map(|root| format!("{} ({})", root.path(), root.access().as_str())),
            self.roots
                .iter()
                .map(|root| format!("{} ({})", root.path(), root.access().as_str())),
        );
        path_changes(
            &mut changes,
            "exec_paths",
            old.exec_paths.iter().map(ToString::to_string),
            self.exec_paths.iter().map(ToString::to_string),
        );
        path_changes(
            &mut changes,
            "runtime root",
            old.runtime_roots.iter().map(ToString::to_string),
            self.runtime_roots.iter().map(ToString::to_string),
        );
        if self.adaptive != old.adaptive {
            match (&old.adaptive, &self.adaptive) {
                (Some(previous), Some(current)) => {
                    if previous.home != current.home {
                        changes.push(format!(
                            "adaptive home {} -> {}",
                            previous.home, current.home
                        ));
                    }
                    for (label, before, after) in [
                        ("derived write root", &previous.writable, &current.writable),
                        (
                            "credential floor",
                            &previous.credentials,
                            &current.credentials,
                        ),
                        ("control floor", &previous.controls, &current.controls),
                    ] {
                        path_changes(
                            &mut changes,
                            label,
                            before.iter().map(ToString::to_string),
                            after.iter().map(ToString::to_string),
                        );
                    }
                }
                _ => changes.push(format!(
                    "adaptive derivation {} -> {}",
                    old.adaptive.is_some(),
                    self.adaptive.is_some()
                )),
            }
        }
        changes
    }
}

fn covered(path: &CanonPath, roots: &[CanonPath]) -> bool {
    roots
        .iter()
        .any(|root| path.as_path().starts_with(root.as_path()))
}

fn path_changes(
    changes: &mut Vec<String>,
    label: &str,
    before: impl Iterator<Item = String>,
    after: impl Iterator<Item = String>,
) {
    let before = before.collect::<std::collections::BTreeSet<_>>();
    let after = after.collect::<std::collections::BTreeSet<_>>();
    for (action, paths) in [
        ("removed", before.difference(&after).collect::<Vec<_>>()),
        ("added", after.difference(&before).collect::<Vec<_>>()),
    ] {
        for path in paths.iter().take(8) {
            changes.push(format!("{label} {action}: {}", path.escape_debug()));
        }
        if paths.len() > 8 {
            changes.push(format!(
                "{label} {action}: {} more paths (inspect /config perimeter)",
                paths.len() - 8
            ));
        }
    }
}
