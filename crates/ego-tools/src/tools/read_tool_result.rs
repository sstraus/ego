//! `read_tool_result`: one bounded page from this session's complete process output.

use ego_core::permit::{GuardedIo, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool};
use ego_core::store::evidence::{EvidenceRead, EvidenceRef};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{only_op, wrong_op};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadToolResultParams {
    /// The exact full-output reference returned by a prior tool result.
    pub reference: EvidenceRef,
    /// Original-byte offset of the fixed-size page to retrieve.
    pub offset: u64,
}

/// Reads one fixed page from an evidence object bound to the current session.
pub struct ReadToolResult;

impl TypedTool for ReadToolResult {
    type Params = ReadToolResultParams;

    fn name(&self) -> &str {
        "read_tool_result"
    }

    fn description(&self) -> &str {
        "Read one fixed 32 KiB page of process output from this session"
    }

    fn prepare_typed(
        &self,
        params: Self::Params,
        _ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        let request = EvidenceRead::new(params.reference, params.offset)?;
        Ok(PreparedCall::new(
            self.name(),
            vec![Op::ReadEvidence { request }],
        ))
    }

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let Op::ReadEvidence { request } = only_op(call)? else {
                return Err(wrong_op(self.name()));
            };
            let page = io.read_evidence(&request).await?;
            Ok(ToolResult::new(vec![
                ego_core::permit::ToolContentBlock::evidence_page(page),
            ]))
        })
    }
}
