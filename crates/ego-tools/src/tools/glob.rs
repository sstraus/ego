//! `glob`: names under a root, and nothing else about them.

use ego_core::permit::{
    EntryKind, GuardedIo, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use globset::{Glob, GlobBuilder};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{note_truncation, only_op, root_of, scan_of, wrong_op};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobParams {
    /// A glob pattern, matched against each file's path relative to the
    /// search root — `**/*.rs`, `src/*.toml`.
    pub pattern: String,
    /// The directory to search. Defaults to the working directory.
    #[serde(default)]
    pub path: Option<String>,
}

/// Finds files by name. Holds `list` authority and no `read` authority, so it
/// cannot read what it found even if it wanted to
/// (`docs/decisions/2026-08-21-reading-without-reading-everything.md` §1).
pub struct GlobTool;

impl TypedTool for GlobTool {
    type Params = GlobParams;

    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files whose path matches a glob pattern. Returns paths relative to the search root, one per line; it does not read the files."
    }

    fn prepare_typed(
        &self,
        params: GlobParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        // Compiled here and thrown away. A pattern that does not parse is a
        // fact about the model's arguments, and saying so before anything is
        // authorised beats minting a permit for a call that cannot run.
        compile(&params.pattern)?;
        Ok(PreparedCall::new(
            self.name(),
            vec![Op::List {
                root: root_of(ctx, params.path.as_ref())?,
                pattern: params.pattern,
            }],
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
            let Op::List { root, pattern } = only_op(call)? else {
                return Err(wrong_op(self.name()));
            };
            let matcher = compile(&pattern)?.compile_matcher();
            let scan = io.list(&scan_of(root.clone()))?;

            // Files only. A glob pattern is how a model asks "which files",
            // and a directory that happens to match `**/*.rs` is not one of
            // them. The relative path is what the pattern is written against;
            // the canonical one is what the permit was checked against, and
            // they are not the same thing.
            let names: Vec<String> = scan
                .entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::File)
                .filter(|entry| matcher.is_match(&entry.relative))
                .map(|entry| entry.relative.display().to_string())
                .collect();

            let mut content = if names.is_empty() {
                format!("no files under {root} match {pattern}")
            } else {
                names.join("\n")
            };
            note_truncation(&mut content, scan.truncated);
            Ok(ToolResult::builtin(content))
        })
    }
}

/// One place that turns a pattern into a matcher, so `prepare`'s validation
/// and `execute`'s use can never disagree about what compiles.
///
/// `literal_separator(true)` and not `globset`'s default: without it `*`
/// crosses `/`, so `src/*.rs` quietly matches `src/deep/lib.rs`. Every glob a
/// model has written before — a shell's, `.gitignore`'s, ripgrep's — draws the
/// line at the separator and spells the other thing `**`, and a tool that
/// silently means something wider than the pattern says is a tool whose
/// results cannot be reasoned about.
fn compile(pattern: &str) -> Result<Glob, ToolError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|err| ToolError::InvalidParams(err.to_string()))
}
