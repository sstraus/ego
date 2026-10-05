//! TUIC reads through the existing guarded argv/file effects. Never reads mail.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ego_core::orchestrator::{Brief, ChildLine, Filter, Orchestrator, bounded_line};
use ego_core::permit::{
    CanonPath, EnvPolicy, FsError, GuardedIo, IoDenied, Op, PrepareCtx, PreparedCall, SpawnSpec,
    StreamProvenance, Tool, ToolError, ToolOutputProvenance, ToolResult, Typed, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    pub root: Option<String>,
}

#[derive(Clone, Copy)]
enum ReadKind {
    Status,
    Brief,
}

struct TuicReadTool {
    kind: ReadKind,
    children: PathBuf,
    results: PathBuf,
}

/// Advertise only where a TUIC socket is explicitly present.
pub fn tools(socket: Option<&std::ffi::OsStr>, home: &Path) -> Vec<Arc<dyn Tool>> {
    if socket.is_none_or(std::ffi::OsStr::is_empty) {
        return Vec::new();
    }
    [ReadKind::Status, ReadKind::Brief]
        .into_iter()
        .map(|kind| {
            Arc::new(Typed(TuicReadTool {
                kind,
                children: home.join("Gits/.tmp/coordinator/children.tsv"),
                results: home.join("Gits/.tmp/results"),
            })) as Arc<dyn Tool>
        })
        .collect()
}

/// Bind the operator's environment once when composing the native catalogue.
pub fn tools_from_environment() -> Vec<Arc<dyn Tool>> {
    tools_from_environment_with_mode(ego_core::orchestrator::Mode::Observe)
}

pub fn tools_from_environment_with_mode(mode: ego_core::orchestrator::Mode) -> Vec<Arc<dyn Tool>> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    tools_with_mode(
        std::env::var_os("TUIC_SOCKET").as_deref(),
        Path::new(&home),
        std::env::var("TUIC_SESSION").ok(),
        mode,
    )
}

pub fn tools_with_mode(
    socket: Option<&std::ffi::OsStr>,
    home: &Path,
    parent: Option<String>,
    mode: ego_core::orchestrator::Mode,
) -> Vec<Arc<dyn Tool>> {
    let mut tools = tools(socket, home);
    if !tools.is_empty() && mode == ego_core::orchestrator::Mode::Active {
        tools.push(super::orchestrator_active::tool(mode, home, parent));
    }
    tools
}

impl TypedTool for TuicReadTool {
    type Params = ReadParams;
    fn name(&self) -> &str {
        match self.kind {
            ReadKind::Status => "orchestrator_status",
            ReadKind::Brief => "orchestrator_brief",
        }
    }
    fn description(&self) -> &str {
        "Read tracked TUIC children and unread result paths; no mail consumption or mutation"
    }
    fn prepare_typed(
        &self,
        params: ReadParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        let root = params
            .root
            .map(|root| {
                let root = Path::new(&root);
                ctx.resolver.canonicalize(&if root.is_absolute() {
                    root.to_owned()
                } else {
                    ctx.cwd.as_path().join(root)
                })
            })
            .transpose()?;
        let session = spec(
            vec![
                "tuic".into(),
                "mcp".into(),
                "session".into(),
                r#"{"action":"list"}"#.into(),
            ],
            root,
        )?;
        let children = super::tools::resolve_target(ctx, &self.children.to_string_lossy())?;
        let mut call = PreparedCall::new(self.name(), Op::Spawn { spec: session })
            .with_op(Op::Read { path: children });
        if matches!(self.kind, ReadKind::Brief) {
            match ctx
                .resolver
                .canonicalize(&self.results.join(".watch-stamp"))
            {
                Ok(stamp) => {
                    let results = ctx.resolver.canonicalize(&self.results)?;
                    call = call.with_op(Op::Spawn {
                        spec: spec(
                            vec![
                                "find".into(),
                                results.as_path().as_os_str().to_owned(),
                                "-maxdepth".into(),
                                "1".into(),
                                "-name".into(),
                                "*.md".into(),
                                "-newer".into(),
                                stamp.as_path().as_os_str().to_owned(),
                            ],
                            None,
                        )?,
                    });
                }
                Err(FsError::NotFound { .. }) => {} // coord-resume.sh reports no unread files without a stamp.
                Err(error) => return Err(ToolError::Fs(error)),
            }
        }
        Ok(call)
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
            let ops = call.ops();
            let [
                Op::Spawn { spec: session },
                Op::Read { path: children },
                rest @ ..,
            ] = ops
            else {
                return Err(ToolError::InvalidParams(
                    "invalid orchestrator read effects".into(),
                ));
            };
            let unread = match rest {
                [] => None,
                [Op::Spawn { spec }] => Some(spec),
                _ => {
                    return Err(ToolError::InvalidParams(
                        "invalid orchestrator brief effects".into(),
                    ));
                }
            };
            let adapter = TuicOrchestrator {
                io,
                session,
                children,
                unread,
            };
            let filter = Filter {
                root: session.cwd().map(|path| path.as_path().to_owned()),
            };
            let text = match self.kind {
                ReadKind::Status => adapter.status(&filter).await.map(|rows| {
                    rows.iter()
                        .map(ChildLine::text)
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
                ReadKind::Brief => adapter.brief(&filter).await.map(|brief| {
                    format!(
                        "Children:\n{}\nUnread results:\n{}",
                        brief
                            .children
                            .iter()
                            .map(ChildLine::text)
                            .collect::<Vec<_>>()
                            .join("\n"),
                        brief.unread_results.join("\n")
                    )
                }),
            }
            .map_err(ToolError::InvalidParams)?;
            Ok(ToolResult::builtin(text))
        })
    }
}

pub(super) fn spec(
    argv: Vec<std::ffi::OsString>,
    cwd: Option<CanonPath>,
) -> Result<SpawnSpec, ToolError> {
    SpawnSpec::new(
        argv,
        cwd,
        Some(Duration::from_secs(10)),
        EnvPolicy::Inherited,
    )
    .map_err(|error| ToolError::InvalidParams(error.to_string()))
}

/// Adapter lifetime is bounded by the exact permit-backed tool execution.
pub struct TuicOrchestrator<'a, 'io> {
    io: &'a dyn GuardedIo<'io>,
    session: &'a SpawnSpec,
    children: &'a CanonPath,
    unread: Option<&'a SpawnSpec>,
}

impl Orchestrator for TuicOrchestrator<'_, '_> {
    fn status<'a>(&'a self, filter: &'a Filter) -> BoxFuture<'a, Result<Vec<ChildLine>, String>> {
        Box::pin(async move {
            let sessions = stdout(
                &self
                    .io
                    .exec(self.session)
                    .await
                    .map_err(|error| error.to_string())?,
            )?;
            let children = match self.io.read(self.children) {
                Ok(bytes) => String::from_utf8(bytes).map_err(|error| error.to_string())?,
                Err(IoDenied::Fs(FsError::NotFound { .. })) => String::new(),
                Err(error) => return Err(error.to_string()),
            };
            project_status(&sessions, &children, filter)
        })
    }
    fn brief<'a>(&'a self, filter: &'a Filter) -> BoxFuture<'a, Result<Brief, String>> {
        Box::pin(async move {
            let children = self.status(filter).await?;
            let unread_results = match self.unread {
                Some(spec) => stdout(
                    &self
                        .io
                        .exec(spec)
                        .await
                        .map_err(|error| error.to_string())?,
                )?
                .lines()
                .map(bounded_line)
                .collect(),
                None => Vec::new(),
            };
            Ok(Brief {
                children,
                unread_results,
            })
        })
    }
}

pub(super) fn stdout(result: &ToolResult) -> Result<String, String> {
    let mut text = String::new();
    for block in &result.content {
        match &block.provenance {
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stdout,
                truncated: true,
                ..
            } => return Err("orchestrator read exceeded the process output cap".into()),
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stdout,
                ..
            }
            | ToolOutputProvenance::Builtin => text.push_str(&block.text),
            _ => {}
        }
    }
    Ok(text)
}

/// Join live sessions against the tracked TSV, in the same order as coord-resume.sh.
pub fn project_status(
    sessions: &str,
    children: &str,
    filter: &Filter,
) -> Result<Vec<ChildLine>, String> {
    let sessions: Vec<serde_json::Value> = serde_json::from_str(sessions)
        .map_err(|error| format!("invalid TUIC session list: {error}"))?;
    let mut rows = Vec::new();
    for child in children.lines() {
        let fields = child.split('\t').collect::<Vec<_>>();
        let [id, name, cwd, ..] = fields.as_slice() else {
            return Err("invalid tracked child row".into());
        };
        if filter
            .root
            .as_ref()
            .is_some_and(|root| !Path::new(cwd).starts_with(root))
        {
            continue;
        }
        let Some(session) = sessions.iter().find(|session| {
            session
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                == Some(*id)
        }) else {
            continue;
        };
        let state = session
            .get("agent_state")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                session
                    .get("shell_state")
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap_or("?");
        rows.push(ChildLine::new(id, name, state, cwd));
    }
    Ok(rows)
}
