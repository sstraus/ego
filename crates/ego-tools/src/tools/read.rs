//! `read`: one named file, through the guarded seam.

use ego_core::permit::{GuardedIo, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{only_op, resolve, wrong_op};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadParams {
    /// The file to read. Absolute, or relative to the working directory.
    pub path: String,
}

/// Reads one file. The narrowest of the three: one path, one authority.
pub struct ReadTool;

impl TypedTool for ReadTool {
    type Params = ReadParams;

    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file's contents. The path may be absolute or relative to the working directory."
    }

    fn prepare_typed(
        &self,
        params: ReadParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall {
            tool: self.name().to_string(),
            ops: vec![Op::Read {
                path: resolve(ctx, &params.path)?,
            }],
        })
    }

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let Op::Read { path } = only_op(call)? else {
                return Err(wrong_op(self.name()));
            };
            // `io.read`, never an `Fs`: the path is re-resolved and rechecked
            // against the permit here, so a symlink swapped in since `prepare`
            // is judged as it stands now.
            let bytes = io.read(&path)?;
            // Lossy: a file with a stray byte in it is still worth showing a
            // model, and failing the call over one would tell it the file is
            // unreadable when it is merely not UTF-8.
            Ok(ToolResult::builtin(
                String::from_utf8_lossy(&bytes).into_owned(),
            ))
        })
    }
}
