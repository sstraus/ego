//! Optional mdkb context through the run's guarded process effects.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ego_core::event::id::{EventId, SessionId};
use ego_core::hook::EffectScope;
use ego_core::permit::{
    CanonPath, Classification, EnvPolicy, Op, PreparedCall, SpawnSpec, StreamProvenance,
    ToolOutputProvenance,
};
use ego_core::turn::{EpisodeItem, GenerationPolicySource, PromptContextSource, ToolEffectPorts};

const RECALL_TIMEOUT: Duration = Duration::from_millis(1500);
const WARMUP_TIMEOUT: Duration = Duration::from_millis(2500);
const STOP_TIMEOUT: Duration = Duration::from_millis(2500);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_EPISODE_BYTES: usize = 256 * 1024;

pub(crate) struct MdkbPromptContext<'a> {
    binary: PathBuf,
    policy: Arc<dyn GenerationPolicySource>,
    effects: ToolEffectPorts<'a>,
}

impl<'a> MdkbPromptContext<'a> {
    pub(crate) fn new(
        binary: PathBuf,
        policy: Arc<dyn GenerationPolicySource>,
        effects: ToolEffectPorts<'a>,
    ) -> Self {
        Self {
            binary,
            policy,
            effects,
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
            "PreToolUse" => "pre-tool-use",
            "PostToolUse" | "PostToolUseFailure" => "post-tool-use",
            "EgoStop" => "ego-stop",
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
            "EgoStop" => "mdkb_episode_stop",
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => "mdkb_tool_observation",
            _ => "mdkb_prompt_recall",
        };
        let prepared = PreparedCall::new(effect_name, Op::Spawn { spec });
        let policy = self.policy.current()?;
        let authorized = match policy.classify(prepared) {
            Classification::Settled(Ok(authorized)) => authorized,
            Classification::Settled(Err(_)) | Classification::Ask(_) => {
                return Err("mdkb context process is not authorized".into());
            }
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
        let max_request = if event == "EgoStop" {
            MAX_EPISODE_BYTES + 1024
        } else {
            MAX_RESPONSE_BYTES
        };
        if input.len() > max_request {
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
            let mut context = Vec::new();
            let common = serde_json::json!({
                "cwd": cwd,
                "session_id": session.to_string(),
                "tool_name": name,
                "tool_input": input,
            });
            if let Ok(Some(hint)) = self
                .invoke(
                    "PreToolUse",
                    serde_json::json!({
                        "hook_event_name": "PreToolUse",
                        "cwd": cwd,
                        "session_id": session.to_string(),
                        "tool_name": name,
                        "tool_input": input,
                    }),
                    cwd,
                    Duration::from_millis(1500),
                )
                .await
            {
                context.push(hint);
            }
            let post_event = if ok {
                "PostToolUse"
            } else {
                "PostToolUseFailure"
            };
            let mut post = common;
            post["hook_event_name"] = serde_json::json!(post_event);
            if !ok {
                let end = result.floor_char_boundary(result.len().min(16 * 1024));
                post["error"] = serde_json::json!(&result[..end]);
            }
            match self
                .invoke(post_event, post, cwd, Duration::from_millis(1500))
                .await
            {
                Ok(Some(hint)) => context.push(hint),
                Ok(None) if context.is_empty() => return Ok(None),
                Err(error) if context.is_empty() => return Err(error),
                _ => {}
            }
            Ok(Some(context.join("\n\n")))
        })
    }

    fn stop<'a>(
        &'a self,
        cwd: &'a Path,
        session: SessionId,
        occurrence: EventId,
        items: Vec<EpisodeItem>,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let episode = episode_jsonl(&items)?;
            self.invoke(
                "EgoStop",
                serde_json::json!({
                    "hook_event_name": "Stop",
                    "cwd": cwd,
                    "session_id": session.to_string(),
                    "occurrence_id": occurrence,
                    "ego_episode_jsonl": episode,
                }),
                cwd,
                STOP_TIMEOUT,
            )
            .await
            .map(|_| ())
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

/// Adapt settled ego items to the episode shape mdkb actually parses.
/// Do not use ego's public event JSONL: it has a different wire vocabulary.
fn episode_jsonl(items: &[EpisodeItem]) -> Result<String, String> {
    let mut out = String::new();
    let mut lines = 0;
    for item in items {
        let records = match item {
            EpisodeItem::User(text) => vec![serde_json::json!({
                "type": "user", "message": {"role": "user", "content": text}
            })],
            EpisodeItem::Tool {
                id,
                name,
                args,
                ok,
                result,
                completed_at,
            } => {
                let input: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
                let file_path = input
                    .get("file_path")
                    .or_else(|| input.get("path"))
                    .and_then(serde_json::Value::as_str);
                let command = if name == "exec" {
                    input.get("argv").map(serde_json::Value::to_string)
                } else {
                    input
                        .get("command")
                        .or_else(|| input.get("pattern"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                };
                vec![
                    serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{
                        "type": "tool_use", "id": id.to_string(), "name": name,
                        "input": {"file_path": file_path, "command": command}
                    }]}}),
                    serde_json::json!({"type": "user", "message": {"role": "user", "content": [{
                        "type": "tool_result", "tool_use_id": id.to_string(),
                        "is_error": !ok, "content": result
                    }]}, "timestamp": completed_at.map(|ts| ts.to_rfc3339())}),
                ]
            }
        };
        for record in records {
            lines += 1;
            if lines > 800 {
                return Err("mdkb episode exceeds 800 records".into());
            }
            let line = serde_json::to_string(&record).map_err(|error| error.to_string())?;
            if out.len() + line.len() + 1 > MAX_EPISODE_BYTES {
                return Err("mdkb episode exceeds 256 KiB".into());
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    Ok(out)
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
    use super::{episode_jsonl, mdkb_tool_input, parse_context};

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
    use ego_core::event::id::ItemId;
    use ego_core::event::time::Timestamp;
    use ego_core::turn::EpisodeItem;

    // Catches: Stop exports ego public events instead of mdkb's accepted transcript blocks.
    #[test]
    fn stop_episode_has_correlated_tool_use_and_result_records() {
        let id = ItemId::new();
        let lines = episode_jsonl(&[
            EpisodeItem::User("Fix the parser".into()),
            EpisodeItem::Tool {
                id,
                name: "edit".into(),
                args: r#"{"file_path":"src/parser.rs"}"#.into(),
                ok: false,
                result: "file not found".into(),
                completed_at: Some(Timestamp::from_millis(1_767_948_240_000)),
            },
        ])
        .unwrap();
        let records: Vec<serde_json::Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["message"]["content"], "Fix the parser");
        assert_eq!(
            records[1]["message"]["content"][0]["input"]["file_path"],
            "src/parser.rs"
        );
        assert_eq!(records[1]["message"]["content"][0]["id"], id.to_string());
        assert_eq!(
            records[2]["message"]["content"][0]["tool_use_id"],
            id.to_string()
        );
        assert_eq!(records[2]["message"]["content"][0]["is_error"], true);
        assert_eq!(records[2]["timestamp"], "2026-01-09T08:44:00.000Z");
        assert_eq!(
            records[2]["message"]["content"][0]["content"],
            "file not found"
        );
    }

    // Catches: an oversized episode is silently truncated into a false clean outcome.
    #[test]
    fn oversized_stop_episode_is_rejected_without_truncation() {
        assert!(episode_jsonl(&[EpisodeItem::User("x".repeat(256 * 1024))]).is_err());
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
