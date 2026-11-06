//! Bounded display of canonical pending write changes, without filesystem reads.

use ego_core::permit::Change;
use similar::TextDiff;

use super::{Capabilities, Line, Palette, Style};

/// Keep the pending change readable without filling the permission prompt.
const MAX_DIFF_LINES: usize = 30;

/// Render fragment replacements or proposed whole-file content for a permission prompt.
pub fn render(change: &Change, capabilities: Capabilities) -> String {
    let (old, new, before, after, note) = match change {
        Change::Notebook(edit) => {
            let encoded = serde_json::to_string(edit).expect("closed cell edit codec");
            let preview: String = encoded.chars().take(2000).collect();
            return format!(
                "Notebook cell edit: {}{}\nOther cells, metadata and outputs are retained.",
                ego_core::permit::terminal_safe(&preview),
                if preview.len() < encoded.len() {
                    "… (truncated)"
                } else {
                    ""
                },
            );
        }
        Change::Replace {
            replace_all,
            chunks,
        } => {
            let note = if *replace_all {
                "Replacement fragment: replace all original non-overlapping occurrences; final content depends on the file at execution time."
            } else {
                "Replacement fragment: replace exactly one occurrence; final content depends on the file at execution time."
            };
            return chunks
                .iter()
                .enumerate()
                .map(|(index, chunk)| {
                    format!(
                        "Chunk {}:\n{}",
                        index + 1,
                        render_diff(
                            &chunk.old_string,
                            &chunk.new_string,
                            "old fragment",
                            "new fragment",
                            note,
                            capabilities
                        )
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        Change::Whole(bytes) => match std::str::from_utf8(bytes) {
            Ok(content) => (
                "",
                content,
                "/dev/null",
                "proposed content",
                "Proposed whole-file content; previous content is not read (an existing file is overwritten).",
            ),
            Err(_) => {
                return format!(
                    "Binary whole-file content ({} bytes); no text diff available.",
                    bytes.len()
                );
            }
        },
    };
    render_diff(old, new, before, after, note, capabilities)
}

fn render_diff(
    old: &str,
    new: &str,
    before: &str,
    after: &str,
    note: &str,
    capabilities: Capabilities,
) -> String {
    let unified = TextDiff::from_lines(old, new)
        .unified_diff()
        .header(before, after)
        .to_string();
    let total = unified.lines().count();
    let mut out = format!("{note}\n");
    for row in unified.lines().take(MAX_DIFF_LINES) {
        let style = match row.as_bytes().first() {
            Some(b'+') => Style::fg(Palette::ADDED_FG),
            Some(b'-') => Style::fg(Palette::REMOVED_FG),
            _ => Style::PLAIN,
        };
        let safe: String = row
            .chars()
            .map(|c| {
                if c.is_control() {
                    char::REPLACEMENT_CHARACTER
                } else {
                    c
                }
            })
            .collect();
        let line = Line::styled(safe, style).truncate_to(capabilities.width);
        out.push_str(&line.render(capabilities.depth));
        out.push('\n');
    }
    if total > MAX_DIFF_LINES {
        out.push_str(&format!(
            "[{} diff lines omitted]\n",
            total - MAX_DIFF_LINES
        ));
    }
    out.trim_end_matches('\n').to_string()
}
