//! `edit`: exact text replacement through the guarded seam.
//!
//! Editing requires both read and write authority for the same file.

use ego_core::permit::{
    BEFORE_IMAGE_LIMIT, BeforeImage, Change, GuardedIo, LineChange, Op, PrepareCtx, PreparedCall,
    ToolError, ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{resolve, wrong_op};

const MAX_REPLACE_ALL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditParams {
    /// One to sixteen existing files.
    #[schemars(length(min = 1, max = 16))]
    pub files: Vec<EditFile>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditFile {
    /// Original requested path.
    pub path: String,
    /// Exact replacements against the original file.
    #[schemars(length(min = 1, max = 16))]
    pub chunks: Vec<EditChunk>,
    /// Replace all original non-overlapping matches of the single chunk.
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditChunk {
    pub old_string: String,
    pub new_string: String,
}

pub struct EditTool;

impl TypedTool for EditTool {
    type Params = EditParams;
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit 1 to 16 existing UTF-8 files in order using exact original-text chunks. All operations require admission before writing; execution stops on the first error. replace_all=true requires one chunk, replaces original non-overlapping matches, and limits input and output to 16 MiB."
    }
    fn prepare_typed(
        &self,
        params: EditParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        if !(1..=16).contains(&params.files.len()) {
            return Err(ToolError::InvalidParams(
                "files must contain 1 to 16 entries".into(),
            ));
        }
        let mut paths = Vec::new();
        let mut names = Vec::new();
        let mut ops = Vec::new();
        for file in params.files {
            if !(1..=16).contains(&file.chunks.len())
                || (file.replace_all && file.chunks.len() != 1)
            {
                return Err(ToolError::InvalidParams(
                    "chunks must contain 1 to 16 entries; replace_all requires exactly one".into(),
                ));
            }
            let mut chunks = Vec::new();
            for chunk in file.chunks {
                if chunk.old_string.is_empty() || chunk.old_string == chunk.new_string {
                    return Err(ToolError::InvalidParams(
                        "old_string must be nonempty and differ from new_string".into(),
                    ));
                }
                chunks.push(ego_core::permit::ReplaceChunk {
                    old_string: chunk.old_string,
                    new_string: chunk.new_string,
                });
            }
            let path = resolve(ctx, &file.path)?;
            if paths.contains(&path) {
                return Err(ToolError::InvalidParams(
                    "duplicate resolved edit target".into(),
                ));
            }
            paths.push(path.clone());
            names.push(file.path);
            ops.push(Op::Read { path: path.clone() });
            ops.push(Op::Write {
                path,
                change: Change::Replace {
                    chunks,
                    replace_all: file.replace_all,
                },
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
            if names.is_empty() || ops.len() != names.len() * 2 {
                return Err(wrong_op(self.name()));
            }
            let mut decoded = Vec::new();
            let mut ops = ops.into_iter();
            while let (Some(read), Some(write)) = (ops.next(), ops.next()) {
                let (
                    Op::Read { path: read_path },
                    Op::Write {
                        path,
                        change:
                            Change::Replace {
                                chunks,
                                replace_all,
                            },
                    },
                ) = (read, write)
                else {
                    return Err(wrong_op(self.name()));
                };
                if path != read_path {
                    return Err(wrong_op(self.name()));
                }
                decoded.push((path, chunks, replace_all));
            }
            let mut items = Vec::new();
            let mut evidence = Vec::new();
            let mut failed = false;
            for (index, (path, chunks, replace_all)) in decoded.into_iter().enumerate() {
                let name = &names[index];
                if failed {
                    items.push(
                        serde_json::json!({"index": index, "path": name, "status": "unattempted"}),
                    );
                    continue;
                }
                let replacement = (|| -> Result<_, ToolError> {
                    let bytes = if replace_all {
                        io.read_bounded(&path, MAX_REPLACE_ALL_BYTES)?
                    } else {
                        io.read(&path)?
                    };
                    let before = String::from_utf8(bytes).map_err(|_| {
                        ToolError::InvalidParams(format!("{path} is not UTF-8 text"))
                    })?;
                    let (after, occurrences) =
                        apply_chunks(&before, &chunks, replace_all, &path.to_string())?;
                    Ok((before, after, occurrences))
                })();
                let (before, after, occurrences) = match replacement {
                    Ok(value) => value,
                    Err(error) => {
                        failed = true;
                        items.push(serde_json::json!({"index": index, "path": name, "status": "failed", "error": error.to_string(), "possibly_modified": false}));
                        continue;
                    }
                };
                let summary = diff_summary(&diff_lines(&before, &after));
                let attempt = io.write_attempt(&path, after.as_bytes());
                let notice = if attempt.result.is_ok() || attempt.checkpoint.is_some() {
                    io.write_notice(&path)
                } else {
                    None
                };
                match &attempt.result {
                    Ok(()) => items.push(serde_json::json!({"index": index, "path": name, "status": "completed", "occurrences": occurrences, "summary": summary, "notice": notice})),
                    Err(error) => { failed = true; items.push(serde_json::json!({"index": index, "path": name, "status": "failed", "error": error.to_string(), "possibly_modified": attempt.possibly_modified, "summary": summary, "notice": notice})); }
                }
                let after_image = if attempt.result.is_ok()
                    && matches!(
                        attempt.before_image,
                        Some(BeforeImage::New | BeforeImage::Text(_))
                    )
                    && after.len() <= BEFORE_IMAGE_LIMIT
                {
                    Some(after)
                } else {
                    None
                };
                evidence.push(ego_core::permit::FileWriteEvidence {
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
            let result = ToolResult::builtin(serde_json::json!({"files": items}).to_string())
                .with_file_writes(evidence);
            if failed {
                Err(ToolError::WriteFailed(Box::new(result)))
            } else {
                Ok(result)
            }
        })
    }
}

fn apply_chunks(
    before: &str,
    chunks: &[ego_core::permit::ReplaceChunk],
    replace_all: bool,
    path: &str,
) -> Result<(String, usize), ToolError> {
    if replace_all {
        let chunk = &chunks[0];
        let count = before.matches(&chunk.old_string).count();
        if count == 0 {
            return Err(ToolError::InvalidParams(format!(
                "that text is not in {path}"
            )));
        }
        let retained = before.len() - count * chunk.old_string.len();
        if count
            .checked_mul(chunk.new_string.len())
            .and_then(|n| retained.checked_add(n))
            .is_none_or(|n| n > MAX_REPLACE_ALL_BYTES)
        {
            return Err(ToolError::InvalidParams(
                "replace_all output exceeds 16 MiB; use a smaller substitution".into(),
            ));
        }
        return Ok((before.replace(&chunk.old_string, &chunk.new_string), count));
    }
    let mut spans = Vec::new();
    for chunk in chunks {
        let mut matches = before
            .char_indices()
            .filter_map(|(i, _)| before[i..].starts_with(&chunk.old_string).then_some(i));
        let Some(start) = matches.next() else {
            return Err(ToolError::InvalidParams(format!(
                "that text is not in {path}"
            )));
        };
        let count = 1 + matches.count();
        if count > 1 {
            return Err(ToolError::InvalidParams(format!(
                "that text appears {count} times in {path}"
            )));
        }
        spans.push((start, start + chunk.old_string.len(), &chunk.new_string));
    }
    spans.sort_by_key(|span| span.0);
    let mut after = String::new();
    let mut end = 0;
    for (start, next, replacement) in spans {
        if start < end {
            return Err(ToolError::InvalidParams(format!(
                "replacement chunks overlap in {path}"
            )));
        }
        after.push_str(&before[end..start]);
        after.push_str(replacement);
        end = next;
    }
    after.push_str(&before[end..]);
    Ok((after, chunks.len()))
}

/// Count changed lines once even when several substitutions touch the same line.
pub(super) fn diff_lines(before: &str, after: &str) -> LineChange {
    let mut change = LineChange {
        added: 0,
        removed: 0,
    };
    for line in similar::TextDiff::from_lines(before, after).iter_all_changes() {
        match line.tag() {
            similar::ChangeTag::Insert => change.added += 1,
            similar::ChangeTag::Delete => change.removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    change
}

/// ` (+A -R lines)`, the receipt tail the terminal row reads its counts from.
pub(super) fn diff_summary(change: &LineChange) -> String {
    format!(" (+{} -{} lines)", change.added, change.removed)
}
