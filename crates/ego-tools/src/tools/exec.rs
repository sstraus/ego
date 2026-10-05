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
    pub timeout_ms: Option<u64>,
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
        "Run one exact argv-native process without a shell"
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
        let timeout = params
            .timeout_ms
            .map(|milliseconds| {
                if milliseconds == 0 {
                    Err(ToolError::InvalidParams(
                        "timeout_ms must be greater than zero".to_string(),
                    ))
                } else {
                    Ok(Duration::from_millis(milliseconds))
                }
            })
            .transpose()?;
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
            io.exec(&spec).await.map_err(ToolError::Process)
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
