//! Existing sessions selected by the same terminal owner as model pickers.
use super::picker_surface::{
    PickerRole, picker_capabilities, picker_row, truncate_cells, window_bounds,
};
use super::*;
use crate::catalogue::{ResumePicker, SessionPick};
use ego_core::store::session_store::SessionMeta;

#[derive(Default)]
struct PageState {
    sessions: Vec<SessionMeta>,
    filter: String,
    cursor: usize,
    loading: bool,
    has_more: bool,
    error: Option<String>,
}

/// The loader owns all store I/O. Its reply only replaces display state and
/// requests Reedline's existing repaint; it never takes terminal input ownership.
fn loader(
    mut source: ResumePicker,
    page: Arc<Mutex<PageState>>,
    repaint: reedline::RepaintSignal,
) -> Sender<()> {
    let (request, requests) = bounded(1);
    std::thread::spawn(move || {
        while requests.recv().is_ok() {
            let rows = source.next_page();
            let mut page = page.lock().expect("session page lock");
            page.loading = false;
            match rows {
                Ok(rows) => {
                    page.sessions = rows;
                    page.has_more = source.has_more();
                }
                Err(error) => {
                    page.error = Some(format!("The saved session list could not be read: {error}"));
                    page.has_more = false;
                }
            }
            drop(page);
            repaint.request_repaint();
        }
    });
    request
}

pub(super) fn run_session_picker(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    source: ResumePicker,
) -> SessionPick {
    let phase = surface.phase;
    let status = surface.status;
    let after = phase.get();
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .session_picker_open = true;
    let page = Arc::new(Mutex::new(PageState {
        loading: true,
        ..PageState::default()
    }));
    let request = loader(source, Arc::clone(&page), editor.repaint_signal());
    let _ = request.send(());
    let prompt = SessionPrompt {
        page: Arc::clone(&page),
        status: Arc::clone(status),
    };
    let selected = 'picker: loop {
        let line = super::question::modal_line(editor, surface, &prompt);
        let _ = editor.erase_last_prompt();
        match line {
            Line::Ended | Line::ExternalBreak(_) => break SessionPick::Quit,
            Line::Interrupted => break SessionPick::Cancelled,
            _ => {}
        }
        let keys = std::mem::take(&mut status.lock().expect("surface status lock").picker_keys);
        let mut page = page.lock().expect("session page lock");
        let count = page
            .sessions
            .iter()
            .filter(|meta| matches_filter(meta, &page.filter))
            .count();
        page.cursor = page.cursor.min(count.saturating_sub(1));
        for key in keys {
            match key.as_str() {
                "escape" => break 'picker SessionPick::Cancelled,
                "enter" => {
                    let filtered: Vec<_> = page
                        .sessions
                        .iter()
                        .filter(|meta| matches_filter(meta, &page.filter))
                        .collect();
                    if let Some(meta) =
                        filtered.get(page.cursor.min(filtered.len().saturating_sub(1)))
                        && meta.workspace_root.is_dir()
                    {
                        break 'picker SessionPick::Selected(Box::new((*meta).clone()));
                    }
                }
                "next" if !page.loading && page.has_more => {
                    page.sessions.clear();
                    page.filter.clear();
                    page.cursor = 0;
                    page.loading = true;
                    let _ = request.try_send(());
                }
                "up" => page.cursor = page.cursor.saturating_sub(1),
                "down" => page.cursor = page.cursor.saturating_add(1),
                "backspace" => {
                    page.filter.pop();
                    page.cursor = 0;
                }
                typed => {
                    if let Some(value) = typed
                        .strip_prefix("char:")
                        .or_else(|| typed.strip_prefix("number:"))
                    {
                        page.filter.push_str(value);
                        page.cursor = 0;
                    }
                }
            }
        }
    };
    // Disconnecting retires the worker after its current page. No join on the
    // terminal owner: Escape and quit must not wait for a filesystem read.
    drop(request);
    status
        .lock()
        .expect("surface status lock")
        .session_picker_open = false;
    leave_modal(editor, phase, status, saved, after);
    selected
}

struct SessionPrompt {
    page: Arc<Mutex<PageState>>,
    status: SharedStatus,
}

impl Prompt for SessionPrompt {
    fn get_indicator_color(&self) -> reedline::Color {
        reedline::Color::Default
    }
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(
            Question::new(String::new(), Arc::clone(&self.status))
                .render_prompt_right()
                .into_owned(),
        )
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Owned(render_page(&self.page, &self.status))
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }
    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}

fn render_page(page: &Mutex<PageState>, status: &SharedStatus) -> String {
    let page = page.lock().expect("session page lock");
    let matches: Vec<_> = page
        .sessions
        .iter()
        .filter(|meta| matches_filter(meta, &page.filter))
        .collect();
    let cursor = page.cursor.min(matches.len().saturating_sub(1));
    let (_, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let caps = picker_capabilities(status);
    let width = caps.width.saturating_sub(1);
    let visible = usize::from(height).saturating_sub(8).max(1);
    let (start, end) = window_bounds(matches.len(), visible, Some(cursor));
    let mut rows = vec![picker_row(
        truncate_cells(&format!("Select session · filter: {}", page.filter), width),
        PickerRole::Question,
        caps.depth,
    )];
    if page.loading {
        rows.push("Loading saved sessions…".to_owned());
    } else if let Some(error) = &page.error {
        rows.push(error.clone());
    } else if page.sessions.is_empty() {
        rows.push("No other saved sessions.".to_owned());
    } else if matches.is_empty() {
        rows.push("No matching sessions on this page; Backspace edits.".to_owned());
    }
    let mut table = vec![vec![
        "".into(),
        "Title".into(),
        "Updated".into(),
        "Model".into(),
    ]];
    table.extend(matches[start..end].iter().enumerate().map(|(index, meta)| {
        let stamp = i64::try_from(meta.updated.as_millis())
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|at| {
                at.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "—".into());
        vec![
            if start + index == cursor { ">" } else { " " }.into(),
            meta.title.as_deref().unwrap_or("Untitled").into(),
            stamp,
            meta.model.as_deref().unwrap_or("—").into(),
        ]
    }));
    for (index, row) in crate::render::table::plain_table_with_width(table, width)
        .into_iter()
        .enumerate()
    {
        rows.push(picker_row(
            row,
            if index > 0 && start + index - 1 == cursor {
                PickerRole::Selected
            } else {
                PickerRole::Label
            },
            caps.depth,
        ));
    }
    if let Some(meta) = matches.get(cursor) {
        rows.push(String::new());
        rows.push(picker_row(
            truncate_cells(
                &format!("{} · {}", meta.id, meta.workspace_root.display()),
                width,
            ),
            PickerRole::Description,
            caps.depth,
        ));
        rows.push(picker_row(
            truncate_cells(meta.summary.as_deref().unwrap_or("No summary"), width),
            PickerRole::Description,
            caps.depth,
        ));
        if !meta.workspace_root.is_dir() {
            rows.push("[unavailable workspace]".into());
        }
    }
    let hint = if page.has_more && !page.loading {
        "Type to filter this page; Up/Down selects; PgDn loads next 10; Enter resumes; Esc cancels."
    } else {
        "Type to filter this page; Up/Down selects; Enter resumes; Esc cancels."
    };
    rows.push(picker_row(
        truncate_cells(hint, width),
        PickerRole::Hint,
        caps.depth,
    ));
    rows.join("\n")
}

fn matches_filter(meta: &SessionMeta, filter: &str) -> bool {
    let filter = filter.to_lowercase();
    filter.is_empty()
        || [
            meta.title.as_deref(),
            meta.model.as_deref(),
            meta.summary.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| text.to_lowercase().contains(&filter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ego_core::session_lifecycle::SessionLifecycleStore;
    use ego_tools::store::{JsonlStore, OsIo, StoreIo};
    use reedline::{EditMode, ReedlineEvent, ReedlineRawEvent};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    struct DelayedRead {
        first: AtomicBool,
        entered: Sender<()>,
        release: Receiver<()>,
    }
    impl StoreIo for DelayedRead {
        fn file_revision(
            &self,
            path: &Path,
        ) -> std::io::Result<Option<ego_tools::store::FileRevision>> {
            OsIo.file_revision(path)
        }
        fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
            if self.first.swap(false, Ordering::SeqCst) {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            OsIo.read(path)
        }
        fn append_line(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
            OsIo.append_line(path, line)
        }
        fn fsync(&self, path: &Path) -> std::io::Result<()> {
            OsIo.fsync(path)
        }
        fn exists(&self, path: &Path) -> std::io::Result<bool> {
            OsIo.exists(path)
        }
        fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            OsIo.replace_atomic(path, bytes)
        }
        fn truncate(&self, path: &Path, length: u64) -> std::io::Result<()> {
            OsIo.truncate(path, length)
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            OsIo.remove_file(path)
        }
        fn list_dirs(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
            OsIo.list_dirs(path)
        }
    }

    // Catches: page I/O runs inline before the modal can render or decode input.
    #[test]
    fn the_seat_accepts_input_while_a_page_loads() {
        let dir = tempfile::tempdir().unwrap();
        let seed = JsonlStore::new(dir.path(), "/workspace", Box::new(OsIo));
        seed.publish_empty_session(
            SessionId::new(),
            Path::new("/workspace"),
            ego_core::event::time::Timestamp::EPOCH,
        )
        .unwrap();
        let (entered, first_read) = bounded(1);
        let (release, released) = bounded(1);
        let store = Arc::new(JsonlStore::new(
            dir.path(),
            "/workspace",
            Box::new(DelayedRead {
                first: AtomicBool::new(true),
                entered,
                release: released,
            }),
        ));
        let page = Arc::new(Mutex::new(PageState {
            loading: true,
            ..PageState::default()
        }));
        let mut editor = Reedline::create();
        let request = loader(
            ResumePicker::new(store, "/workspace".into(), None),
            Arc::clone(&page),
            editor.repaint_signal(),
        );
        request.send(()).unwrap();
        first_read
            .recv_timeout(Duration::from_secs(5))
            .expect("the real first page read is delayed");
        let phase = SurfacePhase::new(EditPhase::Modal);
        let status = Arc::new(Mutex::new(SurfaceStatus {
            session_picker_open: true,
            ..SurfaceStatus::default()
        }));
        let mut mode = super::super::edit_mode::SurfaceEditMode::with_status(
            emacs_composer(),
            phase,
            Arc::clone(&status),
        );
        let key = |code| {
            ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))).unwrap()
        };
        let typed = mode.parse_event(key(KeyCode::Char('x')));
        let escaped = mode.parse_event(key(KeyCode::Esc));
        let frame = render_page(&page, &status);
        let keys = status.lock().unwrap().picker_keys.clone();
        drop(request);
        release.send(()).unwrap();
        assert!(matches!(typed, ReedlineEvent::ExecuteHostCommand(_)));
        assert!(matches!(escaped, ReedlineEvent::ExecuteHostCommand(_)));
        assert_eq!(keys, ["char:x", "escape"]);
        assert!(frame.contains("Loading saved sessions"), "{frame}");
    }
}
