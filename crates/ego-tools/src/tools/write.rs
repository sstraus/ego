//! `write`: whole-file replacements through the guarded seam.

use super::edit::{diff_lines, diff_summary};
use super::{resolve_target, wrong_op};
use ego_core::permit::{
    BEFORE_IMAGE_LIMIT, BeforeImage, Change, FileWriteEvidence, GuardedIo, Op, PrepareCtx,
    PreparedCall, ToolError, ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteParams {
    /// One to sixteen complete file replacements.
    #[schemars(length(min = 1, max = 16))]
    pub files: Vec<WriteFile>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFile {
    /// Absolute, or relative to the working directory.
    pub path: String,
    /// The complete replacement contents.
    pub contents: String,
}

pub struct WriteTool;
impl TypedTool for WriteTool {
    type Params = WriteParams;
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Replace 1 to 16 complete files in order. All operations require admission before writing; execution stops on the first error."
    }
    fn prepare_typed(
        &self,
        params: WriteParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        if !(1..=16).contains(&params.files.len()) {
            return Err(ToolError::InvalidParams(
                "files must contain 1 to 16 entries".into(),
            ));
        }
        let mut ops = Vec::new();
        let mut paths = Vec::new();
        let mut names = Vec::new();
        for file in params.files {
            let path = resolve_target(ctx, &file.path)?;
            if paths.contains(&path) {
                return Err(ToolError::InvalidParams(
                    "duplicate resolved write target".into(),
                ));
            }
            paths.push(path.clone());
            names.push(file.path);
            ops.push(Op::Write {
                path,
                change: Change::Whole(file.contents.into_bytes()),
            });
        }
        let mut ops = ops.into_iter();
        let mut call = PreparedCall::new(self.name(), ops.next().expect("nonempty"));
        for op in ops {
            call = call.with_op(op);
        }
        Ok(call.with_write_items(names))
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
            let names = call.write_items().to_vec();
            let (_, ops) = call.into_parts();
            if ops.is_empty() || ops.len() != names.len() {
                return Err(wrong_op(self.name()));
            }
            let mut items = Vec::new();
            let mut evidence = Vec::new();
            let mut failed = false;
            for (index, op) in ops.into_iter().enumerate() {
                let Op::Write {
                    path,
                    change: Change::Whole(contents),
                } = op
                else {
                    return Err(wrong_op(self.name()));
                };
                let name = &names[index];
                if failed {
                    items.push(json!({"index": index, "path": name, "status": "unattempted"}));
                    continue;
                }
                let attempt = io.write_attempt(&path, &contents);
                let summary = match &attempt.before_image {
                    Some(BeforeImage::New) => {
                        diff_summary(&diff_lines("", &String::from_utf8_lossy(&contents)))
                    }
                    Some(BeforeImage::Text(before)) => {
                        diff_summary(&diff_lines(before, &String::from_utf8_lossy(&contents)))
                    }
                    Some(BeforeImage::Omitted) => {
                        " (diff omitted: the replaced file is over 64 KiB, not text, or protected)"
                            .into()
                    }
                    None => String::new(),
                };
                let notice = if attempt.result.is_ok() || attempt.checkpoint.is_some() {
                    io.write_notice(&path)
                } else {
                    None
                };
                match &attempt.result {
                    Ok(()) => items.push(json!({"index": index, "path": name, "status": "completed", "bytes": contents.len(), "summary": summary, "notice": notice})),
                    Err(error) => { failed = true; items.push(json!({"index": index, "path": name, "status": "failed", "error": error.to_string(), "possibly_modified": attempt.possibly_modified, "summary": summary, "notice": notice})); }
                }
                let after_image = if attempt.result.is_ok()
                    && matches!(
                        attempt.before_image,
                        Some(BeforeImage::New | BeforeImage::Text(_))
                    )
                    && contents.len() <= BEFORE_IMAGE_LIMIT
                {
                    String::from_utf8(contents).ok()
                } else {
                    None
                };
                evidence.push(FileWriteEvidence {
                    path: path.to_string(),
                    completed: attempt.result.is_ok(),
                    before_image: attempt.before_image,
                    after_image,
                    checkpoint: attempt.checkpoint,
                    possibly_modified: attempt.possibly_modified,
                    summary,
                    notice,
                });
            }
            let result =
                ToolResult::builtin(json!({"files":items}).to_string()).with_file_writes(evidence);
            if failed {
                Err(ToolError::WriteFailed(Box::new(result)))
            } else {
                Ok(result)
            }
        })
    }
}
