//! `grep`: the lines under a root that match, without the files they came
//! from ever reaching the tool.

use ego_core::permit::{GuardedIo, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool};
use futures::future::BoxFuture;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{note_truncation, only_op, root_of, scan_of, wrong_op};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepParams {
    /// A regular expression, matched against each line.
    pub query: String,
    /// The file or directory to search. Defaults to the working directory.
    #[serde(default)]
    pub path: Option<String>,
}

/// Searches file contents. Holds `search` authority and no `read` authority:
/// the guarded seam opens the files and this tool only supplies the
/// judgement, so a grep approved to look for `TODO` cannot go read `.env`
/// instead (`docs/architecture.md`
/// §2).
pub struct GrepTool;

impl TypedTool for GrepTool {
    type Params = GrepParams;

    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a line-oriented regular expression; use glob for path names and read for surrounding content. Returns path:line:text for matching lines. Respects ignore rules and traversal limits; an incomplete result is not proof of absence. Narrow the directory or query when truncated."
    }

    fn prepare_typed(
        &self,
        params: GrepParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        // See `glob`: a query that does not compile is the model's mistake and
        // is worth reporting before a permit is minted for it.
        compile(&params.query)?;
        Ok(PreparedCall::new(
            self.name(),
            Op::Search {
                root: root_of(ctx, params.path.as_ref())?,
                query: params.query,
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
            let Op::Search { root, query } = only_op(call)? else {
                return Err(wrong_op(self.name()));
            };
            let regex = compile(&query)?;
            // The matcher is the whole of this tool's contribution: the seam
            // reads, this judges, and only the judgements come back.
            let matcher = move |line: &str| regex.is_match(line);
            let found = io.search(&scan_of(root.clone()), &matcher)?;

            let lines: Vec<String> = found
                .matches
                .iter()
                .map(|hit| {
                    // Relative where it can be, absolute where it cannot: a
                    // hit in a file symlinked out of the root has no relative
                    // name under it, and showing one would hide where the
                    // content actually lives.
                    let path = hit
                        .path
                        .as_path()
                        .strip_prefix(root.as_path())
                        .ok()
                        .filter(|path| !path.as_os_str().is_empty())
                        .unwrap_or_else(|| {
                            if hit.path == root {
                                hit.path
                                    .as_path()
                                    .file_name()
                                    .map(std::path::Path::new)
                                    .unwrap_or(hit.path.as_path())
                            } else {
                                hit.path.as_path()
                            }
                        });
                    format!("{}:{}:{}", path.display(), hit.line, hit.text)
                })
                .collect();

            let mut content = if lines.is_empty() {
                format!("no line under {root} matches {query}")
            } else {
                lines.join("\n")
            };
            note_truncation(&mut content, found.truncated);
            Ok(ToolResult::builtin(content))
        })
    }
}

/// One place that turns a query into a regex, so `prepare`'s validation and
/// `execute`'s use can never disagree about what compiles.
fn compile(query: &str) -> Result<Regex, ToolError> {
    Regex::new(query).map_err(|err| ToolError::InvalidParams(err.to_string()))
}
