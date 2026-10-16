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
/// Ordinary input retains Reedline's default highlighting and editing behavior.
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
        reedline::Highlighter::highlight(
            &reedline::ExampleHighlighter::default(),
            &preview,
            cursor.min(preview.len()),
        )
    }
}

pub(super) fn commit(editor: &mut Reedline, status: &SharedStatus, text: &str) -> bool {
    let width = status.lock().expect("surface status lock").width;
    let Some(row) = row(text, width.saturating_sub(1)) else {
        return false;
    };
    // A completed read owns this footprint until anything else writes. Remove
    // its prompt prefix before sending the receipt through the existing pump.
    let _ = editor.erase_last_prompt();
    commit_to_transcript(status, &row);
    true
}
