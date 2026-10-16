//! Shared, highest-precedence-first filesystem roots for skills and commands.

use std::path::{Path, PathBuf};

use ego_core::event::skill::SkillSourceAuthority;

#[derive(Clone, Debug)]
pub(crate) struct CapabilityRoot {
    pub(crate) source: SkillSourceAuthority,
    pub(crate) path: PathBuf,
}

/// Project roots are never inspected without trust. Walk only as far as the
/// nearest repository marker (a directory or worktree `.git` file).
/// Precedence: nearest project first; ego, agents, claude within each scope;
/// then user ego, agents, claude; finally the system root.
pub(crate) fn discover_capability_roots(
    workspace: &Path,
    trusted_project: bool,
    system: &Path,
    ego_home: &Path,
    home: Option<&Path>,
    leaf: &str,
) -> Vec<CapabilityRoot> {
    let mut roots = Vec::new();
    if trusted_project {
        let boundary = workspace
            .ancestors()
            .find(|path| path.join(".git").exists());
        // A non-repository workspace has no authority over its ancestors.
        for directory in workspace.ancestors() {
            for namespace in [".ego", ".agents", ".claude"] {
                roots.push(CapabilityRoot {
                    source: SkillSourceAuthority::Project,
                    path: directory.join(namespace).join(leaf),
                });
            }
            if boundary.is_none() || Some(directory) == boundary {
                break;
            }
        }
    }
    roots.push(CapabilityRoot {
        source: SkillSourceAuthority::User,
        path: ego_home.join(leaf),
    });
    if let Some(home) = home {
        for namespace in [".agents", ".claude"] {
            roots.push(CapabilityRoot {
                source: SkillSourceAuthority::User,
                path: home.join(namespace).join(leaf),
            });
        }
    }
    roots.push(CapabilityRoot {
        source: SkillSourceAuthority::System,
        path: system.join(leaf),
    });
    roots
}
