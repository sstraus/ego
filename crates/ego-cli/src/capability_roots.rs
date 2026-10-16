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
            for namespace in if leaf == "commands" {
                &[".ego", ".agents", ".claude", ".opencode"][..]
            } else {
                &[".ego", ".agents", ".claude", ".codex"][..]
            } {
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
    if leaf == "commands"
        && let Some(home) = home
    {
        for namespace in [".opencode", ".config/opencode"] {
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

/// Add filesystem-backed foreign skill roots without importing foreign policy.
/// Plugin caches are intentionally not roots: installation does not mean enabled.
pub(crate) fn register_external_skill_roots(
    roots: &mut Vec<CapabilityRoot>,
    codex_home: Option<&Path>,
    claude_config: Option<&Path>,
    claude_managed: &Path,
    codex_managed: &Path,
) {
    let insertion = roots
        .iter()
        .position(|root| root.source == SkillSourceAuthority::System)
        .unwrap_or(roots.len());
    let mut external = Vec::new();
    if let Some(base) = codex_home {
        for suffix in ["skills", "skills/.system"] {
            external.push(CapabilityRoot {
                // Both caches are writable by the user, not machine policy.
                source: SkillSourceAuthority::User,
                path: base.join(suffix),
            });
        }
    }
    if let Some(base) = claude_config {
        external.push(CapabilityRoot {
            source: SkillSourceAuthority::User,
            path: base.join("skills"),
        });
    }
    for path in [
        claude_managed.join(".claude/skills"),
        codex_managed.join("skills"),
    ] {
        external.push(CapabilityRoot {
            source: SkillSourceAuthority::System,
            path,
        });
    }
    external.retain(|candidate| !roots.iter().any(|root| root.path == candidate.path));
    roots.splice(insertion..insertion, external);
}

pub(crate) fn claude_managed_directory() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    }
}

pub(crate) fn codex_managed_directory() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\ProgramData\OpenAI\Codex")
    } else {
        PathBuf::from("/etc/codex")
    }
}

/// No cache scan can reproduce foreign enablement, account sync or host context.
/// Doctor reports these unsupported source classes even when no cache exists.
pub(crate) const UNSUPPORTED_SKILL_SOURCES: &[(&str, &str)] = &[
    (
        "skills.plugins.claude",
        "Claude Code plugin skills are unsupported: enabled plugin roots, manifest skills paths, root SKILL.md and --plugin-dir require foreign enablement and namespace policy; installed caches are not admitted",
    ),
    (
        "skills.plugins.codex",
        "Codex plugin skills are unsupported: configured, bundled and remote plugin roots require foreign enablement and namespace policy; installed caches are not admitted",
    ),
    (
        "skills.dynamic",
        "Dynamic skill sources are unsupported: Claude additional-directory, worktree-origin and claude.ai synced skills; Codex remote environment roots and custom config-layer roots; install a reviewed standalone skill in an ego skill root",
    ),
];
