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
const TAIL_LINES: usize = 3;

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
}

/// One finished tool call.
pub struct Finished<'a> {
    pub name: &'a str,
    /// The call's argument JSON exactly as the model sent it.
    pub args: &'a str,
    pub ok: bool,
    /// The result text the model was given, or the failure message.
    pub result: &'a str,
    pub timing: Option<ego_core::event::domain::ToolTiming>,
}

pub fn record(call: &Finished<'_>, capabilities: Capabilities) -> ToolRecord {
    if let Some((child, name)) = call.name.split_once(" › ") {
        let label = Line::plain(printable(child))
            .truncate_to(capabilities.width.saturating_sub(30))
            .text();
        let prefix = format!("  ↳ {label} › ");
        let mut nested_caps = capabilities;
        nested_caps.width = capabilities
            .width
            .saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str()))
            .max(1);
        let mut nested = record(&Finished { name, ..*call }, nested_caps);
        nested.rows = nested
            .rows
            .lines()
            .map(|line| format!("{prefix}{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        nested.detail = cap(nested
            .detail
            .lines()
            .map(|line| format!("{prefix}{line}"))
            .collect::<Vec<_>>()
            .join("\n"));
        return nested;
    }
    let width = capabilities.width;
    let depth = capabilities.depth;
    let args: Value = serde_json::from_str(call.args).unwrap_or(Value::Null);
    let header = header(call, &args, width);

    let mut rows = vec![header.clone()];
    if call.name == "exec" {
        rows.extend(tail(call, width));
    }

    let mut detail = vec![header];
    detail.extend(body(call, &args, width));
    ToolRecord {
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
fn edit_stat(args: &Value) -> Option<(usize, usize)> {
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
        edit_stat(args).map(|(added, removed)| format!("+{added} -{removed}"))
    } else {
        None
    };
    let outcome = match (call.ok, call.name) {
        (false, _) => Some(failure(call.result, width)),
        (true, "exec") => Some("exit 0".to_owned()),
        (true, "read") => Some(lines_note(line_count(call.result))),
        (true, "write") => Some(lines_note(line_count(
            string_arg(args, "contents").unwrap_or(""),
        ))),
        (true, _) => None,
    };
    let mut notes: Vec<String> = outcome.into_iter().collect();
    notes.push(
        call.timing
            .and_then(|timing| timing.execution_ms)
            .map(|millis| duration(Duration::from_millis(millis)))
            .unwrap_or_else(|| "duration unavailable".to_owned()),
    );
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
        None => keep_end(&printable(last), (width / 3).max(12)),
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
fn header(call: &Finished<'_>, args: &Value, width: usize) -> Line {
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
    let notes = format!(" · {}", parts.notes.join(" · "));
    let fixed = Line::plain(format!("{lead}{stat}{notes}")).width();
    let room = width.saturating_sub(fixed);
    let target = if Line::plain(parts.target.as_str()).width() <= room {
        parts.target.clone()
    } else {
        let kept = Line::plain(parts.target.as_str())
            .truncate_to(room.saturating_sub(1))
            .text();
        format!("{kept}…")
    };

    let mut line = Line::empty();
    line.push(Span::new(mark, Style::fg(colour)));
    line.push(Span::plain(lead[mark.len()..].to_owned()));
    line.push(Span::plain(target));
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
                let text = printable(line);
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

/// The last lines of an exec's output, and a count of the ones left out.
fn tail(call: &Finished<'_>, width: usize) -> Vec<Line> {
    let lines = output_lines(call);
    let hidden = lines.len().saturating_sub(TAIL_LINES);
    let mut rows = Vec::new();
    if hidden > 0 {
        rows.push(
            dim(format!(
                "  … {hidden} more {} · /expand shows all",
                if hidden == 1 { "line" } else { "lines" }
            ))
            .truncate_to(width),
        );
    }
    rows.extend(
        lines[hidden..]
            .iter()
            .map(|line| dim(format!("  {line}")).truncate_to(width)),
    );
    rows
}

/// Everything under the header, for the expanded view: the diff of an edit,
/// the whole text of any other result.
fn body(call: &Finished<'_>, args: &Value, width: usize) -> Vec<Line> {
    if call.ok
        && call.name == "edit"
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
    call.result
        .lines()
        .flat_map(|line| Line::plain(printable(line)).wrap(room))
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
            assert!(header_of(&result).starts_with("  ↳ a very lon"));
            assert!(header_of(&result).contains("› ✓ read"));
            assert!(
                result.rows.lines().all(|line| cells(line) <= width),
                "{width}: {}",
                result.rows
            );
        }
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
                        grid.contains(hidden) && grid.contains("/expand shows all"),
                        "{grid}"
                    );
                }
                assert!(rendered.detail.contains("[stderr]"));
                assert!(rendered.detail.contains("rg: missing.txt") || ok);
                println!("exec width={width} ok={ok} grid={grid:?}");
            }
        }
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

    #[test]
    fn a_failed_exec_shows_its_exit_code_and_the_output_that_explains_it() {
        let result = "[stderr]\nboom\n\nprocess exited unsuccessfully with code Some(3)";
        let record = record(
            &call("exec", r#"{"argv":["sh","-c","exit 3"]}"#, false, result),
            at(80),
        );
        let rows: Vec<_> = record.rows.lines().collect();
        assert_eq!(rows[0], "✗ $ sh -c 'exit 3' · exit 3 · 1.5s");
        assert_eq!(rows[1..], ["  stderr: boom"]);
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
        assert_eq!(rows[1], "  … 17 more lines · /expand shows all");
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
