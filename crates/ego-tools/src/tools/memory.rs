//! Bounded, typed mdkb memory summaries through the normal guarded process path.
use std::ffi::OsString;
use std::time::Duration;

use ego_core::permit::{
    EnvPolicy, GuardedIo, Op, PrepareCtx, PreparedCall, ProcessError, SpawnSpec, StreamProvenance,
    ToolError, ToolOutputProvenance, ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryEntryType {
    Decision,
    Problem,
    Handoff,
}

impl MemoryEntryType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Problem => "problem",
            Self::Handoff => "handoff",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryLookupParams {
    pub query: String,
    pub entry_type: MemoryEntryType,
}

pub struct MemoryLookup;

#[derive(Deserialize)]
struct MemoryEntry {
    id: String,
    entry_type: String,
    title: String,
}

#[derive(Serialize)]
struct Summary {
    id: String,
    #[serde(rename = "type")]
    entry_type: MemoryEntryType,
    title: String,
}

fn named_error(name: &str) -> ToolError {
    ToolError::Process(ProcessError::Backend(name.to_owned()))
}

impl TypedTool for MemoryLookup {
    type Params = MemoryLookupParams;
    fn name(&self) -> &str {
        "memory_lookup"
    }
    fn description(&self) -> &str {
        "Search mdkb memory by decision/problem/handoff; return at most 5 id/type/title entries and 1 KiB"
    }
    fn prepare_typed(
        &self,
        params: Self::Params,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        if params.query.trim().is_empty() || params.query.contains('\0') {
            return Err(ToolError::InvalidParams(
                "memory query must be nonempty and contain no NUL".to_owned(),
            ));
        }
        let executable = if cfg!(windows) { "mdkb.exe" } else { "mdkb" };
        let found = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| {
                let directory = if directory.is_absolute() {
                    directory
                } else {
                    ctx.cwd.as_path().join(directory)
                };
                directory.join(executable).is_file()
            })
        });
        if !found {
            return Err(named_error("mdkb_unavailable_or_failed"));
        }
        let spec = SpawnSpec::new(
            [
                "mdkb",
                "search",
                &params.query,
                "--scope",
                "memory",
                "--entry-type",
                params.entry_type.as_str(),
                "--limit",
                "5",
                "--format",
                "json",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            Some(ctx.cwd.clone()),
            Some(Duration::from_secs(5)),
            EnvPolicy::Inherited,
        )
        .map_err(|_| named_error("mdkb_invalid_request"))?;
        Ok(PreparedCall::new(self.name(), Op::Spawn { spec }))
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
            let Op::Spawn { spec } = super::only_op(call)? else {
                return Err(super::wrong_op(self.name()));
            };
            let entry_type = match spec.argv().get(6).and_then(|arg| arg.to_str()) {
                Some("decision") => MemoryEntryType::Decision,
                Some("problem") => MemoryEntryType::Problem,
                Some("handoff") => MemoryEntryType::Handoff,
                _ => return Err(named_error("mdkb_invalid_request")),
            };
            let result = io.exec(&spec).await.map_err(|error| {
                named_error(match error {
                    ProcessError::Timeout { .. } => "mdkb_timeout",
                    _ => "mdkb_unavailable_or_failed",
                })
            })?;
            let stdout = result
                .content
                .iter()
                .find(|block| {
                    matches!(
                        block.provenance,
                        ToolOutputProvenance::Process {
                            stream: StreamProvenance::Stdout,
                            ..
                        }
                    )
                })
                .ok_or_else(|| named_error("mdkb_invalid_response"))?;
            if stdout.is_truncated() {
                return Err(named_error("mdkb_response_too_large"));
            }
            let entries: Vec<MemoryEntry> = serde_json::from_str(&stdout.text)
                .map_err(|_| named_error("mdkb_invalid_response"))?;
            let mut summaries = Vec::new();
            let mut rendered = "[]".to_owned();
            for entry in entries
                .into_iter()
                .filter(|entry| entry.entry_type == entry_type.as_str())
                .take(5)
            {
                summaries.push(Summary {
                    id: entry.id,
                    entry_type,
                    title: entry.title,
                });
                let candidate = serde_json::to_string(&summaries)
                    .map_err(|_| named_error("mdkb_invalid_response"))?;
                if candidate.len() > 1024 {
                    break;
                }
                rendered = candidate;
            }
            Ok(ToolResult::builtin(rendered))
        })
    }
}
