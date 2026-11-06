//! `read`: named files, through the guarded seam.

use ego_core::permit::{GuardedIo, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::read_pdf;
use super::{resolve, wrong_op};

const LIMIT: usize = 32 * 1024;
const ERROR_RESERVE: usize = 512;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// One to sixteen named files, in result order.
    #[schemars(length(min = 1, max = 16))]
    pub files: Vec<ReadFile>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFile {
    /// Absolute, or relative to the working directory.
    pub path: String,
    /// Original-byte offset, or extracted-text byte offset for PDF files.
    #[serde(default)]
    pub offset: u64,
}

pub struct ReadTool;

impl TypedTool for ReadTool {
    type Params = ReadParams;

    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read 1 to 16 named files. Results are ordered JSON, bounded to 32 KiB in total and 2000 lines per file. Continue a truncated item using its next_offset; PDF offsets count extracted-text bytes."
    }

    fn prepare_typed(
        &self,
        params: ReadParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        if !(1..=16).contains(&params.files.len()) {
            return Err(ToolError::InvalidParams(
                "files must contain 1 to 16 entries".into(),
            ));
        }
        // Reserve metadata and bounded error text for every item before effects.
        let metadata = json!({"files": params.files.iter().enumerate().map(|(index, f)| json!({"index": index, "path": f.path, "status": "error", "error": "x".repeat(ERROR_RESERVE), "content": "", "next_offset": f.offset})).collect::<Vec<_>>()});
        if serde_json::to_vec(&metadata).expect("JSON value").len() > LIMIT {
            return Err(ToolError::InvalidParams(
                "file metadata exceeds read result budget".into(),
            ));
        }
        let mut paths = Vec::new();
        let mut names = Vec::new();
        for file in params.files {
            let path = resolve(ctx, &file.path)?;
            if paths.contains(&path) {
                return Err(ToolError::InvalidParams(
                    "duplicate resolved read target".into(),
                ));
            }
            paths.push(path);
            names.push((file.path, file.offset));
        }
        let mut paths = paths.into_iter();
        let mut call = PreparedCall::new(
            self.name(),
            Op::Read {
                path: paths.next().expect("nonempty"),
            },
        );
        for path in paths {
            call = call.with_op(Op::Read { path });
        }
        Ok(call.with_read_items(names))
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
            let names = call.read_items().to_vec();
            let (_, ops) = call.into_parts();
            if names.len() != ops.len() || names.is_empty() {
                return Err(wrong_op(self.name()));
            }
            let mut items: Vec<Value> = names.iter().enumerate().map(|(index, (path, offset))| json!({"index":index,"path":path,"status":"pending","content":"","next_offset":offset})).collect();
            for (index, op) in ops.into_iter().enumerate() {
                let Op::Read { path } = op else {
                    return Err(wrong_op(self.name()));
                };
                let read = io.read(&path).map_err(ToolError::from).and_then(|bytes| {
                    if read_pdf::is_pdf(&bytes) {
                        let text = std::panic::catch_unwind(|| read_pdf::extract_text(&bytes))
                            .map_err(|_| {
                            ToolError::InvalidParams("cannot read PDF: parser failed".into())
                        })??;
                        Ok(text.into_bytes())
                    } else {
                        Ok(bytes)
                    }
                });
                match read {
                    Err(err) => {
                        let message = err.to_string();
                        let detail = if message.len() > ERROR_RESERVE / 6 {
                            let mut start = message.len() - ERROR_RESERVE / 6;
                            while !message.is_char_boundary(start) {
                                start += 1;
                            }
                            format!("…{}", &message[start..])
                        } else {
                            message
                        };
                        items[index] = json!({"index":index,"path":names[index].0,"status":"error","error":detail});
                    }
                    Ok(bytes) => {
                        let start = usize::try_from(names[index].1)
                            .unwrap_or(usize::MAX)
                            .min(bytes.len());
                        let mut max = start.saturating_add(LIMIT).min(bytes.len());
                        if let Some((line, _)) = bytes[start..max]
                            .iter()
                            .enumerate()
                            .filter(|(_, b)| **b == b'\n')
                            .nth(1999)
                        {
                            max = start + line + 1;
                        }
                        // Reserve the worst-case bounded error for each unprocessed item.
                        let reserve = (items.len() - index - 1) * ERROR_RESERVE;
                        let mut low = start;
                        let mut high = max;
                        while low < high {
                            let mid = low + (high - low).div_ceil(2);
                            items[index] = json!({"index":index,"path":names[index].0,"status":"ok","content":String::from_utf8_lossy(&bytes[start..mid]),"next_offset":mid});
                            if serde_json::to_vec(&json!({"files":items}))
                                .expect("JSON value")
                                .len()
                                + reserve
                                <= LIMIT
                            {
                                low = mid;
                            } else {
                                high = mid - 1;
                            }
                        }
                        let mut end = low;
                        if end < bytes.len() && (bytes[end] & 0xc0) == 0x80 {
                            for lead in (end.saturating_sub(3).max(start)..end).rev() {
                                let width = match bytes[lead] {
                                    0xc2..=0xdf => 2,
                                    0xe0..=0xef => 3,
                                    0xf0..=0xf4 => 4,
                                    _ => continue,
                                };
                                if lead + width > end
                                    && lead + width <= bytes.len()
                                    && std::str::from_utf8(&bytes[lead..lead + width]).is_ok()
                                {
                                    end = lead;
                                }
                                break;
                            }
                        }
                        items[index] = json!({"index":index,"path":names[index].0,"status":"ok","content":String::from_utf8_lossy(&bytes[start..end]),"next_offset":if end < bytes.len() { Some(end as u64) } else { None }});
                    }
                }
            }
            Ok(ToolResult::builtin(
                serde_json::to_string(&json!({"files":items})).expect("JSON value"),
            ))
        })
    }
}
