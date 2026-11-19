//! Caller selections for a session's stable initialization prompt.

/// How a caller changes the launch prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SystemPromptMode {
    /// Extend the launch prompt.
    Append,
    /// Replace the launch prompt.
    Replace,
}

/// A caller's explicit prompt text, with CLI-compatible `@file` syntax.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSystemPrompt {
    /// Append to or replace the launch prompt.
    pub mode: SystemPromptMode,
    /// Prompt text or a workspace-relative `@file` reference.
    pub text: String,
}

/// Immutable prompt selections, carried with the durable run request.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionPromptOptions {
    /// The session's prompt change, if supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<SessionSystemPrompt>,
    /// Disable ambient discovery. Absence preserves the launch selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bare: Option<bool>,
}

/// Refuse contradictory explicit instruction selections on every surface.
pub fn validate_prompt_options(bare: bool, named_instructions: bool) -> Result<(), &'static str> {
    if bare && named_instructions {
        Err(
            "--bare and --instructions contradict each other: --bare turns ambient discovery off, and --instructions names a file to use instead. Drop --bare — an explicitly named file is not ambient",
        )
    } else {
        Ok(())
    }
}
