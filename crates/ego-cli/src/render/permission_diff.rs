//! Bounded display of canonical pending write changes, without filesystem reads.

use ego_core::permit::Change;
use similar::TextDiff;

use super::{Capabilities, Line, Palette, Style};

/// Keep the pending change readable without filling the permission prompt.
const MAX_DIFF_LINES: usize = 30;

/// Render fragment replacements or proposed whole-file content for a permission prompt.
pub fn render(change: &Change, capabilities: Capabilities) -> String {
    let (old, new, before, after, note) = match change {
        Change::Replace {
            old_string,
            new_string,
        } => (
            old_string.as_str(),
            new_string.as_str(),
            "old fragment",
            "new fragment",
            "Replacement fragment; final content depends on the file at execution time.",
        ),
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
