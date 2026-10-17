//! Compact submitted input. This never changes the buffer Reedline returns.

use super::Line;
use super::tool_row::{PREVIEW_LINES, hidden_lines};

pub(crate) fn collapsed(buffer: &str, width: usize) -> Option<(String, String)> {
    let room = width.saturating_sub(2).max(1);
    let rows: Vec<_> = buffer
        .lines()
        .flat_map(|text| {
            Line::plain(ego_core::permit::terminal_safe(&text.replace('\t', " "))).wrap(room)
        })
        .enumerate()
        .map(|(index, line)| format!("{}{}", if index == 0 { "> " } else { "… " }, line.text()))
        .collect();
    let hidden = rows.len().saturating_sub(PREVIEW_LINES);
    if hidden == 0 {
        return None;
    }
    let mut preview = rows[..PREVIEW_LINES].to_vec();
    preview.push(hidden_lines(hidden, width).text());
    let detail = buffer
        .lines()
        .enumerate()
        .map(|(index, text)| {
            format!(
                "{}{}",
                if index == 0 { "> " } else { "… " },
                ego_core::permit::terminal_safe(text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some((preview.join("\n"), detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: the collapse boundary changes short prompt echo behavior.
    #[test]
    fn short_prompt_echo_keeps_the_editor_rendering() {
        for text in ["", "hello", "one\ntwo\nthree", "one\ntwo\nthree\n"] {
            assert!(collapsed(text, 80).is_none(), "{text:?}");
        }
        let (preview, _) = collapsed("one\ntwo\nthree\nfour", 80).unwrap();
        assert!(preview.ends_with("1 more line · Option+O or /expand"));
    }

    // Catches: a single wide line evades collapse or a preview splits Unicode.
    #[test]
    fn wrapped_prompt_echo_counts_terminal_rows() {
        let (preview, detail) = collapsed(&"界".repeat(80), 41).unwrap();
        assert_eq!(preview.lines().count(), 4);
        assert!(preview.ends_with("2 more lines · Option+O or /expand"));
        assert!(preview.lines().all(|line| Line::plain(line).width() <= 41));
        assert_eq!(detail, format!("> {}", "界".repeat(80)));
    }

    // Catches: expanded text loses wrap spaces or lets input control the terminal.
    #[test]
    fn expanded_prompt_echo_preserves_text_and_escapes_controls() {
        let text = "alpha beta gamma delta\n\nthird\nlast \x1b[2J";
        let (preview, detail) = collapsed(text, 80).unwrap();
        assert!(preview.contains("… \n… third\n"));
        assert_eq!(
            detail,
            "> alpha beta gamma delta\n… \n… third\n… last \\u{1b}[2J"
        );
    }
}
