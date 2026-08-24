//! The builtins: `read`, `glob`, `grep`, `write`, `edit`, `exec`, and
//! `read_tool_result`.
//!
//! See `docs/decisions/2026-08-21-reading-without-reading-everything.md` and
//! `docs/decisions/2026-08-21-three-tools-on-a-real-filesystem.md`. Each
//! prepares a *different* operation, because listing, searching and reading
//! are three different authorities: a `glob` that prepared `Op::Read` over its
//! root would be handed the right to read every byte under it, having asked
//! only for names.
//!
//! `write` is the fourth and it is not one more of the same. It is the only
//! one that produces a *mutating* op, and until it existed `ModeApprover`
//! approved every call this workspace could make in every mode — so the five
//! modes, the `plan` floor and every rule with a `write(...)` head were
//! machinery with no reachable input (`docs/13-effect-tools.md` §0).
//!
//! `edit` is the fifth and it is also not one more of the same. It is the only
//! one that prepares *two* operations for one call — a `Read` and a `Write` of
//! the same file, because it genuinely performs both — so it is the first call
//! in this tree that can observe that `classify` and `scope_of` treat a call's
//! ops as a conjunction. Both were written that way for it and neither had ever
//! been exercised.
//!
//! None of the filesystem tools holds an `Fs`. They receive a `GuardedIo`,
//! which re-resolves and re-checks every path before touching it. `exec` and
//! `read_tool_result` cross their own guarded ports instead: `GuardedExec` and
//! the session evidence store.

mod edit;
mod exec;
mod glob;
mod grep;
mod read;
mod read_tool_result;
mod write;

pub use edit::{EditParams, EditTool};
pub use exec::{ExecParams, ExecTool};
pub use glob::{GlobParams, GlobTool};
pub use grep::{GrepParams, GrepTool};
pub use read::{ReadParams, ReadTool};
pub use read_tool_result::{ReadToolResult, ReadToolResultParams};
pub use write::{WriteParams, WriteTool};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ego_core::permit::{
    CanonPath, Op, PrepareCtx, PreparedCall, ScanLimits, ScanRequest, Tool, ToolError, Typed,
};

/// Directory names a builtin traversal does not descend into.
///
/// A usability default and **not a security boundary**: a path inside one of
/// these is refused by the permit exactly like any other, and this list is
/// never consulted when deciding whether something is in scope.
pub const SKIP_DIRS: [&str; 3] = [".git", "target", "node_modules"];

/// The seven, ready to hand to `Tools::new`.
pub fn builtins() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Typed(ReadTool)),
        Arc::new(Typed(GlobTool)),
        Arc::new(Typed(GrepTool)),
        Arc::new(Typed(WriteTool)),
        Arc::new(Typed(EditTool)),
        Arc::new(Typed(ExecTool)),
        Arc::new(Typed(ReadToolResult)),
    ]
}

/// Resolve a path the model named, relative to the session's working
/// directory, into what it points at *now*.
///
/// Fallible on purpose: a model that names a file which is not there gets
/// `ToolError::Fs(NotFound)` out of `prepare`, before there is anything to
/// authorise. That is a better answer than a permit for a path that does not
/// exist.
fn resolve(ctx: &PrepareCtx<'_>, path: &str) -> Result<CanonPath, ToolError> {
    let named = Path::new(path);
    let joined: PathBuf = if named.is_absolute() {
        named.to_path_buf()
    } else {
        ctx.cwd.as_path().join(named)
    };
    Ok(ctx.resolver.canonicalize(&joined)?)
}

/// Resolve a path a tool is about to *create or replace*.
///
/// [`resolve`] is right for every tool that reads, and its failure on a
/// missing file is a feature there. A write inverts it: the ordinary case is
/// a file that is not on disk yet, so the parent is resolved and the final
/// component appended, and only a missing *parent* is an error.
///
/// The distinction is the same one [`GuardedFs::write_target`] draws at
/// execute time, and it is drawn twice on purpose rather than shared: this
/// one runs before any permit exists and produces the path policy will
/// *judge*, and that one runs after and produces the path the bytes will
/// *reach*. A single helper would have to belong to one of those two moments,
/// and the seam re-deriving the target independently of what a tool claimed
/// is the whole reason the seam is there.
fn resolve_target(ctx: &PrepareCtx<'_>, path: &str) -> Result<CanonPath, ToolError> {
    let named = Path::new(path);
    let joined: PathBuf = if named.is_absolute() {
        named.to_path_buf()
    } else {
        ctx.cwd.as_path().join(named)
    };
    // Already there: resolve it whole, so a symlinked target is judged as
    // the file the bytes would actually reach.
    if let Ok(existing) = ctx.resolver.canonicalize(&joined) {
        return Ok(existing);
    }
    let (Some(parent), Some(name)) = (joined.parent(), joined.file_name()) else {
        return Err(ToolError::InvalidParams(format!(
            "{path} does not name a file"
        )));
    };
    Ok(CanonPath::new(
        ctx.resolver.canonicalize(parent)?.as_path().join(name),
    ))
}

/// The root a traversal tool was pointed at, defaulting to the working
/// directory.
fn root_of(ctx: &PrepareCtx<'_>, path: Option<&String>) -> Result<CanonPath, ToolError> {
    match path {
        Some(path) => resolve(ctx, path),
        None => Ok(ctx.cwd.clone()),
    }
}

/// A traversal of `root` with the builtin skips and the default limits.
fn scan_of(root: CanonPath) -> ScanRequest {
    ScanRequest {
        root,
        skip_dirs: SKIP_DIRS.iter().map(|name| name.to_string()).collect(),
        limits: ScanLimits::default(),
    }
}

/// The one operation this call was prepared with.
///
/// A tool reads its arguments back out of the `PreparedCall` rather than
/// keeping the params it was handed, because the call is what policy saw and
/// authorised. That is what makes the query bound into `Op::Search` the query
/// that actually runs.
fn only_op(call: PreparedCall) -> Result<Op, ToolError> {
    let tool = call.tool;
    let mut ops = call.ops.into_iter();
    match (ops.next(), ops.next()) {
        (Some(op), None) => Ok(op),
        _ => Err(wrong_op(&tool)),
    }
}

/// A `PreparedCall` that does not carry what its tool prepared.
///
/// Unreachable unless `prepare` and `execute` disagree, which would be a
/// defect in the tool rather than in the model's arguments — but it is
/// reported rather than panicked on, because a tool must not be able to take
/// the process down.
fn wrong_op(tool: &str) -> ToolError {
    ToolError::InvalidParams(format!(
        "{tool} was asked to execute an operation it did not prepare"
    ))
}

/// Append the note that says an answer is incomplete.
///
/// Every traversal tool needs it and none of them may leave it out: a model
/// that cannot tell a complete answer from a truncated one concludes the file
/// it needed does not exist.
fn note_truncation(content: &mut String, truncated: bool) {
    if truncated {
        content.push_str("\n\n(stopped at a traversal limit: this answer is incomplete)");
    }
}
