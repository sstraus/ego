//! Native active operations share the existing guarded process boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ego_core::orchestrator::{
    ActiveRequest, Brief, ChildLine, Filter, Mode, Orchestrator, SpawnReceipt,
};
use ego_core::permit::{
    GuardedIo, Op, PrepareCtx, PreparedCall, Tool, ToolError, ToolResult, Typed, TypedTool,
};
use futures::future::BoxFuture;
use serde_json::json;

/// Keep an observe instance callable for refusal checks, even when not advertised.
pub fn tool(mode: Mode, home: &Path, parent: Option<String>) -> Arc<dyn Tool> {
    Arc::new(Typed(ActiveTool {
        mode,
        home: home.into(),
        parent,
        rework: None,
    }))
}

/// Bind same-thread correction budgets reconstructed from this session's journal.
pub fn tool_with_rework_limits(
    mode: Mode,
    home: &Path,
    parent: Option<String>,
    limits: BTreeMap<String, u8>,
) -> Arc<dyn Tool> {
    Arc::new(Typed(ActiveTool {
        mode,
        home: home.into(),
        parent,
        rework: Some(Mutex::new(limits)),
    }))
}

pub fn rework_limits(
    entries: &[ego_core::store::session_store::SessionEntry],
) -> Result<BTreeMap<String, u8>, String> {
    use ego_core::event::domain::EventKind;
    use ego_core::store::session_store::SessionEntry;
    let mut limits: BTreeMap<_, _> = super::orchestration_events::spawn_receipts(entries)?
        .into_iter()
        .map(|r| (r.session_id, 0u8))
        .collect();
    for entry in entries {
        if let SessionEntry::Event(event) = entry
            && let EventKind::ToolCallStarted { name, args, .. } = &event.kind
            && name == "orchestrator_active"
            && let Ok(ActiveRequest::Say { session_id, .. }) = serde_json::from_str(args)
            && let Some(count) = limits.get_mut(&session_id)
        {
            *count = count.saturating_add(1);
        }
    }
    Ok(limits)
}

struct ActiveTool {
    mode: Mode,
    home: PathBuf,
    parent: Option<String>,
    rework: Option<Mutex<BTreeMap<String, u8>>>,
}

impl TypedTool for ActiveTool {
    type Params = ActiveRequest;

    fn name(&self) -> &str {
        "orchestrator_active"
    }
    fn description(&self) -> &str {
        "Spawn or consult managed peers, say a message, create a worktree, or land a branch. Land always requires a human card. Spawn results are recorded in this session. End your turn after launching; RESULT/BLOCKED resumes this conversation automatically. Do not poll. At most two native correction requests per owned child thread."
    }

    fn prepare_typed(
        &self,
        mut request: ActiveRequest,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        self.require_active()?;
        let (argv, cwd) = match &mut request {
            ActiveRequest::Spawn {
                role,
                name,
                cwd,
                brief,
            }
            | ActiveRequest::Consult {
                role,
                name,
                cwd,
                brief,
            } => {
                let consult = name.starts_with("consult-");
                let (agent, model, args) = role_args(role, consult)?;
                if !valid_name(name) || brief.trim().is_empty() {
                    return Err(ToolError::InvalidParams(
                        "a safe peer name and nonempty brief are required".into(),
                    ));
                }
                let parent = self
                    .parent
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ToolError::InvalidParams("spawn requires a TUIC_SESSION parent".into())
                    })?;
                let path = super::tools::resolve_target(ctx, cwd)?;
                *cwd = path.as_path().to_string_lossy().into_owned();
                let prompt = managed_prompt(role, name, cwd, parent, brief);
                (
                    vec![
                        "tuic".into(),
                        "mcp".into(),
                        "agent".into(),
                        json!({
                            "action":"spawn", "name":name, "agent_type":agent, "model":model,
                            "args":args, "cwd":cwd, "prompt":prompt, "pty_description":name
                        })
                        .to_string()
                        .into(),
                    ],
                    Some(path),
                )
            }
            ActiveRequest::Say {
                session_id,
                message,
            } => {
                nonempty(session_id)?;
                nonempty(message)?;
                if let Some(limits) = &self.rework
                    && let Some(count) = limits
                        .lock()
                        .expect("rework budget poisoned")
                        .get_mut(session_id)
                {
                    if *count >= 2 {
                        return Err(ToolError::InvalidParams("this thread has exhausted its two rework requests; give maintainer the consolidated outcome".into()));
                    }
                    *count += 1;
                }
                (
                    vec![
                        "tuic".into(),
                        "mcp".into(),
                        "session".into(),
                        json!({
                            "action":"submit", "session_id":session_id, "input":message
                        })
                        .to_string()
                        .into(),
                    ],
                    None,
                )
            }
            ActiveRequest::WorktreeCreate { path, branch } => {
                nonempty(branch)?;
                let root = super::tools::resolve_target(ctx, path)?;
                (
                    vec![
                        "tuic".into(),
                        "mcp".into(),
                        "repo".into(),
                        json!({
                            "action":"worktree_create", "path":root.as_path(), "branch":branch,
                            "spawn_session":false
                        })
                        .to_string()
                        .into(),
                    ],
                    Some(root),
                )
            }
            ActiveRequest::Land {
                path,
                branch,
                message,
            } => {
                nonempty(branch)?;
                if branch.starts_with('-') {
                    return Err(ToolError::InvalidParams("invalid branch".into()));
                }
                nonempty(message)?;
                let root = super::tools::resolve_target(ctx, path)?;
                let wrapper = self
                    .home
                    .join("workspace/example-project/tools/land/tuic-land.sh");
                let wrapper = ctx.resolver.canonicalize(&wrapper)?;
                (
                    vec![
                        wrapper.as_path().as_os_str().to_owned(),
                        root.as_path().as_os_str().to_owned(),
                        branch.clone().into(),
                        message.clone().into(),
                    ],
                    Some(root),
                )
            }
        };
        // Consults carry an explicit read-only contract and a recognizable paid role.
        if let ActiveRequest::Consult { ref name, .. } = request
            && !name.starts_with("consult-")
        {
            return Err(ToolError::InvalidParams(
                "consult names must start with consult-".into(),
            ));
        }
        Ok(PreparedCall::new(
            self.name(),
            Op::Spawn {
                spec: super::orchestrator::spec(argv, cwd)?,
            },
        ))
    }

    fn execute<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            self.require_active()?;
            let [Op::Spawn { spec }] = call.ops() else {
                return Err(ToolError::InvalidParams("invalid active effects".into()));
            };
            let adapter = ActiveAdapter { io, spec };
            let text = adapter.active().await.map_err(ToolError::InvalidParams)?;
            Ok(ToolResult::builtin(text))
        })
    }
}

impl ActiveTool {
    fn require_active(&self) -> Result<(), ToolError> {
        if self.mode == Mode::Observe {
            Err(ToolError::InvalidParams(
                "active orchestration is disabled in observe mode".into(),
            ))
        } else {
            Ok(())
        }
    }
}

fn nonempty(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() || value.contains('\0') {
        Err(ToolError::InvalidParams(
            "nonempty text without NUL is required".into(),
        ))
    } else {
        Ok(())
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn role_args(
    role: &str,
    consult: bool,
) -> Result<(&'static str, &'static str, Vec<String>), ToolError> {
    let (agent, model, args): (_, _, &[&str]) = match role {
        "sol" => (
            "codex",
            "gpt-6.1-sol",
            &[
                "--no-alt-screen",
                "-c",
                "model_reasoning_effort=\"medium\"",
                "--dangerously-bypass-approvals-and-sandbox",
            ],
        ),
        "sonnet" => ("claude", "sonnet", &["--dangerously-skip-permissions"]),
        "astra" if consult => (
            "codex",
            "gpt-6-astra",
            &[
                "--no-alt-screen",
                "-c",
                "model_reasoning_effort=\"low\"",
                "--dangerously-bypass-approvals-and-sandbox",
            ],
        ),
        "fable" if consult => (
            "claude",
            "claude-fable-5-1",
            &["--dangerously-skip-permissions"],
        ),
        "grok" if consult => ("grok", "grok-4.7", &[]),
        _ => {
            return Err(ToolError::InvalidParams(
                "unsupported orchestration role".into(),
            ));
        }
    };
    Ok((agent, model, args.iter().map(|s| (*s).into()).collect()))
}

/// The managed-child header keeps ownership, policy and final delivery explicit.
pub fn managed_prompt(role: &str, name: &str, cwd: &str, parent: &str, brief: &str) -> String {
    let advisor = if name.starts_with("consult-") {
        "Mode: advisor, read-only. Do not edit, commit, or run builds/tests.\n"
    } else {
        ""
    };
    let model = role_args(role, name.starts_with("consult-"))
        .map(|(_, model, _)| model)
        .unwrap_or(role);
    let header = include_str!("managed_child_header.txt")
        .replace("{name}", name)
        .replace("{role}", role)
        .replace("{model}", model)
        .replace("{parent}", parent)
        .replace("{cwd}", cwd);
    format!("{header}\n{advisor}\n{brief}")
}

struct ActiveAdapter<'a, 'io> {
    io: &'a dyn GuardedIo<'io>,
    spec: &'a ego_core::permit::SpawnSpec,
}
impl Orchestrator for ActiveAdapter<'_, '_> {
    fn status<'a>(&'a self, _: &'a Filter) -> BoxFuture<'a, Result<Vec<ChildLine>, String>> {
        Box::pin(async { Err("active adapter has no read projection".into()) })
    }
    fn brief<'a>(&'a self, _: &'a Filter) -> BoxFuture<'a, Result<Brief, String>> {
        Box::pin(async { Err("active adapter has no read projection".into()) })
    }
    fn active<'a>(&'a self) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let output = self.io.exec(self.spec).await.map_err(|e| e.to_string())?;
            let text = super::orchestrator::stdout(&output)?;
            if self.spec.argv()[0] != "tuic" {
                return Ok(text);
            }
            let request: serde_json::Value = serde_json::from_str(
                &self
                    .spec
                    .argv()
                    .get(3)
                    .ok_or("missing TUIC request")?
                    .to_string_lossy(),
            )
            .map_err(|e| e.to_string())?;
            let response: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| e.to_string())?;
            if response.get("error").is_some() {
                return Err(format!("TUIC refused operation: {text}"));
            }
            match request["action"].as_str() {
                Some("spawn") => {
                    let id = response["session_id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or("TUIC spawn returned no session_id")?;
                    let receipt = SpawnReceipt {
                        session_id: id.into(),
                        name: request["name"].as_str().ok_or("missing name")?.into(),
                        cwd: request["cwd"].as_str().ok_or("missing cwd")?.into(),
                        prompt: request["prompt"].as_str().ok_or("missing prompt")?.into(),
                        spawned_at_ms: super::orchestration_events::millis(
                            std::time::SystemTime::now(),
                        ),
                    };
                    Ok(json!({"orchestration_spawn":receipt}).to_string())
                }
                Some("submit") if response["acknowledged"] != true => Err(format!(
                    "TUIC did not acknowledge submission; no automatic retry: {text}"
                )),
                Some("worktree_create") if !response["worktree_path"].is_string() => {
                    Err(format!("TUIC returned no worktree receipt: {text}"))
                }
                _ => Ok(text),
            }
        })
    }
}
