//! Keyboard seat for `ego setup`. Every line of a title or review stays
//! reachable at any terminal size the seat accepts; a smaller terminal gets an
//! actionable refusal, and a save needs the whole review shown first.

use crossterm::event::KeyCode;

use crate::setup_config::Screen;

/// The smallest terminal that can show text, one option and the key line.
pub(crate) const MIN_WIDTH: usize = 20;
pub(crate) const MIN_HEIGHT: usize = 6;

/// One title-and-options view with its navigation state.
#[derive(Debug)]
pub(crate) struct View {
    title: String,
    options: Vec<String>,
    selected: usize,
    offset: usize,
    /// A choice other than the first is accepted only after the last title
    /// line was on screen. Set for a save confirmation.
    require_full_text: bool,
    seen_end: bool,
    hint: Option<&'static str>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Chosen(usize),
    Cancelled,
}

struct Areas {
    text_rows: usize,
    option_rows: usize,
    max_offset: usize,
    width: usize,
}

/// Split at display columns, so a wide character never makes a row wrap.
fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut row = String::new();
        let mut used = 0;
        for character in ego_core::permit::terminal_safe(line).chars() {
            let columns = character.width().unwrap_or(0);
            if used + columns > width {
                lines.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(character);
            used += columns;
        }
        lines.push(row);
    }
    lines
}

/// Cut to `width` display columns, marking a cut with an ellipsis.
pub(crate) fn fit(text: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if text.width() <= width {
        return text.to_owned();
    }
    let mut row = String::new();
    let mut used = 0;
    for character in text.chars() {
        let columns = character.width().unwrap_or(0);
        if used + columns + 1 > width {
            break;
        }
        row.push(character);
        used += columns;
    }
    row.push('…');
    row
}

impl View {
    pub(crate) fn new(title: &str, options: &[String], require_full_text: bool) -> Self {
        Self {
            title: title.to_owned(),
            options: options.to_vec(),
            selected: 0,
            offset: 0,
            require_full_text,
            seen_end: false,
            hint: None,
        }
    }

    fn areas(&self, width: usize, height: usize) -> Option<(Vec<String>, Areas)> {
        if width < MIN_WIDTH || height < MIN_HEIGHT || self.options.is_empty() {
            return None;
        }
        let title = wrap(&self.title, width);
        // Rows left after the blank separator and the key line.
        let available = height - 3;
        let (text_rows, option_rows) = if title.len() + self.options.len() <= available {
            (title.len(), self.options.len())
        } else {
            let options = self.options.len().min(available / 2).max(1);
            let text = title.len().min(available - options);
            (text, self.options.len().min(available - text))
        };
        let max_offset = title.len() - text_rows;
        Some((
            title,
            Areas {
                text_rows,
                option_rows,
                max_offset,
                width,
            },
        ))
    }

    /// Lines to draw, or `None` when the terminal is too small to review.
    pub(crate) fn render(&mut self, width: usize, height: usize) -> Option<Vec<String>> {
        let (title, areas) = self.areas(width, height)?;
        self.offset = self.offset.min(areas.max_offset);
        if self.offset == areas.max_offset {
            self.seen_end = true;
        }
        let mut lines = title[self.offset..self.offset + areas.text_rows].to_vec();
        lines.push(String::new());
        let start = self.selected.saturating_sub(areas.option_rows - 1);
        for (index, option) in self
            .options
            .iter()
            .enumerate()
            .skip(start)
            .take(areas.option_rows)
        {
            let marker = if index == self.selected { '>' } else { ' ' };
            lines.push(fit(
                &format!("{marker} {}", ego_core::permit::terminal_safe(option)),
                areas.width,
            ));
        }
        let mut keys = format!(
            "↑↓ move · Enter choose · Esc cancel · {}/{}",
            self.selected + 1,
            self.options.len()
        );
        if areas.max_offset > 0 {
            keys.push_str(&format!(
                " · ←→ scroll text {}-{}/{}",
                self.offset + 1,
                self.offset + areas.text_rows,
                title.len()
            ));
        }
        if let Some(hint) = self.hint {
            keys = hint.to_owned();
        }
        lines.push(fit("PgUp/PgDn page    Home/End first/last", areas.width));
        if self
            .options
            .iter()
            .any(|option| option.starts_with("Review and save"))
        {
            keys = keys.replace("Enter choose", "Enter choose · S review/save");
        }
        lines.push(fit(&keys, areas.width));
        Some(lines)
    }

    /// Apply one key at the given terminal size. A too-small terminal accepts
    /// only Esc, so nothing can be chosen without its text on screen.
    pub(crate) fn key(&mut self, code: KeyCode, width: usize, height: usize) -> Option<Outcome> {
        let Some((_, areas)) = self.areas(width, height) else {
            return (code == KeyCode::Esc).then_some(Outcome::Cancelled);
        };
        let last = self.options.len() - 1;
        self.hint = None;
        match code {
            KeyCode::Up => self.selected = self.selected.checked_sub(1).unwrap_or(last),
            KeyCode::Down => {
                self.selected = if self.selected == last {
                    0
                } else {
                    self.selected + 1
                }
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(areas.option_rows),
            KeyCode::PageDown => self.selected = (self.selected + areas.option_rows).min(last),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            KeyCode::Left => self.offset = self.offset.saturating_sub(areas.text_rows),
            KeyCode::Right => {
                self.offset = (self.offset + areas.text_rows).min(areas.max_offset);
            }
            KeyCode::Char('s' | 'S') => {
                if let Some(index) = self
                    .options
                    .iter()
                    .position(|option| option.starts_with("Review and save"))
                {
                    return Some(Outcome::Chosen(index));
                }
            }
            KeyCode::Esc => return Some(Outcome::Cancelled),
            KeyCode::Enter if self.require_full_text && self.selected > 0 && !self.seen_end => {
                self.hint = Some("Read the whole review first: press → to scroll to its end.");
            }
            KeyCode::Enter => return Some(Outcome::Chosen(self.selected)),
            _ => {}
        }
        None
    }
}

/// Notices wait for the next view, which clears the screen, and are drawn
/// above it; any left when setup ends are printed then.
#[derive(Default)]
pub(crate) struct TerminalScreen {
    notices: Vec<String>,
    lost: bool,
}

struct RawMode;

impl RawMode {
    fn enter() -> Result<Self, String> {
        crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// `Err` means the terminal is gone; the caller must stop asking.
fn show(mut view: View) -> Result<Option<usize>, ()> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute, terminal,
    };
    use std::io::{Write, stdout};
    let _raw = RawMode::enter().map_err(|_| ())?;
    loop {
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let (width, height) = (usize::from(width), usize::from(height));
        let lines = view.render(width, height).unwrap_or_else(|| {
            vec![format!(
                "Terminal too small for setup: needs {MIN_WIDTH}x{MIN_HEIGHT}. Enlarge it, or press Esc to cancel."
            )]
        });
        let mut out = stdout().lock();
        execute!(
            out,
            cursor::MoveTo(0, 0),
            terminal::Clear(terminal::ClearType::All)
        )
        .map_err(|_| ())?;
        // No line break after the last row: a full-height view must not
        // scroll its first row off the screen.
        write!(out, "{}", lines.join("\r\n")).map_err(|_| ())?;
        out.flush().map_err(|_| ())?;
        drop(out);
        match event::read().map_err(|_| ())? {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                return Ok(None);
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match view.key(key.code, width, height) {
                    Some(Outcome::Chosen(index)) => return Ok(Some(index)),
                    Some(Outcome::Cancelled) => return Ok(None),
                    None => {}
                }
            }
            _ => {}
        }
    }
}

impl TerminalScreen {
    fn show(&mut self, title: &str, options: &[String], require_full_text: bool) -> Option<usize> {
        if self.lost || options.is_empty() {
            return None;
        }
        let mut text = self.notices.join("\n");
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(title);
        match show(View::new(&text, options, require_full_text)) {
            Ok(choice) => {
                self.notices.clear();
                choice
            }
            Err(()) => {
                self.lost = true;
                None
            }
        }
    }
}

impl Drop for TerminalScreen {
    fn drop(&mut self) {
        if !self.notices.is_empty() {
            println!("\r\n{}", self.notices.join("\n"));
        }
    }
}

impl Screen for TerminalScreen {
    fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
        self.show(title, options, false)
    }

    fn input(&mut self, prompt: &str) -> Option<String> {
        use std::io::{Write, stdin, stdout};
        if self.lost {
            return None;
        }
        print!("\r\n{}: ", ego_core::permit::terminal_safe(prompt));
        stdout().flush().ok()?;
        let mut input = String::new();
        if stdin().read_line(&mut input).ok()? == 0 {
            return None;
        }
        Some(input.trim().to_owned())
    }

    fn confirm(&mut self, preview: &str) -> bool {
        self.show(preview, &["No — keep editing".into(), "Yes".into()], true) == Some(1)
    }

    fn notice(&mut self, text: &str) {
        self.notices.push(ego_core::permit::terminal_safe(text));
    }

    fn lost(&self) -> bool {
        self.lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(lines: usize) -> String {
        (0..lines)
            .map(|line| {
                let wide = if line == 7 {
                    "0123456789".repeat(9)
                } else {
                    String::new()
                };
                format!("line-{line:03} {wide}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn confirm_view(text: &str) -> View {
        View::new(
            text,
            &["No — keep editing".into(), "Yes — save changes".into()],
            true,
        )
    }

    // Catches: a review longer or wider than the terminal hides lines, so the
    // operator saves changes they could never see.
    #[test]
    fn every_review_line_is_reachable_in_a_small_terminal() {
        let text = review(40);
        let mut view = confirm_view(&text);
        let mut rows: Vec<String> = Vec::new();
        for _ in 0..40 {
            let frame = view.render(24, 8).unwrap();
            assert!(
                frame.iter().all(|row| row.chars().count() <= 24),
                "{frame:?}"
            );
            for row in frame.into_iter().take_while(|row| !row.is_empty()) {
                if !rows.contains(&row) {
                    rows.push(row);
                }
            }
            view.key(KeyCode::Right, 24, 8);
        }
        assert_eq!(rows.concat(), text.replace('\n', ""));
    }

    // Catches: confirmation defaults to Yes, or accepts Yes before the end of
    // the review was on screen (blind consent).
    #[test]
    fn save_needs_an_explicit_yes_after_the_whole_review() {
        let text = review(40);
        let mut view = confirm_view(&text);
        view.render(24, 8).unwrap();
        assert_eq!(
            view.key(KeyCode::Enter, 24, 8),
            Some(Outcome::Chosen(0)),
            "default is No"
        );
        let mut view = confirm_view(&text);
        view.render(24, 8).unwrap();
        view.key(KeyCode::Down, 24, 8);
        assert_eq!(view.key(KeyCode::Enter, 24, 8), None, "Yes before the end");
        assert!(
            view.render(24, 8)
                .unwrap()
                .last()
                .unwrap()
                .contains("Read the whole review")
        );
        for _ in 0..40 {
            view.key(KeyCode::Right, 24, 8);
        }
        view.render(24, 8).unwrap();
        assert_eq!(view.key(KeyCode::Enter, 24, 8), Some(Outcome::Chosen(1)));
    }

    // Catches: page movement uses a fixed amount rather than visible rows,
    // Home/End land on the wrong boundary, or a save shortcut chooses a setting.
    #[test]
    fn setup_list_keys_follow_the_visible_rows_and_keep_save_reachable() {
        let mut options = (0..50).map(|i| format!("setting-{i}")).collect::<Vec<_>>();
        options.extend([
            "Review and save (1 pending)".into(),
            "Discard and exit".into(),
        ]);
        for (width, height) in [(80, 24), (24, 8)] {
            let mut view = View::new("Settings", &options, false);
            let page = view.areas(width, height).unwrap().1.option_rows;
            view.render(width, height).unwrap();
            view.key(KeyCode::PageDown, width, height);
            assert_eq!(
                view.key(KeyCode::Enter, width, height),
                Some(Outcome::Chosen(page))
            );
            assert_eq!(
                view.key(KeyCode::Char('s'), width, height),
                Some(Outcome::Chosen(50))
            );
            view.key(KeyCode::PageUp, width, height);
            assert_eq!(
                view.key(KeyCode::Enter, width, height),
                Some(Outcome::Chosen(0))
            );
            view.key(KeyCode::End, width, height);
            assert_eq!(
                view.key(KeyCode::Enter, width, height),
                Some(Outcome::Chosen(51))
            );
            view.key(KeyCode::Home, width, height);
            assert_eq!(
                view.key(KeyCode::Enter, width, height),
                Some(Outcome::Chosen(0))
            );
        }
    }

    // Catches: a terminal too small to show a review still lets Enter save.
    #[test]
    fn a_too_small_terminal_refuses_every_choice_but_cancel() {
        let mut view = confirm_view("Review\nmode: plan -> yolo");
        assert!(view.render(MIN_WIDTH - 1, 12).is_none());
        assert!(view.render(40, MIN_HEIGHT - 1).is_none());
        for code in [KeyCode::Down, KeyCode::End, KeyCode::Enter] {
            assert_eq!(view.key(code, 40, MIN_HEIGHT - 1), None);
        }
        assert_eq!(
            view.key(KeyCode::Esc, 40, MIN_HEIGHT - 1),
            Some(Outcome::Cancelled)
        );
    }
}
