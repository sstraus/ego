//! `write`: one named file, replaced whole, through the guarded seam.

use ego_core::permit::{
    Change, GuardedIo, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool,
};
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
/// **Whole-file, and that is the shape rather than a simplification.** This is
/// the tool whose effect can be read off the call with nothing else known:
/// [`Change::Whole`] carries the bytes that will be on disk afterwards, so what
/// the permission system weighs — and what a human would be shown — is the
/// content itself. `edit` is the tool for changing a region, and it pays for
/// that with a `Change` whose result depends on the file it finds.
pub struct WriteTool;

impl TypedTool for WriteTool {
    type Params = WriteParams;

    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create or replace one UTF-8 text file with the complete contents supplied. This \
         overwrites an existing file; it does not append or create parent directories. Use \
         edit for targeted changes to existing text. The path may be absolute or relative \
         to the working directory."
    }

    fn prepare_typed(
        &self,
        params: WriteParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall::new(
            self.name(),
            Op::Write {
                // `resolve_target`, not `resolve`: a write whose target had to
                // already exist could only ever overwrite.
                path: resolve_target(ctx, &params.path)?,
                // The bytes ride in the operation, so what policy weighs and
                // what a human would be shown is the content itself and not a
                // promise about it. A tool that kept its params and wrote
                // those instead would be authorised for one thing and perform
                // another.
                change: Change::Whole(params.contents.into_bytes()),
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
            let Op::Write {
                path,
                change: Change::Whole(contents),
            } = only_op(call)?
            else {
                return Err(wrong_op(self.name()));
            };
            // The bytes come out of the authorised call, never out of the
            // params — see `prepare_typed`.
            io.write(&path, &contents)?;
            Ok(ToolResult::builtin(format!(
                "wrote {} bytes to {path}{}",
                contents.len(),
                io.write_notice(&path)
                    .map(|notice| format!("\n{notice}"))
                    .unwrap_or_default()
            )))
        })
    }
}
