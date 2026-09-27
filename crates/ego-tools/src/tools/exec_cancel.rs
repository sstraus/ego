//! Cancel a process owned by the current session.

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
pub struct ExecCancelParams {
    pub id: String,
}

pub struct ExecCancelTool;

impl TypedTool for ExecCancelTool {
    type Params = ExecCancelParams;

    fn name(&self) -> &str {
        "exec_cancel"
    }

    fn description(&self) -> &str {
        "Cancel one background exec owned by this session"
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
                action: SessionProcessAction::Cancel,
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
                action: SessionProcessAction::Cancel,
            } = only_op(call)?
            else {
                return Err(wrong_op(self.name()));
            };
            io.background_cancel(id).map_err(ToolError::Process)?;
            Ok(ToolResult::builtin(format!(
                "background exec {id}: cancelled"
            )))
        })
    }
}
