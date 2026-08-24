//! The seat: an editor that is active only while a line is being read.
//!
//! Its lock is held only for that read, so the renderer is the only writer
//! while it renders a turn — see
//! `docs/decisions/2026-08-22-the-seat-reads-before-the-screen-writes.md` §2.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::Event;
use reedline::{
    EditMode, Emacs, Prompt, PromptEditMode, PromptHistorySearch, Reedline, ReedlineEvent,
    ReedlineRawEvent, default_emacs_keybindings,
};

use super::keys::{Intercept, intercept};
use super::{Line, answer};
use ego_core::config::Trust;
use ego_core::permit::{PermissionDecision, PermissionRequest};

/// A line editor, and the one key ego takes from it.
pub struct Seat {
    editor: Mutex<Reedline>,
    asked_for_image: Arc<AtomicBool>,
    terminal: bool,
}

impl std::fmt::Debug for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seat")
    }
}

impl Seat {
    /// Build an editor with bracketed paste on.
    ///
    /// Bracketed paste is not a nicety: without it a pasted stack trace
    /// submits its first line as a whole prompt, which is the failure that
    /// makes pasting one impossible. `CLI.md` §6.1.
    pub fn new() -> Self {
        let asked_for_image = Arc::new(AtomicBool::new(false));
        let mode = Ours {
            editor: Emacs::new(default_emacs_keybindings()),
            asked_for_image: Arc::clone(&asked_for_image),
        };

        Self {
            editor: Mutex::new(
                Reedline::create()
                    .with_edit_mode(Box::new(mode))
                    .use_bracketed_paste(true),
            ),
            asked_for_image,
            terminal: std::io::stdin().is_terminal(),
        }
    }

    /// Read one line.
    pub fn read(&self) -> Line {
        self.read_with(&Waiting)
    }

    fn read_with(&self, prompt: &dyn Prompt) -> Line {
        let signal = self.editor.lock().unwrap().read_line(prompt);
        // Taken, not read: the key is consumed here so it cannot leak into
        // the next line.
        let asked_for_image = self.asked_for_image.swap(false, Ordering::Relaxed);

        answer(asked_for_image, signal)
    }
}

impl Default for Seat {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::run::Reads for Seat {
    fn read(&self) -> Line {
        Seat::read(self)
    }
}

impl crate::run::InteractiveSeat for Seat {
    fn has_human(&self) -> bool {
        self.terminal
    }

    fn trust(&self, workspace: &Path) -> Trust {
        // Piped stdin may itself be the first prompt. Consuming it as a trust
        // answer would turn user content into authority. With no terminal
        // there is no human seat, so the only truthful answer is untrusted.
        if !self.terminal {
            return Trust::Untrusted;
        }
        let prompt = Question(format!(
            "Trust project files in {}? [y/N] ",
            workspace.display()
        ));
        loop {
            match self.read_with(&prompt) {
                Line::Submitted(answer) => match parse_trust(&answer) {
                    Some(trust) => return trust,
                    None => crate::output::note("answer yes or no"),
                },
                Line::Blank | Line::Interrupted | Line::Ended => return Trust::Untrusted,
                Line::Refused(why) => crate::output::note(why),
            }
        }
    }

    fn permission(&self, request: &PermissionRequest) -> PermissionDecision {
        if !self.terminal {
            return PermissionDecision::Deny;
        }
        let prompt = Question(format!("{request}. [allow/deny/amend] "));
        loop {
            match self.read_with(&prompt) {
                Line::Submitted(answer) => match parse_permission(&answer) {
                    Some(PermissionWord::Allow) => return PermissionDecision::Allow,
                    Some(PermissionWord::Deny) => return PermissionDecision::Deny,
                    Some(PermissionWord::Amend) => return self.amendment(),
                    None => crate::output::note("answer allow, deny, or amend"),
                },
                Line::Interrupted | Line::Ended => return PermissionDecision::Deny,
                Line::Blank => crate::output::note("answer allow, deny, or amend"),
                Line::Refused(why) => crate::output::note(why),
            }
        }
    }
}

impl Seat {
    fn amendment(&self) -> PermissionDecision {
        let prompt = Question("New JSON parameters: ".to_string());
        loop {
            match self.read_with(&prompt) {
                Line::Submitted(raw) => match serde_json::from_str(&raw) {
                    Ok(params) => return PermissionDecision::Amend { params },
                    Err(err) => crate::output::note(&format!("invalid JSON parameters: {err}")),
                },
                Line::Interrupted | Line::Ended => return PermissionDecision::Deny,
                Line::Blank => crate::output::note("enter the amended parameters as JSON"),
                Line::Refused(why) => crate::output::note(why),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionWord {
    Allow,
    Deny,
    Amend,
}

fn parse_trust(answer: &str) -> Option<Trust> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Trust::Trusted),
        "n" | "no" => Some(Trust::Untrusted),
        _ => None,
    }
}

fn parse_permission(answer: &str) -> Option<PermissionWord> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "a" | "allow" => Some(PermissionWord::Allow),
        "d" | "deny" => Some(PermissionWord::Deny),
        "m" | "amend" => Some(PermissionWord::Amend),
        _ => None,
    }
}

/// The prompt: two characters and nothing else.
///
/// No model name, no token count, no cwd. Everything a status line could say
/// is either already on the screen or answerable by asking, and a prompt that
/// redraws a changing right-hand side is a second writer competing with the
/// renderer for the same row.
struct Waiting;

impl Prompt for Waiting {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    /// Alt+Enter and a bracketed paste both put a real newline in the buffer,
    /// and this is the only mark that the line being typed is still one line.
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}

struct Question(String);

impl Prompt for Question {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.0)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}

/// The Emacs keymap, with ego's keys taken first.
///
/// A custom [`EditMode`] rather than a keybinding, because reedline 0.44's
/// `ExecuteHostCommand` comes back as `Signal::Success(text)` — the same arm a
/// person typing that text produces. A flag set here cannot be typed.
struct Ours {
    editor: Emacs,
    asked_for_image: Arc<AtomicBool>,
}

impl EditMode for Ours {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let raw: Event = event.into();

        if let Event::Key(key) = raw
            && intercept(key.code, key.modifiers) == Intercept::ClipboardImage
        {
            self.asked_for_image.store(true, Ordering::Relaxed);
            // Exits the read so the refusal is seen at once, rather than
            // after the person presses Enter on a line they had given up on.
            return ReedlineEvent::ExecuteHostCommand(String::new());
        }

        match ReedlineRawEvent::try_from(raw) {
            Ok(event) => self.editor.parse_event(event),
            // Unreachable in practice: the only rejected event is a key
            // release, and `raw` came out of a `ReedlineRawEvent`, which is
            // the type that already excluded one.
            Err(()) => ReedlineEvent::None,
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.editor.edit_mode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_accepts_only_an_explicit_yes_or_no() {
        for yes in ["y", "yes", " YES "] {
            assert_eq!(parse_trust(yes), Some(Trust::Trusted));
        }
        for no in ["n", "no", " NO "] {
            assert_eq!(parse_trust(no), Some(Trust::Untrusted));
        }
        for invalid in ["", "maybe", "allow", "1"] {
            assert_eq!(parse_trust(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn the_plain_permission_vocabulary_includes_amend() {
        for (raw, expected) in [
            ("allow", PermissionWord::Allow),
            ("deny", PermissionWord::Deny),
            ("amend", PermissionWord::Amend),
            (" A ", PermissionWord::Allow),
            ("d", PermissionWord::Deny),
            ("m", PermissionWord::Amend),
        ] {
            assert_eq!(parse_permission(raw), Some(expected), "{raw:?}");
        }
        for invalid in ["", "yes", "no", "approve", "cancel"] {
            assert_eq!(parse_permission(invalid), None, "{invalid:?}");
        }
    }
}
