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
        }
    }
}
