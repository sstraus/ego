use super::*;

/// TUIC delivers these as submitted text. Keep the text intact for the model,
/// but do not commit its transport envelope as a user-authored prompt.
fn fields(text: &str) -> Option<(&str, &str, &str)> {
    let rest = text.strip_prefix("BG DONE exit=")?;
    let (exit, rest) = rest.split_once(" log=")?;
    exit.parse::<i32>().ok()?;
    let (log, command) = rest.split_once(" cmd=")?;
    Some((exit, log, command))
}

pub(super) fn row(text: &str, width: usize) -> Option<String> {
    let (exit, log, command) = fields(text)?;
    let first_line = command.split(['\r', '\n']).next().unwrap_or_default();
    let label = if first_line.len() < command.len() {
        format!("{first_line}…")
    } else {
        first_line.to_owned()
    };
    let prefix = format!("background task done · exit {exit} · ");
    let suffix = format!(" · log {log}");
    let fixed = crate::render::Line::plain(format!("{prefix}{suffix}")).width();
    let label = picker_surface::truncate_cells(&label, width.saturating_sub(fixed));
    Some(picker_surface::truncate_cells(
        &format!("{prefix}{label}{suffix}"),
        width,
    ))
}

/// A completion may already exceed the screen before Enter arrives. Clip only
/// its preview, keeping an exact prefix so Reedline's byte cursor remains valid.
/// Mark the interactive shell prefix without changing the editor buffer.
pub(super) struct NoticeHighlighter(pub(super) SharedStatus);

impl reedline::Highlighter for NoticeHighlighter {
    fn highlight(&self, text: &str, cursor: usize) -> reedline::StyledText {
        let preview = if fields(text).is_some() {
            let width = self.0.lock().expect("surface status lock").width;
            crate::render::Line::plain(text.lines().next().unwrap_or_default())
                .truncate_to(width.saturating_sub(3))
                .text()
        } else {
            text.to_owned()
        };
        let mut styled = reedline::Highlighter::highlight(
            &reedline::ExampleHighlighter::default(),
            &preview,
            cursor.min(preview.len()),
        );
        if text.starts_with("! ") && !text.contains('\n') {
            styled.style_range(0, 2, reedline::Color::Red.normal());
        }
        styled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::Highlighter;
    use std::sync::{Arc, Mutex};

    #[test]
    fn shell_prefix_turns_red_after_space_only_in_a_shell_draft() {
        let highlighter = NoticeHighlighter(Arc::new(Mutex::new(Default::default())));
        // Catches a missing visual mode switch or a false shell cue on literal text.
        for (draft, red_prefix) in [
            ("!", false),
            ("! ", true),
            ("! printf caffè", true),
            ("!! printf", false),
            (" ! printf", false),
            ("! printf\nsecond line", false),
        ] {
            let styled = highlighter.highlight(draft, draft.len());
            assert_eq!(styled.raw_string(), draft);
            assert_eq!(
                styled.render_simple().contains("\x1b[31m! \x1b[0m"),
                red_prefix,
                "{draft:?}"
            );
        }
    }
}
