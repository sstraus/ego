//! Inspect the bounded live output and settlement of a session-owned exec.

use ego_core::event::id::DetachedExecId;
use ego_core::permit::{
    GuardedIo, Op, PrepareCtx, PreparedCall, SessionProcessAction, ToolError, ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{only_op, wrong_op};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecStatusParams {
    pub id: String,
}

pub struct ExecStatusTool;

impl TypedTool for ExecStatusTool {
    type Params = ExecStatusParams;

    fn name(&self) -> &str {
        "exec_status"
    }

    fn description(&self) -> &str {
        "Read bounded output and settlement for a background exec ID owned by this session. Use the ID returned by exec with background=true. A running status does not mean completion; use read_tool_result for any returned full-output reference"
    }

    fn prepare_typed(
        &self,
        params: Self::Params,
        _ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        let id = params.id.parse::<DetachedExecId>().map_err(|error| {
            ToolError::InvalidParams(format!("invalid background exec ID: {error}"))
        })?;
        Ok(PreparedCall::new(
            self.name(),
            Op::SessionProcess {
                id,
                action: SessionProcessAction::Inspect,
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
            let Op::SessionProcess {
                id,
                action: SessionProcessAction::Inspect,
            } = only_op(call)?
            else {
                return Err(wrong_op(self.name()));
            };
            let status = io.background_status(id).map_err(ToolError::Process)?;
            Ok(ToolResult::builtin(status.model_text()))
        })
    }
}
