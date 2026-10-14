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
    /// Original-byte offset. Defaults to zero; use the continuation hint for the next page.
    #[serde(default)]
    pub offset: u64,
}

/// Reads one file. The narrowest of the three: one path, one authority.
pub struct ReadTool;

impl TypedTool for ReadTool {
    type Params = ReadParams;

    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read at most 2000 lines or 32 KiB of a file. Truncated results include the next byte offset."
    }

    fn prepare_typed(
        &self,
        params: ReadParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall::new(
            self.name(),
            Op::Read {
                path: resolve(ctx, &params.path)?,
            },
        )
        .with_read_offset(params.offset))
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
            let offset = call.read_offset();
            let Op::Read { path } = only_op(call)? else {
                return Err(wrong_op(self.name()));
            };
            // `io.read`, never an `Fs`: the path is re-resolved and rechecked
            // against the permit here, so a symlink swapped in since `prepare`
            // is judged as it stands now.
            let bytes = io.read(&path)?;
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let mut end = start.saturating_add(32 * 1024).min(bytes.len());
            if let Some(last) = bytes[start..end]
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .nth(1999)
            {
                end = start + last.0 + 1;
            }
            // Do not split a valid UTF-8 character at the byte cap.
            while end > start && end < bytes.len() && (bytes[end] & 0xc0) == 0x80 {
                end -= 1;
            }
            let mut text = String::from_utf8_lossy(&bytes[start..end]).into_owned();
            if end < bytes.len() {
                text.push_str(&format!(
                    "\n[Read truncated; continue with read path={:?} offset={end}]",
                    path.as_path()
                ));
            }
            Ok(ToolResult::builtin(text))
        })
    }
}
