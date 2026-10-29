//! One tool call, as the transcript shows it: a compact row, and the full
//! detail behind the expand key.
//!
//! The row is what `PublicEvent` already carries: the tool name and its
//! argument JSON at `ItemStarted`, the outcome and the result text at
//! `ItemCompleted`. Nothing here reaches back into a tool, so a foreign
//! (MCP) tool gets the generic row and a builtin gets a specialised one from
//! the same two strings.
//!
//! Tool output is data from outside the trust boundary. Every line drawn from
//! it passes through [`printable`], because a result holding `ESC` would
//! otherwise be an instruction to the terminal.

use std::time::Duration;

use serde_json::Value;

use super::caps::{Capabilities, Depth};
use super::diff::{self, Unhighlighted};
use super::line::{Line, Span};
use super::style::{Palette, Style};
use similar::{ChangeTag, TextDiff};
use unicode_segmentation::UnicodeSegmentation;

/// Output lines an exec row shows under its header.
pub(crate) const PREVIEW_LINES: usize = 3;

/// Most bytes of one call's detail kept for the expand key. A result larger
/// than this is cut with a visible mark: the store is a bounded log of recent
/// calls, not a second copy of the evidence store.
pub const DETAIL_LIMIT: usize = 256 * 1024;

/// The exec preamble of a failed result: `ProcessError`'s `Display`, appended
/// by `into_failed_result` as the last block.
const EXIT_PREFIX: &str = "process exited unsuccessfully with code ";

/// What one finished call renders to.
pub struct ToolRecord {
    /// The row and its output tail, already at the terminal's depth and width,
    /// with no trailing newline.
    pub rows: String,
    /// The row followed by the whole result, for the expand key.
    pub detail: String,
    /// Only output-free top-level successful commands may be replaced live.
    pub shell_progress: Option<Line>,
}

/// One finished tool call.
pub struct Finished<'a> {
    pub name: &'a str,
    /// The call's argument JSON exactly as the model sent it.
    pub args: &'a str,
    pub ok: bool,
    /// The result text the model was given, or the failure message.
    pub result: &'a str,
    /// Authoritative blocks, when the reduced item or replay supplies them.
    pub result_blocks: Option<&'a [ego_core::permit::ToolContentBlock]>,
    pub timing: Option<ego_core::event::domain::ToolTiming>,
}

/// Presentation state for adjacent completed calls, shared by live output and replay.
#[derive(Default)]
pub struct ToolRows {
    task: Option<String>,
}

impl ToolRows {
    /// Prose, parent diagnostics or a new turn ends the current task group.
    pub fn separate(&mut self) {
        self.task = None;
    }

    pub fn record(&mut self, call: &Finished<'_>, capabilities: Capabilities) -> ToolRecord {
        let Some((task, name)) = call.name.rsplit_once(" › ") else {
            self.separate();
            return plain_record(call, capabilities);
        };
        let header = Line::plain(format!("  ↳ task {}", printable(task)))
            .truncate_to(capabilities.width)
            .render(capabilities.depth);
        let nested = plain_record(
            &Finished { name, ..*call },
            Capabilities {
                width: capabilities.width.saturating_sub(4).max(1),
                ..capabilities
            },
        );
        let indent = |text: &str| {
            text.lines()
                .map(|line| format!("    {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let rows = indent(&nested.rows);
        let rows = if self.task.as_deref() == Some(task) {
            rows
        } else {
            format!("{header}\n{rows}")
        };
        self.task = Some(task.to_owned());
        ToolRecord {
            rows,
            // Expand is an independent view: it always names its owning task.
            detail: cap(format!("{header}\n{}", indent(&nested.detail))),
            shell_progress: None,
        }
    }
}

/// A standalone call retains its task context even without a preceding group.
pub fn record(call: &Finished<'_>, capabilities: Capabilities) -> ToolRecord {
    ToolRows::default().record(call, capabilities)
}

fn plain_record(call: &Finished<'_>, capabilities: Capabilities) -> ToolRecord {
    let width = capabilities.width;
    let depth = capabilities.depth;
    let args: Value = serde_json::from_str(call.args).unwrap_or(Value::Null);
    let heading = header(call, &args, width, depth);

    let body = body(call, &args, width);
    let mut rows = vec![heading.clone()];
    if call.name == "exec" {
        rows.extend(tail(call, width));
    } else if !body.is_empty() {
        rows.push(hidden_lines(body.len(), width));
    }

    let shell_progress =
        (call.name == "exec" && call.ok && output_lines(call).is_empty()).then(|| heading.clone());
    let mut detail = vec![heading];
    detail.extend(body);
    ToolRecord {
        shell_progress,
        rows: render(&rows, depth),
        detail: cap(render(&detail, depth)),
    }
}

fn render(lines: &[Line], depth: Depth) -> String {
    super::block(lines, depth).trim_end_matches('\n').to_owned()
}

fn cap(mut text: String) -> String {
    if text.len() > DETAIL_LIMIT {
        let mut end = DETAIL_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n… output cut at 256 KiB");
    }
    text
}

/// Replace what could act on a terminal. Tabs become spaces so the cell count
/// the wrapper measures is the cell count the terminal draws.
fn printable(text: &str) -> String {
    ego_core::permit::terminal_safe(&text.replace('\r', "").replace('\t', " "))
}

/// Process output may contain decoration, unlike call arguments or file text.
fn printable_output(text: &str) -> String {
    ego_core::permit::terminal_output_safe(&text.replace('\r', "").replace('\t', " "))
}

/// [`printable`] for text that spans lines: the line breaks stay, the rest of
/// each line is made safe.
fn printable_block(text: &str) -> String {
    text.split('\n')
        .map(printable)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `12ms`, `0.4s`, `1m05s`.
pub fn duration(elapsed: Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{}.{}s", millis / 1000, millis % 1000 / 100)
    } else {
        let seconds = elapsed.as_secs();
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// `(added, removed)` lines of an edit, from the same line diff the expanded
/// view draws, so the two cannot disagree.
fn edit_stat(args: &Value, result: &str) -> Option<(usize, usize)> {
    // Builtin receipts carry actual whole-file totals, including repeated
    // substitutions sharing a line. Older receipts can still use the unique
    // fragment below, but cannot establish replace_all totals.
    if let Some((_, counts)) = result.lines().next().unwrap_or_default().rsplit_once(" (+")
        && let Some(counts) = counts.strip_suffix(" lines)")
        && let Some((added, removed)) = counts.split_once(" -")
        && let (Ok(added), Ok(removed)) = (added.parse(), removed.parse())
    {
        return Some((added, removed));
    }
    // One fragment cannot establish total changed lines across repeated matches.
    if args.get("replace_all").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let old = string_arg(args, "old_string")?;
    let new = string_arg(args, "new_string")?;
    let (mut added, mut removed) = (0, 0);
    for change in TextDiff::from_lines(old, new).iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    Some((added, removed))
}

fn line_count(text: &str) -> usize {
    text.lines().count()
}

/// The words after the verb, and the parts that never give way to width.
struct Parts {
    verb: String,
    target: String,
    /// Joined to the target with a space: `+3 -1`.
    stat: Option<String>,
    /// Joined with ` · `: line count, exit status, duration.
    notes: Vec<String>,
}

fn parts(call: &Finished<'_>, args: &Value, width: usize) -> Parts {
    let (verb, target) = match call.name {
        "read" | "write" | "edit" => (
            call.name.to_owned(),
            string_arg(args, "path").unwrap_or("?").to_owned(),
        ),
        "exec" => ("$".to_owned(), command_line(args)),
        "task" => (
            "task".to_owned(),
            string_arg(args, "name").unwrap_or("?").to_owned(),
        ),
        _ => (
            String::new(),
            format!("{} {}", call.name, compact(call.args)),
        ),
    };
    let stat = if call.ok && call.name == "edit" {
        edit_stat(args, call.result).map(|(added, removed)| format!("+{added} -{removed}"))
    } else {
        None
    };
    let outcome = match (call.ok, call.name) {
        (false, _) => Some(failure(call.result, width)),
        (true, "exec") => Some("exit 0".to_owned()),
        (true, "read") => Some(lines_note(line_count(call.result))),
        (true, "edit") if args.get("replace_all").and_then(Value::as_bool) == Some(true) => {
            Some("all occurrences".to_owned())
        }
        (true, "write") => Some(lines_note(line_count(
            string_arg(args, "contents").unwrap_or(""),
        ))),
        (true, _) => None,
    };
    let mut notes: Vec<String> = outcome.into_iter().collect();
    if call.name == "exec"
        && let Ok(params) = serde_json::from_value::<ego_tools::tools::ExecParams>(args.clone())
        && let Ok(Some(timeout)) = params.effective_timeout()
    {
        let label = if timeout.as_millis() % 60_000 == 0 {
            format!("{}m", timeout.as_secs() / 60)
        } else {
            duration(timeout)
        };
        notes.push(format!("(timeout {label})"));
    }
    if let Some(millis) = call.timing.and_then(|timing| timing.execution_ms) {
        notes.push(duration(Duration::from_millis(millis)));
    }
    if let Some(timing) = call.timing
        && timing.approval_wait_ms > 0
    {
        notes.push(format!(
            "approval {}",
            duration(Duration::from_millis(timing.approval_wait_ms))
        ));
    }
    Parts {
        verb,
        target: printable(&target),
        stat,
        notes,
    }
}

fn command_line(args: &Value) -> String {
    args.get("argv")
        .and_then(Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(Value::as_str)
                .map(quote)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn lines_note(count: usize) -> String {
    format!("{count} {}", if count == 1 { "line" } else { "lines" })
}

/// An argument that would split when read back as a command line is quoted.
fn quote(argument: &str) -> String {
    ego_core::permit::shell_word(argument)
}

/// The arguments of a tool with no specialised row, on one line.
fn compact(args: &str) -> String {
    serde_json::from_str::<Value>(args)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| args.trim().to_owned())
}

/// The reason a call failed, in a few words: `exit 3` for a process, the last
/// line of the message otherwise.
///
/// A message that does not fit keeps its end: a tool error names the path
/// first, which the row already shows, and the cause last.
fn failure(result: &str, width: usize) -> String {
    let last = result
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("failed")
        .trim();
    match last.strip_prefix(EXIT_PREFIX) {
        Some(code) => {
            let code = code
                .strip_prefix("Some(")
                .and_then(|code| code.strip_suffix(')'))
                .unwrap_or("?");
            format!("exit {code}")
        }
        None => keep_end(&printable_output(last), (width / 3).max(12)),
    }
}

/// The last `budget` cells of `text`, marked with a leading `…` when cut.
fn keep_end(text: &str, budget: usize) -> String {
    if Line::plain(text).width() <= budget {
        return text.to_owned();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 1;
    for grapheme in text.graphemes(true).rev() {
        let cells = Line::plain(grapheme).width();
        if used + cells > budget {
            break;
        }
        used += cells;
        kept.push(grapheme);
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

/// The header row. The target gives way to width; the mark, the verb, the
/// outcome and the duration do not, because a row that loses its outcome to
/// a narrow terminal has lost the only thing it was for.
fn header(call: &Finished<'_>, args: &Value, width: usize, depth: Depth) -> Line {
    let parts = parts(call, args, width);
    let mark = if call.ok { "✓" } else { "✗" };
    let colour = if call.ok {
        Palette::ADDED_FG
    } else {
        Palette::ERROR
    };

    let lead = if parts.verb.is_empty() {
        format!("{mark} ")
    } else {
        format!("{mark} {} ", parts.verb)
    };
    let stat = parts
        .stat
        .as_ref()
        .map(|stat| format!(" {stat}"))
        .unwrap_or_default();
    let notes = if parts.notes.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.notes.join(" · "))
    };
    let fixed = Line::plain(format!("{lead}{stat}{notes}")).width();
    let room = width.saturating_sub(fixed);
    let mut target = if call.name == "exec" {
        super::shell::command(args, &parts.target, depth)
    } else {
        Line::plain(parts.target)
    };
    if target.width() > room {
        target = target.truncate_to(room.saturating_sub(1));
        target.push(Span::plain("…"));
    }

    let mut line = Line::empty();
    line.push(Span::new(mark, Style::fg(colour)));
    line.push(Span::plain(lead[mark.len()..].to_owned()));
    for span in target.spans {
        line.push(span);
    }
    line.push(Span::plain(stat));
    line.push(Span::new(notes, Style::fg(Palette::NOTE)));
    line.truncate_to(width)
}

/// The output lines of an exec, retaining stderr identity and excluding only
/// the process exit diagnostic already carried by the header.
fn output_lines(call: &Finished<'_>) -> Vec<String> {
    let mut stderr = false;
    let mut lines = Vec::new();
    for line in call.result.lines() {
        match line {
            "[stdout]" => stderr = false,
            "[stderr]" => stderr = true,
            _ if line.trim().is_empty() => {}
            _ if !call.ok && line.starts_with(EXIT_PREFIX) => {}
            _ => {
                let text = printable_output(line);
                lines.push(if stderr {
                    format!("stderr: {text}")
                } else {
                    text
                });
            }
        }
    }
    lines
}

fn dim(text: String) -> Line {
    Line::styled(text, Style::fg(Palette::NOTE))
}

/// The shared hidden-line count for compact tool and prompt output.
pub(crate) fn hidden_lines(hidden: usize, width: usize) -> Line {
    dim(format!(
        "  … {hidden} more {} · Option+O or /expand",
        if hidden == 1 { "line" } else { "lines" }
    ))
    .truncate_to(width)
}

/// The last lines of an exec's output, and a count of the ones left out.
fn tail(call: &Finished<'_>, width: usize) -> Vec<Line> {
    let lines = output_lines(call);
    let hidden = lines.len().saturating_sub(PREVIEW_LINES);
    let mut rows = Vec::new();
    if hidden > 0 {
        rows.push(hidden_lines(hidden, width));
    }
    rows.extend(lines[hidden..].iter().flat_map(|line| {
        let row = dim(format!("  {line}"));
        // Recovery is an operator action, not disposable command output:
        // keep it visible without requiring the user to expand the tool.
        if !call.ok
            && line
                .strip_prefix("stderr: ")
                .unwrap_or(line)
                .starts_with("sandbox exec denied ")
        {
            row.wrap_hanging(width, 2)
        } else {
            vec![row.truncate_to(width)]
        }
    }));
    rows
}

/// Everything under the header, for the expanded view: the diff of an edit,
/// the whole text of any other result.
fn body(call: &Finished<'_>, args: &Value, width: usize) -> Vec<Line> {
    if call.ok
        && call.name == "edit"
        && args.get("replace_all").and_then(Value::as_bool) != Some(true)
        && let (Some(old), Some(new)) = (
            string_arg(args, "old_string"),
            string_arg(args, "new_string"),
        )
    {
        return diff::render(
            &printable_block(old),
            &printable_block(new),
            width,
            &Unhighlighted,
        );
    }
    let room = width.saturating_sub(2).max(1);
    // Stream identity comes from evidence provenance, never from marker-like
    // text a process may legitimately print. Without blocks, retain all text.
    let visible = call
        .result_blocks
        .filter(|_| call.name == "exec")
        .map(|blocks| {
            ego_core::permit::ToolResult::new(
                blocks
                    .iter()
                    .filter(|block| !block.backend_owned_pipe() || !block.text.trim().is_empty())
                    .cloned()
                    .collect(),
            )
            .model_text()
        });
    visible
        .as_deref()
        .unwrap_or(call.result)
        .lines()
        .flat_map(|line| Line::plain(printable_output(line)).wrap(room))
        .map(|line| {
            let mut indented = Line::plain("  ");
            line.spans.into_iter().for_each(|span| indented.push(span));
            indented
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::line::cells;

    fn at(width: usize) -> Capabilities {
        Capabilities {
            width,
            ..Capabilities::plain()
        }
    }

    fn call<'a>(name: &'a str, args: &'a str, ok: bool, result: &'a str) -> Finished<'a> {
        Finished {
            name,
            args,
            ok,
            result,
            result_blocks: None,
            timing: Some(ego_core::event::domain::ToolTiming {
                execution_ms: Some(1500),
                approval_wait_ms: 0,
            }),
        }
    }

    fn header_of(record: &ToolRecord) -> &str {
        record.rows.lines().next().expect("a header row")
    }

    #[test]
    fn exec_command_syntax_colours_survive_layout_without_colouring_the_prefix() {
        // Catches: parsing the quoted display paints the entire raw script as a string.
        let args = r#"{"argv":["bash","-lc","echo --flag \"text\" $VAR | cat"]}"#;
        let result = record(
            &call("exec", args, true, ""),
            Capabilities {
                depth: Depth::TrueColor,
                ..at(160)
            },
        );
        for token in [
            "\x1b[38;2;155;181;204mecho\x1b[0m",
            "\x1b[38;2;217;190;129m--flag\x1b[0m",
            "\x1b[38;2;151;207;167m\"text\"\x1b[0m",
            "\x1b[38;2;174;185;213m$VAR\x1b[0m",
            "\x1b[38;2;150;173;209m|\x1b[0m",
        ] {
            assert!(
                result.rows.contains(token),
                "missing {token:?}: {:?}",
                result.rows
            );
        }
        assert!(result.rows.starts_with("\x1b[38;2;151;207;167m✓\x1b[0m $ "));
    }

    #[test]
    fn exec_syntax_preserves_old_display_at_every_depth() {
        // Catches: projection changes shell quoting, controls, Unicode, or header layout.
        let args = serde_json::json!({"argv": ["/bin/echo", "", "it's", "a|b", "$HOME", "é界", "a\nb\t\u{1b}"]}).to_string();
        let expected = "✓ $ /bin/echo '' 'it'\\''s' 'a|b' '$HOME' 'é界' 'a\\nb\\t\\u{1b}' · exit 0 · (timeout 2m) · 1.5s";
        for (args, expected) in [
            (args.as_str(), expected),
            (
                r#"{"argv":["bash","-lc","echo --flag \"text\" $VAR | cat"]}"#,
                "✓ $ bash -lc 'echo --flag \"text\" $VAR | cat' · exit 0 · (timeout 2m) · 1.5s",
            ),
        ] {
            for depth in [Depth::None, Depth::TrueColor, Depth::Ansi256, Depth::Ansi16] {
                let result = record(
                    &call("exec", args, true, ""),
                    Capabilities { depth, ..at(200) },
                );
                assert_eq!(
                    ego_core::permit::terminal_output_safe(&result.rows),
                    expected,
                    "{depth:?}"
                );
                match depth {
                    Depth::None => assert!(!result.rows.contains('\x1b')),
                    Depth::TrueColor => assert!(result.rows.contains("38;2;")),
                    Depth::Ansi256 => {
                        assert!(result.rows.contains("38;5;"));
                        assert!(!result.rows.contains("38;2;"));
                    }
                    Depth::Ansi16 => {
                        assert!(result.rows.contains('\x1b'));
                        assert!(!result.rows.contains("38;"));
                    }
                }
            }
        }
    }

    #[test]
    fn exec_script_projection_keeps_apostrophe_and_control_encoding() {
        // Catches: source offsets are used directly after shell-word escaping expands them.
        let args = serde_json::json!({"argv": ["bash", "-lc", "echo 'hi'\n"]}).to_string();
        let result = record(
            &call("exec", &args, true, ""),
            Capabilities {
                depth: Depth::TrueColor,
                ..at(160)
            },
        );
        assert_eq!(
            ego_core::permit::terminal_output_safe(&result.rows),
            r#"✓ $ bash -lc 'echo '\''hi'\''\n' · exit 0 · (timeout 2m) · 1.5s"#
        );
        assert!(
            result
                .rows
                .contains("\x1b[38;2;151;207;167m'\\''hi'\\''\x1b[0m"),
            "{:?}",
            result.rows
        );
    }

    #[test]
    fn exec_nested_expansions_override_strings_without_swallowing_commands() {
        // Catches: broad string/expansion ranges swallow nested command and variable roles.
        let args = r#"{"argv":["bash","-lc","printf \"a $HOME $(id)\" > out # note"]}"#;
        let result = record(
            &call("exec", args, true, ""),
            Capabilities {
                depth: Depth::TrueColor,
                ..at(160)
            },
        );
        for token in [
            "\x1b[38;2;151;207;167m\"a \x1b[0m",
            "\x1b[38;2;174;185;213m$HOME\x1b[0m",
            "\x1b[38;2;155;181;204mid\x1b[0m",
            "\x1b[38;2;150;173;209m>\x1b[0m",
            "\x1b[38;2;153;153;153m# note\x1b[0m",
        ] {
            assert!(
                result.rows.contains(token),
                "missing {token:?}: {:?}",
                result.rows
            );
        }
    }

    #[test]
    fn exec_invalid_raw_script_has_no_partial_target_colours() {
        // Catches: shell-word wrapper quoting hides errors in the raw script.
        for script in ["echo --flag 'unterminated", "echo ok |", "if then"] {
            let args = serde_json::json!({"argv": ["bash", "-lc", script]}).to_string();
            let result = record(
                &call("exec", &args, true, ""),
                Capabilities {
                    depth: Depth::TrueColor,
                    ..at(200)
                },
            );
            let target = result.shell_progress.unwrap();
            for span in target
                .spans
                .iter()
                .skip(1)
                .take(target.spans.len().saturating_sub(2))
            {
                assert_eq!(span.style, Style::PLAIN, "{script}: {span:?}");
            }
        }
    }

    #[test]
    fn exec_truncation_and_live_retruncate_retain_command_colours() {
        // Catches: converting a styled target to text before narrowing drops its colours.
        let args = r#"{"argv":["git","--no-pager","log","an-operand-long-enough-to-truncate"]}"#;
        let result = record(
            &call("exec", args, true, ""),
            Capabilities {
                depth: Depth::TrueColor,
                ..at(52)
            },
        );
        assert!(result.rows.contains("\x1b[38;2;155;181;204mgit\x1b[0m"));
        assert!(
            result
                .rows
                .contains("\x1b[38;2;217;190;129m--no-pager\x1b[0m l…"),
            "{:?}",
            result.rows
        );
        assert_eq!(
            ego_core::permit::terminal_output_safe(&result.rows),
            "✓ $ git --no-pager l… · exit 0 · (timeout 2m) · 1.5s"
        );
        let live = result.shell_progress.unwrap();
        assert!(
            live.truncate_to(10)
                .render(Depth::TrueColor)
                .contains("\x1b[38;2;155;181;204mgit\x1b[0m")
        );
        assert!(live.wrap(12).iter().any(|line| {
            line.render(Depth::TrueColor)
                .contains("\x1b[38;2;155;181;204mgit\x1b[0m")
        }));
    }

    #[test]
    fn exec_direct_arguments_do_not_become_shell_operators_or_expansions() {
        // Catches: parsing literal argv turns data into fake shell syntax.
        let args = r#"{"argv":["echo","--flag","a|b","$HOME"]}"#;
        let result = record(
            &call("exec", args, true, ""),
            Capabilities {
                depth: Depth::TrueColor,
                ..at(160)
            },
        );
        for literal in ["'a|b'", "'$HOME'"] {
            assert!(
                result
                    .rows
                    .contains(&format!("\x1b[38;2;151;207;167m{literal}\x1b[0m")),
                "{:?}",
                result.rows
            );
        }
        assert!(!result.rows.contains("38;2;150;173;209"));
        assert!(!result.rows.contains("38;2;174;185;213"));
    }

    #[test]
    fn exec_shell_fallback_clears_wrapper_for_oversize_and_unknown_dialect() {
        // Catches: fallback leaves misleading interpreter/flag colours behind.
        for argv in [
            vec!["bash".to_owned(), "-lc".to_owned(), "x".repeat(65537)],
            vec!["fish".to_owned(), "-c".to_owned(), "echo $HOME".to_owned()],
        ] {
            let args = serde_json::json!({"argv": argv}).to_string();
            let result = record(
                &call("exec", &args, true, ""),
                Capabilities {
                    depth: Depth::TrueColor,
                    ..at(160)
                },
            );
            assert_eq!(result.rows.matches("\x1b[").count(), 4, "{:?}", result.rows);
        }
    }

    #[test]
    fn nested_task_rows_keep_the_child_label_and_do_not_wrap_into_the_composer() {
        // Catches: a long child name causing nested rows to overwrite the composer.
        for width in [40, 80, 120] {
            let result = record(
                &call(
                    "a very long child name that exceeds a narrow screen › read",
                    r#"{"path":"calc.py"}"#,
                    true,
                    "hello\n",
                ),
                at(width),
            );
            assert!(header_of(&result).starts_with("  ↳ task a very lon"));
            assert!(
                result
                    .rows
                    .lines()
                    .nth(1)
                    .unwrap()
                    .starts_with("    ✓ read")
            );
            assert!(
                result.rows.lines().all(|line| cells(line) <= width),
                "{width}: {}",
                result.rows
            );
        }
    }

    // Catches: empty evidence streams grow labels, or marker-like process
    // payloads are parsed as labels and disappear from expanded output.
    #[test]
    fn expanded_exec_labels_only_streams_with_visible_output() {
        use ego_core::event::domain::{DomainEvent, EventKind};
        let events: Vec<DomainEvent> =
            serde_json::from_str(include_str!("../../tests/fixtures/expand-visible-streams.json")).unwrap();
        let recorded = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ToolCallCompleted { result, .. } => Some(result),
                _ => None,
            })
            .unwrap();
        for (stdout, stderr, diagnostic, expected) in [
            ("out\n", "", None, "  [stdout]\n  out"),
            ("", "warning\n", None, "  [stderr]\n  warning"),
            ("", "", None, ""),
            (
                "",
                "",
                Some("process exited unsuccessfully with code Some(2)"),
                "  process exited unsuccessfully with code Some(2)",
            ),
            ("[stderr]\n", "", None, "  [stdout]\n  [stderr]"),
            (
                "",
                "\n[stdout]\n\n[stderr]\n",
                None,
                "  [stderr]\n\n  [stdout]\n\n  [stderr]",
            ),
        ] {
            let mut result = recorded.clone();
            result.content[0].text = stdout.to_owned();
            result.content[1].text = stderr.to_owned();
            if let Some(diagnostic) = diagnostic {
                result
                    .content
                    .push(ego_core::permit::ToolContentBlock::builtin(diagnostic));
            }
            let text = result.model_text();
            let rendered = record(
                &Finished {
                    result_blocks: Some(&result.content),
                    ..call("exec", r#"{"argv":["x"]}"#, diagnostic.is_none(), &text)
                },
                at(120),
            );
            let body = rendered
                .detail
                .split_once('\n')
                .map_or("", |(_, body)| body);
            assert_eq!(body, expected, "stdout={stdout:?}, stderr={stderr:?}");
        }
        let raw = record(
            &call("exec", r#"{"argv":["x"]}"#, true, "[stderr]"),
            at(120),
        );
        assert!(raw.detail.ends_with("\n  [stderr]"), "{}", raw.detail);
    }

    // Catches: stderr becomes indistinguishable from stdout in a collapsed preview.
    #[test]
    fn rendered_failed_exec_identifies_stderr() {
        for width in [80, 120, 200] {
            for (ok, result, expected, hidden) in [
                (
                    false,
                    "[stderr]\nrg: missing.txt\n\nprocess exited unsuccessfully with code Some(2)",
                    "stderr: rg: missing.txt",
                    None,
                ),
                (
                    false,
                    "[stdout]\none\ntwo\nthree\nfour\n[stderr]\nrg: missing.txt\n\nprocess exited unsuccessfully with code Some(2)",
                    "stderr: rg: missing.txt",
                    Some("2 more lines"),
                ),
                (
                    true,
                    "[stderr]\nwarning: check input",
                    "stderr: warning: check input",
                    None,
                ),
            ] {
                let rendered = record(
                    &call("exec", r#"{"argv":["rg","missing.txt"]}"#, ok, result),
                    at(width),
                );
                let mut terminal = vt100::Parser::new(12, width as u16, 0);
                terminal.process(rendered.rows.replace('\n', "\r\n").as_bytes());
                let grid = terminal.screen().contents();
                assert!(grid.contains(expected), "width={width}: {grid}");
                assert!(
                    grid.contains(if ok { "exit 0" } else { "exit 2" }),
                    "{grid}"
                );
                assert!(rendered.rows.lines().count() <= 5);
                if let Some(hidden) = hidden {
                    assert!(
                        grid.contains(hidden) && grid.contains("Option+O or /expand"),
                        "{grid}"
                    );
                }
                assert!(rendered.detail.contains("[stderr]"));
                assert!(rendered.detail.contains("rg: missing.txt") || ok);
                println!("exec width={width} ok={ok} grid={grid:?}");
            }
        }
    }

    // Catches: absent timing leaves a placeholder or an empty separator.
    #[test]
    fn missing_timing_omits_the_note_and_its_separator() {
        for (name, expected) in [("task", "✓ task child"), ("read", "✓ read a.rs · 1 line")] {
            let record = record(
                &Finished {
                    timing: None,
                    ..call(name, r#"{"name":"child","path":"a.rs"}"#, true, "hello")
                },
                at(80),
            );
            assert_eq!(header_of(&record), expected);
        }
    }

    // Catches: transcript hides the effective default, invents an unbounded
    // background deadline, or reports a rejected foreground override as admitted.
    #[test]
    fn exec_row_shows_effective_timeout() {
        for (args, expected) in [
            (r#"{"argv":["echo"]}"#, Some("(timeout 2m)")),
            (
                r#"{"argv":["echo"],"timeout_ms":null}"#,
                Some("(timeout 2m)"),
            ),
            (
                r#"{"argv":["echo"],"timeout_ms":600000}"#,
                Some("(timeout 10m)"),
            ),
            (
                r#"{"argv":["echo"],"timeout_ms":1500}"#,
                Some("(timeout 1.5s)"),
            ),
            (r#"{"argv":["echo"],"background":true}"#, None),
            (
                r#"{"argv":["echo"],"background":true,"timeout_ms":601000}"#,
                Some("(timeout 10m01s)"),
            ),
            (r#"{"argv":["echo"],"timeout_ms":600001}"#, None),
        ] {
            let rendered = record(&call("exec", args, true, ""), at(120));
            match expected {
                Some(text) => assert!(
                    header_of(&rendered).contains(text),
                    "{args}: {}",
                    rendered.rows
                ),
                None => assert!(
                    !header_of(&rendered).contains("(timeout"),
                    "{args}: {}",
                    rendered.rows
                ),
            }
            println!("exec timeout after: {}", rendered.rows);
        }
    }

    // Catches: a collapsed read counts source lines instead of hidden wrapped rows.
    #[test]
    fn collapsed_read_counts_wrapped_rows() {
        let rendered = record(
            &call("read", r#"{"path":"a.rs"}"#, true, &"界".repeat(80)),
            at(42),
        );
        assert_eq!(
            rendered.rows.lines().nth(1),
            Some("  … 4 more lines · Option+O or /expand")
        );
        println!("read after:\n{}", rendered.rows);
    }

    // Catches: an edit hides the diff without naming its context and change rows.
    #[test]
    fn collapsed_edit_counts_diff_rows() {
        let rendered = record(
            &call(
                "edit",
                r#"{"path":"a.rs","old_string":"x\ny\n","new_string":"x\nz\nw\n"}"#,
                true,
                "replaced 1 occurrence in a.rs (+2 -1 lines)",
            ),
            at(80),
        );
        assert_eq!(
            rendered.rows,
            "✓ edit a.rs +2 -1 · 1.5s\n  … 4 more lines · Option+O or /expand"
        );
        assert!(rendered.detail.contains(" 2 -y"));
        println!("edit after:\n{}", rendered.rows);
    }

    // Catches: a write confuses file lines with the receipt rows actually hidden.
    #[test]
    fn collapsed_write_counts_receipt_rows() {
        let rendered = record(
            &call(
                "write",
                r#"{"path":"a.rs","contents":"x\ny\nz\n"}"#,
                true,
                "wrote a.rs",
            ),
            at(80),
        );
        assert_eq!(
            rendered.rows,
            "✓ write a.rs · 3 lines · 1.5s\n  … 1 more line · Option+O or /expand"
        );
        assert!(rendered.detail.ends_with("  wrote a.rs"));
        println!("write after:\n{}", rendered.rows);
    }

    // Catches: only builtins get counts, or empty results advertise nonexistent rows.
    #[test]
    fn collapsed_generic_counts_hidden_rows() {
        for (result, hint) in [
            (
                "one\n\nthree\n",
                Some("  … 3 more lines · Option+O or /expand"),
            ),
            ("", None),
        ] {
            let rendered = record(&call("mcp:server/tool", "{}", true, result), at(80));
            assert_eq!(rendered.rows.lines().nth(1), hint);
            println!("generic after:\n{}", rendered.rows);
        }
    }

    // Catches: repeated substitutions on one line are multiplied into false totals.
    #[test]
    fn repeated_edit_uses_actual_changed_line_totals() {
        let rendered = record(
            &call(
                "edit",
                r#"{"path":"a.rs","old_string":"x","new_string":"y","replace_all":true}"#,
                true,
                "replaced 3 occurrences in a.rs (+1 -1 lines)\nwrite notice",
            ),
            at(100),
        );
        assert_eq!(
            header_of(&rendered),
            "✓ edit a.rs +1 -1 · all occurrences · 1.5s"
        );
        assert!(
            rendered
                .rows
                .ends_with("  … 2 more lines · Option+O or /expand")
        );
        assert!(rendered.detail.contains("replaced 3 occurrences"));
        println!("repeated edit after:\n{}", rendered.rows);
    }

    #[test]
    fn durations_switch_unit_at_a_second_and_at_a_minute() {
        assert_eq!(duration(Duration::from_millis(12)), "12ms");
        assert_eq!(duration(Duration::from_millis(999)), "999ms");
        assert_eq!(duration(Duration::from_millis(1499)), "1.4s");
        assert_eq!(duration(Duration::from_secs(65)), "1m05s");
    }

    #[test]
    fn a_read_row_names_the_file_the_lines_and_the_time() {
        let record = record(
            &call("read", r#"{"path":"calc.py"}"#, true, "a = 1\nb = 2\n"),
            at(80),
        );
        assert_eq!(header_of(&record), "✓ read calc.py · 2 lines · 1.5s");
    }

    /// The counts come from a line diff and not from the two strings' lengths:
    /// replacing one line by one line is `+1 -1`, not `+1 -0`.
    #[test]
    fn an_edit_row_counts_changed_lines_on_both_sides() {
        let args = r#"{"path":"a.rs","old_string":"x\ny\n","new_string":"x\nz\nw\n"}"#;
        let record = record(
            &call("edit", args, true, "replaced 1 occurrence in a.rs"),
            at(80),
        );
        assert_eq!(header_of(&record), "✓ edit a.rs +2 -1 · 1.5s");
    }

    // Catches: repeat edits display one fragment's line counts or hide the actual receipt.
    #[test]
    fn replace_all_row_does_not_claim_single_fragment_statistics() {
        let result = record(
            &call(
                "edit",
                r#"{"path":"a.rs","old_string":"x","new_string":"y","replace_all":true}"#,
                true,
                "replaced 3 occurrences in a.rs",
            ),
            at(100),
        );
        assert_eq!(header_of(&result), "✓ edit a.rs · all occurrences · 1.5s");
        assert!(result.detail.contains("replaced 3 occurrences in a.rs"));
        assert!(!result.rows.contains("+1 -1"));
    }

    #[test]
    fn a_failed_exec_shows_its_exit_code_and_the_output_that_explains_it() {
        let result = "[stderr]\nboom\n\nprocess exited unsuccessfully with code Some(3)";
        let record = record(
            &call("exec", r#"{"argv":["sh","-c","exit 3"]}"#, false, result),
            at(80),
        );
        let rows: Vec<_> = record.rows.lines().collect();
        assert_eq!(rows[0], "✗ $ sh -c 'exit 3' · exit 3 · (timeout 2m) · 1.5s");
        assert_eq!(rows[1..], ["  stderr: boom"]);
    }

    // Catches: compact exec failure clips the program or recovery instructions,
    // leaving the user unable to repair executable authority without /expand.
    #[test]
    fn sandbox_exec_recovery_is_visible_without_expanding_the_tool() {
        let result = "[stderr]\nsandbox exec denied /outside/tools/program (confinement workspace); no read or write grant can authorize execution. Add the program and required interpreter locations to exec_paths in the user configuration with `ego config edit`. Then /quit and run `ego resume <session-id> --mode <same-mode> --sandbox workspace --model <same-model>`; retain the session's --mode, --sandbox and --model selections. No command was retried.\nprocess exited unsuccessfully with code Some(126)";
        for width in [20, 40, 80, 120] {
            let rendered = record(
                &call(
                    "exec",
                    r#"{"argv":["/bin/sh","-c","program"]}"#,
                    false,
                    result,
                ),
                at(width),
            );
            assert!(
                rendered.rows.lines().all(|row| cells(row) <= width),
                "{width}: {}",
                rendered.rows
            );
            let visible: String = rendered
                .rows
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect();
            for expected in [
                "/outside/tools/program",
                "exec_paths",
                "egoconfigedit",
                "/quit",
                "egoresume<session-id>",
                "--mode<same-mode>",
                "--sandboxworkspace",
                "--model<same-model>",
                "Nocommandwasretried.",
            ] {
                assert!(
                    visible.contains(expected),
                    "width {width} missing {expected:?}: {}",
                    rendered.rows
                );
            }
        }
    }

    // Catches: the recovery exception expands unrelated failure output or a
    // successful command that merely prints text resembling a denial.
    #[test]
    fn ordinary_exec_previews_still_truncate_long_lines() {
        for (ok, prefix) in [
            (false, "ordinary error "),
            (false, "sandbox denied read "),
            (true, "sandbox exec denied "),
        ] {
            let result = format!("[stderr]\n{prefix}{}END-OF-LONG-OUTPUT", "x".repeat(200));
            let rendered = record(
                &call("exec", r#"{"argv":["/bin/echo"]}"#, ok, &result),
                at(40),
            );
            assert_eq!(rendered.rows.lines().count(), 2, "{}", rendered.rows);
            assert!(!rendered.rows.contains("END-OF-LONG-OUTPUT"));
            assert!(rendered.detail.contains("END-OF-LONG-OUTPUT"));
        }
    }

    #[test]
    fn an_exec_tail_keeps_the_last_three_lines_and_counts_the_rest() {
        let result = format!(
            "[stdout]\n{}",
            (1..=20).map(|n| format!("{n}\n")).collect::<String>()
        );
        let record = record(
            &call("exec", r#"{"argv":["seq","1","20"]}"#, true, &result),
            at(80),
        );
        let rows: Vec<_> = record.rows.lines().collect();
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert_eq!(rows[1], "  … 17 more lines · Option+O or /expand");
        assert_eq!(rows[2..], ["  18", "  19", "  20"]);
        assert!(record.detail.contains("  1\n"));
        assert!(record.detail.contains("  20"));
    }

    #[test]
    fn every_row_fits_the_terminal_and_keeps_its_outcome_and_time() {
        let path = format!("{{\"path\":\"{}\"}}", "d/".repeat(60));
        let argv = format!("{{\"argv\":[\"echo\",\"{}\"]}}", "a".repeat(200));
        for width in [120, 80, 60, 40, 20] {
            for (name, args, ok, result) in [
                ("read", path.as_str(), false, "no such file or directory"),
                ("exec", argv.as_str(), true, "out"),
                ("mcp:server/tool", argv.as_str(), true, "out"),
            ] {
                let record = record(&call(name, args, ok, result), at(width));
                for row in record.rows.lines() {
                    assert!(cells(row) <= width, "{width} cols: {row:?}");
                }
                if width >= 40 {
                    assert!(
                        header_of(&record).ends_with("1.5s"),
                        "{width} cols lost its time: {:?}",
                        header_of(&record)
                    );
                }
            }
        }
    }

    // Catches: SGR decoration is printed as escaped text in tool output.
    #[test]
    fn tool_output_strips_sgr_without_hiding_other_controls() {
        for (source, expected) in [
            ("\x1b[32m✓ Added worklog\x1b[0m", "✓ Added worklog"),
            ("\x1b[38;2;1;2;3mcolour\x1b[m", "colour"),
            ("\x1b[38:2::1:2:3mcolour\x1b[0m", "colour"),
            ("\x1b[2Jscreen\u{202e}", "\\u{1b}[2Jscreen\\u{202e}"),
            ("\x1b]0;title\x07", "\\u{1b}]0;title\\u{7}"),
            ("\x1b[32incomplete", "\\u{1b}[32incomplete"),
        ] {
            let result = format!("[stdout]\n{source}\n[stderr]\n{source}\n");
            let rendered = record(&call("exec", r#"{"argv":["x"]}"#, true, &result), at(120));
            let rows: Vec<_> = rendered.rows.lines().skip(1).collect();
            assert_eq!(
                rows,
                [format!("  {expected}"), format!("  stderr: {expected}")]
            );
            let detail: Vec<_> = rendered.detail.lines().skip(1).collect();
            assert_eq!(
                detail,
                [
                    "  [stdout]".to_owned(),
                    format!("  {expected}"),
                    "  [stderr]".to_owned(),
                    format!("  {expected}")
                ]
            );
        }
    }

    /// A result is text from outside the trust boundary; an escape in it must
    /// not reach the terminal as an instruction.
    #[test]
    fn a_result_cannot_carry_an_escape_sequence_to_the_terminal() {
        let hostile = "[stdout]\n\x1b]0;pwned\x07\x1b[2Jscreen\r\n";
        let record = record(&call("exec", r#"{"argv":["x"]}"#, true, hostile), at(80));
        for text in [&record.rows, &record.detail] {
            assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
        }
    }

    /// A replaced text of two lines is two removed rows and two added rows,
    /// and no line break is turned into a replacement character.
    #[test]
    fn an_expanded_multi_line_edit_keeps_one_row_per_line() {
        let args =
            r##"{"path":"p.py","old_string":"# a\nreturn 1\n","new_string":"# b\nreturn 2\n"}"##;
        let record = record(&call("edit", args, true, "ok"), at(80));
        assert!(!record.detail.contains('\u{FFFD}'), "{:?}", record.detail);
        let rows: Vec<&str> = record.detail.lines().map(str::trim_end).collect();
        for expected in [" 1 -# a", " 2 -return 1", " 1 +# b", " 2 +return 2"] {
            assert!(
                rows.iter().any(|row| row.ends_with(expected)),
                "{expected:?} is not a row of {rows:?}"
            );
        }
    }

    #[test]
    fn detail_larger_than_the_limit_is_cut_and_says_so() {
        let huge = "x".repeat(DETAIL_LIMIT * 2);
        let record = record(&call("read", r#"{"path":"big"}"#, true, &huge), at(80));
        assert!(record.detail.len() < DETAIL_LIMIT + 200);
        assert!(record.detail.ends_with("… output cut at 256 KiB"));
    }
}
