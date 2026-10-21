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
        "Read text from one file, at most 2000 lines or 32 KiB. offset is a zero-based byte \
         offset, not a line number; omit it to start at zero. For truncated output, continue \
         with the exact next offset returned. Use glob to find paths and grep to find matching \
         lines. This tool does not decode images or PDFs."
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
            // Retreat only for a valid UTF-8 character crossing the byte cap.
            if end < bytes.len() && (bytes[end] & 0xc0) == 0x80 {
                for lead in (end.saturating_sub(3).max(start)..end).rev() {
                    let width = match bytes[lead] {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => continue,
                    };
                    let char_end = lead + width;
                    if char_end > end
                        && char_end <= bytes.len()
                        && std::str::from_utf8(&bytes[lead..char_end]).is_ok()
                    {
                        end = lead;
                    }
                    break;
                }
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
