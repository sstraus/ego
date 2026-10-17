//! `edit`: one occurrence of one string, replaced, through the guarded seam.
//!
//! The first tool in the tree that needs **two** authorities for one call. See
//! `ARCHITECTURE.md`.

use ego_core::permit::{
    Change, GuardedIo, LineChange, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{resolve, wrong_op};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditParams {
    /// The file to edit. Absolute, or relative to the working directory. It
    /// must already exist.
    pub path: String,
    /// The exact text to replace. It must appear in the file exactly once —
    /// include the surrounding lines if it does not.
    pub old_string: String,
    /// The text to put in its place.
    pub new_string: String,
}

/// Replaces one occurrence of a string in one file.
///
/// **Exact, and exactly one.** No fuzzy fallback, no `replace_all`, no array of
/// edits. Every shipped agent has at least one of those; none of them is needed
/// to make an edit land, and each is a decision of its own — `replace_all` in
/// particular turns one reviewed substitution into an unbounded number of them.
///
/// Unlike [`WriteTool`](super::WriteTool), the bytes this produces are not
/// knowable from the call alone: they depend on the file found at execute time.
/// That is not a shortcut. `prepare` runs before a `Permit` exists, so a
/// `prepare` that read the file would read it whether or not policy went on to
/// allow it — and with `old_string` matching as an oracle, that leaks the
/// contents of files the model may not read. So the operation says *what to
/// change* and the substitution happens under the permit.
pub struct EditTool;

impl TypedTool for EditTool {
    type Params = EditParams;

    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace one exact occurrence of a string in a file. The old text must appear exactly \
         once — if it appears more than once, include surrounding lines until it is unique. \
         The file must already exist. Use `write` to create a file or to replace all of it."
    }

    fn prepare_typed(
        &self,
        params: EditParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        // Rejected here rather than at execute, because neither is a fact
        // about the file: both are true of the call whatever it is pointed at,
        // and an operation nothing could satisfy should not reach policy.
        if params.old_string.is_empty() {
            return Err(ToolError::InvalidParams(
                "old_string is empty, and the empty string is at every position in every file"
                    .to_string(),
            ));
        }
        if params.old_string == params.new_string {
            return Err(ToolError::InvalidParams(
                "old_string and new_string are the same, so this edit would change nothing"
                    .to_string(),
            ));
        }
        // `resolve`, not `resolve_target`: `write`'s leniency about a missing
        // file exists because creating one is most of what a write is for. An
        // edit has nothing to substitute into, so resolving through the parent
        // would only defer the failure to a read that cannot succeed.
        let path = resolve(ctx, &params.path)?;
        Ok(PreparedCall::new(
            self.name(),
            // Both effects remain declared and guarded. A matching write
            // allow covers this same-file replacement's supporting read;
            // explicit read denies still refuse the call.
            Op::Read { path: path.clone() },
        )
        .with_op(Op::Write {
            path,
            change: Change::Replace {
                old_string: params.old_string,
                new_string: params.new_string,
            },
        }))
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
            let (path, old_string, new_string) = self.decode(call)?;

            let bytes = io.read(&path)?;
            // `old_string` is a string, so the file has to be one. A lossy
            // decode would write back a file that differs everywhere the
            // decode guessed, at every byte the tool was never asked about.
            let before = String::from_utf8(bytes).map_err(|_| {
                ToolError::InvalidParams(format!(
                    "{path} is not UTF-8 text, and an edit replaces text"
                ))
            })?;

            match before.matches(&old_string).count() {
                1 => {}
                0 => {
                    return Err(ToolError::InvalidParams(format!(
                        "that text is not in {path}"
                    )));
                }
                n => {
                    // The count, so the model can widen `old_string` rather
                    // than guess. Choosing the first would be choosing a place
                    // the call never named.
                    return Err(ToolError::InvalidParams(format!(
                        "that text appears {n} times in {path}: include the surrounding lines \
                         until it appears once"
                    )));
                }
            }

            let after = before.replace(&old_string, &new_string);
            let change = changed_lines(&before, &after, &old_string, &new_string);
            io.write(&path, after.as_bytes())?;
            Ok(
                ToolResult::builtin(format!("replaced 1 occurrence in {path}"))
                    .with_line_change(change),
            )
        })
    }
}

/// Diff only the lines touched by the one replacement, not the whole file.
fn changed_lines(before: &str, after: &str, old: &str, new: &str) -> LineChange {
    let start = before
        .find(old)
        .expect("the unique old text was just verified");
    let line_start = before[..start].rfind('\n').map_or(0, |index| index + 1);
    let old_end = start + old.len();
    let new_end = start + new.len();
    let before_end = before[old_end..]
        .find('\n')
        .map_or(before.len(), |index| old_end + index + 1);
    let after_end = after[new_end..]
        .find('\n')
        .map_or(after.len(), |index| new_end + index + 1);
    let mut change = LineChange {
        added: 0,
        removed: 0,
    };
    for line in similar::TextDiff::from_lines(
        &before[line_start..before_end],
        &after[line_start..after_end],
    )
    .iter_all_changes()
    {
        match line.tag() {
            similar::ChangeTag::Insert => change.added += 1,
            similar::ChangeTag::Delete => change.removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    change
}

impl EditTool {
    /// The two operations this tool prepares, in the order it prepared them.
    ///
    /// Its own decoder rather than a general "take what you find" accessor —
    /// `only_op` stays single-op — because a decoder that accepts any shape is
    /// a tool that cannot notice `prepare` and `execute` disagreeing. The
    /// `Read`'s path is checked against the `Write`'s: two ops naming two
    /// files would be a call authorised to read one thing and write another.
    fn decode(
        &self,
        call: PreparedCall,
    ) -> Result<(ego_core::permit::CanonPath, String, String), ToolError> {
        let (_, ops) = call.into_parts();
        let mut ops = ops.into_iter();
        let (Some(Op::Read { path: read_path }), Some(Op::Write { path, change }), None) =
            (ops.next(), ops.next(), ops.next())
        else {
            return Err(wrong_op(self.name()));
        };
        let Change::Replace {
            old_string,
            new_string,
        } = change
        else {
            return Err(wrong_op(self.name()));
        };
        if read_path != path {
            return Err(wrong_op(self.name()));
        }
        Ok((path, old_string, new_string))
    }
}

#[cfg(test)]
mod line_change_tests {
    use super::*;

    #[test]
    fn counts_only_changed_lines_and_preserves_matching_context() {
        assert_eq!(
            changed_lines("a\nb\nz\n", "a\nx\ny\nz\n", "b", "x\ny"),
            LineChange {
                added: 2,
                removed: 1
            }
        );
        assert_eq!(
            changed_lines("a\nsame\nb\n", "A\nsame\nB\n", "a\nsame\nb", "A\nsame\nB"),
            LineChange {
                added: 2,
                removed: 2
            }
        );
        assert_eq!(
            changed_lines("a\n", "a", "a\n", "a"),
            LineChange {
                added: 1,
                removed: 1
            }
        );
        assert_eq!(
            changed_lines(
                "prefixoldtail\nnext\n",
                "prefixnewtail\nnext\n",
                "old",
                "new"
            ),
            LineChange {
                added: 1,
                removed: 1
            }
        );
        assert_eq!(
            changed_lines("a\nb\n", "a\nb\n", "b", "b"),
            LineChange {
                added: 0,
                removed: 0
            }
        );
    }
}
