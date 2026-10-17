//! Existing sessions selected by the same terminal owner as model pickers.
use super::picker_surface::{
    PickerRole, picker_capabilities, picker_row, truncate_cells, window_bounds,
};
use super::*;
use ego_core::store::session_store::SessionMeta;

pub(super) fn run_session_picker(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    sessions: Vec<SessionMeta>,
) -> Option<SessionId> {
    let after = phase.get();
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .session_picker_open = true;
    let mut filter = String::new();
    let mut cursor = 0_usize;
    let selected = 'picker: loop {
        let matches: Vec<_> = sessions
            .iter()
            .filter(|meta| matches_filter(meta, &filter))
            .collect();
        cursor = cursor.min(matches.len().saturating_sub(1));
        let (_cols, height) = crossterm::terminal::size().unwrap_or((80, 24));
        let caps = picker_capabilities(status);
        let width = caps.width.saturating_sub(1);
        let visible = usize::from(height)
            .saturating_sub(6)
            .saturating_div(3)
            .max(1);
        let (start, end) = window_bounds(matches.len(), visible, Some(cursor));
        let mut rows = vec![picker_row(
            truncate_cells(&format!("Select session · filter: {filter}"), width),
            PickerRole::Question,
            caps.depth,
        )];
        if sessions.is_empty() {
            rows.push("No other saved sessions.".to_owned());
        } else if matches.is_empty() {
            rows.push("No matching sessions; Backspace edits.".to_owned());
        }
        for (index, meta) in matches[start..end].iter().enumerate() {
            let marker = if start + index == cursor { ">" } else { " " };
            let stamp = i64::try_from(meta.updated.as_millis())
                .ok()
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|at| {
                    at.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| "—".to_owned());
            let available = meta.workspace_root.is_dir();
            rows.push(picker_row(
                truncate_cells(
                    &format!(
                        "{marker} {stamp}  {}  {}",
                        meta.title.as_deref().unwrap_or("Untitled"),
                        meta.model.as_deref().unwrap_or("—")
                    ),
                    width,
                ),
                if marker == ">" {
                    PickerRole::Selected
                } else {
                    PickerRole::Label
                },
                caps.depth,
            ));
            rows.push(picker_row(
                truncate_cells(
                    &format!(
                        "  {}{}",
                        if available { "" } else { "[unavailable] " },
                        meta.workspace_root.display()
                    ),
                    width,
                ),
                PickerRole::Description,
                caps.depth,
            ));
            rows.push(picker_row(
                truncate_cells(
                    &format!("  {}", meta.summary.as_deref().unwrap_or("No summary")),
                    width,
                ),
                PickerRole::Description,
                caps.depth,
            ));
        }
        rows.push(picker_row(
            truncate_cells(
                "Type to filter; Up/Down selects; Enter resumes; Esc cancels.",
                width,
            ),
            PickerRole::Hint,
            caps.depth,
        ));
        let prompt = Question::new(rows.join("\n"), Arc::clone(status));
        let line = answer(false, editor.read_line(&prompt));
        let _ = editor.erase_last_prompt();
        if matches!(
            line,
            Line::Interrupted | Line::Ended | Line::ExternalBreak(_)
        ) {
            break None;
        }
        let keys = std::mem::take(&mut status.lock().expect("surface status lock").picker_keys);
        for key in keys {
            match key.as_str() {
                "escape" => break 'picker None,
                "enter" => {
                    // Filtering and selecting may arrive in one Reedline burst.
                    let filtered: Vec<_> = sessions
                        .iter()
                        .filter(|meta| matches_filter(meta, &filter))
                        .collect();
                    if let Some(meta) = filtered.get(cursor.min(filtered.len().saturating_sub(1)))
                        && meta.workspace_root.is_dir()
                    {
                        break 'picker Some(meta.id);
                    }
                }
                "up" => cursor = cursor.saturating_sub(1),
                "down" => cursor = cursor.saturating_add(1),
                "backspace" => {
                    filter.pop();
                    cursor = 0;
                }
                typed => {
                    if let Some(value) = typed
                        .strip_prefix("char:")
                        .or_else(|| typed.strip_prefix("number:"))
                    {
                        filter.push_str(value);
                        cursor = 0;
                    }
                }
            }
        }
    };
    status
        .lock()
        .expect("surface status lock")
        .session_picker_open = false;
    leave_modal(editor, phase, status, saved, after);
    selected
}

fn matches_filter(meta: &SessionMeta, filter: &str) -> bool {
    let filter = filter.to_lowercase();
    meta.title
        .as_deref()
        .unwrap_or("")
        .to_lowercase()
        .contains(&filter)
        || meta
            .model
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains(&filter)
        || meta
            .summary
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains(&filter)
}
