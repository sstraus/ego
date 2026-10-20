//! Optional mdkb context through the run's guarded process effects.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::hook::EffectScope;
use ego_core::permit::{
    CanonPath, Classification, EnvPolicy, Op, PreparedCall, ProcessExecutor, Resolution, SpawnSpec,
    StreamProvenance, ToolOutputProvenance,
};
use ego_core::turn::{GenerationPolicySource, PromptContextSource, ToolEffectPorts};

const RECALL_TIMEOUT: Duration = Duration::from_millis(1500);
const WARMUP_TIMEOUT: Duration = Duration::from_millis(2500);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub(crate) struct MdkbPromptContext<'a> {
    binary: PathBuf,
    policy: Arc<dyn GenerationPolicySource>,
    effects: ToolEffectPorts<'a>,
    process: &'a dyn ProcessExecutor,
}

impl<'a> MdkbPromptContext<'a> {
    pub(crate) fn new(
        binary: PathBuf,
        policy: Arc<dyn GenerationPolicySource>,
        effects: ToolEffectPorts<'a>,
        process: &'a dyn ProcessExecutor,
    ) -> Self {
        Self {
            binary,
            policy,
            effects,
            process,
        }
    }

    async fn invoke(
        &self,
        event: &str,
        body: serde_json::Value,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        if std::env::var_os("MDKB_DISTILLER_ACTIVE").is_some() {
            return Ok(None);
        }
        let command = match event {
            "SessionStart" => "session-start",
            "UserPromptSubmit" => "user-prompt-submit",
            "PostToolUse" | "PostToolUseFailure" => "post-tool-use",
            _ => return Err("unsupported mdkb context event".into()),
        };
        let spec = SpawnSpec::new(
            vec![
                OsString::from(&self.binary),
                OsString::from("hook"),
                OsString::from(command),
            ],
            Some(CanonPath::new(cwd)),
            Some(timeout),
            EnvPolicy::Inherited,
        )
        .map_err(|error| error.to_string())?;
        let effect_name = match event {
            "SessionStart" => "mdkb_session_warmup",
            "PostToolUse" | "PostToolUseFailure" => "mdkb_tool_observation",
            _ => "mdkb_prompt_recall",
        };
        let prepared = PreparedCall::new(effect_name, Op::Spawn { spec })
            .observe_processes(self.process)
            .map_err(|error| error.to_string())?;
        let policy = self.policy.current()?;
        let authorized = match policy.classify(prepared) {
            Classification::Settled(Ok(authorized)) => authorized,
            Classification::Ask(pending) => match policy.settle(pending).await {
                Resolution::Settled(Ok(authorized)) => authorized,
                _ => return Err("mdkb context process requires permission or was denied".into()),
            },
            Classification::Settled(Err(error)) => return Err(error.to_string()),
        };
        let (prepared, effects) = self.effects.clone().bind(authorized);
        let spec = prepared
            .ops()
            .iter()
            .find_map(|operation| match operation {
                Op::Spawn { spec } => Some(spec),
                _ => None,
            })
            .ok_or("mdkb context has no spawn authority")?;
        let input = serde_json::to_vec(&body).map_err(|error| error.to_string())?;
        if input.len() > MAX_RESPONSE_BYTES {
            return Err("mdkb context request exceeds its byte limit".into());
        }
        let result = effects
            .exec_handler(spec, &input, EffectScope::new(timeout))
            .await
            .map_err(|error| error.to_string())?;
        let stdout = result.content.iter().find(|block| {
            matches!(
                block.provenance(),
                ToolOutputProvenance::Process {
                    stream: StreamProvenance::Stdout,
                    ..
                }
            )
        });
        let Some(stdout) = stdout else {
            return Ok(None);
        };
        if stdout.preview_is_head_and_tail()
            || stdout
                .evidence_reference()
                .is_some_and(|reference| reference.bytes > MAX_RESPONSE_BYTES as u64)
            || stdout.text.len() > MAX_RESPONSE_BYTES
        {
            return Err("mdkb context response exceeds 64 KiB".into());
        }
        parse_context(stdout.text.as_ref(), event)
    }
}

impl PromptContextSource for MdkbPromptContext<'_> {
    fn warmup<'a>(
        &'a self,
        cwd: &'a Path,
        session: SessionId,
    ) -> futures::future::BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            self.invoke(
                "SessionStart",
                serde_json::json!({
                    "hook_event_name": "SessionStart",
                    "cwd": cwd,
                    "session_id": session.to_string(),
                }),
                cwd,
                WARMUP_TIMEOUT,
            )
            .await
        })
    }

    fn recall<'a>(
        &'a self,
        prompt: &'a str,
        cwd: &'a Path,
        session: SessionId,
    ) -> futures::future::BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            self.invoke(
                "UserPromptSubmit",
                serde_json::json!({
                    "hook_event_name": "UserPromptSubmit",
                    "prompt": prompt,
                    "cwd": cwd,
                    "session_id": session.to_string(),
                }),
                cwd,
                RECALL_TIMEOUT,
            )
            .await
        })
    }

    fn tool<'a>(
        &'a self,
        cwd: &'a Path,
        session: SessionId,
        name: &'a str,
        args: &'a str,
        ok: bool,
        result: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let (name, input) = mdkb_tool_input(name, args)?;
            let common = serde_json::json!({
                "cwd": cwd,
                "session_id": session.to_string(),
                "tool_name": name,
                "tool_input": input,
                "tool_response": result,
            });
            let post_event = if ok {
                "PostToolUse"
            } else {
                "PostToolUseFailure"
            };
            let mut post = common;
            post["hook_event_name"] = serde_json::json!(post_event);
            if !ok {
                post["error"] = serde_json::json!(result);
            }
            self.invoke(post_event, post, cwd, Duration::from_millis(1500))
                .await
        })
    }
}

/// Preserve the original input while giving mdkb its existing file-path key.
fn mdkb_tool_input<'a>(name: &'a str, args: &str) -> Result<(&'a str, serde_json::Value), String> {
    let mut input: serde_json::Value =
        serde_json::from_str(args).map_err(|_| "invalid settled tool arguments")?;
    let name = match name {
        "grep" => "Grep",
        "read" => "Read",
        "edit" => "Edit",
        "write" => "Write",
        other => other,
    };
    if name == "Grep"
        && input.get("pattern").is_none()
        && let Some(query) = input
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    {
        input["pattern"] = serde_json::json!(query);
    }
    if matches!(name, "Edit" | "Write")
        && input.get("file_path").is_none()
        && let Some(path) = input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    {
        input["file_path"] = serde_json::json!(path);
    }
    Ok((name, input))
}

fn parse_context(stdout: &str, event: &str) -> Result<Option<String>, String> {
    if stdout.trim().is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|_| "invalid mdkb context response")?;
    if value == serde_json::json!({}) {
        return Ok(None);
    }
    match (
        value.pointer("/hookSpecificOutput/hookEventName"),
        value.pointer("/hookSpecificOutput/additionalContext"),
    ) {
        (Some(serde_json::Value::String(name)), Some(serde_json::Value::String(context)))
            if name == event =>
        {
            Ok(Some(context.clone()))
        }
        _ => Err("invalid mdkb context response shape".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{mdkb_tool_input, parse_context};

    // Catches: ego uses `query`, but mdkb suggests code lookup from `pattern`.
    #[test]
    fn grep_hint_receives_the_original_search_pattern() {
        let (name, input) =
            mdkb_tool_input("grep", r#"{"query":"fn parse","path":"src"}"#).unwrap();
        assert_eq!(name, "Grep");
        assert_eq!(input["pattern"], "fn parse");
        assert_eq!(input["path"], "src");
    }

    // Catches: ego uses `path`, but mdkb reindexes only `file_path` on edits.
    #[test]
    fn edit_notification_carries_mdkb_file_path_without_losing_input() {
        let (name, input) = mdkb_tool_input(
            "edit",
            r#"{"path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
        )
        .unwrap();
        assert_eq!(name, "Edit");
        assert_eq!(input["file_path"], "src/lib.rs");
        assert_eq!(input["old_string"], "a");
        assert_eq!(input["new_string"], "b");
    }
    #[test]
    fn accepts_only_matching_context_events() {
        assert_eq!(parse_context("{}", "SessionStart").unwrap(), None);
        assert_eq!(
            parse_context(r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"warmup"}}"#, "SessionStart").unwrap(),
            Some("warmup".into())
        );
        assert_eq!(
            parse_context(r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"verified fact"}}"#, "UserPromptSubmit").unwrap(),
            Some("verified fact".into())
        );
        assert!(parse_context(r#"{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"wrong event"}}"#, "SessionStart").is_err());
    }
}
