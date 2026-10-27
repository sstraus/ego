//! The permanent argv-native process producer.
//!
//! This tool accepts a vector that is already argv. It never parses a shell
//! language and never inserts a shell between policy and the process backend.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ego_core::permit::{
    CanonPath, EnvPolicy, GuardedIo, Op, PrepareCtx, PreparedCall, SpawnSpec, ToolError,
    ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

/// Parameters for one exact process spawn.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecParams {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// Process lifetime in milliseconds. Foreground defaults to 120000 (maximum
    /// 600000). Background has no deadline when omitted or null.
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub background: bool,
}

impl ExecParams {
    /// The admitted process lifetime shared by preparation and presentation.
    /// Background work retains its explicit lifetime without the foreground cap.
    pub fn effective_timeout(&self) -> Result<Option<Duration>, ToolError> {
        let milliseconds = self.timeout_ms.or((!self.background).then_some(120_000));
        milliseconds
            .map(|milliseconds| {
                if milliseconds == 0 {
                    return Err(ToolError::InvalidParams(
                        "timeout_ms must be greater than zero".into(),
                    ));
                }
                if !self.background && milliseconds > 600_000 {
                    return Err(ToolError::InvalidParams(
                        "foreground timeout_ms must not exceed 600000; use background=true for longer work".into(),
                    ));
                }
                Ok(Duration::from_millis(milliseconds))
            })
            .transpose()
    }
}

#[derive(Default)]
pub struct ExecTool {
    environment: EnvPolicy,
}

impl ExecTool {
    /// Select the run-owned child environment before tool preparation.
    pub fn with_environment(environment: EnvPolicy) -> Self {
        Self { environment }
    }
}

impl TypedTool for ExecTool {
    type Params = ExecParams;

    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Run one argv-native process: pass executable and arguments separately, with no implicit shell, pipes or redirects. Use cwd for its working directory. timeout_ms limits the entire process lifetime, not how long to wait for a handle. Foreground defaults to 120000 ms (2 min), with overrides from 1 to 600000 ms (10 min); omitted or null uses the default. Background has no deadline when timeout_ms is omitted or null, and accepts an explicit lifetime limit. Set background=true for long work; returns a session-owned ID immediately for exec_status or exec_cancel, and completion wakes an idle interactive session. Use read_tool_result with returned full-output references for truncated output"
    }

    fn prepare_typed(
        &self,
        params: Self::Params,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        if params.argv.is_empty() {
            return Err(ToolError::InvalidParams(
                "argv must contain an executable".to_string(),
            ));
        }
        if params.argv.iter().any(|arg| arg.contains('\0')) {
            return Err(ToolError::InvalidParams(
                "argv must not contain NUL bytes".to_string(),
            ));
        }
        let timeout = params.effective_timeout()?;
        let cwd = match params.cwd {
            Some(named) => Some(resolve_directory(ctx, &named)?),
            None => Some(ctx.cwd.clone()),
        };
        let spec = SpawnSpec::new(
            params.argv.into_iter().map(OsString::from).collect(),
            cwd,
            timeout,
            self.environment.clone(),
        )
        .map_err(|err| ToolError::InvalidParams(err.to_string()))?;

        let call = PreparedCall::new(self.name(), Op::Spawn { spec });
        Ok(if params.background {
            call.with_background()
        } else {
            call
        })
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
            let background = call.background();
            let (tool, ops) = call.into_parts();
            let mut ops = ops.into_iter();
            let spec = match (ops.next(), ops.next()) {
                (Some(Op::Spawn { spec }), None) => spec,
                _ => {
                    return Err(ToolError::InvalidParams(format!(
                        "{tool} was asked to execute an operation it did not prepare"
                    )));
                }
            };
            if let Some(id) = background {
                io.exec_background(id, &spec)
                    .await
                    .map_err(ToolError::Process)?;
                Ok(ToolResult::builtin(format!("background exec id: {id}")))
            } else {
                io.exec(&spec).await.map_err(ToolError::Process)
            }
        })
    }
}

fn resolve_directory(ctx: &PrepareCtx<'_>, named: &str) -> Result<CanonPath, ToolError> {
    let named = Path::new(named);
    let joined: PathBuf = if named.is_absolute() {
        named.to_path_buf()
    } else {
        ctx.cwd.as_path().join(named)
    };
    Ok(ctx.resolver.canonicalize(&joined)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: omitted foreground deadlines stay unbounded, or rejected overrides
    // reach SpawnSpec. This drives tool preparation, not just timeout arithmetic.
    #[test]
    fn foreground_exec_timeout_contract() {
        let fs = crate::fs::OsFs;
        let ctx = PrepareCtx {
            cwd: CanonPath::new(std::env::current_dir().unwrap()),
            resolver: &fs,
        };
        for (args, expected) in [
            (r#"{"argv":["echo"]}"#, Ok(Some(120000))),
            (r#"{"argv":["echo"],"timeout_ms":null}"#, Ok(Some(120000))),
            (r#"{"argv":["echo"],"timeout_ms":1}"#, Ok(Some(1))),
            (r#"{"argv":["echo"],"timeout_ms":600000}"#, Ok(Some(600000))),
            (r#"{"argv":["echo"],"timeout_ms":0}"#, Err(())),
            (r#"{"argv":["echo"],"timeout_ms":600001}"#, Err(())),
            (
                r#"{"argv":["echo"],"timeout_ms":18446744073709551615}"#,
                Err(()),
            ),
            (r#"{"argv":["echo"],"background":true}"#, Ok(None)),
            (
                r#"{"argv":["echo"],"background":true,"timeout_ms":null}"#,
                Ok(None),
            ),
            (
                r#"{"argv":["echo"],"background":true,"timeout_ms":601000}"#,
                Ok(Some(601000)),
            ),
            (
                r#"{"argv":["echo"],"background":true,"timeout_ms":0}"#,
                Err(()),
            ),
        ] {
            let params = serde_json::from_str(args).unwrap();
            let prepared = ExecTool::default().prepare_typed(params, &ctx);
            match expected {
                Ok(timeout) => {
                    let call = prepared.unwrap_or_else(|error| panic!("{args}: {error}"));
                    let [Op::Spawn { spec }] = call.ops() else {
                        panic!("one spawn: {args}")
                    };
                    assert_eq!(spec.timeout(), timeout.map(Duration::from_millis), "{args}");
                }
                Err(()) => assert!(
                    matches!(prepared, Err(ToolError::InvalidParams(_))),
                    "{args}"
                ),
            }
        }
    }
}
