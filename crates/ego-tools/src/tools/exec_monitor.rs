//! Subscribe the model to new output lines of a session-owned exec.

use ego_core::event::id::DetachedExecId;
use ego_core::permit::{
    DetachedExecMonitor, GuardedIo, Op, PrepareCtx, PreparedCall, SessionProcessAction, ToolError,
    ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{only_op, wrong_op};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecMonitorParams {
    pub id: String,
    /// Regular expression a complete line must match; omitted or null
    /// delivers every new line.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Lines one delivery carries, from 1 to 100; omitted or null uses 20.
    #[serde(default)]
    pub max_lines: Option<usize>,
}

impl ExecMonitorParams {
    /// Range and pattern are validated once, where the monitor is installed.
    fn monitor(&self) -> DetachedExecMonitor {
        DetachedExecMonitor {
            pattern: self.pattern.clone(),
            max_lines: self.max_lines.unwrap_or(DetachedExecMonitor::DEFAULT_LINES),
        }
    }
}

pub struct ExecMonitorTool;

impl TypedTool for ExecMonitorTool {
    type Params = ExecMonitorParams;

    fn name(&self) -> &str {
        "exec_monitor"
    }

    fn description(&self) -> &str {
        "Watch new complete output lines of a background exec ID owned by this session, without polling. Set pattern to a regular expression to receive only matching lines; omit it to receive every new line. Queued lines enter your context at the next step boundary, and wake an idle interactive session. Each delivery has at most max_lines lines (default 20, at most 100) and 4096 bytes; lines longer than 512 bytes are cut, and later lines are counted as omitted. The monitor ends when the exec settles or is cancelled; calling exec_monitor again for the same ID replaces the pattern and limit, but only exec_cancel removes the monitor. Without a pattern, every delivery can start a new turn of an idle session until the exec settles; use a pattern for servers and long builds"
    }

    fn prepare_typed(
        &self,
        params: Self::Params,
        _ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        let id = params.id.parse::<DetachedExecId>().map_err(|error| {
            ToolError::InvalidParams(format!("invalid background exec ID: {error}"))
        })?;
        let monitor = params.monitor();
        Ok(PreparedCall::new(
            self.name(),
            Op::SessionProcess {
                id,
                action: SessionProcessAction::Inspect,
            },
        )
        .with_monitor(monitor))
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
            let monitor = call
                .monitor()
                .cloned()
                .ok_or_else(|| wrong_op(self.name()))?;
            let Op::SessionProcess {
                id,
                action: SessionProcessAction::Inspect,
            } = only_op(call)?
            else {
                return Err(wrong_op(self.name()));
            };
            let filter = match &monitor.pattern {
                Some(pattern) => format!("lines matching {pattern:?}"),
                None => "every new line".to_owned(),
            };
            let max_lines = monitor.max_lines;
            io.background_monitor(id, monitor)
                .map_err(ToolError::Process)?;
            Ok(ToolResult::builtin(format!(
                "background exec {id}: monitoring {filter}, at most {max_lines} lines per delivery, until it settles"
            )))
        })
    }
}
