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
use ego_core::turn::{EpisodeItem, GenerationPolicySource, PromptContextSource, ToolEffectPorts};

const RECALL_TIMEOUT: Duration = Duration::from_millis(1500);
const WARMUP_TIMEOUT: Duration = Duration::from_millis(2500);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_EPISODE_BYTES: usize = 262_144;
const MAX_EPISODE_RECORDS: usize = 800;
// A JSON string may expand each source byte to six bytes, plus the envelope.
const MAX_STOP_REQUEST_BYTES: usize = 6 * MAX_EPISODE_BYTES + 4096;

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
            "Stop" => "stop",
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
            "Stop" => "mdkb_stop_observation",
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
        let max_input = if event == "Stop" {
            MAX_STOP_REQUEST_BYTES
        } else {
            MAX_RESPONSE_BYTES
        };
        if input.len() > max_input {
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

    fn stop<'a>(
        &'a self,
        cwd: &'a Path,
        session: SessionId,
        items: Vec<EpisodeItem>,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let episode = episode_jsonl(&items)?;
            self.invoke(
                "Stop",
                serde_json::json!({
                    "hook_event_name": "Stop",
                    "session_id": session.to_string(),
                    "episode_jsonl": episode,
                }),
                cwd,
                RECALL_TIMEOUT,
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

/// Project complete records or omit the whole observation: never send a
/// truncated failure, an orphan tool result, or a transcript-file fallback.
fn episode_jsonl(items: &[EpisodeItem]) -> Result<String, String> {
    let mut out = String::new();
    let mut count = 0;
    for item in items {
        let records = match item {
            EpisodeItem::User(text) => vec![serde_json::json!({
                "type": "user", "message": {"content": text}
            })],
            EpisodeItem::Assistant(text) => vec![serde_json::json!({
                "type": "assistant", "message": {"content": text}
            })],
            EpisodeItem::Tool {
                id,
                name,
                args,
                ok,
                result,
                completed_at,
            } => {
                let (name, input) = mdkb_tool_input(name, args)?;
                let mut result_record = serde_json::json!({
                    "type": "user", "message": {"content": [{
                        "type": "tool_result", "tool_use_id": id.to_string(),
                        "is_error": !ok, "content": result
                    }]}
                });
                if let Some(at) = completed_at {
                    result_record["timestamp"] = serde_json::json!(at.to_rfc3339());
                }
                vec![
                    serde_json::json!({
                        "type": "assistant", "message": {"content": [{
                            "type": "tool_use", "id": id.to_string(), "name": name, "input": input
                        }]}
                    }),
                    result_record,
                ]
            }
        };
        for record in records {
            count += 1;
            if count > MAX_EPISODE_RECORDS {
                return Err("mdkb episode exceeds 800 records".into());
            }
            let line = serde_json::to_string(&record).map_err(|error| error.to_string())?;
            if out.len() + line.len() + 1 > MAX_EPISODE_BYTES {
                return Err("mdkb episode exceeds 262144 bytes".into());
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
    use super::*;

    // Catches: exclusive limits reject legal boundary episodes, or count/byte
    // overflow emits a partial record instead of omitting the observation.
    #[test]
    fn episode_wire_limits_are_inclusive_and_never_truncate() {
        assert_eq!(episode_jsonl(&[]).unwrap(), "");
        let mut items = vec![EpisodeItem::User("x".into()); 800];
        assert_eq!(episode_jsonl(&items).unwrap().lines().count(), 800);
        items.push(EpisodeItem::Assistant("y".into()));
        assert!(episode_jsonl(&items).is_err());
        // Independent literal wire shape from mdkb 014 docs/hooks.md.
        let overhead = "{\"type\":\"user\",\"message\":{\"content\":\"\"}}\n".len();
        let text = "x".repeat(262_144 - overhead);
        let wire = episode_jsonl(&[EpisodeItem::User(text.clone())]).unwrap();
        assert_eq!(wire.len(), 262_144);
        let record: serde_json::Value = serde_json::from_str(wire.trim()).unwrap();
        assert_eq!(record["message"]["content"], text);
        assert!(episode_jsonl(&[EpisodeItem::User(format!("{text}x"))]).is_err());
    }
}
