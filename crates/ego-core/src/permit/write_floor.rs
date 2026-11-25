//! Paths no outside-workspace write question reaches, whatever the mode.
//!
//! The exec sandbox refuses the same names for its own writes
//! (`ego-tools/src/exec/adaptive_floor.rs`), so a file tool never writes what
//! a command of the same run could not. The Git control names below are the one
//! list both floors derive from.

use super::CanonPath;
use std::path::Component;

/// Git control entries: they run or steer code the next time Git runs.
pub const GIT_CONTROLS: [&str; 6] = [
    "config",
    "config.worktree",
    "hooks",
    "commondir",
    "gitdir",
    "info/attributes",
];

/// True for a Git control entry, an agent instruction file or an operator
/// instruction file, wherever it sits. Names match case-insensitively: the
/// usual macOS filesystem treats case variants as one file.
pub(crate) fn is_control_path(path: &CanonPath) -> bool {
    let parts: Vec<String> = path
        .as_path()
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let named = |name: &str| parts.iter().any(|part| part == name);
    if named("agents.md") || named("claude.md") {
        return true;
    }
    let operator = parts
        .iter()
        .position(|part| matches!(part.as_str(), ".codex" | ".claude" | ".claude-private"));
    if operator.is_some_and(|at| parts[at + 1..].iter().any(|part| part.ends_with(".md"))) {
        return true;
    }
    let Some(git) = parts.iter().position(|part| part == ".git") else {
        return false;
    };
    let rest = &parts[git + 1..];
    if rest.is_empty() {
        // A `.git` file selects the repository; a directory is a new checkout.
        return !path.as_path().is_dir();
    }
    (0..rest.len()).any(|at| {
        let prefix_ok = match rest[..at].first().map(String::as_str) {
            None => true,
            Some("modules") => true,
            Some("worktrees") => at == 2,
            Some(_) => false,
        };
        prefix_ok
            && GIT_CONTROLS.iter().any(|control| {
                let names: Vec<&str> = control.split('/').collect();
                rest.get(at..at + names.len())
                    .is_some_and(|window| window.iter().map(String::as_str).eq(names))
            })
    })
}
