//! `edit`: exact text replacement through the guarded seam.
//!
//! Editing requires both read and write authority for the same file.

use ego_core::permit::{
    Change, GuardedIo, LineChange, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool,
};
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{resolve, wrong_op};

const MAX_REPLACE_ALL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditParams {
    /// The file to edit. Absolute, or relative to the working directory. It
    /// must already exist.
    pub path: String,
    /// The exact text to replace. Must occur exactly once unless replace_all is true.
    pub old_string: String,
    /// The text to put in its place.
    pub new_string: String,
    /// Replace all original non-overlapping matches. Defaults to false (unique match only).
    #[serde(default)]
    pub replace_all: bool,
}

/// Replaces exact text in one existing UTF-8 file. Bulk replacement is explicit.
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
        "Replace exact text in one existing UTF-8 file. By default old_string must occur exactly \
         once; include surrounding context to disambiguate. Set replace_all=true to replace all \
         original non-overlapping matches, not matches introduced by the replacement. Returns \
         actual occurrence and changed-line counts. Use write for new files or whole-file replacement. \
         In replace_all mode, input and output are limited to 16 MiB."
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
                replace_all: params.replace_all,
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
            let (path, old_string, new_string, replace_all) = self.decode(call)?;

            let bytes = if replace_all {
                io.read_bounded(&path, MAX_REPLACE_ALL_BYTES)?
            } else {
                io.read(&path)?
            };
            // `old_string` is a string, so the file has to be one. A lossy
            // decode would write back a file that differs everywhere the
            // decode guessed, at every byte the tool was never asked about.
            let before = String::from_utf8(bytes).map_err(|_| {
                ToolError::InvalidParams(format!(
                    "{path} is not UTF-8 text, and an edit replaces text"
                ))
            })?;

            let occurrences = before.matches(&old_string).count();
            match occurrences {
                1 => {}
                n if n > 1 && replace_all => {}
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

            if replace_all {
                // Original matches are disjoint, so their total length fits in before.
                // Check the expanded size before allocating the replacement string.
                let retained = before.len() - occurrences * old_string.len();
                let output_bytes = occurrences
                    .checked_mul(new_string.len())
                    .and_then(|inserted| retained.checked_add(inserted));
                if output_bytes.is_none_or(|bytes| bytes > MAX_REPLACE_ALL_BYTES) {
                    return Err(ToolError::InvalidParams(
                        "replace_all output exceeds 16 MiB; use a smaller substitution".into(),
                    ));
                }
            }
            let after = before.replace(&old_string, &new_string);
            let change = if replace_all {
                diff_lines(&before, &after)
            } else {
                changed_lines(&before, &after, &old_string, &new_string)
            };
            io.write(&path, after.as_bytes())?;
            Ok(ToolResult::builtin(format!(
                "replaced {occurrences} {} in {path} (+{} -{} lines){}",
                if occurrences == 1 {
                    "occurrence"
                } else {
                    "occurrences"
                },
                change.added,
                change.removed,
                io.write_notice(&path)
                    .map(|notice| format!("\n{notice}"))
                    .unwrap_or_default()
            ))
            .with_line_change(change))
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
    diff_lines(
        &before[line_start..before_end],
        &after[line_start..after_end],
    )
}

/// Count changed lines once even when several substitutions touch the same line.
fn diff_lines(before: &str, after: &str) -> LineChange {
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
    ) -> Result<(ego_core::permit::CanonPath, String, String, bool), ToolError> {
        let (_, ops) = call.into_parts();
        let mut ops = ops.into_iter();
        let (Some(Op::Read { path: read_path }), Some(Op::Write { path, change }), None) =
            (ops.next(), ops.next(), ops.next())
        else {
            return Err(wrong_op(self.name()));
        };
        let Change::Replace {
            replace_all,
            old_string,
            new_string,
        } = change
        else {
            return Err(wrong_op(self.name()));
        };
        if read_path != path {
            return Err(wrong_op(self.name()));
        }
        Ok((path, old_string, new_string, replace_all))
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
