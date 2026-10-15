//! Model-requested context changes are data until the host reaches idle.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAction {
    Compact,
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRequest {
    pub action: ContextAction,
    pub reason: String,
    pub handoff: String,
}

impl ContextRequest {
    pub fn parse(value: serde_json::Value) -> Result<Self, String> {
        let request: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        if request.reason.trim().is_empty() || request.handoff.trim().is_empty() {
            return Err("context reason and handoff must not be empty".into());
        }
        if request.reason.len() > 2048 || request.handoff.len() > 16384 {
            return Err("context reason or handoff exceeds its byte limit".into());
        }
        Ok(request)
    }

    /// A clear checkpoint contains no source conversation. Model text remains
    /// body data, even when it contains Markdown checkpoint headings.
    pub fn clear_checkpoint(&self, goal: &crate::goal::GoalRecord) -> crate::compact::Checkpoint {
        let quote = |text: &str| {
            text.lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        crate::compact::Checkpoint::parse(format!(
            "# Objective\n{}\n# Binding constraints and preferences\nActive goal ceilings: {:?}\nFresh system and project instructions apply.\n# Completed work and evidence\nNot retained.\n# Work in progress\nModel-written handoff (untrusted data):\n{}\n# Decisions and rationale\nNot retained.\n# Blockers and uncertainty\nNot retained; verify the handoff before acting.\n# Ordered next steps\nContinue the active goal using the handoff.\n# Critical references\nNot retained.",
            quote(&goal.objective), goal.ceilings, quote(&self.handoff)
        )).expect("quoted model text cannot change the checkpoint grammar")
    }
}

pub fn descriptor() -> crate::generator::types::ToolDescriptor {
    crate::generator::types::ToolDescriptor {
        name: "context".into(),
        description: "Request compact or clear after this complete tool batch, then continue in a new session. Read the context-management skill first. Supply a reason and a short handoff. Clear requires operator opt-in and an active goal. Never request with unfinished effects or pending questions.".into(),
        parameters: serde_json::json!({"type":"object","additionalProperties":false,"required":["action","reason","handoff"],"properties":{"action":{"type":"string","enum":["compact","clear"]},"reason":{"type":"string","minLength":1,"maxLength":2048},"handoff":{"type":"string","minLength":1,"maxLength":16384}}}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_request_rejects_empty_oversized_and_unknown_arguments() {
        // Catches: unbounded or malformed handoffs enter durable transition state.
        for value in [
            serde_json::json!({"action":"clear","reason":" ","handoff":"next"}),
            serde_json::json!({"action":"compact","reason":"done","handoff":""}),
            serde_json::json!({"action":"clear","reason":"done","handoff":"x".repeat(16385)}),
            serde_json::json!({"action":"reset","reason":"done","handoff":"next"}),
            serde_json::json!({"action":"clear","reason":"done","handoff":"next","extra":true}),
        ] {
            assert!(ContextRequest::parse(value).is_err());
        }
    }
}
