//! `write`: one named file, replaced whole, through the guarded seam.

use ego_core::permit::{GuardedIo, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{only_op, resolve_target, wrong_op};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteParams {
    /// The file to write. Absolute, or relative to the working directory.
    /// It is created if it does not exist; its parent directory must.
    pub path: String,
    /// The complete new contents. This replaces the file, it does not append
    /// to it.
    pub contents: String,
}

/// Replaces one file's contents. The first builtin that can do damage.
///
/// **Whole-file, and that is the shape rather than a simplification.** A
/// partial write — append, insert at a line — is an operation whose effect
/// cannot be read off the call: `Op::Write` carries the bytes that will be on
/// disk afterwards, and that is what makes a write something the permission
/// system, and eventually a human, can be *shown*. An `edit` that changes a
/// region is the next tool and needs a pre-image to be reviewable
/// (`docs/13-effect-tools.md` Step C); it is not this one with an offset.
pub struct WriteTool;

impl TypedTool for WriteTool {
    type Params = WriteParams;

    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write a file, replacing its entire contents. Creates the file if it does not exist; \
         its parent directory must already exist. The path may be absolute or relative to the \
         working directory."
    }

    fn prepare_typed(
        &self,
        params: WriteParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall {
            tool: self.name().to_string(),
            ops: vec![Op::Write {
                // `resolve_target`, not `resolve`: a write whose target had to
                // already exist could only ever overwrite.
                path: resolve_target(ctx, &params.path)?,
                // The bytes ride in the operation, so what policy weighs and
                // what a human would be shown is the content itself and not a
                // promise about it. A tool that kept its params and wrote
                // those instead would be authorised for one thing and perform
                // another.
                contents: params.contents.into_bytes(),
            }],
        })
    }

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let Op::Write { path, contents } = only_op(call)? else {
                return Err(wrong_op(self.name()));
            };
            // The bytes come out of the authorised call, never out of the
            // params — see `prepare_typed`.
            io.write(&path, &contents)?;
            Ok(ToolResult {
                content: format!("wrote {} bytes to {path}", contents.len()),
            })
        })
    }
}
