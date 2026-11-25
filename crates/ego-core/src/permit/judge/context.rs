//! Session-state projection shared by action and residual-write opinions.

use super::JudgeTaskContext;
use crate::event::id::TurnId;
use crate::state::{ItemKind, SessionState};

impl JudgeTaskContext {
    /// Capture the latest task and current-turn reason without tool output.
    pub fn from_state(state: &SessionState, turn: TurnId, roots: Vec<String>) -> Self {
        let mut items: Vec<_> = state.items.values().collect();
        items.sort_by_key(|item| item.seq);
        let latest = items
            .iter()
            .rev()
            .find(|item| item.kind == ItemKind::UserPrompt);
        let prompts: Vec<_> = items
            .iter()
            .filter(|item| item.turn == turn && item.kind == ItemKind::UserPrompt)
            .collect();
        let task = if prompts.len() > 1 {
            let mut task = String::from(
                "User prompts in chronological order; later prompts take precedence:\n",
            );
            for (index, prompt) in prompts.iter().enumerate() {
                if index + 1 == prompts.len() {
                    task.push_str("Latest user prompt (takes precedence):\n");
                } else {
                    task.push_str("Earlier user prompt:\n");
                }
                task.push_str(&prompt.text);
                task.push('\n');
            }
            task
        } else {
            prompts
                .first()
                .copied()
                .or(latest)
                .map_or(String::new(), |item| item.text.clone())
        };
        let agent_reason = items
            .iter()
            .rev()
            .find(|item| item.turn == turn && item.kind == ItemKind::Assistant)
            .map_or(String::new(), |item| item.text.clone());
        let recent_calls = items
            .iter()
            .rev()
            .filter(|item| item.kind == ItemKind::ToolCall && item.completed)
            .take(3)
            .map(|item| {
                let structure = serde_json::from_str::<serde_json::Value>(&item.args)
                    .ok()
                    .and_then(|args| {
                        args.get("argv")
                            .and_then(serde_json::Value::as_array)
                            .map(|argv| {
                                let argv: Vec<_> = argv
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .map(str::to_owned)
                                    .collect();
                                crate::permit::shell::observe(&argv).skeleton
                            })
                    })
                    .unwrap_or_else(|| "<non-exec-action>".into());
                super::JudgeRecentCall {
                    tool: item.tool_name.clone().unwrap_or_default(),
                    structure,
                    ok: item.ok,
                }
            })
            .collect();
        Self {
            task,
            agent_reason,
            recent_calls,
            roots,
            repository: Vec::new(),
            precedents: Vec::new(),
        }
    }

    /// Add what the judge needs to recognise ordinary work in this repository.
    pub fn with_development(
        mut self,
        workspace: &std::path::Path,
        precedents: Vec<String>,
    ) -> Self {
        self.repository = repository_profile(workspace);
        self.precedents = precedents;
        self
    }
}

const BUILD_MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "Rust (cargo)"),
    (
        "package.json",
        "JavaScript or TypeScript (npm-compatible scripts)",
    ),
    ("pnpm-lock.yaml", "pnpm"),
    ("yarn.lock", "yarn"),
    ("go.mod", "Go"),
    ("Package.swift", "Swift package"),
    ("pyproject.toml", "Python"),
    ("Makefile", "make"),
    ("justfile", "just"),
    ("CMakeLists.txt", "CMake"),
];

/// Detected stacks, same-repository worktrees and the project instruction
/// summary. Reads only names and the head of one instruction file.
pub(crate) fn repository_profile(workspace: &std::path::Path) -> Vec<String> {
    use std::io::Read as _;
    let mut lines = Vec::new();
    let tools: Vec<&str> = BUILD_MARKERS
        .iter()
        .filter(|(file, _)| workspace.join(file).exists())
        .map(|(_, tool)| *tool)
        .collect();
    if !tools.is_empty() {
        lines.push(format!("stacks and build tools: {}", tools.join(", ")));
    }
    let git = workspace.join(".git");
    let common = if git.is_file() {
        std::fs::read_to_string(&git)
            .ok()
            .and_then(|text| {
                text.strip_prefix("gitdir:")
                    .map(|dir| dir.trim().to_owned())
            })
            .and_then(|dir| {
                std::path::Path::new(&dir)
                    .ancestors()
                    .find(|path| path.file_name().is_some_and(|name| name == "worktrees"))
                    .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            })
    } else {
        Some(git)
    };
    if let Some(common) = common {
        let mut worktrees = vec![
            common
                .parent()
                .map_or_else(String::new, |main| main.display().to_string()),
        ];
        if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
            for entry in entries.flatten().take(32) {
                if let Ok(text) = std::fs::read_to_string(entry.path().join("gitdir"))
                    && let Some(dir) = std::path::Path::new(text.trim()).parent()
                {
                    worktrees.push(dir.display().to_string());
                }
            }
        }
        worktrees.retain(|path| !path.is_empty());
        lines.push(format!(
            "worktrees of this repository (one workspace): {}",
            worktrees.join(", ")
        ));
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let mut head = String::new();
        if let Ok(file) = std::fs::File::open(workspace.join(name))
            && file.take(2048).read_to_string(&mut head).is_ok()
        {
            lines.push(format!("project instructions ({name}, start): {head}"));
            break;
        }
    }
    lines
}
